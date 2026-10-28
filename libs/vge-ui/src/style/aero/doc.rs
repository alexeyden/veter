//! Aero parts for a document viewer: the page on a frosted sheet over
//! the sky-and-hills wallpaper, between glass header and status bars, in
//! the Aero palette — navy ink, aqua headings and quote bars, frosted
//! code plates, framed pictures with a drop shadow and highlighter
//! search hits.
//!
//! The wallpaper and sheet are the document's ground, an element of its
//! own under the page. VGE clips only to an element's size, so the page
//! element is what keeps half-scrolled rows on the sheet: the client
//! shrinks it by [`PAGE_INSET`] at either end, and the sheet reaches
//! [`SHEET_PAD`] past that.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};

use super::{INK, bubble, scroll_thumb, track};
use crate::measure::{elide, text_cells};
use crate::paint::{Px, ellipse, hash, hex, hgrad, radial, rr, text, unit, vgrad, white};
use crate::shape::rounded_rect_path_corners;
use crate::style::{DocChrome, DocFrame, DocRole, Figure};

const HEADING_INK: u32 = 0x0a5fa8;
const SUBHEADING_INK: u32 = 0x123f6b;
const DIM_INK: u32 = 0x4a6f93;
const LINK_INK: u32 = 0x0a7fd0;
const WARN_INK: u32 = 0xc0392b;
const AMBER: u32 = 0xe57a00;
/// Corner radius of the reading sheet, in pixels.
const SHEET_R: f32 = 8.0;
/// Rows between a bar and the page clip, at either end.
pub(super) const PAGE_INSET: f32 = 0.5;
/// How far the sheet reaches past the page clip, so a row cut off by
/// the clip is cut inside the sheet rather than at its rim.
const SHEET_PAD: f32 = 0.15;

pub(super) fn ink(role: DocRole) -> Color {
    hex(
        match role {
            DocRole::Body | DocRole::Code => INK,
            DocRole::Heading(1) => HEADING_INK,
            DocRole::Heading(l) if l <= 4 => SUBHEADING_INK,
            DocRole::Heading(_) | DocRole::Dim | DocRole::Marker => DIM_INK,
            DocRole::Link | DocRole::Accent => LINK_INK,
            DocRole::Warn => WARN_INK,
        },
        1.0,
    )
}

pub(super) fn ground(cmds: &mut Vec<DrawCmd>, p: Px, view: &DocFrame) {
    let w = view.cols;
    let (top, bottom) = (1.0, (view.rows - 1.0).max(1.0));
    let h = bottom - top;
    let band = Rect {
        x: 0.0,
        y: top,
        w,
        h,
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(top, bottom, hex(0x0b3f80, 1.0), hex(0x4aa2e0, 1.0)),
        rects: vec![band],
    });
    for (cx, cy, rx, ry, light, deep) in [
        (
            w * 0.12,
            top + h * 1.22,
            w * 0.6,
            h * 0.4,
            0x8fd95a,
            0x23863a,
        ),
        (
            w * 0.74,
            top + h * 1.2,
            w * 0.75,
            h * 0.44,
            0xa6e86a,
            0x2f9e44,
        ),
    ] {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(cy - ry, bottom, hex(light, 1.0), hex(deep, 1.0)),
            segments: ellipse(cx, cy, rx, ry),
        });
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(cy - ry, cy - ry * 0.7, white(0.35), white(0.0)),
            segments: ellipse(cx, cy, rx * 0.97, ry * 0.97),
        });
    }
    let sun = Point {
        x: w * 0.86,
        y: top + h * 0.08,
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: radial(
            sun,
            Point {
                x: sun.x + p.x(260.0),
                y: sun.y,
            },
            white(0.45),
            white(0.0),
        ),
        rects: vec![band],
    });
    let count = ((w * h) / 40.0) as u32;
    for i in 0..count {
        let k = hash(i.wrapping_mul(2654435761).wrapping_add(0x3d));
        let r = 4.0 + unit(k >> 17) * unit(k >> 3) * 18.0;
        bubble(cmds, p, unit(k) * w, top + unit(k >> 9) * h * 0.9, r);
    }

    // The sheet spans the reading column with a margin on either side,
    // stopping short of the scrollbar.
    let x0 = (view.content_x - 2.0).max(0.4);
    let x1 = (view.cols - view.content_x + 2.0).min(view.cols - 1.4);
    let y0 = view.content_y - SHEET_PAD;
    let y1 = view.content_y + view.content_h + SHEET_PAD;
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xf7fbfe, 0.94), hex(0xe8f2fa, 0.9)),
        stroke: Style::Flat(white(0.9)),
        line_width: p.line(1.0),
        segments: rr(p, x0, y0, x1, y1, SHEET_R),
    });
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(hex(0x2a6a9a, 0.45)),
        line_width: p.line(1.0),
        segments: rr(
            p,
            x0 - p.x(1.0),
            y0 - p.y(1.0),
            x1 + p.x(1.0),
            y1 + p.y(1.0),
            SHEET_R + 1.0,
        ),
    });
}

