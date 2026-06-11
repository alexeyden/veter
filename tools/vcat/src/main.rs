//! vcat — print images to a VGE-aware terminal.
//!
//! Pipeline:
//!   1. Decode the images (PNG, JPEG, WebP) via the `image` crate.
//!   2. Probe the running terminal for its cell pixel dimensions.
//!   3. Query the kernel for terminal column count (TIOCGWINSZ) so we
//!      can clamp display width.
//!   4. Compute target cell width and height per image that preserves
//!      its visual aspect ratio on this terminal's anisotropic cell
//!      grid, then flow the images left-to-right with wrap-around —
//!      like words in a paragraph, bottom-aligned within each row.
//!   5. Resize each to exact pixel dimensions matching its cell
//!      footprint (Lanczos), upload as a Raw RGBA8 / WebP VGE image,
//!      and create elements placed where the next prompt would have
//!      been.
//!
//! The terminal handshake, placement math, encoding, and response
//! parsing live in the shared `vge-render` crate; this binary owns the
//! CLI, the flow layout, the cursor-anchoring, and the upload progress
//! bar.
//!
//! Run inside veter:
//!     vcat ~/Downloads/photo.jpg
//!     vcat --width 40 logo.png
//!     vcat *.png

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgGroup, Parser, ValueEnum};
use image::ImageReader;
use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style, UpdateCommandBody,
    UpdateCommandsBody, UpdateTextBody, UpdateTextRange, UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::*;

use vge_render::is_ssh_session;
use vge_render::placement::{Placement, compute_placement};
use vge_render::probe::run_probe;
use vge_render::response::wait_for_chunk_ack;
use vge_render::tty::{
    RawTty, drain_stale_stdin, poll_stdin_until, read_stdin, winsize_cols, winsize_rows,
};
use vge_render::upload::{Encoding, choose_encoding, encode_payload};

#[derive(Parser, Debug)]
#[command(version, about = "Display images inside a VGE-aware terminal.")]
#[command(group(
    // The mode-selecting flags are mutually exclusive: pick one of
    // `--mode <m>`, `-r`, `-l`, or `-L Q`, or none (auto-detect).
    ArgGroup::new("encoding")
        .args(["mode", "raw", "lossless", "lossy"])
        .multiple(false)
))]
struct Cli {
    /// Paths to PNG, JPEG, or WebP files. Multiple files flow
    /// left-to-right and wrap to the next row when they don't fit the
    /// terminal width, like words in a paragraph.
    #[arg(required = true)]
    files: Vec<std::path::PathBuf>,

    /// Force the displayed image width in cell units (applied to each
    /// image). Without this flag, vcat uses the image's natural pixel
    /// width divided by the terminal's cell pixel width, clamped to
    /// the terminal column count.
    #[arg(long)]
    width: Option<u32>,

    /// Milliseconds to wait for the terminal's probe / cursor
    /// responses before giving up. 2000 ms covers nested chains
    /// (e.g. vmux-over-ssh-over-vmux-over-veter) where each layer
    /// adds a poll-cadence boundary plus SSH round-trip; bump higher
    /// if the chain is deeper still.
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,

    /// Print progress to stderr at each pipeline stage.
    #[arg(short, long)]
    verbose: bool,

    /// Wire encoding for the uploaded image. `raw` sends straight
    /// RGBA8 bytes (fastest to encode, biggest payload). `webp-lossless`
    /// and `webp-lossy` both ride the pure-Rust `zenwebp` encoder.
    /// Lossy quality is controlled by `--quality` (0..=100). Shorthand
    /// flags: `-r` (raw), `-l` (lossless), `-L Q` (lossy at quality Q).
    /// If no mode flag is given, defaults to `webp-lossy` when an SSH
    /// session is detected (`SSH_CONNECTION` / `SSH_TTY` set), `raw`
    /// otherwise.
    #[arg(long, value_enum)]
    mode: Option<Mode>,

    /// Quality for `--mode webp-lossy`, in 0..=100. Ignored for the
    /// other modes. Conflicts with `-L` (which packs mode + quality
    /// into one flag).
    #[arg(long, default_value_t = 75.0, conflicts_with = "lossy")]
    quality: f32,

