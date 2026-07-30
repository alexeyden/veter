//! Place an image into a pane whose foreground program is something
//! else entirely — a TUI we do not control and cannot modify.
//!
//! This is the out-of-band case. Everything about it is shaped by one
//! constraint: we can write to the pane, but we can never read from it.
//! A reply would land in the pane's *input* queue, where the foreground
//! program is blocked in `read()`, and whoever the kernel wakes gets the
//! bytes. So every command goes out with `REQ_ID_NO_RESPONSE` and every
//! capability we need is obtained without a round-trip —
//! `TIOCGWINSZ` for live cell metrics, `$VETER_LIMITS` for the static
//! caps (`doc/vector-graphics-extension.md` §11.1).
//!
//! The other constraint is vertical space. We must not create it: a
//! foreground TUI dead-reckons its frame position, so newlines injected
//! underneath it strand the previous frame and desynchronise the next.
//! Space can only be reserved *in-band*, by the application itself. So
//! the division of labour is:
//!
//!   * the application prints a marker line and reserves the rows
//!     below it (for a markdown renderer, a fenced code block whose
//!     body is blank lines — plain blank lines get collapsed);
//!   * we anchor to that marker with `OriginAnchor::Marker` and draw
//!     into the rows it reserved.
//!
//! The terminal owns the grid, so it resolves the marker to a row. We
//! never need to know where the cursor is.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Color, Command, CreateElementBody, DrawCmd, OriginAnchor, Retention, Style,
    UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::{Encoding, compute_placement, encode_payload};

/// Reserved id namespace (§6.8). Everything this tool creates is
/// prefixed, so `--clear` is one command per table and can never
/// destroy an element another client owns.
const NS: &str = "vplace.";

/// Bytes per `UploadImage` chunk. Comfortably under the 1 MiB
/// `max_write_bytes` a `WritePortal` relay imposes, so a pane-hosted
/// upload is never rejected for being one oversized write.
const CHUNK: usize = 128 * 1024;

#[derive(Parser, Debug)]
#[command(about = "Place an image into a pane, anchored to a marker the app printed")]
struct Args {
    /// Image file to display.
    #[arg(required_unless_present = "clear")]
    image: Option<PathBuf>,

