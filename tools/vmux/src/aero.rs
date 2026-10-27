//! Frutiger Aero chrome — glossy gel pills, frosted glass, an aqua sky
//! for a tab bar, and bubbles. A private style, not a product one: the
//! palette is its own and ignores the host theme on purpose.
//!
//! Selected by `VMUX_STYLE=aero` or `--style aero` (deliberately absent
//! from `--help`), and compiled only with the `aero` cargo feature.
//! Everything lives in this file; `main.rs` reaches it through hooks
//! that each carry `#[cfg(feature = "aero")]`, and each hook returns
//! before the stock code runs, so the stock path never changes shape.
//! Dropping it is: delete this file, the feature in `Cargo.toml`, and
//! every line `grep -n 'feature = "aero"' tools/vmux/src/main.rs` finds.

use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Result, bail};
use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Align, Color, CreateElementBody, DrawCmd, FontStyle, OriginAnchor, Style,
};
use vge_protocol::path::{PathNode, PathSegment};
use vge_ui::measure::{prefix_cells, text_cells};
use vge_ui::modal::{ModalIds, ScrollModal};
use vge_ui::picker::Picker;
use vge_ui::shape::rounded_rect_path;

use super::{
    CHROME_DRAW_ORDER, MODAL_DRAW_ORDER, MODAL_ELEMENT_ID, MODAL_IDS, Mode, PaneRect, RenameTarget,
    ScrollIndicator, State, TABBAR_CHEVRON_W, TABBAR_ELEMENT_ID, TabbarLayout, help_modal,
};

const UNSET: u8 = 0;
const STOCK: u8 = 1;
const AERO: u8 = 2;

static STYLE: AtomicU8 = AtomicU8::new(UNSET);

/// Pick the style by name — `--style`, which outranks `VMUX_STYLE`.
pub fn select(name: &str) -> Result<()> {
    STYLE.store(parse(name)?, Ordering::Relaxed);
    Ok(())
}

fn parse(name: &str) -> Result<u8> {
    match name {
        "aero" => Ok(AERO),
        "default" | "stock" => Ok(STOCK),
        other => bail!("unknown style: {other}"),
    }
}

/// Whether the Aero chrome is on. Without a `--style`, `VMUX_STYLE`
/// decides on first use; an unknown value there falls back to stock
/// rather than refusing to start a multiplexer.
pub fn enabled() -> bool {
    let mut s = STYLE.load(Ordering::Relaxed);
    if s == UNSET {
        s = std::env::var("VMUX_STYLE")
            .ok()
            .and_then(|v| parse(&v).ok())
            .unwrap_or(STOCK);
        STYLE.store(s, Ordering::Relaxed);
    }
    s == AERO
}

fn hex(rgb: u32, a: f32) -> Color {
    Color {
        r: ((rgb >> 16) & 0xff) as f32 / 255.0,
        g: ((rgb >> 8) & 0xff) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a,
    }
}

fn white(a: f32) -> Color {
    hex(0xffffff, a)
}

/// Pixel ↔ cell conversions for the current cell size.
#[derive(Clone, Copy)]
pub struct Px {
    pub w: f32,
    pub h: f32,
}

impl Px {
    fn x(self, px: f32) -> f32 {
        px / self.w.max(f32::EPSILON)
    }
    fn y(self, px: f32) -> f32 {
        px / self.h.max(f32::EPSILON)
    }
    /// A stroke width: VGE scales `line_width` by the mean cell side.
    fn line(self, px: f32) -> f32 {
        px / ((self.w + self.h) * 0.5).max(f32::EPSILON)
    }
}

fn vgrad(y0: f32, y1: f32, c0: Color, c1: Color) -> Style {
    Style::LinearGradient {
        p0: Point { x: 0.0, y: y0 },
        p1: Point { x: 0.0, y: y1 },
        c0,
        c1,
    }
}

fn hgrad(x0: f32, x1: f32, c0: Color, c1: Color) -> Style {
    Style::LinearGradient {
        p0: Point { x: x0, y: 0.0 },
        p1: Point { x: x1, y: 0.0 },
        c0,
        c1,
    }
}

fn radial(center: Point, outer: Point, c_inner: Color, c_outer: Color) -> Style {
    Style::RadialGradient {
        center,
        outer,
        c_inner,
        c_outer,
    }
}

