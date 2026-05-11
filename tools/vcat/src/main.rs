//! vcat — print an image to a VGE-aware terminal.
//!
//! Pipeline:
//!   1. Decode the image (PNG, JPEG, WebP) via the `image` crate.
//!   2. Probe the running terminal for its cell pixel dimensions.
//!   3. Query the kernel for terminal column count (TIOCGWINSZ) so we
//!      can clamp display width.
//!   4. Compute target cell width and height that preserves the image's
//!      visual aspect ratio on this terminal's anisotropic cell grid.
//!   5. Resize to exact pixel dimensions matching that cell footprint
//!      (Lanczos), upload as a Raw RGBA8 VGE image, and create an
//!      element placed where the next prompt would have been.
//!
//! Run inside veter:
//!     vcat ~/Downloads/photo.jpg
//!     vcat --width 40 logo.png

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use image::ImageReader;
use vge_protocol::apc::ApcStream;
use vge_protocol::codec::{Point, Reader, Rect};
use vge_protocol::command::{Command, CreateElementBody, DrawCmd, UploadImageBody};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::*;

#[derive(Parser, Debug)]
#[command(version, about = "Display an image inside a VGE-aware terminal.")]
struct Cli {
    /// Path to a PNG, JPEG, or WebP file.
    file: PathBuf,

    /// Force the displayed image width in cell units. Without this
    /// flag, vcat uses the image's natural pixel width divided by the
    /// terminal's cell pixel width, clamped to the terminal column
    /// count.
    #[arg(long)]
    width: Option<u32>,

    /// Milliseconds to wait for the terminal's probe / cursor
    /// responses before giving up.
    #[arg(long, default_value_t = 500)]
    timeout_ms: u64,

    /// Print progress to stderr at each pipeline stage.
    #[arg(short, long)]
    verbose: bool,

    /// Wire encoding for the uploaded image. `raw` sends straight
    /// RGBA8 bytes (fastest to encode, biggest payload).
    /// `webp-lossless` and `webp-lossy` both ride the pure-Rust
    /// `oxideav-webp` encoder so the workspace builds clean against
    /// `aarch64-unknown-linux-musl` without a C cross-compiler.
    /// Lossy quality is controlled by `--quality` (0..=100).
    #[arg(long, value_enum, default_value_t = Mode::Raw)]
    mode: Mode,

    /// Quality for `--mode webp-lossy`, in 0..=100. Ignored for the
    /// other modes.
    #[arg(long, default_value_t = 75.0)]
    quality: f32,
}

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
enum Mode {
    Raw,
    WebpLossless,
    WebpLossy,
}

