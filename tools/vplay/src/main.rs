//! vplay — interactive image and video viewer for VGE-aware terminals.
//!
//! Draws media via a single VGE `DrawImage`: in image mode the texture is
//! uploaded once and pan/zoom is just a `source_rect` update (no
//! re-upload); in video mode each frame is cropped+resized to the visible
//! footprint and swapped in. A status bar and (for video) a draggable
//! seek bar overlay the media. Video frames come from an external ffmpeg.

mod image_src;
mod input;
mod video;
mod viewport;

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::Parser;
use image::{RgbaImage, imageops, imageops::FilterType};
use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style, UpdateCommandBody,
    UpdateTextBody, UpdateTextRange, UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::is_ssh_session;
use vge_render::probe::run_probe;
use vge_render::tty::{
    RawTty, drain_stale_stdin, install_sigwinch, poll_stdin_until, read_stdin, take_sigwinch,
    winsize,
};
use vge_render::upload::{choose_encoding, encode_payload};

use image_src::{Frame, load_image};
use input::{Dir, Event, InputParser};
use video::{Decoder, VideoMeta, grab_one_frame, probe_video};
use viewport::Viewport;

const EL_BG: &str = "vplay-bg";
const EL_IMG: &str = "vplay-img";
const EL_STATUS: &str = "vplay-status";
const EL_SEEK: &str = "vplay-seek";
const IMG_ID: &str = "vplay-tex";
const IMG_ID_A: &str = "vplay-fa";
const IMG_ID_B: &str = "vplay-fb";

const ACCENT: (f32, f32, f32) = (0.337, 0.475, 0.624); // #56799f

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
    /// Cap playback frame rate (video).
    #[arg(long, default_value_t = 30.0)]
    max_fps: f64,
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
    })
}

fn update_image_el(target: Rect, id: &str, source: Option<Rect>) -> Command {
    Command::UpdateCommand(UpdateCommandBody {
        id: EL_IMG.into(),
        index: 0,
        command: image_draw(target, id, source),
    })
}

