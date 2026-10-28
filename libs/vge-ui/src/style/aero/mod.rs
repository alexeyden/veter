//! Frutiger Aero — glossy gel pills, frosted glass, aqua skies and
//! bubbles. Its palette is its own: surfaces, inks and materials are
//! fixed, and the host theme reaches it only through the accent.
//!
//! This module holds the materials every part is made of (gel, frost,
//! smoke, orbs, bubbles, scroll thumbs), the window pieces the modals
//! are built from, and the modals themselves; each client family's
//! parts live in a child module ([`mux`], [`doc`], [`browser`]).

mod browser;
mod doc;
mod icons;
mod mux;

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, CreateElementBody, DrawCmd, OriginAnchor, Style};

use super::{
    DocChrome, DocFrame, DocRole, Engine, Figure, FileKind, IconBox, ListInk, Metrics, PaneChrome,
    StatusLeft, TabBar,
};
use crate::measure::{prefix_cells, text_cells};
use crate::modal::{ModalIds, ScrollModal};
use crate::paint::{Px, ellipse, hex, hgrad, pill, radial, rr, text, vgrad, white};
use crate::picker::PickerView;
use crate::shape::rounded_rect_path_corners;

/// The Aero engine.
pub struct Aero;

const METRICS: Metrics = Metrics {
    tabbar: mux::TABBAR_METRICS,
    page_inset: doc::PAGE_INSET,
};