/// A rounded rect whose corners are `r` *pixels* — circular on screen
/// whatever the cell aspect.
fn rr(px: Px, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Vec<PathSegment> {
    let rx = px.x(r).min((x1 - x0) / 2.0);
    let ry = px.y(r).min((y1 - y0) / 2.0);
    rounded_rect_path(x0, y0, x1, y1, rx, ry)
}

/// A capsule: ends fully round along its shorter side.
fn pill(px: Px, x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<PathSegment> {
    let r = ((x1 - x0) * px.w).min((y1 - y0) * px.h) / 2.0;
    rr(px, x0, y0, x1, y1, r)
}

fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<PathSegment> {
    let arc = |x: f32| PathNode::ArcEllipseTo {
        large: false,
        sweep: false,
        rx,
        ry,
        rotation: 0.0,
        dst: Point { x, y: cy },
    };
    vec![PathSegment {
        start: Point { x: cx - rx, y: cy },
        nodes: vec![arc(cx + rx), arc(cx - rx), PathNode::ClosePath],
    }]
}

/// Text with a one-pixel drop shadow down and to the right.
#[allow(clippy::too_many_arguments)]
fn text(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x: f32,
    y: f32,
    align: Align,
    s: &str,
    color: Color,
    bold: bool,
    shadow: Option<Color>,
) {
    let font_style = FontStyle(if bold { 0x01 } else { 0x00 });
    if let Some(sh) = shadow {
        cmds.push(DrawCmd::DrawText {
            origin: Point {
                x: x + px.x(1.0),
                y: y + px.y(1.0),
            },
            align,
            fill: Style::Flat(sh),
            font_style,
            font_scale: 1.0,
            text: s.to_string(),
        });
    }
    cmds.push(DrawCmd::DrawText {
        origin: Point { x, y },
        align,
        fill: Style::Flat(color),
        font_style,
        font_scale: 1.0,
        text: s.to_string(),
    });
}

/// A gel colourway: body gradient, the inner glow rising from the
/// bottom, and the rim.
struct Gel {
    top: u32,
    bottom: u32,
    glow: u32,
    rim: u32,
}

const GEL_GREEN: Gel = Gel {
    top: 0x8fdc4f,
    bottom: 0x2e9e1f,
    glow: 0xe8ff9a,
    rim: 0x1d6312,
};
const GEL_AMBER: Gel = Gel {
    top: 0xffc54a,
    bottom: 0xe57a00,
    glow: 0xfff3b0,
    rim: 0x8a4300,
};
const GEL_AQUA: Gel = Gel {
    top: 0x6fd3f7,
    bottom: 0x0a86d8,
    glow: 0x8ff6ff,
    rim: 0x0b4f8f,
};

/// The upper-half sheen every glossy thing carries: a white wash that
/// ends in a hard edge at the middle.
fn gloss(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, a: f32) {
    let gx0 = x0 + px.x(2.0);
    let gx1 = x1 - px.x(2.0);
    let gy0 = y0 + px.y(1.5);
    let gy1 = y0 + (y1 - y0) * 0.5;
    if gx1 <= gx0 {
        return;
    }
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(gy0, gy1, white(a), white(a * 0.05)),
        segments: pill(px, gx0, gy0, gx1, gy1),
    });
}

fn gel(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, g: &Gel) {
    let shape = pill(px, x0, y0, x1, y1);
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(g.top, 1.0), hex(g.bottom, 1.0)),
        stroke: Style::Flat(hex(g.rim, 0.9)),
        line_width: px.line(1.0),
        segments: shape.clone(),
    });
    let cx = (x0 + x1) / 2.0;
    cmds.push(DrawCmd::FillPath {
        fill: radial(
            Point { x: cx, y: y1 },
            Point {
                x: cx + (x1 - x0) * 0.35,
                y: y1,
            },
            hex(g.glow, 0.5),
            hex(g.glow, 0.0),
        ),
        segments: shape,
    });
    gloss(cmds, px, x0, y0, x1, y1, 0.42);
}

/// Frosted glass: a translucent white body with a bright rim.
fn frost(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, white(0.2), white(0.04)),
        stroke: Style::Flat(white(0.55)),
        line_width: px.line(1.0),
        segments: pill(px, x0, y0, x1, y1),
    });
    gloss(cmds, px, x0, y0, x1, y1, 0.28);
}

/// Smoked glass, for chrome that floats over shell text.
fn smoke(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0x2a6a9a, 0.7), hex(0x0b2a44, 0.85)),
        stroke: Style::Flat(white(0.5)),
        line_width: px.line(1.0),
        segments: pill(px, x0, y0, x1, y1),
    });
    gloss(cmds, px, x0, y0, x1, y1, 0.22);
}