/// A code block's band: a frosted-blue plate whose rims run down the
/// sides, with a sheen under its top edge.
#[allow(clippy::too_many_arguments)]
pub(super) fn code_plate(
    cmds: &mut Vec<DrawCmd>,
    p: Px,
    x0: f32,
    x1: f32,
    y: f32,
    h: f32,
    first: bool,
    last: bool,
    zoom: f32,
) {
    // A block is a column of rows; only its first and last round their
    // corners, so the rims run down the sides alone.
    let r = 5.0_f32;
    let (rx, ry) = (p.x(r).min((x1 - x0) / 2.0), p.y(r).min(h / 2.0));
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(hex(0xdcebf7, 0.95)),
        segments: rounded_rect_path_corners(x0, y, x1, y + h, rx, ry, first, first, last, last),
    });
    let rim = Style::Flat(hex(0xa9c8e4, 1.0));
    let w = p.x(1.0);
    let sides = [(x0, y), (x1 - w, y)];
    cmds.push(DrawCmd::FillRectangles {
        fill: rim.clone(),
        rects: sides
            .iter()
            .map(|&(x, y)| Rect {
                x,
                y: y + if first { ry } else { 0.0 },
                w,
                h: h - if first { ry } else { 0.0 } - if last { ry } else { 0.0 },
            })
            .collect(),
    });
    if first {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(y, y + 0.6 * zoom, white(0.6), white(0.0)),
            segments: rr(p, x0 + w, y + p.y(1.0), x1 - w, y + 0.6 * zoom, r - 1.0),
        });
    }
}

pub(super) fn table_band(cmds: &mut Vec<DrawCmd>, r: Rect, head: bool) {
    let fill = if head {
        vgrad(r.y, r.y + r.h, hex(0xbfe6fb, 0.9), hex(0x8fcdf2, 0.9))
    } else {
        Style::Flat(hex(0x0a5fa8, 0.06))
    };
    cmds.push(DrawCmd::FillRectangles {
        fill,
        rects: vec![r],
    });
}

/// Quote bars as aqua gel rods — amber for a warning or caution alert.
pub(super) fn quote_bar(cmds: &mut Vec<DrawCmd>, r: Rect, warning: bool) {
    let (light, deep) = if warning {
        (0xffc54a, AMBER)
    } else {
        (0x6fd3f7, 0x0a86d8)
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: hgrad(r.x, r.x + r.w, hex(light, 1.0), hex(deep, 1.0)),
        rects: vec![r],
    });
}

/// A rule that fades in from both ends, so it reads as a light catching
/// the glass rather than a line drawn on it.
pub(super) fn rule(cmds: &mut Vec<DrawCmd>, r: Rect, weight: f32) {
    let w = weight.max(0.06);
    let x0 = r.x;
    let mid = x0 + r.w * 0.5;
    let x1 = x0 + r.w;
    let ry = r.y + (r.h - w) * 0.5;
    let blue = |a| hex(0x2f86d6, a);
    for (a, b, c0, c1) in [
        (x0, mid, blue(0.0), blue(0.7)),
        (mid, x1, blue(0.7), blue(0.0)),
    ] {
        cmds.push(DrawCmd::FillRectangles {
            fill: hgrad(a, b, c0, c1),
            rects: vec![Rect {
                x: a,
                y: ry,
                w: b - a,
                h: w,
            }],
        });
    }
}

