//! Aero file-type icons. The flat ones take the host's accent, which
//! Aero ignores everywhere else; these carry the Aero palette instead —
//! manila folders, white paper, a sky in every picture — and each sits
//! on a soft drop shadow. Shapes are laid out in the icon box's unit
//! square.

use vge_protocol::codec::Point;
use vge_protocol::command::{DrawCmd, Style};

use crate::paint::{Px, ellipse, hex, radial, vgrad, white};
use crate::style::{FileKind, IconBox};

/// A unit-square point inside the icon box.
fn up(b: &IconBox, u: f32, v: f32) -> Point {
    Point {
        x: b.x + u * b.w,
        y: b.y + v * b.h,
    }
}

fn pts(b: &IconBox, uv: &[(f32, f32)]) -> Vec<Point> {
    uv.iter().map(|&(u, v)| up(b, u, v)).collect()
}

/// The four corners of a unit-square rect.
fn quad(u: f32, v: f32, w: f32, h: f32) -> [(f32, f32); 4] {
    [(u, v), (u + w, v), (u + w, v + h), (u, v + h)]
}

/// A top-to-bottom gradient between two unit-square heights.
fn ugrad(b: &IconBox, v0: f32, v1: f32, c0: u32, c1: u32) -> Style {
    vgrad(b.y + v0 * b.h, b.y + v1 * b.h, hex(c0, 1.0), hex(c1, 1.0))
}

fn fill(cmds: &mut Vec<DrawCmd>, b: &IconBox, style: Style, uv: &[(f32, f32)]) {
    cmds.push(DrawCmd::FillPolygon {
        fill: style,
        points: pts(b, uv),
    });
}

/// A filled shape with a one-pixel rim.
fn rimmed(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox, style: Style, rim: u32, uv: &[(f32, f32)]) {
    cmds.push(DrawCmd::OutlineFillPolygon {
        fill: style,
        stroke: Style::Flat(hex(rim, 0.85)),
        line_width: px.line(1.0),
        points: pts(b, uv),
    });
}

/// A soft drop shadow under the silhouette `poly` (absolute cells):
/// a close dark layer and a wider faint one, down and to the right.
pub(super) fn shadow(cmds: &mut Vec<DrawCmd>, px: Px, poly: &[(f32, f32)]) {
    for (dx, dy, a) in [(3.0, 4.5, 0.1), (1.5, 2.5, 0.22)] {
        cmds.push(DrawCmd::FillPolygon {
            fill: Style::Flat(hex(0x001a33, a)),
            points: poly
                .iter()
                .map(|&(x, y)| Point {
                    x: x + px.x(dx),
                    y: y + px.y(dy),
                })
                .collect(),
        });
    }
}

/// [`shadow`] for a unit-square silhouette.
fn ushadow(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox, uv: &[(f32, f32)]) {
    let abs: Vec<(f32, f32)> = pts(b, uv).iter().map(|p| (p.x, p.y)).collect();
    shadow(cmds, px, &abs);
}

pub(super) fn icon(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    kind: FileKind,
    b: IconBox,
    is_link: bool,
    broken: bool,
) {
    match kind {
        FileKind::Dir => folder(cmds, px, &b),
        FileKind::Image => photo(cmds, px, &b),
        FileKind::Video => film(cmds, px, &b),
        FileKind::Audio => note(cmds, px, &b),
        FileKind::Text => page(cmds, px, &b, true),
        FileKind::Archive => parcel(cmds, px, &b),
        FileKind::Binary => page(cmds, px, &b, false),
    }
    if broken {
        for line in [[(0.15, 0.15), (0.85, 0.85)], [(0.85, 0.15), (0.15, 0.85)]] {
            cmds.push(DrawCmd::DrawLineStrip {
                stroke: Style::Flat(hex(0xd93025, 0.95)),
                line_width: 0.08,
                points: pts(&b, &line),
            });
        }
    } else if is_link {
        // The shortcut corner, as a little aqua gel.
        let corner = [(0.62, 0.98), (0.98, 0.98), (0.98, 0.62)];
        rimmed(
            cmds,
            px,
            &b,
            ugrad(&b, 0.62, 0.98, 0x6fd3f7, 0x0a86d8),
            0xffffff,
            &corner,
        );
    }
}

/// A manila folder: the back leaf with its tab, a glossy front leaf.
fn folder(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox) {
    let back = [
        (0.04, 0.20),
        (0.42, 0.20),
        (0.50, 0.30),
        (0.96, 0.30),
        (0.96, 0.88),
        (0.04, 0.88),
    ];
    ushadow(cmds, px, b, &back);
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.20, 0.88, 0xe8b84a, 0xc98e1f),
        0x9a6a14,
        &back,
    );
    let front = [(0.04, 0.40), (0.96, 0.40), (0.88, 0.88), (0.12, 0.88)];
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.40, 0.88, 0xffe58f, 0xf2b636),
        0xa8761a,
        &front,
    );
    let sheen = [(0.06, 0.42), (0.94, 0.42), (0.92, 0.57), (0.08, 0.57)];
    let gloss = vgrad(
        up(b, 0.0, 0.42).y,
        up(b, 0.0, 0.57).y,
        white(0.55),
        white(0.05),
    );
    fill(cmds, b, gloss, &sheen);
}

