//! Spinner — VGE element-transform demo (§9.11–§9.13).
//!
//! Creates one complex multi-command shape (spokes, rings, gradient
//! hub) and then animates it with a single ~40-byte `UpdateTransform`
//! envelope per frame, instead of re-sending the geometry. Run inside
//! veter. Q / Esc / Ctrl-C quits.

use std::f32::consts::TAU;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use vge_protocol::codec::{Point, Transform};
use vge_protocol::command::{Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_protocol::path::{PathNode, PathSegment};
use vge_render::probe::run_probe;
use vge_render::tty::{
    RawTty, drain_stale_stdin, install_sigwinch, poll_stdin_until, read_stdin, take_sigwinch,
    winsize,
};

const FRAME_DT: Duration = Duration::from_millis(16); // ~60 Hz
const REV_PERIOD: Duration = Duration::from_secs(4);
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Visual radius of the whole spinner, in cell-width units.
const R_OUTER: f32 = 8.5;

fn main() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("spinner must run with stdin and stdout connected to a terminal");
    }

    let _tty = RawTty::enable()?;
    drain_stale_stdin();

    // Probe for the cell pixel ratio — geometry is aspect-compensated
    // so the spinner is visually circular, and the transform math
    // (§9.13) wants the same ratio for rotate-about pivots.
    let probe = run_probe(PROBE_TIMEOUT)?
        .ok_or_else(|| anyhow!("VGE probe timed out — terminal does not appear to support VGE"))?;
    let cw = probe.cell_pixel_width as f32;
    let ch = probe.cell_pixel_height as f32;

    let _alt = AltScreen::enter()?;
    let sigwinch = install_sigwinch();

    let (mut cols, mut rows) = term_size();
    send(&[
        (
            Command::CreateElement(spinner_body(cols, rows, cw, ch)),
            REQ_ID_NO_RESPONSE,
        ),
        (
            Command::CreateElement(hint_body(cols, rows)),
            REQ_ID_NO_RESPONSE,
        ),
    ])?;

    let start = Instant::now();
    let mut buf = [0u8; 256];
    loop {
        let theta = TAU * (start.elapsed().as_secs_f32() / REV_PERIOD.as_secs_f32());
        // Geometry is centered on the element origin, so this is a pure
        // rotation matrix — no cell-size math on the hot path (§9.13).
        let t = Transform::rotate_about(theta, 0.0, 0.0, cw, ch);
        send(&[(
            Command::UpdateTransform {
                id: "spinner".into(),
                transform: t,
            },
            REQ_ID_NO_RESPONSE,
        )])?;

        if take_sigwinch(sigwinch) {
            let (c, r) = term_size();
            if (c, r) != (cols, rows) {
                (cols, rows) = (c, r);
                send(&[
                    (
                        Command::UpdateOrigin {
                            id: "spinner".into(),
                            origin: center(cols, rows),
                        },
                        REQ_ID_NO_RESPONSE,
                    ),
                    (
                        Command::UpdateOrigin {
                            id: "hint".into(),
                            origin: hint_origin(cols, rows),
                        },
                        REQ_ID_NO_RESPONSE,
                    ),
                ])?;
            }
        }

        let deadline = Instant::now() + FRAME_DT;
        while poll_stdin_until(deadline)? {
            let n = read_stdin(&mut buf)?;
            if n == 0 {
                break;
            }
            // Raw mode: look for q / Q / Esc / Ctrl-C anywhere in the
            // chunk (terminal responses can't appear here — every
            // command uses REQ_ID_NO_RESPONSE after the probe).
            if buf[..n]
                .iter()
                .any(|&b| b == b'q' || b == b'Q' || b == 0x1B || b == 0x03)
            {
                send(&[(Command::ClearAll, REQ_ID_NO_RESPONSE)])?;
                return Ok(());
            }
        }
    }
}

// --- Geometry ---
//
// All shape coordinates are element-local cell units centered on the
// origin. Visual circularity on an anisotropic grid: a point at visual
// radius `r` (in cell-width units) and angle `a` sits at
// `(r·cos a, r·sin a · cw/ch)` — x in cell widths, y compensated into
// cell heights.

