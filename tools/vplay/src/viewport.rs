//! Pure zoom/pan math. No I/O — unit-testable in isolation.
//!
//! The viewport is a rectangular region of the terminal grid (in cells)
//! onto which a source image of `img_w × img_h` pixels is drawn at a
//! given `zoom` (device px per source px). `src_x/src_y` is the source
//! pixel shown at the top-left of the visible image region when the
//! image overflows the viewport; when the image is smaller than the
//! viewport along an axis it is centred (letterboxed) and that axis's
//! source offset is pinned to 0.

use vge_protocol::codec::Rect;

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;

/// Result of laying the image into the viewport: where to draw on the
/// grid (`target`, in cells) and which source sub-rectangle to sample
/// (`source`, in source pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub target: Rect,
    pub source: Rect,
}

#[derive(Clone, Debug)]
pub struct Viewport {
    pub img_w: f32,
    pub img_h: f32,
    pub cell_pw: f32,
    pub cell_ph: f32,
    /// Viewport top-left in cells (absolute on the grid).
    pub origin_col: f32,
    pub origin_row: f32,
    pub vp_cols: f32,
    pub vp_rows: f32,
    pub zoom: f32,
    pub src_x: f32,
    pub src_y: f32,
}

impl Viewport {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        img_w: u32,
        img_h: u32,
        cell_pw: f32,
        cell_ph: f32,
        origin_col: f32,
        origin_row: f32,
        vp_cols: f32,
        vp_rows: f32,
    ) -> Self {
        let mut v = Self {
            img_w: img_w.max(1) as f32,
            img_h: img_h.max(1) as f32,
            cell_pw: cell_pw.max(1.0),
            cell_ph: cell_ph.max(1.0),
            origin_col,
            origin_row,
            vp_cols: vp_cols.max(1.0),
            vp_rows: vp_rows.max(1.0),
            zoom: 1.0,
            src_x: 0.0,
            src_y: 0.0,
        };
        v.fit();
        v
    }

    /// Re-set the viewport geometry (e.g. after a resize), preserving
    /// zoom but re-clamping the pan.
    pub fn set_viewport(&mut self, origin_col: f32, origin_row: f32, vp_cols: f32, vp_rows: f32) {
        self.origin_col = origin_col;
        self.origin_row = origin_row;
        self.vp_cols = vp_cols.max(1.0);
        self.vp_rows = vp_rows.max(1.0);
        self.clamp_src();
    }

    pub fn vp_w_px(&self) -> f32 {
        self.vp_cols * self.cell_pw
    }
    pub fn vp_h_px(&self) -> f32 {
        self.vp_rows * self.cell_ph
    }

    pub fn zoom_percent(&self) -> u32 {
        (self.zoom * 100.0).round() as u32
    }

    /// Fit the whole image inside the viewport, centred.
    pub fn fit(&mut self) {
        let zx = self.vp_w_px() / self.img_w;
        let zy = self.vp_h_px() / self.img_h;
        self.zoom = zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM);
        self.src_x = 0.0;
        self.src_y = 0.0;
        self.clamp_src();
    }

    /// 1:1 source pixels to device pixels, centred.
    pub fn actual(&mut self) {
        self.zoom = 1.0;
        self.center();
    }

    fn center(&mut self) {
        let sw = self.vp_w_px() / self.zoom;
        let sh = self.vp_h_px() / self.zoom;
        self.src_x = ((self.img_w - sw) / 2.0).max(0.0);
        self.src_y = ((self.img_h - sh) / 2.0).max(0.0);
        self.clamp_src();
    }

    fn clamp_src(&mut self) {
        let sw = self.vp_w_px() / self.zoom;
        let sh = self.vp_h_px() / self.zoom;
        let maxx = (self.img_w - sw).max(0.0);
        let maxy = (self.img_h - sh).max(0.0);
        self.src_x = self.src_x.clamp(0.0, maxx);
        self.src_y = self.src_y.clamp(0.0, maxy);
    }

    /// Pan by a delta in cells (drag direction): moving the pointer
    /// right drags the image right, i.e. reveals source to the left.
    pub fn pan_cells(&mut self, dcols: f32, drows: f32) {
        self.src_x -= dcols * self.cell_pw / self.zoom;
        self.src_y -= drows * self.cell_ph / self.zoom;
        self.clamp_src();
    }

    /// Multiply zoom by `factor`, keeping the source point currently
    /// under the given absolute cell coordinate fixed where possible.
    pub fn zoom_at(&mut self, factor: f32, cursor_col: f32, cursor_row: f32) {
        let cx = ((cursor_col - self.origin_col) * self.cell_pw).clamp(0.0, self.vp_w_px());
        let cy = ((cursor_row - self.origin_row) * self.cell_ph).clamp(0.0, self.vp_h_px());
        let (sxp, syp) = self.cursor_source(cx, cy);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        // Re-derive the pan so the same source point lands under the
        // cursor on any axis where the image now overflows.
        if self.img_w * self.zoom > self.vp_w_px() {
            self.src_x = sxp - cx / self.zoom;
        } else {
            self.src_x = 0.0;
        }
        if self.img_h * self.zoom > self.vp_h_px() {
            self.src_y = syp - cy / self.zoom;
        } else {
            self.src_y = 0.0;
        }
        self.clamp_src();
    }

    /// Per-axis layout: (target_offset_px, target_w_px, source_off,
    /// source_w). When the displayed image fits, it is centred and the
    /// whole axis is sampled; when it overflows, the viewport is filled
    /// and a window of the source is sampled.
    fn axis(img: f32, zoom: f32, vp_px: f32, src: f32) -> (f32, f32, f32, f32) {
        let disp = img * zoom;
        if disp <= vp_px {
            let toff = (vp_px - disp) / 2.0;
            (toff, disp, 0.0, img)
        } else {
            let sw = vp_px / zoom;
            (0.0, vp_px, src.clamp(0.0, (img - sw).max(0.0)), sw)
        }
    }

    fn cursor_source(&self, cx: f32, cy: f32) -> (f32, f32) {
        let (txo, tw, sx, sw) = Self::axis(self.img_w, self.zoom, self.vp_w_px(), self.src_x);
        let (tyo, th, sy, sh) = Self::axis(self.img_h, self.zoom, self.vp_h_px(), self.src_y);
        let sxp = if cx <= txo {
            sx
        } else if cx >= txo + tw {
            sx + sw
        } else {
            sx + (cx - txo) / self.zoom
        };
        let syp = if cy <= tyo {
            sy
        } else if cy >= tyo + th {
            sy + sh
        } else {
            sy + (cy - tyo) / self.zoom
        };
        (sxp, syp)
    }

    /// Compute the on-grid target rect (cells) and source rect (px).
    pub fn layout(&self) -> Layout {
        let (txo, tw, sx, sw) = Self::axis(self.img_w, self.zoom, self.vp_w_px(), self.src_x);
        let (tyo, th, sy, sh) = Self::axis(self.img_h, self.zoom, self.vp_h_px(), self.src_y);
        Layout {
            target: Rect {
                x: self.origin_col + txo / self.cell_pw,
                y: self.origin_row + tyo / self.cell_ph,
                w: tw / self.cell_pw,
                h: th / self.cell_ph,
            },
            source: Rect {
                x: sx,
                y: sy,
                w: sw,
                h: sh,
            },
        }
    }

    /// Map an absolute cell coordinate to a source pixel, or `None` if
    /// the cursor is outside the drawn image (in the letterbox margins
    /// or off the viewport).
    pub fn cursor_pixel(&self, cursor_col: f32, cursor_row: f32) -> Option<(u32, u32)> {
        let cx = (cursor_col - self.origin_col) * self.cell_pw;
        let cy = (cursor_row - self.origin_row) * self.cell_ph;
        if cx < 0.0 || cy < 0.0 || cx > self.vp_w_px() || cy > self.vp_h_px() {
            return None;
        }
        let (txo, tw, sx, _) = Self::axis(self.img_w, self.zoom, self.vp_w_px(), self.src_x);
        let (tyo, th, sy, _) = Self::axis(self.img_h, self.zoom, self.vp_h_px(), self.src_y);
        if cx < txo || cx > txo + tw || cy < tyo || cy > tyo + th {
            return None;
        }
        let px = (sx + (cx - txo) / self.zoom).clamp(0.0, self.img_w - 1.0);
        let py = (sy + (cy - tyo) / self.zoom).clamp(0.0, self.img_h - 1.0);
        Some((px as u32, py as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_centers_small_image() {
        // 100x100 image, 9x20 px cells, 80x24 viewport.
        let vp = Viewport::new(100, 100, 9.0, 20.0, 0.0, 0.0, 80.0, 24.0);
        let l = vp.layout();
        // Whole image sampled.
        assert!((l.source.w - 100.0).abs() < 1e-3);
        assert!((l.source.h - 100.0).abs() < 1e-3);
        // Target fits inside viewport.
        assert!(l.target.w <= 80.0 + 1e-3);
        assert!(l.target.h <= 24.0 + 1e-3);
    }

    #[test]
    fn zoom_in_overflows_and_pans() {
        let mut vp = Viewport::new(1000, 1000, 10.0, 20.0, 0.0, 0.0, 40.0, 20.0);
        vp.actual(); // zoom 1.0 → 1000px image, viewport 400x400px → overflow
        let l = vp.layout();
        assert_eq!(l.target.w, 40.0);
        assert_eq!(l.target.h, 20.0);
        // Source window = viewport_px / zoom = 400x400.
        assert!((l.source.w - 400.0).abs() < 1e-3);
        assert!((l.source.h - 400.0).abs() < 1e-3);
        // Centred: src offset = (1000-400)/2 = 300.
        assert!((l.source.x - 300.0).abs() < 1e-3);
    }

    #[test]
    fn pan_clamps_to_bounds() {
        let mut vp = Viewport::new(1000, 1000, 10.0, 20.0, 0.0, 0.0, 40.0, 20.0);
        vp.actual();
        vp.pan_cells(-1000.0, -1000.0); // huge pan
        let l = vp.layout();
        // src_x clamped to img_w - src_w = 1000 - 400 = 600.
        assert!(l.source.x <= 600.0 + 1e-3);
        assert!(l.source.x >= 0.0);
    }

    #[test]
    fn cursor_pixel_in_letterbox_is_none() {
        let vp = Viewport::new(10, 10, 10.0, 10.0, 0.0, 0.0, 80.0, 24.0);
        // Top-left cell is well inside the letterbox margin for a tiny
        // centred image.
        assert_eq!(vp.cursor_pixel(0.5, 0.5), None);
    }
}