    /// Substring the application printed on the row above its reserved
    /// gap. Keep it short and at the start of a line: it is matched
    /// against a single grid row, so a marker long enough to wrap
    /// cannot be found.
    ///
    /// **The marker must not be visible below the row you mean.** The
    /// terminal anchors to the *most recent* matching row (§9.4), so
    /// when the string is on screen twice the lower one wins — and an
    /// interactive shell echoing this very command line counts, which
    /// makes the placement anchor to itself. Use `--marker-file` when
    /// invoking by hand. A hook is unaffected: nothing echoes it.
    #[arg(short, long,
          required_unless_present_any = ["clear", "marker_file", "measure"],
          conflicts_with = "marker_file")]
    marker: Option<String>,

    /// Read the marker from a file, so the literal never reaches the
    /// screen as part of this invocation. See the hazard on `--marker`.
    #[arg(long)]
    marker_file: Option<PathBuf>,

    /// Rows below the marker to place at.
    ///
    /// The default of `0` puts the image on the first row *after* the
    /// marker's line, not on the marker itself: a markdown renderer
    /// leaves a blank spacing row between a paragraph and the fenced
    /// block under it, and that row is the top of the usable gap.
    /// Measured against Claude Code's renderer — raise it by one if a
    /// host leaves no such spacing row.
    #[arg(long, default_value_t = 0.0)]
    offset_y: f32,

    /// Columns to indent the image by, so it lines up with text the
    /// host renders inside its own margin (Claude Code indents message
    /// bodies by 2). Default `0` = flush with the pane's left edge.
    #[arg(long, default_value_t = 0.0)]
    offset_x: f32,

    /// Width in cells. Defaults to the image's natural width, clamped
    /// to the pane and to `--max-rows`.
    #[arg(long)]
    width_cells: Option<u32>,

    /// Tallest the image may be, in rows. A tall image is scaled down
    /// (aspect preserved) rather than overrunning the gap the
    /// application reserved and painting over what follows. Defaults to
    /// two thirds of the pane's height.
    #[arg(long)]
    max_rows: Option<u32>,

    /// Leave the marker's own row visible.
    ///
    /// By default a full-width rectangle is painted over that row
    /// first, because at the default `--offset-y` the image lands on
    /// the marker line and hides only the part behind it — the tail of
    /// a long path keeps showing past the image's right edge. Pass
    /// this when the marker row holds something worth reading.
    #[arg(long)]
    no_cover: bool,

    /// Colour of that rectangle, `#rrggbb`. Defaults to veter's
    /// terminal background (`veter/src/main.rs` clears to rgb 30,30,30),
    /// which is what makes the covered row look empty rather than
    /// patched.
    #[arg(long, default_value = "#1e1e1e")]
    cover_color: String,

    /// Target tty. Defaults to `$VMUX_PANE_TTY`, then `/dev/tty`.
    #[arg(long)]
    tty: Option<PathBuf>,

    /// Distinguishes several images placed in one pane. Defaults to a
    /// slug of the marker.
    #[arg(long)]
    id: Option<String>,

    /// Report the footprint this image would occupy and exit, drawing
    /// nothing. An application has to reserve the rows *before* the
    /// image can be placed into them, so it needs the number first.
    #[arg(long)]
    measure: bool,

    /// Remove everything this tool has placed in the pane and exit.
    #[arg(long)]
    clear: bool,
}

/// Extra rendered rows a fenced code block costs beyond the blank lines
/// inside it, in Claude Code's markdown renderer: six blank body lines
/// were measured to render as seven blank rows.
///
/// This is a property of one renderer, not of the terminal, which is
/// why it only shows up in `--measure`'s advisory output and nowhere in
/// the placement path.
const FENCE_OVERHEAD_ROWS: u32 = 1;

