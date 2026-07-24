//! File-type icons, drawn as VGE vectors.
//!
//! Every icon is composed inside a unit box and scaled into the tile's
//! picture area, so a tile looks the same at every zoom level. Cell
//! anisotropy is handled by the caller passing the box it wants filled;
//! the shapes themselves are laid out in that box's own coordinates.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Color, DrawCmd, Style};
use vge_protocol::path::{PathNode, PathSegment};
use vge_ui::theme::{accent_color, darken};

use crate::entry::Media;

/// Primary icon stroke/fill — light enough to read on the dark tile.
pub const INK: Color = Color {
    r: 0.72,
    g: 0.76,
    b: 0.84,
    a: 1.0,
};
/// Secondary detail (text ruling, film sprockets).
pub const INK_DIM: Color = Color {
    r: 0.46,
    g: 0.49,
    b: 0.57,
    a: 1.0,
};

/// The box an icon is drawn into, in absolute cell coordinates.
#[derive(Debug, Clone, Copy)]
pub struct IconBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl IconBox {
    /// A box of `frac` of `area`'s smaller dimension, centered in it.
    /// `cell_pw/cell_ph` keep the result visually square.
    pub fn centered(
        area: (f32, f32, f32, f32),
        frac: f32,
        cell_pw: f32,
        cell_ph: f32,
    ) -> IconBox {
        let (ax, ay, aw, ah) = area;
        // Square in pixels: pick the side that fits both ways.
        let px = (aw * cell_pw).min(ah * cell_ph) * frac;
        let w = px / cell_pw.max(1.0);
        let h = px / cell_ph.max(1.0);
        IconBox {
            x: ax + (aw - w) * 0.5,
            y: ay + (ah - h) * 0.5,
            w,
            h,
        }
    }

    /// Map unit-box coordinates (0..1) into the box.
    fn p(&self, u: f32, v: f32) -> Point {
        Point {
            x: self.x + u * self.w,
            y: self.y + v * self.h,
        }
    }

    /// Map a unit-box rectangle into the box.
    fn r(&self, u: f32, v: f32, uw: f32, vh: f32) -> Rect {
        Rect {
            x: self.x + u * self.w,
            y: self.y + v * self.h,
            w: uw * self.w,
            h: vh * self.h,
        }
    }
}

fn flat(c: Color) -> Style {
    Style::Flat(c)
}

fn poly(b: &IconBox, fill: Style, pts: &[(f32, f32)]) -> DrawCmd {
    DrawCmd::FillPolygon {
        fill,
        points: pts.iter().map(|(u, v)| b.p(*u, *v)).collect(),
    }
}

fn rect(b: &IconBox, fill: Style, u: f32, v: f32, uw: f32, vh: f32) -> DrawCmd {
    DrawCmd::FillRectangles {
        fill,
        rects: vec![b.r(u, v, uw, vh)],
    }
}

fn strip(b: &IconBox, stroke: Style, width: f32, pts: &[(f32, f32)]) -> DrawCmd {
    DrawCmd::DrawLineStrip {
        stroke,
        line_width: width,
        points: pts.iter().map(|(u, v)| b.p(*u, *v)).collect(),
    }
}

/// An ellipse as a closed two-arc path, since VGE has no circle op and
/// terminal cells are not square.
fn ellipse(b: &IconBox, fill: Style, cu: f32, cv: f32, ru: f32, rv: f32) -> DrawCmd {
    let (rx, ry) = (ru * b.w, rv * b.h);
    let start = b.p(cu - ru, cv);
    let end = b.p(cu + ru, cv);
    let arc = |dst: Point, sweep: bool| PathNode::ArcEllipseTo {
        large: false,
        sweep,
        rx,
        ry,
        rotation: 0.0,
        dst,
    };
    DrawCmd::FillPath {
        fill,
        segments: vec![PathSegment {
            start,
            nodes: vec![arc(end, false), arc(start, false), PathNode::ClosePath],
        }],
    }
}

/// The icon for `media`, drawn into `b`. `is_link` adds the little
/// shortcut arrow every file manager puts on a symlink.
pub fn draw(media: Media, b: IconBox, is_link: bool, broken: bool) -> Vec<DrawCmd> {
    let mut cmds = match media {
        Media::Dir => folder(&b),
        Media::Image => picture(&b),
        Media::Video => film(&b),
        Media::Audio => note(&b),
        Media::Text => page(&b, true),
        Media::Archive => archive(&b),
        Media::Binary => page(&b, false),
    };
    if broken {
        cmds.push(strip(&b, flat(INK_DIM), 0.08, &[(0.15, 0.15), (0.85, 0.85)]));
        cmds.push(strip(&b, flat(INK_DIM), 0.08, &[(0.85, 0.15), (0.15, 0.85)]));
    } else if is_link {
        cmds.push(poly(
            &b,
            flat(accent_color()),
            &[(0.62, 0.98), (0.98, 0.98), (0.98, 0.62)],
        ));
    }
    cmds
}

/// A folder: back panel with a tab, front panel slightly offset.
fn folder(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        // Tab + back panel.
        poly(
            b,
            flat(darken(accent, 0.25)),
            &[
                (0.04, 0.20),
                (0.42, 0.20),
                (0.50, 0.30),
                (0.96, 0.30),
                (0.96, 0.88),
                (0.04, 0.88),
            ],
        ),
        // Front panel, lifted so the two read as separate leaves.
        poly(
            b,
            flat(accent),
            &[(0.04, 0.40), (0.96, 0.40), (0.88, 0.88), (0.12, 0.88)],
        ),
    ]
}

