//! Where everything sits on the grid.
//!
//! The window is one status row at the bottom, and above it a row of
//! panes: zero to two narrow *ancestor* columns on the left (the
//! grandparent and parent listings, ranger-style) and the current
//! directory filling the rest — either as a wide icon grid or as a
//! one-row-per-entry detail list ([`View`]).
//!
//! Both layouts are the *same* geometry with different numbers in it: a
//! list is a grid one tile wide, whose tile spans the pane and carries
//! its picture at the left rather than above the label. Everything the
//! caller needs — [`Layout::page`], [`Layout::tile_origin`],
//! [`Layout::tile_at`], [`Layout::scroll_for`] — is therefore shared, so
//! scrolling, hit-testing and thumbnail prefetch have one implementation.
//!
//! Tile geometry is derived from the terminal's pixel-per-cell ratios so
//! a tile's picture area is visually square whatever the font metrics.

/// A rectangle in cell coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Area {
    pub fn contains(&self, col: f32, row: f32) -> bool {
        col >= self.x && col < self.x + self.w && row >= self.y && row < self.y + self.h
    }
}

/// How the current directory is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Dolphin-style icon grid: a tile per entry, picture above the name.
    Grid,
    /// Detail list: a row per entry — small picture, name, size, date.
    List,
}

impl View {
    pub fn toggle(self) -> View {
        match self {
            View::Grid => View::List,
            View::List => View::Grid,
        }
    }

    /// Index into per-view state, so each layout keeps its own zoom.
    pub fn index(self) -> usize {
        match self {
            View::Grid => 0,
            View::List => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Grid => "grid",
            View::List => "list",
        }
    }

    /// The sizes `+` / `-` step through in this view: tile widths in the
    /// grid, row heights in the list.
    pub fn zoom_steps(self) -> &'static [f32] {
        match self {
            View::Grid => &TILE_WIDTHS,
            View::List => &LIST_ROW_HEIGHTS,
        }
    }

    /// Step a fresh session starts at.
    pub fn default_zoom(self) -> usize {
        match self {
            View::Grid => DEFAULT_TILE_ZOOM,
            View::List => DEFAULT_LIST_ZOOM,
        }
    }
}

/// Number of views, for per-view state arrays.
pub const VIEWS: usize = 2;

/// Tile widths the `+` / `-` keys step through, in cells.
pub const TILE_WIDTHS: [f32; 4] = [8.0, 12.0, 16.0, 24.0];
/// Index into [`TILE_WIDTHS`] a fresh session starts at.
pub const DEFAULT_TILE_ZOOM: usize = 1;

/// List row heights the same keys step through, in rows. One row is the
/// dense default a list is chosen for; taller rows buy a legible
/// thumbnail without giving up the columns.
pub const LIST_ROW_HEIGHTS: [f32; 4] = [1.0, 2.0, 3.0, 5.0];
/// Index into [`LIST_ROW_HEIGHTS`] a fresh session starts at.
pub const DEFAULT_LIST_ZOOM: usize = 0;

/// Rows of a tile spent on its label rather than its picture.
const LABEL_ROWS: f32 = 1.0;
/// Gap between tiles, in cells (horizontal) and rows (vertical).
const TILE_GAP_X: f32 = 1.0;
const TILE_GAP_Y: f32 = 1.0;

/// Cells along the pane's right edge the scrollbar draws in (`render`
/// puts it 0.6 in). List rows stop short of it so a long name — or the
/// date column — never runs underneath.
const SCROLLBAR_COLS: f32 = 1.0;

/// Fraction of the window each ancestor column gets, innermost first.
/// The grid keeps whatever is left, and never less than
/// [`MIN_GRID_COLS`].
const ANCESTOR_FRACTIONS: [f32; 2] = [0.16, 0.11];
const MIN_ANCESTOR_COLS: f32 = 10.0;
const MIN_GRID_COLS: f32 = 20.0;

#[derive(Debug, Clone)]
pub struct Layout {
    pub cols: u32,
    pub rows: u32,
    /// Ancestor columns, outermost (grandparent) first — so they read
    /// left to right in draw order.
    pub ancestors: Vec<Area>,
    /// Viewport of the current directory's pane.
    pub grid: Area,
    /// Full-width status row along the bottom.
    pub status: Area,
    /// How the pane draws its entries.
    pub view: View,
    /// One tile's footprint, including its label row.
    pub tile_w: f32,
    pub tile_h: f32,
    /// Rows of a tile given over to the picture. In the list that is the
    /// whole row — the picture sits at its left edge, not above a label.
    pub tile_img_h: f32,
    /// Space between tiles. Zero in the list, which draws its rows
    /// contiguously.
    pub gap_x: f32,
    pub gap_y: f32,
    /// Tiles per grid row (always 1 in the list), and fully-visible rows
    /// in the viewport.
    pub grid_cols: usize,
    pub grid_rows: usize,
}