/// A sheet of paper with a folded corner; `ruled` adds text lines.
fn page(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox, ruled: bool) {
    let body = [
        (0.16, 0.08),
        (0.66, 0.08),
        (0.84, 0.28),
        (0.84, 0.92),
        (0.16, 0.92),
    ];
    ushadow(cmds, px, b, &body);
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.08, 0.92, 0xffffff, 0xe3edf6),
        0x8aa4bd,
        &body,
    );
    let fold = [(0.66, 0.08), (0.84, 0.28), (0.66, 0.28)];
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.08, 0.28, 0xd8e4ef, 0xa9bfd4),
        0x8aa4bd,
        &fold,
    );
    if ruled {
        for (i, v) in [0.44_f32, 0.58, 0.72].iter().enumerate() {
            let right = if i == 2 { 0.58 } else { 0.72 };
            fill(
                cmds,
                b,
                Style::Flat(hex(0x7f9bb8, 1.0)),
                &quad(0.26, *v, right - 0.26, 0.06),
            );
        }
    }
}

/// A framed picture of the Aero sky itself: blue over a green hill,
/// with a sun.
fn photo(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox) {
    let frame = quad(0.08, 0.16, 0.84, 0.68);
    ushadow(cmds, px, b, &frame);
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.16, 0.84, 0xffffff, 0xdde8f2),
        0x8aa4bd,
        &frame,
    );
    fill(
        cmds,
        b,
        ugrad(b, 0.22, 0.78, 0x2f86d6, 0xa9e0ff),
        &quad(0.14, 0.22, 0.72, 0.56),
    );
    let hill = [
        (0.14, 0.78),
        (0.42, 0.50),
        (0.60, 0.66),
        (0.72, 0.56),
        (0.86, 0.78),
    ];
    fill(cmds, b, ugrad(b, 0.50, 0.78, 0x9be15d, 0x2f9e44), &hill);
    let sun = up(b, 0.32, 0.38);
    let (rx, ry) = (0.07 * b.w, 0.07 * b.h);
    cmds.push(DrawCmd::FillPath {
        fill: radial(
            Point {
                x: sun.x - rx * 0.3,
                y: sun.y - ry * 0.3,
            },
            Point {
                x: sun.x + rx,
                y: sun.y,
            },
            hex(0xfff6b0, 1.0),
            hex(0xffc21a, 1.0),
        ),
        segments: ellipse(sun.x, sun.y, rx, ry),
    });
    let sheen = vgrad(
        up(b, 0.0, 0.22).y,
        up(b, 0.0, 0.46).y,
        white(0.35),
        white(0.0),
    );
    fill(cmds, b, sheen, &quad(0.14, 0.22, 0.72, 0.24));
}

/// A strip of film with an aqua play button.
fn film(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox) {
    let body = quad(0.08, 0.18, 0.84, 0.64);
    ushadow(cmds, px, b, &body);
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.18, 0.82, 0x3a3f4a, 0x15181e),
        0x0b0d10,
        &body,
    );
    fill(
        cmds,
        b,
        ugrad(b, 0.18, 0.82, 0x1c5f9e, 0x0b2f55),
        &quad(0.22, 0.18, 0.56, 0.64),
    );
    for i in 0..3 {
        let v = 0.26 + i as f32 * 0.20;
        for u in [0.11, 0.81] {
            fill(cmds, b, Style::Flat(white(0.8)), &quad(u, v, 0.08, 0.10));
        }
    }
    let play = [(0.42, 0.34), (0.66, 0.50), (0.42, 0.66)];
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.34, 0.66, 0xb4f4ff, 0x0a86d8),
        0x0b4f8f,
        &play,
    );
}

/// A glossy blue musical note.
fn note(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox) {
    let stem = quad(0.52, 0.14, 0.07, 0.52);
    let flag = [(0.52, 0.14), (0.84, 0.22), (0.84, 0.34), (0.52, 0.26)];
    let head = up(b, 0.40, 0.68);
    let (rx, ry) = (0.15 * b.w, 0.12 * b.h);
    ushadow(cmds, px, b, &stem);
    ushadow(cmds, px, b, &flag);
    for (dx, dy, a) in [(3.0, 4.5, 0.1), (1.5, 2.5, 0.22)] {
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(hex(0x001a33, a)),
            segments: ellipse(head.x + px.x(dx), head.y + px.y(dy), rx, ry),
        });
    }
    let blue = ugrad(b, 0.14, 0.80, 0x5ab8ef, 0x0b5fc4);
    fill(cmds, b, blue.clone(), &stem);
    fill(cmds, b, blue, &flag);
    cmds.push(DrawCmd::FillPath {
        fill: radial(
            Point {
                x: head.x - rx * 0.35,
                y: head.y - ry * 0.35,
            },
            Point {
                x: head.x + rx,
                y: head.y + ry,
            },
            hex(0x8fd0ff, 1.0),
            hex(0x0b5fc4, 1.0),
        ),
        segments: ellipse(head.x, head.y, rx, ry),
    });
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(white(0.7)),
        segments: ellipse(head.x - rx * 0.35, head.y - ry * 0.4, rx * 0.35, ry * 0.25),
    });
}

/// A cardboard box, taped shut.
fn parcel(cmds: &mut Vec<DrawCmd>, px: Px, b: &IconBox) {
    let body = quad(0.10, 0.24, 0.80, 0.60);
    ushadow(cmds, px, b, &body);
    rimmed(
        cmds,
        px,
        b,
        ugrad(b, 0.24, 0.84, 0xd9a86c, 0xa8733a),
        0x6e4a22,
        &body,
    );
    fill(
        cmds,
        b,
        ugrad(b, 0.24, 0.38, 0xc48f52, 0x9a6630),
        &quad(0.10, 0.24, 0.80, 0.14),
    );
    fill(
        cmds,
        b,
        Style::Flat(hex(0xf1e3b5, 0.9)),
        &quad(0.44, 0.24, 0.12, 0.60),
    );
}
