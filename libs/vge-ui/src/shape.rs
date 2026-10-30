//! Rounded-rectangle path builders — the one visual language every
//! client's chrome is drawn in.
//!
//! Rounding is expressed in *cell* units, so an `rx` equal to `ry` would
//! look elliptical on the anisotropic cell grids terminals actually
//! have. [`chrome_corner_radii`] converts a y-radius into the matching
//! x-radius for a given cell aspect; call sites pass the pair down.

use vge_protocol::codec::Point;
use vge_protocol::path::{PathNode, PathSegment};

/// Slight corner rounding applied uniformly across chrome (modal
/// corners, title pills, active tab top edges). One value so all chrome
/// reads as part of the same visual language; conservative on purpose —
/// too aggressive and the chrome turns into a button. Y units only.
pub const CHROME_CORNER_RY: f32 = 0.175;

/// Compute an `(rx, ry)` pair in cell units that yields a visually
/// circular arc given the host's pixel-per-cell ratios. Clamps so a
/// corner never exceeds 40% of the rect's smaller dimension (which would
/// let arcs collapse into each other on narrow shapes). Scaled by the
/// user's `flat.rounding` ([`crate::style::Settings`]), since every
/// flat part is cornered through here.
pub fn chrome_corner_radii(width: f32, height: f32, cell_pw: f32, cell_ph: f32) -> (f32, f32) {
    corner_radii(
        width,
        height,
        cell_pw,
        cell_ph,
        crate::style::settings().rounding,
    )
}

fn corner_radii(width: f32, height: f32, cell_pw: f32, cell_ph: f32, rounding: f32) -> (f32, f32) {
    // In pixels, so the clamp keeps the corner a circle: clamping each
    // axis on its own would leave a one-row bar's corner wider than it
    // is tall once the radius outgrows the bar, and the bar pointed.
    let pw = cell_pw.max(f32::EPSILON);
    let r = (CHROME_CORNER_RY * rounding * cell_ph)
        .min(height * 0.4 * cell_ph)
        .min(width * 0.4 * pw);
    (r / pw, r / cell_ph.max(f32::EPSILON))
}

/// Build a closed rectangle path with selectable rounded corners.
/// Corner toggles are `(top_left, top_right, bottom_right, bottom_left)`
/// — `false` produces a square corner. Traversal is CCW in y-down coords
/// (matches femtovg's tessellator preference — see `brick_drawcmd` in
/// tools/breakout).
pub fn rounded_rect_path(x0: f32, y0: f32, x1: f32, y1: f32, rx: f32, ry: f32) -> Vec<PathSegment> {
    rounded_rect_path_corners(x0, y0, x1, y1, rx, ry, true, true, true, true)
}

