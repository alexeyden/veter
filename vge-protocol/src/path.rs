// VGE path decoder + arc-to-Bezier helper.
//
// `PathSegment` is the per-segment self-describing form
// (`point start, varu n_nodes, PathNode[n_nodes]`). Each `PathNode` is
// `u8 kind` followed by a kind-specific body; see the doc for the byte
// layout.

use std::f32::consts::{PI, TAU};

use super::codec::{DecodeError, DecodeResult, Point, Reader};

#[derive(Debug, Clone, PartialEq)]
pub enum PathNode {
    LineTo {
        dst: Point,
    },
    HorizontalLineTo {
        x: f32,
    },
    VerticalLineTo {
        y: f32,
    },
    CubicBezierTo {
        c0: Point,
        c1: Point,
        dst: Point,
    },
    QuadraticBezierTo {
        c: Point,
        dst: Point,
    },
    ArcCircleTo {
        large: bool,
        sweep: bool,
        radius: f32,
        dst: Point,
    },
    ArcEllipseTo {
        large: bool,
        sweep: bool,
        rx: f32,
        ry: f32,
        rotation: f32, // radians
        dst: Point,
    },
    ClosePath,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathSegment {
    pub start: Point,
    pub nodes: Vec<PathNode>,
}

pub const NODE_LINE_TO: u8 = 0;
pub const NODE_HLINE_TO: u8 = 1;
pub const NODE_VLINE_TO: u8 = 2;
pub const NODE_CUBIC_TO: u8 = 3;
pub const NODE_ARC_CIRCLE: u8 = 4;
pub const NODE_ARC_ELLIPSE: u8 = 5;
pub const NODE_CLOSE: u8 = 6;
pub const NODE_QUAD_TO: u8 = 7;

fn read_node(r: &mut Reader<'_>) -> DecodeResult<PathNode> {
    let kind = r.u8()?;
    match kind {
        NODE_LINE_TO => Ok(PathNode::LineTo { dst: r.point()? }),
        NODE_HLINE_TO => Ok(PathNode::HorizontalLineTo { x: r.f32()? }),
        NODE_VLINE_TO => Ok(PathNode::VerticalLineTo { y: r.f32()? }),
        NODE_CUBIC_TO => Ok(PathNode::CubicBezierTo {
            c0: r.point()?,
            c1: r.point()?,
            dst: r.point()?,
        }),
        NODE_QUAD_TO => Ok(PathNode::QuadraticBezierTo {
            c: r.point()?,
            dst: r.point()?,
        }),
        NODE_ARC_CIRCLE => {
            let flags = r.u8()?;
            let radius = r.f32()?;
            if !radius.is_finite() {
                return Err(DecodeError::bad_payload());
            }
            Ok(PathNode::ArcCircleTo {
                large: flags & 0x01 != 0,
                sweep: flags & 0x02 != 0,
                radius,
                dst: r.point()?,
            })
        }
        NODE_ARC_ELLIPSE => {
            let flags = r.u8()?;
            let rx = r.f32()?;
            let ry = r.f32()?;
            let rotation = r.f32()?;
            if !(rx.is_finite() && ry.is_finite() && rotation.is_finite()) {
                return Err(DecodeError::bad_payload());
            }
            Ok(PathNode::ArcEllipseTo {
                large: flags & 0x01 != 0,
                sweep: flags & 0x02 != 0,
                rx,
                ry,
                rotation,
                dst: r.point()?,
            })
        }
        NODE_CLOSE => Ok(PathNode::ClosePath),
        _ => Err(DecodeError::bad_payload()),
    }
}

/// Read one self-describing PathSegment: `point start, varu n_nodes,
/// PathNode[n_nodes]`.
fn read_segment(r: &mut Reader<'_>) -> DecodeResult<PathSegment> {
    let start = r.point()?;
    let n_nodes = r.varu()? as usize;
    let mut nodes = Vec::with_capacity(n_nodes);
    for _ in 0..n_nodes {
        nodes.push(read_node(r)?);
    }
    Ok(PathSegment { start, nodes })
}

/// Read `varu n_segments` followed by that many self-describing segments.
pub fn read_path_segments(r: &mut Reader<'_>) -> DecodeResult<Vec<PathSegment>> {
    let n = r.varu()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_segment(r)?);
    }
    Ok(v)
}

