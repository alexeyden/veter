//! Center-screen overlays, built as VGE elements.
//!
//! Three shapes cover everything the clients need:
//!
//! * [`prompt_element`] — a fixed 4-row box with a title strip, one body
//!   line and an optional text caret. Rename prompts, y/n confirms.
//! * [`picker_element`] — title strip, input line with caret, and the
//!   filtered rows of a [`Picker`], with a scrollbar when they overflow.
//! * [`ScrollModal`] — a scrollable body of static lines (keybinding
//!   help, a file preview) built as a clipping parent plus children, so
//!   scrolling is an `UpdateOrigin` on two children rather than a
//!   rebuild of the body.
//!
//! All three share the same chrome: a [`surface_style`] fill, an accent
//! title strip rounded on its top corners only, and an accent outline
//! drawn last.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Command, CreateElementBody, DrawCmd, FontStyle, Style};

use crate::picker::Picker;
use crate::shape::{chrome_corner_radii, rounded_rect_path, rounded_rect_path_corners};
use crate::theme::{
    COLOR_ACTIVE_TEXT, COLOR_DIM_TEXT, COLOR_MODAL_TEXT, COLOR_SCROLLBAR, accent_color,
    accent_style, surface_style, title_thumb_style,
};

/// Half-width of the text caret bar, in cells.
const CARET_HALF_W: f32 = 0.07;

/// Rows a picker shows at most, however long the list is.
const PICKER_MAX_ROWS: usize = 14;

/// Element ids a [`ScrollModal`] creates. The parent owns the clip rect
/// and the title strip; deleting it cascades to every child (§9.6).
#[derive(Debug, Clone, Copy)]
pub struct ModalIds<'a> {
    pub root: &'a str,
    pub body_fill: &'a str,
    pub body_lines: &'a str,
    pub track: &'a str,
    pub thumb: &'a str,
}

/// Title strip + outline + body fill, the chrome every modal opens with.
/// `cmds` gets the body fill first (so content draws over it) and the
/// caller appends its content before calling [`close_chrome`].
fn open_chrome(box_w: f32, box_h: f32, title: &str, rx: f32, ry: f32) -> Vec<DrawCmd> {
    vec![
        // Body fill — full rounded rect; the title strip draws over its
        // top region.
        DrawCmd::FillPath {
            fill: surface_style(),
            segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
        },
        // Title strip — rounded only on the top corners so the seam with
        // the body fill below is straight.
        DrawCmd::FillPath {
            fill: accent_style(),
            segments: rounded_rect_path_corners(
                0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false,
            ),
        },
        DrawCmd::DrawText {
            origin: Point {
                x: box_w * 0.5,
                y: 0.0,
            },
            align: Align::Center,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(0x01),
            text: title.into(),
        },
    ]
}

/// The accent outline, drawn last so it sits above every fill.
fn close_chrome(cmds: &mut Vec<DrawCmd>, box_w: f32, box_h: f32, rx: f32, ry: f32) {
    cmds.push(DrawCmd::DrawLinePath {
        stroke: accent_style(),
        line_width: 0.1,
        segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
    });
}

/// A thin vertical bar at the insertion point, drawn instead of a caret
/// glyph in the string so it doesn't shift the text.
fn caret_bar(x: f32, y0: f32, y1: f32) -> DrawCmd {
    DrawCmd::FillPath {
        fill: Style::Flat(accent_color()),
        segments: rounded_rect_path(x - CARET_HALF_W, y0, x + CARET_HALF_W, y1, 0.0, 0.0),
    }
}

