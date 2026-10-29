//! Aero parts for a multiplexer: the tab bar, pane titles and scroll
//! thumbs, and the dividers between panes.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, DrawCmd, Style};
use vge_protocol::path::{PathNode, PathSegment};

use super::{GEL_AQUA, Gel, bubble, gel, gloss, scroll_thumb, smoke};
use crate::measure::text_cells;
use crate::paint::{Px, hash, hex, pill, radial, text, unit, vgrad, white};
use crate::shape::rounded_rect_path_corners;
use crate::style::{PaneChrome, TabBar, TabbarMetrics};

/// A two-row bar of tabs standing on its bottom edge, Windows XP
/// fashion: the label sits on the bar's centre line, the active tab
/// rises a little above the rest and runs down into the panes, and the
/// tabs stand close together.
pub const TABBAR_METRICS: TabbarMetrics = TabbarMetrics {
    rows: 2,
    margin: 0.5,
    gap: 0.3,
    name_pad: 4.0,
};
/// The bar's height in cells.
const BAR_H: f32 = TABBAR_METRICS.rows as f32;
/// Top and bottom of the session capsule.
const CAP_Y0: f32 = 0.3;
const CAP_Y1: f32 = BAR_H - 0.3;
/// Where a tab's top edge sits: the active one higher than the rest.
const TAB_Y0: f32 = 0.38;
const ACTIVE_Y0: f32 = 0.18;
/// The radius of a tab's two top corners, in pixels.
const TAB_R: f32 = 6.0;
/// The active tab's shadow: the deep navy of the sky it falls on, not
/// black, which would read as dirt on the blue.
const SHADOW: u32 = 0x06264d;
/// The orange along the top of a tab with unseen output, Windows XP's
/// hot-tab strip: a darker edge on the tab's own outline, a solid band
/// `HILITE_SOLID` pixels deep under it, fading out into the tab's body
/// by `HILITE_FADE`.
const HILITE_EDGE: u32 = 0xd96a0a;
const HILITE: u32 = 0xffb43c;
const HILITE_SOLID: f32 = 2.5;
const HILITE_FADE: f32 = 9.0;
/// The text row, centred on the bar.
const TEXT_Y: f32 = (BAR_H - 1.0) * 0.5;