/// A picture in a white photo border on a soft drop shadow; while it
/// decodes, a pale plate with its label holds the space.
pub(super) fn figure(cmds: &mut Vec<DrawCmd>, p: Px, rect: Rect, content: Figure<'_>, zoom: f32) {
    let (bx, by) = (p.x(3.0), p.y(3.0));
    let (x0, y0, x1, y1) = (
        rect.x - bx,
        rect.y - by,
        rect.x + rect.w + bx,
        rect.y + rect.h + by,
    );
    for (dx, dy, a) in [(4.0, 6.0, 0.1), (2.0, 3.0, 0.2)] {
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(hex(0x001a33, a)),
            segments: rr(
                p,
                x0 + p.x(dx),
                y0 + p.y(dy),
                x1 + p.x(dx),
                y1 + p.y(dy),
                3.0,
            ),
        });
    }
    cmds.push(DrawCmd::OutlineFillPath {
        fill: Style::Flat(white(1.0)),
        stroke: Style::Flat(hex(0x9fb6cc, 1.0)),
        line_width: p.line(1.0),
        segments: rr(p, x0, y0, x1, y1, 3.0),
    });
    match content {
        Figure::Image(id) => cmds.push(DrawCmd::DrawImage {
            target_rect: rect,
            image_id: id,
            source_rect: None,
        }),
        Figure::Placeholder { label } => {
            cmds.push(DrawCmd::FillRectangles {
                fill: vgrad(
                    rect.y,
                    rect.y + rect.h,
                    hex(0xe9f3fb, 1.0),
                    hex(0xcfe3f4, 1.0),
                ),
                rects: vec![rect],
            });
            cmds.push(DrawCmd::DrawText {
                origin: Point {
                    x: rect.x + rect.w * 0.5,
                    y: rect.y + (rect.h - zoom) * 0.5,
                },
                align: Align::Center,
                fill: Style::Flat(hex(DIM_INK, 1.0)),
                font_style: FontStyle(0),
                font_scale: zoom,
                text: label.to_string(),
            });
        }
    }
}

/// The chip behind an inline `code` span: a little frosted-blue lozenge.
#[allow(clippy::too_many_arguments)]
pub(super) fn code_chip(
    cmds: &mut Vec<DrawCmd>,
    p: Px,
    x0: f32,
    x1: f32,
    y: f32,
    h: f32,
    zoom: f32,
) {
    let inset = 0.08 * zoom;
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(
            y + inset,
            y + h - inset,
            hex(0xeef6fc, 1.0),
            hex(0xd3e6f5, 1.0),
        ),
        stroke: Style::Flat(hex(0xa9c8e4, 1.0)),
        line_width: p.line(1.0),
        segments: rr(p, x0, y + inset, x1, y + h - inset, 4.0),
    });
}

/// An aqua stroke under a top-level heading, fading out to the right.
pub(super) fn heading_mark(
    cmds: &mut Vec<DrawCmd>,
    level: u8,
    x: f32,
    w: f32,
    y: f32,
    h: f32,
    zoom: f32,
) {
    if level != 1 {
        return;
    }
    let x0 = x;
    let x1 = x0 + w.max(8.0) * 1.6;
    let t = 0.1 * zoom;
    cmds.push(DrawCmd::FillRectangles {
        fill: hgrad(x0, x1, hex(0x0a86d8, 0.85), hex(0x6fd3f7, 0.0)),
        rects: vec![Rect {
            x: x0,
            y: y + h - t * 1.5,
            w: x1 - x0,
            h: t,
        }],
    });
}

/// Search hits in a highlighter yellow, the one the reader is on in a
/// deeper amber.
#[allow(clippy::too_many_arguments)]
pub(super) fn search_hit(
    cmds: &mut Vec<DrawCmd>,
    p: Px,
    x: f32,
    w: f32,
    y: f32,
    h: f32,
    zoom: f32,
    current: bool,
) {
    let (c0, c1) = if current {
        (0xffd34d, 0xffa21a)
    } else {
        (0xfff3a6, 0xffe066)
    };
    let (y0, y1) = (y + 0.05 * zoom, y + h - 0.05 * zoom);
    cmds.push(DrawCmd::FillPath {
        fill: vgrad(y0, y1, hex(c0, 0.9), hex(c1, 0.9)),
        segments: rr(p, x, y0, x + w, y1, 3.0),
    });
}

// ── Chrome ──────────────────────────────────────────────────────────────

