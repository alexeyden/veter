//! Frutiger Aero for vfm: a sky-and-hills wallpaper with a sun glow and
//! drifting bubbles, frosted glass panes it shows through, the light-blue
//! selection box of a Win7 explorer, amber gel marks and a glass status
//! bar. The primitives, palette, modals and the style switch are shared
//! with `vmux` and live in `vge_ui::aero`; see there for how the style
//! is selected and how to drop it.
//!
//! A child of `render`, so it reuses that module's geometry (the column
//! scroll window, the picture fit) rather than restating it: a click
//! must land on the row it looks like it lands on in either style.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, CreateElementBody, DrawCmd, Style};
use vge_ui::aero::{
    GEL_AMBER, INK, Px, bubble, ellipse, gel, hash, hex, picker_element, prompt_element, radial,
    rr, scroll_elements, scroll_thumb, text, track, unit, vgrad, white,
};
use vge_ui::measure::text_cells;

use super::{ColumnView, GridView, StatusView, column_window, elide, elide_front, picture};
use crate::layout::{Layout, View};
use crate::{App, MODAL_IDS, Mode, ORDER_MODAL};

/// Directory names, a step bluer than file names.
const DIR_INK: u32 = 0x0a5fa8;
const DIM_INK: u32 = 0x4a6f93;
const ERR_INK: u32 = 0xb3261e;
/// Corner radius of a glass pane, in pixels.
const PANE_R: f32 = 8.0;

/// The wallpaper: a deep-to-pale sky, two green hills along the bottom,
/// a sun glow in the top right and bubbles scattered over it all. The
/// panes are glass, so it shows through them faintly.
pub fn backdrop(l: &Layout, px: Px) -> Vec<DrawCmd> {
    let (w, h) = (l.cols as f32, l.rows as f32);
    let mut cmds = vec![DrawCmd::FillRectangles {
        fill: vgrad(0.0, h, hex(0x0b3f80, 1.0), hex(0x4aa2e0, 1.0)),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }],
    }];
    // The far hill first, so the near one overlaps it.
    for (cx, cy, rx, ry, top, bottom) in [
        (w * 0.12, h * 1.22, w * 0.6, h * 0.4, 0x8fd95a, 0x23863a),
        (w * 0.74, h * 1.2, w * 0.75, h * 0.44, 0xa6e86a, 0x2f9e44),
    ] {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(cy - ry, h, hex(top, 1.0), hex(bottom, 1.0)),
            segments: ellipse(cx, cy, rx, ry),
        });
        // A sheen along the crest, as if the grass were lacquered.
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(cy - ry, cy - ry * 0.7, white(0.35), white(0.0)),
            segments: ellipse(cx, cy, rx * 0.97, ry * 0.97),
        });
    }
    let sun = Point {
        x: w * 0.86,
        y: h * 0.1,
    };
    cmds.push(DrawCmd::FillRectangles {
        fill: radial(
            sun,
            Point {
                x: sun.x + px.x(260.0),
                y: sun.y,
            },
            white(0.45),
            white(0.0),
        ),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        }],
    });
    // Bubbles, seeded by index so a redraw keeps them where they were.
    let count = ((w * h) / 45.0) as u32;
    for i in 0..count {
        let k = hash(i.wrapping_mul(2654435761).wrapping_add(0xb0b));
        let cx = unit(k) * w;
        let cy = unit(k >> 9) * h * 0.85;
        let r = 4.0 + unit(k >> 17) * unit(k >> 3) * 18.0;
        bubble(&mut cmds, px, cx, cy, r);
    }
    cmds
}