    /// Shorthand for `--mode raw`.
    #[arg(short = 'r', long = "raw")]
    raw: bool,

    /// Shorthand for `--mode webp-lossless`.
    #[arg(short = 'l', long = "lossless")]
    lossless: bool,

    /// Shorthand for `--mode webp-lossy --quality QUALITY`. QUALITY
    /// must be in 0..=100.
    #[arg(short = 'L', long = "lossy", value_name = "QUALITY")]
    lossy: Option<f32>,
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
    // Resolve a forced encoding from the four mode-selecting flags. The
    // ArgGroup on `Cli` already guarantees at most one is set, so branch
    // order doesn't matter for correctness. `None` means auto-detect
    // after the probe (so we can honour the terminal's advertised
    // encodings).
    let forced_enc: Option<Encoding> = if cli.raw {
        Some(Encoding::Raw)
    } else if cli.lossless {
        Some(Encoding::WebpLossless)
    } else if let Some(q) = cli.lossy {
        Some(Encoding::WebpLossy(q))
    } else {
        cli.mode.map(|m| match m {
            Mode::Raw => Encoding::Raw,
            Mode::WebpLossless => Encoding::WebpLossless,
            Mode::WebpLossy => Encoding::WebpLossy(cli.quality),
        })
    };

    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vcat must run with stdin and stdout connected to a terminal");
    }

    // Decode every input up front — the flow layout needs all image
    // dimensions before any vertical space can be reserved.
    let mut images: Vec<image::RgbaImage> = Vec::with_capacity(cli.files.len());
    for file in &cli.files {
        trace!(v, "decoding {}", file.display());
        let dyn_img = ImageReader::open(file)
            .with_context(|| format!("opening {}", file.display()))?
            .with_guessed_format()
            .with_context(|| format!("inspecting {}", file.display()))?
            .decode()
            .with_context(|| format!("decoding {}", file.display()))?;
        let rgba = dyn_img.to_rgba8();
        let (w_px, h_px) = rgba.dimensions();
        if w_px == 0 || h_px == 0 {
            bail!("{}: image has zero extent", file.display());
        }
        trace!(v, "decoded {w_px}x{h_px} px");
        images.push(rgba);
    }

    let _guard = RawTty::enable()?;

    drain_stale_stdin();
    trace!(v, "probing");
    let probe = run_probe(Duration::from_millis(cli.timeout_ms))?
        .ok_or_else(|| anyhow!("VGE probe timed out — terminal does not appear to support VGE"))?;
    let cell_pw = probe.cell_pixel_width.max(1) as f32;
    let cell_ph = probe.cell_pixel_height.max(1) as f32;
    trace!(v, "probe ok: cells={cell_pw}x{cell_ph}");

    let enc = forced_enc.unwrap_or_else(|| {
        let e = choose_encoding(
            probe.supported_image_encodings,
            is_ssh_session(),
            cli.quality,
        );
        trace!(v, "auto encoding: {e:?}");
        e
    });

    let term_cols = winsize_cols().unwrap_or(80) as u32;
    trace!(v, "term_cols={term_cols}");

    let placements: Vec<Placement> = images
        .iter()
        .map(|img| {
            let (w_px, h_px) = img.dimensions();
            compute_placement(w_px, h_px, cell_pw, cell_ph, term_cols, cli.width)
        })
        .collect();
    let layout = flow_layout(&placements, term_cols);
    for (i, ((x, y), placement)) in layout.positions.iter().zip(&placements).enumerate() {
        trace!(
            v,
            "placement[{i}]: {}x{} cells at ({x}, {y:.3}), target_rect_h={:.3}, pixels {}x{}",
            placement.w_cells,
            placement.h_cells,
            placement.target_rect_h,
            placement.target_px_w,
            placement.target_px_h
        );
    }
    trace!(v, "block: {} rows total", layout.total_rows);

    // Reserve vertical space for the whole block and read back the
    // cursor's new row.
    let mut stdout = std::io::stdout().lock();
    for _ in 0..layout.total_rows {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    trace!(v, "querying cursor");
    stdout.write_all(b"\x1b[6n")?;
    stdout.flush()?;
    let cursor_row = match read_cursor_row(Duration::from_millis(cli.timeout_ms))? {
        Some(r) => r,
        None => {
            // DSR timed out. Common cause is a multi-hop chain
            // (vmux-in-vmux over ssh) where the round trip exceeds
            // the configured timeout. Fall back to TIOCGWINSZ.
            let rows = winsize_rows().unwrap_or(24) as u32;
            eprintln!(
                "vcat: cursor-position query timed out at {}ms; falling \
                 back to row {} (TIOCGWINSZ). If the placement looks off, \
                 retry with --timeout-ms <larger>.",
                cli.timeout_ms, rows
            );
            rows
        }
    };
    trace!(v, "cursor row={cursor_row}");
    // After printing total_rows newlines the cursor is at row C
    // (1-indexed, top of screen = 1). The block should occupy rows
    // [C - total_rows, C) in 1-indexed terms, which is a block top of
    // C - total_rows - 1 in VGE 0-indexed cells from the live screen
    // top. For tall blocks the top may go negative — VGE anchors those
    // to scrollback and clips automatically (§5.2). Don't clamp to 0.
    let block_top_y = (cursor_row as i32 - layout.total_rows as i32 - 1) as f32;

    // Upload each image and create its element, left-to-right in flow
    // order. req_ids stay monotonic across images so chunk acks never
    // collide.
    let pid = std::process::id();
    let mut req_id: u32 = 0;
    for (idx, (rgba, placement)) in images.into_iter().zip(&placements).enumerate() {
        let (x_cells, y_offset) = layout.positions[idx];
        let element_origin = Point {
            x: x_cells as f32,
            y: block_top_y + y_offset,
        };
        upload_one(
            &mut stdout,
            rgba,
            placement,
            element_origin,
            enc,
            &format!("vcat-img-{pid}-{idx}"),
            &format!("vcat-el-{pid}-{idx}"),
            &mut req_id,
            cli.timeout_ms,
            v,
        )
        .with_context(|| format!("uploading {}", cli.files[idx].display()))?;
    }
    drop(stdout);

    Ok(())
}

