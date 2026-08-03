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
mod work;

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Parser;
use vge_protocol::codec::{Point, Rect};
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
use vge_render::upload::Encoding;

use image_src::Frame;
use input::{Dir, Event, InputParser};
use playlist::Playlist;
use video::{Decode, DecodeState, VideoMeta, probe_frame_times, probe_video, start_decode};
use viewport::Viewport;

/// Namespace every element and image id shares (§6.8), so cleanup is one
/// prefix sweep per table rather than a list of ids to keep in step.
const ID_PREFIX: &str = "vplay-";

const EL_BG: &str = "vplay-bg";
const EL_IMG: &str = "vplay-img";
const EL_STATUS: &str = "vplay-status";
const EL_SEEK: &str = "vplay-seek";
const EL_PROGRESS: &str = "vplay-prog";
const IMG_ID_A: &str = "vplay-fa";
const IMG_ID_B: &str = "vplay-fb";

const ACCENT: (f32, f32, f32) = (0.337, 0.475, 0.624); // #56799f

/// Retention for every texture vplay uploads (§8.2).
///
/// Manual — vplay owns its textures' lifetimes, because the host's `Auto`
/// refcount does not survive what the `SIGWINCH` path does to a picture:
/// it deletes every `vplay-` element and recreates the chrome, then
/// recreates the image element over the texture it already uploaded. But
/// deleting an element releases its `DrawImage` reference (§8.0), and an
/// `Auto` image at zero refs is collected there and then — so the
/// recreate would name a texture the host had just thrown away:
/// `ERR_UNKNOWN_IMAGE`, silent (§4: unrequested commands get no
/// response), and a blank picture.
///
/// In exchange every id must be released by hand — which vplay already
/// does: the upload's final chunk drops the texture it supersedes,
/// [`queue_upload`] drops a superseded in-flight upload, and `TermExit`
/// sweeps the whole `vplay-` prefix (§8.2) on the way out. At most two
/// textures are ever live: every picture, still or video frame, streams
/// into whichever of the ping-pong pair the screen isn't using.
const RETENTION: Retention = Retention::Manual;

/// Minimum interval between progress-panel repaints.
const PROGRESS_FRAME: Duration = Duration::from_millis(33);
/// How long the loop must stay busy before the progress panel appears,
/// so quick seeks and steps don't flash an indicator.
const BUSY_DELAY: Duration = Duration::from_millis(160);
/// Sweeps per second of the indeterminate bar (one out-and-back is two
/// sweeps). Time-based, so the rate is independent of the loop's tick.
const SWEEP_SPEED: f32 = 1.1;
/// Uploads bigger than this stream chunk-by-chunk from the event loop
/// (§8.2) so the loop — and the progress panel — stays live during the
/// transfer; one chunk's PTY write is short enough to not visibly stall
/// the loop.
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

/// A picture's upload, streamed chunk-by-chunk from the event loop
/// (§8.2), so the loop — and the progress panel — stays live during a
/// multi-megabyte transfer. Stills and video frames both take this path:
/// a still is simply a picture that changes only when the user steps to
/// the next file. The element retarget is *not* baked in at queue time:
/// the user may pan/zoom while it streams, so the caller builds the
/// follow-up from the live viewport when the final chunk goes out,
/// keeping the swap atomic with the upload's completion.
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
            // Pinned — see `RETENTION`. Only the first chunk's flag is
            // read by the host, but every chunk carries the same value
            // so the body is self-consistent.
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

// --- the progress panel ---

/// What the loop is waiting on, and how far along it is. The three
/// stages a picture goes through are visibly different work — decoded,
/// encoded, then streamed to the terminal — so the panel names the one
/// it is in instead of just saying "busy". Only the last can be
/// measured; the other two report a name and sweep.
enum Busy<'a> {
    /// Producing the pixels: ffmpeg for a video frame, which reports the
    /// fraction of it written back so far (see [`Decode::progress`]), or
    /// the worker for a still, which can only name the file.
    Decoding {
        what: Option<&'a str>,
        frac: Option<f32>,
    },
    /// Compressing the payload on the worker (WebP; a raw payload is a
    /// copy, over before the panel could appear).
    Encoding,
    /// Streaming an encoded payload to the terminal, `done` of `total`
    /// bytes handed to the PTY.
    Sending { done: usize, total: usize },
}