/// A frosted glass pane: a milky body the wallpaper shows through, a
/// sheen along its top, a bright inner rim and a cool outer one.
fn glass_pane(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xe6f2fb, 0.62), hex(0xcfe4f5, 0.5)),
        stroke: Style::Flat(white(0.9)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, PANE_R),
    });
    let (sx0, sx1, sy0) = (x0 + px.x(2.0), x1 - px.x(2.0), y0 + px.y(1.5));
    if sx1 > sx0 {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(sy0, sy0 + 0.9, white(0.45), white(0.0)),
            segments: rr(px, sx0, sy0, sx1, (sy0 + 0.9).min(y1), PANE_R - 2.0),
        });
    }
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(hex(0x2a6a9a, 0.45)),
        line_width: px.line(1.0),
        segments: rr(
            px,
            x0 - px.x(1.0),
            y0 - px.y(1.0),
            x1 + px.x(1.0),
            y1 + px.y(1.0),
            PANE_R + 1.0,
        ),
    });
}

/// The Win7 explorer selection: a pale blue glossy box with a crisp
/// rim and a white inner line.
fn select_box(cmds: &mut Vec<DrawCmd>, px: Px, x0: f32, y0: f32, x1: f32, y1: f32) {
    cmds.push(DrawCmd::OutlineFillPath {
        fill: vgrad(y0, y1, hex(0xf4faff, 1.0), hex(0xb3daf9, 1.0)),
        stroke: Style::Flat(hex(0x2f7fd0, 1.0)),
        line_width: px.line(1.0),
        segments: rr(px, x0, y0, x1, y1, 3.0),
    });
    cmds.push(DrawCmd::DrawLinePath {
        stroke: Style::Flat(white(0.75)),
        line_width: px.line(1.0),
        segments: rr(
            px,
            x0 + px.x(1.0),
            y0 + px.y(1.0),
            x1 - px.x(1.0),
            y1 - px.y(1.0),
            2.0,
        ),
    });
}

fn ink(rgb: u32) -> Color {
    hex(rgb, 1.0)
}

pub fn column_commands(v: &ColumnView, px: Px) -> Vec<DrawCmd> {
    let a = v.area;
    let mut cmds = Vec::new();
    glass_pane(
        &mut cmds,
        px,
        a.x + 0.25,
        a.y + 0.1,
        a.x + a.w - 0.25,
        a.y + a.h - 0.1,
    );

    if let Some(err) = v.error {
        let s = elide(err, a.w as usize - 2);
        text(
            &mut cmds,
            px,
            a.x + 1.0,
            a.y + 1.0,
            Align::Left,
            &s,
            ink(DIM_INK),
            false,
            None,
        );
        return cmds;
    }

    // Row geometry is `render::column_commands`'s, which `column_row_at`
    // hit-tests against.
    let rows = (a.h as usize).saturating_sub(1).max(1);
    let start = column_window(rows, v.entries.len(), v.selected);
    let text_w = (a.w as usize).saturating_sub(3).max(4);
    for (row, e) in v.entries.iter().skip(start).take(rows).enumerate() {
        let y = a.y + 0.5 + row as f32;
        let is_sel = v.selected == Some(start + row);
        if is_sel {
            select_box(
                &mut cmds,
                px,
                a.x + 0.5,
                y + 0.05,
                a.x + a.w - 0.5,
                y + 0.95,
            );
        }
        let (label, color) = if e.is_dir() {
            let s = format!("{}/", elide(&e.name, text_w.saturating_sub(1)));
            (s, ink(DIR_INK))
        } else {
            (elide(&e.name, text_w), ink(INK))
        };
        text(
            &mut cmds,
            px,
            a.x + 1.0,
            y,
            Align::Left,
            &label,
            color,
            is_sel,
            None,
        );
    }
    if v.entries.is_empty() {
        text(
            &mut cmds,
            px,
            a.x + 1.0,
            a.y + 0.5,
            Align::Left,
            "empty",
            ink(DIM_INK),
            false,
            None,
        );
    }
    cmds
}

