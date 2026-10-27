//! Frutiger Aero chrome for vmux's own surfaces — the tab bar, pane
//! titles, separators and scrollbars, plus which Aero modal each mode
//! puts up. The primitives, palette, modals and the style switch are
//! shared with `vfm` and live in `vge_ui::aero`; see there for how the
//! style is selected and how to drop it.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, CreateElementBody, DrawCmd, OriginAnchor, Style};
use vge_ui::aero::*;
use vge_ui::measure::text_cells;

use super::{
    CHROME_DRAW_ORDER, MODAL_DRAW_ORDER, MODAL_ELEMENT_ID, MODAL_IDS, Mode, PaneRect, RenameTarget,
    ScrollIndicator, State, TABBAR_CHEVRON_W, TABBAR_ELEMENT_ID, TabbarLayout, help_modal,
};

/// The tab bar element — the Aero stand-in for `build_tabbar_commands`,
/// drawn over the same [`TabbarLayout`] so clicks still land where the
/// tabs are painted.
pub fn tabbar(
    layout: &TabbarLayout,
    active: usize,
    host_w: f32,
    px: Px,
    activity: &[bool],
) -> CreateElementBody {
    CreateElementBody {
        id: TABBAR_ELEMENT_ID.to_string(),
        commands: tabbar_commands(layout, active, host_w, px, activity),
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: CHROME_DRAW_ORDER,
        parent: None,
        size: None,
        transform: None,
        anchor: OriginAnchor::Viewport,
    }
}

fn tabbar_commands(
    layout: &TabbarLayout,
    active: usize,
    host_w: f32,
    px: Px,
    activity: &[bool],
) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();

    // The sky: a deep-to-light aqua bar with the glass sheen across its
    // upper half, a bright lip along the bottom and a glow spilling down
    // over the panes.
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, 1.0, hex(0x5ab8ef, 0.97), hex(0x1468b4, 0.97)),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w: host_w,
            h: 1.0,
        }],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(0.0, 0.5, white(0.28), white(0.04)),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w: host_w,
            h: 0.5,
        }],
    });
    let lip = px.y(1.0);
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(hex(0xc8f6ff, 0.95)),
        rects: vec![Rect {
            x: 0.0,
            y: 1.0 - lip,
            w: host_w,
            h: lip,
        }],
    });
    cmds.push(DrawCmd::FillRectangles {
        fill: vgrad(1.0, 1.45, hex(0x3fb8ff, 0.55), hex(0x3fb8ff, 0.0)),
        rects: vec![Rect {
            x: 0.0,
            y: 1.0,
            w: host_w,
            h: 0.45,
        }],
    });

    // Bubbles in the free stretch after the last tab, some rising out
    // of the glow. Seeded by absolute column so they hold still while
    // tabs come and go.
    let free_x0 = layout
        .slots
        .last()
        .map(|s| s.x0 + s.num_w + s.name_w)
        .unwrap_or(layout.session_w())
        + 1.0;
    let free_x1 = host_w
        - if layout.has_right {
            TABBAR_CHEVRON_W
        } else {
            0.0
        }
        - 0.5;
    for col in 0..host_w as u32 {
        let h = hash(col.wrapping_add(0x5eed));
        if !h.is_multiple_of(3) {
            continue;
        }
        let r = 3.5 + unit(h >> 3) * 7.5;
        let cx = col as f32 + unit(h >> 7);
        let cy = 0.2 + unit(h >> 11) * 1.0;
        if cx - px.x(r) < free_x0 || cx + px.x(r) > free_x1 {
            continue;
        }
        bubble(&mut cmds, px, cx, cy, r);
    }

    let navy = hex(0x06264d, 0.75);
    if let Some((label, w)) = &layout.session_seg {
        let (x0, x1, y0, y1) = (0.1, *w - 0.1, 0.1, 0.92);
        cmds.push(DrawCmd::OutlineFillPath {
            fill: vgrad(y0, y1, white(1.0), hex(0x9fb3c8, 1.0)),
            stroke: Style::Flat(hex(0x5f7a96, 0.9)),
            line_width: px.line(1.0),
            segments: pill(px, x0, y0, x1, y1),
        });
        gloss(&mut cmds, px, x0, y0, x1, y1, 0.5);
        text(
            &mut cmds,
            px,
            0.0,
            0.0,
            Align::Left,
            &format!(" {label} "),
            hex(0x12345a, 1.0),
            true,
            Some(white(0.7)),
        );
    }

    if layout.has_left {
        text(
            &mut cmds,
            px,
            layout.session_w(),
            0.0,
            Align::Left,
            " ‹",
            white(1.0),
            true,
            Some(navy),
        );
    }

    for slot in &layout.slots {
        let i = slot.index;
        let x = slot.x0;
        let (x0, x1, y0, y1) = (x + 0.1, x + slot.num_w + slot.name_w - 0.1, 0.1, 0.92);
        let is_active = i == active;
        let has_activity = activity.get(i).copied().unwrap_or(false);
        let shadow = if is_active {
            gel(&mut cmds, px, x0, y0, x1, y1, &GEL_GREEN);
            hex(0x154a0c, 0.85)
        } else if has_activity {
            gel(&mut cmds, px, x0, y0, x1, y1, &GEL_AMBER);
            hex(0x6b3300, 0.85)
        } else {
            frost(&mut cmds, px, x0, y0, x1, y1);
            navy
        };

        // The number rides in an orb at the head of the pill; " n " is
        // centred over `num_w`, and so is the orb.
        orb(&mut cmds, px, x + 0.35, 0.18, x + slot.num_w - 0.35, 0.84);
        text(
            &mut cmds,
            px,
            x,
            0.0,
            Align::Left,
            &format!(" {} ", i + 1),
            white(1.0),
            true,
            Some(hex(0x06264d, 0.9)),
        );
        text(
            &mut cmds,
            px,
            x + slot.num_w,
            0.0,
            Align::Left,
            &format!(" {} ", slot.label),
            white(1.0),
            is_active,
            Some(shadow),
        );
    }

    if layout.has_right {
        text(
            &mut cmds,
            px,
            host_w - TABBAR_CHEVRON_W,
            0.0,
            Align::Left,
            "› ",
            white(1.0),
            true,
            Some(navy),
        );
    }
    cmds
}