/// Resize, encode, chunk-upload one image, and create its VGE element
/// at `element_origin`. The chunked upload (§8.1) slices the payload
/// into ~32 KB chunks over SSH so the placeholder progress UI can be
/// driven from the host's per-chunk acks; local runs send one chunk.
#[allow(clippy::too_many_arguments)]
fn upload_one(
    stdout: &mut std::io::StdoutLock<'_>,
    rgba: image::RgbaImage,
    placement: &Placement,
    element_origin: Point,
    enc: Encoding,
    img_id: &str,
    elem_id: &str,
    req_id: &mut u32,
    timeout_ms: u64,
    v: bool,
) -> Result<()> {
    trace!(v, "resizing");
    let resized = image::imageops::resize(
        &rgba,
        placement.target_px_w,
        placement.target_px_h,
        image::imageops::FilterType::Lanczos3,
    );
    drop(rgba);
    trace!(v, "resized");

    trace!(v, "encoding {enc:?}");
    let raw_rgba = resized.into_raw();
    let raw_len = raw_rgba.len();
    let (encoding, payload) =
        encode_payload(raw_rgba, placement.target_px_w, placement.target_px_h, enc)?;
    trace!(v, "encoded: {} -> {} bytes", raw_len, payload.len());

    // Chunked upload (§8.1). Over SSH we slice the payload into ~32 KB
    // chunks so vcat can drive a placeholder progress UI from the
    // host's per-chunk acks. Local runs send a single chunk.
    let total_bytes = payload.len() as u32;
    let target_chunk_size: u32 = if is_ssh_session() {
        32 * 1024
    } else {
        total_bytes.max(1)
    };
    let chunk_size = target_chunk_size.max(1).min(total_bytes.max(1));
    let num_chunks = total_bytes.div_ceil(chunk_size).max(1);
    let show_progress = num_chunks > 1;
    trace!(
        v,
        "uploading {} bytes in {} chunk(s) of {} bytes (progress UI: {})",
        total_bytes,
        num_chunks,
        chunk_size,
        show_progress
    );

    let target_rect = Rect {
        x: 0.0,
        y: 0.0,
        w: placement.w_cells as f32,
        h: placement.target_rect_h,
    };
    let final_draw = DrawCmd::DrawImage {
        target_rect,
        image_id: img_id.to_string(),
        source_rect: None,
    };

    // The element's command-index layout is fixed (see
    // `build_placeholder_commands`): index 0 = bar track, 1 = bar fill,
    // 2 = label. UpdateCommand / UpdateText target these.
    let placeholder_cmds = build_placeholder_commands(target_rect, total_bytes);
    let total_mb = bytes_to_mb(total_bytes);

    for i in 0..num_chunks {
        let offset = i * chunk_size;
        let end = (offset + chunk_size).min(total_bytes);
        let is_last = i == num_chunks - 1;
        let chunk_data = payload[offset as usize..end as usize].to_vec();
        let chunk_cmd = Command::UploadImage(UploadImageBody {
            id: img_id.to_string(),
            encoding,
            width: placement.target_px_w,
            height: placement.target_px_h,
            total_bytes,
            chunk_offset: offset,
            is_last,
            data: chunk_data,
        });
        *req_id += 1; // monotonic across images, distinct from REQ_ID_NO_RESPONSE
        let rid = *req_id;

        let mut frames: Vec<(Command, u32)> = Vec::with_capacity(4);

        if i == 0 && show_progress {
            frames.push((
                Command::CreateElement(CreateElementBody {
                    id: elem_id.to_string(),
                    commands: placeholder_cmds.clone(),
                    origin: element_origin,
                    is_visible: true,
                    draw_order: 0,
                    parent: None,
                    size: None,
                    transform: None,
                }),
                REQ_ID_NO_RESPONSE,
            ));
        }

        if i > 0 && show_progress {
            let acked = offset; // cumulative bytes acked so far
            frames.push((
                Command::UpdateCommand(UpdateCommandBody {
                    id: elem_id.to_string(),
                    index: 1,
                    command: bar_fill_cmd(target_rect, acked, total_bytes),
                }),
                REQ_ID_NO_RESPONSE,
            ));
            frames.push((
                Command::UpdateText(UpdateTextBody {
                    id: elem_id.to_string(),
                    command_index: 2,
                    range: UpdateTextRange::Whole,
                    replacement: progress_text(acked, total_bytes, total_mb),
                }),
                REQ_ID_NO_RESPONSE,
            ));
        }

        frames.push((chunk_cmd, rid));

        if is_last {
            let final_element = if show_progress {
                Command::UpdateCommands(UpdateCommandsBody {
                    id: elem_id.to_string(),
                    commands: vec![final_draw.clone()],
                })
            } else {
                Command::CreateElement(CreateElementBody {
                    id: elem_id.to_string(),
                    commands: vec![final_draw.clone()],
                    origin: element_origin,
                    is_visible: true,
                    draw_order: 0,
                    parent: None,
                    size: None,
                    transform: None,
                })
            };
            frames.push((final_element, REQ_ID_NO_RESPONSE));
        }

        let envelope = build_envelope(&frames);
        trace!(
            v,
            "chunk {}/{}: env={} bytes, chunk_offset={}, is_last={}",
            i + 1,
            num_chunks,
            envelope.len(),
            offset,
            is_last
        );
        stdout.write_all(&envelope)?;
        stdout.flush()?;

        let bytes_received = wait_for_chunk_ack(img_id, rid, Duration::from_millis(timeout_ms))?
            .ok_or_else(|| {
                anyhow!(
                    "chunk-ack timed out for chunk {}/{} (req_id {}); \
                     try --timeout-ms <larger>",
                    i + 1,
                    num_chunks,
                    rid
                )
            })?;
        trace!(
            v,
            "chunk {} acked: bytes_received={}",
            i + 1,
            bytes_received
        );
    }

    Ok(())
}