pub fn grid_commands(v: &GridView) -> Vec<DrawCmd> {
    let px = Px {
        w: v.cell_pw,
        h: v.cell_ph,
    };
    let l = v.layout;
    let a = l.grid;
    let mut cmds = Vec::new();
    glass_pane(
        &mut cmds,
        px,
        a.x,
        a.y + 0.1,
        a.x + a.w - 0.25,
        a.y + a.h - 0.1,
    );

    if let Some(err) = v.error {
        let s = elide(err, a.w as usize - 4);
        text(
            &mut cmds,
            px,
            a.x + 2.0,
            a.y + 1.0,
            Align::Left,
            &s,
            ink(ERR_INK),
            true,
            None,
        );
        return cmds;
    }
    if v.entries.is_empty() {
        let (cx, cy) = (a.x + a.w * 0.5, a.y + a.h * 0.5);
        let msg = "empty directory";
        text(
            &mut cmds,
            px,
            cx,
            cy,
            Align::Center,
            msg,
            ink(DIM_INK),
            false,
            None,
        );
        return cmds;
    }

    let first = v.scroll_row * l.grid_cols;
    let last = (first + l.page()).min(v.entries.len());
    for index in first..last {
        match l.view {
            View::Grid => tile(&mut cmds, v, px, index),
            View::List => row(&mut cmds, v, px, index),
        }
    }

    let total_rows = v.entries.len().div_ceil(l.grid_cols);
    if total_rows > l.grid_rows {
        let track_h = a.h - 1.0;
        let thumb_h = (track_h * l.grid_rows as f32 / total_rows as f32).max(1.0);
        let max_scroll = (total_rows - l.grid_rows) as f32;
        let frac = if max_scroll > 0.0 {
            v.scroll_row as f32 / max_scroll
        } else {
            0.0
        };
        let x = a.x + a.w - 0.75;
        track(&mut cmds, px, x, a.y + 0.5, track_h);
        let y = a.y + 0.5 + (track_h - thumb_h) * frac;
        scroll_thumb(&mut cmds, px, x + 0.04, y + 0.05, 0.3, thumb_h - 0.1);
    }
    cmds
}

/// One tile: a faint glass plate, or the selection box under the
/// cursor; the picture; an amber gel badge when marked; the label.
fn tile(cmds: &mut Vec<DrawCmd>, v: &GridView, px: Px, index: usize) {
    let l = v.layout;
    let e = &v.entries[index];
    let (x, y) = l.tile_origin(index, v.scroll_row);
    let selected = index == v.cursor;
    let (x1, y1) = (x + l.tile_w, y + l.tile_h);
    if selected {
        select_box(cmds, px, x, y, x1, y1);
    } else {
        cmds.push(DrawCmd::FillPath {
            fill: vgrad(y, y1, white(0.22), white(0.06)),
            segments: rr(px, x, y, x1, y1, 4.0),
        });
    }
    cmds.extend(picture(
        v,
        e,
        (x + 0.25, y + 0.15, l.tile_w - 0.5, l.tile_img_h - 0.15),
        0.66,
    ));
    if v.marks.contains(&e.path) {
        gel(cmds, px, x + 0.3, y + 0.2, x + 1.5, y + 0.85, &GEL_AMBER);
    }
    let label_w = (l.tile_w as usize).saturating_sub(1).max(3);
    let label = elide(&e.name, label_w);
    let color = ink(if e.is_dir() { DIR_INK } else { INK });
    let lx = x + l.tile_w * 0.5;
    text(
        cmds,
        px,
        lx,
        y + l.tile_img_h,
        Align::Center,
        &label,
        color,
        selected,
        None,
    );
}

