//! The terminal's own chrome, in Aero: a glass window with a pale
//! panel inside it, and gel buttons — the accent's gel for the safe
//! answer, a red one for the destructive.

use vge_protocol::codec::Rect;
use vge_protocol::command::{Align, DrawCmd, Style};

use super::{GEL_RED, Gel, INK, bubble, frame_glass, gel, panel, rims, shadow};
use crate::paint::{Px, hex, pill, text, white};
use crate::style::{ButtonKind, Dialog};

pub(super) fn dialog(cmds: &mut Vec<DrawCmd>, px: Px, d: &Dialog<'_>) {
    // A navy wash rather than black, so the terminal behind reads as
    // under glass rather than switched off.
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(hex(0x001a33, 0.45)),
        rects: vec![d.screen],
    });
    shadow(cmds, px, d.w, d.h);
    frame_glass(cmds, px, d.w, d.h);
    // The frame shows round the panel as a band a third of a cell
    // wide, and as many pixels deep.
    let (inset_x, inset_y) = (0.35, px.y(0.35 * px.w));
    panel(cmds, px, inset_x, inset_y, d.w - inset_x, d.h - inset_y);
    for (cx, cy, r) in [(d.w - 1.6, 0.3, 4.0), (1.4, d.h - 0.25, 3.0)] {
        bubble(cmds, px, cx, cy, r);
    }
    text(
        cmds,
        px,
        d.w * 0.5,
        d.title_y,
        Align::Center,
        d.title,
        hex(INK, 1.0),
        true,
        None,
    );

    for b in d.buttons {
        let Rect { x, y, w, h } = b.rect;
        let g = match b.kind {
            ButtonKind::Default => Gel::for_color(d.inks.accent),
            ButtonKind::Danger => GEL_RED,
        };
        gel(cmds, px, x, y, x + w, y + h, &g);
        if b.hot {
            cmds.push(DrawCmd::FillPath {
                fill: Style::Flat(white(0.2)),
                segments: pill(px, x, y, x + w, y + h),
            });
        }
        text(
            cmds,
            px,
            x + w * 0.5,
            y + (h - 1.0) * 0.5,
            Align::Center,
            b.label,
            white(1.0),
            true,
            Some(hex(g.shade, 0.85)),
        );
    }
    rims(cmds, px, d.w, d.h);
}
