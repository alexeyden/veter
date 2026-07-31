//! prt-cli — emit PRT (Portal Extension) envelopes to stdout for manual
//! testing.
//!
//! Usage examples (run inside a veter session — stdout IS the PTY,
//! veter reads it through its APC parser):
//!
//!   prt-cli probe
//!   prt-cli create-portal left  --size 80x24 --origin 0,0
//!   prt-cli create-portal right --size 80x24 --origin 80,0
//!   prt-cli write-portal left --data $'\e[31mHello\e[0m\r\n'
//!   prt-cli write-portal left --file replay.log
//!   prt-cli set-focus portal --id left
//!   prt-cli set-cursor-style hollow
//!   prt-cli delete-portal left
//!   prt-cli clear-all
//!
//! Outside a PRT-aware terminal the bytes appear as a stray APC
//! sequence which most terminals quietly ignore.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use prt_protocol::apc::ApcStream;
use prt_protocol::codec::Reader;
use prt_protocol::command::{
    AnchorMode, Command, CreatePortalBody, CursorStyle, FocusTarget, UpdateOriginBody,
    WritePortalBody,
};
use prt_protocol::encode::build_envelope;
use prt_protocol::frame::*;

#[derive(Debug, Parser)]
#[command(version, about = "Emit PRT protocol envelopes for manual testing")]
struct Cli {
    /// Request ID echoed back by the host in its response.
    #[arg(long, global = true, default_value_t = 0)]
    request_id: u32,

    /// Don't try to read the host's response. Without this off, response
    /// bytes get left in the TTY input buffer and the next shell prompt
    /// may render garbled.
    #[arg(long, global = true)]
    no_read: bool,

    /// Milliseconds to wait for the host's response before giving up.
    #[arg(long, global = true, default_value_t = 250)]
    timeout_ms: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Send a Probe (§2.1). Host responds with its caps.
    Probe,

    /// Remove every portal from the current host screen (§6.7).
    ClearAll,

    /// Create a portal (§6.1).
    CreatePortal {
        id: String,
        /// "WxH" in cell units.
        #[arg(long, value_parser = parse_size)]
        size: (u32, u32),
        /// "x,y" in cell units. Default 0,0.
        #[arg(long, value_parser = parse_origin, default_value = "0,0")]
        origin: (i32, i32),
        #[arg(long, default_value = "live")]
        anchor: AnchorArg,
        /// Initial visibility.
        #[arg(long, default_value_t = true)]
        visible: bool,
        #[arg(long, default_value_t = 0)]
        draw_order: i32,
        /// Per-portal scrollback ring size (host clamps to its cap).
        #[arg(long, default_value_t = 0)]
        scrollback_lines: u32,
    },

    /// Delete a portal (§6.2).
    DeletePortal { id: String },

    /// Resize a portal's grid (§6.3). Triggers ResizeNotify.
    UpdateSize {
        id: String,
        #[arg(long, value_parser = parse_size)]
        size: (u32, u32),
    },

    /// Re-anchor a portal (§6.4). `--anchor` must match the portal's
    /// current mode.
    UpdateOrigin {
        id: String,
        #[arg(long, value_parser = parse_origin)]
        origin: (i32, i32),
        #[arg(long, default_value = "live")]
        anchor: AnchorArg,
    },

    /// Toggle visibility (§6.5). Hidden portals still parse their byte
    /// stream; only rendering is suppressed.
    UpdateVisibility {
        id: String,
        #[arg(long)]
        visible: bool,
    },

    /// Set draw_order (§6.6).
    UpdateDrawOrder {
        id: String,
        #[arg(long)]
        draw_order: i32,
    },

    /// Feed bytes into a portal's vt100 (§7.1). The data source is
    /// `--data <STR>`, `--file <PATH>`, or stdin (default if neither
    /// is given). Stdin reads until EOF and chunks output internally;
    /// the wire batches into a single envelope of size up to
    /// max_write_bytes.
    WritePortal {
        id: String,
        #[arg(long)]
        data: Option<String>,
        #[arg(long)]
        file: Option<String>,
    },

    /// Tell the host where keyboard focus sits (§9.1). Affects cursor
    /// rendering only — input never crosses the PRT wire.
    SetFocus {
        #[command(subcommand)]
        target: FocusArg,
    },

    /// Configure host-wide unfocused-cursor policy (§9.2).
    SetCursorStyle { style: CursorStyleArg },
}

#[derive(Debug, Clone, ValueEnum)]
enum AnchorArg {
    Live,
    Scrollback,
}

