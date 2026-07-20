//! In-progress shape creation: the drag rectangle and grid snapping.
//!
//! SGR mouse reports arrive in whole cells, so the pointer can only ever
//! land on a cell boundary anyway. Rather than fight that, geometry
//! snaps to a fixed doc-space grid one cell wide and one cell tall —
//! which is what makes block diagrams line up for free.
//!
//! The grid is defined in *doc* px (multiples of the probe's cell size),
//! not screen cells, so shapes stay on the same grid regardless of the
//! zoom they were drawn at.

use vge_protocol::codec::Point;

use crate::camera::Camera;

/// Smallest drag, in grid cells, that commits an element. Anything
/// under this is treated as a stray click rather than a degenerate
/// zero-size shape.
const MIN_CELLS: f32 = 1.0;

#[derive(Debug, Clone, Copy)]
pub struct Drag {
    /// Where the press landed, in snapped doc px.
    pub start: Point,
    /// Where the pointer is now, in snapped doc px.
    pub current: Point,
}

impl Drag {
    pub fn new(start: Point) -> Self {
        Self {
            start,
            current: start,
        }
    }

    /// Signed extent, `(x, y, w, h)`. Width and height may be negative:
    /// polyline tools need the direction to know which end is the tip,
    /// and `ToolState::new_element` normalises for the shapes that care.
    pub fn extent(&self) -> (f32, f32, f32, f32) {
        (
            self.start.x,
            self.start.y,
            self.current.x - self.start.x,
            self.current.y - self.start.y,
        )
    }

    /// Whether the drag is big enough to be worth committing.
    pub fn is_significant(&self, cam: &Camera) -> bool {
        let (_, _, w, h) = self.extent();
        w.abs() >= cam.cell_w * MIN_CELLS || h.abs() >= cam.cell_h * MIN_CELLS
    }
}

/// Quantise a doc-space point to the grid.
///
/// Grid points sit at cell *centres*, offset half a cell from the cell
/// boundaries. A mouse report only identifies a cell, so the pointer's
/// best-estimate position is that cell's centre — and snapping to a
/// grid of centres lands exactly there, with no bias in any direction.
/// Snapping to boundaries instead would put every click precisely
/// halfway between two grid points, where the tie-break shifts every
/// shape half a cell off the drag that made it.
///
/// Spacing is still one cell, so shapes remain a whole number of cells
/// wide and align with each other exactly as before.
pub fn snap(p: Point, cam: &Camera) -> Point {
    Point {
        x: ((p.x / cam.cell_w - 0.5).round() + 0.5) * cam.cell_w,
        y: ((p.y / cam.cell_h - 0.5).round() + 0.5) * cam.cell_h,
    }
}

/// Snap the doc-space point under a screen cell — the usual entry point
/// from a mouse event.
pub fn snap_screen(col: u16, row: u16, cam: &Camera) -> Point {
    snap(cam.pointer_to_doc(col, row), cam)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera::new(8.0, 17.0)
    }

    /// Grid points sit at cell centres — half a cell off the boundaries,
    /// one cell apart.
    #[test]
    fn snap_lands_on_cell_centres() {
        let c = cam();
        let p = snap(Point { x: 19.0, y: 40.0 }, &c);
        assert_eq!(p.x, 20.0); // centre of cell 2: (2 + 0.5) * 8
        assert_eq!(p.y, 42.5); // centre of cell 2: (2 + 0.5) * 17
        assert_eq!((p.x - c.cell_w / 2.0) % c.cell_w, 0.0);
        assert_eq!((p.y - c.cell_h / 2.0) % c.cell_h, 0.0);
    }

    /// The whole point of the centre grid: the snapped point is exactly
    /// where the pointer is estimated to be, not half a cell away.
    #[test]
    fn snapping_a_click_is_lossless() {
        let c = cam();
        for col in 0..12u16 {
            for row in 0..5u16 {
                let p = c.pointer_to_doc(col, row);
                let s = snap_screen(col, row, &c);
                assert!(
                    (p.x - s.x).abs() < 1e-3 && (p.y - s.y).abs() < 1e-3,
                    "cell ({col},{row}) moved from ({}, {}) to ({}, {})",
                    p.x,
                    p.y,
                    s.x,
                    s.y
                );
            }
        }
    }

    #[test]
    fn snap_is_idempotent() {
        let c = cam();
        let once = snap(Point { x: 123.4, y: 56.7 }, &c);
        assert_eq!(snap(once, &c).x, once.x);
        assert_eq!(snap(once, &c).y, once.y);
    }

    #[test]
    fn snap_screen_steps_one_cell_per_column() {
        let c = cam();
        let mut prev = snap_screen(0, 3, &c).x;
        for col in 1..20u16 {
            let x = snap_screen(col, 3, &c).x;
            assert_eq!(x - prev, c.cell_w, "col {col} did not advance one cell");
            prev = x;
        }
    }

    #[test]
    fn extent_keeps_direction_signed() {
        let d = Drag {
            start: Point { x: 80.0, y: 34.0 },
            current: Point { x: 16.0, y: 0.0 },
        };
        let (x, y, w, h) = d.extent();
        assert_eq!((x, y), (80.0, 34.0));
        assert!(w < 0.0 && h < 0.0, "backwards drag must stay negative");
    }

    /// The whole point of snapping: a shape drawn from snapped
    /// endpoints must be a whole number of cells, in *document* space,
    /// not merely look aligned at the zoom it was drawn at. Its origin
    /// sits on the centre grid, so shapes still align with each other.
    #[test]
    fn snapped_drag_produces_grid_aligned_geometry() {
        use crate::tools::{Tool, ToolState};
        let c = cam();
        let st = ToolState {
            tool: Tool::Box,
            ..Default::default()
        };
        let mut d = Drag::new(snap(Point { x: 19.0, y: 40.0 }, &c));
        d.current = snap(Point { x: 131.0, y: 99.0 }, &c);
        let (x, y, w, h) = d.extent();
        let e = st.new_element("t", x, y, w, h).expect("element");
        assert_eq!(
            (e.x - c.cell_w / 2.0) % c.cell_w,
            0.0,
            "x off the centre grid: {}",
            e.x
        );
        assert_eq!(
            (e.y - c.cell_h / 2.0) % c.cell_h,
            0.0,
            "y off the centre grid: {}",
            e.y
        );
        assert_eq!(e.width % c.cell_w, 0.0, "w not a whole cell count: {}", e.width);
        assert_eq!(e.height % c.cell_h, 0.0, "h not a whole cell count: {}", e.height);
    }

    #[test]
    fn tiny_drags_are_not_significant() {
        let c = cam();
        let start = Point { x: 0.0, y: 0.0 };
        assert!(!Drag::new(start).is_significant(&c));

        let mut d = Drag::new(start);
        d.current = Point { x: 8.0, y: 0.0 };
        assert!(d.is_significant(&c), "one full cell should commit");

        // Backwards drags count by magnitude, not sign.
        let mut back = Drag::new(start);
        back.current = Point { x: -17.0, y: 0.0 };
        assert!(back.is_significant(&c));
    }
}