/// A pane's chrome — the Aero stand-in for `build_chrome_commands`,
/// with the same title and scrollbar geometry.
pub fn pane_chrome(
    rect: PaneRect,
    title: &str,
    focused: bool,
    show_title: bool,
    px: Px,
    scroll: Option<ScrollIndicator>,
) -> Vec<DrawCmd> {
    let pw = rect.w as f32;
    let ph = rect.h as f32;
    let mut cmds = Vec::new();
    if show_title && !title.is_empty() {
        let text_x = pw - 1.0;
        let y0 = 0.25;
        let x0 = text_x - text_cells(title) - 0.5;
        pane_title(
            &mut cmds,
            px,
            x0,
            y0,
            text_x + 0.5,
            y0 + 1.0,
            text_x,
            title,
            focused,
        );
    }
    if let Some(s) = scroll {
        let portal_rows = ph.max(1.0);
        let total = portal_rows + s.history_depth as f32;
        let track_y0 = 1.25;
        let track_y1 = (ph - 0.5).max(track_y0 + 1.0);
        let track_h = (track_y1 - track_y0).max(1.0);
        let thumb_h = ((portal_rows / total).clamp(0.05, 1.0) * track_h).max(0.6);
        let available = (track_h - thumb_h).max(0.0);
        // 0 offset = bottom (live); offset = history_depth = top.
        let thumb_norm = if s.history_depth == 0 {
            1.0
        } else {
            (s.history_depth.saturating_sub(s.offset) as f32) / s.history_depth as f32
        };
        let thumb_y = track_y0 + thumb_norm * available;
        scroll_thumb(&mut cmds, px, pw - 1.30, thumb_y, 0.35, thumb_h);
    }
    cmds
}

/// The pane title: an aqua gel capsule on the focused pane, smoked
/// glass elsewhere. `x0..x1` × `y0..y1` is the thumb; the text is
/// right-aligned at `text_x`.
#[allow(clippy::too_many_arguments)]
fn pane_title(
    cmds: &mut Vec<DrawCmd>,
    px: Px,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    text_x: f32,
    title: &str,
    focused: bool,
) {
    if focused {
        gel(cmds, px, x0, y0, x1, y1, &GEL_AQUA);
    } else {
        smoke(cmds, px, x0, y0, x1, y1);
    }
    text(
        cmds,
        px,
        text_x,
        y0,
        Align::Right,
        title,
        white(1.0),
        focused,
        Some(hex(0x06264d, 0.9)),
    );
}

/// Pane separators: each stock rule becomes a soft aqua glow under a
/// bright core. Takes the stock commands, so the separator geometry
/// stays `build_separators_body`'s alone.
pub fn restyle_separators(cmds: &mut Vec<DrawCmd>) {
    if !enabled() {
        return;
    }
    let mut out = Vec::with_capacity(cmds.len() * 2);
    for cmd in cmds.drain(..) {
        match cmd {
            DrawCmd::DrawLines { lines, .. } => {
                out.push(DrawCmd::DrawLines {
                    stroke: Style::Flat(hex(0x3fb8ff, 0.25)),
                    line_width: 0.35,
                    lines: lines.clone(),
                });
                out.push(DrawCmd::DrawLines {
                    stroke: Style::Flat(hex(0xaef0ff, 0.95)),
                    line_width: 0.1,
                    lines,
                });
            }
            other => out.push(other),
        }
    }
    *cmds = out;
}

/// The modal for whatever the current mode wants up — the Aero stand-in
/// for `State::build_modal_elements`.
pub fn modal_elements(state: &State) -> Vec<CreateElementBody> {
    let px = Px {
        w: state.cell_pw,
        h: state.cell_ph,
    };
    let (w, h) = (state.host_w, state.host_h);
    match &state.mode {
        Mode::Rename { target, editor } => {
            let title = match target {
                RenameTarget::Pane(_) => "Rename pane",
                RenameTarget::Tab(_) => "Rename tab",
            };
            vec![prompt_element(
                MODAL_ELEMENT_ID,
                w,
                h,
                title,
                &editor.buffer,
                px,
                Some(editor.cursor),
                MODAL_DRAW_ORDER,
            )]
        }
        Mode::Help { offset, .. } => {
            scroll_elements(help_modal(), MODAL_IDS, w, h, *offset, px, MODAL_DRAW_ORDER)
        }
        Mode::ConfirmQuit => vec![prompt_element(
            MODAL_ELEMENT_ID,
            w,
            h,
            "Quit vmux?",
            "y = quit     n = cancel",
            px,
            None,
            MODAL_DRAW_ORDER,
        )],
        Mode::Picker(picker) => vec![picker_element(
            MODAL_ELEMENT_ID,
            w,
            h,
            &picker.ui,
            px,
            MODAL_DRAW_ORDER,
        )],
        Mode::Normal | Mode::Prefix | Mode::Resize { .. } => Vec::new(),
    }
}