/// A glossy blue orb, lit from the top left.
fn orb(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    let shape = pill(px, x0, y0, x1, y1);
    let (w, h) = (x1 - x0, y1 - y0);
    cmds.push(DrawCmd::OutlineFillPath {
        fill: radial(
            Point {
                x: x0 + w * 0.35,
                y: y0 + h * 0.3,
            },
            Point { x: x1, y: y1 },
            hex(0x8fd0ff, 1.0),
            hex(0x0b5fc4, 1.0),
        ),
        stroke: Style::Flat(hex(0x06326e, 0.9)),
        line_width: px.line(1.0),
        segments: shape,
    });
    gloss(cmds, px, x0, y0, x1, y1, 0.4);
}

fn bubble(cmds: &mut Vec<DrawCmd>, px: Px, cx: f32, cy: f32, r: f32) {
    let (rx, ry) = (px.x(r), px.y(r));
    let shape = ellipse(cx, cy, rx, ry);
    cmds.push(DrawCmd::FillPath {
        fill: radial(
            Point {
                x: cx - rx * 0.35,
                y: cy - ry * 0.35,
            },
            Point {
                x: cx + rx,
                y: cy + ry * 0.4,
            },
            white(0.05),
            hex(0x9fe6ff, 0.45),
        ),
        segments: shape.clone(),
    });
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(white(0.8)),
        line_width: px.line(1.0),
        segments: shape,
    });
    let hr = r * 0.3;
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(white(0.9)),
        segments: ellipse(cx - rx * 0.4, cy - ry * 0.4, px.x(hr), px.y(hr * 0.75)),
    });
}

/// A cheap stable hash, so a bubble keeps its place across redraws.
fn hash(mut v: u32) -> u32 {
    v ^= v >> 16;
    v = v.wrapping_mul(0x7feb_352d);
    v ^= v >> 15;
    v = v.wrapping_mul(0x846c_a68b);
    v ^ (v >> 16)
}

fn unit(v: u32) -> f32 {
    (v & 0xffff) as f32 / 65535.0
}

/// The tab bar element — the Aero stand-in for `build_tabbar_commands`,
/// drawn over the same [`TabbarLayout`] so clicks still land where the
/// tabs are painted.
pub fn tabbar(
    layout: &TabbarLayout,
    active: usize,
    host_w: f32,
    px: Px,
    activity: &[bool],
) -> CreateElementBody {
    CreateElementBody {
        id: TABBAR_ELEMENT_ID.to_string(),
        commands: tabbar_commands(layout, active, host_w, px, activity),
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: CHROME_DRAW_ORDER,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    }
}