/// The tab bar: a two-row sky with the tabs standing on its bottom
/// edge — the accent's gel for the active one, frosted glass for the
/// rest, and an orange strip along the top of one with unseen output.
pub(super) fn tab_bar(cmds: &mut Vec<DrawCmd>, px: Px, bar: &TabBar<'_>) {
    let host_w = bar.width;
    let band = |y: f32, h: f32| Rect {
        x: 0.0,
        y,
        w: host_w,
        h,
    };

    // The sky: a deep-to-light aqua bar with the glass sheen across its
    // upper half, a bright lip along the bottom and a glow spilling down
    // over the panes.
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, BAR_H, hex(0x5ab8ef, 0.97), hex(0x1468b4, 0.97)),
        rects: vec![band(0.0, BAR_H)],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, BAR_H * 0.5, white(0.28), white(0.04)),
        rects: vec![band(0.0, BAR_H * 0.5)],
    });
    let lip = px.y(1.0);
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(hex(0xc8f6ff, 0.95)),
        rects: vec![band(BAR_H - lip, lip)],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(BAR_H, BAR_H + 0.45, hex(0x3fb8ff, 0.55), hex(0x3fb8ff, 0.0)),
        rects: vec![band(BAR_H, 0.45)],
    });

    // Bubbles in the free stretch after the last tab, some rising out
    // of the glow. Seeded by absolute column so they hold still while
    // tabs come and go.
    let session_w = bar.session.map(|(_, w)| w).unwrap_or(0.0);
    let free_x0 = bar
        .tabs
        .last()
        .map(|t| t.x0 + t.num_w + t.name_w)
        .unwrap_or(session_w)
        + 1.0;
    let free_x1 = bar.right_chevron.unwrap_or(host_w) - 0.5;
    for col in 0..host_w as u32 {
        let h = hash(col.wrapping_add(0x5eed));
        if !h.is_multiple_of(3) {
            continue;
        }
        let r = 3.5 + unit(h >> 3) * 7.5;
        let cx = col as f32 + unit(h >> 7);
        let cy = 0.25 + unit(h >> 11) * (BAR_H + 0.1);
        if cx - px.x(r) < free_x0 || cx + px.x(r) > free_x1 {
            continue;
        }
        bubble(cmds, px, cx, cy, r);
    }

    let navy = hex(0x06264d, 0.75);
    if let Some((label, w)) = bar.session {
        let (x0, x1) = (0.3, w - 0.1);
        cmds.push(DrawCmd::OutlineFillPath {
            fill: vgrad(CAP_Y0, CAP_Y1, white(1.0), hex(0x9fb3c8, 1.0)),
            stroke: Style::Flat(hex(0x5f7a96, 0.9)),
            line_width: px.line(1.0),
            segments: pill(px, x0, CAP_Y0, x1, CAP_Y1),
        });
        gloss(cmds, px, x0, CAP_Y0, x1, CAP_Y1, 0.5);
        text(
            cmds,
            px,
            (x0 + x1) * 0.5,
            TEXT_Y,
            Align::Center,
            label,
            hex(0x12345a, 1.0),
            true,
            Some(white(0.7)),
        );
    }

    if let Some(x) = bar.left_chevron {
        text(
            cmds,
            px,
            x,
            TEXT_Y,
            Align::Left,
            " ‹",
            white(1.0),
            true,
            Some(navy),
        );
    }

    // The active tab is the accent's gel — the one place the host's
    // colour shows, and what tells one nesting level from the next.
    let active = Gel::for_color(crate::theme::accent_color());
    // The active tab goes last, so its shadow falls on its neighbours.
    let (front, rest): (Vec<_>, Vec<_>) = bar.tabs.iter().partition(|t| t.active);
    for slot in rest.into_iter().chain(front) {
        let i = slot.index;
        let x = slot.x0;
        let (x0, x1) = (x, x + slot.num_w + slot.name_w);
        let is_active = slot.active;
        let has_activity = slot.activity;
        // An inactive tab stands on the bar's bright lip; the active
        // one runs down over it and carries the lip in its own tone.
        let y1 = BAR_H - px.y(1.0);
        let shadow = if is_active {
            drop_shadow(cmds, px, x0, ACTIVE_Y0, x1, BAR_H);
            gel_tab(cmds, px, x0, ACTIVE_Y0, x1, BAR_H, &active);
            // The bar's lip carries on under the active tab in the
            // gel's own light tone, so the tab sits on the same edge as
            // the rest rather than breaking it.
            let lip = px.y(1.0);
            cmds.push(DrawCmd::FillRectangles {
                fill: Style::Flat(hex(active.glow, 0.95)),
                rects: vec![Rect {
                    x: x0,
                    y: BAR_H - lip,
                    w: x1 - x0,
                    h: lip,
                }],
            });
            hex(active.shade, 0.85)
        } else {
            frost_tab(cmds, px, x0, TAB_Y0, x1, y1);
            if has_activity {
                hilite(cmds, px, x0, TAB_Y0, x1, y1);
            }
            navy
        };

        // The number sits in a socket pressed into the head of the
        // tab, centred over `num_w` and darkened from the tab's own
        // colour, so it reads as moulded into the tab rather than as a
        // badge stuck on it.
        let num_cx = x + 0.4 + slot.num_w * 0.5;
        let well = if is_active { active.shade } else { FROST_SHADE };
        let hw = slot.num_w * 0.38;
        socket(
            cmds,
            px,
            num_cx - hw,
            TEXT_Y + 0.1,
            num_cx + hw,
            TEXT_Y + 0.9,
            well,
        );
        text(
            cmds,
            px,
            num_cx,
            TEXT_Y,
            Align::Center,
            &format!("{}", i + 1),
            white(1.0),
            true,
            Some(hex(well, 0.7)),
        );
        text(
            cmds,
            px,
            x + slot.num_w + slot.name_w * 0.5,
            TEXT_Y,
            Align::Center,
            slot.label,
            white(1.0),
            is_active,
            Some(shadow),
        );
    }

    if let Some(x) = bar.right_chevron {
        text(
            cmds,
            px,
            x,
            TEXT_Y,
            Align::Left,
            "› ",
            white(1.0),
            true,
            Some(navy),
        );
    }
}

