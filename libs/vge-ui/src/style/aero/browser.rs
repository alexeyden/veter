//! Aero parts for a file browser: a sky-and-hills wallpaper with a sun
//! glow and drifting bubbles, frosted glass panes it shows through, the
//! light-blue selection box of a Win7 explorer, amber gel marks, photo
//! borders on thumbnails and a glass status bar. The file-type icons
//! are [`super::icons`].

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, DrawCmd, Style};

use super::icons::shadow;
use super::{GEL_AMBER, INK, bubble, gel, scroll_thumb, track};
use crate::measure::text_cells;
use crate::paint::{Px, ellipse, hash, hex, radial, rr, text, unit, vgrad, white};
use crate::style::{ListInk, StatusLeft};

/// Directory names, a step bluer than file names.
const DIR_INK: u32 = 0x0a5fa8;
const DIM_INK: u32 = 0x4a6f93;
const ERR_INK: u32 = 0xb3261e;
/// Corner radius of a glass pane, in pixels.
const PANE_R: f32 = 8.0;

fn ink(rgb: u32) -> Color {
    hex(rgb, 1.0)
}

fn name_ink(is_dir: bool) -> Color {
    ink(if is_dir { DIR_INK } else { INK })
}

/// The wallpaper: a deep-to-pale sky, two green hills along the bottom,
/// a sun glow in the top right and bubbles scattered over it all. The
/// panes are glass, so it shows through them faintly.
pub(super) fn backdrop(cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, h, hex(0x0b3f80, 1.0), hex(0x4aa2e0, 1.0)),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }],
    });
    // The far hill first, so the near one overlaps it.
    for (cx, cy, rx, ry, top, bottom) in [
        (w * 0.12, h * 1.22, w * 0.6, h * 0.4, 0x8fd95a, 0x23863a),
        (w * 0.74, h * 1.2, w * 0.75, h * 0.44, 0xa6e86a, 0x2f9e44),
    ] {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(cy - ry, h, hex(top, 1.0), hex(bottom, 1.0)),
            segments: ellipse(cx, cy, rx, ry),
        });
        // A sheen along the crest, as if the grass were lacquered.
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(cy - ry, cy - ry * 0.7, white(0.35), white(0.0)),
            segments: ellipse(cx, cy, rx * 0.97, ry * 0.97),
        });
    }
    let sun = Point {
        x: w * 0.86,
        y: h * 0.1,
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: radial(
            sun,
            Point {
                x: sun.x + px.x(260.0),
                y: sun.y,
            },
            white(0.45),
            white(0.0),
        ),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }],
    });
    // Bubbles, seeded by index so a redraw keeps them where they were.
    let count = ((w * h) / 45.0) as u32;
    for i in 0..count {
        let k = hash(i.wrapping_mul(2654435761).wrapping_add(0xb0b));
        let cx = unit(k) * w;
        let cy = unit(k >> 9) * h * 0.85;
        let r = 4.0 + unit(k >> 17) * unit(k >> 3) * 18.0;
        bubble(cmds, px, cx, cy, r);
    }
}

/// A frosted glass pane: a milky body the wallpaper shows through, a
/// sheen along its top, a bright inner rim and a cool outer one.
pub(super) fn glass_pane(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xe6f2fb, 0.62), hex(0xcfe4f5, 0.5)),
        stroke: Style::Flat(white(0.9)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, PANE_R),
    });
    let (sx0, sx1, sy0) = (x0 + px.x(2.0), x1 - px.x(2.0), y0 + px.y(1.5));
    if sx1 > sx0 {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(sy0, sy0 + 0.9, white(0.45), white(0.0)),
            segments: rr(px, sx0, sy0, sx1, (sy0 + 0.9).min(y1), PANE_R - 2.0),
        });
    }
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(hex(0x2a6a9a, 0.45)),
        line_width: px.line(1.0),
        segments: rr(
            px,
            x0 - px.x(1.0),
            y0 - px.y(1.0),
            x1 + px.x(1.0),
            y1 + px.y(1.0),
            PANE_R + 1.0,
        ),
    });
}