/// A one-line prompt or confirm box, centered on the host grid.
///
/// The 4-cell box is (title strip)(body top pad)(body line)(body bottom
/// pad). `caret` is the insertion point as a char index into `line`;
/// `None` draws no caret (a y/n confirm).
#[allow(clippy::too_many_arguments)]
pub fn prompt_element(
    id: &str,
    host_w: u32,
    host_h: u32,
    title: &str,
    line: &str,
    cell_pw: f32,
    cell_ph: f32,
    caret: Option<usize>,
    draw_order: i32,
) -> CreateElementBody {
    let chars = line.chars().count() as f32;
    let inner_w = chars.max(20.0);
    let box_w = (inner_w + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = 4.0_f32.min(host_h.saturating_sub(2) as f32);

    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let (rx, ry) = chrome_corner_radii(box_w, box_h, cell_pw, cell_ph);
    let mut cmds = open_chrome(box_w, box_h, title, rx, ry);
    close_chrome(&mut cmds, box_w, box_h, rx, ry);
    cmds.push(DrawCmd::DrawText {
        origin: Point {
            x: box_w * 0.5,
            y: 2.0,
        },
        align: Align::Center,
        fill: Style::Flat(COLOR_MODAL_TEXT),
        font_style: FontStyle(0x00),
        text: line.into(),
    });

    // The body line is center-aligned, so its left edge is
    // `box_w/2 - chars/2`; the bar sits `caret` cells to the right of
    // that (one glyph advance ≈ one cell, the same assumption the box
    // sizing above makes).
    if let Some(c) = caret {
        let caret_x = box_w * 0.5 - chars * 0.5 + c.min(line.chars().count()) as f32;
        cmds.push(caret_bar(caret_x, 2.1, 2.9));
    }

    CreateElementBody {
        id: id.to_string(),
        commands: cmds,
        origin: Point {
            x: origin_x,
            y: origin_y,
        },
        is_visible: true,
        draw_order,
        parent: None,
        size: None,
        transform: None,
    }
}

/// Build the picker modal as a single VGE element (rebuilt on every
/// keystroke — cheap, and the content changes with each filter edit).
/// Layout top to bottom: title strip (row 0), the filter / command input
/// with a caret (row 1), then the filtered rows with the selected one
/// highlighted. A scrollbar appears when matches overflow.
pub fn picker_element<P>(
    id: &str,
    host_w: u32,
    host_h: u32,
    picker: &Picker<P>,
    cell_pw: f32,
    cell_ph: f32,
    draw_order: i32,
) -> Vec<CreateElementBody> {
    // Rows available for the list (everything but the title + input rows
    // and a little vertical margin), capped so a huge list stays compact.
    let avail = (host_h.saturating_sub(4)).max(1) as usize;
    let visible_rows = picker.matches.len().max(1).min(avail).min(PICKER_MAX_ROWS);

    // Window the matches so the selection stays on screen.
    let total = picker.matches.len();
    let max_scroll = total.saturating_sub(visible_rows);
    let scroll = (if picker.selected >= visible_rows {
        picker.selected - visible_rows + 1
    } else {
        0
    })
    .min(max_scroll);

    // Width: widest of the title, the input line, and every row (label +
    // gap + hint), within the host. Taken over *all* items (not just the
    // current matches) so the box keeps a stable width while filtering;
    // the input line can still widen it when a long command is typed.
    let input_line = format!("> {}", picker.editor.buffer);
    let mut max_w = picker
        .title
        .chars()
        .count()
        .max(input_line.chars().count());
    for it in &picker.items {
        max_w = max_w.max(it.label.chars().count() + 3 + it.hint.chars().count());
    }
    let inner_w = (max_w as f32).max(26.0);
    let box_w = (inner_w + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = (2 + visible_rows) as f32;

    // Anchor the top edge using the box's *maximum* height (the full
    // item count, which is stable across filtering) so the input row
    // never moves as matches shrink — the box only grows/shrinks
    // downward.
    let max_rows = picker.items.len().max(1).min(avail).min(PICKER_MAX_ROWS);
    let max_box_h = (2 + max_rows) as f32;
    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - max_box_h) * 0.5).floor();
    let (rx, ry) = chrome_corner_radii(box_w, box_h, cell_pw, cell_ph);

    let mut cmds = open_chrome(box_w, box_h, &picker.title, rx, ry);
    cmds.push(DrawCmd::DrawText {
        origin: Point { x: 1.0, y: 1.0 },
        align: Align::Left,
        fill: Style::Flat(COLOR_MODAL_TEXT),
        font_style: FontStyle(0x00),
        text: input_line,
    });
    // Caret bar — "> " is 2 cells, then `cursor` glyphs in.
    let caret_x = 1.0 + 2.0 + picker.editor.cursor.min(picker.editor.char_count()) as f32;
    cmds.push(caret_bar(caret_x, 1.1, 1.9));

    for j in 0..visible_rows {
        let m = scroll + j;
        if m >= total {
            break;
        }
        let it = &picker.items[picker.matches[m]];
        let y = 2.0 + j as f32;
        let selected = m == picker.selected;
        if selected {
            cmds.push(DrawCmd::FillRectangles {
                fill: title_thumb_style(),
                rects: vec![Rect {
                    x: 0.5,
                    y: y + 0.05,
                    w: box_w - 1.0,
                    h: 0.9,
                }],
            });
        }
        cmds.push(DrawCmd::DrawText {
            origin: Point { x: 1.5, y },
            align: Align::Left,
            fill: Style::Flat(if selected {
                COLOR_ACTIVE_TEXT
            } else {
                COLOR_MODAL_TEXT
            }),
            font_style: FontStyle(if selected { 0x01 } else { 0x00 }),
            text: it.label.clone(),
        });
        if !it.hint.is_empty() {
            cmds.push(DrawCmd::DrawText {
                origin: Point {
                    x: box_w - 1.5,
                    y,
                },
                align: Align::Right,
                fill: Style::Flat(COLOR_DIM_TEXT),
                font_style: FontStyle(0x00),
                text: it.hint.clone(),
            });
        }
    }

    if total > visible_rows {
        let track_h = visible_rows as f32;
        let thumb_h = (track_h * visible_rows as f32 / total as f32).max(0.5);
        let thumb_max = (track_h - thumb_h).max(0.0);
        let thumb_y = 2.0
            + if max_scroll == 0 {
                0.0
            } else {
                thumb_max * scroll as f32 / max_scroll as f32
            };
        cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(COLOR_SCROLLBAR),
            rects: vec![Rect {
                x: box_w - 0.7,
                y: 2.0,
                w: 0.4,
                h: track_h,
            }],
        });
        cmds.push(DrawCmd::FillRectangles {
            fill: accent_style(),
            rects: vec![Rect {
                x: box_w - 0.7,
                y: thumb_y,
                w: 0.4,
                h: thumb_h,
            }],
        });
    }

    close_chrome(&mut cmds, box_w, box_h, rx, ry);

    vec![CreateElementBody {
        id: id.to_string(),
        commands: cmds,
        origin: Point {
            x: origin_x,
            y: origin_y,
        },
        is_visible: true,
        draw_order,
        parent: None,
        size: None,
        transform: None,
    }]
}

