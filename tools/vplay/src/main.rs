//! vplay — interactive image and video viewer for VGE-aware terminals.
//!
//! Draws media via a single VGE `DrawImage` with a `source_rect` ROI:
//! the full-resolution texture is uploaded once per picture (once total
//! for images, once per seek for video, ping-ponged between two ids) and
//! pan/zoom is a pure host-side `source_rect` update — no pixels travel.
//! A status bar and (for video) a draggable seek bar overlay the media.
//! Video frames come from an external ffmpeg.

mod image_src;
mod input;
mod playlist;
mod video;
mod viewport;

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Parser;
use vge_protocol::codec::{Point, Rect, Transform};
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, OriginAnchor, Retention, Style, UpdateCommandBody, UpdateTextBody, UpdateTextRange, UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::is_ssh_session;
use vge_render::probe::run_probe;
use vge_render::tty::{
    RawTty, drain_stale_stdin, install_sigwinch, poll_stdin_and, poll_stdin_until, read_stdin,
    take_sigwinch, winsize,
};
use vge_render::upload::{Encoding, encode_payload};

use image_src::{Frame, load_image};
use input::{Dir, Event, InputParser};
use playlist::Playlist;
use video::{Decode, DecodeState, VideoMeta, grab_one_frame, probe_frame_times, probe_video, start_decode};
use viewport::Viewport;

/// Namespace every element and image id shares (§6.8), so cleanup is one
/// prefix sweep per table rather than a list of ids to keep in step.
const ID_PREFIX: &str = "vplay-";

const EL_BG: &str = "vplay-bg";
const EL_IMG: &str = "vplay-img";
const EL_STATUS: &str = "vplay-status";
const EL_SEEK: &str = "vplay-seek";
const EL_SPINNER: &str = "vplay-spin";
const IMG_ID: &str = "vplay-tex";
const IMG_ID_A: &str = "vplay-fa";
const IMG_ID_B: &str = "vplay-fb";

const ACCENT: (f32, f32, f32) = (0.337, 0.475, 0.624); // #56799f

/// Retention for every texture vplay uploads (§8.2).
///
/// Manual — vplay owns its textures' lifetimes, because the host's `Auto`
/// refcount does not survive either of the two things vplay does to a
/// picture:
///
/// * **A resize.** The `SIGWINCH` path deletes every `vplay-` element and
///   recreates the chrome, then recreates the image element over the
///   texture it already uploaded. But deleting an element releases its
///   `DrawImage` reference (§8.0), and an `Auto` image at zero refs is
///   collected there and then — so the recreate would name a texture the
///   host had just thrown away: `ERR_UNKNOWN_IMAGE`, silent (§4:
///   unrequested commands get no response), and a blank picture.
/// * **A same-id swap.** Cycling stills reuses the one still id
///   (`DropImage`, `UploadImage`, retarget, one envelope) and the
///   retarget releases the *old* `DrawImage` — which, under `Auto`,
///   collects the pixels that landed under that id moments earlier. The
///   video path ping-pongs between two ids instead, so there `Auto`
///   collects the correct (outgoing) slot, and it is only the explicit
///   `DropImage` that ends up a no-op.
///
/// In exchange every id must be released by hand — which vplay already
/// does: a swap drops the id it supersedes, `queue_frame_upload` drops a
/// superseded in-flight upload, and `TermExit` sweeps the whole
/// `vplay-` prefix (§8.2) on the way out. At most two frame textures and
/// one still are ever live.
const RETENTION: Retention = Retention::Manual;

/// Spinner angular speed, rad/s (§9.12 UpdateTransform). Time-based so
/// the rotation rate is independent of the loop's variable tick.
const SPIN_SPEED: f32 = 5.5;
/// Minimum interval between spinner transform updates.
const SPIN_FRAME: Duration = Duration::from_millis(33);
/// How long a decode must stay pending before its spinner appears, so
/// quick seeks/steps don't flash an indicator.
const SPIN_DELAY: Duration = Duration::from_millis(160);
/// Frame uploads bigger than this stream chunk-by-chunk from the event
/// loop (§8.2) so the spinner keeps animating during the transfer; one
/// chunk's PTY write is short enough to not visibly stall the loop.
const UPLOAD_CHUNK_BYTES: usize = 1 << 20;

fn flat(r: f32, g: f32, b: f32, a: f32) -> Style {
    Style::Flat(Color { r, g, b, a })
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Interactive image/video viewer for VGE-aware terminals."
)]
struct Cli {
    /// Path to an image (png/jpeg/webp) or video file.
    file: std::path::PathBuf,
    /// Force image mode (don't probe as video).
    #[arg(long)]
    image: bool,
    /// Force video mode (decode with ffmpeg).
    #[arg(long)]
    video: bool,
    /// Milliseconds to wait for the terminal's VGE probe response.
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,
}

