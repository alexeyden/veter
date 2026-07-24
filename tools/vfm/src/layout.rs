//! Where everything sits on the grid.
//!
//! The window is one status row at the bottom, and above it a row of
//! panes: zero to two narrow *ancestor* columns on the left (the
//! grandparent and parent listings, ranger-style) and the current
//! directory as a wide icon grid filling the rest.
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

/// Tile widths the `+` / `-` keys step through, in cells.
pub const TILE_WIDTHS: [f32; 4] = [8.0, 12.0, 16.0, 24.0];
/// Index into [`TILE_WIDTHS`] a fresh session starts at.
pub const DEFAULT_TILE_ZOOM: usize = 1;

/// Rows of a tile spent on its label rather than its picture.
const LABEL_ROWS: f32 = 1.0;
/// Gap between tiles, in cells (horizontal) and rows (vertical).
const TILE_GAP_X: f32 = 1.0;
const TILE_GAP_Y: f32 = 1.0;

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
    /// Viewport of the icon grid.
    pub grid: Area,
    /// Full-width status row along the bottom.
    pub status: Area,
    /// One tile's footprint, including its label row.
    pub tile_w: f32,
    pub tile_h: f32,
    /// Rows of a tile given over to the picture.
    pub tile_img_h: f32,
    /// Tiles per grid row, and fully-visible rows in the viewport.
    pub grid_cols: usize,
    pub grid_rows: usize,
}

impl Layout {
    /// Lay the window out for `depth` available ancestor directories
    /// (0..=2 are used; more are ignored) at tile-zoom `zoom`.
    pub fn compute(
        cols: u32,
        rows: u32,
        cell_pw: f32,
        cell_ph: f32,
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

        let tile_w = TILE_WIDTHS[zoom.min(TILE_WIDTHS.len() - 1)];
        // Square picture area: the same pixel extent across and down.
        let tile_img_h = ((tile_w * cell_pw) / cell_ph.max(1.0)).round().max(1.0);
        let tile_h = tile_img_h + LABEL_ROWS;

        let grid_cols = (((grid.w + TILE_GAP_X) / (tile_w + TILE_GAP_X)).floor() as usize).max(1);
        let grid_rows = (((grid.h + TILE_GAP_Y) / (tile_h + TILE_GAP_Y)).floor() as usize).max(1);

        Layout {
            cols,
            rows,
            ancestors,
            grid,
            status,
            tile_w,
            tile_h,
            tile_img_h,
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
        let x = self.grid.x + col as f32 * (self.tile_w + TILE_GAP_X);
        let y = self.grid.y + (row as f32 - scroll_row as f32) * (self.tile_h + TILE_GAP_Y);
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
        let step_x = self.tile_w + TILE_GAP_X;
        let step_y = self.tile_h + TILE_GAP_Y;
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
        Layout::compute(120, 40, 9.0, 20.0, DEFAULT_TILE_ZOOM, depth)
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
        let narrow = Layout::compute(34, 20, 9.0, 20.0, DEFAULT_TILE_ZOOM, 2);
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