impl Engine for Aero {
    fn name(&self) -> &'static str {
        "aero"
    }

    fn metrics(&self) -> &Metrics {
        &METRICS
    }

    fn tab_bar(&self, cmds: &mut Vec<DrawCmd>, px: Px, bar: &TabBar<'_>) {
        mux::tab_bar(cmds, px, bar);
    }

    fn pane_chrome(&self, cmds: &mut Vec<DrawCmd>, px: Px, pane: &PaneChrome<'_>) {
        mux::pane_chrome(cmds, px, pane);
    }

    fn separators(&self, cmds: &mut Vec<DrawCmd>, lines: Vec<(Point, Point)>) {
        mux::separators(cmds, lines);
    }

    fn doc_ground(&self, cmds: &mut Vec<DrawCmd>, px: Px, frame: &DocFrame) {
        doc::ground(cmds, px, frame);
    }

    fn doc_chrome(&self, cmds: &mut Vec<DrawCmd>, px: Px, frame: &DocFrame, c: &DocChrome<'_>) {
        doc::chrome(cmds, px, frame, c);
    }

    fn doc_ink(&self, role: DocRole) -> Color {
        doc::ink(role)
    }

    fn code_plate(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x0: f32,
        x1: f32,
        y: f32,
        h: f32,
        first: bool,
        last: bool,
        zoom: f32,
    ) {
        doc::code_plate(cmds, px, x0, x1, y, h, first, last, zoom);
    }

    fn table_band(&self, cmds: &mut Vec<DrawCmd>, r: Rect, head: bool) {
        doc::table_band(cmds, r, head);
    }

    fn quote_bar(&self, cmds: &mut Vec<DrawCmd>, r: Rect, warning: bool) {
        doc::quote_bar(cmds, r, warning);
    }

    fn rule(&self, cmds: &mut Vec<DrawCmd>, r: Rect, weight: f32) {
        doc::rule(cmds, r, weight);
    }

    fn figure(&self, cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, content: Figure<'_>, zoom: f32) {
        doc::figure(cmds, px, r, content, zoom);
    }

    fn code_chip(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x0: f32,
        x1: f32,
        y: f32,
        h: f32,
        zoom: f32,
    ) {
        doc::code_chip(cmds, px, x0, x1, y, h, zoom);
    }

    fn heading_mark(
        &self,
        cmds: &mut Vec<DrawCmd>,
        level: u8,
        x: f32,
        w: f32,
        y: f32,
        h: f32,
        zoom: f32,
    ) {
        doc::heading_mark(cmds, level, x, w, y, h, zoom);
    }

    fn search_hit(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        w: f32,
        y: f32,
        h: f32,
        zoom: f32,
        current: bool,
    ) {
        doc::search_hit(cmds, px, x, w, y, h, zoom, current);
    }

    fn browser_backdrop(&self, cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
        browser::backdrop(cmds, px, w, h);
    }

    fn column_pane(&self, cmds: &mut Vec<DrawCmd>, px: Px, area: Rect) {
        browser::column_pane(cmds, px, area);
    }

    fn column_row(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        y: f32,
        label: &str,
        is_dir: bool,
        selected: bool,
    ) {
        browser::column_row(cmds, px, area, y, label, is_dir, selected);
    }

    fn browser_note(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        align: Align,
        text: &str,
        warn: bool,
    ) {
        browser::note(cmds, px, x, y, align, text, warn);
    }

    fn grid_pane(&self, cmds: &mut Vec<DrawCmd>, px: Px, area: Rect) {
        browser::grid_pane(cmds, px, area);
    }

    fn browser_scrollbar(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        track_h: f32,
        thumb_h: f32,
        frac: f32,
    ) {
        browser::scrollbar(cmds, px, area, track_h, thumb_h, frac);
    }

    fn tile_plate(&self, cmds: &mut Vec<DrawCmd>, px: Px, tile: Rect, img_h: f32, selected: bool) {
        let _ = img_h;
        browser::tile_plate(cmds, px, tile, selected);
    }

    fn tile_mark(&self, cmds: &mut Vec<DrawCmd>, px: Px, tile: Rect) {
        browser::tile_mark(cmds, px, tile);
    }

    fn tile_label(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        label: &str,
        is_dir: bool,
        selected: bool,
    ) {
        browser::tile_label(cmds, px, x, y, label, is_dir, selected);
    }

    fn list_row_plate(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        row: Rect,
        odd: bool,
        selected: bool,
    ) {
        browser::list_row_plate(cmds, px, row, odd, selected);
    }

    fn list_row_mark(&self, cmds: &mut Vec<DrawCmd>, px: Px, row: Rect) {
        browser::list_row_mark(cmds, px, row);
    }

    fn list_text(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        align: Align,
        text: &str,
        what: ListInk,
        selected: bool,
    ) {
        browser::list_text(cmds, px, x, y, align, text, what, selected);
    }

    fn thumbnail(&self, cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, image_id: String) {
        browser::thumbnail(cmds, px, r, image_id);
    }

    fn file_icon(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        kind: FileKind,
        b: IconBox,
        is_link: bool,
        broken: bool,
    ) {
        icons::icon(cmds, px, kind, b, is_link, broken);
    }

    fn status_bar(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        left: &str,
        left_kind: StatusLeft,
        right: &str,
    ) {
        browser::status_bar(cmds, px, area, left, left_kind, right);
    }

    fn prompt(
        &self,
        id: &str,
        host_w: u32,
        host_h: u32,
        title: &str,
        line: &str,
        px: Px,
        caret: Option<usize>,
        draw_order: i32,
    ) -> CreateElementBody {
        prompt_element(id, host_w, host_h, title, line, px, caret, draw_order)
    }

    fn picker(
        &self,
        id: &str,
        host_w: u32,
        host_h: u32,
        picker: &dyn PickerView,
        px: Px,
        draw_order: i32,
    ) -> Vec<CreateElementBody> {
        vec![picker_element(id, host_w, host_h, picker, px, draw_order)]
    }

    fn scroll_modal(
        &self,
        modal: &ScrollModal,
        ids: ModalIds<'_>,
        host_w: u32,
        host_h: u32,
        offset: u32,
        px: Px,
        draw_order: i32,
    ) -> Vec<CreateElementBody> {
        scroll_elements(modal, ids, host_w, host_h, offset, px, draw_order)
    }
}

/// A gel colourway: body gradient, the inner glow rising from the
/// bottom, and the rim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gel {
    pub top: u32,
    pub bottom: u32,
    pub glow: u32,
    pub rim: u32,
    /// The drop shadow under text on the gel.
    pub shade: u32,
}

pub const GEL_GREEN: Gel = Gel {
    top: 0x8fdc4f,
    bottom: 0x2e9e1f,
    glow: 0xe8ff9a,
    rim: 0x1d6312,
    shade: 0x154a0c,
};
pub const GEL_AMBER: Gel = Gel {
    top: 0xffc54a,
    bottom: 0xe57a00,
    glow: 0xfff3b0,
    rim: 0x8a4300,
    shade: 0x6b3300,
};
pub const GEL_AQUA: Gel = Gel {
    top: 0x6fd3f7,
    bottom: 0x0a86d8,
    glow: 0x8ff6ff,
    rim: 0x0b4f8f,
    shade: 0x06264d,
};