macro_rules! trace {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            eprintln!("[vcat] {}", format!($($arg)*));
        }
    };
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let v = cli.verbose;

    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vcat must run with stdin and stdout connected to a terminal");
    }

    trace!(v, "decoding {}", cli.file.display());
    let dyn_img = ImageReader::open(&cli.file)
        .with_context(|| format!("opening {}", cli.file.display()))?
        .with_guessed_format()
        .with_context(|| format!("inspecting {}", cli.file.display()))?
        .decode()
        .with_context(|| format!("decoding {}", cli.file.display()))?;
    let rgba = dyn_img.to_rgba8();
    let (w_px, h_px) = rgba.dimensions();
    if w_px == 0 || h_px == 0 {
        bail!("image has zero extent");
    }
    trace!(v, "decoded {w_px}x{h_px} px");

    let _guard = RawTty::enable()?;

    drain_stale_stdin();
    trace!(v, "probing");
    let probe = run_probe(Duration::from_millis(cli.timeout_ms))?
        .ok_or_else(|| anyhow!("VGE probe timed out — terminal does not appear to support VGE"))?;
    let cell_pw = probe.cell_pixel_width.max(1) as f32;
    let cell_ph = probe.cell_pixel_height.max(1) as f32;
    trace!(v, "probe ok: cells={cell_pw}x{cell_ph}");

    let term_cols = winsize_cols().unwrap_or(80) as u32;
    trace!(v, "term_cols={term_cols}");

    let placement = compute_placement(w_px, h_px, cell_pw, cell_ph, term_cols, cli.width);
    trace!(
        v,
        "placement: {}x{} cells, target_rect_h={:.3}, pixels {}x{}",
        placement.w_cells,
        placement.h_cells,
        placement.target_rect_h,
        placement.target_px_w,
        placement.target_px_h
    );

    trace!(v, "resizing");
    let resized = image::imageops::resize(
        &rgba,
        placement.target_px_w,
        placement.target_px_h,
        image::imageops::FilterType::Lanczos3,
    );
    trace!(v, "resized");

    // Reserve vertical space and read back the cursor's new row.
    let mut stdout = std::io::stdout().lock();
    for _ in 0..placement.h_cells {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    trace!(v, "querying cursor");
    stdout.write_all(b"\x1b[6n")?;
    stdout.flush()?;
    let cursor_row = read_cursor_row(Duration::from_millis(cli.timeout_ms))?
        .ok_or_else(|| anyhow!("cursor-position query timed out"))?;
    trace!(v, "cursor row={cursor_row}");
    // After printing h_cells newlines the cursor is at row C (1-indexed,
    // top of screen = 1). The image should occupy rows
    // [C - h_cells, C) in 1-indexed terms, which is origin.y =
    // C - h_cells - 1 in VGE 0-indexed cells from the live screen top.
    //
    // For tall images (h_cells > rows-from-original-cursor) vt100
    // scrolled while we were printing newlines, so the image's correct
    // anchor line is in the scrollback we just produced — i.e. origin.y
    // is negative. VGE elements with negative origin.y are anchored to
    // scrollback lines that have already passed off-screen at the top
    // (§5.2); rendering automatically clips the image to whatever
    // portion is currently visible. Don't clamp to 0 here — that would
    // pin the image to the top of the live screen and shove its bottom
    // edge past the prompt.
    let origin_y = (cursor_row as i32 - placement.h_cells as i32 - 1) as f32;

    // Build and write the VGE envelope.
    let pid = std::process::id();
    let img_id = format!("vcat-img-{pid}");
    let elem_id = format!("vcat-el-{pid}");
    trace!(v, "encoding mode={:?}", cli.mode);
    let raw_rgba = resized.into_raw();
    let (encoding, payload) = match cli.mode {
        Mode::Raw => (0x01u8, raw_rgba),
        Mode::WebpLossless => {
            // oxideav-webp's lossless path expects packed ARGB u32s.
            let argb: Vec<u32> = raw_rgba
                .chunks_exact(4)
                .map(|c| {
                    ((c[3] as u32) << 24)
                        | ((c[0] as u32) << 16)
                        | ((c[1] as u32) << 8)
                        | (c[2] as u32)
                })
                .collect();
            let out = oxideav_webp::encode_vp8l_argb(
                placement.target_px_w,
                placement.target_px_h,
                &argb,
                /* has_alpha */ true,
            )
            .context("webp lossless encode")?;
            trace!(v, "webp lossless: {} -> {} bytes", raw_rgba.len(), out.len());
            (0x02u8, out)
        }
        Mode::WebpLossy => {
            if !cli.quality.is_finite() || !(0.0..=100.0).contains(&cli.quality) {
                bail!("--quality must be in 0..=100, got {}", cli.quality);
            }
            let out = oxideav_webp::encode_vp8_lossy_rgba(
                placement.target_px_w,
                placement.target_px_h,
                &raw_rgba,
                cli.quality,
                &oxideav_webp::WebpMetadata::default(),
            )
            .context("webp lossy encode")?;
            trace!(
                v,
                "webp lossy q={}: {} -> {} bytes",
                cli.quality,
                raw_rgba.len(),
                out.len()
            );
            (0x02u8, out)
        }
    };
    let upload = Command::UploadImage(UploadImageBody {
        id: img_id.clone(),
        encoding,
        width: placement.target_px_w,
        height: placement.target_px_h,
        data: payload,
    });
    let create = Command::CreateElement(CreateElementBody {
        id: elem_id,
        commands: vec![DrawCmd::DrawImage {
            target_rect: Rect {
                x: 0.0,
                y: 0.0,
                w: placement.w_cells as f32,
                h: placement.target_rect_h,
            },
            image_id: img_id,
        }],
        origin: Point {
            x: 0.0,
            y: origin_y,
        },
        is_visible: true,
        draw_order: 0,
        parent: None,
        size: None,
    });
    let envelope = build_envelope(&[(upload, 1), (create, 2)]);
    trace!(v, "writing envelope: {} bytes", envelope.len());
    stdout.write_all(&envelope)?;
    stdout.flush()?;
    drop(stdout);
    trace!(v, "envelope written, draining response");

    let drained = drain_response_envelope(Duration::from_millis(cli.timeout_ms))?;
    trace!(v, "drain result: {drained}");

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Placement {
    /// Width of the rendered image in cells. Used both for the
    /// `target_rect.w` and to bound terminal-column reservation.
    w_cells: u32,
    /// `target_rect.h` in cells — fractional, set so the image keeps
    /// its true visual aspect ratio on this anisotropic cell grid.
    /// Ranges over (0, h_cells].
    target_rect_h: f32,
    /// Number of full rows to reserve via newlines. Equal to
    /// `target_rect_h.ceil()`. The bottom (h_cells - target_rect_h)
    /// fraction of a cell is empty whitespace below the image.
    h_cells: u32,
    /// Exact pixel target for resizing — preserves the image's pixel
    /// aspect ratio; the renderer stretches this onto target_rect.
    target_px_w: u32,
    target_px_h: u32,
}

/// Compute the cell footprint and exact pixel target for an image of
/// `w_px × h_px` displayed on a terminal with `cell_pw × cell_ph` pixel
/// cells and `term_cols` columns. If `forced_w_cells` is set, that's the
/// width; otherwise width is the image's natural width in cells clamped
/// to terminal columns.
fn compute_placement(
    w_px: u32,
    h_px: u32,
    cell_pw: f32,
    cell_ph: f32,
    term_cols: u32,
    forced_w_cells: Option<u32>,
) -> Placement {
    let cell_pw = cell_pw.max(1.0);
    let cell_ph = cell_ph.max(1.0);

    let natural_w_cells = ((w_px as f32) / cell_pw).ceil().max(1.0) as u32;
    let max_w_cells = match forced_w_cells {
        Some(w) if w > 0 => w,
        _ => term_cols.max(1),
    };
    let w_cells = natural_w_cells.min(max_w_cells).max(1);

    // Pixel target preserves the image's true aspect: we draw the
    // image at its natural ratio, and let target_rect.h be a
    // fractional number of cells so anisotropic cell grids don't
    // distort it.
    let target_px_w = (w_cells as f32 * cell_pw).round().max(1.0) as u32;
    let target_px_h =
        (target_px_w as f32 * h_px as f32 / w_px as f32).round().max(1.0) as u32;
    let target_rect_h = (target_px_h as f32 / cell_ph).max(1.0 / cell_ph);
    let h_cells = target_rect_h.ceil().max(1.0) as u32;

    Placement {
        w_cells,
        target_rect_h,
        h_cells,
        target_px_w,
        target_px_h,
    }
}

// --- terminal I/O helpers ---

/// Read up to one full T2C response envelope and discard it. Used
/// after the final UploadImage+CreateElement to keep response bytes
/// from leaking to the next program's stdin.
fn drain_response_envelope(timeout: Duration) -> Result<bool> {
    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        if !poll_stdin_until(deadline)? {
            return Ok(false);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        let out = apc.feed(&buf[..n]);
        if !out.payloads.is_empty() {
            return Ok(true);
        }
    }
}

/// Pull anything currently sitting on stdin without blocking. Used at
/// startup as a recovery measure if a previous run left bytes
/// unconsumed.
fn drain_stale_stdin() {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::ZERO) {
            Ok(n) if n > 0 => {
                if read_stdin(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
            _ => break,
        }
    }
}

/// Probe the terminal for its cell pixel dimensions.
fn run_probe(timeout: Duration) -> Result<Option<ProbeData>> {
    let env = build_envelope(&[(Command::Probe, 1)]);
    {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&env)?;
        stdout.flush()?;
    }

    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        if !poll_stdin_until(deadline)? {
            return Ok(None);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let out = apc.feed(&buf[..n]);
        if let Some(payload) = out.payloads.into_iter().next() {
            return Ok(Some(parse_probe_payload(&payload)?));
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProbeData {
    cell_pixel_width: u16,
    cell_pixel_height: u16,
}

fn parse_probe_payload(payload: &[u8]) -> Result<ProbeData> {
    let mut r = Reader::new(payload);
    let _version = r.u8().map_err(|_| anyhow!("probe payload: missing version"))?;
    let _payload_len = r.u32().map_err(|_| anyhow!("probe payload: missing length"))?;
    let frame_type = r.u8().map_err(|_| anyhow!("probe payload: missing frame type"))?;
    if frame_type != RSP_PROBE {
        bail!(
            "expected ProbeResponse (0x{:02X}), got 0x{:02X}",
            RSP_PROBE,
            frame_type
        );
    }
    let _req_id = r.u32().map_err(|_| anyhow!("probe payload: missing request_id"))?;
    let _body_len = r.u32().map_err(|_| anyhow!("probe payload: missing body_len"))?;
    let _proto = r.u16().map_err(|_| anyhow!("probe body: protocol_version"))?;
    let cw = r.u16().map_err(|_| anyhow!("probe body: cell_pixel_width"))?;
    let ch = r.u16().map_err(|_| anyhow!("probe body: cell_pixel_height"))?;
    Ok(ProbeData {
        cell_pixel_width: cw,
        cell_pixel_height: ch,
    })
}

/// Read bytes from stdin until we see a CSI cursor-position-report
/// terminator (`ESC [ <row> ; <col> R`). Returns the row, 1-indexed.
fn read_cursor_row(timeout: Duration) -> Result<Option<u32>> {
    let deadline = Instant::now() + timeout;
    let mut accum: Vec<u8> = Vec::with_capacity(32);
    let mut buf = [0u8; 64];
    loop {
        if !poll_stdin_until(deadline)? {
            return Ok(None);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        accum.extend_from_slice(&buf[..n]);
        if let Some(row) = parse_cursor_position(&accum)? {
            return Ok(Some(row));
        }
    }
}

/// Look for `ESC [ <row> ; <col> R` somewhere in `buf`. Returns the
/// 1-indexed row if found.
fn parse_cursor_position(buf: &[u8]) -> Result<Option<u32>> {
    let Some(esc_pos) = buf.iter().position(|&b| b == 0x1B) else {
        return Ok(None);
    };
    if esc_pos + 1 >= buf.len() {
        return Ok(None);
    }
    if buf[esc_pos + 1] != b'[' {
        return Ok(None);
    }
    // Look for terminating 'R' from esc_pos + 2.
    let body_start = esc_pos + 2;
    let r_off = match buf[body_start..].iter().position(|&b| b == b'R') {
        Some(off) => off,
        None => return Ok(None),
    };
    let body = &buf[body_start..body_start + r_off];
    let body_str = std::str::from_utf8(body)
        .map_err(|_| anyhow!("cursor-position body not valid UTF-8"))?;
    let (row_str, _col) = body_str
        .split_once(';')
        .ok_or_else(|| anyhow!("cursor-position body lacks ';'"))?;
    let row: u32 = row_str
        .trim()
        .parse()
        .map_err(|_| anyhow!("cursor-position row not a u32: {body_str:?}"))?;
    Ok(Some(row))
}

fn poll_stdin_until(deadline: Instant) -> Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;
    let now = Instant::now();
    if now >= deadline {
        return Ok(false);
    }
    let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as u16;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let n = poll(&mut fds, PollTimeout::from(remaining_ms)).context("poll(stdin)")?;
    Ok(n > 0)
}

fn read_stdin(buf: &mut [u8]) -> Result<usize> {
    let fd = std::io::stdin().as_raw_fd();
    let n = nix::unistd::read(fd, buf).context("read(stdin)")?;
    Ok(n)
}

fn winsize_cols() -> Option<u16> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc != 0 || ws.ws_col == 0 {
        None
    } else {
        Some(ws.ws_col)
    }
}

// --- termios raw-mode guard, mirrors vge-cli's; kept inline to keep
// vcat self-contained ---

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
        raw.local_flags &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ISIG);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn placement_natural_size_when_smaller_than_terminal() {
        // 100×50 image on 10×20 cells. Natural width = 10 cells.
        // Pixels: 10 cells × 10 cell_pw = 100. target_px_h = 50
        // (preserves 2:1 aspect). target_rect_h = 50/20 = 2.5 cells,
        // so h_cells = 3 (one row of empty space at the bottom).
        let p = compute_placement(100, 50, 10.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 10);
        assert!(approx_eq(p.target_rect_h, 2.5));
        assert_eq!(p.h_cells, 3);
        assert_eq!(p.target_px_w, 100);
        assert_eq!(p.target_px_h, 50);
    }

    #[test]
    fn placement_clamped_to_terminal_width() {
        // 1000×500 image on 10×20 cells, terminal 80 cols. Natural
        // width = 100 cells, clamped to 80. Pixels: 800 × 400.
        // target_rect_h = 400/20 = 20.
        let p = compute_placement(1000, 500, 10.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 80);
        assert!(approx_eq(p.target_rect_h, 20.0));
        assert_eq!(p.h_cells, 20);
        assert_eq!(p.target_px_w, 800);
        assert_eq!(p.target_px_h, 400);
    }

    #[test]
    fn placement_forced_width_overrides_natural_and_terminal() {
        let p = compute_placement(1000, 500, 10.0, 20.0, 80, Some(40));
        assert_eq!(p.w_cells, 40);
        // 400 px wide → 200 px tall → 10 cells.
        assert!(approx_eq(p.target_rect_h, 10.0));
        assert_eq!(p.h_cells, 10);
    }

    #[test]
    fn placement_anisotropic_aspect_preserved() {
        // Square image, 100×100 px, on 9×20 cells. Natural width =
        // ceil(100/9) = 12 cells. Pixels: 108×108.
        // target_rect_h = 108/20 = 5.4 — fractional, preserves the
        // visual squareness despite anisotropic cells.
        let p = compute_placement(100, 100, 9.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 12);
        assert!(approx_eq(p.target_rect_h, 5.4));
        assert_eq!(p.h_cells, 6);
        assert_eq!(p.target_px_w, 108);
        assert_eq!(p.target_px_h, 108);
    }

    #[test]
    fn placement_minimum_one_cell() {
        let p = compute_placement(1, 1, 10.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 1);
        assert!(p.h_cells >= 1);
        assert!(p.target_rect_h > 0.0);
    }

    #[test]
    fn cursor_position_parses() {
        let buf = b"\x1b[24;1R";
        assert_eq!(parse_cursor_position(buf).unwrap(), Some(24));
    }

    #[test]
    fn cursor_position_with_leading_garbage() {
        let buf = b"hello\x1b[42;7Rworld";
        assert_eq!(parse_cursor_position(buf).unwrap(), Some(42));
    }

    #[test]
    fn cursor_position_partial_returns_none() {
        let buf = b"\x1b[24;";
        assert_eq!(parse_cursor_position(buf).unwrap(), None);
    }
}