// --- SVG endpoint arc → cubic Bézier conversion ---
//
// Femtovg's Path::arc takes a center+angle parameterization but only
// supports circular arcs. For elliptic arcs we expand to ≤90°
// cubic-Bézier segments and feed them to Path::bezier_to.

/// Yield cubic Bézier control points (c1, c2, end) approximating the
/// elliptic arc that connects `p0` to `p1`. Coordinates are kept in the
/// caller's units (the conversion is unit-agnostic).
///
/// Edge cases per SVG spec Appendix F.6.5:
/// - Equal endpoints → empty (no arc to draw).
/// - rx == 0 || ry == 0 → single LineTo (returned as one Bézier whose
///   controls coincide with endpoints).
/// - Out-of-range radii → uniformly scaled up to just reach `p1`.
pub fn arc_to_beziers(
    p0: Point,
    p1: Point,
    mut rx: f32,
    mut ry: f32,
    rotation: f32,
    large: bool,
    sweep: bool,
) -> Vec<(Point, Point, Point)> {
    if (p0.x - p1.x).abs() < f32::EPSILON && (p0.y - p1.y).abs() < f32::EPSILON {
        return Vec::new();
    }
    rx = rx.abs();
    ry = ry.abs();
    if rx == 0.0 || ry == 0.0 {
        // Degenerate: SVG says treat as straight line.
        return vec![(p0, p1, p1)];
    }

    let cos_phi = rotation.cos();
    let sin_phi = rotation.sin();

    // Step 1: compute (x1', y1') in the ellipse's local frame.
    let dx = (p0.x - p1.x) * 0.5;
    let dy = (p0.y - p1.y) * 0.5;
    let x1p = cos_phi * dx + sin_phi * dy;
    let y1p = -sin_phi * dx + cos_phi * dy;

    // Step 2: ensure radii are large enough.
    let lambda = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    // Step 3: compute (cx', cy').
    let rx2 = rx * rx;
    let ry2 = ry * ry;
    let x1p2 = x1p * x1p;
    let y1p2 = y1p * y1p;
    let denom = rx2 * y1p2 + ry2 * x1p2;
    let mut factor_sq = (rx2 * ry2 - rx2 * y1p2 - ry2 * x1p2) / denom;
    if factor_sq < 0.0 {
        factor_sq = 0.0;
    }
    let mut factor = factor_sq.sqrt();
    if large == sweep {
        factor = -factor;
    }
    let cxp = factor * (rx * y1p) / ry;
    let cyp = factor * -(ry * x1p) / rx;

    // Step 4: compute (cx, cy) in the original frame.
    let mx = (p0.x + p1.x) * 0.5;
    let my = (p0.y + p1.y) * 0.5;
    let cx = cos_phi * cxp - sin_phi * cyp + mx;
    let cy = sin_phi * cxp + cos_phi * cyp + my;

    // Step 5: compute theta1 and delta_theta.
    let v1x = (x1p - cxp) / rx;
    let v1y = (y1p - cyp) / ry;
    let v2x = (-x1p - cxp) / rx;
    let v2y = (-y1p - cyp) / ry;

    let theta1 = signed_angle(1.0, 0.0, v1x, v1y);
    let mut delta = signed_angle(v1x, v1y, v2x, v2y);
    if !sweep && delta > 0.0 {
        delta -= TAU;
    } else if sweep && delta < 0.0 {
        delta += TAU;
    }

    // Step 6: subdivide into ≤90° arcs and emit one Bézier each.
    let n_segments = ((delta.abs() / (PI * 0.5)).ceil() as usize).max(1);
    let segment_delta = delta / n_segments as f32;
    let alpha = (segment_delta * 0.5).sin() * 4.0 / 3.0 / (1.0 + (segment_delta * 0.5).cos());

    let mut out = Vec::with_capacity(n_segments);
    let mut t = theta1;
    let (mut sx, mut sy) = ellipse_point(cx, cy, rx, ry, cos_phi, sin_phi, t);
    for _ in 0..n_segments {
        let t_next = t + segment_delta;
        let (ex, ey) = ellipse_point(cx, cy, rx, ry, cos_phi, sin_phi, t_next);
        let (sdx, sdy) = ellipse_derivative(rx, ry, cos_phi, sin_phi, t);
        let (edx, edy) = ellipse_derivative(rx, ry, cos_phi, sin_phi, t_next);
        let c1 = Point {
            x: sx + alpha * sdx,
            y: sy + alpha * sdy,
        };
        let c2 = Point {
            x: ex - alpha * edx,
            y: ey - alpha * edy,
        };
        out.push((c1, c2, Point { x: ex, y: ey }));
        sx = ex;
        sy = ey;
        t = t_next;
    }
    out
}

