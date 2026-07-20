//! Pan / zoom, and the three coordinate spaces the editor lives in.
//!
//! - **doc px** — Excalidraw's device-independent pixels. What's saved.
//! - **canvas cells** — doc px divided by the probe's cell pixel size.
//!   Dividing each axis independently is what makes anisotropic cells
//!   (§5.1) cancel out: a square in doc space is a square on screen,
//!   with no per-shape aspect fudging.
//! - **screen cells** — canvas cells after the camera's zoom and pan.
//!   This is what SGR mouse reports arrive in.
//!
//! The camera is expressed entirely as the canvas element's affine
//! transform (§9.11), which is why pan and zoom never resend geometry:
//! with `L = diag(z, z)` and translation `t` in cell units, §9.11's
//! `pixel = O + L·(S·p) + S·t` collapses to
//!
//! ```text
//! screen_cell = zoom · canvas_cell + pan
//! ```

use vge_protocol::codec::{Point, Transform};

pub const MIN_ZOOM: f32 = 0.1;
pub const MAX_ZOOM: f32 = 8.0;
const ZOOM_STEP: f32 = 1.15;

#[derive(Debug, Clone, Copy)]
pub struct Camera {
    pub zoom: f32,
    /// Translation in *cell* units, per §9.11's split.
    pub pan: Point,
    /// Probe cell pixel size — the doc-px <-> canvas-cell divisor.
    pub cell_w: f32,
    pub cell_h: f32,
}

impl Camera {
    pub fn new(cell_w: f32, cell_h: f32) -> Self {
        Self {
            zoom: 1.0,
            pan: Point { x: 0.0, y: 0.0 },
            cell_w: cell_w.max(1.0),
            cell_h: cell_h.max(1.0),
        }
    }

    /// The canvas element's transform. Pan/zoom is exactly this, sent as
    /// one `UpdateTransform` — no geometry ever moves on the wire.
    pub fn transform(&self) -> Transform {
        Transform {
            a: self.zoom,
            b: 0.0,
            c: 0.0,
            d: self.zoom,
            e: self.pan.x,
            f: self.pan.y,
        }
    }

    /// doc px -> canvas cells (pre-transform; what `render.rs` emits).
    pub fn doc_to_canvas(&self, x: f32, y: f32) -> Point {
        Point {
            x: x / self.cell_w,
            y: y / self.cell_h,
        }
    }

    /// Screen cell -> doc px. The inverse used to turn a mouse report
    /// back into a document coordinate.
    pub fn screen_to_doc(&self, col: f32, row: f32) -> Point {
        Point {
            x: (col - self.pan.x) * self.cell_w / self.zoom,
            y: (row - self.pan.y) * self.cell_h / self.zoom,
        }
    }

    pub fn pan_by(&mut self, dcols: f32, drows: f32) {
        self.pan.x += dcols;
        self.pan.y += drows;
    }

    /// Zoom about a screen-cell anchor, keeping the document point under
    /// the cursor pinned.
    pub fn zoom_at(&mut self, factor: f32, col: f32, row: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let ratio = new_zoom / self.zoom;
        if ratio == 1.0 {
            return;
        }
        self.pan.x = col - ratio * (col - self.pan.x);
        self.pan.y = row - ratio * (row - self.pan.y);
        self.zoom = new_zoom;
    }

    pub fn zoom_in_at(&mut self, col: f32, row: f32) {
        self.zoom_at(ZOOM_STEP, col, row);
    }

    pub fn zoom_out_at(&mut self, col: f32, row: f32) {
        self.zoom_at(1.0 / ZOOM_STEP, col, row);
    }

    pub fn reset(&mut self) {
        self.zoom = 1.0;
        self.pan = Point { x: 0.0, y: 0.0 };
    }

    pub fn zoom_percent(&self) -> u32 {
        (self.zoom * 100.0).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera::new(8.0, 17.0)
    }

    #[test]
    fn screen_to_doc_inverts_the_forward_mapping() {
        let mut c = cam();
        c.zoom = 1.75;
        c.pan = Point { x: 3.5, y: -2.25 };
        let (dx, dy) = (321.0, 88.0);
        // Forward: doc -> canvas -> screen, per the module doc comment.
        let canvas = c.doc_to_canvas(dx, dy);
        let (col, row) = (
            c.zoom * canvas.x + c.pan.x,
            c.zoom * canvas.y + c.pan.y,
        );
        let back = c.screen_to_doc(col, row);
        assert!((back.x - dx).abs() < 1e-3, "{} vs {dx}", back.x);
        assert!((back.y - dy).abs() < 1e-3, "{} vs {dy}", back.y);
    }

    #[test]
    fn zoom_at_pins_the_cursor_point() {
        let mut c = cam();
        let (col, row) = (40.0, 12.0);
        let before = c.screen_to_doc(col, row);
        c.zoom_in_at(col, row);
        c.zoom_in_at(col, row);
        c.zoom_out_at(col, row);
        let after = c.screen_to_doc(col, row);
        assert!((before.x - after.x).abs() < 1e-3);
        assert!((before.y - after.y).abs() < 1e-3);
    }

    #[test]
    fn zoom_clamps() {
        let mut c = cam();
        for _ in 0..200 {
            c.zoom_in_at(0.0, 0.0);
        }
        assert!(c.zoom <= MAX_ZOOM);
        for _ in 0..400 {
            c.zoom_out_at(0.0, 0.0);
        }
        assert!(c.zoom >= MIN_ZOOM);
    }
}
