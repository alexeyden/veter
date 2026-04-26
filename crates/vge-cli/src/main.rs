//! vge-cli — emit VGE protocol envelopes to stdout for manual testing.
//!
//! Usage examples (run inside a vterm session — stdout IS the PTY,
//! vterm reads it through its APC parser):
//!
//!   vge-cli probe
//!   vge-cli create-rect myrect --at 5,3 --size 10,5 --color ff0000ff
//!   vge-cli create-text label --at 0,0 --origin 40,5 --align center --text "hi"
//!   vge-cli set-style accent --color 00aaffff
//!   vge-cli create-rect themed --at 0,8 --size 20,3 --style-ref accent
//!   vge-cli fill-polygon tri --points 0,0 4,0 2,3 --color ffaa00ff
//!   vge-cli draw-lines plot --line-width 0.1 --color 00ff00ff \
//!       --segments 0,0:5,5 5,5:10,0
//!   vge-cli delete myrect
//!   vge-cli clear-all
//!
//! All bytes are written raw to stdout. Outside a VGE-aware terminal
//! the bytes appear as a stray APC sequence which most terminals
//! quietly ignore.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

use vge_protocol::apc::ApcStream;
use vge_protocol::codec::{Point, Reader, Rect};
use vge_protocol::command::{
    Align, Color, Command, ConcreteStyle, CreateElementBody, DrawCmd, FontStyle, Style,
    UpdateImageBody, UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::*;

#[derive(Debug, Parser)]
#[command(version, about = "Emit VGE protocol envelopes for manual testing")]
struct Cli {
    /// Request ID echoed back by the terminal in its response.
    #[arg(long, global = true, default_value_t = 0)]
    request_id: u32,

    /// Don't try to read the terminal's response. With this off, the
    /// response bytes get left in the TTY input buffer and the next
    /// shell prompt may render garbled.
    #[arg(long, global = true)]
    no_read: bool,

    /// Milliseconds to wait for the terminal's response before giving up.
    #[arg(long, global = true, default_value_t = 250)]
    timeout_ms: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Send a Probe (§2.1). The terminal responds with its limits.
    Probe,

    /// Clear all elements on the current screen (§6.7).
    ClearAll,

    /// Delete a single element by ID (§6.2).
    Delete {
        id: String,
    },

    /// Re-anchor an element's origin (§6.6).
    SetOrigin {
        id: String,
        /// "x,y" in cell units.
        #[arg(long, value_parser = parse_point)]
        origin: Point,
    },

    /// Toggle element visibility (§6.6).
    SetVisible {
        id: String,
        #[arg(long)]
        visible: bool,
    },

    /// Set the draw_order of an element (§6.6).
    SetDrawOrder {
        id: String,
        #[arg(long)]
        draw_order: i32,
    },

    /// Upsert a style into the global style table (§7.3).
    SetStyle(SetStyleArgs),

    /// CreateElement with a single FillRectangles command.
    CreateRect(CreateRectArgs),

    /// CreateElement with a single DrawText command.
    CreateText(CreateTextArgs),

    /// CreateElement with a single FillPolygon command.
    FillPolygon(FillPolygonArgs),

    /// CreateElement with a single DrawLines command (independent
    /// segments).
    DrawLines(DrawLinesArgs),

    /// CreateElement with a single DrawLineStrip command (open polyline).
    DrawLineStrip(DrawLineStripArgs),

    /// Upload a raw RGBA8 image (§8.1, encoding 0x01). The file must
    /// contain exactly width*height*4 straight-alpha bytes.
    UploadRaw(UploadRawArgs),

    /// Upload a WebP image (§8.1, encoding 0x02). The CLI peeks the
    /// dimensions from the file header so the user doesn't have to.
    UploadWebp(UploadWebpArgs),

    /// Drop an uploaded image from the table (§8.2).
    DropImage { id: String },

    /// CreateElement with a single DrawImage command (§7.5).
    CreateImage(CreateImageArgs),

    /// Swap the image referenced by a DrawImage command (§6.5).
    UpdateImage {
        /// Element ID.
        id: String,
        #[arg(long, default_value_t = 0)]
        index: u32,
        /// New image table id to point at.
        #[arg(long)]
        image: String,
    },
}

#[derive(Debug, clap::Args)]
struct StyleArgs {
    /// Flat color "RRGGBBAA" hex.
    #[arg(long, value_parser = parse_color, group = "style")]
    color: Option<Color>,

    /// Reference an entry in the global style table by ID.
    #[arg(long, group = "style")]
    style_ref: Option<String>,

    /// Two-stop linear gradient: "x0,y0:x1,y1:RRGGBBAA:RRGGBBAA"
    #[arg(long, value_parser = parse_linear_gradient, group = "style")]
    linear: Option<Style>,

    /// Two-stop radial gradient: "cx,cy:ox,oy:RRGGBBAA:RRGGBBAA"
    #[arg(long, value_parser = parse_radial_gradient, group = "style")]
    radial: Option<Style>,
}

impl StyleArgs {
    fn into_style(self, default_color: Color) -> Style {
        if let Some(c) = self.color {
            Style::Flat(c)
        } else if let Some(id) = self.style_ref {
            Style::Ref(id)
        } else if let Some(s) = self.linear {
            s
        } else if let Some(s) = self.radial {
            s
        } else {
            Style::Flat(default_color)
        }
    }
}

#[derive(Debug, clap::Args)]
struct SetStyleArgs {
    id: String,
    /// Flat color "RRGGBBAA". For gradients, use --linear / --radial.
    #[arg(long, value_parser = parse_color, group = "style")]
    color: Option<Color>,
    #[arg(long, value_parser = parse_linear_gradient, group = "style")]
    linear: Option<Style>,
    #[arg(long, value_parser = parse_radial_gradient, group = "style")]
    radial: Option<Style>,
}

#[derive(Debug, clap::Args)]
struct CreateRectArgs {
    id: String,
    /// Element origin "x,y" in cell units.
    #[arg(long = "at", value_parser = parse_point, default_value = "0,0")]
    at: Point,
    /// Rect size "w,h" in cell units (relative to the element origin).
    #[arg(long, value_parser = parse_point, default_value = "1,1")]
    size: Point,
    /// Rect offset within the element "dx,dy" (defaults to 0,0).
    #[arg(long, value_parser = parse_point, default_value = "0,0")]
    offset: Point,
    #[arg(long, default_value_t = 0)]
    draw_order: i32,
    #[command(flatten)]
    style: StyleArgs,
}

#[derive(Debug, clap::Args)]
struct CreateTextArgs {
    id: String,
    /// Element origin "x,y" in cell units.
    #[arg(long = "at", value_parser = parse_point, default_value = "0,0")]
    at: Point,
    /// Text origin within the element (the alignment anchor).
    #[arg(long, value_parser = parse_point, default_value = "0,0")]
    origin: Point,
    #[arg(long, value_parser = parse_align, default_value = "left")]
    align: Align,
    /// Bitmask of font-style bits: 1=bold 2=italic 4=underline 8=strikethrough.
    #[arg(long, default_value_t = 0)]
    font_style: u8,
    #[arg(long)]
    text: String,
    #[arg(long, default_value_t = 0)]
    draw_order: i32,
    #[command(flatten)]
    style: StyleArgs,
}

#[derive(Debug, clap::Args)]
struct FillPolygonArgs {
    id: String,
    #[arg(long = "at", value_parser = parse_point, default_value = "0,0")]
    at: Point,
    /// At least 3 points; format: "x,y" each, space-separated.
    #[arg(long, value_parser = parse_point, num_args = 3.., required = true)]
    points: Vec<Point>,
    #[arg(long, default_value_t = 0)]
    draw_order: i32,
    #[command(flatten)]
    style: StyleArgs,
}

#[derive(Debug, clap::Args)]
struct DrawLinesArgs {
    id: String,
    #[arg(long = "at", value_parser = parse_point, default_value = "0,0")]
    at: Point,
    /// Independent line segments: "x1,y1:x2,y2" each, space-separated.
    #[arg(long, value_parser = parse_segment, num_args = 1.., required = true)]
    segments: Vec<(Point, Point)>,
    #[arg(long, default_value_t = 0.05)]
    line_width: f32,
    #[arg(long, default_value_t = 0)]
    draw_order: i32,
    #[command(flatten)]
    style: StyleArgs,
}

#[derive(Debug, clap::Args)]
struct DrawLineStripArgs {
    id: String,
    #[arg(long = "at", value_parser = parse_point, default_value = "0,0")]
    at: Point,
    /// At least 2 points; format: "x,y" each, space-separated.
    #[arg(long, value_parser = parse_point, num_args = 2.., required = true)]
    points: Vec<Point>,
    #[arg(long, default_value_t = 0.05)]
    line_width: f32,
    #[arg(long, default_value_t = 0)]
    draw_order: i32,
    #[command(flatten)]
    style: StyleArgs,
}

#[derive(Debug, clap::Args)]
struct UploadRawArgs {
    id: String,
    #[arg(long)]
    width: u32,
    #[arg(long)]
    height: u32,
    /// Path to a file containing exactly width*height*4 RGBA8 bytes.
    #[arg(long)]
    file: std::path::PathBuf,
}

#[derive(Debug, clap::Args)]
struct UploadWebpArgs {
    id: String,
    /// Path to a complete WebP file. CLI reads the dimensions and
    /// embeds them in the wire envelope.
    #[arg(long)]
    file: std::path::PathBuf,
}

#[derive(Debug, clap::Args)]
struct CreateImageArgs {
    /// Element ID for the new element.
    id: String,
    /// ID of the uploaded image to draw.
    #[arg(long)]
    image: String,
    /// Element origin "x,y" in cell units.
    #[arg(long = "at", value_parser = parse_point, default_value = "0,0")]
    at: Point,
    /// Target rect size "w,h" in cell units (relative to element origin).
    #[arg(long, value_parser = parse_point, default_value = "1,1")]
    size: Point,
    /// Target rect offset within the element "dx,dy".
    #[arg(long, value_parser = parse_point, default_value = "0,0")]
    offset: Point,
    #[arg(long, default_value_t = 0)]
    draw_order: i32,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cmd = build_command(cli.cmd)?;
    let envelope = build_envelope(&[(cmd, cli.request_id)]);

    use std::io::IsTerminal;
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdout_is_tty = std::io::stdout().is_terminal();
    let want_response = !cli.no_read && stdin_is_tty && stdout_is_tty;

    // Put the TTY in raw mode *before* writing, so that:
    //  - the kernel doesn't rewrite our binary output (OPOST/ONLCR
    //    would turn 0x0A into 0x0D 0x0A, corrupting WebP bodies and
    //    any other byte stream that happens to contain newlines);
    //  - any response arriving while we're still in the foreground
    //    doesn't get echoed by the line discipline or buffered as a
    //    "line" awaiting Enter.
    // We need raw mode whenever stdout is a TTY, regardless of whether
    // we're going to read the response back.
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
                drop(_guard); // restore TTY before printing.
                print_response(&frames);
            }
            None => {
                // Timeout — silent. Either no VGE-aware terminal is
                // listening, or the response hasn't arrived yet.
                eprintln!("vge-cli: no response within {}ms", cli.timeout_ms);
            }
        }
    }

    Ok(())
}