impl Layout {
    /// Lay the window out for `depth` available ancestor directories
    /// (0..=2 are used; more are ignored), drawing the current directory
    /// as `view` at size step `zoom`.
    pub fn compute(
        cols: u32,
        rows: u32,
        cell_pw: f32,
        cell_ph: f32,
        view: View,
        zoom: usize,
        depth: usize,
    ) -> Layout {
        let w = cols.max(1) as f32;
        let h = rows.max(1) as f32;
        let status = Area {
            x: 0.0,
            y: (h - 1.0).max(0.0),
            w,
            h: 1.0,
        };
        let body_h = (h - 1.0).max(1.0);

        // Ancestor columns are taken from the left, innermost last, but
        // only while the grid keeps a usable width.
        let mut widths: Vec<f32> = Vec::new();
        let mut used = 0.0;
        for i in 0..depth.min(ANCESTOR_FRACTIONS.len()) {
            let want = (w * ANCESTOR_FRACTIONS[i]).round().max(MIN_ANCESTOR_COLS);
            if w - used - want < MIN_GRID_COLS {
                break;
            }
            used += want;
            widths.push(want);
        }
        // `widths[0]` is the immediate parent; draw order is outermost
        // first, so reverse into screen order.
        widths.reverse();

        let mut ancestors = Vec::new();
        let mut x = 0.0;
        for cw in &widths {
            ancestors.push(Area {
                x,
                y: 0.0,
                w: *cw,
                h: body_h,
            });
            x += cw;
        }

        let grid = Area {
            x,
            y: 0.0,
            w: (w - x).max(1.0),
            h: body_h,
        };

        let steps = view.zoom_steps();
        let step = steps[zoom.min(steps.len() - 1)];
        let (tile_w, tile_h, tile_img_h, gap_x, gap_y) = match view {
            View::Grid => {
                // Square picture area: the same pixel extent across and down.
                let img_h = ((step * cell_pw) / cell_ph.max(1.0)).round().max(1.0);
                (step, img_h + LABEL_ROWS, img_h, TILE_GAP_X, TILE_GAP_Y)
            }
            // One row spanning the pane, picture and columns inside it.
            View::List => (
                (grid.w - SCROLLBAR_COLS).max(1.0),
                step,
                step,
                0.0,
                0.0,
            ),
        };

        let grid_cols = match view {
            View::Grid => (((grid.w + gap_x) / (tile_w + gap_x)).floor() as usize).max(1),
            View::List => 1,
        };
        let grid_rows = (((grid.h + gap_y) / (tile_h + gap_y)).floor() as usize).max(1);

        Layout {
            cols,
            rows,
            ancestors,
            grid,
            status,
            view,
            tile_w,
            tile_h,
            tile_img_h,
            gap_x,
            gap_y,
            grid_cols,
            grid_rows,
        }
    }

    /// Number of tiles that fit in the viewport at once.
    pub fn page(&self) -> usize {
        self.grid_cols * self.grid_rows
    }

    /// Top-left corner of tile `index` within the grid, given the first
    /// visible row. Coordinates are absolute cells.
    pub fn tile_origin(&self, index: usize, scroll_row: usize) -> (f32, f32) {
        let row = index / self.grid_cols;
        let col = index % self.grid_cols;
        let x = self.grid.x + col as f32 * (self.tile_w + self.gap_x);
        let y = self.grid.y + (row as f32 - scroll_row as f32) * (self.tile_h + self.gap_y);
        (x, y)
    }

    /// The tile index under a click, if the pointer is over one (and not
    /// in the gap between tiles).
    pub fn tile_at(&self, col: f32, row: f32, scroll_row: usize, count: usize) -> Option<usize> {
        if !self.grid.contains(col, row) {
            return None;
        }
        let rel_x = col - self.grid.x;
        let rel_y = row - self.grid.y;
        let step_x = self.tile_w + self.gap_x;
        let step_y = self.tile_h + self.gap_y;
        let cx = (rel_x / step_x).floor();
        let cy = (rel_y / step_y).floor();
        if rel_x - cx * step_x >= self.tile_w || rel_y - cy * step_y >= self.tile_h {
            return None; // in the gutter
        }
        if cx as usize >= self.grid_cols {
            return None;
        }
        let index = (cy as usize + scroll_row) * self.grid_cols + cx as usize;
        (index < count).then_some(index)
    }

