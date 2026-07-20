//! Hit testing and resize handles, all in document space.
//!
//! VGE delivers no input (§9.10), so every "what did I click on"
//! question is answered here against the document model rather than by
//! the terminal. Tolerances are passed in by the caller, which derives
//! them from the cell size — a click is only ever cell-accurate, so
//! sub-cell precision would be false confidence.

use vge_protocol::codec::Point;

use crate::doc::{Document, Element, Shape};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    NW,
    NE,
    SW,
    SE,
    /// Polyline first vertex.
    Start,
    /// Polyline last vertex.
    End,
}

/// Index of the topmost element under `p`, if any. Later elements draw
/// on top, so the search runs backwards.
pub fn hit_test(doc: &Document, p: Point, tol: f32) -> Option<usize> {
    doc.elements
        .iter()
        .enumerate()
        .rev()
        .find(|(_, e)| !e.is_deleted && e.container_id.is_none() && hits(e, p, tol))
        .map(|(i, _)| i)
}

/// Whether a point is on an element. Box-like shapes are hit anywhere
/// inside their bounds, not just on the outline — diagram shapes are
/// usually transparent, and outline-only hit testing makes them
/// frustrating to grab at cell precision.
pub fn hits(e: &Element, p: Point, tol: f32) -> bool {
    let Some(shape) = e.shape() else {
        return false;
    };
    let (cx, cy) = (e.x + e.width / 2.0, e.y + e.height / 2.0);
    let (hw, hh) = (e.width / 2.0, e.height / 2.0);
    match shape {
        Shape::Rectangle | Shape::Text => {
            p.x >= e.x - tol
                && p.x <= e.x + e.width + tol
                && p.y >= e.y - tol
                && p.y <= e.y + e.height + tol
        }
        Shape::Ellipse => {
            let rx = (hw + tol).max(f32::EPSILON);
            let ry = (hh + tol).max(f32::EPSILON);
            let (dx, dy) = ((p.x - cx) / rx, (p.y - cy) / ry);
            dx * dx + dy * dy <= 1.0
        }
        Shape::Diamond => {
            let rx = (hw + tol).max(f32::EPSILON);
            let ry = (hh + tol).max(f32::EPSILON);
            ((p.x - cx) / rx).abs() + ((p.y - cy) / ry).abs() <= 1.0
        }
        Shape::Line | Shape::Arrow => points_abs(e)
            .windows(2)
            .any(|w| dist_to_segment(p, w[0], w[1]) <= tol),
    }
}

/// Element vertices in absolute doc coordinates.
pub fn points_abs(e: &Element) -> Vec<Point> {
    e.points
        .iter()
        .map(|[px, py]| Point {
            x: e.x + px,
            y: e.y + py,
        })
        .collect()
}

fn dist_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let (vx, vy) = (b.x - a.x, b.y - a.y);
    let len2 = vx * vx + vy * vy;
    if len2 <= f32::EPSILON {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * vx + (p.y - a.y) * vy) / len2).clamp(0.0, 1.0);
    let (qx, qy) = (a.x + t * vx, a.y + t * vy);
    ((p.x - qx).powi(2) + (p.y - qy).powi(2)).sqrt()
}

/// Handle positions in doc space. Polylines expose their endpoints;
/// everything else exposes its four corners.
pub fn handles(e: &Element) -> Vec<(Handle, Point)> {
    match e.shape() {
        Some(Shape::Line) | Some(Shape::Arrow) => {
            let pts = points_abs(e);
            match (pts.first(), pts.last()) {
                (Some(a), Some(b)) if pts.len() >= 2 => {
                    vec![(Handle::Start, *a), (Handle::End, *b)]
                }
                _ => Vec::new(),
            }
        }
        Some(_) => vec![
            (Handle::NW, Point { x: e.x, y: e.y }),
            (
                Handle::NE,
                Point {
                    x: e.x + e.width,
                    y: e.y,
                },
            ),
            (
                Handle::SW,
                Point {
                    x: e.x,
                    y: e.y + e.height,
                },
            ),
            (
                Handle::SE,
                Point {
                    x: e.x + e.width,
                    y: e.y + e.height,
                },
            ),
        ],
        None => Vec::new(),
    }
}