impl From<AnchorArg> for AnchorMode {
    fn from(a: AnchorArg) -> Self {
        match a {
            AnchorArg::Live => AnchorMode::Live,
            AnchorArg::Scrollback => AnchorMode::Scrollback,
        }
    }
}

#[derive(Debug, Subcommand, Clone)]
enum FocusArg {
    /// Host owns focus.
    Host,
    /// A portal in the host's current scope owns focus.
    Portal {
        #[arg(long)]
        id: String,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum CursorStyleArg {
    Hidden,
    Hollow,
    Dim,
}

impl From<CursorStyleArg> for CursorStyle {
    fn from(s: CursorStyleArg) -> Self {
        match s {
            CursorStyleArg::Hidden => CursorStyle::Hidden,
            CursorStyleArg::Hollow => CursorStyle::Hollow,
            CursorStyleArg::Dim => CursorStyle::Dim,
        }
    }
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let w: u32 = w.parse().map_err(|e| format!("width: {e}"))?;
    let h: u32 = h.parse().map_err(|e| format!("height: {e}"))?;
    Ok((w, h))
}

fn parse_origin(s: &str) -> Result<(i32, i32), String> {
    let (x, y) = s
        .split_once(',')
        .ok_or_else(|| format!("expected x,y, got {s:?}"))?;
    let x: i32 = x.parse().map_err(|e| format!("x: {e}"))?;
    let y: i32 = y.parse().map_err(|e| format!("y: {e}"))?;
    Ok((x, y))
}

fn build_command(c: Cmd) -> Result<Command> {
    Ok(match c {
        Cmd::Probe => Command::Probe,
        Cmd::ClearAll => Command::ClearAll,
        Cmd::CreatePortal {
            id,
            size,
            origin,
            anchor,
            visible,
            draw_order,
            scrollback_lines,
        } => Command::CreatePortal(CreatePortalBody {
            id,
            size_w: size.0,
            size_h: size.1,
            origin_x: origin.0,
            origin_y: origin.1,
            anchor_mode: anchor.into(),
            is_visible: visible,
            draw_order,
            flags: 0,
            scrollback_lines,
        }),
        Cmd::DeletePortal { id } => Command::DeletePortal { id },
        Cmd::UpdateSize { id, size } => Command::UpdateSize {
            id,
            new_w: size.0,
            new_h: size.1,
        },
        Cmd::UpdateOrigin {
            id,
            origin,
            anchor,
        } => Command::UpdateOrigin(UpdateOriginBody {
            id,
            new_origin_x: origin.0,
            new_origin_y: origin.1,
            anchor_mode: anchor.into(),
        }),
        Cmd::UpdateVisibility { id, visible } => Command::UpdateVisibility {
            id,
            is_visible: visible,
        },
        Cmd::UpdateDrawOrder { id, draw_order } => {
            Command::UpdateDrawOrder { id, draw_order }
        }
        Cmd::WritePortal { id, data, file } => {
            let bytes = match (data, file) {
                (Some(_), Some(_)) => {
                    return Err(anyhow!("--data and --file are mutually exclusive"))
                }
                (Some(s), None) => s.into_bytes(),
                (None, Some(path)) => {
                    std::fs::read(&path).with_context(|| format!("reading {path}"))?
                }
                (None, None) => {
                    let mut buf = Vec::new();
                    std::io::stdin()
                        .read_to_end(&mut buf)
                        .context("reading stdin")?;
                    buf
                }
            };
            Command::WritePortal(WritePortalBody { id, data: bytes })
        }
        Cmd::SetFocus { target } => {
            let target = match target {
                FocusArg::Host => FocusTarget::Host,
                FocusArg::Portal { id } => FocusTarget::Portal(id),
            };
            Command::SetFocus { target }
        }
        Cmd::SetCursorStyle { style } => Command::SetCursorStyle {
            unfocused: style.into(),
        },
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = build_command(cli.cmd)?;
    let envelope = build_envelope(&[(cmd, cli.request_id)]);

    use std::io::IsTerminal;
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let want_response = !cli.no_read && stdin_is_tty && stdout_is_tty;

    // Raw mode protects the binary envelope on the way out and the
    // binary response on the way back: OPOST would translate \n →
    // \r\n; canon mode would buffer waiting for Enter; ECHO would
    // splatter the response into the visible scrollback; XON/XOFF
    // and CR/LF translation would mangle bytes inside the response
    // payload. We need raw mode whenever stdout is a TTY, regardless
    // of whether we're going to read the response back.
    let _guard = if stdout_is_tty {
        Some(RawTty::enable()?)
    } else {
        None
    };

    {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&envelope)
            .context("writing envelope to stdout")?;
        stdout.flush().ok();
    }

    if want_response {
        match read_response(Duration::from_millis(cli.timeout_ms))? {
            Some(frames) => {
                drop(_guard); // restore TTY before printing
                print_response(&frames);
            }
            None => {
                eprintln!("prt-cli: no response within {}ms", cli.timeout_ms);
            }
        }
    }

    Ok(())
}

/// Restore termios on drop.
struct RawTty {
    fd: std::os::fd::RawFd,
    saved: Option<nix::sys::termios::Termios>,
}

impl RawTty {
    fn enable() -> Result<Self> {
        use nix::sys::termios::{
            tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg,
        };
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        let saved = tcgetattr(&stdin).context("tcgetattr")?;
        let mut raw = saved.clone();
        raw.local_flags &= !(LocalFlags::ICANON
            | LocalFlags::ECHO
            | LocalFlags::ECHONL
            | LocalFlags::ISIG);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.input_flags &= !(InputFlags::IXON
            | InputFlags::IXOFF
            | InputFlags::INLCR
            | InputFlags::ICRNL
            | InputFlags::IGNCR);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr (raw)")?;
        Ok(Self {
            fd,
            saved: Some(saved),
        })
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            use nix::sys::termios::{tcsetattr, SetArg};
            let _ = unsafe {
                let borrowed = std::os::fd::BorrowedFd::borrow_raw(self.fd);
                tcsetattr(borrowed, SetArg::TCSANOW, &saved)
            };
        }
    }
}

/// Frame in a host-to-client envelope: (frame_type, request_id, body).
type ResponseFrame = (u8, u32, Vec<u8>);

/// Read response/event envelopes from stdin until either:
/// (a) we observe at least one envelope and then stdin is idle for
///     a short grace period, OR
/// (b) the wall-clock budget expires with nothing received.
///
/// Events may follow a response across one or more envelopes (§1.2);
/// the grace period catches them without inflating the timeout for
/// commands that produce no events.
fn read_response(timeout: Duration) -> Result<Option<Vec<ResponseFrame>>> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;

    let stdin = std::io::stdin();
    let stdin_fd = stdin.as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd) };

    let mut stream = ApcStream::with_marker(*MARKER_T2C);
    let mut frames: Vec<ResponseFrame> = Vec::new();
    let mut received_any = false;
    let start = Instant::now();
    // Once we've observed a response envelope, wait this much for
    // possible trailing events before returning.
    let post_event_grace = Duration::from_millis(20);

    loop {
        let elapsed = start.elapsed();
        let remaining = if !received_any {
            if elapsed >= timeout {
                break;
            }
            timeout - elapsed
        } else {
            post_event_grace
        };
        let timeout_ms: i32 = remaining
            .as_millis()
            .min(i32::MAX as u128)
            .try_into()
            .unwrap_or(0);

        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        let n = poll(&mut fds, PollTimeout::from(timeout_ms as u16)).context("poll")?;
        if n == 0 {
            // No more bytes within the deadline — we're done.
            break;
        }

        let mut buf = [0u8; 4096];
        let read = nix::unistd::read(stdin_fd, &mut buf).context("read stdin")?;
        if read == 0 {
            break;
        }
        let out = stream.feed(&buf[..read]);
        for payload in out.payloads {
            let mut r = Reader::new(&payload);
            let _version = r.u8().map_err(|_| anyhow!("short payload"))?;
            let _payload_len = r.u32().map_err(|_| anyhow!("short payload"))?;
            while !r.at_end() {
                let frame_type =
                    r.u8().map_err(|_| anyhow!("short frame header"))?;
                let request_id =
                    r.u32().map_err(|_| anyhow!("short frame header"))?;
                let body_len = r
                    .u32()
                    .map_err(|_| anyhow!("short frame header"))?
                    as usize;
                let body = r
                    .take(body_len)
                    .map_err(|_| anyhow!("short frame body"))?
                    .to_vec();
                frames.push((frame_type, request_id, body));
                received_any = true;
            }
        }
    }

    if frames.is_empty() {
        Ok(None)
    } else {
        Ok(Some(frames))
    }
}