/// One list row, after `render::row_commands`: the same columns, with
/// faint blue striping and the selection box.
fn row(cmds: &mut Vec<DrawCmd>, v: &GridView, px: Px, index: usize) {
    use super::{LIST_DATE_COLS, LIST_DATE_MIN_W, LIST_SIZE_COLS, LIST_SIZE_MIN_W};
    let l = v.layout;
    let e = &v.entries[index];
    let (x, y) = l.tile_origin(index, v.scroll_row);
    let w = l.tile_w;
    let selected = index == v.cursor;
    if selected {
        select_box(cmds, px, x + 0.15, y, x + w, y + l.tile_h);
    } else if index % 2 == 1 {
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(hex(0xcfe5f7, 0.4)),
            segments: rr(px, x + 0.15, y, x + w, y + l.tile_h, 3.0),
        });
    }
    if v.marks.contains(&e.path) {
        gel(
            cmds,
            px,
            x + 0.2,
            y + 0.1,
            x + 0.55,
            y + l.tile_h - 0.1,
            &GEL_AMBER,
        );
    }

    let icon_w = (l.tile_h * v.cell_ph / v.cell_pw.max(1.0)).max(1.0);
    let icon_x = x + 0.7;
    cmds.extend(picture(
        v,
        e,
        (icon_x, y + 0.05, icon_w, l.tile_h - 0.1),
        0.82,
    ));

    let ty = y + (l.tile_h - 1.0) * 0.5;
    let meta = ink(DIM_INK);
    let mut right = x + w - 0.5;
    if w >= LIST_DATE_MIN_W {
        text(
            cmds,
            px,
            right,
            ty,
            Align::Right,
            &e.mtime_label(),
            meta,
            false,
            None,
        );
        right -= LIST_DATE_COLS;
    }
    if w >= LIST_SIZE_MIN_W {
        text(
            cmds,
            px,
            right,
            ty,
            Align::Right,
            &e.size_label(),
            meta,
            false,
            None,
        );
        right -= LIST_SIZE_COLS;
    }
    let name_x = icon_x + icon_w + 0.6;
    let name_w = ((right - name_x - 1.0).max(4.0)) as usize;
    let (label, color) = if e.is_dir() {
        let s = format!("{}/", elide(&e.name, name_w.saturating_sub(1)));
        (s, ink(DIR_INK))
    } else {
        (elide(&e.name, name_w), ink(INK))
    };
    text(
        cmds,
        px,
        name_x,
        ty,
        Align::Left,
        &label,
        color,
        selected,
        None,
    );
}