fn tabbar_commands(
    layout: &TabbarLayout,
    active: usize,
    host_w: f32,
    px: Px,
    activity: &[bool],
) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();

    // The sky: a deep-to-light aqua bar with the glass sheen across its
    // upper half, a bright lip along the bottom and a glow spilling down
    // over the panes.
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, 1.0, hex(0x5ab8ef, 0.97), hex(0x1468b4, 0.97)),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w: host_w,
            h: 1.0,
        }],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, 0.5, white(0.28), white(0.04)),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w: host_w,
            h: 0.5,
        }],
    });
    let lip = px.y(1.0);
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(hex(0xc8f6ff, 0.95)),
        rects: vec![Rect {
            x: 0.0,
            y: 1.0 - lip,
            w: host_w,
            h: lip,
        }],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(1.0, 1.45, hex(0x3fb8ff, 0.55), hex(0x3fb8ff, 0.0)),
        rects: vec![Rect {
            x: 0.0,
            y: 1.0,
            w: host_w,
            h: 0.45,
        }],
    });

    // Bubbles in the free stretch after the last tab, some rising out
    // of the glow. Seeded by absolute column so they hold still while
    // tabs come and go.
    let free_x0 = layout
        .slots
        .last()
        .map(|s| s.x0 + s.num_w + s.name_w)
        .unwrap_or(layout.session_w())
        + 1.0;
    let free_x1 = host_w
        - if layout.has_right {
            TABBAR_CHEVRON_W
        } else {
            0.0
        }
        - 0.5;
    for col in 0..host_w as u32 {
        let h = hash(col.wrapping_add(0x5eed));
        if !h.is_multiple_of(3) {
            continue;
        }
        let r = 3.5 + unit(h >> 3) * 7.5;
        let cx = col as f32 + unit(h >> 7);
        let cy = 0.2 + unit(h >> 11) * 1.0;
        if cx - px.x(r) < free_x0 || cx + px.x(r) > free_x1 {
            continue;
        }
        bubble(&mut cmds, px, cx, cy, r);
    }

    let navy = hex(0x06264d, 0.75);
    if let Some((label, w)) = &layout.session_seg {
        let (x0, x1, y0, y1) = (0.1, *w - 0.1, 0.1, 0.92);
        cmds.push(DrawCmd::OutlineFillPath {
            fill: vgrad(y0, y1, white(1.0), hex(0x9fb3c8, 1.0)),
            stroke: Style::Flat(hex(0x5f7a96, 0.9)),
            line_width: px.line(1.0),
            segments: pill(px, x0, y0, x1, y1),
        });
        gloss(&mut cmds, px, x0, y0, x1, y1, 0.5);
        text(
            &mut cmds,
            px,
            0.0,
            0.0,
            Align::Left,
            &format!(" {label} "),
            hex(0x12345a, 1.0),
            true,
            Some(white(0.7)),
        );
    }

    if layout.has_left {
        text(
            &mut cmds,
            px,
            layout.session_w(),
            0.0,
            Align::Left,
            " ‹",
            white(1.0),
            true,
            Some(navy),
        );
    }

    for slot in &layout.slots {
        let i = slot.index;
        let x = slot.x0;
        let (x0, x1, y0, y1) = (x + 0.1, x + slot.num_w + slot.name_w - 0.1, 0.1, 0.92);
        let is_active = i == active;
        let has_activity = activity.get(i).copied().unwrap_or(false);
        let shadow = if is_active {
            gel(&mut cmds, px, x0, y0, x1, y1, &GEL_GREEN);
            hex(0x154a0c, 0.85)
        } else if has_activity {
            gel(&mut cmds, px, x0, y0, x1, y1, &GEL_AMBER);
            hex(0x6b3300, 0.85)
        } else {
            frost(&mut cmds, px, x0, y0, x1, y1);
            navy
        };

        // The number rides in an orb at the head of the pill; " n " is
        // centred over `num_w`, and so is the orb.
        orb(&mut cmds, px, x + 0.35, 0.18, x + slot.num_w - 0.35, 0.84);
        text(
            &mut cmds,
            px,
            x,
            0.0,
            Align::Left,
            &format!(" {} ", i + 1),
            white(1.0),
            true,
            Some(hex(0x06264d, 0.9)),
        );
        text(
            &mut cmds,
            px,
            x + slot.num_w,
            0.0,
            Align::Left,
            &format!(" {} ", slot.label),
            white(1.0),
            is_active,
            Some(shadow),
        );
    }

    if layout.has_right {
        text(
            &mut cmds,
            px,
            host_w - TABBAR_CHEVRON_W,
            0.0,
            Align::Left,
            "› ",
            white(1.0),
            true,
            Some(navy),
        );
    }
    cmds
}

/// A pane's chrome — the Aero stand-in for `build_chrome_commands`,
/// with the same title and scrollbar geometry.
pub fn pane_chrome(
    rect: PaneRect,
    title: &str,
    focused: bool,
    show_title: bool,
    px: Px,
    scroll: Option<ScrollIndicator>,
) -> Vec<DrawCmd> {
    let pw = rect.w as f32;
    let ph = rect.h as f32;
    let mut cmds = Vec::new();
    if show_title && !title.is_empty() {
        let text_x = pw - 1.0;
        let y0 = 0.25;
        let x0 = text_x - text_cells(title) - 0.5;
        pane_title(
            &mut cmds,
            px,
            x0,
            y0,
            text_x + 0.5,
            y0 + 1.0,
            text_x,
            title,
            focused,
        );
    }
    if let Some(s) = scroll {
        let portal_rows = ph.max(1.0);
        let total = portal_rows + s.history_depth as f32;
        let track_y0 = 1.25;
        let track_y1 = (ph - 0.5).max(track_y0 + 1.0);
        let track_h = (track_y1 - track_y0).max(1.0);
        let thumb_h = ((portal_rows / total).clamp(0.05, 1.0) * track_h).max(0.6);
        let available = (track_h - thumb_h).max(0.0);
        // 0 offset = bottom (live); offset = history_depth = top.
        let thumb_norm = if s.history_depth == 0 {
            1.0
        } else {
            (s.history_depth.saturating_sub(s.offset) as f32) / s.history_depth as f32
        };
        let thumb_y = track_y0 + thumb_norm * available;
        scroll_thumb(&mut cmds, px, pw - 1.30, thumb_y, 0.35, thumb_h);
    }
    cmds
}