/// Restore termios on drop. The `Option<termios>` lets us steal the
/// state via `take()` to avoid double restoration.
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
        // Disable canonical mode, echo, and signal generation so
        // response bytes flow straight through.
        raw.local_flags &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ISIG);
        // Disable output post-processing — without this the kernel
        // rewrites our binary output (\n -> \r\n via ONLCR), which
        // corrupts WebP and any other binary payload whose contents
        // happen to contain a 0x0A byte.
        raw.output_flags &= !OutputFlags::OPOST;
        // Disable input flags that would mangle the response bytes:
        // XON/XOFF flow control would consume 0x11/0x13, and CR/LF
        // translation would alter 0x0A/0x0D in the response payload.
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
            // SAFETY: stdin's fd outlives the program; tcsetattr is
            // legal on any open TTY descriptor.
            use nix::sys::termios::{tcsetattr, SetArg};
            // Borrow the fd via std::io::stdin() — same descriptor.
            let stdin = std::io::stdin();
            let _ = stdin; // keep the borrow alive
            let _ = unsafe {
                let borrowed = std::os::fd::BorrowedFd::borrow_raw(self.fd);
                tcsetattr(borrowed, SetArg::TCSANOW, &saved)
            };
        }
    }
}

type ResponseFrame = (u8, u32, Vec<u8>);

