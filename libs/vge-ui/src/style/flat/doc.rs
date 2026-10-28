//! Flat parts for a document viewer. The page is deliberately *not*
//! given a ground of its own: the terminal's background is the paper,
//! so a themed veter, a light scheme and a plain xterm all read
//! correctly without the engine choosing a colour for any of them.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};

use crate::measure::elide;
use crate::paint::Px;
use crate::shape::{chrome_corner_radii, rounded_rect_path, rounded_rect_path_corners};
use crate::style::{DocChrome, DocFrame, DocRole, Figure};
use crate::theme;

/// Body text on a terminal that publishes no theme.
const COLOR_TEXT: Color = Color {
    r: 0.86,
    g: 0.87,
    b: 0.90,
    a: 1.0,
};

fn text_color() -> Color {
    theme::host_fg().unwrap_or(COLOR_TEXT)
}

fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

pub(super) fn ink(role: DocRole) -> Color {
    match role {
        DocRole::Body => text_color(),
        // A colour spine down the document: h1 takes the accent, the
        // levels under it step down through the text colours, so depth
        // reads even where the size difference is small.
        DocRole::Heading(1) => theme::accent_color(),
        DocRole::Heading(l) if l <= 4 => theme::title_text(),
        DocRole::Heading(_) => theme::dim_text(),
        DocRole::Dim | DocRole::Marker => theme::dim_text(),
        DocRole::Code => text_color(),
        DocRole::Link | DocRole::Accent => theme::accent_color(),
        DocRole::Warn => theme::warn_color().unwrap_or(theme::accent_color()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn code_plate(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x0: f32,
    x1: f32,
    y: f32,
    h: f32,
    first: bool,
    last: bool,
) {
    let (rx, ry) = chrome_corner_radii(x1 - x0, h.max(1.0), px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(theme::inset_bg()),
        segments: rounded_rect_path_corners(x0, y, x1, y + h, rx, ry, first, first, last, last),
    });
}

pub(super) fn table_band(cmds: &mut Vec<DrawCmd>, r: Rect, head: bool) {
    let fill = if head {
        alpha(theme::accent_color(), 0.18)
    } else {
        // Every other body row, so a wide table stays readable across.
        alpha(text_color(), 0.05)
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(fill),
        rects: vec![r],
    });
}

pub(super) fn quote_bar(cmds: &mut Vec<DrawCmd>, r: Rect, warning: bool) {
    let color = if warning {
        theme::warn_color().unwrap_or(theme::accent_color())
    } else {
        theme::accent_color()
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(color, 0.75)),
        rects: vec![r],
    });
}

pub(super) fn rule(cmds: &mut Vec<DrawCmd>, r: Rect, weight: f32) {
    let w = weight.max(0.02);
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(text_color(), 0.22)),
        rects: vec![Rect {
            x: r.x,
            y: r.y + (r.h - w) * 0.5,
            w: r.w,
            h: w,
        }],
    });
}

