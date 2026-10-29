//! Flat parts for a multiplexer: the tab bar, pane titles and scroll
//! thumbs, and the dividers between panes — rounded fills in the host
//! theme's accent and surfaces.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, DrawCmd, FontStyle, Style};

use crate::measure::text_cells;
use crate::paint::Px;
use crate::shape::{chrome_corner_radii, rounded_rect_path, rounded_rect_path_corners};
use crate::style::{PaneChrome, TabBar, TabBarParts};
use crate::theme::{
    accent_style, active_text, activity_style, activity_text, dim_text, scrollbar, surface_style,
    title_text, title_thumb_style,
};

/// The tab bar: a rule along the bottom of the row in the accent, each
/// tab a number tag on the surface and a name rect that fills with the
/// accent when active and with the activity shade on unseen output.
pub(super) fn tab_bar(px: Px, bar: &TabBar<'_>) -> TabBarParts {
    let mut ground = Vec::new();
    let mut tabs = Vec::with_capacity(bar.tabs.len());
    let cmds = &mut ground;
    // No bar background — row 0 of the host vt100 is left untouched
    // (vmux never writes there) so the terminal's default cell color
    // shows through, matching whatever theme the user runs veter in.
    // Solid rule separating the tab row from the pane area at row 1.
    // A stroked line is centred on its path, so positioning it at y = 1.0
    // would straddle the boundary and the tab fills (which span [0, 1.0])
    // would overpaint its top half — the line then reads ~half as thick
    // under every tab. We want `RULE_W` of *visible* rule below y = 1.0,
    // so the bottom edge sits at 1.0 + RULE_W. Abutting the top edge
    // exactly at y = 1.0 leaves an anti-aliasing seam where the background
    // bleeds through between the fills and the rule, so we extend the
    // stroke up by `SEAM_OVERLAP` into the fills (drawn after the rule, so
    // they paint over the sliver): the overlap closes the seam without
    // thinning the visible rule.
    const RULE_W: f32 = 0.1;
    const SEAM_OVERLAP: f32 = 0.04;
    let stroke_w = RULE_W + SEAM_OVERLAP;
    let rule_y = 1.0 + (RULE_W - SEAM_OVERLAP) / 2.0;
    cmds.push(DrawCmd::DrawLines {
        stroke: accent_style(),
        line_width: stroke_w,
        lines: vec![(
            Point { x: 0.0, y: rule_y },
            Point {
                x: bar.width,
                y: rule_y,
            },
        )],
    });

    // Session-name segment: a dim rounded block pinned at x = 0,
    // showing the `vsd` session this vmux lives in.
    if let Some((label, w)) = bar.session {
        let (rx, ry) = chrome_corner_radii(w, 1.0, px.w, px.h);
        cmds.push(DrawCmd::FillPath {
            fill: surface_style(),
            segments: rounded_rect_path_corners(
                0.0, 0.0, w, 1.0, rx, ry, true,  // TL rounded
                false, // TR (meets the first tab / chevron)
                false, // BR
                false, // BL (sits on the row-1 rule)
            ),
        });
        cmds.push(DrawCmd::DrawText {
            origin: Point { x: 0.0, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(title_text()),
            font_style: FontStyle(0x01),
            font_scale: 1.0,
            text: format!(" {label} "),
        });
    }

    // Left overflow chevron — tabs clipped off the start of the bar.
    if let Some(x) = bar.left_chevron {
        cmds.push(DrawCmd::DrawText {
            origin: Point { x, y: 0.0 },
            align: Align::Left,
            fill: accent_style(),
            font_style: FontStyle(0x00),
            font_scale: 1.0,
            text: " ‹".to_string(),
        });
    }

    for slot in &bar.tabs {
        let mut tab = Vec::new();
        let cmds = &mut tab;
        let i = slot.index;
        let x = slot.x0;
        let num_w = slot.num_w;
        let name_w = slot.name_w;
        let num_text = format!(" {} ", i + 1);
        let name_text = format!(" {} ", slot.label);
        let is_active = slot.active;

        // Number sub-rect ("tag"), top-left corner rounded. Always the dim
        // modal background — activity lights up the name rect instead, so
        // the tag stays a stable anchor at the head of every pill.
        let has_activity = slot.activity;
        let (num_rx, num_ry) = chrome_corner_radii(num_w, 1.0, px.w, px.h);
        cmds.push(DrawCmd::FillPath {
            fill: surface_style(),
            segments: rounded_rect_path_corners(
                x,
                0.0,
                x + num_w,
                1.0,
                num_rx,
                num_ry,
                true,  // TL rounded
                false, // TR (meets the name rect, kept square)
                false, // BR
                false, // BL (sits on the row-1 rule)
            ),
        });
        // The tag always sits on the surface, so it takes the surface's
        // text colour. `active_text` is picked against the *accent* —
        // near-black under every pastel-accented scheme — and vanished
        // into a dark surface.
        cmds.push(DrawCmd::DrawText {
            origin: Point { x, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(title_text()),
            font_style: FontStyle(0x00),
            font_scale: 1.0,
            text: num_text,
        });

        // Name sub-rect — the body of the pill. Full accent on the active
        // tab; the muted activity shade when an inactive tab has unseen
        // background activity, so the whole pill lights up rather than just
        // the tag. Top-right corner rounded so the silhouette reads as a tab.
        let name_x0 = x + num_w;
        let name_bg = if is_active {
            Some(accent_style())
        } else if has_activity {
            Some(activity_style())
        } else {
            None
        };
        if let Some(name_fill) = name_bg {
            let (name_rx, name_ry) = chrome_corner_radii(name_w, 1.0, px.w, px.h);
            cmds.push(DrawCmd::FillPath {
                fill: name_fill,
                segments: rounded_rect_path_corners(
                    name_x0,
                    0.0,
                    name_x0 + name_w,
                    1.0,
                    name_rx,
                    name_ry,
                    false, // TL (meets the number rect)
                    true,  // TR rounded
                    false, // BR (sits on the row-1 rule)
                    false, // BL
                ),
            });
        }
        // Text picked against whichever fill the pill body has (full
        // accent, or the muted activity shade); dim only on a bare
        // pill. Bold still marks the active tab specifically.
        cmds.push(DrawCmd::DrawText {
            origin: Point { x: name_x0, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(if is_active {
                active_text()
            } else if has_activity {
                activity_text()
            } else {
                dim_text()
            }),
            font_style: FontStyle(if is_active { 0x01 } else { 0x00 }),
            font_scale: 1.0,
            text: name_text,
        });
        tabs.push(tab);
    }

    // Right overflow chevron — tabs clipped off the end of the bar.
    if let Some(x) = bar.right_chevron {
        cmds.push(DrawCmd::DrawText {
            origin: Point { x, y: 0.0 },
            align: Align::Left,
            fill: accent_style(),
            font_style: FontStyle(0x00),
            font_scale: 1.0,
            text: "› ".to_string(),
        });
    }
    TabBarParts { ground, tabs }
}

/// A pane's title, on a translucent accent thumb when focused and the
/// surface otherwise, and its scroll thumb.
pub(super) fn pane_chrome(cmds: &mut Vec<DrawCmd>, px: Px, pane: &PaneChrome<'_>) {
    let (pw, ph, title, focused) = (pane.w, pane.h, pane.title, pane.focused);

    // Title text in the top-right portal cell, right-aligned, with
    // a semi-transparent rounded "thumb" behind it (no stroke). Drawn
    // at higher draw_order than the portal so it overlays shell text.
    // 0.5-cell inset from the right edge keeps the thumb clear of any
    // separator on the pane boundary. Both thumb and text are nudged
    // down by 1/4 of a cell so the thumb doesn't visually fuse with the
    // top edge of the pane.
    if pane.show_title && !title.is_empty() {
        let text_w = text_cells(title);
        let top_pad = 0.25;
        let text_origin_x = pw - 1.0;
        let text_origin_y = top_pad;
        let pad_x = 0.5;
        let thumb_x0 = text_origin_x - text_w - pad_x;
        let thumb_x1 = text_origin_x + pad_x;
        let thumb_y0 = text_origin_y;
        let thumb_y1 = text_origin_y + 1.0;
        // Same slight rounding as the modal/tab chrome, with rx
        // compensated for anisotropic cells.
        let (rx, ry) = chrome_corner_radii(thumb_x1 - thumb_x0, thumb_y1 - thumb_y0, px.w, px.h);
        // Focused pane gets the translucent accent thumb; inactive panes
        // use the dim surface fill — the same color as an inactive tab's
        // number cell — so focus reads as an accent highlight.
        cmds.push(DrawCmd::FillPath {
            fill: if focused {
                title_thumb_style()
            } else {
                surface_style()
            },
            segments: rounded_rect_path(thumb_x0, thumb_y0, thumb_x1, thumb_y1, rx, ry),
        });
        cmds.push(DrawCmd::DrawText {
            origin: Point {
                x: text_origin_x,
                y: text_origin_y,
            },
            align: Align::Right,
            fill: Style::Flat(title_text()),
            font_style: FontStyle(if focused { 0x01 } else { 0x00 }),
            font_scale: 1.0,
            text: title.to_string(),
        });
    }

    // Scrollbar thumb at the right edge of the pane. Drawn in cell
    // units; sized by visible_rows / (visible_rows + history_depth)
    // and positioned by offset. Track starts below the title row so the
    // thumb never collides with the scroll-indicator title that sits in
    // the top-right cell while we're scrolling.
    if let Some(s) = pane.scroll {
        let portal_rows = ph.max(1.0);
        let total = portal_rows + s.history_depth as f32;
        let track_x = pw - 1.30;
        let track_w = 0.35;
        let track_y0 = 1.25;
        let track_y1 = (ph - 0.5).max(track_y0 + 1.0);
        let track_h = (track_y1 - track_y0).max(1.0);

        let thumb_h_norm = (portal_rows / total).clamp(0.05, 1.0);
        let thumb_h = (thumb_h_norm * track_h).max(0.6);
        let available = (track_h - thumb_h).max(0.0);
        // 0 offset = bottom (live); offset = history_depth = top.
        let thumb_norm = if s.history_depth == 0 {
            1.0
        } else {
            (s.history_depth.saturating_sub(s.offset) as f32) / s.history_depth as f32
        };
        let thumb_y = track_y0 + thumb_norm * available;

        cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(scrollbar()),
            rects: vec![Rect {
                x: track_x,
                y: thumb_y,
                w: track_w,
                h: thumb_h,
            }],
        });
    }
}

/// The dividers: plain accent lines.
pub(super) fn separators(cmds: &mut Vec<DrawCmd>, lines: Vec<(Point, Point)>) {
    cmds.push(DrawCmd::DrawLines {
        stroke: accent_style(),
        line_width: 0.1,
        lines,
    });
}