/// Gap between images on the same row, in cells — the inter-word space
/// of the flow layout.
const GAP_CELLS: u32 = 1;

/// Result of [`flow_layout`]: where each image lands inside the block.
struct FlowLayout {
    /// Per-image (column, row offset from block top) in cells. The row
    /// offset is fractional so each image's bottom edge sits exactly on
    /// its row's bottom despite fractional `target_rect_h` values.
    positions: Vec<(u32, f32)>,
    /// Whole rows the block occupies — how many newlines to reserve.
    total_rows: u32,
}

/// Flow a sequence of image footprints left-to-right with wrap-around,
/// like words in a paragraph: images on the same row are separated by
/// `GAP_CELLS` and bottom-aligned (text-baseline style); a row is as
/// tall as its tallest image; an image that doesn't fit in the
/// remaining columns starts a new row.
fn flow_layout(placements: &[Placement], term_cols: u32) -> FlowLayout {
    let term_cols = term_cols.max(1);

    // Pass 1: assign each image a row and a column.
    let mut row_of: Vec<usize> = Vec::with_capacity(placements.len());
    let mut x_of: Vec<u32> = Vec::with_capacity(placements.len());
    let mut row = 0usize;
    let mut cur_x = 0u32;
    for p in placements {
        let x = if cur_x == 0 { 0 } else { cur_x + GAP_CELLS };
        let x = if x > 0 && x + p.w_cells > term_cols {
            row += 1;
            0
        } else {
            x
        };
        row_of.push(row);
        x_of.push(x);
        cur_x = x + p.w_cells;
    }

    // Pass 2: row heights -> row tops -> bottom-aligned offsets.
    let num_rows = row + 1;
    let mut row_h = vec![0u32; num_rows];
    for (p, &r) in placements.iter().zip(&row_of) {
        row_h[r] = row_h[r].max(p.h_cells);
    }
    let mut row_top = vec![0u32; num_rows];
    for r in 1..num_rows {
        row_top[r] = row_top[r - 1] + row_h[r - 1];
    }
    let positions = placements
        .iter()
        .zip(&row_of)
        .zip(&x_of)
        .map(|((p, &r), &x)| {
            let y = row_top[r] as f32 + (row_h[r] as f32 - p.target_rect_h);
            (x, y)
        })
        .collect();
    FlowLayout {
        positions,
        total_rows: row_top[num_rows - 1] + row_h[num_rows - 1],
    }
}