/// Which handle, if any, is within `tol` of `p`.
pub fn handle_at(e: &Element, p: Point, tol: f32) -> Option<Handle> {
    handles(e)
        .into_iter()
        .find(|(_, q)| (p.x - q.x).abs() <= tol && (p.y - q.y).abs() <= tol)
        .map(|(h, _)| h)
}

/// Move an element by a doc-space delta. Polyline vertices are stored
/// relative to the bounding box, so only the origin moves.
pub fn translate(e: &mut Element, dx: f32, dy: f32) {
    e.x += dx;
    e.y += dy;
}

/// Drag a handle to `to`. `min_w`/`min_h` keep a shape from collapsing
/// to nothing; polyline endpoints are exempt, since a short line is
/// legitimate.
pub fn resize(e: &mut Element, h: Handle, to: Point, min_w: f32, min_h: f32) {
    match h {
        Handle::Start | Handle::End => {
            let mut pts = points_abs(e);
            if pts.len() < 2 {
                return;
            }
            let idx = if h == Handle::Start { 0 } else { pts.len() - 1 };
            pts[idx] = to;
            rebuild_polyline(e, &pts);
        }
        _ => {
            // The corner diagonally opposite the dragged one stays put.
            let (fx, fy) = match h {
                Handle::NW => (e.x + e.width, e.y + e.height),
                Handle::NE => (e.x, e.y + e.height),
                Handle::SW => (e.x + e.width, e.y),
                Handle::SE => (e.x, e.y),
                _ => unreachable!(),
            };
            let x0 = fx.min(to.x);
            let x1 = fx.max(to.x);
            let y0 = fy.min(to.y);
            let y1 = fy.max(to.y);
            e.x = x0;
            e.y = y0;
            e.width = (x1 - x0).max(min_w);
            e.height = (y1 - y0).max(min_h);
        }
    }
}