pub const GEL_RED: Gel = Gel {
    top: 0xff8a7a,
    bottom: 0xd42a1e,
    glow: 0xffc9b8,
    rim: 0x7a120b,
    shade: 0x5a0d08,
};
pub const GEL_YELLOW: Gel = Gel {
    top: 0xffea70,
    bottom: 0xd9ab00,
    glow: 0xfffbc4,
    rim: 0x7a5e00,
    shade: 0x5c4600,
};
pub const GEL_BLUE: Gel = Gel {
    top: 0x86b8ff,
    bottom: 0x1d5fd6,
    glow: 0xc6deff,
    rim: 0x0d3478,
    shade: 0x0a2a5e,
};
pub const GEL_PINK: Gel = Gel {
    top: 0xff9ad6,
    bottom: 0xd2338f,
    glow: 0xffd3ee,
    rim: 0x7a1352,
    shade: 0x5c0d3a,
};
pub const GEL_VIOLET: Gel = Gel {
    top: 0xc6a2ff,
    bottom: 0x7a3fd6,
    glow: 0xe6d6ff,
    rim: 0x40177a,
    shade: 0x2d1060,
};

/// Aero's own colours, by the canonical swatch names (`theme`'s
/// aliases apply: `aqua` and `teal` are cyan, `pink` magenta, `violet`
/// purple). A host drawing with Aero publishes these as its swatches,
/// and picks its accent rotation from them.
pub const GELS: [(&str, Gel); 8] = [
    ("red", GEL_RED),
    ("orange", GEL_AMBER),
    ("yellow", GEL_YELLOW),
    ("green", GEL_GREEN),
    ("cyan", GEL_AQUA),
    ("blue", GEL_BLUE),
    ("magenta", GEL_PINK),
    ("purple", GEL_VIOLET),
];

/// The accent rotation a host drawing with Aero uses unless told
/// otherwise: a green top level, aqua one portal in, then violet and
/// pink.
pub const DEFAULT_ACCENTS: [&str; 4] = ["green", "aqua", "violet", "pink"];

impl Gel {
    /// The one colour that stands for this gel on the wire: its deep
    /// tone, which is what the eye takes for the gel's colour.
    pub const fn color(&self) -> u32 {
        self.bottom
    }

    /// The gel for a published colour: the one of [`GELS`] it stands
    /// for, or one derived from it — lighter on top, a pale glow, a dark
    /// rim — for a colour that is not Aero's own.
    pub fn for_color(c: Color) -> Gel {
        let rgb = crate::theme::pack(c) >> 8;
        if let Some((_, g)) = GELS.iter().find(|(_, g)| g.color() == rgb) {
            return *g;
        }
        let towards = |t: u32, k: f32| {
            let ch = |v: f32, w: u32| {
                ((v + (w as f32 / 255.0 - v) * k).clamp(0.0, 1.0) * 255.0).round() as u32
            };
            (ch(c.r, t >> 16 & 0xff) << 16) | (ch(c.g, t >> 8 & 0xff) << 8) | ch(c.b, t & 0xff)
        };
        Gel {
            top: towards(0xffffff, 0.45),
            bottom: rgb,
            glow: towards(0xffffff, 0.8),
            rim: towards(0x000000, 0.45),
            shade: towards(0x000000, 0.7),
        }
    }
}

/// The gel called `name`, aliases and all.
pub fn gel_named(name: &str) -> Option<&'static Gel> {
    let name = crate::theme::canonical_name(name);
    GELS.iter().find(|(n, _)| *n == name).map(|(_, g)| g)
}

/// The upper-half sheen every glossy thing carries: a white wash that
/// ends in a hard edge at the middle.
pub fn gloss(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, a: f32) {
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

pub fn gel(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, g: &Gel) {
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

/// The opacity of a glass body the wallpaper shows through: the stock
/// `alpha`, scaled by the user's `aero.glass`.
pub(super) fn glass_alpha(alpha: f32) -> f32 {
    (alpha * crate::style::settings().glass).min(1.0)
}

/// Frosted glass: a translucent white body with a bright rim.
pub fn frost(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, white(0.2), white(0.04)),
        stroke: Style::Flat(white(0.55)),
        line_width: px.line(1.0),
        segments: pill(px, x0, y0, x1, y1),
    });
    gloss(cmds, px, x0, y0, x1, y1, 0.28);
}