/// What the status bar calls a file: its name, or the whole path when it
/// has no trailing name component.
fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .unwrap_or(p.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn is_video_ext(p: &std::path::Path) -> bool {
    match p.extension().and_then(|e| e.to_str()) {
        Some(e) => matches!(
            e.to_ascii_lowercase().as_str(),
            "mp4"
                | "mkv"
                | "webm"
                | "mov"
                | "avi"
                | "m4v"
                | "mpg"
                | "mpeg"
                | "wmv"
                | "flv"
                | "ts"
                | "gif"
                | "ogv"
                | "3gp"
        ),
        None => false,
    }
}

fn np(c: Command) -> (Command, u32) {
    (c, REQ_ID_NO_RESPONSE)
}

fn send<W: Write>(out: &mut W, cmds: &[(Command, u32)]) {
    if cmds.is_empty() {
        return;
    }
    let env = build_envelope(cmds);
    let _ = out.write_all(&env);
    let _ = out.flush();
}

// --- element / command builders ---

fn image_draw(target: Rect, id: &str, source: Option<Rect>) -> DrawCmd {
    DrawCmd::DrawImage {
        target_rect: target,
        image_id: id.to_string(),
        source_rect: source,
    }
}

fn create_image_el(target: Rect, id: &str, source: Option<Rect>) -> Command {
    Command::CreateElement(CreateElementBody {
        id: EL_IMG.into(),
        commands: vec![image_draw(target, id, source)],
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: 1,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    })
}

fn update_image_el(target: Rect, id: &str, source: Option<Rect>) -> Command {
    Command::UpdateCommand(UpdateCommandBody {
        id: EL_IMG.into(),
        index: 0,
        command: image_draw(target, id, source),
    })
}

/// Pick an encoding that also respects the host's advertised
/// `max_image_bytes` (0 = no limit): raw locally, but fall back to lossy
/// WebP when a raw payload would exceed the limit — a full-resolution
/// still or frame can (raw RGBA is `w*h*4` bytes). Errors only if raw is
/// over-limit and the host does not support WebP.
fn pick_encoding(
    w: u32,
    h: u32,
    supported: u8,
    ssh: bool,
    max_image_bytes: u32,
) -> Result<Encoding> {
    let raw_bytes = w as usize * h as usize * 4;
    let limit = max_image_bytes as usize;
    let raw_fits = limit == 0 || raw_bytes <= limit;
    let webp_ok = supported & 0x02 != 0;
    if (ssh || !raw_fits) && webp_ok {
        Ok(Encoding::WebpLossy(80.0))
    } else if raw_fits {
        Ok(Encoding::Raw)
    } else {
        bail!(
            "raw {w}x{h} image ({raw_bytes} bytes) exceeds the host limit of \
             {limit} bytes and the host does not support WebP"
        )
    }
}

/// The single-shot upload of a still, under the one still texture id.
/// Pinned — see [`RETENTION`].
fn upload_cmd(
    id: &str,
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    supported: u8,
    ssh: bool,
    max_image_bytes: u32,
) -> Result<Command> {
    let enc = pick_encoding(w, h, supported, ssh, max_image_bytes)?;
    let (encoding, payload) = encode_payload(rgba, w, h, enc)?;
    if max_image_bytes > 0 && payload.len() > max_image_bytes as usize {
        bail!(
            "encoded {w}x{h} image ({} bytes) exceeds the host limit of {} bytes",
            payload.len(),
            max_image_bytes
        );
    }
    Ok(Command::UploadImage(UploadImageBody {
        retention: RETENTION,
        id: id.into(),
        encoding,
        width: w,
        height: h,
        total_bytes: payload.len() as u32,
        chunk_offset: 0,
        is_last: true,
        data: payload,
    }))
}

/// A video-frame upload streamed chunk-by-chunk from the event loop
/// (§8.2), so the loop — and the seek spinner — stays live during a
/// multi-megabyte transfer. The element retarget is *not* baked in at
/// queue time: the user may pan/zoom while the frame streams, so the
/// caller builds the follow-up from the live viewport when the final
/// chunk goes out, keeping the frame swap atomic with the upload's
/// completion.
struct ChunkedUpload {
    id: String,
    encoding: u8,
    width: u32,
    height: u32,
    payload: Vec<u8>,
    offset: usize,
}

impl ChunkedUpload {
    /// True if the next `pump` call sends the final chunk.
    fn next_is_last(&self) -> bool {
        self.offset + UPLOAD_CHUNK_BYTES >= self.payload.len()
    }

    /// Send the next chunk; `follow_up` rides in the final chunk's
    /// envelope. Returns `true` when the upload has fully streamed.
    fn pump<W: Write>(&mut self, out: &mut W, follow_up: Vec<Command>) -> bool {
        let end = (self.offset + UPLOAD_CHUNK_BYTES).min(self.payload.len());
        let is_last = end == self.payload.len();
        let mut cmds = vec![np(Command::UploadImage(UploadImageBody {
            // Pinned, like the still (see `RETENTION`). Only the first
            // chunk's flag is read by the host, but every chunk carries
            // the same value so the body is self-consistent.
            retention: RETENTION,
            id: self.id.clone(),
            encoding: self.encoding,
            width: self.width,
            height: self.height,
            total_bytes: self.payload.len() as u32,
            chunk_offset: self.offset as u32,
            is_last,
            data: self.payload[self.offset..end].to_vec(),
        }))];
        if is_last {
            cmds.extend(follow_up.into_iter().map(np));
        }
        send(out, &cmds);
        self.offset = end;
        is_last
    }
}

fn create_bg(cols: u16, media_rows: u16) -> Command {
    Command::CreateElement(CreateElementBody {
        id: EL_BG.into(),
        commands: vec![DrawCmd::FillRectangles {
            fill: flat(0.08, 0.08, 0.10, 1.0),
            rects: vec![Rect {
                x: 0.0,
                y: 0.0,
                w: cols as f32,
                h: media_rows as f32,
            }],
        }],
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: 0,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    })
}

fn create_status(cols: u16, rows: u16) -> Command {
    let sr = (rows - 1) as f32;
    let text = |x: f32, align: Align, c: (f32, f32, f32)| DrawCmd::DrawText {
        origin: Point { x, y: sr },
        align,
        fill: flat(c.0, c.1, c.2, 1.0),
        font_style: FontStyle::default(),
        text: String::new(),
    };
    Command::CreateElement(CreateElementBody {
        id: EL_STATUS.into(),
        commands: vec![
            DrawCmd::FillRectangles {
                fill: flat(0.10, 0.11, 0.14, 0.92),
                rects: vec![Rect {
                    x: 0.0,
                    y: sr,
                    w: cols as f32,
                    h: 1.0,
                }],
            },
            text(0.5, Align::Left, (0.86, 0.90, 0.96)),
            text(cols as f32 / 2.0, Align::Center, (0.86, 0.90, 0.96)),
            text(cols as f32 - 0.5, Align::Right, (0.70, 0.78, 0.90)),
        ],
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: 10,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    })
}

fn status_text(idx: usize, text: String) -> Command {
    Command::UpdateText(UpdateTextBody {
        id: EL_STATUS.into(),
        command_index: idx,
        range: UpdateTextRange::Whole,
        replacement: text,
    })
}

fn seek_rects(cols: u16, rows: u16, frac: f32) -> (Rect, Rect, Rect) {
    let sr = (rows - 2) as f32;
    let x = 1.0;
    let w = (cols as f32 - 2.0).max(1.0);
    let frac = frac.clamp(0.0, 1.0);
    let track = Rect {
        x,
        y: sr + 0.35,
        w,
        h: 0.3,
    };
    let prog = Rect {
        x,
        y: sr + 0.35,
        w: w * frac,
        h: 0.3,
    };
    let knob = Rect {
        x: (x + w * frac - 0.3).clamp(x - 0.3, x + w - 0.3),
        y: sr + 0.1,
        w: 0.6,
        h: 0.8,
    };
    (track, prog, knob)
}

fn create_seek(cols: u16, rows: u16, frac: f32) -> Command {
    let (t, p, k) = seek_rects(cols, rows, frac);
    Command::CreateElement(CreateElementBody {
        id: EL_SEEK.into(),
        commands: vec![
            DrawCmd::FillRectangles {
                fill: flat(0.20, 0.22, 0.27, 0.9),
                rects: vec![t],
            },
            DrawCmd::FillRectangles {
                fill: flat(ACCENT.0, ACCENT.1, ACCENT.2, 1.0),
                rects: vec![p],
            },
            DrawCmd::FillRectangles {
                fill: flat(0.85, 0.90, 0.97, 1.0),
                rects: vec![k],
            },
        ],
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: 11,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    })
}

fn update_seek(cols: u16, rows: u16, frac: f32) -> Vec<Command> {
    let (_, p, k) = seek_rects(cols, rows, frac);
    vec![
        Command::UpdateCommand(UpdateCommandBody {
            id: EL_SEEK.into(),
            index: 1,
            command: DrawCmd::FillRectangles {
                fill: flat(ACCENT.0, ACCENT.1, ACCENT.2, 1.0),
                rects: vec![p],
            },
        }),
        Command::UpdateCommand(UpdateCommandBody {
            id: EL_SEEK.into(),
            index: 2,
            command: DrawCmd::FillRectangles {
                fill: flat(0.85, 0.90, 0.97, 1.0),
                rects: vec![k],
            },
        }),
    ]
}

/// A centred, initially-hidden spinner: a fat white arc over a faint
/// ring, rotated via `UpdateTransform` (§9.11).
///
/// One element, geometry centred on the origin and aspect-compensated
/// (`x` in cell widths, `y` in rows) so both circles are pixel-circular —
/// the ring is rotation-invariant, so only the highlight arc appears to
/// spin. The per-tick update is a pure rotation matrix (§9.13); the
/// geometry is created once and never re-sent.
fn create_spinner(cols: u16, media_rows: u16, cell_pw: f32, cell_ph: f32) -> Command {
    let cx = cols as f32 / 2.0;
    let cy = media_rows as f32 / 2.0;
    let aspect = cell_ph / cell_pw;

    let ry = 0.48_f32;
    let rx = ry * aspect;
    let arc_pt = |theta: f32| Point {
        x: rx * theta.cos(),
        y: ry * theta.sin(),
    };
    use std::f32::consts::TAU;

    Command::CreateElement(CreateElementBody {
        id: EL_SPINNER.into(),
        commands: vec![
            // Faint full ring under the bright arc.
            DrawCmd::DrawLineLoop {
                stroke: flat(1.0, 1.0, 1.0, 0.25),
                line_width: 0.2,
                points: (0..32).map(|i| arc_pt(TAU * i as f32 / 32.0)).collect(),
            },
            // The rotating highlight: a 120° arc.
            DrawCmd::DrawLineStrip {
                stroke: flat(1.0, 1.0, 1.0, 1.0),
                line_width: 0.2,
                points: (0..=14).map(|i| arc_pt(TAU / 3.0 * i as f32 / 14.0)).collect(),
            },
        ],
        origin: Point { x: cx, y: cy },
        is_visible: false,
        draw_order: 20,
        parent: None,
        size: None,
        transform: Some(Transform::IDENTITY),
        anchor: OriginAnchor::Viewport,
    })
}

fn spinner_angle(theta: f32) -> Command {
    // Geometry is centred on the element's origin, so this is a pure
    // rotation matrix — no cell-size math needed (§9.13).
    Command::UpdateTransform {
        id: EL_SPINNER.into(),
        transform: Transform::rotate_about(theta, 0.0, 0.0, 1.0, 1.0),
    }
}

fn spinner_show(visible: bool) -> Command {
    Command::UpdateVisibility {
        id: EL_SPINNER.into(),
        is_visible: visible,
    }
}

fn fmt_pts(s: f64) -> String {
    let s = s.max(0.0);
    let m = (s / 60.0) as u64;
    let sec = s - (m as f64) * 60.0;
    format!("{m:02}:{sec:06.3}")
}

fn cursor_readout(cur: &Option<(u32, u32, [u8; 4])>) -> String {
    match cur {
        Some((x, y, c)) => format!(
            "({x},{y}) #{:02X}{:02X}{:02X}{:02X}",
            c[0], c[1], c[2], c[3]
        ),
        None => "—".into(),
    }
}

// --- rendering the media element ---

#[allow(clippy::too_many_arguments)]
fn render_image_mode<W: Write>(out: &mut W, vp: &Viewport, created: &mut bool) {
    let l = vp.layout();
    let cmd = if *created {
        update_image_el(l.target, IMG_ID, Some(l.source))
    } else {
        *created = true;
        create_image_el(l.target, IMG_ID, Some(l.source))
    };
    send(out, &[np(cmd)]);
}

/// One keyboard pan step, in cells.
const PAN_CELLS: f32 = 3.0;

/// Pan the viewport one step in `dir` (arrow keys in the axes that don't
/// mean something else, and `hjkl` always).
fn pan(vp: &mut Viewport, dir: Dir) {
    match dir {
        Dir::Up => vp.pan_cells(0.0, PAN_CELLS),
        Dir::Down => vp.pan_cells(0.0, -PAN_CELLS),
        Dir::Left => vp.pan_cells(PAN_CELLS, 0.0),
        Dir::Right => vp.pan_cells(-PAN_CELLS, 0.0),
    }
}

/// Move the playlist cursor `delta` entries and prepare the first entry
/// that both decodes and encodes, skipping the ones that don't — a
/// directory listing is no promise that every file is a usable image.
/// Returns the entry's path, its decoded pixels and the upload command
/// that installs them, or `None` (cursor left where it started) when
/// nothing else in the directory could be shown.
///
/// Decoding is synchronous, as it is at startup: a still is one upload,
/// with no seek spinner to keep alive.
fn step_image(
    pl: &mut Playlist,
    delta: isize,
    supported: u8,
    ssh: bool,
    max_image_bytes: u32,
) -> Option<(std::path::PathBuf, Frame, Command)> {
    let start = pl.index();
    // At most every *other* entry, so a directory of undecodable files
    // terminates instead of spinning.
    for _ in 0..pl.len().saturating_sub(1) {
        let path = pl.step(delta);
        let Ok(f) = load_image(&path) else { continue };
        // The upload is built here, before anything is sent, so a frame
        // that busts the host's limits is skipped like an undecodable
        // one rather than tearing down the picture on screen.
        let Ok(up) = upload_cmd(
            IMG_ID,
            f.w,
            f.h,
            f.rgba.clone(),
            supported,
            ssh,
            max_image_bytes,
        ) else {
            continue;
        };
        return Some((path, f, up));
    }
    pl.set_index(start);
    None
}

/// Queue the full decoded frame for upload under the next ping-pong id
/// as a [`ChunkedUpload`]. The event loop pumps the chunks and builds
/// the element retarget (with the viewport's then-current `source_rect`)
/// when the final chunk goes out; `cur_id` flips to the new texture only
/// then. Pan/zoom never re-enters this path — it is a host-side
/// `source_rect` update on the already-uploaded texture.
fn queue_frame_upload<W: Write>(
    out: &mut W,
    full: &Frame,
    cur_id: &str,
    upload: &mut Option<ChunkedUpload>,
    supported: u8,
    ssh: bool,
    max_image_bytes: u32,
) -> Result<()> {
    // Supersede a still-streaming previous frame: abort its upload
    // host-side (§8.2 — DropImage on an in-progress id) and reuse the
    // id. Its element retarget never went out, so there is nothing to
    // roll back.
    if let Some(old) = upload.take() {
        send(out, &[np(Command::DropImage { id: old.id, by_prefix: false })]);
    }

    // Same size-aware choice as the still path: raw locally, WebP when a
    // full-resolution frame would exceed the host's advertised limit.
    let enc = pick_encoding(full.w, full.h, supported, ssh, max_image_bytes)?;
    let (encoding, payload) = encode_payload(full.rgba.clone(), full.w, full.h, enc)?;
    if max_image_bytes > 0 && payload.len() > max_image_bytes as usize {
        bail!(
            "encoded {}x{} frame ({} bytes) exceeds the host limit of {} bytes",
            full.w,
            full.h,
            payload.len(),
            max_image_bytes
        );
    }

    // `cur_id` is the texture the element currently references; the new
    // frame streams into the other slot of the A/B pair.
    let next_id = if cur_id == IMG_ID_A { IMG_ID_B } else { IMG_ID_A };
    *upload = Some(ChunkedUpload {
        id: next_id.to_string(),
        encoding,
        width: full.w,
        height: full.h,
        payload,
        offset: 0,
    });
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vplay must run with stdin and stdout connected to a terminal");
    }

    let is_video = if cli.video {
        true
    } else if cli.image {
        false
    } else {
        is_video_ext(&cli.file)
    };

    let path_str = cli.file.to_string_lossy().into_owned();
    let mut name = file_label(&cli.file);

    // Probe video metadata / load the image up front so we fail before
    // touching the terminal if the input is bad or ffmpeg is missing.
    let meta: Option<VideoMeta> = if is_video {
        Some(probe_video(&path_str)?)
    } else {
        None
    };
    let image_frame: Option<Frame> = if is_video {
        None
    } else {
        Some(load_image(&cli.file)?)
    };

    // --- terminal setup ---
    let _raw = RawTty::enable()?;
    let winch = install_sigwinch();
    let mut out = std::io::stdout();
    out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h")?;
    out.flush()?;
    let _term = TermExit;

    drain_stale_stdin();
    let probe = run_probe(Duration::from_millis(cli.timeout_ms))?.ok_or_else(|| {
        anyhow::anyhow!("VGE probe timed out — this terminal does not appear to support VGE")
    })?;
    let cell_pw = probe.cell_pixel_width.max(1) as f32;
    let cell_ph = probe.cell_pixel_height.max(1) as f32;
    let supported = probe.supported_image_encodings;
    let max_image_bytes = probe.max_image_bytes;
    let ssh = is_ssh_session();

    let ws = winsize().ok_or_else(|| anyhow::anyhow!("could not query terminal size"))?;
    let mut cols = ws.ws_col.max(1);
    let mut rows = ws.ws_row.max(1);
    let min_rows = if is_video { 3 } else { 2 };
    if rows < min_rows {
        bail!("terminal too short ({rows} rows); need at least {min_rows}");
    }
    let mut media_rows = rows - if is_video { 2 } else { 1 };

    // Source dimensions for the viewport. Mutable: in image mode the
    // left/right arrows swap in a sibling still of any size.
    let (mut src_w, mut src_h) = match (&meta, &image_frame) {
        (Some(m), _) => (m.width, m.height),
        (_, Some(f)) => (f.w, f.h),
        _ => unreachable!(),
    };
    let mut vp = Viewport::new(
        src_w,
        src_h,
        cell_pw,
        cell_ph,
        0.0,
        0.0,
        cols as f32,
        media_rows as f32,
    );

    // Chrome.
    let mut frac0 = 0.0f32;
    send(
        &mut out,
        &[
            np(create_bg(cols, media_rows)),
            np(create_status(cols, rows)),
        ],
    );
    if is_video {
        send(
            &mut out,
            &[
                np(create_seek(cols, rows, frac0)),
                np(create_spinner(cols, media_rows, cell_pw, cell_ph)),
            ],
        );
    }

    // --- per-mode state ---
    let mut created_img = false;
    let mut cur_id = IMG_ID.to_string();
    let mut source_frame: Frame;
    // A decoded frame streaming to the terminal chunk-by-chunk (§8.2).
    let mut upload: Option<ChunkedUpload> = None;

    // Video state. There is no continuous playback: the displayed frame
    // changes only when the user seeks.
    let fps = meta.as_ref().map(|m| m.fps).unwrap_or(30.0);
    let mut cur_pts = 0.0f64;
    let mut cur_index = 0u64;

    // Image state: the opened file's sibling stills, which the left/right
    // arrows cycle through. Scanned once — the directory as it was when
    // vplay opened.
    let mut pl = if is_video {
        None
    } else {
        Some(Playlist::scan(&cli.file))
    };

    if is_video {
        let m = meta.as_ref().unwrap();
        // Grab and show the first frame. Queued, not sent — the event
        // loop streams it in chunks like any later frame.
        let first = grab_one_frame(&path_str, m.width, m.height, 0.0)?
            .ok_or_else(|| anyhow::anyhow!("could not decode the first video frame"))?;
        source_frame = Frame::new(m.width, m.height, first);
        queue_frame_upload(
            &mut out,
            &source_frame,
            &cur_id,
            &mut upload,
            supported,
            ssh,
            max_image_bytes,
        )?;
    } else {
        let f = image_frame.unwrap();
        source_frame = f.clone();
        // Upload the native image once; pan/zoom is source_rect-only.
        send(
            &mut out,
            &[np(upload_cmd(
                IMG_ID,
                f.w,
                f.h,
                f.rgba,
                supported,
                ssh,
                max_image_bytes,
            )?)],
        );
        render_image_mode(&mut out, &vp, &mut created_img);
    }

    // --- event loop ---
    let mut parser = InputParser::new();
    let mut inbuf = [0u8; 4096];
    let mut cursor: Option<(f32, f32)> = None;
    let mut drag = Drag::None;
    let mut dirty_media = false;
    let mut dirty_status = true;
    let mut dirty_seek = is_video;
    let mut quit = false;

    // Background decode of the frame the user just seeked to. While one is
    // in flight the loop animates a spinner and stays responsive; a newer
    // seek replaces it (killing the superseded ffmpeg).
    let mut pending: Option<Decode> = None;
    let mut busy_since: Option<Instant> = None;
    let mut spinner_visible = false;
    let spin_t0 = Instant::now();
    let mut last_spin: Option<Instant> = None;

    // Exact per-frame presentation times (display order). When available
    // they are the source of truth for the frame count and for mapping a
    // frame index to the timestamp ffmpeg should decode — this makes
    // seeking frame-exact even for variable-frame-rate streams. Empty for
    // images, or videos whose container yields no usable packet index; the
    // seek path then falls back to the `index / fps` grid.
    let frame_times = if is_video {
        probe_frame_times(&path_str)
    } else {
        Vec::new()
    };
    let total_frames = if !frame_times.is_empty() {
        Some(frame_times.len() as u64)
    } else {
        meta.as_ref().and_then(|m| m.total_frames())
    };
    let duration = meta.as_ref().map(|m| m.duration()).unwrap_or(0.0);

    while !quit {
        if take_sigwinch(winch)
            && let Some(ws) = winsize()
        {
            cols = ws.ws_col.max(1);
            rows = ws.ws_row.max(min_rows);
            media_rows = rows - if is_video { 2 } else { 1 };
            vp.set_viewport(0.0, 0.0, cols as f32, media_rows as f32);
            // Elements only — the textures must survive, and the loop
            // recreates the image element over the live one below.
            send(
                &mut out,
                &[np(Command::DeleteElement {
                    id: ID_PREFIX.into(),
                    by_prefix: true,
                })],
            );
            created_img = false;
            send(
                &mut out,
                &[
                    np(create_bg(cols, media_rows)),
                    np(create_status(cols, rows)),
                ],
            );
            if is_video {
                send(
                    &mut out,
                    &[
                        np(create_seek(cols, rows, frac0)),
                        np(create_spinner(cols, media_rows, cell_pw, cell_ph)),
                    ],
                );
                // The wipe took the spinner with it; let the loop
                // re-show it if a decode is still pending.
                spinner_visible = false;
            }
            dirty_media = true;
            dirty_status = true;
            dirty_seek = is_video;
        }

        // How long to block waiting for input. With no continuous playback
        // the loop is event-driven; 50 ms keeps a lone ESC responsive. While
        // a decode is pending, wake more often (and on the decode's pipe) to
        // animate the spinner — a ~50-byte UpdateTransform per tick — and
        // apply the frame the moment it lands. While an upload is streaming,
        // barely block at all: each iteration pushes one chunk, and the 1 ms
        // poll keeps input (a superseding seek) flowing between chunks.
        let tick = if upload.is_some() {
            1
        } else if pending.is_some() {
            33
        } else {
            50
        };
        let deadline = Instant::now() + Duration::from_millis(tick);

        let stdin_ready = match pending.as_ref() {
            Some(d) => poll_stdin_and(d.fd(), deadline).unwrap_or((false, false)).0,
            None => poll_stdin_until(deadline).unwrap_or(false),
        };
        let events = if stdin_ready {
            let n = read_stdin(&mut inbuf).unwrap_or(0);
            if n == 0 {
                break;
            }
            parser.feed(&inbuf[..n])
        } else {
            parser.flush()
        };

        for ev in events {
            match ev {
                Event::Quit => quit = true,
                Event::Fit => {
                    vp.fit();
                    dirty_media = true;
                    dirty_status = true;
                }
                Event::Actual => {
                    vp.actual();
                    dirty_media = true;
                    dirty_status = true;
                }
                Event::ZoomIn | Event::ZoomOut => {
                    let factor = if ev == Event::ZoomIn { 1.25 } else { 0.8 };
                    let (c, r) = cursor.unwrap_or((
                        vp.origin_col + vp.vp_cols / 2.0,
                        vp.origin_row + vp.vp_rows / 2.0,
                    ));
                    vp.zoom_at(factor, c, r);
                    dirty_media = true;
                    dirty_status = true;
                }
                Event::WheelUp { col, row } | Event::WheelDown { col, row } => {
                    cursor = Some((col as f32 + 0.5, row as f32 + 0.5));
                    let factor = if matches!(ev, Event::WheelUp { .. }) {
                        1.2
                    } else {
                        1.0 / 1.2
                    };
                    vp.zoom_at(factor, col as f32 + 0.5, row as f32 + 0.5);
                    dirty_media = true;
                    dirty_status = true;
                }
                Event::Pan(dir) => {
                    pan(&mut vp, dir);
                    dirty_media = true;
                    dirty_status = true;
                }
                Event::Arrow(dir) => {
                    let horizontal = matches!(dir, Dir::Left | Dir::Right);
                    if is_video && horizontal {
                        let dt = if dir == Dir::Right { 5.0 } else { -5.0 };
                        let target = frame_at_time(cur_pts + dt, &frame_times, fps);
                        request_seek(
                            target,
                            total_frames,
                            &frame_times,
                            meta.as_ref().unwrap(),
                            &path_str,
                            &mut cur_pts,
                            &mut cur_index,
                            &mut pending,
                        )?;
                        dirty_status = true;
                        dirty_seek = true;
                    } else if horizontal
                        && let Some(pl) = pl.as_mut()
                    {
                        // Image mode: walk the directory's stills.
                        // Horizontal panning moves to `h`/`l`.
                        let delta = if dir == Dir::Right { 1 } else { -1 };
                        if let Some((path, f, up)) =
                            step_image(pl, delta, supported, ssh, max_image_bytes)
                        {
                            name = file_label(&path);
                            src_w = f.w;
                            src_h = f.h;
                            source_frame = f;
                            // A fresh still starts fitted, like the
                            // opened one did.
                            vp = Viewport::new(
                                src_w,
                                src_h,
                                cell_pw,
                                cell_ph,
                                0.0,
                                0.0,
                                cols as f32,
                                media_rows as f32,
                            );
                            // The still texture is a single slot, so the
                            // swap is drop-then-upload (sound only
                            // because the still is pinned — see
                            // `upload_cmd`), kept in one envelope
                            // together with the element's new geometry so
                            // no frame is ever composed with the old
                            // source_rect over the new texture.
                            let l = vp.layout();
                            let el = if created_img {
                                update_image_el(l.target, IMG_ID, Some(l.source))
                            } else {
                                created_img = true;
                                create_image_el(l.target, IMG_ID, Some(l.source))
                            };
                            send(
                                &mut out,
                                &[
                                    np(Command::DropImage {
                                        id: IMG_ID.into(),
                                        by_prefix: false,
                                    }),
                                    np(up),
                                    np(el),
                                ],
                            );
                            dirty_status = true;
                        }
                    } else {
                        pan(&mut vp, dir);
                        dirty_media = true;
                        dirty_status = true;
                    }
                }
                Event::StepNext | Event::StepPrev => {
                    if is_video {
                        let target =
                            cur_index as i64 + if ev == Event::StepNext { 1 } else { -1 };
                        request_seek(
                            target,
                            total_frames,
                            &frame_times,
                            meta.as_ref().unwrap(),
                            &path_str,
                            &mut cur_pts,
                            &mut cur_index,
                            &mut pending,
                        )?;
                        dirty_status = true;
                        dirty_seek = true;
                    }
                }
                Event::MouseDown { col, row } => {
                    cursor = Some((col as f32 + 0.5, row as f32 + 0.5));
                    if is_video && row == rows - 2 {
                        drag = Drag::Seek;
                        let frac =
                            ((col as f32 - 1.0) / (cols as f32 - 2.0).max(1.0)).clamp(0.0, 1.0);
                        request_seek(
                            frame_at_time(frac as f64 * duration, &frame_times, fps),
                            total_frames,
                            &frame_times,
                            meta.as_ref().unwrap(),
                            &path_str,
                            &mut cur_pts,
                            &mut cur_index,
                            &mut pending,
                        )?;
                        dirty_status = true;
                        dirty_seek = true;
                    } else {
                        drag = Drag::Pan {
                            last_col: col as f32,
                            last_row: row as f32,
                        };
                    }
                }
                Event::MouseUp { .. } => {
                    drag = Drag::None;
                }
                Event::MouseMove { col, row, pressed } => {
                    cursor = Some((col as f32 + 0.5, row as f32 + 0.5));
                    dirty_status = true;
                    match drag {
                        Drag::Pan { last_col, last_row } if pressed => {
                            let dcol = col as f32 - last_col;
                            let drow = row as f32 - last_row;
                            vp.pan_cells(dcol, drow);
                            drag = Drag::Pan {
                                last_col: col as f32,
                                last_row: row as f32,
                            };
                            dirty_media = true;
                        }
                        Drag::Seek if pressed && is_video => {
                            let frac =
                                ((col as f32 - 1.0) / (cols as f32 - 2.0).max(1.0)).clamp(0.0, 1.0);
                            request_seek(
                                frame_at_time(frac as f64 * duration, &frame_times, fps),
                                total_frames,
                                &frame_times,
                                meta.as_ref().unwrap(),
                                &path_str,
                                &mut cur_pts,
                                &mut cur_index,
                                &mut pending,
                            )?;
                            dirty_status = true;
                            dirty_seek = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        if quit {
            break;
        }

        // Apply a finished background decode (or discard a failed one). On
        // EAGAIN it stays pending and the spinner keeps animating.
        if pending.is_some() {
            match pending.as_mut().unwrap().poll() {
                DecodeState::Pending => {}
                DecodeState::Done(rgba) => {
                    let m = meta.as_ref().unwrap();
                    source_frame = Frame::new(m.width, m.height, rgba);
                    pending = None;
                    queue_frame_upload(
                        &mut out,
                        &source_frame,
                        &cur_id,
                        &mut upload,
                        supported,
                        ssh,
                        max_image_bytes,
                    )?;
                    dirty_status = true;
                }
                DecodeState::Failed => pending = None,
            }
        }

        // Coalesced redraws. Pan/zoom is pure host-side: re-point the
        // element's source_rect at the already-uploaded full-frame
        // texture — no pixels travel.
        if dirty_media {
            if is_video {
                let l = vp.layout();
                if created_img {
                    send(
                        &mut out,
                        &[np(update_image_el(l.target, &cur_id, Some(l.source)))],
                    );
                } else if cur_id == IMG_ID_A || cur_id == IMG_ID_B {
                    // After a resize the element is gone but the texture
                    // survives in the session image table — recreate the
                    // element over it, no re-upload. This is the reason
                    // frames are pinned: releasing the deleted element's
                    // reference would collect an `Auto` texture and this
                    // would name a dead id (see `RETENTION`).
                    send(
                        &mut out,
                        &[np(create_image_el(l.target, &cur_id, Some(l.source)))],
                    );
                    created_img = true;
                }
                // Otherwise no texture has finished uploading yet; the
                // first upload's completion creates the element.
            } else {
                render_image_mode(&mut out, &vp, &mut created_img);
            }
            dirty_media = false;
            dirty_status = true;
        }

        // Stream the next chunk of an in-flight frame upload. The final
        // chunk carries the element retarget — built here, from the
        // viewport's current layout, so pans/zooms made while the frame
        // streamed are honoured — and the texture the element references
        // flips here and only here.
        if let Some(u) = upload.as_mut() {
            let follow_up = if u.next_is_last() {
                let l = vp.layout();
                let mut fu = vec![if created_img {
                    update_image_el(l.target, &u.id, Some(l.source))
                } else {
                    create_image_el(l.target, &u.id, Some(l.source))
                }];
                created_img = true;
                // Retire the texture the element previously referenced
                // (also covers the one surviving a resize), or the next
                // A/B flip would collide with it. Now that frames are
                // pinned this drop *is* the reclamation — under `Auto`
                // the retarget above already collected the outgoing slot
                // and this was a silent no-op.
                if cur_id == IMG_ID_A || cur_id == IMG_ID_B {
                    fu.push(Command::DropImage {
                        id: cur_id.clone(),
                        by_prefix: false,
                    });
                }
                fu
            } else {
                Vec::new()
            };
            if u.pump(&mut out, follow_up) {
                cur_id = u.id.clone();
                upload = None;
            }
        }

        if dirty_status {
            let cur = cursor
                .and_then(|(c, r)| vp.cursor_pixel(c, r))
                .map(|(x, y)| (x, y, source_frame.pixel(x, y)));
            let (left, center, right) = if is_video {
                let totals = total_frames
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "?".into());
                (
                    format!("{name}  {src_w}x{src_h}  {fps:.2}fps"),
                    format!(
                        "{}%  f {}/{}  {}",
                        vp.zoom_percent(),
                        cur_index,
                        totals,
                        fmt_pts(cur_pts)
                    ),
                    cursor_readout(&cur),
                )
            } else {
                // The playlist position only earns its space when there
                // is somewhere to cycle to.
                let pos = match pl.as_ref().filter(|p| p.len() > 1) {
                    Some(p) => format!("  {}/{}", p.index() + 1, p.len()),
                    None => String::new(),
                };
                (
                    format!("{name}  {src_w}x{src_h}"),
                    format!("{}%{pos}", vp.zoom_percent()),
                    cursor_readout(&cur),
                )
            };
            send(
                &mut out,
                &[
                    np(status_text(1, left)),
                    np(status_text(2, center)),
                    np(status_text(3, right)),
                ],
            );
            dirty_status = false;
        }

        if is_video && dirty_seek {
            let frac = if duration > 0.0 {
                (cur_pts / duration).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            frac0 = frac;
            let cmds: Vec<(Command, u32)> =
                update_seek(cols, rows, frac).into_iter().map(np).collect();
            send(&mut out, &cmds);
            dirty_seek = false;
        }

        // Spinner: reveal once the picture has been stale for SPIN_DELAY
        // — counting both the decode and the chunked upload that follows
        // it — so quick seeks/steps don't flash it; hide it the moment
        // the picture catches up. The angle is wall-clock based so
        // the rotation rate is steady across tick lengths; updates are
        // rate-limited to one transform per SPIN_FRAME.
        match (pending.is_some() || upload.is_some(), busy_since) {
            (true, None) => busy_since = Some(Instant::now()),
            (false, Some(_)) => busy_since = None,
            _ => {}
        }
        let busy = busy_since.is_some_and(|t| t.elapsed() >= SPIN_DELAY);
        if busy {
            let mut cmds: Vec<(Command, u32)> = Vec::new();
            if !spinner_visible {
                spinner_visible = true;
                cmds.push(np(spinner_show(true)));
            }
            if last_spin.is_none_or(|t| t.elapsed() >= SPIN_FRAME) {
                last_spin = Some(Instant::now());
                cmds.push(np(spinner_angle(
                    spin_t0.elapsed().as_secs_f32() * SPIN_SPEED,
                )));
            }
            send(&mut out, &cmds);
        } else if spinner_visible {
            spinner_visible = false;
            send(&mut out, &[np(spinner_show(false))]);
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum Drag {
    None,
    Pan { last_col: f32, last_row: f32 },
    Seek,
}

/// Map a timeline position in seconds to the index of the frame visible at
/// that instant: the last frame whose presentation time is `<= time`. Uses
/// the exact PTS table when present (correct for variable-frame-rate
/// streams), else the `round(time * fps)` grid.
fn frame_at_time(time: f64, frame_times: &[f64], fps: f64) -> i64 {
    if frame_times.is_empty() {
        return (time * fps.max(1.0)).round() as i64;
    }
    match frame_times.partition_point(|&t| t <= time) {
        0 => 0,
        n => (n - 1) as i64,
    }
}

/// Resolve a frame `index` (clamped to the valid range) to the frame's
/// presentation time and the timestamp ffmpeg should decode.
///
/// Seeking is frame-exact. With a PTS table the frame's real presentation
/// time is known, so ffmpeg is aimed at the *middle* of the target frame
/// (halfway to the next frame's PTS) — robust against float slop and
/// variable frame spacing. Without one it falls back to the CFR grid,
/// aiming at `(index + 0.5) / fps`. Either way `cur_index` ends up equal to
/// the frame actually shown, and callers address frames by index so every
/// path snaps to the same grid and never drifts. Returns
/// `(clamped_index, frame_pts, aim_time)`.
fn frame_aim(index: i64, total_frames: Option<u64>, frame_times: &[f64], meta: &VideoMeta) -> (u64, f64, f64) {
    let fps = meta.fps.max(1.0);
    let mut idx = index.max(0) as u64;
    if let Some(last) = total_frames.map(|t| t.saturating_sub(1)) {
        idx = idx.min(last);
    }
    let i = idx as usize;
    let (pts, aim) = if let Some(&t0) = frame_times.get(i) {
        let aim = if let Some(&t1) = frame_times.get(i + 1) {
            // Centre of frame `i`: between its PTS and the next frame's.
            (t0 + t1) * 0.5
        } else if i > 0 {
            // Last frame: nudge just past its PTS by half the prior gap.
            t0 + (t0 - frame_times[i - 1]).max(0.0) * 0.5
        } else {
            t0
        };
        (t0, aim)
    } else {
        // No PTS table — assume constant frame rate.
        (idx as f64 / fps, (idx as f64 + 0.5) / fps)
    };
    (
        idx,
        pts.clamp(0.0, meta.duration()),
        aim.clamp(0.0, meta.duration()),
    )
}

/// Kick off the decode of frame `index` in the background, replacing any
/// decode already in flight (its ffmpeg is killed when the old [`Decode`] is
/// dropped). `cur_index` / `cur_pts` are updated immediately so the status
/// bar and seek knob track the target while the picture catches up; the
/// decoded frame is applied later by the event loop when the decode lands.
#[allow(clippy::too_many_arguments)]
fn request_seek(
    index: i64,
    total_frames: Option<u64>,
    frame_times: &[f64],
    meta: &VideoMeta,
    path: &str,
    cur_pts: &mut f64,
    cur_index: &mut u64,
    pending: &mut Option<Decode>,
) -> Result<()> {
    let (idx, pts, aim) = frame_aim(index, total_frames, frame_times, meta);
    *cur_index = idx;
    *cur_pts = pts;
    *pending = Some(start_decode(path, meta.width, meta.height, aim)?);
    Ok(())
}

/// Restores the terminal (leaves alt screen, re-shows cursor, disables
/// mouse) and clears VGE state on drop.
struct TermExit;

impl Drop for TermExit {
    fn drop(&mut self) {
        let mut o = std::io::stdout();
        // Two prefix sweeps (§6.2 / §8.2) take everything vplay owns:
        // every element, and every texture — the live one, whichever A/B
        // slot is stale, and any in-flight chunked upload (§8.2). Naming
        // the ids individually meant two of the three drops always failed,
        // since only one texture is ever live.
        let env = build_envelope(&[
            (
                Command::DeleteElement {
                    id: ID_PREFIX.into(),
                    by_prefix: true,
                },
                REQ_ID_NO_RESPONSE,
            ),
            (
                Command::DropImage {
                    id: ID_PREFIX.into(),
                    by_prefix: true,
                },
                REQ_ID_NO_RESPONSE,
            ),
        ]);
        let _ = o.write_all(&env);
        let _ = o.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        let _ = o.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::frame_at_time;

    #[test]
    fn frame_at_time_uses_pts_table() {
        // Variable spacing: frame 2 is long (1.0..2.5).
        let times = [0.0, 0.5, 1.0, 2.5, 3.0];
        // Exact boundaries select that frame.
        assert_eq!(frame_at_time(0.0, &times, 25.0), 0);
        assert_eq!(frame_at_time(1.0, &times, 25.0), 2);
        // Mid-frame stays on the frame whose PTS it's past.
        assert_eq!(frame_at_time(2.0, &times, 25.0), 2);
        assert_eq!(frame_at_time(2.5, &times, 25.0), 3);
        // Before the start clamps to 0; past the end clamps to the last.
        assert_eq!(frame_at_time(-1.0, &times, 25.0), 0);
        assert_eq!(frame_at_time(99.0, &times, 25.0), 4);
    }

    #[test]
    fn frame_at_time_falls_back_to_grid() {
        // No PTS table: round(time * fps).
        assert_eq!(frame_at_time(0.0, &[], 25.0), 0);
        assert_eq!(frame_at_time(1.0, &[], 25.0), 25);
        assert_eq!(frame_at_time(0.5, &[], 25.0), 13); // round(12.5) -> 13
    }
}