/// The deep tone of a frosted tab, which its number's socket is
/// darkened toward: the blue of the sky the glass sits on.
const FROST_SHADE: u32 = 0x0a2a5e;

/// A socket pressed into a tab: a hollow darkened toward `shade`, with
/// the lip's shadow along its top and a light catching its lower edge.
fn socket(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, shade: u32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(shade, 0.5), hex(shade, 0.18)),
        stroke: vgrad(y0, y1, hex(0x000000, 0.45), white(0.6)),
        line_width: px.line(1.0),
        segments: pill(px, x0, y0, x1, y1),
    });
    let (ix0, ix1) = (x0 + px.x(1.0), x1 - px.x(1.0));
    let (iy0, iy1) = (y0 + px.y(1.0), y1 - px.y(1.0));
    if ix1 > ix0 && iy1 > iy0 {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(y0, y0 + px.y(4.0), hex(0x000000, 0.3), hex(0x000000, 0.0)),
            segments: pill(px, ix0, iy0, ix1, iy1),
        });
    }
}

/// A tab's outline: square at the bottom, `TAB_R` round at the top.
fn tab_path(px: Px, x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<PathSegment> {
    let rx = px.x(TAB_R).min((x1 - x0) * 0.5);
    let ry = px.y(TAB_R).min(y1 - y0);
    rounded_rect_path_corners(x0, y0, x1, y1, rx, ry, true, true, false, false)
}

/// The same outline left open along the bottom, for the rim: a tab's
/// bottom edge is where it meets the bar, not a line of its own.
fn tab_rim(px: Px, x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<PathSegment> {
    let rx = px.x(TAB_R).min((x1 - x0) * 0.5);
    let ry = px.y(TAB_R).min(y1 - y0);
    let arc = |x: f32, y: f32| PathNode::ArcEllipseTo {
        large: false,
        sweep: true,
        rx,
        ry,
        rotation: 0.0,
        dst: Point { x, y },
    };
    vec![PathSegment {
        start: Point { x: x0, y: y1 },
        nodes: vec![
            PathNode::VerticalLineTo { y: y0 + ry },
            arc(x0 + rx, y0),
            PathNode::HorizontalLineTo { x: x1 - rx },
            arc(x1, y0 + ry),
            PathNode::VerticalLineTo { y: y1 },
        ],
    }]
}

/// The soft shadow round the active tab, lifting it off the bar: rings
/// of the tab's outline growing outward a pixel at a time, each faint,
/// so their overlap darkens toward the tab and fades away from it.
/// VGE has no blur; this is one, a few pixels wide.
fn drop_shadow(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    for (grow, alpha) in [(4.0, 0.04), (3.0, 0.07), (2.0, 0.11), (1.0, 0.16)] {
        let r = TAB_R + grow;
        let (gx, gy) = (px.x(grow), px.y(grow));
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(hex(SHADOW, alpha)),
            segments: rounded_rect_path_corners(
                x0 - gx,
                y0 - gy,
                x1 + gx,
                y1,
                px.x(r),
                px.y(r),
                true,
                true,
                false,
                false,
            ),
        });
    }
}