/// The pane title: an aqua gel capsule on the focused pane, smoked
/// glass elsewhere. `x0..x1` × `y0..y1` is the thumb; the text is
/// right-aligned at `text_x`.
#[allow(clippy::too_many_arguments)]
fn pane_title(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    text_x: f32,
    title: &str,
    focused: bool,
) {
    if focused {
        gel(cmds, px, x0, y0, x1, y1, &GEL_AQUA);
    } else {
        smoke(cmds, px, x0, y0, x1, y1);
    }
    text(
        cmds,
        px,
        text_x,
        y0,
        Align::Right,
        title,
        white(1.0),
        focused,
        Some(hex(0x06264d, 0.9)),
    );
}

/// An aqua capsule scrollbar thumb, lit along its left edge.
fn scroll_thumb(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y: f32, w: f32, h: f32) {
    let (x0, x1, y1) = (x - w * 0.25, x + w * 1.25, y + h);
    cmds.push(DrawCmd::OutlineFillPath {
        fill: hgrad(x0, x1, hex(0xc4f6ff, 1.0), hex(0x0a7fd0, 1.0)),
        stroke: Style::Flat(hex(0x0b4f8f, 0.9)),
        line_width: px.line(1.0),
        segments: pill(px, x0, y, x1, y1),
    });
    let gx1 = x0 + (x1 - x0) * 0.5;
    cmds.push(DrawCmd::FillPath {
        fill: hgrad(x0, gx1, white(0.45), white(0.03)),
        segments: pill(px, x0 + px.x(1.5), y + px.y(2.0), gx1, y1 - px.y(2.0)),
    });
}

/// Pane separators: each stock rule becomes a soft aqua glow under a
/// bright core. Takes the stock commands, so the separator geometry
/// stays `build_separators_body`'s alone.
pub fn restyle_separators(cmds: &mut Vec<DrawCmd>) {
    if !enabled() {
        return;
    }
    let mut out = Vec::with_capacity(cmds.len() * 2);
    for cmd in cmds.drain(..) {
        match cmd {
            DrawCmd::DrawLines { lines, .. } => {
                out.push(DrawCmd::DrawLines {
                    stroke: Style::Flat(hex(0x3fb8ff, 0.25)),
                    line_width: 0.35,
                    lines: lines.clone(),
                });
                out.push(DrawCmd::DrawLines {
                    stroke: Style::Flat(hex(0xaef0ff, 0.95)),
                    line_width: 0.1,
                    lines,
                });
            }
            other => out.push(other),
        }
    }
    *cmds = out;
}

// ── Modals ──────────────────────────────────────────────────────────────
//
// Each mirrors its `vge_ui::modal` counterpart's geometry row for row,
// so a box sits where the stock one does and the help modal's
// `ScrollModal::scroll_commands` still moves the right children.

/// Corner radius of a window, in pixels.
const WIN_R: f32 = 9.0;
const INK: u32 = 0x0d2f52;

/// The modal for whatever the current mode wants up — the Aero stand-in
/// for `State::build_modal_elements`.
pub fn modal_elements(state: &State) -> Vec<CreateElementBody> {
    let px = Px {
        w: state.cell_pw,
        h: state.cell_ph,
    };
    let (w, h) = (state.host_w, state.host_h);
    match &state.mode {
        Mode::Rename { target, editor } => {
            let title = match target {
                RenameTarget::Pane(_) => "Rename pane",
                RenameTarget::Tab(_) => "Rename tab",
            };
            vec![prompt_element(
                w,
                h,
                title,
                &editor.buffer,
                px,
                Some(editor.cursor),
            )]
        }
        Mode::Help { offset, .. } => help_elements(help_modal(), MODAL_IDS, w, h, *offset, px),
        Mode::ConfirmQuit => vec![prompt_element(
            w,
            h,
            "Quit vmux?",
            "y = quit     n = cancel",
            px,
            None,
        )],
        Mode::Picker(picker) => vec![picker_element(w, h, &picker.ui, px)],
        Mode::Normal | Mode::Prefix | Mode::Resize { .. } => Vec::new(),
    }
}

fn modal_body(commands: Vec<DrawCmd>, x: f32, y: f32) -> CreateElementBody {
    CreateElementBody {
        id: MODAL_ELEMENT_ID.to_string(),
        commands,
        origin: Point { x, y },
        is_visible: true,
        draw_order: MODAL_DRAW_ORDER,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    }
}

