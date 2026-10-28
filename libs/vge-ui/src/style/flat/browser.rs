//! Flat parts for a file browser: rounded panels on the terminal's own
//! surfaces, the cursor as a solid accent plate, marks in a warm badge.
//! The file-type icons are [`super::icons`].

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};

use crate::paint::Px;
use crate::shape::{chrome_corner_radii, rounded_rect_path};
use crate::style::{ListInk, StatusLeft};
use crate::theme::{
    self, accent_color, accent_style, active_text, darken, dim_text, surface_style, title_text,
    title_thumb_style,
};

/// Window backdrop on a terminal that publishes no theme — dark enough
/// that the panes read as panels. Under a host theme it is the
/// terminal's own background ([`backdrop_color`]).
const COLOR_BACKDROP: Color = Color {
    r: 0.07,
    g: 0.07,
    b: 0.09,
    a: 1.0,
};
/// A tile's own plate, under its picture and label, on a terminal that
/// publishes no theme ([`tile_color`]).
const COLOR_TILE: Color = Color {
    r: 0.13,
    g: 0.13,
    b: 0.16,
    a: 1.0,
};
/// Status-line text for a failed operation, on a terminal that
/// publishes no theme ([`error_color`]).
pub const COLOR_ERROR: Color = Color {
    r: 0.92,
    g: 0.48,
    b: 0.45,
    a: 1.0,
};
/// Marked-entry badge — a warm tone that reads as "chosen by hand",
/// distinct from the accent the cursor uses.
const MARK_COLOR: Color = Color {
    r: 0.95,
    g: 0.76,
    b: 0.33,
    a: 1.0,
};

/// The window's ground: the terminal's background when the host
/// publishes its palette, so the browser sits on a light theme as a
/// light window rather than a dark hole in it.
fn backdrop_color() -> Color {
    theme::host_bg().unwrap_or(COLOR_BACKDROP)
}

/// A tile's plate: the host's recessed surface, one step off the
/// backdrop in whichever direction the theme runs.
fn tile_color() -> Color {
    theme::host_surface_inset()
        .map(|c| Color { a: 1.0, ..c })
        .unwrap_or(COLOR_TILE)
}

fn error_color() -> Color {
    theme::warn_color().unwrap_or(COLOR_ERROR)
}

/// The accent as text ink. A `Style::Ref` cannot be used for text fills
/// we also shade, so this resolves it concretely.
fn accent_text() -> Color {
    let c = accent_color();
    // Lift it toward the foreground so it stays legible as small text —
    // toward white on a dark theme, toward the ink on a light one.
    let f = theme::host_fg().unwrap_or(Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    });
    Color {
        r: (c.r * 0.55 + f.r * 0.45).min(1.0),
        g: (c.g * 0.55 + f.g * 0.45).min(1.0),
        b: (c.b * 0.55 + f.b * 0.45).min(1.0),
        a: 1.0,
    }
}

fn text(x: f32, y: f32, align: Align, fill: Color, bold: bool, s: &str) -> DrawCmd {
    DrawCmd::DrawText {
        origin: Point { x, y },
        align,
        fill: Style::Flat(fill),
        font_style: FontStyle(if bold { 0x01 } else { 0x00 }),
        font_scale: 1.0,
        text: s.to_string(),
    }
}

pub(super) fn backdrop(cmds: &mut Vec<DrawCmd>, w: f32, h: f32) {
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(backdrop_color()),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }],
    });
}

pub(super) fn column_pane(cmds: &mut Vec<DrawCmd>, px: Px, a: Rect) {
    let (rx, ry) = chrome_corner_radii(a.w, a.h, px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: surface_style(),
        segments: rounded_rect_path(a.x + 0.25, a.y, a.x + a.w - 0.25, a.y + a.h, rx, ry),
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn column_row(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    a: Rect,
    y: f32,
    label: &str,
    is_dir: bool,
    selected: bool,
) {
    if selected {
        let (rx, ry) = chrome_corner_radii(a.w, a.h, px.w, px.h);
        cmds.push(DrawCmd::FillPath {
            fill: title_thumb_style(),
            segments: rounded_rect_path(a.x + 0.5, y + 0.05, a.x + a.w - 0.5, y + 0.95, rx, ry),
        });
    }
    // The selection is the translucent thumb, which the column's ground
    // shows through — so the selected row keeps the ink a file name
    // has, not the text a solid accent would want.
    let color = if selected {
        title_text()
    } else if is_dir {
        accent_text()
    } else {
        title_text()
    };
    cmds.push(text(a.x + 1.0, y, Align::Left, color, selected, label));
}

pub(super) fn note(cmds: &mut Vec<DrawCmd>, x: f32, y: f32, align: Align, s: &str, warn: bool) {
    let color = if warn { error_color() } else { dim_text() };
    cmds.push(text(x, y, align, color, warn, s));
}

pub(super) fn scrollbar(cmds: &mut Vec<DrawCmd>, a: Rect, track_h: f32, thumb_h: f32, frac: f32) {
    let x = a.x + a.w - 0.6;
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(Color {
            a: 0.18,
            ..title_text()
        }),
        rects: vec![Rect {
            x,
            y: a.y + 0.5,
            w: 0.3,
            h: track_h,
        }],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: accent_style(),
        rects: vec![Rect {
            x,
            y: a.y + 0.5 + (track_h - thumb_h) * frac,
            w: 0.3,
            h: thumb_h,
        }],
    });
}