/// Smoked glass, for chrome that floats over shell text.
pub fn smoke(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0x2a6a9a, 0.7), hex(0x0b2a44, 0.85)),
        stroke: Style::Flat(white(0.5)),
        line_width: px.line(1.0),
        segments: pill(px, x0, y0, x1, y1),
    });
    gloss(cmds, px, x0, y0, x1, y1, 0.22);
}

/// A glossy blue orb, lit from the top left.
pub fn orb(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
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

/// A soap bubble, unless the user's `aero.bubbles` turned them off —
/// every bubble Aero draws goes through here.
pub fn bubble(cmds: &mut Vec<DrawCmd>, px: Px, cx: f32, cy: f32, r: f32) {
    if !crate::style::settings().bubbles {
        return;
    }
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

/// An aqua capsule scrollbar thumb, lit along its left edge.
pub fn scroll_thumb(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y: f32, w: f32, h: f32) {
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

// ── Modals ──────────────────────────────────────────────────────────────
//
// Each mirrors its `crate::modal` counterpart's geometry row for row,
// so a box sits where the stock one does and `ScrollModal::scroll_commands`
// still moves the right children.

fn modal_body(
    id: &str,
    commands: Vec<DrawCmd>,
    x: f32,
    y: f32,
    draw_order: i32,
) -> CreateElementBody {
    CreateElementBody {
        id: id.to_string(),
        commands,
        origin: Point { x, y },
        is_visible: true,
        draw_order,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    }
}

/// Corner radius of a window, in pixels.
pub const WIN_R: f32 = 9.0;
pub const INK: u32 = 0x0d2f52;

/// A soft drop shadow: a few widening, fading layers down and right.
pub fn shadow(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
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
pub fn frame_glass(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(0.0, h, hex(0x4fb0ea, 0.94), hex(0x0f5aa0, 0.96)),
        segments: rr(px, 0.0, 0.0, w, h, WIN_R),
    });
}

/// The title bar's sheen, bubbles and centred title over row 0.
pub fn title_bar(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, title: &str) {
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
pub fn panel(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xf6fcff, 0.97), hex(0xd6ebf8, 0.97)),
        stroke: Style::Flat(hex(0x0b4f8f, 0.6)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, 5.0),
    });
}

/// A slightly sunken white text field.
pub fn field(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xe4f1fa, 1.0), white(1.0)),
        stroke: Style::Flat(hex(0x7fb2d9, 1.0)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, 4.0),
    });
}

pub fn caret(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y0: f32, y1: f32) {
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
pub fn rims(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
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
#[allow(clippy::too_many_arguments)]
fn prompt_element(
    id: &str,
    host_w: u32,
    host_h: u32,
    title: &str,
    line: &str,
    px: Px,
    caret_at: Option<usize>,
    draw_order: i32,
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
    modal_body(id, cmds, origin_x, origin_y, draw_order)
}

/// The picker (command palette, tab and pane pickers), after
/// `picker_element`: title on row 0, the input on row 1, matches from
/// row 2.
fn picker_element(
    id: &str,
    host_w: u32,
    host_h: u32,
    picker: &dyn PickerView,
    px: Px,
    draw_order: i32,
) -> CreateElementBody {
    const MAX_ROWS: usize = 14;
    let avail = (host_h.saturating_sub(4)).max(1) as usize;
    let visible_rows = picker.matches().len().max(1).min(avail).min(MAX_ROWS);
    let total = picker.matches().len();
    let max_scroll = total.saturating_sub(visible_rows);
    let scroll = picker
        .selected()
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(max_scroll);

    let input_line = format!("> {}", picker.editor().buffer);
    let mut max_w = text_cells(picker.title()).max(text_cells(&input_line));
    for i in 0..picker.item_count() {
        let (label, hint) = picker.item(i);
        max_w = max_w.max(text_cells(label) + 3.0 + text_cells(hint));
    }
    let box_w = (max_w.max(26.0) + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = (2 + visible_rows) as f32;
    let max_rows = picker.item_count().max(1).min(avail).min(MAX_ROWS);
    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - (2 + max_rows) as f32) * 0.5).floor();

    // The frame runs a little past the last row so the glass shows as a
    // border under the content panel too.
    let frame_h = box_h + 0.35;
    let mut cmds = Vec::new();
    shadow(&mut cmds, px, box_w, frame_h);
    frame_glass(&mut cmds, px, box_w, frame_h);
    title_bar(&mut cmds, px, box_w, picker.title());
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
        1.0 + 2.0 + picker.editor().cursor_cells(),
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
        let (label, hint) = picker.item(picker.matches()[m]);
        let y = 2.0 + j as f32;
        let selected = m == picker.selected();
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
            label,
            fg,
            selected,
            shade,
        );
        if !hint.is_empty() {
            let hint_ink = if selected {
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
                hint,
                hint_ink,
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
    modal_body(id, cmds, origin_x, origin_y, draw_order)
}

/// A faint recessed groove for a scrollbar thumb to ride in.
pub fn track(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y: f32, h: f32) {
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(hex(0x0b4f8f, 0.12)),
        segments: pill(px, x - 0.07, y + 0.05, x + 0.47, y + h - 0.05),
    });
}