/// A soft drop shadow: a few widening, fading layers down and right.
fn shadow(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
    for k in 1..=4 {
        let g = k as f32;
        let (dx, dy) = (px.x(g), px.y(g));
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(hex(0x001a33, 0.09)),
            segments: rr(
                px,
                -dx + px.x(3.0),
                -dy + px.y(4.0),
                w + dx + px.x(3.0),
                h + dy + px.y(4.0),
                WIN_R + g * 2.0,
            ),
        });
    }
}

/// The glass of a window's frame, `w × h`.
fn frame_glass(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(0.0, h, hex(0x4fb0ea, 0.94), hex(0x0f5aa0, 0.96)),
        segments: rr(px, 0.0, 0.0, w, h, WIN_R),
    });
}

/// The title bar's sheen, bubbles and centred title over row 0.
fn title_bar(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, title: &str) {
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(0.0, 0.55, white(0.3), white(0.03)),
        segments: rr(px, px.x(2.0), px.y(1.5), w - px.x(2.0), 0.55, WIN_R - 2.0),
    });
    for (cx, cy, r) in [(w - 2.2, 0.45, 6.0), (w - 3.4, 0.6, 3.5), (2.0, 0.55, 4.0)] {
        bubble(cmds, px, cx, cy, r);
    }
    text(
        cmds,
        px,
        w * 0.5,
        0.0,
        Align::Center,
        title,
        white(1.0),
        true,
        Some(hex(0x06264d, 0.9)),
    );
}

/// The pale content panel the rows read dark on.
fn panel(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xf6fcff, 0.97), hex(0xd6ebf8, 0.97)),
        stroke: Style::Flat(hex(0x0b4f8f, 0.6)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, 5.0),
    });
}

/// A slightly sunken white text field.
fn field(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xe4f1fa, 1.0), white(1.0)),
        stroke: Style::Flat(hex(0x7fb2d9, 1.0)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, 4.0),
    });
}

fn caret(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y0: f32, y1: f32) {
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(hex(0x0a86d8, 1.0)),
        rects: vec![Rect {
            x: x - px.x(1.0),
            y: y0,
            w: px.x(2.0),
            h: y1 - y0,
        }],
    });
}

/// A bright inner line just inside the rim, then the rim itself —
/// drawn last, over every fill.
fn rims(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(white(0.35)),
        line_width: px.line(1.0),
        segments: rr(
            px,
            px.x(1.0),
            px.y(1.0),
            w - px.x(1.0),
            h - px.y(1.0),
            WIN_R - 1.0,
        ),
    });
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(hex(0x06326e, 0.9)),
        line_width: px.line(1.0),
        segments: rr(px, 0.0, 0.0, w, h, WIN_R),
    });
}