fn print_response(frames: &[ResponseFrame]) {
    for (ft, rid, body) in frames {
        match *ft {
            RSP_OK => println!("Ok request_id={rid}"),
            RSP_ERR => print_err(*rid, body),
            RSP_PROBE => print_probe(*rid, body),
            EVT_RAW_REPLY => print_event_string_then_bytes("RawReply", body),
            EVT_BELL => print_event_string("Bell", body),
            EVT_TITLE_CHANGE => print_event_string_then_string("TitleChange", body),
            EVT_ICON_NAME_CHANGE => {
                print_event_string_then_string("IconNameChange", body)
            }
            EVT_WORKING_DIR_CHANGE => {
                print_event_string_then_string("WorkingDirChange", body)
            }
            EVT_CLIPBOARD_OP => print_clipboard_op(body),
            EVT_CURSOR_VISIBILITY_CHANGE => {
                print_event_string_then_u8("CursorVisibilityChange", body, "visible")
            }
            EVT_BUFFER_MODE_CHANGE => {
                print_event_string_then_u8("BufferModeChange", body, "on_alt")
            }
            EVT_PORTAL_EVICTED => print_portal_evicted(body),
            EVT_RESIZE_NOTIFY => print_resize_notify(body),
            EVT_MOUSE_MODE_CHANGE => print_mouse_mode_change(body),
            other => {
                println!(
                    "frame type=0x{other:02x} request_id={rid} body={}",
                    hex(body)
                );
            }
        }
    }
}

