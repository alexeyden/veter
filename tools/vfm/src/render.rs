//! Everything vfm puts on screen, as VGE draw commands.
//!
//! The window is drawn entirely with VGE rather than terminal text: the
//! text layer always renders *below* VGE elements (spec §12), so a
//! selection bar behind a filename is only possible if the filename is a
//! `DrawText` too. What each part looks like is the style engine's
//! ([`vge_ui::style`]), so the panes share vmux's look whichever engine
//! is in force; this module decides where the parts go, which is also
//! what the hit-tests read.
//!
//! Each pane is one element whose command list is rebuilt when it
//! changes; the event loop pushes those with `UpdateCommands`, so
//! nothing is re-created per keystroke.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use vge_protocol::codec::Rect;
use vge_protocol::command::{Align, DrawCmd};
use vge_ui::measure::text_cells;
use vge_ui::paint::Px;
use vge_ui::style::{self, Engine, FileKind, IconBox, ListInk, StatusLeft};

use crate::entry::{Entry, Kind, ListOpts, Media};
use crate::layout::{Area, Layout, View};
use crate::thumbs::{Slot, Thumbs, stamp_of};

fn rect(a: Area) -> Rect {
    Rect {
        x: a.x,
        y: a.y,
        w: a.w,
        h: a.h,
    }
}

fn file_kind(m: Media) -> FileKind {
    match m {
        Media::Dir => FileKind::Dir,
        Media::Image => FileKind::Image,
        Media::Video => FileKind::Video,
        Media::Audio => FileKind::Audio,
        Media::Text => FileKind::Text,
        Media::Archive => FileKind::Archive,
        Media::Binary => FileKind::Binary,
    }
}

/// Full-window backdrop.
pub fn backdrop(l: &Layout, cell_pw: f32, cell_ph: f32) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();
    let px = Px {
        w: cell_pw,
        h: cell_ph,
    };
    style::engine().browser_backdrop(&mut cmds, px, l.cols as f32, l.rows as f32);
    cmds
}

/// One ancestor column.
pub struct ColumnView<'a> {
    pub area: Area,
    pub entries: &'a [Entry],
    /// Index of the entry that leads to the directory we are in.
    pub selected: Option<usize>,
    pub error: Option<&'a str>,
}

/// Rows of a column that fit, and the first one shown so the selected
/// entry stays visible (centered once the list is long enough to scroll).
fn column_window(rows: usize, total: usize, selected: Option<usize>) -> usize {
    let Some(sel) = selected else { return 0 };
    if total <= rows {
        return 0;
    }
    let half = rows / 2;
    sel.saturating_sub(half).min(total - rows)
}

/// The entry index in an ancestor column under a click at cell-row
/// `click_row`, or `None` if the click missed a row (the top margin or
/// the empty space below the last entry). Mirrors the row geometry
/// [`column_commands`] draws, so a click lands on the row it looks like
/// it lands on.
pub fn column_row_at(
    area: Area,
    total: usize,
    selected: Option<usize>,
    click_row: f32,
) -> Option<usize> {
    let rows = (area.h as usize).saturating_sub(1).max(1);
    let start = column_window(rows, total, selected);
    // Rows are drawn at `y = area.y + 0.5 + row`; the leading 0.5 is a
    // top margin, so a click above the first row's band misses.
    let rel = click_row - (area.y + 0.5);
    if rel < 0.0 {
        return None;
    }
    let row = rel.floor() as usize;
    if row >= rows {
        return None;
    }
    let idx = start + row;
    (idx < total).then_some(idx)
}

pub fn column_commands(v: &ColumnView, cell_pw: f32, cell_ph: f32) -> Vec<DrawCmd> {
    let px = Px {
        w: cell_pw,
        h: cell_ph,
    };
    column_commands_in(style::engine(), v, px)
}