/// Rename prompts and the quit confirm, after `prompt_element`: title
/// on row 0, the line on row 2 — in a text field when there is a caret.
fn prompt_element(
    host_w: u32,
    host_h: u32,
    title: &str,
    line: &str,
    px: Px,
    caret_at: Option<usize>,
) -> CreateElementBody {
    let line_w = text_cells(line);
    let box_w = (line_w.max(20.0) + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = 4.0_f32.min(host_h.saturating_sub(2) as f32);
    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let mut cmds = Vec::new();
    shadow(&mut cmds, px, box_w, box_h);
    frame_glass(&mut cmds, px, box_w, box_h);
    title_bar(&mut cmds, px, box_w, title);
    panel(&mut cmds, px, 0.3, 1.0, box_w - 0.3, box_h - 0.3);
    if caret_at.is_some() {
        field(&mut cmds, px, 1.0, 1.95, box_w - 1.0, 3.05);
    }
    text(
        &mut cmds,
        px,
        box_w * 0.5,
        2.0,
        Align::Center,
        line,
        hex(INK, 1.0),
        caret_at.is_none(),
        None,
    );
    if let Some(c) = caret_at {
        let x = box_w * 0.5 - line_w * 0.5 + prefix_cells(line, c);
        caret(&mut cmds, px, x, 2.15, 2.85);
    }
    rims(&mut cmds, px, box_w, box_h);
    modal_body(cmds, origin_x, origin_y)
}

/// The picker (command palette, tab and pane pickers), after
/// `picker_element`: title on row 0, the input on row 1, matches from
/// row 2.
fn picker_element<P>(host_w: u32, host_h: u32, picker: &Picker<P>, px: Px) -> CreateElementBody {
    const MAX_ROWS: usize = 14;
    let avail = (host_h.saturating_sub(4)).max(1) as usize;
    let visible_rows = picker.matches.len().max(1).min(avail).min(MAX_ROWS);
    let total = picker.matches.len();
    let max_scroll = total.saturating_sub(visible_rows);
    let scroll = picker
        .selected
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_scroll);

    let input_line = format!("> {}", picker.editor.buffer);
    let mut max_w = text_cells(&picker.title).max(text_cells(&input_line));
    for it in &picker.items {
        max_w = max_w.max(text_cells(&it.label) + 3.0 + text_cells(&it.hint));
    }
    let box_w = (max_w.max(26.0) + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = (2 + visible_rows) as f32;
    let max_rows = picker.items.len().max(1).min(avail).min(MAX_ROWS);
    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - (2 + max_rows) as f32) * 0.5).floor();

    // The frame runs a little past the last row so the glass shows as a
    // border under the content panel too.
    let frame_h = box_h + 0.35;
    let mut cmds = Vec::new();
    shadow(&mut cmds, px, box_w, frame_h);
    frame_glass(&mut cmds, px, box_w, frame_h);
    title_bar(&mut cmds, px, box_w, &picker.title);
    panel(&mut cmds, px, 0.3, 1.0, box_w - 0.3, box_h + 0.05);

    let ink = hex(INK, 1.0);
    field(&mut cmds, px, 0.6, 1.1, box_w - 0.6, 1.9);
    text(
        &mut cmds,
        px,
        1.0,
        1.0,
        Align::Left,
        &input_line,
        ink,
        false,
        None,
    );
    caret(
        &mut cmds,
        px,
        1.0 + 2.0 + picker.editor.cursor_cells(),
        1.2,
        1.8,
    );

    let has_bar = total > visible_rows;
    let row_x1 = box_w - if has_bar { 1.1 } else { 0.55 };
    for j in 0..visible_rows {
        let m = scroll + j;
        if m >= total {
            break;
        }
        let it = &picker.items[picker.matches[m]];
        let y = 2.0 + j as f32;
        let selected = m == picker.selected;
        if selected {
            gel(&mut cmds, px, 0.55, y + 0.06, row_x1, y + 0.94, &GEL_AQUA);
        }
        let (fg, shade) = if selected {
            (white(1.0), Some(hex(0x06264d, 0.9)))
        } else {
            (ink, None)
        };
        text(
            &mut cmds,
            px,
            1.5,
            y,
            Align::Left,
            &it.label,
            fg,
            selected,
            shade,
        );
        if !it.hint.is_empty() {
            let hint = if selected {
                hex(0xe6f7ff, 1.0)
            } else {
                hex(0x4a6f93, 1.0)
            };
            let hx = if has_bar { box_w - 1.6 } else { box_w - 1.5 };
            text(
                &mut cmds,
                px,
                hx,
                y,
                Align::Right,
                &it.hint,
                hint,
                false,
                shade,
            );
        }
    }

    if has_bar {
        let track_h = visible_rows as f32;
        let thumb_h = (track_h * visible_rows as f32 / total as f32).max(0.5);
        let thumb_y = 2.0
            + if max_scroll == 0 {
                0.0
            } else {
                (track_h - thumb_h).max(0.0) * scroll as f32 / max_scroll as f32
            };
        let tx = box_w - 0.85;
        track(&mut cmds, px, tx, 2.0, track_h);
        scroll_thumb(&mut cmds, px, tx + 0.04, thumb_y + 0.05, 0.3, thumb_h - 0.1);
    }

    rims(&mut cmds, px, box_w, frame_h);
    modal_body(cmds, origin_x, origin_y)
}

/// A faint recessed groove for a scrollbar thumb to ride in.
fn track(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y: f32, h: f32) {
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(hex(0x0b4f8f, 0.12)),
        segments: pill(px, x - 0.07, y + 0.05, x + 0.47, y + h - 0.05),
    });
}

