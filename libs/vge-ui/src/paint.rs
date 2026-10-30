//! Drawing primitives every style engine shares: colours from hex, the
//! pixel ↔ cell conversion, gradients, pixel-radius rounded shapes,
//! ellipses, shadowed text and a stable hash for scattering decoration.
//! Style-neutral — what a thing looks like is decided in
//! [`crate::style`], out of these.

use vge_protocol::codec::Point;
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};
use vge_protocol::path::{PathNode, PathSegment};

use crate::shape::rounded_rect_path;

pub fn hex(rgb: u32, a: f32) -> Color {
    Color {
        r: ((rgb >> 16) & 0xff) as f32 / 255.0,
        g: ((rgb >> 8) & 0xff) as f32 / 255.0,
        b: (rgb & 0xff) as f32 / 255.0,
        a,
    }
}

pub fn white(a: f32) -> Color {
    hex(0xffffff, a)
}

/// Pixel ↔ cell conversions for the current cell size.
#[derive(Clone, Copy, Debug)]
pub struct Px {
    pub w: f32,
    pub h: f32,
}

impl Px {
    pub fn x(self, px: f32) -> f32 {
        px / self.w.max(f32::EPSILON)
    }
    pub fn y(self, px: f32) -> f32 {
        px / self.h.max(f32::EPSILON)
    }
    /// A stroke width: VGE scales `line_width` by the mean cell side.
    pub fn line(self, px: f32) -> f32 {
        px / ((self.w + self.h) * 0.5).max(f32::EPSILON)
    }
}

pub fn vgrad(y0: f32, y1: f32, c0: Color, c1: Color) -> Style {
    Style::LinearGradient {
        p0: Point { x: 0.0, y: y0 },
        p1: Point { x: 0.0, y: y1 },
        c0,
        c1,
    }
}

pub fn hgrad(x0: f32, x1: f32, c0: Color, c1: Color) -> Style {
    Style::LinearGradient {
        p0: Point { x: x0, y: 0.0 },
        p1: Point { x: x1, y: 0.0 },
        c0,
        c1,
    }
}

pub fn radial(center: Point, outer: Point, c_inner: Color, c_outer: Color) -> Style {
    Style::RadialGradient {
        center,
        outer,
        c_inner,
        c_outer,
    }
}

/// A rounded rect whose corners are `r` *pixels* — circular on screen
/// whatever the cell aspect.
pub fn rr(px: Px, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Vec<PathSegment> {
    let rx = px.x(r).min((x1 - x0) / 2.0);
    let ry = px.y(r).min((y1 - y0) / 2.0);
    rounded_rect_path(x0, y0, x1, y1, rx, ry)
}

/// A capsule: ends fully round along its shorter side.
pub fn pill(px: Px, x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<PathSegment> {
    let r = ((x1 - x0) * px.w).min((y1 - y0) * px.h) / 2.0;
    rr(px, x0, y0, x1, y1, r)
}

pub fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Vec<PathSegment> {
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

/// Text, with a one-pixel drop shadow down and to the right when
/// `shadow` names a colour for it.
#[allow(clippy::too_many_arguments)]
pub fn text(
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

/// An hourglass in the character cell at `(x, y)` of text drawn at
/// `scale`: what a status with no figure to show — busy, no telling how
/// long — puts in front of its text. A glass outline over sand settled
/// in the bottom bulb with a little left in the top, in `ink`, with
/// `shadow` one pixel down and right like [`text`]'s.
pub fn hourglass(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x: f32,
    y: f32,
    scale: f32,
    ink: Color,
    shadow: Option<Color>,
) {
    // In pixels from the cell's top-left: a glyph a little narrower
    // than it is tall, as a font's would be, centred in the cell.
    let (cw, ch) = (scale * px.w, scale * px.h);
    let gh = (ch * 0.72).round().max(6.0);
    let gw = (gh * 0.62).round().min(cw);
    let (l, t) = (((cw - gw) / 2.0).round(), ((ch - gh) / 2.0).round());
    let (r, b) = (l + gw, t + gh);
    let mid = t + gh / 2.0;
    let cap = (gh * 0.1).max(1.0);
    let (inset, neck) = (gw * 0.12, gw * 0.08);
    let glass_x = |yy: f32| {
        // The left wall's x at height `yy`: the glass narrows linearly
        // from the cap to the neck.
        let half = if yy < mid {
            (mid - yy) / (mid - t - cap)
        } else {
            (yy - mid) / (b - cap - mid)
        };
        l + inset + (gw / 2.0 - inset - neck) * (1.0 - half.clamp(0.0, 1.0))
    };
    let draw = |cmds: &mut Vec<DrawCmd>, dx: f32, dy: f32, ink: Color| {
        let at = |px_x: f32, px_y: f32| Point {
            x: x + px.x(px_x + dx),
            y: y + px.y(px_y + dy),
        };
        let seg = |pts: &[(f32, f32)]| PathSegment {
            start: at(pts[0].0, pts[0].1),
            nodes: pts[1..]
                .iter()
                .map(|&(a, b)| PathNode::LineTo { dst: at(a, b) })
                .chain([PathNode::ClosePath])
                .collect(),
        };
        let sand_top = t + cap + (mid - t - cap) * 0.55;
        let pile = mid + (b - cap - mid) * 0.35;
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(ink),
            segments: vec![
                seg(&[(l, t), (r, t), (r, t + cap), (l, t + cap)]),
                seg(&[(l, b - cap), (r, b - cap), (r, b), (l, b)]),
                // What is left in the top bulb, draining to the neck.
                seg(&[
                    (glass_x(sand_top), sand_top),
                    (l + gw - (glass_x(sand_top) - l), sand_top),
                    (l + gw / 2.0, mid),
                ]),
                // The pile in the bottom one.
                seg(&[
                    (l + gw / 2.0, pile),
                    (r - inset, b - cap),
                    (l + inset, b - cap),
                ]),
            ],
        });
        let wall = |yy: f32| (glass_x(yy), l + gw - (glass_x(yy) - l));
        let (tl, tr) = wall(t + cap);
        let (ml, mr) = wall(mid);
        let (bl, br) = wall(b - cap);
        cmds.push(DrawCmd::DrawLinePath {
            stroke: Style::Flat(ink),
            line_width: px.line(1.0),
            segments: vec![PathSegment {
                start: at(tl, t + cap),
                nodes: [
                    (ml, mid),
                    (bl, b - cap),
                    (br, b - cap),
                    (mr, mid),
                    (tr, t + cap),
                ]
                .into_iter()
                .map(|(a, b)| PathNode::LineTo { dst: at(a, b) })
                .chain([PathNode::ClosePath])
                .collect(),
            }],
        });
    };
    if let Some(sh) = shadow {
        draw(cmds, 1.0, 1.0, sh);
    }
    draw(cmds, 0.0, 0.0, ink);
}

/// A cheap stable hash, so scattered decoration keeps its place across
/// redraws.
pub fn hash(mut v: u32) -> u32 {
    v ^= v >> 16;
    v = v.wrapping_mul(0x7feb_352d);
    v ^= v >> 15;
    v = v.wrapping_mul(0x846c_a68b);
    v ^ (v >> 16)
}

pub fn unit(v: u32) -> f32 {
    (v & 0xffff) as f32 / 65535.0
}