/// A page with a folded corner; `ruled` adds text lines.
fn page(b: &IconBox, ruled: bool) -> Vec<DrawCmd> {
    let mut cmds = vec![
        poly(
            b,
            flat(INK),
            &[
                (0.16, 0.08),
                (0.66, 0.08),
                (0.84, 0.28),
                (0.84, 0.92),
                (0.16, 0.92),
            ],
        ),
        // The fold, in the tile's own shade so it reads as a crease.
        poly(b, flat(INK_DIM), &[(0.66, 0.08), (0.84, 0.28), (0.66, 0.28)]),
    ];
    if ruled {
        for (i, v) in [0.44_f32, 0.58, 0.72].iter().enumerate() {
            let right = if i == 2 { 0.58 } else { 0.72 };
            cmds.push(rect(b, flat(INK_DIM), 0.26, *v, right - 0.26, 0.06));
        }
    }
    cmds
}

/// A picture: framed panel with a horizon, a hill and a sun.
fn picture(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        rect(b, flat(INK), 0.08, 0.16, 0.84, 0.68),
        rect(b, flat(darken(accent, 0.55)), 0.14, 0.22, 0.72, 0.56),
        ellipse(b, flat(accent), 0.32, 0.38, 0.07, 0.07),
        poly(
            b,
            flat(INK),
            &[(0.14, 0.78), (0.42, 0.46), (0.60, 0.66), (0.72, 0.54), (0.86, 0.78)],
        ),
    ]
}

/// A film frame: panel with sprocket holes down both edges.
fn film(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    let mut cmds = vec![
        rect(b, flat(INK), 0.08, 0.18, 0.84, 0.64),
        rect(b, flat(darken(accent, 0.6)), 0.22, 0.18, 0.56, 0.64),
    ];
    for i in 0..3 {
        let v = 0.26 + i as f32 * 0.20;
        cmds.push(rect(b, flat(darken(accent, 0.6)), 0.11, v, 0.08, 0.10));
        cmds.push(rect(b, flat(darken(accent, 0.6)), 0.81, v, 0.08, 0.10));
    }
    // Play triangle.
    cmds.push(poly(b, flat(accent), &[(0.42, 0.34), (0.66, 0.50), (0.42, 0.66)]));
    cmds
}

/// A musical note.
fn note(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        rect(b, flat(INK), 0.52, 0.14, 0.07, 0.52),
        poly(b, flat(INK), &[(0.52, 0.14), (0.84, 0.22), (0.84, 0.34), (0.52, 0.26)]),
        ellipse(b, flat(accent), 0.40, 0.68, 0.15, 0.12),
    ]
}

/// A shipping box with a band across it.
fn archive(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        rect(b, flat(INK), 0.10, 0.24, 0.80, 0.60),
        rect(b, flat(darken(accent, 0.3)), 0.10, 0.24, 0.80, 0.14),
        rect(b, flat(darken(accent, 0.1)), 0.44, 0.24, 0.12, 0.60),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> (f32, f32, f32, f32) {
        (10.0, 4.0, 12.0, 5.0)
    }

    #[test]
    fn icon_box_is_square_in_pixels_and_centered() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        assert!((b.w * 9.0 - b.h * 20.0).abs() < 1e-3, "square in pixels");
        // Centered within the area on both axes.
        assert!(((b.x - 10.0) - (12.0 - b.w - (b.x - 10.0))).abs() < 1e-3);
        assert!(((b.y - 4.0) - (5.0 - b.h - (b.y - 4.0))).abs() < 1e-3);
        // And it fits.
        assert!(b.w <= 12.0 && b.h <= 5.0);
    }

    #[test]
    fn every_media_kind_draws_something() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        for m in [
            Media::Dir,
            Media::Image,
            Media::Video,
            Media::Audio,
            Media::Text,
            Media::Archive,
            Media::Binary,
        ] {
            assert!(!draw(m, b, false, false).is_empty(), "{m:?}");
        }
    }

    #[test]
    fn link_and_broken_badges_add_marks() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        let plain = draw(Media::Text, b, false, false).len();
        assert_eq!(draw(Media::Text, b, true, false).len(), plain + 1);
        assert_eq!(draw(Media::Text, b, false, true).len(), plain + 2);
    }

    #[test]
    fn shapes_stay_inside_their_box() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        for cmd in draw(Media::Image, b, true, false) {
            let pts: Vec<Point> = match cmd {
                DrawCmd::FillPolygon { points, .. } => points,
                DrawCmd::DrawLineStrip { points, .. } => points,
                DrawCmd::FillRectangles { rects, .. } => rects
                    .iter()
                    .flat_map(|r| {
                        [
                            Point { x: r.x, y: r.y },
                            Point {
                                x: r.x + r.w,
                                y: r.y + r.h,
                            },
                        ]
                    })
                    .collect(),
                _ => continue,
            };
            for p in pts {
                assert!(p.x >= b.x - 1e-3 && p.x <= b.x + b.w + 1e-3, "{p:?}");
                assert!(p.y >= b.y - 1e-3 && p.y <= b.y + b.h + 1e-3, "{p:?}");
            }
        }
    }
}
