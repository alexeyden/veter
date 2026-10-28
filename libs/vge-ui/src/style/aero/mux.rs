//! Aero parts for a multiplexer: the tab bar, pane titles and scroll
//! thumbs, and the dividers between panes.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, DrawCmd, Style};

use super::{GEL_AMBER, GEL_AQUA, Gel, bubble, frost, gel, gloss, orb, scroll_thumb, smoke};
use crate::measure::text_cells;
use crate::paint::{Px, hash, hex, pill, text, unit, vgrad, white};
use crate::style::{PaneChrome, TabBar, TabbarMetrics};

/// A two-row bar, so a tab is a capsule with air above and below it:
/// the label sits on the bar's centre line with half a row clear either
/// way, and the capsules stand apart from each other and the edges.
pub const TABBAR_METRICS: TabbarMetrics = TabbarMetrics {
    rows: 2,
    margin: 0.6,
    gap: 0.8,
    name_pad: 4.0,
};
/// The bar's height in cells.
const BAR_H: f32 = TABBAR_METRICS.rows as f32;
/// Top and bottom of a tab capsule.
const CAP_Y0: f32 = 0.3;
const CAP_Y1: f32 = BAR_H - 0.3;
/// The text row, centred on the bar.
const TEXT_Y: f32 = (BAR_H - 1.0) * 0.5;

/// The tab bar: a two-row sky with the tabs as capsules on it — the
/// accent's gel for the active one, amber for one with unseen output, frosted
/// glass for the rest.
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
    for slot in &bar.tabs {
        let i = slot.index;
        let x = slot.x0;
        let (x0, x1) = (x, x + slot.num_w + slot.name_w);
        let is_active = slot.active;
        let has_activity = slot.activity;
        let shadow = if is_active {
            gel(cmds, px, x0, CAP_Y0, x1, CAP_Y1, &active);
            hex(active.shade, 0.85)
        } else if has_activity {
            gel(cmds, px, x0, CAP_Y0, x1, CAP_Y1, &GEL_AMBER);
            hex(GEL_AMBER.shade, 0.85)
        } else {
            frost(cmds, px, x0, CAP_Y0, x1, CAP_Y1);
            navy
        };

        // The number rides in an orb at the head of the capsule, both
        // centred over `num_w`.
        let num_cx = x + 0.4 + slot.num_w * 0.5;
        orb(
            cmds,
            px,
            num_cx - slot.num_w * 0.42,
            CAP_Y0 + 0.22,
            num_cx + slot.num_w * 0.42,
            CAP_Y1 - 0.22,
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
            Some(hex(0x06264d, 0.9)),
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