fn column_commands_in(engine: &dyn Engine, v: &ColumnView, px: Px) -> Vec<DrawCmd> {
    let a = v.area;
    let mut cmds = Vec::new();
    engine.column_pane(&mut cmds, px, rect(a));

    if let Some(err) = v.error {
        let s = elide(err, a.w as usize - 2);
        engine.browser_note(&mut cmds, px, a.x + 1.0, a.y + 1.0, Align::Left, &s, false);
        return cmds;
    }

    // Row geometry is what `column_row_at` hit-tests against.
    let rows = (a.h as usize).saturating_sub(1).max(1);
    let start = column_window(rows, v.entries.len(), v.selected);
    let text_w = (a.w as usize).saturating_sub(3).max(4);

    for (row, e) in v.entries.iter().skip(start).take(rows).enumerate() {
        let y = a.y + 0.5 + row as f32;
        let is_sel = v.selected == Some(start + row);
        let label = if e.is_dir() {
            format!("{}/", elide(&e.name, text_w.saturating_sub(1)))
        } else {
            elide(&e.name, text_w)
        };
        engine.column_row(&mut cmds, px, rect(a), y, &label, e.is_dir(), is_sel);
    }

    if v.entries.is_empty() {
        engine.browser_note(&mut cmds, px, a.x + 1.0, a.y + 0.5, Align::Left, "empty", false);
    }
    cmds
}

/// The current directory's pane: one tile per entry in the icon grid,
/// one row per entry in the detail list ([`View`]).
pub struct GridView<'a> {
    pub layout: &'a Layout,
    pub entries: &'a [Entry],
    pub cursor: usize,
    pub scroll_row: usize,
    pub marks: &'a HashSet<PathBuf>,
    pub thumbs: &'a Thumbs,
    pub cell_pw: f32,
    pub cell_ph: f32,
    pub error: Option<&'a str>,
}

impl GridView<'_> {
    fn px(&self) -> Px {
        Px {
            w: self.cell_pw,
            h: self.cell_ph,
        }
    }
}

pub fn grid_commands(v: &GridView) -> Vec<DrawCmd> {
    grid_commands_in(style::engine(), v)
}

fn grid_commands_in(engine: &dyn Engine, v: &GridView) -> Vec<DrawCmd> {
    let px = v.px();
    let l = v.layout;
    let a = l.grid;
    let mut cmds: Vec<DrawCmd> = Vec::new();
    engine.grid_pane(&mut cmds, px, rect(a));

    if let Some(err) = v.error {
        let s = elide(err, a.w as usize - 4);
        engine.browser_note(&mut cmds, px, a.x + 2.0, a.y + 1.0, Align::Left, &s, true);
        return cmds;
    }
    if v.entries.is_empty() {
        let (cx, cy) = (a.x + a.w * 0.5, a.y + a.h * 0.5);
        engine.browser_note(&mut cmds, px, cx, cy, Align::Center, "empty directory", false);
        return cmds;
    }

    let first = v.scroll_row * l.grid_cols;
    let last = (first + l.page()).min(v.entries.len());
    for index in first..last {
        match l.view {
            View::Grid => tile_commands(&mut cmds, engine, v, index),
            View::List => row_commands(&mut cmds, engine, v, index),
        }
    }

    // Scrollbar down the right edge when the listing overflows.
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
        engine.browser_scrollbar(&mut cmds, px, rect(a), track_h, thumb_h, frac);
    }
    cmds
}

/// One tile: plate, selection, picture (thumbnail or icon), mark, label.
fn tile_commands(cmds: &mut Vec<DrawCmd>, engine: &dyn Engine, v: &GridView, index: usize) {
    let px = v.px();
    let l = v.layout;
    let e = &v.entries[index];
    let (x, y) = l.tile_origin(index, v.scroll_row);
    let tile = Rect {
        x,
        y,
        w: l.tile_w,
        h: l.tile_h,
    };
    let selected = index == v.cursor;
    engine.tile_plate(cmds, px, tile, l.tile_img_h, selected);
    picture(
        cmds,
        engine,
        v,
        e,
        (x + 0.25, y + 0.15, l.tile_w - 0.5, l.tile_img_h - 0.15),
        0.66,
    );
    if v.marks.contains(&e.path) {
        engine.tile_mark(cmds, px, tile);
    }
    let label_w = (l.tile_w as usize).saturating_sub(1).max(3);
    let label = elide(&e.name, label_w);
    let lx = x + l.tile_w * 0.5;
    engine.tile_label(cmds, px, lx, y + l.tile_img_h, &label, e.is_dir(), selected);
}

