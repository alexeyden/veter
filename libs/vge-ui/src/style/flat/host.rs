//! The terminal's own chrome: what veter drew in femtovg before the
//! engines, as commands — a rounded panel on the theme's surface with
//! an accent rim, a filled accent button and an outlined warm one.

use vge_protocol::codec::Rect;
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};

use crate::paint::{Px, rr};
use crate::style::{ButtonKind, Dialog};

/// The panel's corner, as a fraction of a cell's height in pixels.
/// Its own rather than [`crate::shape::chrome_corner_radii`]'s, which
/// is a tab's; scaled by `flat.rounding` all the same.
const PANEL_R: f32 = 0.5;
/// A button's corner, against the panel's.
const BUTTON_R: f32 = 0.6;

pub(super) fn dialog(cmds: &mut Vec<DrawCmd>, px: Px, d: &Dialog<'_>) {
    let inks = d.inks;
    // The scrim dims the terminal without hiding it, so what is about
    // to be closed stays readable.
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(Color {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 150.0 / 255.0,
        }),
        rects: vec![d.screen],
    });

    let r = px.h * PANEL_R * crate::style::settings().rounding;
    cmds.push(DrawCmd::OutlineFillPath {
        fill: Style::Flat(inks.surface),
        stroke: Style::Flat(inks.accent),
        line_width: px.line(2.0),
        segments: rr(px, 0.0, 0.0, d.w, d.h, r),
    });
    label(cmds, d.w * 0.5, d.title_y, d.title, inks.text);

    for b in d.buttons {
        let Rect { x, y, w, h } = b.rect;
        let shape = rr(px, x, y, x + w, y + h, r * BUTTON_R);
        match b.kind {
            ButtonKind::Default => cmds.push(DrawCmd::FillPath {
                fill: Style::Flat(if b.hot {
                    lighten(inks.accent, 0.25)
                } else {
                    inks.accent
                }),
                segments: shape,
            }),
            ButtonKind::Danger => cmds.push(DrawCmd::OutlineFillPath {
                fill: Style::Flat(if b.hot {
                    lighten(inks.warn_fill, 0.12)
                } else {
                    inks.warn_fill
                }),
                stroke: Style::Flat(inks.warn_border),
                line_width: px.line(1.0),
                segments: shape,
            }),
        }
        let ink = match b.kind {
            ButtonKind::Default => inks.on_accent,
            ButtonKind::Danger => inks.warn_text,
        };
        label(cmds, x + w * 0.5, y + (h - 1.0) * 0.5, b.label, ink);
    }
}

fn label(cmds: &mut Vec<DrawCmd>, x: f32, y: f32, s: &str, color: Color) {
    cmds.push(DrawCmd::DrawText {
        origin: vge_protocol::codec::Point { x, y },
        align: Align::Center,
        fill: Style::Flat(color),
        font_style: FontStyle::default(),
        font_scale: 1.0,
        text: s.to_string(),
    });
}

/// `c` moved `t` of the way toward white.
fn lighten(c: Color, t: f32) -> Color {
    let l = |v: f32| v + (1.0 - v) * t;
    Color {
        r: l(c.r),
        g: l(c.g),
        b: l(c.b),
        a: c.a,
    }
}