/// A glass bar across row `y`, lit along the edge that meets the page
/// (`lit_bottom` for the header) and glowing over it.
fn glass_bar(cmds: &mut Vec<DrawCmd>, p: Px, w: f32, y: f32, lit_bottom: bool) {
    let glow = |y0: f32, y1: f32, c0: f32, c1: f32| DrawCmd::FillRectangles {
        fill: vgrad(y0, y1, hex(0x3fb8ff, c0), hex(0x3fb8ff, c1)),
        rects: vec![Rect {
            x: 0.0,
            y: y0,
            w,
            h: y1 - y0,
        }],
    };
    cmds.push(if lit_bottom {
        glow(y + 1.0, y + 1.4, 0.5, 0.0)
    } else {
        glow(y - 0.4, y, 0.0, 0.45)
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(y, y + 1.0, hex(0x5ab8ef, 0.97), hex(0x1468b4, 0.97)),
        rects: vec![Rect {
            x: 0.0,
            y,
            w,
            h: 1.0,
        }],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(y, y + 0.5, white(0.28), white(0.04)),
        rects: vec![Rect {
            x: 0.0,
            y,
            w,
            h: 0.5,
        }],
    });
    let lip = p.y(1.0);
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(hex(0xc8f6ff, 0.95)),
        rects: vec![Rect {
            x: 0.0,
            y: if lit_bottom { y + 1.0 - lip } else { y },
            w,
            h: lip,
        }],
    });
}

/// Bubbles in the stretch `x0..x1` of row `y`, seeded by column.
fn bar_bubbles(cmds: &mut Vec<DrawCmd>, p: Px, x0: f32, x1: f32, y: f32, seed: u32) {
    for col in x0.ceil().max(0.0) as u32..x1.max(x0).floor() as u32 {
        let k = hash(col.wrapping_add(seed));
        if !k.is_multiple_of(3) {
            continue;
        }
        let r = 3.0 + unit(k >> 5) * 6.0;
        let cx = col as f32 + unit(k >> 9);
        if cx - p.x(r) < x0 || cx + p.x(r) > x1 {
            continue;
        }
        bubble(cmds, p, cx, y + 0.2 + unit(k >> 13) * 0.6, r);
    }
}

/// The header and status bars as glass, the same strings on them, and an
/// aqua scroll thumb.
pub(super) fn chrome(cmds: &mut Vec<DrawCmd>, p: Px, view: &DocFrame, c: &DocChrome<'_>) {
    let last_row = (view.rows - 1.0).max(0.0);
    glass_bar(cmds, p, view.cols, 0.0, true);
    glass_bar(cmds, p, view.cols, last_row, false);

    let navy = Some(hex(0x06264d, 0.85));
    let half = (view.cols as usize).saturating_sub(4) / 2;
    let title = elide(c.title, half.max(8));
    text(
        cmds,
        p,
        1.0,
        0.0,
        Align::Left,
        &title,
        white(1.0),
        true,
        navy,
    );
    let mut gap1 = view.cols - 1.0;
    if let Some(section) = c.section.filter(|s| !s.is_empty()) {
        let s = elide(section, half.max(8));
        gap1 -= text_cells(&s) + 1.0;
        let x = view.cols - 1.0;
        text(
            cmds,
            p,
            x,
            0.0,
            Align::Right,
            &s,
            hex(0xd8f0ff, 1.0),
            false,
            navy,
        );
    }
    bar_bubbles(cmds, p, 2.0 + text_cells(&title), gap1, 0.0, 0x51e5);

    let status = elide(c.status, (view.cols as usize).saturating_sub(8));
    let (ink, shade) = if c.status_warn {
        (hex(0xffd6d0, 1.0), Some(hex(0x5a0d08, 0.9)))
    } else {
        (hex(0xd8f0ff, 1.0), navy)
    };
    text(
        cmds,
        p,
        1.0,
        last_row,
        Align::Left,
        &status,
        ink,
        false,
        shade,
    );
    let pct = format!("{:>3}%", (c.progress * 100.0).round() as i32);
    let x = view.cols - 1.0;
    text(
        cmds,
        p,
        x,
        last_row,
        Align::Right,
        &pct,
        white(1.0),
        true,
        navy,
    );

    if c.visible < 1.0 && view.content_h > 0.0 {
        let (ty, th) = (view.content_y, view.content_h);
        let x = view.cols - 0.85;
        track(cmds, p, x, ty, th);
        let thumb_h = (th * c.visible).max(1.0);
        let thumb_y = ty + (th - thumb_h) * c.progress.clamp(0.0, 1.0);
        scroll_thumb(cmds, p, x + 0.04, thumb_y + 0.05, 0.3, thumb_h - 0.1);
    }
}