/// Detail columns of a list row, in cells: the size and the modified
/// time. Fixed width, so the numbers line up down the pane.
const LIST_SIZE_COLS: f32 = 7.0;
const LIST_DATE_COLS: f32 = 17.0;
/// Row widths at which those columns are worth their cells. Below each,
/// the column is dropped and the name keeps the room.
const LIST_SIZE_MIN_W: f32 = 30.0;
const LIST_DATE_MIN_W: f32 = 50.0;

/// One list row: selection, mark, picture at the left, then name, size
/// and modified time.
fn row_commands(cmds: &mut Vec<DrawCmd>, engine: &dyn Engine, v: &GridView, index: usize) {
    let px = v.px();
    let l = v.layout;
    let e = &v.entries[index];
    let (x, y) = l.tile_origin(index, v.scroll_row);
    let w = l.tile_w;
    let row = Rect {
        x,
        y,
        w,
        h: l.tile_h,
    };
    let selected = index == v.cursor;
    engine.list_row_plate(cmds, px, row, index % 2 == 1, selected);
    if v.marks.contains(&e.path) {
        engine.list_row_mark(cmds, px, row);
    }

    // A square picture box at the left, as tall as the row.
    let icon_w = (l.tile_h * v.cell_ph / v.cell_pw.max(1.0)).max(1.0);
    let icon_x = x + 0.7;
    picture(cmds, engine, v, e, (icon_x, y + 0.05, icon_w, l.tile_h - 0.1), 0.82);

    // One text baseline, centered when the row is more than a row tall.
    let ty = y + (l.tile_h - 1.0) * 0.5;
    // Columns are laid out from the right; the name takes what is left.
    let mut right = x + w - 0.5;
    if w >= LIST_DATE_MIN_W {
        let s = e.mtime_label();
        engine.list_text(cmds, px, right, ty, Align::Right, &s, ListInk::Meta, selected);
        right -= LIST_DATE_COLS;
    }
    if w >= LIST_SIZE_MIN_W {
        let s = e.size_label();
        engine.list_text(cmds, px, right, ty, Align::Right, &s, ListInk::Meta, selected);
        right -= LIST_SIZE_COLS;
    }

    let name_x = icon_x + icon_w + 0.6;
    let name_w = ((right - name_x - 1.0).max(4.0)) as usize;
    let label = if e.is_dir() {
        format!("{}/", elide(&e.name, name_w.saturating_sub(1)))
    } else {
        elide(&e.name, name_w)
    };
    let name = ListInk::Name { is_dir: e.is_dir() };
    engine.list_text(cmds, px, name_x, ty, Align::Left, &label, name, selected);
}

/// An entry's picture fitted into `area`: its thumbnail once a decoder
/// has one, else the file-type icon at `icon_frac` of the box.
fn picture(
    cmds: &mut Vec<DrawCmd>,
    engine: &dyn Engine,
    v: &GridView,
    e: &Entry,
    area: (f32, f32, f32, f32),
    icon_frac: f32,
) {
    let px = v.px();
    match v.thumbs.slot(&e.path, stamp_of(e.size, e.mtime)) {
        Some(Slot::Ready { image_id, w, h }) => {
            let r = fit(area, *w, *h, v.cell_pw, v.cell_ph);
            engine.thumbnail(cmds, px, r, image_id.clone());
        }
        _ => engine.file_icon(
            cmds,
            px,
            file_kind(e.media()),
            IconBox::centered(area, icon_frac, v.cell_pw, v.cell_ph),
            e.is_link,
            e.kind == Kind::Broken,
        ),
    }
}

/// Fit a `w × h` pixel image inside `(x, y, w, h)` cells, preserving its
/// visual aspect ratio on this cell grid and centering it.
fn fit(area: (f32, f32, f32, f32), w: u32, h: u32, cell_pw: f32, cell_ph: f32) -> Rect {
    let (ax, ay, aw, ah) = area;
    let (px_w, px_h) = (aw * cell_pw, ah * cell_ph);
    let scale = (px_w / w.max(1) as f32).min(px_h / h.max(1) as f32);
    let dw = (w as f32 * scale / cell_pw).min(aw);
    let dh = (h as f32 * scale / cell_ph).min(ah);
    Rect {
        x: ax + (aw - dw) * 0.5,
        y: ay + (ah - dh) * 0.5,
        w: dw,
        h: dh,
    }
}