fn main() -> Result<()> {
    let args = Args::parse();
    let tty_path = resolve_tty(args.tty.as_deref())?;
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open(&tty_path)
        .with_context(|| format!("open {}", tty_path.display()))?;

    if args.clear {
        // Prefix form (§6.2 / §8.2): one command per table. Elements
        // first — dropping an image still referenced would leave the
        // element rendering a magenta debug fill until it goes.
        let env = build_envelope(&[
            (
                Command::DeleteElement { id: NS.into(), by_prefix: true },
                REQ_ID_NO_RESPONSE,
            ),
            (
                Command::DropImage { id: NS.into(), by_prefix: true },
                REQ_ID_NO_RESPONSE,
            ),
        ]);
        tty.write_all(&env)?;
        tty.flush()?;
        return Ok(());
    }

    let image_path = args.image.expect("clap enforces this");
    let caps = Caps::probe(&tty)?;

    let img = image::open(&image_path)
        .with_context(|| format!("decode {}", image_path.display()))?
        .to_rgba8();
    let (w_px, h_px) = (img.width(), img.height());
    if w_px == 0 || h_px == 0 {
        bail!("image has a zero dimension");
    }

    let max_rows = args.max_rows.unwrap_or_else(|| {
        ((caps.rows as f32 * DEFAULT_HEIGHT_FRACTION) as u32).max(3)
    });
    let placement = fit_placement(w_px, h_px, &caps, args.width_cells, max_rows);

    if args.measure {
        // stdout only, one `key=value` line, so a caller can read it
        // without parsing prose.
        println!(
            "rows={} cols={} fence_blank_lines={}",
            placement.h_cells,
            placement.w_cells,
            placement.h_cells.saturating_sub(FENCE_OVERHEAD_ROWS).max(1),
        );
        return Ok(());
    }

    let marker = resolve_marker(args.marker, args.marker_file.as_deref())?;

    // Resize to the exact pixel target so the terminal stores only the
    // pixels it will actually draw — an out-of-band upload crosses a
    // relay, so shipping a full-resolution buffer to fill 40 cells is
    // pure waste.
    let resized = image::imageops::resize(
        &img,
        placement.target_px_w.max(1),
        placement.target_px_h.max(1),
        image::imageops::FilterType::Lanczos3,
    );
    let (enc_byte, payload) = encode_payload(
        resized.into_raw(),
        placement.target_px_w,
        placement.target_px_h,
        caps.encoding,
    )?;

    if payload.len() as u64 > caps.max_image_bytes as u64 {
        bail!(
            "encoded image is {} bytes, over the terminal's {} byte cap",
            payload.len(),
            caps.max_image_bytes
        );
    }

    let slug = args.id.unwrap_or_else(|| slugify(&marker));
    let img_id = format!("{NS}{slug}");
    let elem_id = format!("{NS}{slug}");

    // Re-placing under the same marker replaces rather than collides:
    // ids are stable per slug, and CreateElement/UploadImage both
    // reject a duplicate id (§6.1, §8.2).
    let mut out = build_envelope(&[
        (
            Command::DeleteElement { id: elem_id.clone(), by_prefix: false },
            REQ_ID_NO_RESPONSE,
        ),
        (
            Command::DropImage { id: img_id.clone(), by_prefix: false },
            REQ_ID_NO_RESPONSE,
        ),
    ]);

    let total = payload.len() as u32;
    let n_chunks = payload.len().div_ceil(CHUNK).max(1);
    for i in 0..n_chunks {
        let start = i * CHUNK;
        let end = ((i + 1) * CHUNK).min(payload.len());
        out.extend(build_envelope(&[(
            Command::UploadImage(UploadImageBody {
                // Auto: this image is referenced by exactly one element
                // and should die with it. When the marker's line falls
                // out of scrollback the element goes, the refcount hits
                // zero, and the image is collected — no bookkeeping and
                // no leak (§8.0).
                retention: Retention::Auto,
                id: img_id.clone(),
                encoding: enc_byte,
                width: placement.target_px_w,
                height: placement.target_px_h,
                total_bytes: total,
                chunk_offset: start as u32,
                is_last: i == n_chunks - 1,
                data: payload[start..end].to_vec(),
            }),
            REQ_ID_NO_RESPONSE,
        )]));
    }

    // Command order is draw order: the cover goes down first so the
    // image sits on top of it rather than behind.
    let mut commands = Vec::new();
    if !args.no_cover {
        commands.push(DrawCmd::FillRectangles {
            fill: Style::Flat(parse_hex(&args.cover_color)?),
            rects: vec![Rect {
                // Element-local coordinates: the origin sits at
                // `offset_x` columns in and on the anchored row, so
                // `-offset_x` is the pane's left edge and `-offset_y`
                // is the marker's own row whatever the offset.
                x: -args.offset_x,
                y: -args.offset_y,
                w: caps.cols as f32,
                h: 1.0,
            }],
        });
    }
    commands.push(DrawCmd::DrawImage {
        target_rect: Rect {
            x: 0.0,
            y: 0.0,
            w: placement.w_cells as f32,
            h: placement.target_rect_h,
        },
        image_id: img_id,
        source_rect: None,
    });

    out.extend(build_envelope(&[(
        Command::CreateElement(CreateElementBody {
            id: elem_id,
            commands,
            origin: Point { x: args.offset_x, y: args.offset_y },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
            transform: None,
            anchor: OriginAnchor::Marker(marker),
        }),
        REQ_ID_NO_RESPONSE,
    )]));

    tty.write_all(&out)?;
    tty.flush()?;

    eprintln!(
        "vplace: {}x{} px -> {} cols x {:.1} rows, {} bytes in {} chunk(s) -> {}",
        w_px,
        h_px,
        placement.w_cells,
        placement.target_rect_h,
        payload.len(),
        n_chunks,
        tty_path.display()
    );
    Ok(())
}