/// [`rounded_rect_path`] with per-corner control.
#[allow(clippy::too_many_arguments)]
pub fn rounded_rect_path_corners(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rx: f32,
    ry: f32,
    tl: bool,
    tr: bool,
    br: bool,
    bl: bool,
) -> Vec<PathSegment> {
    // A zero radius is a square corner, not a degenerate arc — which
    // is what `flat.rounding = 0` asks every corner for.
    let round = rx > 0.0 && ry > 0.0;
    let (tl, tr, br, bl) = (tl && round, tr && round, br && round, bl && round);
    let arc = |dst: Point| PathNode::ArcEllipseTo {
        large: false,
        sweep: false,
        rx,
        ry,
        rotation: 0.0,
        dst,
    };
    let start_y = if tl { y0 + ry } else { y0 };
    let mut nodes: Vec<PathNode> = Vec::new();
    // Down the left edge.
    nodes.push(PathNode::VerticalLineTo {
        y: if bl { y1 - ry } else { y1 },
    });
    if bl {
        nodes.push(arc(Point { x: x0 + rx, y: y1 }));
    }
    // Across the bottom.
    nodes.push(PathNode::HorizontalLineTo {
        x: if br { x1 - rx } else { x1 },
    });
    if br {
        nodes.push(arc(Point { x: x1, y: y1 - ry }));
    }
    // Up the right edge.
    nodes.push(PathNode::VerticalLineTo {
        y: if tr { y0 + ry } else { y0 },
    });
    if tr {
        nodes.push(arc(Point { x: x1 - rx, y: y0 }));
    }
    // Across the top.
    nodes.push(PathNode::HorizontalLineTo {
        x: if tl { x0 + rx } else { x0 },
    });
    if tl {
        nodes.push(arc(Point { x: x0, y: y0 + ry }));
    }
    nodes.push(PathNode::ClosePath);
    vec![PathSegment {
        start: Point { x: x0, y: start_y },
        nodes,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_radii_compensate_for_cell_aspect() {
        // Cells twice as tall as wide → rx is half of ry in cell units,
        // which is the same number of pixels.
        let (rx, ry) = chrome_corner_radii(10.0, 4.0, 10.0, 20.0);
        assert!((ry - CHROME_CORNER_RY).abs() < 1e-6);
        assert!((rx - CHROME_CORNER_RY * 2.0).abs() < 1e-6);
    }

    /// `flat.rounding` scales the radius; at zero the corners are
    /// square, with no arcs at all.
    #[test]
    fn rounding_scales_the_corners() {
        let (rx, ry) = corner_radii(10.0, 4.0, 10.0, 20.0, 2.0);
        assert!((ry - CHROME_CORNER_RY * 2.0).abs() < 1e-6);
        assert!((rx - CHROME_CORNER_RY * 4.0).abs() < 1e-6);
        assert_eq!(corner_radii(10.0, 4.0, 10.0, 20.0, 0.0), (0.0, 0.0));
        let segs = rounded_rect_path(0.0, 0.0, 4.0, 2.0, 0.0, 0.0);
        assert!(
            !segs[0]
                .nodes
                .iter()
                .any(|n| matches!(n, PathNode::ArcEllipseTo { .. }))
        );
    }

    /// A radius bigger than a one-row bar allows is clamped as a circle:
    /// the corner stays as wide as it is tall, in pixels.
    #[test]
    fn a_clamped_corner_stays_round() {
        let (rx, ry) = corner_radii(20.0, 1.0, 10.0, 20.0, 4.0);
        assert!((ry - 0.4).abs() < 1e-6, "{ry}");
        assert!((rx * 10.0 - ry * 20.0).abs() < 1e-4, "{rx} {ry}");
    }

    #[test]
    fn corner_radii_clamp_on_small_rects() {
        let (rx, ry) = chrome_corner_radii(0.2, 0.2, 10.0, 10.0);
        assert!(rx <= 0.08 + 1e-6, "{rx}");
        assert!(ry <= 0.08 + 1e-6, "{ry}");
    }

    #[test]
    fn square_corners_emit_no_arcs() {
        let segs =
            rounded_rect_path_corners(0.0, 0.0, 4.0, 2.0, 0.2, 0.2, false, false, false, false);
        let arcs = segs[0]
            .nodes
            .iter()
            .filter(|n| matches!(n, PathNode::ArcEllipseTo { .. }))
            .count();
        assert_eq!(arcs, 0);
        assert!(matches!(segs[0].nodes.last(), Some(PathNode::ClosePath)));
    }

    #[test]
    fn all_round_emits_four_arcs_and_closes() {
        let segs = rounded_rect_path(0.0, 0.0, 8.0, 3.0, 0.2, 0.15);
        let arcs = segs[0]
            .nodes
            .iter()
            .filter(|n| matches!(n, PathNode::ArcEllipseTo { .. }))
            .count();
        assert_eq!(arcs, 4);
        assert!(matches!(segs[0].nodes.last(), Some(PathNode::ClosePath)));
        // Start sits on the left edge, below the top-left arc, so the
        // seam falls on a straight run rather than mid-curve.
        assert_eq!(segs[0].start.x, 0.0);
        assert!((segs[0].start.y - 0.15).abs() < 1e-6);
    }
}