    /// Scroll position (in tile rows) that keeps `cursor` visible, given
    /// the current one.
    pub fn scroll_for(&self, cursor: usize, scroll_row: usize) -> usize {
        let row = cursor / self.grid_cols;
        if row < scroll_row {
            row
        } else if row >= scroll_row + self.grid_rows {
            row + 1 - self.grid_rows
        } else {
            scroll_row
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(depth: usize) -> Layout {
        Layout::compute(120, 40, 9.0, 20.0, View::Grid, DEFAULT_TILE_ZOOM, depth)
    }

    fn list(zoom: usize) -> Layout {
        Layout::compute(120, 40, 9.0, 20.0, View::List, zoom, 1)
    }

    #[test]
    fn panes_tile_the_window_without_overlap() {
        let l = layout(2);
        assert_eq!(l.ancestors.len(), 2);
        let mut x = 0.0;
        for a in &l.ancestors {
            assert_eq!(a.x, x);
            x += a.w;
        }
        assert_eq!(l.grid.x, x);
        assert_eq!(l.grid.x + l.grid.w, l.cols as f32);
        assert_eq!(l.status.y, l.rows as f32 - 1.0);
        assert_eq!(l.grid.h + l.status.h, l.rows as f32);
    }

    #[test]
    fn ancestor_columns_are_dropped_before_the_grid_gets_cramped() {
        let narrow = Layout::compute(34, 20, 9.0, 20.0, View::Grid, DEFAULT_TILE_ZOOM, 2);
        assert!(narrow.grid.w >= MIN_GRID_COLS);
        assert!(narrow.ancestors.len() < 2);
        let root = layout(0);
        assert!(root.ancestors.is_empty(), "at / there is no parent");
    }

    #[test]
    fn tile_picture_area_is_visually_square() {
        let l = layout(1);
        let px_w = l.tile_w * 9.0;
        let px_h = l.tile_img_h * 20.0;
        assert!((px_w - px_h).abs() <= 20.0, "{px_w} vs {px_h}");
    }

    #[test]
    fn tile_origin_and_hit_test_agree() {
        let l = layout(1);
        for index in [0usize, 1, 5, 9] {
            let (x, y) = l.tile_origin(index, 0);
            assert_eq!(l.tile_at(x + 0.5, y + 0.5, 0, 100), Some(index));
        }
        // The gutter between tiles selects nothing.
        let (x, _) = l.tile_origin(1, 0);
        assert_eq!(l.tile_at(x - 0.5, 0.5, 0, 100), None);
        // Past the end of the listing, nothing.
        assert_eq!(l.tile_at(l.grid.x + 0.5, 0.5, 0, 0), None);
    }

    #[test]
    fn hit_test_accounts_for_scroll() {
        let l = layout(1);
        let (x, y) = l.tile_origin(l.grid_cols, 1);
        // Row 1 scrolled to the top is the first visible row.
        assert_eq!(y, l.grid.y);
        assert_eq!(l.tile_at(x + 0.5, y + 0.5, 1, 100), Some(l.grid_cols));
    }

    #[test]
    fn the_list_is_one_tile_wide_and_fills_the_pane_height() {
        let l = list(DEFAULT_LIST_ZOOM);
        assert_eq!(l.grid_cols, 1, "one entry per row");
        assert_eq!(l.tile_h, 1.0, "the default step is a dense one-row list");
        // Rows are contiguous, so the pane's whole height is usable, bar
        // the column the scrollbar takes out of each row's width.
        assert_eq!(l.grid_rows, l.grid.h as usize);
        assert_eq!(l.page(), l.grid_rows);
        assert!(l.tile_w < l.grid.w && l.tile_w > l.grid.w - 2.0);
    }

    #[test]
    fn taller_list_rows_show_fewer_of_them() {
        let dense = list(0);
        let tall = list(LIST_ROW_HEIGHTS.len() - 1);
        assert!(tall.tile_h > dense.tile_h);
        assert!(tall.grid_rows < dense.grid_rows);
        // Row `n` starts exactly where row `n-1` ended — no gutter.
        let (_, y0) = tall.tile_origin(0, 0);
        let (_, y1) = tall.tile_origin(1, 0);
        assert_eq!(y1 - y0, tall.tile_h);
    }

    #[test]
    fn list_rows_hit_test_where_they_are_drawn() {
        for zoom in 0..LIST_ROW_HEIGHTS.len() {
            let l = list(zoom);
            for index in [0usize, 1, 3] {
                let (x, y) = l.tile_origin(index, 0);
                assert_eq!(l.tile_at(x + 0.5, y + 0.5, 0, 100), Some(index), "zoom {zoom}");
            }
            // Scroll moves which entry the top row is.
            let (x, y) = l.tile_origin(4, 4);
            assert_eq!(y, l.grid.y);
            assert_eq!(l.tile_at(x + 0.5, y + 0.5, 4, 100), Some(4));
        }
    }

    #[test]
    fn scrolling_follows_the_cursor_in_both_directions() {
        let l = layout(1);
        let page = l.page();
        assert_eq!(l.scroll_for(0, 0), 0);
        // A cursor one row past the viewport scrolls by exactly one row.
        let first_below = page + l.grid_cols - 1;
        assert_eq!(l.scroll_for(first_below, 0), 1);
        // Moving back up snaps to the cursor's row.
        assert_eq!(l.scroll_for(0, 5), 0);
        // A cursor already in view leaves the scroll alone.
        assert_eq!(l.scroll_for(l.grid_cols, 1), 1);
    }
}