/// A scrollable modal over a fixed body of lines.
///
/// `lines[0]` is the title (drawn in the strip); the rest are the
/// scrollable body. The body is a single child element whose origin
/// shifts to scroll, clipped by the parent's `size`, so scrolling never
/// rebuilds draw commands — see [`ScrollModal::scroll_commands`].
#[derive(Debug, Clone)]
pub struct ScrollModal {
    /// Title first, then body lines.
    pub lines: Vec<String>,
    /// Minimum inner width, in cells.
    pub min_inner_w: f32,
    /// Horizontal padding added to the widest line.
    pub pad_w: f32,
    /// Left inset of the body text, in cells.
    pub text_x: f32,
    /// Number of body lines a half-page jump moves through.
    pub half_page: i64,
}

impl ScrollModal {
    /// A modal over `lines` with the defaults the help overlay uses.
    pub fn new(lines: Vec<String>) -> Self {
        ScrollModal {
            lines,
            min_inner_w: 30.0,
            pad_w: 6.0,
            text_x: 3.0,
            half_page: 6,
        }
    }

    /// Box size in cells for a given host grid.
    pub fn box_size(&self, host_w: u32, host_h: u32) -> (f32, f32) {
        let max_line = self
            .lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(20) as f32;
        let inner_w = max_line.max(self.min_inner_w);
        let box_w = (inner_w + self.pad_w).min(host_w.saturating_sub(2) as f32);
        let inner_h = self.lines.len() as f32;
        let box_h = (inner_h + 2.0).min(host_h.saturating_sub(2) as f32);
        (box_w, box_h)
    }