fn spinner_body(cols: u16, rows: u16, cw: f32, ch: f32) -> CreateElementBody {
    let vp = |r: f32, a: f32| Point {
        x: r * a.cos(),
        y: r * a.sin() * cw / ch,
    };

    let palette: [Color; 8] = [
        rgb(0xE0_5A_4E), // red
        rgb(0xE8_9B_3C), // orange
        rgb(0xE6_D4_4E), // yellow
        rgb(0x7C_C0_5A), // green
        rgb(0x4E_C0_A8), // teal
        rgb(0x4E_96_E0), // blue
        rgb(0x8A_6E_E0), // violet
        rgb(0xD0_5A_B4), // magenta
    ];

    let mut commands = Vec::new();

    // Outer ring: 48-gon line loop.
    let ring = |r: f32| -> Vec<Point> {
        (0..48)
            .map(|i| vp(r, TAU * i as f32 / 48.0))
            .collect()
    };
    commands.push(DrawCmd::DrawLineLoop {
        stroke: Style::Flat(rgb(0x6A_70_80)),
        line_width: 0.12,
        points: ring(R_OUTER),
    });
    commands.push(DrawCmd::DrawLineLoop {
        stroke: Style::Flat(rgb(0x3A_40_50)),
        line_width: 0.06,
        points: ring(R_OUTER - 0.7),
    });

    // Eight tapered spokes: narrow at the rim, wide at the hub.
    for (i, color) in palette.iter().enumerate() {
        let a = TAU * i as f32 / 8.0;
        let half_inner = 0.16; // radians, half-width at the hub end
        let half_outer = 0.045;
        commands.push(DrawCmd::FillPolygon {
            fill: Style::Flat(*color),
            points: vec![
                vp(2.2, a - half_inner),
                vp(7.0, a - half_outer),
                vp(7.0, a + half_outer),
                vp(2.2, a + half_inner),
            ],
        });
    }

    // Accent dots at the rim, between spokes — one FillPath with eight
    // circular subpaths.
    let dot_r = 0.32;
    let dot_segments: Vec<PathSegment> = (0..8)
        .map(|i| {
            let a = TAU * (i as f32 + 0.5) / 8.0;
            let c = vp(7.6, a);
            circle_segment(c, dot_r, cw, ch)
        })
        .collect();
    commands.push(DrawCmd::FillPath {
        fill: Style::Flat(rgb(0xC8_CC_D8)),
        segments: dot_segments,
    });

    // Hub: radial-gradient disc.
    commands.push(DrawCmd::FillPath {
        fill: Style::RadialGradient {
            center: Point { x: -0.4, y: -0.4 * cw / ch },
            outer: vp(1.8, 0.0),
            c_inner: rgb(0xF2_F4_F8),
            c_outer: rgb(0x80_88_98),
        },
        segments: vec![circle_segment(Point { x: 0.0, y: 0.0 }, 1.8, cw, ch)],
    });

    CreateElementBody {
        id: "spinner".into(),
        commands,
        origin: center(cols, rows),
        is_visible: true,
        draw_order: 0,
        parent: None,
        size: None,
        transform: Some(Transform::IDENTITY),
    }
}

/// A visually-circular subpath of radius `r` (cell-width units) around
/// `c`, drawn as two aspect-compensated half arcs (CCW, like breakout's
/// ball, so femtovg's tessellator fills it cleanly).
fn circle_segment(c: Point, r: f32, cw: f32, ch: f32) -> PathSegment {
    let ry = r * cw / ch;
    let arc = |dst: Point| PathNode::ArcEllipseTo {
        large: false,
        sweep: false,
        rx: r,
        ry,
        rotation: 0.0,
        dst,
    };
    PathSegment {
        start: Point { x: c.x - r, y: c.y },
        nodes: vec![
            arc(Point { x: c.x + r, y: c.y }),
            arc(Point { x: c.x - r, y: c.y }),
            PathNode::ClosePath,
        ],
    }
}

fn hint_body(cols: u16, rows: u16) -> CreateElementBody {
    CreateElementBody {
        id: "hint".into(),
        commands: vec![DrawCmd::DrawText {
            origin: Point { x: 0.0, y: 0.0 },
            align: Align::Center,
            fill: Style::Flat(rgb(0x9A_A0_B0)),
            font_style: FontStyle(0),
            text: "one UpdateTransform per frame — q quits".into(),
        }],
        origin: hint_origin(cols, rows),
        is_visible: true,
        draw_order: 1,
        parent: None,
        size: None,
        transform: None,
    }
}

fn center(cols: u16, rows: u16) -> Point {
    Point {
        x: cols as f32 / 2.0,
        y: rows as f32 / 2.0,
    }
}

fn hint_origin(cols: u16, rows: u16) -> Point {
    Point {
        x: cols as f32 / 2.0,
        y: rows as f32 - 1.5,
    }
}

fn term_size() -> (u16, u16) {
    winsize()
        .map(|ws| (ws.ws_col.max(1), ws.ws_row.max(1)))
        .unwrap_or((80, 24))
}

fn rgb(packed: u32) -> Color {
    Color {
        r: ((packed >> 16) & 0xFF) as f32 / 255.0,
        g: ((packed >> 8) & 0xFF) as f32 / 255.0,
        b: (packed & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

// --- Terminal I/O ---

fn send(cmds: &[(Command, u32)]) -> Result<()> {
    let env = build_envelope(cmds);
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&env)?;
    stdout.flush()?;
    Ok(())
}

/// RAII guard for the alternate screen + hidden cursor. Restores both
/// on drop so the user's shell comes back intact even on error paths.
struct AltScreen;

impl AltScreen {
    fn enter() -> Result<Self> {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H")?;
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for AltScreen {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(b"\x1b[?25h\x1b[?1049l");
        let _ = stdout.flush();
    }
}