/// The Win7 explorer selection: a pale blue glossy box with a crisp
/// rim and a white inner line.
pub(super) fn select_box(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xf4faff, 1.0), hex(0xb3daf9, 1.0)),
        stroke: Style::Flat(hex(0x2f7fd0, 1.0)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, 3.0),
    });
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(white(0.75)),
        line_width: px.line(1.0),
        segments: rr(
            px,
            x0 + px.x(1.0),
            y0 + px.y(1.0),
            x1 - px.x(1.0),
            y1 - px.y(1.0),
            2.0,
        ),
    });
}

pub(super) fn column_pane(cmds: &mut Vec<DrawCmd>, px: Px, a: Rect) {
    glass_pane(
        cmds,
        px,
        a.x + 0.25,
        a.y + 0.1,
        a.x + a.w - 0.25,
        a.y + a.h - 0.1,
    );
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
        select_box(cmds, px, a.x + 0.5, y + 0.05, a.x + a.w - 0.5, y + 0.95);
    }
    text(
        cmds,
        px,
        a.x + 1.0,
        y,
        Align::Left,
        label,
        name_ink(is_dir),
        selected,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn note(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x: f32,
    y: f32,
    align: Align,
    s: &str,
    warn: bool,
) {
    let color = ink(if warn { ERR_INK } else { DIM_INK });
    text(cmds, px, x, y, align, s, color, warn, None);
}

pub(super) fn grid_pane(cmds: &mut Vec<DrawCmd>, px: Px, a: Rect) {
    glass_pane(cmds, px, a.x, a.y + 0.1, a.x + a.w - 0.25, a.y + a.h - 0.1);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn scrollbar(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    a: Rect,
    track_h: f32,
    thumb_h: f32,
    frac: f32,
) {
    let x = a.x + a.w - 0.75;
    track(cmds, px, x, a.y + 0.5, track_h);
    let y = a.y + 0.5 + (track_h - thumb_h) * frac;
    scroll_thumb(cmds, px, x + 0.04, y + 0.05, 0.3, thumb_h - 0.1);
}

/// A tile: the selection box under the cursor, a faint glass plate
/// otherwise.
pub(super) fn tile_plate(cmds: &mut Vec<DrawCmd>, px: Px, t: Rect, selected: bool) {
    let (x, y, x1, y1) = (t.x, t.y, t.x + t.w, t.y + t.h);
    if selected {
        select_box(cmds, px, x, y, x1, y1);
    } else {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(y, y1, white(0.22), white(0.06)),
            segments: rr(px, x, y, x1, y1, 4.0),
        });
    }
}