impl Busy<'_> {
    /// Fraction for the bar, or `None` when the phase can't be measured
    /// and the bar should sweep instead.
    fn frac(&self) -> Option<f32> {
        match self {
            Busy::Decoding { frac, .. } => *frac,
            Busy::Encoding => None,
            Busy::Sending { done, total } => Some(frac_of(*done, *total)),
        }
    }

    fn label(&self) -> String {
        match self {
            Busy::Decoding {
                what: Some(name), ..
            } => format!("decoding {name}"),
            Busy::Decoding {
                frac: Some(f),
                what: None,
            } => format!("decoding  {:.0}%", f * 100.0),
            Busy::Decoding {
                frac: None,
                what: None,
            } => "decoding".to_string(),
            Busy::Encoding => "encoding".to_string(),
            Busy::Sending { done, total } => format!(
                "sending  {:.0}%  {:.1}/{:.1} MB",
                frac_of(*done, *total) * 100.0,
                mib(*done),
                mib(*total)
            ),
        }
    }
}

fn frac_of(done: usize, total: usize) -> f32 {
    if total == 0 {
        1.0
    } else {
        (done as f32 / total as f32).clamp(0.0, 1.0)
    }
}

fn mib(bytes: usize) -> f32 {
    bytes as f32 / (1024.0 * 1024.0)
}

/// Track rectangle of the progress bar and the origin of its label, both
/// centred in the media area. Recomputed from the current size rather
/// than stored, so the resize path's recreate lands in the right place.
fn progress_rects(cols: u16, media_rows: u16) -> (Rect, Point) {
    let w = (cols as f32 * 0.5).clamp(12.0, 48.0).min(cols as f32 - 2.0).max(2.0);
    let row = (media_rows as f32 * 0.5).floor();
    let track = Rect {
        x: ((cols as f32 - w) * 0.5).round(),
        y: row + 0.35,
        w,
        h: 0.3,
    };
    let label = Point {
        x: cols as f32 * 0.5,
        y: (row - 1.0).max(0.0),
    };
    (track, label)
}

/// The initially-hidden progress panel: a bar over the media, with its
/// phase and percentage on the row above. Command indices are fixed —
/// 0 track, 1 fill, 2 label — and [`progress_fill`] / [`progress_label`]
/// update the latter two in place.
fn create_progress(cols: u16, media_rows: u16) -> Command {
    let (track, label) = progress_rects(cols, media_rows);
    Command::CreateElement(CreateElementBody {
        id: EL_PROGRESS.into(),
        commands: vec![
            DrawCmd::FillRectangles {
                fill: flat(0.20, 0.22, 0.27, 0.9),
                rects: vec![track],
            },
            DrawCmd::FillRectangles {
                fill: flat(ACCENT.0, ACCENT.1, ACCENT.2, 1.0),
                rects: vec![Rect { w: 0.0, ..track }],
            },
            DrawCmd::DrawText {
                origin: label,
                align: Align::Center,
                fill: flat(0.86, 0.90, 0.96, 1.0),
                font_style: FontStyle::default(),
                text: String::new(),
            },
        ],
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: false,
        draw_order: 20,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    })
}

/// The bar's filled span: anchored left and proportional when the phase
/// has a fraction, else a segment sliding out and back, which reads as
/// "working, length unknown" rather than as a stuck bar. `t` is elapsed
/// wall-clock seconds, so the sweep rate is independent of the tick.
fn progress_fill(track: Rect, frac: Option<f32>, t: f32) -> Command {
    let rect = match frac {
        Some(f) => Rect {
            w: track.w * f.clamp(0.0, 1.0),
            ..track
        },
        None => {
            let seg = track.w * 0.25;
            // Triangle wave in 0..=1: out, then back.
            let phase = (t * SWEEP_SPEED) % 2.0;
            let k = if phase <= 1.0 { phase } else { 2.0 - phase };
            Rect {
                x: track.x + (track.w - seg) * k,
                w: seg,
                ..track
            }
        }
    };
    Command::UpdateCommand(UpdateCommandBody {
        id: EL_PROGRESS.into(),
        index: 1,
        command: DrawCmd::FillRectangles {
            fill: flat(ACCENT.0, ACCENT.1, ACCENT.2, 1.0),
            rects: vec![rect],
        },
    })
}