    /// Number of body lines (everything after the title).
    fn body_lines(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    /// `(visible body rows, max scroll offset)` for a box height. Body
    /// lines start at row 2 (after the title strip and a one-row gap)
    /// and stop at row `box_h - 1`, leaving a one-row bottom pad. When
    /// `box_h` is too small for the gap/pad, body content fills whatever
    /// is left.
    pub fn body_window(&self, box_h: f32) -> (usize, u32) {
        let body_rows = (box_h as i32 - 3).max(1) as usize;
        let max_offset = self.body_lines().saturating_sub(body_rows) as u32;
        (body_rows, max_offset)
    }

    /// Largest scroll offset for a given host grid.
    pub fn max_offset(&self, host_w: u32, host_h: u32) -> u32 {
        let (_, box_h) = self.box_size(host_w, host_h);
        self.body_window(box_h).1
    }

    /// Y-coordinate of the scrollbar thumb element's origin
    /// (parent-local) for a scroll `offset`. Centralised so initial
    /// creation and `UpdateOrigin`-driven scrolls compute identical
    /// values.
    pub fn thumb_origin_y(&self, box_h: f32, offset: u32) -> f32 {
        let (body_rows, max_offset) = self.body_window(box_h);
        let body_lines = self.body_lines();
        if body_lines <= body_rows || max_offset == 0 {
            return 2.0;
        }
        let track_h = body_rows as f32;
        let thumb_h = (track_h * body_rows as f32 / body_lines as f32).max(0.5);
        let thumb_max_off = (track_h - thumb_h).max(0.0);
        let off = offset.min(max_offset) as f32;
        2.0 + thumb_max_off * off / max_offset as f32
    }

    /// Build the modal: a parent element (chrome + clip rect) plus
    /// children for the body fill, the scrollable body lines, and — when
    /// content overflows — a scrollbar track and thumb.
    pub fn elements(
        &self,
        ids: ModalIds<'_>,
        host_w: u32,
        host_h: u32,
        offset: u32,
        cell_pw: f32,
        cell_ph: f32,
        draw_order: i32,
    ) -> Vec<CreateElementBody> {
        let (box_w, box_h) = self.box_size(host_w, host_h);
        let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
        let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

        let (body_rows, max_offset) = self.body_window(box_h);
        let offset = offset.min(max_offset);
        let body_lines = self.body_lines();
        let scrollable = body_lines > body_rows;

        let (rx, ry) = chrome_corner_radii(box_w, box_h, cell_pw, cell_ph);
        let title = self.lines.first().cloned().unwrap_or_default();

        let mut elements: Vec<CreateElementBody> = Vec::new();

        // Parent — its `size` clips every child to the modal's bounds,
        // and its own `commands` (title strip, title text, outline) draw
        // last, on top of every child, so the title strip cleanly masks
        // any body line that scrolls into rows 0/1.
        elements.push(CreateElementBody {
            id: ids.root.into(),
            commands: vec![
                DrawCmd::FillPath {
                    fill: accent_style(),
                    segments: rounded_rect_path_corners(
                        0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false,
                    ),
                },
                DrawCmd::DrawText {
                    origin: Point {
                        x: box_w * 0.5,
                        y: 0.0,
                    },
                    align: Align::Center,
                    fill: Style::Flat(COLOR_MODAL_TEXT),
                    font_style: FontStyle(0x01),
                    text: title,
                },
                DrawCmd::DrawLinePath {
                    stroke: accent_style(),
                    line_width: 0.1,
                    segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
                },
            ],
            origin: Point {
                x: origin_x,
                y: origin_y,
            },
            is_visible: true,
            draw_order,
            parent: None,
            size: Some(Point {
                x: box_w,
                y: box_h,
            }),
            transform: None,
        });

        // Body fill — full rounded rect underneath the scrolling lines.
        // The parent's title strip covers its top region; we rely on
        // draw order rather than masking the fill itself.
        elements.push(CreateElementBody {
            id: ids.body_fill.into(),
            commands: vec![DrawCmd::FillPath {
                fill: surface_style(),
                segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 0,
            parent: Some(ids.root.into()),
            size: None,
            transform: None,
        });

        // Body lines — every line drawn at its natural y position; the
        // element's origin shifts the whole stack up to scroll, and the
        // parent's clip rect hides what goes out of view.
        let mut body_cmds = Vec::with_capacity(body_lines);
        for (i, line) in self.lines.iter().enumerate().skip(1) {
            let bold = !line.is_empty() && !line.starts_with(' ');
            body_cmds.push(DrawCmd::DrawText {
                origin: Point {
                    x: self.text_x,
                    y: 1.0 + i as f32,
                },
                align: Align::Left,
                fill: Style::Flat(COLOR_MODAL_TEXT),
                font_style: FontStyle(if bold { 0x01 } else { 0x00 }),
                text: line.clone(),
            });
        }
        elements.push(CreateElementBody {
            id: ids.body_lines.into(),
            commands: body_cmds,
            origin: Point {
                x: 0.0,
                y: -(offset as f32),
            },
            is_visible: true,
            draw_order: 1,
            parent: Some(ids.root.into()),
            size: None,
            transform: None,
        });

        if scrollable {
            let track_h = body_rows as f32;
            let thumb_h = (track_h * body_rows as f32 / body_lines as f32).max(0.5);

            elements.push(CreateElementBody {
                id: ids.track.into(),
                commands: vec![DrawCmd::FillRectangles {
                    fill: Style::Flat(COLOR_SCROLLBAR),
                    rects: vec![Rect {
                        x: box_w - 1.0,
                        y: 2.0,
                        w: 0.4,
                        h: track_h,
                    }],
                }],
                origin: Point { x: 0.0, y: 0.0 },
                is_visible: true,
                draw_order: 2,
                parent: Some(ids.root.into()),
                size: None,
                transform: None,
            });

            // The thumb command is anchored at local (box_w-1, 0); the
            // element's origin places it at the right y, so a scroll
            // update is a single `UpdateOrigin` away.
            elements.push(CreateElementBody {
                id: ids.thumb.into(),
                commands: vec![DrawCmd::FillRectangles {
                    fill: accent_style(),
                    rects: vec![Rect {
                        x: box_w - 1.0,
                        y: 0.0,
                        w: 0.4,
                        h: thumb_h,
                    }],
                }],
                origin: Point {
                    x: 0.0,
                    y: self.thumb_origin_y(box_h, offset),
                },
                is_visible: true,
                draw_order: 3,
                parent: Some(ids.root.into()),
                size: None,
                transform: None,
            });
        }

        elements
    }

    /// The `UpdateOrigin` pair that scrolls an already-built modal to
    /// `offset`. Empty when the body doesn't overflow (nothing to
    /// scroll, and the thumb element doesn't exist).
    pub fn scroll_commands(
        &self,
        ids: ModalIds<'_>,
        host_w: u32,
        host_h: u32,
        offset: u32,
    ) -> Vec<Command> {
        let (_, box_h) = self.box_size(host_w, host_h);
        let (body_rows, max_offset) = self.body_window(box_h);
        if self.body_lines() <= body_rows {
            return Vec::new();
        }
        let off = offset.min(max_offset);
        vec![
            Command::UpdateOrigin {
                id: ids.body_lines.into(),
                origin: Point {
                    x: 0.0,
                    y: -(off as f32),
                },
            },
            Command::UpdateOrigin {
                id: ids.thumb.into(),
                origin: Point {
                    x: 0.0,
                    y: self.thumb_origin_y(box_h, off),
                },
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::{FilterMode, PickerItem};

    const IDS: ModalIds<'static> = ModalIds {
        root: "m",
        body_fill: "m-bg",
        body_lines: "m-body",
        track: "m-track",
        thumb: "m-thumb",
    };

    fn lines(n: usize) -> Vec<String> {
        std::iter::once("Title".to_string())
            .chain((0..n).map(|i| format!("  line {i}")))
            .collect()
    }

    #[test]
    fn prompt_box_is_centered_and_four_rows_tall() {
        let el = prompt_element("p", 80, 24, "Rename", "abc", 10.0, 20.0, Some(3), 100);
        assert_eq!(el.id, "p");
        assert_eq!(el.draw_order, 100);
        // 20-char minimum inner width + 4 padding.
        assert_eq!(el.origin.x, ((80.0 - 24.0) * 0.5_f32).floor());
        assert_eq!(el.origin.y, 10.0);
    }

    #[test]
    fn prompt_caret_is_omitted_for_a_confirm() {
        let with = prompt_element("p", 80, 24, "T", "abc", 10.0, 20.0, Some(0), 0);
        let without = prompt_element("p", 80, 24, "T", "abc", 10.0, 20.0, None, 0);
        assert_eq!(with.commands.len(), without.commands.len() + 1);
    }

    #[test]
    fn scroll_modal_clips_with_the_parent_and_scrolls_the_body_child() {
        let m = ScrollModal::new(lines(60));
        let els = m.elements(IDS, 80, 24, 5, 10.0, 20.0, 100);
        let root = &els[0];
        assert!(root.size.is_some(), "parent must clip its children");
        let body = els.iter().find(|e| e.id == "m-body").unwrap();
        assert_eq!(body.origin.y, -5.0);
        assert_eq!(body.parent.as_deref(), Some("m"));
        assert!(els.iter().any(|e| e.id == "m-thumb"));
    }

    #[test]
    fn short_bodies_get_no_scrollbar() {
        let m = ScrollModal::new(lines(3));
        let els = m.elements(IDS, 80, 24, 0, 10.0, 20.0, 0);
        assert!(!els.iter().any(|e| e.id == "m-thumb"));
        assert!(m.scroll_commands(IDS, 80, 24, 1).is_empty());
    }

    #[test]
    fn scrolling_moves_the_body_and_thumb_together() {
        let m = ScrollModal::new(lines(60));
        let cmds = m.scroll_commands(IDS, 80, 24, m.max_offset(80, 24));
        assert_eq!(cmds.len(), 2);
        let (_, box_h) = m.box_size(80, 24);
        let (body_rows, max_offset) = m.body_window(box_h);
        assert!(max_offset > 0 && body_rows > 0);
        // The thumb bottoms out exactly at the end of the track.
        let y = m.thumb_origin_y(box_h, max_offset);
        let thumb_h = (body_rows as f32 * body_rows as f32 / 60.0).max(0.5);
        assert!((y + thumb_h - (2.0 + body_rows as f32)).abs() < 1e-3, "{y}");
    }

    #[test]
    fn offsets_past_the_end_clamp() {
        let m = ScrollModal::new(lines(60));
        let (_, box_h) = m.box_size(80, 24);
        let max = m.body_window(box_h).1;
        assert_eq!(m.thumb_origin_y(box_h, max), m.thumb_origin_y(box_h, max + 99));
    }

    #[test]
    fn picker_box_top_edge_does_not_move_as_matches_shrink() {
        let items: Vec<PickerItem<usize>> = (0..10)
            .map(|i| PickerItem::new(format!("item{i}"), "", i))
            .collect();
        let mut p = Picker::new("Pick", FilterMode::Whole, items);
        let before = picker_element("k", 80, 24, &p, 10.0, 20.0, 0)[0].origin.y;
        p.editor.insert('7');
        p.refilter();
        assert_eq!(p.matches.len(), 1);
        let after = picker_element("k", 80, 24, &p, 10.0, 20.0, 0)[0].origin.y;
        assert_eq!(before, after);
    }

    #[test]
    fn picker_renders_a_scrollbar_only_when_rows_overflow() {
        let many: Vec<PickerItem<usize>> = (0..40)
            .map(|i| PickerItem::new(format!("item{i}"), "", i))
            .collect();
        let p = Picker::new("Pick", FilterMode::Whole, many);
        let el = &picker_element("k", 80, 40, &p, 10.0, 20.0, 0)[0];
        let rects = el
            .commands
            .iter()
            .filter(|c| matches!(c, DrawCmd::FillRectangles { .. }))
            .count();
        // One highlight for the selected row + track + thumb.
        assert_eq!(rects, 3);

        let few: Vec<PickerItem<usize>> = (0..3)
            .map(|i| PickerItem::new(format!("item{i}"), "", i))
            .collect();
        let p = Picker::new("Pick", FilterMode::Whole, few);
        let el = &picker_element("k", 80, 40, &p, 10.0, 20.0, 0)[0];
        let rects = el
            .commands
            .iter()
            .filter(|c| matches!(c, DrawCmd::FillRectangles { .. }))
            .count();
        assert_eq!(rects, 1, "selection highlight only");
    }
}