/// The marker, from whichever source was given.
///
/// Only the first line is used and it is trimmed, so a file written
/// with a trailing newline — i.e. every ordinary way of writing one —
/// still yields the exact substring the application printed.
fn resolve_marker(inline: Option<String>, file: Option<&Path>) -> Result<String> {
    let raw = match (inline, file) {
        (Some(m), _) => m,
        (None, Some(p)) => std::fs::read_to_string(p)
            .with_context(|| format!("read marker from {}", p.display()))?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
        (None, None) => bail!("clap should have required --marker or --marker-file"),
    };
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        bail!("marker is empty: an empty marker matches no row (§9.4)");
    }
    Ok(raw)
}

/// Where to write. `$VMUX_PANE_TTY` is the pane we occupy when running
/// under vmux; `/dev/tty` is the fallback for a plain veter child.
/// Deliberately never stdout — a hook's stdout is captured by whatever
/// spawned it, and the bytes would never reach a terminal.
fn resolve_tty(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("VMUX_PANE_TTY")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    if Path::new("/dev/tty").exists() {
        return Ok(PathBuf::from("/dev/tty"));
    }
    bail!("no target tty: set $VMUX_PANE_TTY or pass --tty")
}

/// Everything we need about the terminal, obtained with no round-trip.
struct Caps {
    cell_pw: u16,
    cell_ph: u16,
    cols: u16,
    rows: u16,
    max_image_bytes: u32,
    encoding: Encoding,
}

impl Caps {
    fn probe(tty: &std::fs::File) -> Result<Self> {
        if std::env::var("VETER").unwrap_or_default().is_empty() {
            bail!("$VETER is unset — this does not look like a veter terminal");
        }
        // TIOCGWINSZ on the *target* tty, not on stdout: a hook's
        // stdout is a pipe, and even when it is a tty it is not
        // necessarily the pane we are drawing into.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, &mut ws as *mut _) };
        if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 {
            bail!("TIOCGWINSZ on the target tty reported no size");
        }
        if ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
            bail!("terminal reports no pixel dimensions; cannot size an image");
        }
        let limits = std::env::var("VETER_LIMITS").unwrap_or_default();
        let supported = limit(&limits, "enc").unwrap_or(0x01) as u8;
        Ok(Self {
            cell_pw: ws.ws_xpixel / ws.ws_col,
            cell_ph: ws.ws_ypixel / ws.ws_row,
            cols: ws.ws_col,
            rows: ws.ws_row,
            max_image_bytes: limit(&limits, "mib").unwrap_or(32 * 1024 * 1024) as u32,
            // Prefer WebP whenever the host supports it. Unlike an
            // in-pane client, our bytes cross the vmux relay as
            // `WritePortal` payload, so payload size costs more here
            // than encode time does.
            encoding: if supported & 0x02 != 0 {
                Encoding::WebpLossy(0.85)
            } else {
                Encoding::Raw
            },
        })
    }
}

/// Fraction of the pane an image may occupy vertically when no
/// `--max-rows` is given. A gap taller than this pushes everything the
/// user was reading off screen to show one picture, which is a bad
/// trade in a conversation; two thirds leaves the surrounding text
/// legible while still being big enough to see.
const DEFAULT_HEIGHT_FRACTION: f32 = 2.0 / 3.0;