/// A marked tile's badge: an amber gel.
pub(super) fn tile_mark(cmds: &mut Vec<DrawCmd>, px: Px, t: Rect) {
    gel(
        cmds,
        px,
        t.x + 0.3,
        t.y + 0.2,
        t.x + 1.5,
        t.y + 0.85,
        &GEL_AMBER,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn tile_label(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x: f32,
    y: f32,
    label: &str,
    is_dir: bool,
    selected: bool,
) {
    text(
        cmds,
        px,
        x,
        y,
        Align::Center,
        label,
        name_ink(is_dir),
        selected,
        None,
    );
}

/// A list row: the selection box, or faint blue striping on odd rows.
pub(super) fn list_row_plate(cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, odd: bool, selected: bool) {
    let (x, y, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    if selected {
        select_box(cmds, px, x + 0.15, y, x1, y1);
    } else if odd {
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(hex(0xcfe5f7, 0.4)),
            segments: rr(px, x + 0.15, y, x1, y1, 3.0),
        });
    }
}

/// A marked row's badge: a slim amber gel down its left edge.
pub(super) fn list_row_mark(cmds: &mut Vec<DrawCmd>, px: Px, r: Rect) {
    gel(
        cmds,
        px,
        r.x + 0.2,
        r.y + 0.1,
        r.x + 0.55,
        r.y + r.h - 0.1,
        &GEL_AMBER,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn list_text(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x: f32,
    y: f32,
    align: Align,
    s: &str,
    what: ListInk,
    selected: bool,
) {
    let (color, bold) = match what {
        ListInk::Meta => (ink(DIM_INK), false),
        ListInk::Name { is_dir } => (name_ink(is_dir), selected),
    };
    text(cmds, px, x, y, align, s, color, bold, None);
}

/// A thumbnail in a white photo border on a soft drop shadow.
pub(super) fn thumbnail(cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, image_id: String) {
    let (bx, by) = (px.x(2.0), px.y(2.0));
    let frame = [
        (r.x - bx, r.y - by),
        (r.x + r.w + bx, r.y - by),
        (r.x + r.w + bx, r.y + r.h + by),
        (r.x - bx, r.y + r.h + by),
    ];
    shadow(cmds, px, &frame);
    cmds.push(DrawCmd::OutlineFillPolygon {
        fill: Style::Flat(white(1.0)),
        stroke: Style::Flat(hex(0x9fb6cc, 1.0)),
        line_width: px.line(1.0),
        points: frame.iter().map(|&(x, y)| Point { x, y }).collect(),
    });
    cmds.push(DrawCmd::DrawImage {
        target_rect: r,
        image_id,
        source_rect: None,
    });
}

/// The status line as a glass bar, lit along its top edge, with a glow
/// rising into the pane above and bubbles in the gap between its texts.
pub(super) fn status_bar(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    a: Rect,
    left: &str,
    left_kind: StatusLeft,
    right: &str,
) {
    let (x1, y1) = (a.x + a.w, a.y + a.h);
    cmds.extend([
        DrawCmd::FillRectangles {
            fill: vgrad(a.y - 0.4, a.y, hex(0x3fb8ff, 0.0), hex(0x3fb8ff, 0.45)),
            rects: vec![Rect {
                x: a.x,
                y: a.y - 0.4,
                w: a.w,
                h: 0.4,
            }],
        },
        DrawCmd::FillRectangles {
            fill: vgrad(a.y, y1, hex(0x5ab8ef, 0.97), hex(0x1468b4, 0.97)),
            rects: vec![Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
            }],
        },
        DrawCmd::FillRectangles {
            fill: vgrad(a.y, a.y + 0.5, white(0.28), white(0.04)),
            rects: vec![Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: 0.5,
            }],
        },
        DrawCmd::FillRectangles {
            fill: Style::Flat(hex(0xc8f6ff, 0.95)),
            rects: vec![Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: px.y(1.0),
            }],
        },
    ]);

    let shade = Some(hex(0x06264d, 0.85));
    let rx = x1 - 1.0;
    text(
        cmds,
        px,
        rx,
        a.y,
        Align::Right,
        right,
        hex(0xd8f0ff, 1.0),
        false,
        shade,
    );

    let (color, bold, left_shade) = match left_kind {
        StatusLeft::Busy => (hex(0xd6ff9e, 1.0), true, shade),
        StatusLeft::Failed => (hex(0xffd6d0, 1.0), true, Some(hex(0x5a0d08, 0.9))),
        StatusLeft::Message => (white(1.0), true, shade),
        StatusLeft::Path => (white(1.0), false, shade),
    };
    text(
        cmds,
        px,
        a.x + 1.0,
        a.y,
        Align::Left,
        left,
        color,
        bold,
        left_shade,
    );

    // Bubbles in whatever gap the two texts leave.
    let gap0 = a.x + 2.0 + text_cells(left);
    let gap1 = rx - text_cells(right) - 1.0;
    for col in gap0.ceil() as u32..gap1.max(gap0).floor() as u32 {
        let k = hash(col.wrapping_add(0xfeed));
        if !k.is_multiple_of(4) {
            continue;
        }
        let r = 3.0 + unit(k >> 5) * 5.0;
        let cx = col as f32 + unit(k >> 9);
        if cx - px.x(r) < gap0 || cx + px.x(r) > gap1 {
            continue;
        }
        bubble(cmds, px, cx, a.y + 0.2 + unit(k >> 13) * 0.6, r);
    }
}