fn print_err(rid: u32, body: &[u8]) {
    let mut r = Reader::new(body);
    let code = r.u16().unwrap_or(0);
    let msg = r.string().unwrap_or("");
    println!("Err request_id={rid} code=0x{code:04x} message={msg:?}");
}

fn print_probe(rid: u32, body: &[u8]) {
    let mut r = Reader::new(body);
    let proto = r.u16().unwrap_or(0);
    let max_portals = r.u32().unwrap_or(0);
    let max_w = r.u32().unwrap_or(0);
    let max_h = r.u32().unwrap_or(0);
    let max_sb = r.u32().unwrap_or(0);
    let max_wr = r.u32().unwrap_or(0);
    let features = r.u8().unwrap_or(0);
    let max_nest = r.u8().unwrap_or(0);
    let vge_extra = r.u8().ok();
    println!(
        "ProbeResponse request_id={rid} proto={proto} max_portals={max_portals} \
         max_cells={max_w}x{max_h} max_scrollback_lines={max_sb} \
         max_write_bytes={max_wr} features=0x{features:02x} \
         max_nesting_depth={max_nest} vge_features={vge_extra:?}"
    );
}

fn print_event_string(label: &str, body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    println!("{label} id={id:?}");
}

fn print_event_string_then_string(label: &str, body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let v = r.string().unwrap_or("");
    println!("{label} id={id:?} value={v:?}");
}

fn print_event_string_then_bytes(label: &str, body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let data = r.bytes().unwrap_or(&[]);
    println!("{label} id={id:?} data={}", hex(data));
}

fn print_event_string_then_u8(label: &str, body: &[u8], field: &str) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let v = r.u8().unwrap_or(0);
    println!("{label} id={id:?} {field}={v}");
}

fn print_clipboard_op(body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let sel = r.u8().unwrap_or(0);
    let op = r.u8().unwrap_or(0);
    let data = r.bytes().unwrap_or(&[]);
    let op_label = match op {
        CLIPBOARD_SET => "set",
        CLIPBOARD_QUERY => "query",
        _ => "?",
    };
    println!(
        "ClipboardOp id={id:?} selection={:?} op={op_label} data={}",
        sel as char,
        hex(data)
    );
}

fn print_portal_evicted(body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let reason = r.u8().unwrap_or(0);
    let label = match reason {
        EVICT_SCROLLBACK => "scrollback",
        EVICT_ERASE => "erase",
        EVICT_ALT_SWAP => "alt_swap",
        _ => "?",
    };
    println!("PortalEvicted id={id:?} reason={label}");
}

fn print_resize_notify(body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let rows = r.u32().unwrap_or(0);
    let cols = r.u32().unwrap_or(0);
    println!("ResizeNotify id={id:?} rows={rows} cols={cols}");
}

fn print_mouse_mode_change(body: &[u8]) {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("");
    let proto = r.u8().unwrap_or(0);
    let enc = r.u8().unwrap_or(0);
    let focus = r.u8().unwrap_or(0);
    let proto_label = match proto {
        0 => "off",
        1 => "x10",
        2 => "normal",
        3 => "button",
        4 => "any",
        _ => "?",
    };
    let enc_label = match enc {
        0 => "default",
        1 => "utf8",
        2 => "sgr",
        3 => "urxvt",
        _ => "?",
    };
    println!(
        "MouseModeChange id={id:?} protocol={proto_label} encoding={enc_label} \
         focus_events={focus}"
    );
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2 + 2);
    s.push_str("0x");
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}