/// The sheen over a tab's upper half, the tab's own shape.
fn tab_sheen(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, a: f32) {
    let (sx0, sx1, sy0) = (x0 + px.x(2.0), x1 - px.x(2.0), y0 + px.y(1.5));
    let sy1 = y0 + (y1 - y0) * 0.5;
    if sx1 <= sx0 || sy1 <= sy0 {
        return;
    }
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(sy0, sy1, white(a), white(a * 0.05)),
        segments: tab_path(px, sx0, sy0, sx1, sy1),
    });
}

/// A gel tab: [`gel`]'s material on a tab's outline.
fn gel_tab(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32, g: &Gel) {
    let shape = tab_path(px, x0, y0, x1, y1);
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(y0, y1, hex(g.top, 1.0), hex(g.bottom, 1.0)),
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
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(hex(g.rim, 0.9)),
        line_width: px.line(1.0),
        segments: tab_rim(px, x0, y0, x1, y1),
    });
    tab_sheen(cmds, px, x0, y0, x1, y1, 0.42);
}

/// A frosted glass tab, for one that is neither active nor busy.
fn frost_tab(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(y0, y1, white(0.26), white(0.06)),
        segments: tab_path(px, x0, y0, x1, y1),
    });
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(white(0.55)),
        line_width: px.line(1.0),
        segments: tab_rim(px, x0, y0, x1, y1),
    });
    tab_sheen(cmds, px, x0, y0, x1, y1, 0.28);
}

/// The orange strip along the top of a tab with unseen output. It is
/// cut from the tab's own outline rather than laid across it, so it
/// rounds over the corners with the tab, and it fades into the body
/// instead of ending in a hard edge.
fn hilite(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    let fade = (y0 + px.y(HILITE_FADE)).min(y1);
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(
            y0 + px.y(HILITE_SOLID),
            fade,
            hex(HILITE, 0.95),
            hex(HILITE, 0.0),
        ),
        segments: tab_path(px, x0, y0, x1, fade),
    });
    // The darker edge over the rim, across the top and round both
    // corners, stopping where the corners straighten out.
    let rx = px.x(TAB_R).min((x1 - x0) * 0.5);
    let ry = px.y(TAB_R).min(y1 - y0);
    let arc = |x: f32, y: f32| PathNode::ArcEllipseTo {
        large: false,
        sweep: true,
        rx,
        ry,
        rotation: 0.0,
        dst: Point { x, y },
    };
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(hex(HILITE_EDGE, 1.0)),
        line_width: px.line(1.5),
        segments: vec![PathSegment {
            start: Point { x: x0, y: y0 + ry },
            nodes: vec![
                arc(x0 + rx, y0),
                PathNode::HorizontalLineTo { x: x1 - rx },
                arc(x1, y0 + ry),
            ],
        }],
    });
}

/// A pane's title — aqua gel when focused, smoked glass otherwise —
/// and its scroll thumb, an aqua capsule.
pub(super) fn pane_chrome(cmds: &mut Vec<DrawCmd>, px: Px, pane: &PaneChrome<'_>) {
    let (pw, ph, title, focused) = (pane.w, pane.h, pane.title, pane.focused);
    if pane.show_title && !title.is_empty() {
        let text_x = pw - 1.0;
        let y0 = 0.25;
        let x0 = text_x - text_cells(title) - 0.5;
        pane_title(
            cmds,
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
    if let Some(s) = pane.scroll {
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
        scroll_thumb(cmds, px, pw - 1.30, thumb_y, 0.35, thumb_h);
    }
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

/// The dividers: each a soft aqua glow under a bright core.
pub(super) fn separators(cmds: &mut Vec<DrawCmd>, lines: Vec<(Point, Point)>) {
    cmds.push(DrawCmd::DrawLines {
        stroke: Style::Flat(hex(0x3fb8ff, 0.25)),
        line_width: 0.35,
        lines: lines.clone(),
    });
    cmds.push(DrawCmd::DrawLines {
        stroke: Style::Flat(hex(0xaef0ff, 0.95)),
        line_width: 0.1,
        lines,
    });
}