fn progress_label(text: String) -> Command {
    Command::UpdateText(UpdateTextBody {
        id: EL_PROGRESS.into(),
        command_index: 2,
        range: UpdateTextRange::Whole,
        replacement: text,
    })
}

fn progress_show(visible: bool) -> Command {
    Command::UpdateVisibility {
        id: EL_PROGRESS.into(),
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

// --- driving the media element ---

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

/// Wrap a worker-encoded payload as a [`ChunkedUpload`] aimed at
/// whichever half of the ping-pong pair the screen isn't showing —
/// `cur_id` is the texture the element currently references (empty
/// before the first upload lands), so the old picture stays drawable
/// until the new one has fully arrived.
///
/// Nothing is sent here: the event loop pumps the chunks and builds the
/// element retarget (with the viewport's then-current `source_rect`)
/// when the final one goes out, so `cur_id` flips only then. Pan/zoom
/// never re-enters this path — it is a host-side `source_rect` update on
/// the already-uploaded texture.
fn into_upload(cur_id: &str, frame: &Frame, encoding: u8, payload: Vec<u8>) -> ChunkedUpload {
    let next_id = if cur_id == IMG_ID_A { IMG_ID_B } else { IMG_ID_A };
    ChunkedUpload {
        id: next_id.to_string(),
        encoding,
        width: frame.w,
        height: frame.h,
        payload,
        offset: 0,
    }
}

/// Make `up` the in-flight upload. Any earlier one is superseded: its
/// texture is aborted host-side (§8.2 — `DropImage` on an in-progress
/// id) so `up` can stream into that slot, and since the superseded
/// upload's element retarget never went out there is nothing to roll
/// back on screen.
fn queue_upload<W: Write>(out: &mut W, upload: &mut Option<ChunkedUpload>, up: ChunkedUpload) {
    if let Some(old) = upload.take() {
        send(out, &[np(Command::DropImage { id: old.id, by_prefix: false })]);
    }
    *upload = Some(up);
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

    // Probe video metadata up front — it is cheap, and it fails before
    // we touch the terminal if the input is bad or ffmpeg is missing.
    // The pixels are decoded further down, after the chrome exists, so
    // the wait has something to say for itself.
    let meta: Option<VideoMeta> = if is_video {
        Some(probe_video(&path_str)?)
    } else {
        None
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

    // Chrome first, so the decode below has somewhere to report itself.
    // The progress panel exists in both modes — a still's upload is as
    // worth reporting as a frame's — and starts hidden.
    let mut frac0 = 0.0f32;
    send(
        &mut out,
        &[
            np(create_bg(cols, media_rows)),
            np(create_status(cols, rows)),
            np(create_progress(cols, media_rows)),
        ],
    );
    if is_video {
        send(&mut out, &[np(create_seek(cols, rows, frac0))]);
    }

    // Source dimensions. Known up front for a video (ffprobe read them);
    // for a still they arrive with the decoded picture, and stay zero
    // until then — the status bar leaves the size out rather than
    // claiming one, and every later still may differ again.
    let (mut src_w, mut src_h) = match &meta {
        Some(m) => (m.width, m.height),
        None => (0, 0),
    };
    // Rebuilt when the first still lands (see `apply_ready`); a video's
    // is right from the start.
    let mut vp = Viewport::new(
        src_w.max(1),
        src_h.max(1),
        cell_pw,
        cell_ph,
        0.0,
        0.0,
        cols as f32,
        media_rows as f32,
    );

    // --- per-mode state ---
    let mut created_img = false;
    // The texture the image element currently draws; empty until the
    // first upload completes, which is also what tells the loop there is
    // nothing to point an element at yet.
    let mut cur_id = String::new();
    // The picture's pixels, kept for the cursor's colour readout. A 1x1
    // stand-in until the first one is decoded; `have_picture` says which.
    let mut source_frame = Frame::new(1, 1, vec![0; 4]);
    let mut have_picture = false;
    // A picture streaming to the terminal chunk-by-chunk (§8.2).
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

    // Decoding and encoding run on a worker (see `work`), so neither
    // blocks the loop: keys keep working and the progress panel keeps
    // animating through both.
    let enc_params = work::EncodeParams {
        supported,
        ssh,
        max_image_bytes,
    };
    let mut worker = work::Worker::spawn();
    // Background decode of the frame the user just seeked to — ffmpeg,
    // which is its own kind of worker. A newer seek replaces it (killing
    // the superseded ffmpeg).
    let mut pending: Option<Decode> = None;

    // The first picture, started the same way every later one is: a
    // still goes to the worker, a video frame to ffmpeg. The loop picks
    // up whichever lands, so the wait is reported rather than sat on.
    if is_video {
        let m = meta.as_ref().unwrap();
        pending = Some(start_decode(&path_str, m.width, m.height, 0.0)?);
    } else {
        worker.load(cli.file.clone(), enc_params);
    }
    // Name of the still being decoded, for the panel to show while the
    // worker is on it. `None` in video mode, where the decode reports a
    // fraction instead.
    let mut loading: Option<String> = (!is_video).then(|| name.clone());
    // The playlist walk a failed decode should continue: direction, the
    // index to fall back to when the whole directory refuses, and how
    // many entries have been tried.
    let mut step_dir = 0isize;
    let mut step_start = 0usize;
    let mut step_tries = 0usize;
    // Set when the file vplay was opened on turns out to be unusable.
    // Reported after the loop, once the alt screen is back.
    let mut fatal: Option<String> = None;

    // --- event loop ---
    let mut parser = InputParser::new();
    let mut inbuf = [0u8; 4096];
    let mut cursor: Option<(f32, f32)> = None;
    let mut drag = Drag::None;
    let mut dirty_media = false;
    let mut dirty_status = true;
    let mut dirty_seek = is_video;
    let mut quit = false;

    // Progress panel: when the current wait started (`None` = idle), so
    // it can be held back for BUSY_DELAY, and the repaint rate limit.
    let mut busy_since: Option<Instant> = None;
    let mut progress_visible = false;
    let busy_t0 = Instant::now();
    let mut last_progress: Option<Instant> = None;

    // Exact per-frame presentation times (display order). When available
    // they are the source of truth for the frame count and for mapping a
    // frame index to the timestamp ffmpeg should decode — this makes
    // seeking frame-exact even for variable-frame-rate streams. Empty for
    // images, or videos whose container yields no usable packet index; the
    // seek path then falls back to the `index / fps` grid.
    //
    // Probed on a thread, because it costs one demux of the whole file:
    // measured at ~140 packets/s on a 6.5 GB recording, which is two
    // minutes of ffprobe for a file that long. Run inline it delayed the
    // event loop — and therefore the first frame, which the loop is what
    // applies — for exactly that long, with the chrome up and nothing in
    // it. The grid fallback covers seeking until the table lands.
    let mut frame_times: Vec<f64> = Vec::new();
    let frame_times_rx = if is_video {
        let (tx, rx) = std::sync::mpsc::channel();
        let path = path_str.clone();
        std::thread::spawn(move || {
            let _ = tx.send(probe_frame_times(&path));
        });
        Some(rx)
    } else {
        None
    };
    let mut total_frames = meta.as_ref().and_then(|m| m.total_frames());
    let duration = meta.as_ref().map(|m| m.duration()).unwrap_or(0.0);

    while !quit {
        // The packet index, if the probe thread has finished with it.
        // Adopting it mid-session only sharpens seeking: the frame the
        // user is on keeps its timestamp, and the count firms up from
        // the container's estimate to the real one.
        if let Some(rx) = frame_times_rx.as_ref()
            && let Ok(times) = rx.try_recv()
            && !times.is_empty()
        {
            total_frames = Some(times.len() as u64);
            frame_times = times;
            dirty_status = true;
            dirty_seek = true;
        }

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
                    np(create_progress(cols, media_rows)),
                ],
            );
            if is_video {
                send(&mut out, &[np(create_seek(cols, rows, frac0))]);
            }
            // The wipe took the progress panel with it; the recreate
            // above comes back hidden, so let the loop re-show it if
            // there is still work in flight.
            progress_visible = false;
            dirty_media = true;
            dirty_status = true;
            dirty_seek = is_video;
        }

        // How long to block waiting for input. With no continuous playback
        // the loop is event-driven; 50 ms keeps a lone ESC responsive. While
        // a decode is pending, wake more often (and on the decode's pipe) to
        // repaint the progress panel — a bar rect plus a short label per
        // tick — and apply the frame the moment it lands. While an upload
        // is streaming, barely block at all: each iteration pushes one
        // chunk, and the 1 ms poll keeps input (a superseding seek)
        // flowing between chunks.
        let tick = if upload.is_some() {
            1
        } else if pending.is_some() || worker.is_busy() {
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
                        // Horizontal panning moves to `h`/`l`. The
                        // decode goes to the worker, so holding an arrow
                        // down keeps stepping instead of stalling on
                        // each file — every press supersedes the decode
                        // still in flight.
                        step_dir = if dir == Dir::Right { 1 } else { -1 };
                        step_start = pl.index();
                        step_tries = 0;
                        let path = pl.step(step_dir);
                        loading = Some(file_label(&path));
                        worker.load(path, enc_params);
                        dirty_status = true;
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
        // EAGAIN it stays pending and the progress panel keeps reporting.
        if pending.is_some() {
            match pending.as_mut().unwrap().poll() {
                DecodeState::Pending => {}
                DecodeState::Done(rgba) => {
                    // The pixels are here; the encode that turns them
                    // into a payload goes to the worker, so the loop
                    // stays live through it too.
                    let m = meta.as_ref().unwrap();
                    pending = None;
                    worker.encode(Frame::new(m.width, m.height, rgba), enc_params);
                    dirty_status = true;
                }
                DecodeState::Failed => {
                    pending = None;
                    // A seek that lands nowhere just leaves the current
                    // frame up. The *first* decode failing is different:
                    // there is nothing on screen and never will be, so
                    // say so instead of sitting on empty chrome.
                    if !have_picture {
                        fatal = Some(format!(
                            "could not decode the first frame of {path_str} \
                             (ffmpeg produced none)"
                        ));
                        quit = true;
                    }
                }
            }
        }

        // A decoded / encoded picture from the worker. A still also
        // brings its own dimensions and name; a video frame only had to
        // be encoded, and keeps the geometry the stream already set.
        match worker.poll() {
            Some(work::Done::Ready(r)) => {
                if let Some(path) = &r.path {
                    name = file_label(path);
                    // Every still may be a different size, so the
                    // viewport is rebuilt around it — keeping the zoom
                    // the user had chosen, except for the very first,
                    // which fits.
                    let zoom = have_picture.then_some(vp.zoom);
                    src_w = r.frame.w;
                    src_h = r.frame.h;
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
                    if let Some(z) = zoom {
                        vp.zoom = z;
                    }
                }
                // The new picture streams into the other half of the
                // ping-pong pair, so the one on screen stays drawable
                // until the last chunk lands and the pump swaps them in
                // a single envelope — no gap where the element points at
                // a texture that isn't there yet.
                let up = into_upload(&cur_id, &r.frame, r.encoding, r.payload);
                queue_upload(&mut out, &mut upload, up);
                source_frame = r.frame;
                have_picture = true;
                loading = None;
                dirty_status = true;
            }
            Some(work::Done::Failed { path, error, .. }) => {
                // A directory listing is no promise that every file is a
                // usable picture. If the user was walking one, keep
                // going in the same direction; give up (cursor restored)
                // once the whole directory has refused.
                let walking = match (path.is_some(), pl.as_mut()) {
                    (true, Some(pl)) if step_dir != 0 && step_tries + 1 < pl.len() => {
                        step_tries += 1;
                        let next = pl.step(step_dir);
                        loading = Some(file_label(&next));
                        worker.load(next, enc_params);
                        true
                    }
                    (true, Some(pl)) if step_dir != 0 => {
                        pl.set_index(step_start);
                        step_dir = 0;
                        false
                    }
                    _ => false,
                };
                if !walking {
                    loading = None;
                    // Nothing has ever reached the screen — the file
                    // vplay was opened on, or a whole directory of
                    // duds — so there is nothing to fall back to.
                    // Report it and leave.
                    if !have_picture {
                        fatal = Some(error);
                        quit = true;
                    }
                }
                dirty_status = true;
            }
            None => {}
        }

        // Coalesced redraws. Pan/zoom is pure host-side: re-point the
        // element's source_rect at the already-uploaded full-resolution
        // texture — no pixels travel.
        if dirty_media && !cur_id.is_empty() {
            let l = vp.layout();
            let cmd = if created_img {
                update_image_el(l.target, &cur_id, Some(l.source))
            } else {
                // After a resize the element is gone but the texture
                // survives in the session image table — recreate the
                // element over it, no re-upload. This is the reason
                // textures are pinned: releasing the deleted element's
                // reference would collect an `Auto` texture and this
                // would name a dead id (see `RETENTION`).
                created_img = true;
                create_image_el(l.target, &cur_id, Some(l.source))
            };
            send(&mut out, &[np(cmd)]);
            dirty_status = true;
        }
        // Cleared even when there was no texture to point at yet: the
        // first upload's completion creates the element from the live
        // viewport anyway.
        dirty_media = false;

        // Stream the next chunk of an in-flight upload. The final
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
                // A/B flip would collide with it. Now that textures are
                // pinned this drop *is* the reclamation — under `Auto`
                // the retarget above already collected the outgoing slot
                // and this was a silent no-op.
                if !cur_id.is_empty() {
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
                    if have_picture {
                        format!("{name}  {src_w}x{src_h}")
                    } else {
                        name.clone()
                    },
                    if have_picture {
                        format!("{}%{pos}", vp.zoom_percent())
                    } else {
                        String::new()
                    },
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

        // The progress panel — which of the two phases the picture is in
        // and how far through it. A pending decode outranks a streaming
        // upload: a seek made mid-transfer supersedes that transfer, so
        // the decode is the work the user is actually waiting on.
        //
        // Revealed only once the wait has lasted BUSY_DELAY, counting
        // both phases, so quick seeks and steps don't flash it; hidden
        // the moment the picture catches up. Repaints are rate-limited
        // to one per PROGRESS_FRAME — an upload pumps a chunk every
        // millisecond, which is far more often than a bar needs redrawing.
        let busy = if let Some(d) = pending.as_ref() {
            Some(Busy::Decoding {
                what: None,
                frac: d.progress(),
            })
        } else {
            match worker.phase() {
                work::Phase::Decoding => Some(Busy::Decoding {
                    what: loading.as_deref(),
                    frac: None,
                }),
                work::Phase::Encoding => Some(Busy::Encoding),
                work::Phase::Idle => upload.as_ref().map(|u| Busy::Sending {
                    done: u.offset,
                    total: u.payload.len(),
                }),
            }
        };
        match (busy.is_some(), busy_since) {
            (true, None) => busy_since = Some(Instant::now()),
            (false, Some(_)) => busy_since = None,
            _ => {}
        }
        match busy.filter(|_| busy_since.is_some_and(|t| t.elapsed() >= BUSY_DELAY)) {
            Some(b) => {
                let mut cmds: Vec<(Command, u32)> = Vec::new();
                if !progress_visible {
                    progress_visible = true;
                    cmds.push(np(progress_show(true)));
                    // Paint the panel in the same envelope that reveals
                    // it, rather than showing a blank bar until the rate
                    // limit next allows a repaint.
                    last_progress = None;
                }
                if last_progress.is_none_or(|t| t.elapsed() >= PROGRESS_FRAME) {
                    last_progress = Some(Instant::now());
                    let (track, _) = progress_rects(cols, media_rows);
                    cmds.push(np(progress_fill(
                        track,
                        b.frac(),
                        busy_t0.elapsed().as_secs_f32(),
                    )));
                    cmds.push(np(progress_label(b.label())));
                }
                send(&mut out, &cmds);
            }
            None => {
                if progress_visible {
                    progress_visible = false;
                    send(&mut out, &[np(progress_show(false))]);
                }
            }
        }
    }

    // The opened file turned out to be undecodable, or too big for the
    // host. Raised here rather than at the point of failure so `TermExit`
    // has put the main screen back before anyone prints to it.
    if let Some(e) = fatal {
        bail!(e);
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
mod progress_tests {
    use super::*;

    fn track() -> Rect {
        progress_rects(80, 24).0
    }

    fn fill_rect(cmd: &Command) -> Rect {
        match cmd {
            Command::UpdateCommand(b) => match &b.command {
                DrawCmd::FillRectangles { rects, .. } => rects[0],
                _ => panic!("progress fill is not a rectangle"),
            },
            _ => panic!("progress fill is not an UpdateCommand"),
        }
    }

    #[test]
    fn sending_reports_bytes_and_percent() {
        let b = Busy::Sending {
            done: 5 * 1024 * 1024,
            total: 20 * 1024 * 1024,
        };
        assert_eq!(b.frac(), Some(0.25));
        assert_eq!(b.label(), "sending  25%  5.0/20.0 MB");
    }

    /// A zero-length payload is complete, not a division by zero.
    #[test]
    fn empty_payload_reads_as_done() {
        let b = Busy::Sending { done: 0, total: 0 };
        assert_eq!(b.frac(), Some(1.0));
    }

    /// Before ffmpeg writes anything there is no fraction to show, so
    /// the bar sweeps rather than sitting at 0%.
    #[test]
    fn decode_without_bytes_is_indeterminate() {
        let waiting = Busy::Decoding {
            what: None,
            frac: None,
        };
        assert_eq!(waiting.frac(), None);
        assert_eq!(waiting.label(), "decoding");
        assert_eq!(
            Busy::Decoding {
                what: None,
                frac: Some(0.5)
            }
            .label(),
            "decoding  50%"
        );
    }

    /// A still's decode can only be named, never measured — so the
    /// label carries the file and the bar sweeps.
    #[test]
    fn still_decode_names_the_file() {
        let b = Busy::Decoding {
            what: Some("photo.jpg"),
            frac: None,
        };
        assert_eq!(b.frac(), None);
        assert_eq!(b.label(), "decoding photo.jpg");
    }

    /// The encode is opaque too, and says so.
    #[test]
    fn encoding_is_indeterminate() {
        assert_eq!(Busy::Encoding.frac(), None);
        assert_eq!(Busy::Encoding.label(), "encoding");
    }

    #[test]
    fn determinate_fill_grows_from_the_left() {
        let t = track();
        let empty = fill_rect(&progress_fill(t, Some(0.0), 0.0));
        let half = fill_rect(&progress_fill(t, Some(0.5), 0.0));
        let full = fill_rect(&progress_fill(t, Some(1.0), 0.0));
        assert_eq!((empty.x, empty.w), (t.x, 0.0));
        assert_eq!((half.x, half.w), (t.x, t.w * 0.5));
        assert_eq!((full.x, full.w), (t.x, t.w));
    }

    /// Out-of-range fractions are clamped, so a bar can't spill past its
    /// track if a byte count ever overshoots.
    #[test]
    fn determinate_fill_clamps() {
        let t = track();
        assert_eq!(fill_rect(&progress_fill(t, Some(2.0), 0.0)).w, t.w);
        assert_eq!(fill_rect(&progress_fill(t, Some(-1.0), 0.0)).w, 0.0);
    }

    /// The sweeping segment stays inside the track at every phase of its
    /// out-and-back travel.
    #[test]
    fn sweep_stays_inside_the_track() {
        let t = track();
        for i in 0..64 {
            let r = fill_rect(&progress_fill(t, None, i as f32 * 0.05));
            assert!(r.x >= t.x - 0.001, "left edge escaped at step {i}");
            assert!(r.x + r.w <= t.x + t.w + 0.001, "right edge escaped at step {i}");
            assert!(r.w > 0.0);
        }
    }

    /// The panel is centred in the media area and its label sits above
    /// the bar, not on top of it.
    #[test]
    fn panel_is_centred_with_the_label_above() {
        let (track, label) = progress_rects(80, 24);
        assert_eq!(track.x + track.w / 2.0, 40.0);
        assert_eq!(label.x, 40.0);
        assert!(label.y < track.y);
    }

    /// A terminal narrower than the preferred bar width still gets a bar
    /// that fits inside it.
    #[test]
    fn narrow_terminal_keeps_the_bar_on_screen() {
        let (track, _) = progress_rects(10, 6);
        assert!(track.x >= 0.0);
        assert!(track.x + track.w <= 10.0);
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