fn signed_angle(ux: f32, uy: f32, vx: f32, vy: f32) -> f32 {
    let dot = ux * vx + uy * vy;
    let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
    let c = (dot / len).clamp(-1.0, 1.0);
    let a = c.acos();
    if ux * vy - uy * vx < 0.0 { -a } else { a }
}

fn ellipse_point(
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    cos_phi: f32,
    sin_phi: f32,
    t: f32,
) -> (f32, f32) {
    let ct = t.cos();
    let st = t.sin();
    (
        cx + cos_phi * rx * ct - sin_phi * ry * st,
        cy + sin_phi * rx * ct + cos_phi * ry * st,
    )
}

fn ellipse_derivative(
    rx: f32,
    ry: f32,
    cos_phi: f32,
    sin_phi: f32,
    t: f32,
) -> (f32, f32) {
    let ct = t.cos();
    let st = t.sin();
    (
        -cos_phi * rx * st - sin_phi * ry * ct,
        -sin_phi * rx * st + cos_phi * ry * ct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Writer;

    fn write_node(w: &mut Writer, kind: u8) {
        w.u8(kind);
    }

    #[test]
    fn round_trip_all_node_kinds() {
        let mut w = Writer::new();
        // 1 segment
        w.varu(1);
        // start = (0, 0)
        w.f32(0.0);
        w.f32(0.0);
        // 8 nodes
        w.varu(8);

        // LineTo
        write_node(&mut w, NODE_LINE_TO);
        w.f32(1.0);
        w.f32(2.0);
        // HLine
        write_node(&mut w, NODE_HLINE_TO);
        w.f32(3.0);
        // VLine
        write_node(&mut w, NODE_VLINE_TO);
        w.f32(4.0);
        // Cubic
        write_node(&mut w, NODE_CUBIC_TO);
        for f in [5.0f32, 6.0, 7.0, 8.0, 9.0, 10.0] {
            w.f32(f);
        }
        // Quad
        write_node(&mut w, NODE_QUAD_TO);
        for f in [11.0f32, 12.0, 13.0, 14.0] {
            w.f32(f);
        }
        // Arc circle (large=1, sweep=0)
        write_node(&mut w, NODE_ARC_CIRCLE);
        w.u8(0x01);
        w.f32(2.5);
        w.f32(15.0);
        w.f32(16.0);
        // Arc ellipse (large=0, sweep=1)
        write_node(&mut w, NODE_ARC_ELLIPSE);
        w.u8(0x02);
        w.f32(2.0);
        w.f32(3.0);
        w.f32(0.5);
        w.f32(17.0);
        w.f32(18.0);
        // Close
        write_node(&mut w, NODE_CLOSE);

        let mut r = Reader::new(&w.buf);
        let segs = read_path_segments(&mut r).unwrap();
        assert!(r.at_end());
        assert_eq!(segs.len(), 1);
        let s = &segs[0];
        assert_eq!(s.start, Point { x: 0.0, y: 0.0 });
        assert_eq!(s.nodes.len(), 8);
        assert_eq!(s.nodes[0], PathNode::LineTo { dst: Point { x: 1.0, y: 2.0 } });
        assert_eq!(s.nodes[1], PathNode::HorizontalLineTo { x: 3.0 });
        assert_eq!(s.nodes[2], PathNode::VerticalLineTo { y: 4.0 });
        assert!(matches!(s.nodes[3], PathNode::CubicBezierTo { .. }));
        assert!(matches!(s.nodes[4], PathNode::QuadraticBezierTo { .. }));
        match s.nodes[5] {
            PathNode::ArcCircleTo { large, sweep, radius, dst } => {
                assert!(large);
                assert!(!sweep);
                assert_eq!(radius, 2.5);
                assert_eq!(dst, Point { x: 15.0, y: 16.0 });
            }
            _ => panic!("wrong node"),
        }
        match s.nodes[6] {
            PathNode::ArcEllipseTo { large, sweep, rx, ry, rotation, dst } => {
                assert!(!large);
                assert!(sweep);
                assert_eq!((rx, ry), (2.0, 3.0));
                assert_eq!(rotation, 0.5);
                assert_eq!(dst, Point { x: 17.0, y: 18.0 });
            }
            _ => panic!("wrong node"),
        }
        assert_eq!(s.nodes[7], PathNode::ClosePath);
    }

    #[test]
    fn high_bit_in_kind_rejected() {
        // The old TinyVG-derived "line_width override" lived in the high
        // bit of the tag byte. We dropped it; anything ≥ 0x80 (or any
        // other unknown kind) must now be rejected outright.
        let mut w = Writer::new();
        w.varu(1);
        w.f32(0.0);
        w.f32(0.0);
        w.varu(1);
        w.u8(0x80 | NODE_LINE_TO); // formerly "line_width-decorated LineTo"
        w.f32(0.0);
        w.f32(0.0);
        let mut r = Reader::new(&w.buf);
        assert!(read_path_segments(&mut r).is_err());
    }

    #[test]
    fn unknown_kind_rejected() {
        let mut w = Writer::new();
        w.varu(1);
        w.f32(0.0);
        w.f32(0.0);
        w.varu(1);
        w.u8(0x42); // unknown kind
        let mut r = Reader::new(&w.buf);
        assert!(read_path_segments(&mut r).is_err());
    }

    #[test]
    fn empty_arc_returns_empty() {
        let p = Point { x: 1.0, y: 2.0 };
        let beziers = arc_to_beziers(p, p, 1.0, 1.0, 0.0, false, false);
        assert!(beziers.is_empty());
    }

    #[test]
    fn degenerate_radii_returns_lineto() {
        let beziers = arc_to_beziers(
            Point { x: 0.0, y: 0.0 },
            Point { x: 1.0, y: 0.0 },
            0.0,
            1.0,
            0.0,
            false,
            false,
        );
        assert_eq!(beziers.len(), 1);
        let (_, _, end) = beziers[0];
        assert_eq!(end, Point { x: 1.0, y: 0.0 });
    }

    #[test]
    fn semicircle_endpoints() {
        // Semicircle from (1,0) to (-1,0) with r=1: the arc passes
        // through (0,1) (sweep=1, large=0) or (0,-1) (sweep=0, large=0).
        let beziers = arc_to_beziers(
            Point { x: 1.0, y: 0.0 },
            Point { x: -1.0, y: 0.0 },
            1.0,
            1.0,
            0.0,
            false,
            true,
        );
        assert!(!beziers.is_empty());
        let (_, _, end) = beziers[beziers.len() - 1];
        assert!((end.x - -1.0).abs() < 1e-4);
        assert!(end.y.abs() < 1e-4);
    }
}