fn upload_cmd(
    id: &str,
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    supported: u8,
    ssh: bool,
) -> Result<Command> {
    let enc = choose_encoding(supported, ssh, 80.0);
    let (encoding, payload) = encode_payload(rgba, w, h, enc)?;
    Ok(Command::UploadImage(UploadImageBody {
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

/// Crop the visible source window of `full` and resize it to the display
/// footprint, upload it under the next ping-pong id, and point EL_IMG at
/// it (source_rect = None, since the texture is already the exact crop).
#[allow(clippy::too_many_arguments)]
fn render_video_frame<W: Write>(
    out: &mut W,
    vp: &Viewport,
    full: &Frame,
    cur_id: &mut String,
    created: &mut bool,
    supported: u8,
    ssh: bool,
    cell_pw: f32,
    cell_ph: f32,
) -> Result<()> {
    let l = vp.layout();
    let sx = (l.source.x.max(0.0) as u32).min(full.w.saturating_sub(1));
    let sy = (l.source.y.max(0.0) as u32).min(full.h.saturating_sub(1));
    let sw = (l.source.w.round() as u32).clamp(1, full.w - sx);
    let sh = (l.source.h.round() as u32).clamp(1, full.h - sy);
    let tw = (l.target.w * cell_pw).round().max(1.0) as u32;
    let th = (l.target.h * cell_ph).round().max(1.0) as u32;

    // Manual crop into a tight buffer (avoids cloning the whole frame).
    let mut crop = vec![0u8; (sw as usize) * (sh as usize) * 4];
    let row_bytes = (sw as usize) * 4;
    for row in 0..sh {
        let src_off = (((sy + row) as usize) * (full.w as usize) + sx as usize) * 4;
        let dst_off = (row as usize) * row_bytes;
        crop[dst_off..dst_off + row_bytes]
            .copy_from_slice(&full.rgba[src_off..src_off + row_bytes]);
    }
    let crop_img = RgbaImage::from_raw(sw, sh, crop)
        .ok_or_else(|| anyhow::anyhow!("crop buffer size mismatch"))?;
    let resized = imageops::resize(&crop_img, tw, th, FilterType::Triangle);

    let next_id = if *cur_id == IMG_ID_A {
        IMG_ID_B
    } else {
        IMG_ID_A
    };
    let mut cmds = vec![np(upload_cmd(
        next_id,
        tw,
        th,
        resized.into_raw(),
        supported,
        ssh,
    )?)];
    if *created {
        cmds.push(np(update_image_el(l.target, next_id, None)));
        cmds.push(np(Command::DropImage { id: cur_id.clone() }));
    } else {
        cmds.push(np(create_image_el(l.target, next_id, None)));
        *created = true;
    }
    send(out, &cmds);
    *cur_id = next_id.to_string();
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
    let name = cli
        .file
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_str.clone());

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
    let ssh = is_ssh_session();

    let ws = winsize().ok_or_else(|| anyhow::anyhow!("could not query terminal size"))?;
    let mut cols = ws.ws_col.max(1);
    let mut rows = ws.ws_row.max(1);
    let min_rows = if is_video { 3 } else { 2 };
    if rows < min_rows {
        bail!("terminal too short ({rows} rows); need at least {min_rows}");
    }
    let mut media_rows = rows - if is_video { 2 } else { 1 };

    // Source dimensions for the viewport.
    let (src_w, src_h) = match (&meta, &image_frame) {
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
        send(&mut out, &[np(create_seek(cols, rows, frac0))]);
    }

    // --- per-mode playback state ---
    let mut created_img = false;
    let mut cur_id = IMG_ID.to_string();
    let mut source_frame: Frame;

    // Video state.
    let fps = meta.as_ref().map(|m| m.fps).unwrap_or(30.0);
    let interval = Duration::from_secs_f64(1.0 / cli.max_fps.min(fps).max(1.0));
    let mut decoder: Option<Decoder> = None;
    let mut playing = false;
    let mut cur_pts = 0.0f64;
    let mut cur_index = 0u64;
    let mut next_due = Instant::now();

    if is_video {
        let m = meta.as_ref().unwrap();
        // Grab and show the first frame, then start continuous playback.
        let first = grab_one_frame(&path_str, m.width, m.height, 0.0)?
            .ok_or_else(|| anyhow::anyhow!("could not decode the first video frame"))?;
        source_frame = Frame::new(m.width, m.height, first);
        render_video_frame(
            &mut out,
            &vp,
            &source_frame,
            &mut cur_id,
            &mut created_img,
            supported,
            ssh,
            cell_pw,
            cell_ph,
        )?;
        decoder = Some(Decoder::start(&path_str, m.width, m.height, m.fps, 0.0)?);
        playing = true;
        next_due = Instant::now() + interval;
    } else {
        let f = image_frame.unwrap();
        source_frame = f.clone();
        // Upload the native image once; pan/zoom is source_rect-only.
        send(
            &mut out,
            &[np(upload_cmd(IMG_ID, f.w, f.h, f.rgba, supported, ssh)?)],
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

    let total_frames = meta.as_ref().and_then(|m| m.nb_frames);
    let duration = meta.as_ref().map(|m| m.duration()).unwrap_or(0.0);

    while !quit {
        if take_sigwinch(winch)
            && let Some(ws) = winsize()
        {
            cols = ws.ws_col.max(1);
            rows = ws.ws_row.max(min_rows);
            media_rows = rows - if is_video { 2 } else { 1 };
            vp.set_viewport(0.0, 0.0, cols as f32, media_rows as f32);
            send(&mut out, &[np(Command::ClearAll)]);
            created_img = false;
            send(
                &mut out,
                &[
                    np(create_bg(cols, media_rows)),
                    np(create_status(cols, rows)),
                ],
            );
            if is_video {
                send(&mut out, &[np(create_seek(cols, rows, frac0))]);
            }
            dirty_media = true;
            dirty_status = true;
            dirty_seek = is_video;
        }

        // How long to block waiting for input.
        let wait = if is_video && playing {
            let now = Instant::now();
            if next_due > now {
                (next_due - now).min(Duration::from_millis(50))
            } else {
                Duration::from_millis(0)
            }
        } else {
            Duration::from_millis(50)
        };
        let deadline = Instant::now() + wait;

        let events = if poll_stdin_until(deadline).unwrap_or(false) {
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
                Event::Arrow(dir) => {
                    const PAN: f32 = 3.0;
                    if is_video && matches!(dir, Dir::Left | Dir::Right) {
                        let dt = if dir == Dir::Right { 5.0 } else { -5.0 };
                        seek_to(
                            cur_pts + dt,
                            &mut out,
                            &vp,
                            meta.as_ref().unwrap(),
                            &path_str,
                            &mut playing,
                            &mut decoder,
                            &mut cur_pts,
                            &mut cur_index,
                            &mut next_due,
                            &mut source_frame,
                            &mut cur_id,
                            &mut created_img,
                            supported,
                            ssh,
                            cell_pw,
                            cell_ph,
                        )?;
                        dirty_status = true;
                        dirty_seek = true;
                    } else {
                        match dir {
                            Dir::Up => vp.pan_cells(0.0, PAN),
                            Dir::Down => vp.pan_cells(0.0, -PAN),
                            Dir::Left => vp.pan_cells(PAN, 0.0),
                            Dir::Right => vp.pan_cells(-PAN, 0.0),
                        }
                        dirty_media = true;
                        dirty_status = true;
                    }
                }
                Event::PlayPause => {
                    if is_video {
                        if playing {
                            playing = false;
                            decoder = None;
                        } else {
                            let m = meta.as_ref().unwrap();
                            decoder = Some(Decoder::start(
                                &path_str, m.width, m.height, m.fps, cur_pts,
                            )?);
                            playing = true;
                            next_due = Instant::now();
                        }
                        dirty_status = true;
                    }
                }
                Event::StepNext | Event::StepPrev => {
                    if is_video {
                        playing = false;
                        decoder = None;
                        let step = 1.0 / fps;
                        let t = if ev == Event::StepNext {
                            cur_pts + step
                        } else {
                            cur_pts - step
                        };
                        seek_to(
                            t,
                            &mut out,
                            &vp,
                            meta.as_ref().unwrap(),
                            &path_str,
                            &mut playing,
                            &mut decoder,
                            &mut cur_pts,
                            &mut cur_index,
                            &mut next_due,
                            &mut source_frame,
                            &mut cur_id,
                            &mut created_img,
                            supported,
                            ssh,
                            cell_pw,
                            cell_ph,
                        )?;
                        dirty_status = true;
                        dirty_seek = true;
                    }
                }
                Event::MouseDown { col, row } => {
                    cursor = Some((col as f32 + 0.5, row as f32 + 0.5));
                    if is_video && row == rows - 2 {
                        let was = playing;
                        if playing {
                            playing = false;
                            decoder = None;
                        }
                        drag = Drag::Seek { was_playing: was };
                        let frac =
                            ((col as f32 - 1.0) / (cols as f32 - 2.0).max(1.0)).clamp(0.0, 1.0);
                        seek_to(
                            frac as f64 * duration,
                            &mut out,
                            &vp,
                            meta.as_ref().unwrap(),
                            &path_str,
                            &mut playing,
                            &mut decoder,
                            &mut cur_pts,
                            &mut cur_index,
                            &mut next_due,
                            &mut source_frame,
                            &mut cur_id,
                            &mut created_img,
                            supported,
                            ssh,
                            cell_pw,
                            cell_ph,
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
                    if let Drag::Seek { was_playing } = drag
                        && was_playing
                        && is_video
                    {
                        let m = meta.as_ref().unwrap();
                        decoder = Some(Decoder::start(
                            &path_str, m.width, m.height, m.fps, cur_pts,
                        )?);
                        playing = true;
                        next_due = Instant::now();
                        dirty_status = true;
                    }
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
                        Drag::Seek { .. } if pressed && is_video => {
                            let frac =
                                ((col as f32 - 1.0) / (cols as f32 - 2.0).max(1.0)).clamp(0.0, 1.0);
                            seek_to(
                                frac as f64 * duration,
                                &mut out,
                                &vp,
                                meta.as_ref().unwrap(),
                                &path_str,
                                &mut playing,
                                &mut decoder,
                                &mut cur_pts,
                                &mut cur_index,
                                &mut next_due,
                                &mut source_frame,
                                &mut cur_id,
                                &mut created_img,
                                supported,
                                ssh,
                                cell_pw,
                                cell_ph,
                            )?;
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

        // Advance continuous video playback.
        if is_video && playing && Instant::now() >= next_due {
            let frame = decoder.as_ref().and_then(|d| d.try_recv_latest());
            if let Some(fr) = frame {
                cur_index = fr.index;
                cur_pts = cur_index as f64 / fps;
                let m = meta.as_ref().unwrap();
                source_frame = Frame::new(m.width, m.height, fr.rgba);
                render_video_frame(
                    &mut out,
                    &vp,
                    &source_frame,
                    &mut cur_id,
                    &mut created_img,
                    supported,
                    ssh,
                    cell_pw,
                    cell_ph,
                )?;
                next_due += interval;
                let now = Instant::now();
                if next_due < now {
                    next_due = now + interval;
                }
                dirty_status = true;
                dirty_seek = true;
                dirty_media = false; // just drew the frame at current layout
            } else if decoder.as_mut().map(|d| d.finished()).unwrap_or(false) {
                playing = false;
                dirty_status = true;
            } else {
                next_due = Instant::now() + Duration::from_millis(2);
            }
        }

        // Coalesced redraws.
        if dirty_media {
            if is_video {
                render_video_frame(
                    &mut out,
                    &vp,
                    &source_frame,
                    &mut cur_id,
                    &mut created_img,
                    supported,
                    ssh,
                    cell_pw,
                    cell_ph,
                )?;
            } else {
                render_image_mode(&mut out, &vp, &mut created_img);
            }
            dirty_media = false;
            dirty_status = true;
        }

        if dirty_status {
            let cur = cursor
                .and_then(|(c, r)| vp.cursor_pixel(c, r))
                .map(|(x, y)| (x, y, source_frame.pixel(x, y)));
            let (left, center, right) = if is_video {
                let state = if playing { "▶" } else { "⏸" };
                let totals = total_frames
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "?".into());
                (
                    format!("{name}  {src_w}x{src_h}  {fps:.2}fps"),
                    format!(
                        "{state} {}%  f {}/{}  {}",
                        vp.zoom_percent(),
                        cur_index,
                        totals,
                        fmt_pts(cur_pts)
                    ),
                    cursor_readout(&cur),
                )
            } else {
                (
                    format!("{name}  {src_w}x{src_h}"),
                    format!("{}%", vp.zoom_percent()),
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
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum Drag {
    None,
    Pan { last_col: f32, last_row: f32 },
    Seek { was_playing: bool },
}

/// Seek to `time` seconds. While playing, restart the continuous decoder
/// there; while paused, grab and display a single frame.
#[allow(clippy::too_many_arguments)]
fn seek_to<W: Write>(
    time: f64,
    out: &mut W,
    vp: &Viewport,
    meta: &VideoMeta,
    path: &str,
    playing: &mut bool,
    decoder: &mut Option<Decoder>,
    cur_pts: &mut f64,
    cur_index: &mut u64,
    next_due: &mut Instant,
    source_frame: &mut Frame,
    cur_id: &mut String,
    created: &mut bool,
    supported: u8,
    ssh: bool,
    cell_pw: f32,
    cell_ph: f32,
) -> Result<()> {
    let time = time.clamp(0.0, meta.duration());
    *cur_pts = time;
    *cur_index = (time * meta.fps).round() as u64;
    if *playing {
        *decoder = Some(Decoder::start(
            path,
            meta.width,
            meta.height,
            meta.fps,
            time,
        )?);
        *next_due = Instant::now();
    } else if let Some(rgba) = grab_one_frame(path, meta.width, meta.height, time)? {
        *source_frame = Frame::new(meta.width, meta.height, rgba);
        render_video_frame(
            out,
            vp,
            source_frame,
            cur_id,
            created,
            supported,
            ssh,
            cell_pw,
            cell_ph,
        )?;
    }
    Ok(())
}

/// Restores the terminal (leaves alt screen, re-shows cursor, disables
/// mouse) and clears VGE state on drop.
struct TermExit;

impl Drop for TermExit {
    fn drop(&mut self) {
        let mut o = std::io::stdout();
        let env = build_envelope(&[(Command::ClearAll, REQ_ID_NO_RESPONSE)]);
        let _ = o.write_all(&env);
        let _ = o.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        let _ = o.flush();
    }
}
