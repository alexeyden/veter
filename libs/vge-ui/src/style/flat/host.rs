//! The terminal's own chrome: what veter drew in femtovg before the
//! engines, as commands — rounded panels on the theme's surface with
//! an accent rim, filled accent buttons and chips, outlined warm ones.

use vge_protocol::codec::Rect;
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};

use crate::paint::{Px, rr};
use crate::style::{ButtonKind, ChipKind, Dialog, FieldText, SearchPanel};

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

    let r = panel_radius(px);
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

pub(super) fn search_panel(cmds: &mut Vec<DrawCmd>, px: Px, p: &SearchPanel<'_>) {
    let inks = p.inks;
    let r = panel_radius(px);
    // It must not dim the grid the way the close prompt does — the
    // matches under it have to stay readable — so it lifts off it with
    // a soft shadow instead: nested rounded rects, largest and faintest
    // first.
    for step in (1..=3).rev() {
        let grow = step as f32 * 2.0;
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(Color {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.1,
            }),
            segments: rr(
                px,
                -px.x(grow),
                px.y(2.0 - grow),
                p.w + px.x(grow),
                p.h + px.y(2.0 + grow),
                r + grow,
            ),
        });
    }
    cmds.push(DrawCmd::OutlineFillPath {
        fill: Style::Flat(inks.surface),
        stroke: Style::Flat(inks.accent),
        line_width: px.line(2.0),
        segments: rr(px, 0.0, 0.0, p.w, p.h, r),
    });

    // The field is recessed so it reads as something you type into; a
    // label in its place leaves it flat, since a recessed box that
    // ignores the keyboard would be a lie.
    let label_only = matches!(p.text, Some((_, _, FieldText::Label)));
    if !label_only {
        let Rect { x, y, w, h } = p.field;
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(inks.surface_inset),
            segments: rr(px, x, y, x + w, y + h, r * BUTTON_R),
        });
    }
    if let Some((x, s, kind)) = p.text {
        let ink = match kind {
            FieldText::Query => inks.text,
            FieldText::Placeholder => fade(inks.text, 0.32),
            FieldText::Label => fade(inks.text, 0.75),
        };
        text(cmds, x, p.text_y, Align::Left, s, ink);
    }
    if let Some(Rect { x, y, w, h }) = p.caret {
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(lighten(inks.accent, 0.35)),
            segments: rr(px, x, y, x + w, y + h, w * px.w * 0.5),
        });
    }
    if let Some((x, s)) = p.hint {
        text(cmds, x, p.text_y, Align::Left, s, fade(inks.text, 0.4));
    }

    for chip in p.chips {
        let Rect { x, y, w, h } = chip.rect;
        let (fill, border, ink) = match chip.kind {
            ChipKind::Counter => (mix(inks.surface, inks.accent, 0.3), None, p.current_match),
            ChipKind::Warn => (inks.warn_fill, Some(inks.warn_border), inks.warn_text),
            ChipKind::Toggle { on: true } => (inks.accent, None, inks.on_accent),
            ChipKind::Toggle { on: false } => (
                inks.surface_inset,
                Some(fade(inks.text, 0.22)),
                fade(inks.text, 0.5),
            ),
        };
        let segments = rr(px, x, y, x + w, y + h, r * BUTTON_R);
        cmds.push(match border {
            Some(stroke) => DrawCmd::OutlineFillPath {
                fill: Style::Flat(fill),
                stroke: Style::Flat(stroke),
                line_width: px.line(1.0),
                segments,
            },
            None => DrawCmd::FillPath {
                fill: Style::Flat(fill),
                segments,
            },
        });
        text(cmds, x + w * 0.5, p.text_y, Align::Center, chip.label, ink);
    }
}

pub(super) fn jump_label(
    cmds: &mut Vec<DrawCmd>,
    x: f32,
    y: f32,
    s: &str,
    fill: Color,
    ink: Color,
) {
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(fill),
        rects: vec![Rect {
            x,
            y,
            w: 1.0,
            h: 1.0,
        }],
    });
    text(cmds, x, y, Align::Left, s, ink);
}

/// A slim thumb in the terminal's foreground, faded
/// ([`crate::theme::scrollbar`]) — it has to read over whatever the grid
/// holds, on a light scheme as on a dark one, which plain white did not.
pub(super) fn scroll_thumb(cmds: &mut Vec<DrawCmd>, px: Px, thumb: Rect) {
    let Rect { x, y, w, h } = thumb;
    let r = 3.0 * crate::style::settings().rounding;
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(crate::theme::scrollbar()),
        segments: rr(px, x, y, x + w, y + h, r),
    });
}

/// The panels' corner, in pixels.
fn panel_radius(px: Px) -> f32 {
    px.h * PANEL_R * crate::style::settings().rounding
}

fn label(cmds: &mut Vec<DrawCmd>, x: f32, y: f32, s: &str, color: Color) {
    text(cmds, x, y, Align::Center, s, color);
}

fn text(cmds: &mut Vec<DrawCmd>, x: f32, y: f32, align: Align, s: &str, color: Color) {
    cmds.push(DrawCmd::DrawText {
        origin: vge_protocol::codec::Point { x, y },
        align,
        fill: Style::Flat(color),
        font_style: FontStyle::default(),
        font_scale: 1.0,
        text: s.to_string(),
    });
}

/// `c` at `alpha` of its opacity.
fn fade(c: Color, alpha: f32) -> Color {
    Color {
        a: c.a * alpha,
        ..c
    }
}

/// `a` moved `t` of the way toward `b`, alpha included.
fn mix(a: Color, b: Color, t: f32) -> Color {
    let l = |x: f32, y: f32| x + (y - x) * t;
    Color {
        r: l(a.r, b.r),
        g: l(a.g, b.g),
        b: l(a.b, b.b),
        a: l(a.a, b.a),
    }
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