/// Placement that also respects a row budget.
///
/// [`compute_placement`] clamps width to the pane but leaves height
/// unbounded, so a tall image overruns the rows the application
/// reserved, paints over whatever follows, and can run off the bottom
/// of the screen entirely. Width is the only lever — height follows
/// from it and the aspect ratio — so shrink the width until the height
/// fits.
///
/// Both `--measure` and the placement path go through here, so the rows
/// an application is told to reserve are the rows that get drawn.
fn fit_placement(
    w_px: u32,
    h_px: u32,
    caps: &Caps,
    forced_w_cells: Option<u32>,
    max_rows: u32,
) -> vge_render::Placement {
    let compute = |w: Option<u32>| {
        compute_placement(
            w_px,
            h_px,
            caps.cell_pw as f32,
            caps.cell_ph as f32,
            caps.cols as u32,
            w,
        )
    };
    let p = compute(forced_w_cells);
    if p.h_cells <= max_rows || max_rows == 0 {
        return p;
    }
    // One proportional step gets very close; the loop then walks the
    // last cell or two off, since ceil() means the relationship is not
    // exactly linear. Bounded by the width, so it always terminates.
    let scaled = (p.w_cells as f32 * max_rows as f32 / p.target_rect_h).floor() as u32;
    let mut w = scaled.clamp(1, p.w_cells);
    loop {
        let q = compute(Some(w));
        if q.h_cells <= max_rows || w <= 1 {
            return q;
        }
        w -= 1;
    }
}

/// Parse `#rrggbb` (or bare `rrggbb`) into a VGE colour.
fn parse_hex(s: &str) -> Result<Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("colour must be #rrggbb, got {s:?}");
    }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f32 / 255.0;
    Ok(Color { r: v(0), g: v(2), b: v(4), a: 1.0 })
}

/// One `key=value` from `$VETER_LIMITS`. Unknown keys are ignored by
/// construction and an unparseable value reads as absent, so the caller
/// falls back to the §11 default rather than to zero.
fn limit(limits: &str, key: &str) -> Option<u64> {
    limits
        .split(',')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.parse().ok())
}

/// Derive a stable, id-safe slug from a marker so repeated placements
/// under the same marker reuse one id instead of piling up.
fn slugify(marker: &str) -> String {
    let s: String = marker
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    // Element ids are capped at 64 bytes (§6.8) and the namespace
    // prefix eats into that.
    let cap = 64 - NS.len();
    if s.len() > cap { s[..cap].to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_id_safe_and_bounded() {
        assert_eq!(slugify("@@IMG:a1@@"), "IMG-a1");
        assert_eq!(slugify("⟦IMG:x⟧"), "IMG-x");
        let long = slugify(&"a".repeat(200));
        assert!(long.len() + NS.len() <= 64, "slug must fit the §6.8 id cap");
    }

    #[test]
    fn marker_file_is_trimmed_to_its_first_line() {
        // Written with `printf`/`echo`, a marker file ends in a newline;
        // the trailing byte must not become part of the substring the
        // terminal searches for, or nothing ever matches.
        let p = std::env::temp_dir().join("vplace-marker-test");
        std::fs::write(&p, "@@IMG:x1@@\nignored\n").unwrap();
        assert_eq!(resolve_marker(None, Some(&p)).unwrap(), "@@IMG:x1@@");

        std::fs::write(&p, "   \n").unwrap();
        assert!(
            resolve_marker(None, Some(&p)).is_err(),
            "an empty marker matches no row and must be refused"
        );
    }

    #[test]
    fn inline_marker_wins_and_is_trimmed() {
        assert_eq!(
            resolve_marker(Some("  @@IMG:y@@ ".into()), None).unwrap(),
            "@@IMG:y@@"
        );
    }

    #[test]
    fn limits_parsing_ignores_unknown_keys_and_bad_values() {
        let l = "mib=123,junk=x,enc=3,mi=notanumber";
        assert_eq!(limit(l, "mib"), Some(123));
        assert_eq!(limit(l, "enc"), Some(3));
        assert_eq!(limit(l, "mi"), None, "unparseable must read as absent");
        assert_eq!(limit(l, "nope"), None);
    }
}