/// A scrolling overlay (help, a file preview), after `ScrollModal::elements`: the same parent and
/// children, ids and origins, so `ScrollModal::scroll_commands` scrolls
/// it unchanged. The parent's own commands draw over its children, so
/// the title bar lives there and masks body lines scrolling under it.
fn scroll_elements(
    modal: &ScrollModal,
    ids: ModalIds<'_>,
    host_w: u32,
    host_h: u32,
    offset: u32,
    px: Px,
    draw_order: i32,
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
    // cut off — a scrolling window goes without one.
    let mut top = Vec::new();
    let ry = px.y(WIN_R).min(0.5);
    let rx = px.x(WIN_R).min(box_w / 2.0);
    top.push(DrawCmd::FillPath {
        fill: vgrad(0.0, box_h, hex(0x4fb0ea, 0.94), hex(0x0f5aa0, 0.96)),
        segments: rounded_rect_path_corners(0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false),
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
        draw_order,
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

    /// A host publishes each gel by its colour, and the client has to
    /// get the very same gel back from it — that is what keeps a
    /// default Aero tab bar identical to one drawn from the gel itself.
    #[test]
    fn every_gel_comes_back_from_its_published_colour() {
        for (name, g) in &GELS {
            let c = crate::theme::unpack(g.color() << 8 | 0xff);
            assert_eq!(Gel::for_color(c), *g, "{name}");
        }
        assert_eq!(gel_named("aqua"), Some(&GEL_AQUA));
        assert_eq!(gel_named("violet"), Some(&GEL_VIOLET));
        assert_eq!(gel_named("pink"), Some(&GEL_PINK));
        for name in DEFAULT_ACCENTS {
            assert!(gel_named(name).is_some(), "{name}");
        }
    }

    /// A colour that is not one of Aero's still makes a gel: lighter
    /// on top than at the bottom, rimmed darker.
    #[test]
    fn a_foreign_colour_derives_a_gel_of_its_own() {
        let g = Gel::for_color(crate::theme::unpack(0x80_40_20_ff));
        assert_eq!(g.bottom, 0x804020);
        let lum = |c: u32| (c >> 16 & 0xff) + (c >> 8 & 0xff) + (c & 0xff);
        assert!(lum(g.top) > lum(g.bottom) && lum(g.rim) < lum(g.bottom));
    }

    /// A scrolling overlay scrolls through the stock `scroll_commands`,
    /// so the Aero build has to put the same ids at the same origins.
    #[test]
    fn scroll_elements_match_the_stock_scroll_contract() {
        let modal = ScrollModal::new((0..60).map(|i| format!("line {i}")).collect());
        let px = Px { w: 12.0, h: 28.0 };
        let stock = modal.elements(IDS, 100, 30, 7, px.w, px.h, 100);
        let aero = scroll_elements(&modal, IDS, 100, 30, 7, px, 100);
        let shape = |els: &[CreateElementBody]| -> Vec<(String, Option<String>, f32, f32)> {
            els.iter()
                .map(|e| (e.id.clone(), e.parent.clone(), e.origin.x, e.origin.y))
                .collect()
        };
        assert_eq!(shape(&stock), shape(&aero));
    }
}