/// Cell-units height of the progress bar inside the image rect.
fn bar_height_cells(target_rect_h: f32) -> f32 {
    (target_rect_h * 0.12).clamp(0.4, 1.2)
}

fn bar_track_rect(image_rect: Rect) -> Rect {
    let h = bar_height_cells(image_rect.h);
    let pad_x = (image_rect.w * 0.05).clamp(0.5, 4.0);
    Rect {
        x: image_rect.x + pad_x,
        y: image_rect.y + (image_rect.h - h) * 0.5,
        w: (image_rect.w - 2.0 * pad_x).max(0.5),
        h,
    }
}

fn bar_fill_cmd(image_rect: Rect, acked: u32, total: u32) -> DrawCmd {
    let track = bar_track_rect(image_rect);
    let frac = if total == 0 {
        0.0
    } else {
        acked as f32 / total as f32
    };
    let fill_w = (track.w * frac).clamp(0.0, track.w);
    DrawCmd::FillRectangles {
        fill: Style::Flat(Color {
            r: 0.42,
            g: 0.78,
            b: 1.0,
            a: 1.0,
        }),
        rects: vec![Rect {
            x: track.x,
            y: track.y,
            w: fill_w,
            h: track.h,
        }],
    }
}

fn build_placeholder_commands(image_rect: Rect, total: u32) -> Vec<DrawCmd> {
    let track = bar_track_rect(image_rect);
    let track_cmd = DrawCmd::FillRectangles {
        fill: Style::Flat(Color {
            r: 0.20,
            g: 0.22,
            b: 0.27,
            a: 0.85,
        }),
        rects: vec![track],
    };
    let fill_cmd = bar_fill_cmd(image_rect, 0, total);
    let total_mb = bytes_to_mb(total);
    let label_origin = Point {
        x: image_rect.x + image_rect.w * 0.5,
        y: (track.y - 1.0).max(image_rect.y),
    };
    let label_cmd = DrawCmd::DrawText {
        origin: label_origin,
        align: Align::Center,
        fill: Style::Flat(Color {
            r: 0.88,
            g: 0.92,
            b: 1.0,
            a: 1.0,
        }),
        font_style: FontStyle::default(),
        text: progress_text(0, total, total_mb),
    };
    vec![track_cmd, fill_cmd, label_cmd]
}

