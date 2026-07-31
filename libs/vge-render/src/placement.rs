//! Image placement math: map an image's pixel dimensions onto a cell
//! footprint that preserves its visual aspect ratio on an anisotropic
//! terminal cell grid. Moved verbatim from vcat.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Width of the rendered image in cells. Used both for the
    /// `target_rect.w` and to bound terminal-column reservation.
    pub w_cells: u32,
    /// `target_rect.h` in cells — fractional, set so the image keeps
    /// its true visual aspect ratio on this anisotropic cell grid.
    /// Ranges over (0, h_cells].
    pub target_rect_h: f32,
    /// Number of full rows to reserve via newlines. Equal to
    /// `target_rect_h.ceil()`. The bottom (h_cells - target_rect_h)
    /// fraction of a cell is empty whitespace below the image.
    pub h_cells: u32,
    /// Exact pixel target for resizing — preserves the image's pixel
    /// aspect ratio; the renderer stretches this onto target_rect.
    pub target_px_w: u32,
    pub target_px_h: u32,
}

/// Compute the cell footprint and exact pixel target for an image of
/// `w_px × h_px` displayed on a terminal with `cell_pw × cell_ph` pixel
/// cells and `term_cols` columns. If `forced_w_cells` is set, that's the
/// width; otherwise width is the image's natural width in cells clamped
/// to terminal columns.
pub fn compute_placement(
    w_px: u32,
    h_px: u32,
    cell_pw: f32,
    cell_ph: f32,
    term_cols: u32,
    forced_w_cells: Option<u32>,
) -> Placement {
    let cell_pw = cell_pw.max(1.0);
    let cell_ph = cell_ph.max(1.0);

    let natural_w_cells = ((w_px as f32) / cell_pw).ceil().max(1.0) as u32;
    let max_w_cells = match forced_w_cells {
        Some(w) if w > 0 => w,
        _ => term_cols.max(1),
    };
    let w_cells = natural_w_cells.min(max_w_cells).max(1);

    // Pixel target preserves the image's true aspect: we draw the
    // image at its natural ratio, and let target_rect.h be a
    // fractional number of cells so anisotropic cell grids don't
    // distort it.
    let target_px_w = (w_cells as f32 * cell_pw).round().max(1.0) as u32;
    let target_px_h = (target_px_w as f32 * h_px as f32 / w_px as f32)
        .round()
        .max(1.0) as u32;
    let target_rect_h = (target_px_h as f32 / cell_ph).max(1.0 / cell_ph);
    let h_cells = target_rect_h.ceil().max(1.0) as u32;

    Placement {
        w_cells,
        target_rect_h,
        h_cells,
        target_px_w,
        target_px_h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn placement_natural_size_when_smaller_than_terminal() {
        let p = compute_placement(100, 50, 10.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 10);
        assert!(approx_eq(p.target_rect_h, 2.5));
        assert_eq!(p.h_cells, 3);
        assert_eq!(p.target_px_w, 100);
        assert_eq!(p.target_px_h, 50);
    }

    #[test]
    fn placement_clamped_to_terminal_width() {
        let p = compute_placement(1000, 500, 10.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 80);
        assert!(approx_eq(p.target_rect_h, 20.0));
        assert_eq!(p.h_cells, 20);
        assert_eq!(p.target_px_w, 800);
        assert_eq!(p.target_px_h, 400);
    }

    #[test]
    fn placement_forced_width_overrides_natural_and_terminal() {
        let p = compute_placement(1000, 500, 10.0, 20.0, 80, Some(40));
        assert_eq!(p.w_cells, 40);
        assert!(approx_eq(p.target_rect_h, 10.0));
        assert_eq!(p.h_cells, 10);
    }

    #[test]
    fn placement_anisotropic_aspect_preserved() {
        let p = compute_placement(100, 100, 9.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 12);
        assert!(approx_eq(p.target_rect_h, 5.4));
        assert_eq!(p.h_cells, 6);
        assert_eq!(p.target_px_w, 108);
        assert_eq!(p.target_px_h, 108);
    }

    #[test]
    fn placement_minimum_one_cell() {
        let p = compute_placement(1, 1, 10.0, 20.0, 80, None);
        assert_eq!(p.w_cells, 1);
        assert!(p.h_cells >= 1);
        assert!(p.target_rect_h > 0.0);
    }
}