/// A tile's plate — the accent under the cursor — and the plate its
/// picture sits on, so a selected tile still frames the image rather
/// than tinting it.
pub(super) fn tile_plate(cmds: &mut Vec<DrawCmd>, px: Px, t: Rect, img_h: f32, selected: bool) {
    let (x, y) = (t.x, t.y);
    let (rx, ry) = chrome_corner_radii(t.w, t.h, px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(if selected {
            Color {
                a: 0.9,
                ..accent_color()
            }
        } else {
            tile_color()
        }),
        segments: rounded_rect_path(x, y, x + t.w, y + t.h, rx, ry),
    });
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(darken(tile_color(), 0.25)),
        segments: rounded_rect_path(x + 0.25, y + 0.15, x + t.w - 0.25, y + img_h, rx, ry),
    });
}

pub(super) fn tile_mark(cmds: &mut Vec<DrawCmd>, px: Px, t: Rect) {
    let (rx, ry) = chrome_corner_radii(t.w, t.h, px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(MARK_COLOR),
        segments: rounded_rect_path(t.x + 0.3, t.y + 0.2, t.x + 1.3, t.y + 1.0, rx, ry),
    });
}

pub(super) fn tile_label(cmds: &mut Vec<DrawCmd>, x: f32, y: f32, label: &str, selected: bool) {
    let color = if selected {
        active_text()
    } else {
        title_text()
    };
    cmds.push(text(x, y, Align::Center, color, selected, label));
}

/// A list row: the accent plate under the cursor, a faint one on odd
/// rows — that striping is what carries the eye across a wide row.
pub(super) fn list_row_plate(cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, odd: bool, selected: bool) {
    let (rx, ry) = chrome_corner_radii(r.w, r.h, px.w, px.h);
    let fill = if selected {
        Color {
            a: 0.9,
            ..accent_color()
        }
    } else if odd {
        Color {
            a: 0.5,
            ..tile_color()
        }
    } else {
        return;
    };
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(fill),
        segments: rounded_rect_path(r.x, r.y, r.x + r.w, r.y + r.h, rx, ry),
    });
}

pub(super) fn list_row_mark(cmds: &mut Vec<DrawCmd>, r: Rect) {
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(MARK_COLOR),
        rects: vec![Rect {
            x: r.x + 0.15,
            y: r.y + 0.1,
            w: 0.3,
            h: (r.h - 0.2).max(0.4),
        }],
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn list_text(
    cmds: &mut Vec<DrawCmd>,
    x: f32,
    y: f32,
    align: Align,
    s: &str,
    what: ListInk,
    selected: bool,
) {
    let (color, bold) = match what {
        ListInk::Meta if selected => (active_text(), false),
        ListInk::Meta => (dim_text(), false),
        ListInk::Name { .. } if selected => (active_text(), true),
        ListInk::Name { is_dir: true } => (accent_text(), false),
        ListInk::Name { is_dir: false } => (title_text(), false),
    };
    cmds.push(text(x, y, align, color, bold, s));
}

pub(super) fn status_bar(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    a: Rect,
    left: &str,
    left_kind: StatusLeft,
    right: &str,
) {
    let (rx, ry) = chrome_corner_radii(a.w, a.h, px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: surface_style(),
        segments: rounded_rect_path(a.x, a.y, a.x + a.w, a.y + a.h, rx, ry),
    });
    cmds.push(text(
        a.x + a.w - 1.0,
        a.y,
        Align::Right,
        dim_text(),
        false,
        right,
    ));
    let (color, bold) = match left_kind {
        StatusLeft::Busy => (accent_text(), true),
        StatusLeft::Failed => (error_color(), true),
        StatusLeft::Message => (title_text(), true),
        StatusLeft::Path => (title_text(), false),
    };
    cmds.push(text(a.x + 1.0, a.y, Align::Left, color, bold, left));
}