fn bytes_to_mb(bytes: u32) -> f32 {
    bytes as f32 / (1024.0 * 1024.0)
}

fn progress_text(acked: u32, total: u32, total_mb: f32) -> String {
    let pct = if total == 0 {
        0.0
    } else {
        (acked as f32 / total as f32 * 100.0).clamp(0.0, 100.0)
    };
    format!(
        "{pct:>3.0}%  {acked_mb:.2} / {total_mb:.2} MB",
        acked_mb = bytes_to_mb(acked),
    )
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
    let body_start = esc_pos + 2;
    let r_off = match buf[body_start..].iter().position(|&b| b == b'R') {
        Some(off) => off,
        None => return Ok(None),
    };
    let body = &buf[body_start..body_start + r_off];
    let body_str =
        std::str::from_utf8(body).map_err(|_| anyhow!("cursor-position body not valid UTF-8"))?;
    let (row_str, _col) = body_str
        .split_once(';')
        .ok_or_else(|| anyhow!("cursor-position body lacks ';'"))?;
    let row: u32 = row_str
        .trim()
        .parse()
        .map_err(|_| anyhow!("cursor-position row not a u32: {body_str:?}"))?;
    Ok(Some(row))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn placement(w_cells: u32, target_rect_h: f32) -> Placement {
        Placement {
            w_cells,
            target_rect_h,
            h_cells: target_rect_h.ceil().max(1.0) as u32,
            target_px_w: w_cells * 10,
            target_px_h: (target_rect_h * 20.0) as u32,
        }
    }

    #[test]
    fn flow_single_image_fills_own_block() {
        let l = flow_layout(&[placement(10, 5.0)], 80);
        assert_eq!(l.total_rows, 5);
        assert_eq!(l.positions, vec![(0, 0.0)]);
    }

    #[test]
    fn flow_two_images_share_a_row_with_gap() {
        let l = flow_layout(&[placement(10, 4.0), placement(20, 4.0)], 80);
        assert_eq!(l.total_rows, 4);
        assert_eq!(l.positions, vec![(0, 0.0), (11, 0.0)]);
    }

    #[test]
    fn flow_wraps_when_image_does_not_fit() {
        let l = flow_layout(&[placement(50, 4.0), placement(40, 6.0)], 80);
        assert_eq!(l.total_rows, 10);
        assert_eq!(l.positions, vec![(0, 0.0), (0, 4.0)]);
    }

    #[test]
    fn flow_bottom_aligns_shorter_images_in_row() {
        // Second image is 2 cells shorter than the row: its top shifts
        // down so the bottoms line up.
        let l = flow_layout(&[placement(10, 6.0), placement(10, 4.0)], 80);
        assert_eq!(l.total_rows, 6);
        assert_eq!(l.positions[0], (0, 0.0));
        assert_eq!(l.positions[1], (11, 2.0));
    }

    #[test]
    fn flow_fractional_height_sits_on_row_bottom() {
        // target_rect_h = 4.5 -> h_cells = 5; bottom-aligned means a
        // 0.5-cell gap above, none below.
        let l = flow_layout(&[placement(10, 4.5)], 80);
        assert_eq!(l.total_rows, 5);
        let (x, y) = l.positions[0];
        assert_eq!(x, 0);
        assert!((y - 0.5).abs() < 1e-3);
    }

    #[test]
    fn flow_full_width_images_stack_vertically() {
        let l = flow_layout(&[placement(80, 3.0), placement(80, 2.0)], 80);
        assert_eq!(l.total_rows, 5);
        assert_eq!(l.positions, vec![(0, 0.0), (0, 3.0)]);
    }
}