pub(super) fn figure(cmds: &mut Vec<DrawCmd>, px: Px, rect: Rect, content: Figure<'_>, zoom: f32) {
    match content {
        Figure::Image(id) => cmds.push(DrawCmd::DrawImage {
            target_rect: rect,
            image_id: id,
            source_rect: None,
        }),
        // Decoding, or gone: hold the space with the plate the picture
        // will fill, so nothing under it jumps when it lands.
        Figure::Placeholder { label } => {
            let (rx, ry) = chrome_corner_radii(rect.w, rect.h, px.w, px.h);
            cmds.push(DrawCmd::FillPath {
                fill: Style::Flat(theme::inset_bg()),
                segments: rounded_rect_path(
                    rect.x,
                    rect.y,
                    rect.x + rect.w,
                    rect.y + rect.h,
                    rx,
                    ry,
                ),
            });
            cmds.push(DrawCmd::DrawText {
                origin: Point {
                    x: rect.x + rect.w * 0.5,
                    y: rect.y + (rect.h - zoom) * 0.5,
                },
                align: Align::Center,
                fill: Style::Flat(theme::dim_text()),
                font_style: FontStyle(0),
                font_scale: zoom,
                text: label.to_string(),
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn code_chip(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x0: f32,
    x1: f32,
    y: f32,
    h: f32,
    zoom: f32,
) {
    let inset = 0.06 * zoom;
    let (rx, ry) = chrome_corner_radii(x1 - x0, h, px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(alpha(theme::inset_bg(), 0.85)),
        segments: rounded_rect_path(x0, y + inset, x1, y + h - inset, rx, ry),
    });
}

/// Wash behind a search match, brighter for the one the reader is on.
pub(super) fn search_hit(
    cmds: &mut Vec<DrawCmd>,
    x: f32,
    w: f32,
    y: f32,
    h: f32,
    zoom: f32,
    current: bool,
) {
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(
            theme::accent_color(),
            if current { 0.65 } else { 0.28 },
        )),
        rects: vec![Rect {
            x,
            y: y + 0.05 * zoom,
            w,
            h: h - 0.1 * zoom,
        }],
    });
}

/// The header bar, status line and scrollbar — everything outside the
/// page's clip.
pub(super) fn chrome(cmds: &mut Vec<DrawCmd>, px: Px, view: &DocFrame, c: &DocChrome<'_>) {
    let surface = theme::modal_bg();
    let last_row = (view.rows - 1.0).max(0.0);

    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(surface),
        rects: vec![
            Rect {
                x: 0.0,
                y: 0.0,
                w: view.cols,
                h: 1.0,
            },
            Rect {
                x: 0.0,
                y: last_row,
                w: view.cols,
                h: 1.0,
            },
        ],
    });
    // A hairline under the header, in the accent, so the bar reads as
    // chrome rather than as the first line of the document.
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(theme::accent_color(), 0.55)),
        rects: vec![Rect {
            x: 0.0,
            y: 1.0 - 0.06,
            w: view.cols,
            h: 0.06,
        }],
    });

    let half = (view.cols as usize).saturating_sub(4) / 2;
    cmds.push(bar_text(
        1.0,
        0.0,
        Align::Left,
        theme::title_text(),
        0x01,
        elide(c.title, half.max(8)),
    ));
    if let Some(section) = c.section.filter(|s| !s.is_empty()) {
        cmds.push(bar_text(
            view.cols - 1.0,
            0.0,
            Align::Right,
            theme::dim_text(),
            0,
            elide(section, half.max(8)),
        ));
    }

    let status_color = if c.status_warn {
        theme::warn_color().unwrap_or(theme::accent_color())
    } else {
        theme::dim_text()
    };
    cmds.push(bar_text(
        1.0,
        last_row,
        Align::Left,
        status_color,
        0,
        elide(c.status, (view.cols as usize).saturating_sub(8)),
    ));
    cmds.push(bar_text(
        view.cols - 1.0,
        last_row,
        Align::Right,
        theme::dim_text(),
        0,
        format!("{:>3}%", (c.progress * 100.0).round() as i32),
    ));

    scrollbar(cmds, px, c, view);
}

fn bar_text(x: f32, y: f32, align: Align, fill: Color, bits: u8, text: String) -> DrawCmd {
    DrawCmd::DrawText {
        origin: Point { x, y },
        align,
        fill: Style::Flat(fill),
        font_style: FontStyle(bits),
        font_scale: 1.0,
        text,
    }
}

fn scrollbar(cmds: &mut Vec<DrawCmd>, px: Px, c: &DocChrome<'_>, view: &DocFrame) {
    if c.visible >= 1.0 {
        return;
    }
    let track_y = view.content_y;
    let track_h = view.content_h;
    if track_h <= 0.0 {
        return;
    }
    let x = view.cols - 1.0;
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(text_color(), 0.08)),
        rects: vec![Rect {
            x,
            y: track_y,
            w: 1.0,
            h: track_h,
        }],
    });
    let thumb_h = (track_h * c.visible).max(1.0);
    let thumb_y = track_y + (track_h - thumb_h) * c.progress.clamp(0.0, 1.0);
    let (rx, ry) = chrome_corner_radii(1.0, thumb_h, px.w, px.h);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(theme::scrollbar()),
        segments: rounded_rect_path(x + 0.25, thumb_y, x + 0.75, thumb_y + thumb_h, rx, ry),
    });
}