/// The status line as a glass bar, lit along its top edge, with a glow
/// rising into the pane above and bubbles in the gap between its texts.
pub fn status_commands(v: &StatusView, px: Px) -> Vec<DrawCmd> {
    let a = v.area;
    let (x1, y1) = (a.x + a.w, a.y + a.h);
    let mut cmds = vec![
        DrawCmd::FillRectangles {
            fill: vgrad(a.y - 0.4, a.y, hex(0x3fb8ff, 0.0), hex(0x3fb8ff, 0.45)),
            rects: vec![Rect {
                x: a.x,
                y: a.y - 0.4,
                w: a.w,
                h: 0.4,
            }],
        },
        DrawCmd::FillRectangles {
            fill: vgrad(a.y, y1, hex(0x5ab8ef, 0.97), hex(0x1468b4, 0.97)),
            rects: vec![Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: a.h,
            }],
        },
        DrawCmd::FillRectangles {
            fill: vgrad(a.y, a.y + 0.5, white(0.28), white(0.04)),
            rects: vec![Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: 0.5,
            }],
        },
        DrawCmd::FillRectangles {
            fill: Style::Flat(hex(0xc8f6ff, 0.95)),
            rects: vec![Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: px.y(1.0),
            }],
        },
    ];

    // The same strings `render::status_commands` shows.
    let mut right = String::new();
    if let Some((n, what)) = v.clip {
        right.push_str(&format!("{what} {n}   "));
    }
    if v.marks > 0 {
        right.push_str(&format!("marked {}   ", v.marks));
    }
    if !v.opts.filter.is_empty() {
        right.push_str(&format!("/{}   ", v.opts.filter));
    }
    if v.opts.show_hidden {
        right.push_str("hidden   ");
    }
    right.push_str(&format!(
        "{}   {}{}   {}/{}",
        v.view.label(),
        v.opts.sort.label(),
        if v.opts.reverse { "↓" } else { "↑" },
        if v.total == 0 { 0 } else { v.index + 1 },
        v.total
    ));
    let shade = Some(hex(0x06264d, 0.85));
    let rx = x1 - 1.0;
    text(
        &mut cmds,
        px,
        rx,
        a.y,
        Align::Right,
        &right,
        hex(0xd8f0ff, 1.0),
        false,
        shade,
    );

    let room = (a.w as usize).saturating_sub(text_cells(&right) as usize + 3);
    let (left, color, bold, left_shade) = match (v.busy, v.message) {
        (Some(busy), _) => (format!("{busy}…"), hex(0xd6ff9e, 1.0), true, shade),
        (None, Some((msg, true))) => (
            msg.to_string(),
            hex(0xffd6d0, 1.0),
            true,
            Some(hex(0x5a0d08, 0.9)),
        ),
        (None, Some((msg, false))) => (msg.to_string(), white(1.0), true, shade),
        (None, None) => (v.cwd.display().to_string(), white(1.0), false, shade),
    };
    let left = elide_front(&left, room);
    text(
        &mut cmds,
        px,
        a.x + 1.0,
        a.y,
        Align::Left,
        &left,
        color,
        bold,
        left_shade,
    );

    // Bubbles in whatever gap the two texts leave.
    let gap0 = a.x + 2.0 + text_cells(&left);
    let gap1 = rx - text_cells(&right) - 1.0;
    for col in gap0.ceil() as u32..gap1.max(gap0).floor() as u32 {
        let k = hash(col.wrapping_add(0xfeed));
        if !k.is_multiple_of(4) {
            continue;
        }
        let r = 3.0 + unit(k >> 5) * 5.0;
        let cx = col as f32 + unit(k >> 9);
        if cx - px.x(r) < gap0 || cx + px.x(r) > gap1 {
            continue;
        }
        bubble(&mut cmds, px, cx, a.y + 0.2 + unit(k >> 13) * 0.6, r);
    }
    cmds
}

/// The modal for the current mode — the Aero stand-in for
/// `App::modal_elements`.
pub fn modal_elements(s: &App) -> Vec<CreateElementBody> {
    let px = Px {
        w: s.cell_pw,
        h: s.cell_ph,
    };
    let id = MODAL_IDS.root;
    match &s.mode {
        Mode::Normal => Vec::new(),
        Mode::Prompt { kind, editor } => vec![prompt_element(
            id,
            s.cols,
            s.rows,
            kind.title(),
            &editor.buffer,
            px,
            Some(editor.cursor),
            ORDER_MODAL,
        )],
        Mode::Confirm { title, .. } => vec![prompt_element(
            id,
            s.cols,
            s.rows,
            title,
            "y = yes     n = cancel",
            px,
            None,
            ORDER_MODAL,
        )],
        Mode::Palette(picker) => vec![picker_element(id, s.cols, s.rows, picker, px, ORDER_MODAL)],
        Mode::Overlay { modal, offset } => {
            scroll_elements(modal, MODAL_IDS, s.cols, s.rows, *offset, px, ORDER_MODAL)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Area;

    /// The column must hit-test the way the stock one does, so a row is
    /// drawn where `render::column_row_at` expects it.
    #[test]
    fn column_rows_sit_where_the_stock_rows_do() {
        let area = Area {
            x: 0.0,
            y: 0.0,
            w: 14.0,
            h: 8.0,
        };
        let v = ColumnView {
            area,
            entries: &[],
            selected: None,
            error: None,
        };
        let px = Px { w: 12.0, h: 28.0 };
        let stock = super::super::column_commands(&v, px.w, px.h);
        let aero = column_commands(&v, px);
        let origin = |cmds: &[DrawCmd]| {
            cmds.iter().find_map(|c| match c {
                DrawCmd::DrawText { origin, .. } => Some((origin.x, origin.y)),
                _ => None,
            })
        };
        assert_eq!(origin(&stock), origin(&aero));
    }
}