struct ProbeFields {
    proto: u16,
    cw: u16,
    ch: u16,
    scale: f32,
    max_el: u32,
    max_cmds: u32,
    max_text: u32,
    max_img_bytes: u32,
    max_imgs: u32,
    encs: u8,
}

fn read_response(timeout: Duration) -> Result<Option<Vec<ResponseFrame>>> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    // SAFETY: stdin lives for the program; we only borrow for the
    // duration of poll/read.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };

    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let remaining = deadline - now;
        let ms: i32 = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        let n = poll(&mut fds, PollTimeout::from(ms as u16)).context("poll")?;
        if n == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; 4096];
        let nread = nix::unistd::read(fd, &mut buf).context("read")?;
        if nread == 0 {
            return Ok(None);
        }
        let out = apc.feed(&buf[..nread]);
        if let Some(payload) = out.payloads.into_iter().next() {
            return Ok(Some(parse_payload(&payload)?));
        }
    }
}

/// Walk the unstuffed response payload and return its frames as
/// `(frame_type, request_id, body)` tuples.
fn parse_payload(payload: &[u8]) -> Result<Vec<ResponseFrame>> {
    let mut r = Reader::new(payload);
    let version = r
        .u8()
        .map_err(|_| anyhow!("response truncated at version byte"))?;
    if version > PROTOCOL_VERSION {
        bail!("response declares unsupported protocol_version {version}");
    }
    let _payload_len = r
        .u32()
        .map_err(|_| anyhow!("response truncated at length field"))?;

    let mut frames = Vec::new();
    while !r.at_end() {
        let ty = r.u8().map_err(|_| anyhow!("frame truncated at type"))?;
        let req = r.u32().map_err(|_| anyhow!("frame truncated at req_id"))?;
        let body_len = r
            .u32()
            .map_err(|_| anyhow!("frame truncated at body_len"))? as usize;
        let body = r
            .take(body_len)
            .map_err(|_| anyhow!("frame body truncated"))?
            .to_vec();
        frames.push((ty, req, body));
    }
    Ok(frames)
}

