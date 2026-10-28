//! Flat file-type icons, drawn as VGE vectors in the host theme's
//! accent and text colours.
//!
//! Every icon is composed inside a unit box and scaled into the tile's
//! picture area, so a tile looks the same at every zoom level. Cell
//! anisotropy is handled by the caller passing the box it wants filled;
//! the shapes themselves are laid out in that box's own coordinates.

use vge_protocol::codec::Point;
use vge_protocol::command::{Color, DrawCmd, Style};
use vge_protocol::path::{PathNode, PathSegment};

use crate::style::{FileKind, IconBox};
use crate::theme::{self, accent_color, darken};

/// Primary icon stroke/fill on a terminal that publishes no theme —
/// light enough to read on the dark tile.
const INK: Color = Color {
    r: 0.72,
    g: 0.76,
    b: 0.84,
    a: 1.0,
};
/// Secondary detail (text ruling, film sprockets), likewise.
const INK_DIM: Color = Color {
    r: 0.46,
    g: 0.49,
    b: 0.57,
    a: 1.0,
};

/// Icon ink: the host's text colours when it publishes them, which is
/// what reads on the host-surface tile under a light theme too.
fn ink() -> Color {
    if theme::host_bg().is_some() {
        theme::title_text()
    } else {
        INK
    }
}

fn ink_dim() -> Color {
    if theme::host_bg().is_some() {
        theme::dim_text()
    } else {
        INK_DIM
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

/// The icon for `kind`, drawn into `b`. `is_link` adds the little
/// shortcut arrow every file manager puts on a symlink.
pub(super) fn draw(kind: FileKind, b: IconBox, is_link: bool, broken: bool) -> Vec<DrawCmd> {
    let mut cmds = match kind {
        FileKind::Dir => folder(&b),
        FileKind::Image => picture(&b),
        FileKind::Video => film(&b),
        FileKind::Audio => note(&b),
        FileKind::Text => page(&b, true),
        FileKind::Archive => archive(&b),
        FileKind::Binary => page(&b, false),
    };
    if broken {
        cmds.push(strip(
            &b,
            flat(ink_dim()),
            0.08,
            &[(0.15, 0.15), (0.85, 0.85)],
        ));
        cmds.push(strip(
            &b,
            flat(ink_dim()),
            0.08,
            &[(0.85, 0.15), (0.15, 0.85)],
        ));
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
            flat(ink()),
            &[
                (0.16, 0.08),
                (0.66, 0.08),
                (0.84, 0.28),
                (0.84, 0.92),
                (0.16, 0.92),
            ],
        ),
        // The fold, in the tile's own shade so it reads as a crease.
        poly(
            b,
            flat(ink_dim()),
            &[(0.66, 0.08), (0.84, 0.28), (0.66, 0.28)],
        ),
    ];
    if ruled {
        for (i, v) in [0.44_f32, 0.58, 0.72].iter().enumerate() {
            let right = if i == 2 { 0.58 } else { 0.72 };
            cmds.push(rect(b, flat(ink_dim()), 0.26, *v, right - 0.26, 0.06));
        }
    }
    cmds
}

/// A picture: framed panel with a horizon, a hill and a sun.
fn picture(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        rect(b, flat(ink()), 0.08, 0.16, 0.84, 0.68),
        rect(b, flat(darken(accent, 0.55)), 0.14, 0.22, 0.72, 0.56),
        ellipse(b, flat(accent), 0.32, 0.38, 0.07, 0.07),
        poly(
            b,
            flat(ink()),
            &[
                (0.14, 0.78),
                (0.42, 0.46),
                (0.60, 0.66),
                (0.72, 0.54),
                (0.86, 0.78),
            ],
        ),
    ]
}

/// A film frame: panel with sprocket holes down both edges.
fn film(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    let mut cmds = vec![
        rect(b, flat(ink()), 0.08, 0.18, 0.84, 0.64),
        rect(b, flat(darken(accent, 0.6)), 0.22, 0.18, 0.56, 0.64),
    ];
    for i in 0..3 {
        let v = 0.26 + i as f32 * 0.20;
        cmds.push(rect(b, flat(darken(accent, 0.6)), 0.11, v, 0.08, 0.10));
        cmds.push(rect(b, flat(darken(accent, 0.6)), 0.81, v, 0.08, 0.10));
    }
    // Play triangle.
    cmds.push(poly(
        b,
        flat(accent),
        &[(0.42, 0.34), (0.66, 0.50), (0.42, 0.66)],
    ));
    cmds
}

/// A musical note.
fn note(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        rect(b, flat(ink()), 0.52, 0.14, 0.07, 0.52),
        poly(
            b,
            flat(ink()),
            &[(0.52, 0.14), (0.84, 0.22), (0.84, 0.34), (0.52, 0.26)],
        ),
        ellipse(b, flat(accent), 0.40, 0.68, 0.15, 0.12),
    ]
}

/// A shipping box with a band across it.
fn archive(b: &IconBox) -> Vec<DrawCmd> {
    let accent = accent_color();
    vec![
        rect(b, flat(ink()), 0.10, 0.24, 0.80, 0.60),
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
    fn every_media_kind_draws_something() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        for m in FileKind::ALL {
            assert!(!draw(m, b, false, false).is_empty(), "{m:?}");
        }
    }

    #[test]
    fn link_and_broken_badges_add_marks() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        let plain = draw(FileKind::Text, b, false, false).len();
        assert_eq!(draw(FileKind::Text, b, true, false).len(), plain + 1);
        assert_eq!(draw(FileKind::Text, b, false, true).len(), plain + 2);
    }

    #[test]
    fn shapes_stay_inside_their_box() {
        let b = IconBox::centered(area(), 0.8, 9.0, 20.0);
        for cmd in draw(FileKind::Image, b, true, false) {
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