/// The bottom status line.
pub struct StatusView<'a> {
    pub area: Area,
    pub cwd: &'a Path,
    pub index: usize,
    pub total: usize,
    pub marks: usize,
    pub view: View,
    pub opts: &'a ListOpts,
    /// Pending clipboard, as `(count, "copy" | "move")`.
    pub clip: Option<(usize, &'static str)>,
    /// Transient message and whether it reports a failure.
    pub message: Option<(&'a str, bool)>,
    pub busy: Option<&'a str>,
}

pub fn status_commands(v: &StatusView, cell_pw: f32, cell_ph: f32) -> Vec<DrawCmd> {
    let px = Px {
        w: cell_pw,
        h: cell_ph,
    };
    status_commands_in(style::engine(), v, px)
}

fn status_commands_in(engine: &dyn Engine, v: &StatusView, px: Px) -> Vec<DrawCmd> {
    let a = v.area;
    // Right side first, so the path knows how much room it has left.
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

    // Left: a message while there is one, else the current path.
    let room = (a.w as usize).saturating_sub(text_cells(&right) as usize + 3);
    let (left, kind) = match (v.busy, v.message) {
        (Some(busy), _) => (format!("{busy}…"), StatusLeft::Busy),
        (None, Some((msg, true))) => (msg.to_string(), StatusLeft::Failed),
        (None, Some((msg, false))) => (msg.to_string(), StatusLeft::Message),
        (None, None) => (v.cwd.display().to_string(), StatusLeft::Path),
    };
    let left = elide_front(&left, room);
    let mut cmds = Vec::new();
    engine.status_bar(&mut cmds, px, rect(a), &left, kind, &right);
    cmds
}

/// Clip `s` to `max` grid columns, marking the cut with `…`. Columns,
/// not characters: a CJK name is twice as wide as its length suggests
/// and would otherwise run past the tile it labels.
pub use vge_ui::measure::{elide, elide_front};

#[cfg(test)]
mod tests {
    use super::*;
    use vge_ui::style::flat::Flat;

    // These check what vfm lays out, so they pin the engine rather than
    // take whichever one `VETER_STYLE` names.
    fn grid_commands(v: &GridView) -> Vec<DrawCmd> {
        grid_commands_in(&Flat, v)
    }

    fn status_commands(v: &StatusView, cell_pw: f32, cell_ph: f32) -> Vec<DrawCmd> {
        status_commands_in(&Flat, v, Px { w: cell_pw, h: cell_ph })
    }
    use std::time::SystemTime;

    fn entry(name: &str, dir: bool) -> Entry {
        Entry {
            name: name.into(),
            path: PathBuf::from(name),
            kind: if dir { Kind::Dir } else { Kind::File },
            size: 0,
            mtime: SystemTime::UNIX_EPOCH,
            is_link: false,
        }
    }

    fn layout() -> Layout {
        Layout::compute(120, 40, 9.0, 20.0, View::Grid, 1, 2)
    }

    fn view<'a>(l: &'a Layout, entries: &'a [Entry], marks: &'a HashSet<PathBuf>, thumbs: &'a Thumbs) -> GridView<'a> {
        GridView {
            layout: l,
            entries,
            cursor: 0,
            scroll_row: 0,
            marks,
            thumbs,
            cell_pw: 9.0,
            cell_ph: 20.0,
            error: None,
        }
    }

    fn texts(cmds: &[DrawCmd]) -> Vec<String> {
        cmds.iter()
            .filter_map(|c| match c {
                DrawCmd::DrawText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn elide_keeps_within_the_cap_at_both_ends() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("abcdefgh", 4).chars().count(), 4);
        assert!(elide("abcdefgh", 4).ends_with('…'));
        assert_eq!(elide_front("/a/very/long/path", 6).chars().count(), 6);
        assert!(elide_front("/a/very/long/path", 6).starts_with('…'));
        assert!(elide_front("/a/very/long/path", 6).ends_with("path"));
    }

    #[test]
    fn image_fit_preserves_aspect_and_centers() {
        // A 2:1 picture in a square-ish cell area.
        let r = fit((10.0, 4.0, 12.0, 5.0), 400, 200, 9.0, 20.0);
        let (px_w, px_h) = (r.w * 9.0, r.h * 20.0);
        assert!((px_w / px_h - 2.0).abs() < 0.05, "{px_w}×{px_h}");
        assert!(r.w <= 12.0 + 1e-3 && r.h <= 5.0 + 1e-3, "stays inside");
        // Centered on the axis it doesn't fill.
        assert!(((r.y - 4.0) - (5.0 - r.h - (r.y - 4.0))).abs() < 1e-3);
    }

    #[test]
    fn a_column_scrolls_to_keep_the_selection_visible() {
        assert_eq!(column_window(10, 5, Some(3)), 0, "no scroll when it fits");
        assert_eq!(column_window(10, 100, Some(0)), 0);
        assert_eq!(column_window(10, 100, Some(50)), 45, "centered");
        assert_eq!(column_window(10, 100, Some(99)), 90, "clamped at the end");
        assert_eq!(column_window(10, 100, None), 0);
    }

    #[test]
    fn a_column_click_maps_to_the_row_it_lands_on() {
        let area = Area {
            x: 0.0,
            y: 0.0,
            w: 12.0,
            h: 6.0, // 5 drawable rows (h - 1)
        };
        // Rows drawn at y = 0.5, 1.5, 2.5, …; a click at cell 1 hits row 0.
        assert_eq!(column_row_at(area, 5, None, 1.0), Some(0));
        assert_eq!(column_row_at(area, 5, None, 2.0), Some(1));
        // The top-margin half-cell and the space past the last entry miss.
        assert_eq!(column_row_at(area, 5, None, 0.0), None);
        assert_eq!(column_row_at(area, 3, None, 4.0), None, "below the list");
        // With a long list, the click resolves through the scroll window.
        assert_eq!(column_row_at(area, 100, Some(50), 1.0), Some(48));
    }

    #[test]
    fn every_visible_tile_draws_a_label() {
        let l = layout();
        let entries: Vec<Entry> = (0..200).map(|i| entry(&format!("f{i}.txt"), false)).collect();
        let marks = HashSet::new();
        let thumbs = Thumbs::new(64, 1, 8);
        let cmds = grid_commands(&view(&l, &entries, &marks, &thumbs));
        let labels = texts(&cmds).len();
        assert_eq!(labels, l.page(), "one label per visible tile");
    }

    #[test]
    fn tiles_past_the_end_of_the_listing_are_not_drawn() {
        let l = layout();
        let entries: Vec<Entry> = (0..3).map(|i| entry(&format!("f{i}"), false)).collect();
        let marks = HashSet::new();
        let thumbs = Thumbs::new(64, 1, 8);
        let labels = texts(&grid_commands(&view(&l, &entries, &marks, &thumbs))).len();
        assert_eq!(labels, 3);
    }

    #[test]
    fn a_list_row_carries_the_name_and_both_detail_columns() {
        let l = Layout::compute(120, 40, 9.0, 20.0, View::List, 0, 2);
        let entries = vec![entry("sub", true), entry("photo.png", false)];
        let marks = HashSet::new();
        let thumbs = Thumbs::new(64, 1, 8);
        let cmds = grid_commands(&view(&l, &entries, &marks, &thumbs));
        let t = texts(&cmds);
        // Per row: date, size, name — the name last, over the plate.
        assert_eq!(t.len(), 6, "{t:?}");
        assert_eq!(t[2], "sub/", "a directory keeps its trailing slash");
        assert_eq!(t[1], "dir");
        assert_eq!(t[5], "photo.png");
        assert_eq!(t[4], "0B");
        assert!(t[0].starts_with("19") || t[0].starts_with("20"), "{}", t[0]);
    }

    #[test]
    fn a_narrow_list_drops_the_detail_columns_for_the_name() {
        let marks = HashSet::new();
        let thumbs = Thumbs::new(64, 1, 8);
        let entries = vec![entry("a-fairly-long-name.txt", false)];
        // 24 columns: too narrow for either column, so only the name.
        let narrow = Layout::compute(24, 20, 9.0, 20.0, View::List, 0, 0);
        assert_eq!(texts(&grid_commands(&view(&narrow, &entries, &marks, &thumbs))).len(), 1);
        // Wide enough for the size but not the date.
        let mid = Layout::compute(40, 20, 9.0, 20.0, View::List, 0, 0);
        assert_eq!(texts(&grid_commands(&view(&mid, &entries, &marks, &thumbs))).len(), 2);
    }

    #[test]
    fn every_visible_list_row_draws_its_name() {
        let l = Layout::compute(120, 40, 9.0, 20.0, View::List, 0, 2);
        let entries: Vec<Entry> = (0..200).map(|i| entry(&format!("f{i}.txt"), false)).collect();
        let marks = HashSet::new();
        let thumbs = Thumbs::new(64, 1, 8);
        let mut v = view(&l, &entries, &marks, &thumbs);
        v.scroll_row = 5;
        let t = texts(&grid_commands(&v));
        for row in 0..l.grid_rows {
            let name = format!("f{}.txt", 5 + row);
            assert!(t.contains(&name), "{name} missing from {t:?}");
        }
    }

    #[test]
    fn an_empty_directory_says_so() {
        let l = layout();
        let marks = HashSet::new();
        let thumbs = Thumbs::new(64, 1, 8);
        let cmds = grid_commands(&view(&l, &[], &marks, &thumbs));
        assert!(matches!(cmds.as_slice(), [DrawCmd::DrawText { .. }]));
    }

    #[test]
    fn the_status_line_reports_position_and_flags() {
        let opts = ListOpts {
            show_hidden: true,
            ..ListOpts::default()
        };
        let v = StatusView {
            area: Area {
                x: 0.0,
                y: 39.0,
                w: 120.0,
                h: 1.0,
            },
            cwd: Path::new("/home/user/pics"),
            index: 4,
            total: 57,
            marks: 3,
            view: View::Grid,
            opts: &opts,
            clip: Some((2, "copy")),
            message: None,
            busy: None,
        };
        let cmds = status_commands(&v, 9.0, 20.0);
        let texts: Vec<String> = cmds
            .iter()
            .filter_map(|c| match c {
                DrawCmd::DrawText { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        let right = &texts[0];
        assert!(right.contains("5/57"), "{right}");
        assert!(right.contains("marked 3") && right.contains("copy 2"));
        assert!(right.contains("hidden") && right.contains("name↑"));
        assert_eq!(texts[1], "/home/user/pics");
    }

    #[test]
    fn a_message_replaces_the_path_while_it_is_up() {
        let opts = ListOpts::default();
        let v = StatusView {
            area: Area {
                x: 0.0,
                y: 39.0,
                w: 120.0,
                h: 1.0,
            },
            cwd: Path::new("/tmp"),
            index: 0,
            total: 1,
            marks: 0,
            view: View::List,
            opts: &opts,
            clip: None,
            message: Some(("deleting: nope", true)),
            busy: None,
        };
        let left = |v: &StatusView| {
            status_commands(v, 9.0, 20.0)
                .into_iter()
                .rev()
                .find_map(|c| match c {
                    DrawCmd::DrawText { text, fill, .. } => Some((text, fill)),
                    _ => None,
                })
                .expect("the left text")
        };
        let (text, failed) = left(&v);
        assert_eq!(text, "deleting: nope");
        // A failure takes its own ink, not a plain message's.
        let (_, plain) = left(&StatusView {
            message: Some(("deleted", false)),
            ..v
        });
        assert_ne!(failed, plain);
    }

    /// Rows must sit where `column_row_at` expects them under every
    /// engine, so a click lands on the row it looks like it lands on.
    #[test]
    fn column_rows_sit_at_the_same_place_under_every_engine() {
        let area = Area {
            x: 0.0,
            y: 0.0,
            w: 14.0,
            h: 8.0,
        };
        let entries = [entry("a", false), entry("b", true)];
        let v = ColumnView {
            area,
            entries: &entries,
            selected: Some(1),
            error: None,
        };
        let px = Px { w: 12.0, h: 28.0 };
        let origins = |engine: &dyn Engine| -> Vec<(f32, f32)> {
            column_commands_in(engine, &v, px)
                .iter()
                .filter_map(|c| match c {
                    DrawCmd::DrawText { origin, .. } => Some((origin.x, origin.y)),
                    _ => None,
                })
                .collect()
        };
        let flat = origins(&Flat);
        assert_eq!(flat.len(), 2);
        assert_eq!(flat, origins(&vge_ui::style::aero::Aero));
    }
}