fn print_response(frames: &[ResponseFrame]) {
    for (ty, req_id, body) in frames {
        match *ty {
            RSP_OK => println!("Ok (request_id={req_id})"),
            RSP_ERR => {
                let mut r = Reader::new(body);
                let code = r.u16().unwrap_or(0xFFFF);
                let msg = r.string().unwrap_or("");
                println!(
                    "Err (request_id={req_id}, code=0x{code:04X}{})",
                    if msg.is_empty() {
                        String::new()
                    } else {
                        format!(", message={msg:?}")
                    }
                );
            }
            RSP_PROBE => print_probe_body(*req_id, body),
            other => println!("Unknown response frame type 0x{other:02X} (request_id={req_id})"),
        }
    }
}

fn print_probe_body(req_id: u32, body: &[u8]) {
    let mut r = Reader::new(body);
    let parsed = (|| {
        Some(ProbeFields {
            proto: r.u16().ok()?,
            cw: r.u16().ok()?,
            ch: r.u16().ok()?,
            scale: r.f32().ok()?,
            max_el: r.u32().ok()?,
            max_cmds: r.u32().ok()?,
            max_text: r.u32().ok()?,
            max_img_bytes: r.u32().ok()?,
            max_imgs: r.u32().ok()?,
            encs: r.u8().ok()?,
        })
    })();
    if let Some(ProbeFields {
        proto,
        cw,
        ch,
        scale,
        max_el,
        max_cmds,
        max_text,
        max_img_bytes,
        max_imgs,
        encs,
    }) = parsed
    {
        println!("Probe (request_id={req_id}):");
        println!("  protocol_version           = {proto}");
        println!("  cell_pixel_width           = {cw}");
        println!("  cell_pixel_height          = {ch}");
        println!("  scale_factor               = {scale}");
        println!("  max_elements               = {max_el}");
        println!("  max_commands_per_element   = {max_cmds}");
        println!("  max_text_bytes             = {max_text}");
        println!("  max_image_bytes            = {max_img_bytes}");
        println!("  max_images                 = {max_imgs}");
        println!("  supported_image_encodings  = 0x{encs:02X}");
    } else {
        println!("Probe (request_id={req_id}): malformed body, {} bytes", body.len());
    }
}

