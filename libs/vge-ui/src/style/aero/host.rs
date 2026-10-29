//! The terminal's own chrome, in Aero: glass windows, gel buttons and
//! chips — the accent's gel for the safe answer and for what is lit,
//! a red one for the destructive answer and for nothing found.

use vge_protocol::codec::Rect;
use vge_protocol::command::{Align, Color, DrawCmd, Style};

use super::{
    GEL_RED, Gel, INK, bubble, caret, field, frame_glass, frost, gel, gel_rr, panel, rims, shadow,
};
use crate::paint::{Px, hex, pill, text, white};
use crate::style::{ButtonKind, ChipKind, Dialog, FieldText, SearchPanel};

/// The navy a white label on the window glass is shadowed with.
const GLASS_SHADE: u32 = 0x06264d;

/// How far a jump label's cap reaches past its cell on either side,
/// and its corner — in pixels.
const JUMP_OVERHANG: f32 = 2.0;
const JUMP_RADIUS: f32 = 4.0;

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

pub(super) fn search_panel(cmds: &mut Vec<DrawCmd>, px: Px, p: &SearchPanel<'_>) {
    shadow(cmds, px, p.w, p.h);
    frame_glass(cmds, px, p.w, p.h);
    bubble(cmds, px, p.w - 0.9, 0.35, 3.5);

    let label_only = matches!(p.text, Some((_, _, FieldText::Label)));
    if !label_only {
        let Rect { x, y, w, h } = p.field;
        field(cmds, px, x, y, x + w, y + h);
    }
    if let Some((x, s, kind)) = p.text {
        // Dark ink in the white field; a label sits on the glass itself.
        let (ink, bold, shade) = match kind {
            FieldText::Query => (hex(INK, 1.0), false, None),
            FieldText::Placeholder => (hex(INK, 0.4), false, None),
            FieldText::Label => (white(0.95), true, Some(hex(GLASS_SHADE, 0.9))),
        };
        text(cmds, px, x, p.text_y, Align::Left, s, ink, bold, shade);
    }
    if let Some(Rect { x, y, h, .. }) = p.caret {
        caret(cmds, px, x + px.x(1.0), y, y + h);
    }
    if let Some((x, s)) = p.hint {
        let shade = Some(hex(GLASS_SHADE, 0.8));
        text(
            cmds,
            px,
            x,
            p.text_y,
            Align::Left,
            s,
            white(0.8),
            false,
            shade,
        );
    }

    for chip in p.chips {
        let Rect { x, y, w, h } = chip.rect;
        let (x1, y1) = (x + w, y + h);
        let lit = match chip.kind {
            ChipKind::Counter | ChipKind::Toggle { on: true } => {
                Some(Gel::for_color(p.inks.accent))
            }
            ChipKind::Warn => Some(GEL_RED),
            ChipKind::Toggle { on: false } => None,
        };
        let (ink, shade) = match lit {
            Some(g) => {
                gel(cmds, px, x, y, x1, y1, &g);
                (white(1.0), hex(g.shade, 0.85))
            }
            None => {
                frost(cmds, px, x, y, x1, y1);
                (white(0.6), hex(GLASS_SHADE, 0.6))
            }
        };
        let cx = x + w * 0.5;
        text(
            cmds,
            px,
            cx,
            p.text_y,
            Align::Center,
            chip.label,
            ink,
            true,
            Some(shade),
        );
    }
    rims(cmds, px, p.w, p.h);
}

/// A gel key cap: the key's colour as a gel, a little wider than its
/// cell — a one-cell capsule pinches the letter between its ends — and
/// opaque, so it hides the character under it.
pub(super) fn jump_label(cmds: &mut Vec<DrawCmd>, px: Px, x: f32, y: f32, s: &str, fill: Color) {
    let g = Gel::for_color(fill);
    let over = px.x(JUMP_OVERHANG);
    gel_rr(
        cmds,
        px,
        x - over,
        y,
        x + 1.0 + over,
        y + 1.0,
        JUMP_RADIUS,
        &g,
    );
    text(
        cmds,
        px,
        x + 0.5,
        y,
        Align::Center,
        s,
        white(1.0),
        true,
        Some(hex(g.shade, 0.85)),
    );
}