/// The help overlay, after `ScrollModal::elements`: the same parent and
/// children, ids and origins, so `ScrollModal::scroll_commands` scrolls
/// it unchanged. The parent's own commands draw over its children, so
/// the title bar lives there and masks body lines scrolling under it.
fn help_elements(
    modal: &ScrollModal,
    ids: ModalIds<'_>,
    host_w: u32,
    host_h: u32,
    offset: u32,
    px: Px,
) -> Vec<CreateElementBody> {
    let (box_w, box_h) = modal.box_size(host_w, host_h);
    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();
    let (body_rows, max_offset) = modal.body_window(box_h);
    let offset = offset.min(max_offset);
    let body_lines = modal.lines.len().saturating_sub(1);
    let title = modal.lines.first().cloned().unwrap_or_default();

    let child = |id: &str, commands: Vec<DrawCmd>, y: f32, draw_order: i32| CreateElementBody {
        id: id.into(),
        commands,
        origin: Point { x: 0.0, y },
        is_visible: true,
        draw_order,
        parent: Some(ids.root.into()),
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    };

    // The parent clips every child to the box, so the shadow would be
    // cut off — the help window goes without one.
    let mut top = Vec::new();
    let ry = px.y(WIN_R).min(0.5);
    let rx = px.x(WIN_R).min(box_w / 2.0);
    top.push(DrawCmd::FillPath {
        fill: vgrad(0.0, box_h, hex(0x4fb0ea, 0.94), hex(0x0f5aa0, 0.96)),
        segments: vge_ui::shape::rounded_rect_path_corners(
            0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false,
        ),
    });
    title_bar(&mut top, px, box_w, &title);
    rims(&mut top, px, box_w, box_h);
    let mut elements = vec![CreateElementBody {
        id: ids.root.into(),
        commands: top,
        origin: Point {
            x: origin_x,
            y: origin_y,
        },
        is_visible: true,
        draw_order: MODAL_DRAW_ORDER,
        parent: None,
        size: Some(Point { x: box_w, y: box_h }),
        transform: None,
        anchor: OriginAnchor::Viewport,
    }];

    let mut fill = Vec::new();
    frame_glass(&mut fill, px, box_w, box_h);
    panel(&mut fill, px, 0.3, 1.0, box_w - 0.3, box_h - 0.3);
    elements.push(child(ids.body_fill, fill, 0.0, 0));

    // Section headings (unindented lines) in a deeper blue, bold.
    let mut lines = Vec::with_capacity(body_lines);
    for (i, line) in modal.lines.iter().enumerate().skip(1) {
        let heading = !line.is_empty() && !line.starts_with(' ');
        let color = hex(if heading { 0x0a5fa8 } else { INK }, 1.0);
        text(
            &mut lines,
            px,
            modal.text_x,
            1.0 + i as f32,
            Align::Left,
            line,
            color,
            heading,
            None,
        );
    }
    elements.push(child(ids.body_lines, lines, -(offset as f32), 1));

    if body_lines > body_rows {
        let track_h = body_rows as f32;
        let thumb_h = (track_h * body_rows as f32 / body_lines as f32).max(0.5);
        let mut groove = Vec::new();
        track(&mut groove, px, box_w - 1.0, 2.0, track_h);
        elements.push(child(ids.track, groove, 0.0, 2));
        // Drawn at local y = 0; the element's origin places it, so a
        // scroll is one `UpdateOrigin`, as in the stock modal.
        let mut thumb = Vec::new();
        scroll_thumb(&mut thumb, px, box_w - 0.93, 0.05, 0.3, thumb_h - 0.1);
        elements.push(child(
            ids.thumb,
            thumb,
            modal.thumb_origin_y(box_h, offset),
            3,
        ));
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: ModalIds<'static> = ModalIds {
        root: "m",
        body_fill: "m-bg",
        body_lines: "m-body",
        track: "m-track",
        thumb: "m-thumb",
    };

    /// Help scrolls through the stock `scroll_commands`, so the Aero
    /// build has to put the same ids at the same origins.
    #[test]
    fn help_matches_the_stock_scroll_contract() {
        let modal = ScrollModal::new((0..60).map(|i| format!("line {i}")).collect());
        let px = Px { w: 12.0, h: 28.0 };
        let stock = modal.elements(IDS, 100, 30, 7, px.w, px.h, MODAL_DRAW_ORDER);
        let aero = help_elements(&modal, IDS, 100, 30, 7, px);
        let shape = |els: &[CreateElementBody]| -> Vec<(String, Option<String>, f32, f32)> {
            els.iter()
                .map(|e| (e.id.clone(), e.parent.clone(), e.origin.x, e.origin.y))
                .collect()
        };
        assert_eq!(shape(&stock), shape(&aero));
    }

    #[test]
    fn style_names_parse() {
        assert_eq!(parse("aero").unwrap(), AERO);
        assert_eq!(parse("default").unwrap(), STOCK);
        assert!(parse("vista").is_err());
    }
}