fn build_command(cmd: Cmd) -> Result<Command> {
    Ok(match cmd {
        Cmd::Probe => Command::Probe,
        Cmd::ClearAll => Command::ClearAll,
        Cmd::Delete { id } => Command::DeleteElement { id },
        Cmd::SetOrigin { id, origin } => Command::UpdateOrigin { id, origin },
        Cmd::SetVisible { id, visible } => Command::UpdateVisibility {
            id,
            is_visible: visible,
        },
        Cmd::SetDrawOrder { id, draw_order } => Command::UpdateDrawOrder { id, draw_order },
        Cmd::SetStyle(a) => {
            let style = if let Some(c) = a.color {
                ConcreteStyle::Flat(c)
            } else if let Some(s) = a.linear {
                ConcreteStyle::from_style(s).map_err(|_| anyhow!("invalid gradient"))?
            } else if let Some(s) = a.radial {
                ConcreteStyle::from_style(s).map_err(|_| anyhow!("invalid gradient"))?
            } else {
                bail!("set-style requires --color, --linear, or --radial");
            };
            Command::SetGlobalStyle { id: a.id, style }
        }
        Cmd::CreateRect(a) => Command::CreateElement(CreateElementBody {
            id: a.id,
            commands: vec![DrawCmd::FillRectangles {
                fill: a.style.into_style(white()),
                rects: vec![Rect {
                    x: a.offset.x,
                    y: a.offset.y,
                    w: a.size.x,
                    h: a.size.y,
                }],
            }],
            origin: a.at,
            is_visible: true,
            draw_order: a.draw_order,
        }),
        Cmd::CreateText(a) => Command::CreateElement(CreateElementBody {
            id: a.id,
            commands: vec![DrawCmd::DrawText {
                origin: a.origin,
                align: a.align,
                fill: a.style.into_style(white()),
                font_style: FontStyle(a.font_style),
                text: a.text,
            }],
            origin: a.at,
            is_visible: true,
            draw_order: a.draw_order,
        }),
        Cmd::FillPolygon(a) => Command::CreateElement(CreateElementBody {
            id: a.id,
            commands: vec![DrawCmd::FillPolygon {
                fill: a.style.into_style(white()),
                points: a.points,
            }],
            origin: a.at,
            is_visible: true,
            draw_order: a.draw_order,
        }),
        Cmd::DrawLines(a) => Command::CreateElement(CreateElementBody {
            id: a.id,
            commands: vec![DrawCmd::DrawLines {
                stroke: a.style.into_style(white()),
                line_width: a.line_width,
                lines: a.segments,
            }],
            origin: a.at,
            is_visible: true,
            draw_order: a.draw_order,
        }),
        Cmd::DrawLineStrip(a) => Command::CreateElement(CreateElementBody {
            id: a.id,
            commands: vec![DrawCmd::DrawLineStrip {
                stroke: a.style.into_style(white()),
                line_width: a.line_width,
                points: a.points,
            }],
            origin: a.at,
            is_visible: true,
            draw_order: a.draw_order,
        }),
        Cmd::UploadRaw(a) => {
            let data = std::fs::read(&a.file)
                .with_context(|| format!("reading {}", a.file.display()))?;
            let expected = (a.width as usize) * (a.height as usize) * 4;
            if data.len() != expected {
                bail!(
                    "file size {} bytes != width*height*4 = {} bytes",
                    data.len(),
                    expected
                );
            }
            Command::UploadImage(UploadImageBody {
                id: a.id,
                encoding: 0x01,
                width: a.width,
                height: a.height,
                data,
            })
        }
        Cmd::UploadWebp(a) => {
            let data = std::fs::read(&a.file)
                .with_context(|| format!("reading {}", a.file.display()))?;
            // Peek dimensions so the user doesn't have to pass them.
            let img = image::load_from_memory_with_format(&data, image::ImageFormat::WebP)
                .context("decoding WebP for dimension check")?;
            Command::UploadImage(UploadImageBody {
                id: a.id,
                encoding: 0x02,
                width: img.width(),
                height: img.height(),
                data,
            })
        }
        Cmd::DropImage { id } => Command::DropImage { id },
        Cmd::CreateImage(a) => Command::CreateElement(CreateElementBody {
            id: a.id,
            commands: vec![DrawCmd::DrawImage {
                target_rect: vge_protocol::codec::Rect {
                    x: a.offset.x,
                    y: a.offset.y,
                    w: a.size.x,
                    h: a.size.y,
                },
                image_id: a.image,
            }],
            origin: a.at,
            is_visible: true,
            draw_order: a.draw_order,
        }),
        Cmd::UpdateImage { id, index, image } => Command::UpdateImage(UpdateImageBody {
            id,
            command_index: index as usize,
            new_image_id: image,
        }),
    })
}