/// Re-derive the bounding box and relative vertices after an endpoint
/// moved, keeping the `points`-relative-to-`(x, y)` invariant.
fn rebuild_polyline(e: &mut Element, pts: &[Point]) {
    let x0 = pts.iter().fold(f32::MAX, |a, p| a.min(p.x));
    let y0 = pts.iter().fold(f32::MAX, |a, p| a.min(p.y));
    let x1 = pts.iter().fold(f32::MIN, |a, p| a.max(p.x));
    let y1 = pts.iter().fold(f32::MIN, |a, p| a.max(p.y));
    e.x = x0;
    e.y = y0;
    e.width = x1 - x0;
    e.height = y1 - y0;
    e.points = pts.iter().map(|p| [p.x - x0, p.y - y0]).collect();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Element;

    fn rect() -> Element {
        Element::new("r", Shape::Rectangle, 100.0, 100.0, 200.0, 100.0)
    }

    fn arrow() -> Element {
        Element::polyline("a", Shape::Arrow, &[(100.0, 100.0), (300.0, 200.0)])
    }

    #[test]
    fn rectangle_is_hit_inside_and_missed_outside() {
        let r = rect();
        assert!(hits(&r, Point { x: 150.0, y: 150.0 }, 0.0));
        assert!(hits(&r, Point { x: 100.0, y: 100.0 }, 0.0), "corner");
        assert!(!hits(&r, Point { x: 99.0, y: 150.0 }, 0.0));
        assert!(hits(&r, Point { x: 99.0, y: 150.0 }, 4.0), "tolerance");
    }

    #[test]
    fn ellipse_excludes_the_bounding_box_corners() {
        let mut e = rect();
        e.kind = "ellipse".into();
        assert!(hits(&e, Point { x: 200.0, y: 150.0 }, 0.0), "centre");
        assert!(
            !hits(&e, Point { x: 101.0, y: 101.0 }, 0.0),
            "corner is outside the ellipse"
        );
    }

    #[test]
    fn diamond_excludes_corners_but_includes_centre() {
        let mut d = rect();
        d.kind = "diamond".into();
        assert!(hits(&d, Point { x: 200.0, y: 150.0 }, 0.0));
        assert!(!hits(&d, Point { x: 105.0, y: 105.0 }, 0.0));
        // Mid-edge vertices are on the boundary.
        assert!(hits(&d, Point { x: 200.0, y: 100.0 }, 0.0));
    }

    #[test]
    fn polyline_needs_tolerance_near_the_segment() {
        let a = arrow();
        // Midpoint of (100,100)-(300,200).
        assert!(hits(&a, Point { x: 200.0, y: 150.0 }, 1.0));
        // Well off the line but inside its bounding box.
        assert!(!hits(&a, Point { x: 280.0, y: 110.0 }, 4.0));
    }

    #[test]
    fn hit_test_returns_the_topmost_element() {
        let mut doc = Document::default();
        let mut bottom = rect();
        bottom.id = "bottom".into();
        let mut top = rect();
        top.id = "top".into();
        doc.elements = vec![bottom, top];
        let i = hit_test(&doc, Point { x: 150.0, y: 150.0 }, 0.0).expect("hit");
        assert_eq!(doc.elements[i].id, "top");
    }

    #[test]
    fn deleted_elements_are_not_hit() {
        let mut doc = Document::default();
        let mut r = rect();
        r.is_deleted = true;
        doc.elements = vec![r];
        assert!(hit_test(&doc, Point { x: 150.0, y: 150.0 }, 0.0).is_none());
    }

    #[test]
    fn translate_moves_the_origin_only() {
        let mut a = arrow();
        let before = a.points.clone();
        translate(&mut a, 10.0, -5.0);
        assert_eq!((a.x, a.y), (110.0, 95.0));
        assert_eq!(a.points, before, "relative vertices must not change");
    }

    #[test]
    fn resizing_a_corner_pins_the_opposite_one() {
        let mut r = rect();
        resize(&mut r, Handle::SE, Point { x: 400.0, y: 300.0 }, 1.0, 1.0);
        assert_eq!((r.x, r.y), (100.0, 100.0), "NW must stay pinned");
        assert_eq!((r.width, r.height), (300.0, 200.0));

        let mut r2 = rect();
        resize(&mut r2, Handle::NW, Point { x: 50.0, y: 60.0 }, 1.0, 1.0);
        assert_eq!((r2.x, r2.y), (50.0, 60.0));
        // SE corner was (300, 200) and must not have moved.
        assert_eq!((r2.x + r2.width, r2.y + r2.height), (300.0, 200.0));
    }

    #[test]
    fn resizing_past_the_pinned_corner_flips_without_negative_size() {
        let mut r = rect();
        resize(&mut r, Handle::SE, Point { x: 0.0, y: 0.0 }, 1.0, 1.0);
        assert!(r.width > 0.0 && r.height > 0.0);
        assert_eq!((r.x, r.y), (0.0, 0.0));
    }

    #[test]
    fn resize_enforces_a_minimum_size() {
        let mut r = rect();
        resize(&mut r, Handle::SE, Point { x: 100.0, y: 100.0 }, 8.0, 17.0);
        assert_eq!((r.width, r.height), (8.0, 17.0));
    }

    #[test]
    fn dragging_an_endpoint_rebuilds_the_bounding_box() {
        let mut a = arrow();
        resize(&mut a, Handle::End, Point { x: 50.0, y: 50.0 }, 1.0, 1.0);
        let pts = points_abs(&a);
        assert_eq!(pts.first().copied(), Some(Point { x: 100.0, y: 100.0 }));
        assert_eq!(pts.last().copied(), Some(Point { x: 50.0, y: 50.0 }));
        // Bounding box now spans the two points.
        assert_eq!((a.x, a.y), (50.0, 50.0));
        assert_eq!((a.width, a.height), (50.0, 50.0));
    }

    #[test]
    fn handles_match_the_shape_family() {
        assert_eq!(handles(&rect()).len(), 4);
        let a = arrow();
        let hs = handles(&a);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].0, Handle::Start);
        assert_eq!(hs[1].1, Point { x: 300.0, y: 200.0 });
    }

    #[test]
    fn handle_at_finds_corners_within_tolerance() {
        let r = rect();
        assert_eq!(handle_at(&r, Point { x: 302.0, y: 202.0 }, 4.0), Some(Handle::SE));
        assert_eq!(handle_at(&r, Point { x: 200.0, y: 150.0 }, 4.0), None);
    }
}