fn white() -> Color {
    Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    }
}

// --- value parsers ---

fn parse_point(s: &str) -> Result<Point, String> {
    let (x, y) = s
        .split_once(',')
        .ok_or_else(|| format!("expected x,y (got {s:?})"))?;
    let x: f32 = x.trim().parse().map_err(|e| format!("bad x: {e}"))?;
    let y: f32 = y.trim().parse().map_err(|e| format!("bad y: {e}"))?;
    Ok(Point { x, y })
}

fn parse_segment(s: &str) -> Result<(Point, Point), String> {
    let (a, b) = s
        .split_once(':')
        .ok_or_else(|| format!("expected x1,y1:x2,y2 (got {s:?})"))?;
    Ok((parse_point(a)?, parse_point(b)?))
}

fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 8 {
        return Err(format!("expected RRGGBBAA hex (8 chars), got {} chars", s.len()));
    }
    let parse_byte = |idx: usize| -> Result<u8, String> {
        u8::from_str_radix(&s[idx..idx + 2], 16).map_err(|e| format!("bad hex at {idx}: {e}"))
    };
    let r = parse_byte(0)?;
    let g = parse_byte(2)?;
    let b = parse_byte(4)?;
    let a = parse_byte(6)?;
    Ok(Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    })
}

fn parse_linear_gradient(s: &str) -> Result<Style, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 4 {
        return Err(format!("expected x0,y0:x1,y1:c0:c1 (got {s:?})"));
    }
    Ok(Style::LinearGradient {
        p0: parse_point(parts[0])?,
        p1: parse_point(parts[1])?,
        c0: parse_color(parts[2])?,
        c1: parse_color(parts[3])?,
    })
}

fn parse_radial_gradient(s: &str) -> Result<Style, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 4 {
        return Err(format!("expected cx,cy:ox,oy:c0:c1 (got {s:?})"));
    }
    Ok(Style::RadialGradient {
        center: parse_point(parts[0])?,
        outer: parse_point(parts[1])?,
        c_inner: parse_color(parts[2])?,
        c_outer: parse_color(parts[3])?,
    })
}

fn parse_align(s: &str) -> Result<Align, String> {
    match s.to_ascii_lowercase().as_str() {
        "left" | "l" => Ok(Align::Left),
        "center" | "centre" | "c" => Ok(Align::Center),
        "right" | "r" => Ok(Align::Right),
        _ => Err(format!("expected left|center|right, got {s:?}")),
    }
}
