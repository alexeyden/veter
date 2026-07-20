//! Document model -> VGE draw commands.
//!
//! One VGE element per document element, id-for-id, all parented to the
//! `canvas` element that carries the camera transform. Geometry is
//! emitted *centred on (0, 0)* with the element's `origin` holding its
//! position, so that later phases can move a shape with a single
//! `UpdateOrigin` and rotate it with a pure rotation matrix (§9.13).

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style,
};
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_protocol::path::{PathNode, PathSegment};

use crate::camera::Camera;
use crate::doc::{Document, Element, Shape};

pub const CANVAS_ID: &str = "canvas";
pub const CANVAS_ORDER: i32 = 0;
/// Live drag preview. Parented to the canvas so it pans and zooms with
/// the document, and drawn above every committed shape.
pub const PREVIEW_ID: &str = "canvas.preview";
pub const PREVIEW_ORDER: i32 = 900;
/// Selection outline and handles. Parented to the canvas so it tracks
/// the shape it decorates, and drawn above the preview.
pub const SELECTION_ID: &str = "canvas.selection";
pub const SELECTION_ORDER: i32 = 950;
/// Chrome sits above the canvas and is *not* parented to it, so the
/// camera transform never touches it.
pub const CHROME_ORDER: i32 = 1000;

/// The canvas element: no geometry of its own, just a transform anchor
/// and parent for every shape.
pub fn canvas_element(cam: &Camera) -> Command {
    Command::CreateElement(CreateElementBody {
        id: CANVAS_ID.into(),
        commands: Vec::new(),
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: CANVAS_ORDER,
        parent: None,
        size: None,
        transform: Some(cam.transform()),
    })
}

/// The preview element, created empty and hidden. During a drag it is
/// re-pointed with `UpdateOrigin` + `UpdateCommands` rather than being
/// destroyed and recreated each frame.
pub fn preview_element() -> Command {
    Command::CreateElement(CreateElementBody {
        id: PREVIEW_ID.into(),
        commands: Vec::new(),
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: false,
        draw_order: PREVIEW_ORDER,
        parent: Some(CANVAS_ID.into()),
        size: None,
        transform: None,
    })
}

/// The selection overlay element, created empty and hidden. Unlike the
/// shape elements its geometry is in *absolute* canvas cells with a
/// fixed origin, so following a selection is one `UpdateCommands`.
pub fn selection_element() -> Command {
    Command::CreateElement(CreateElementBody {
        id: SELECTION_ID.into(),
        commands: Vec::new(),
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: false,
        draw_order: SELECTION_ORDER,
        parent: Some(CANVAS_ID.into()),
        size: None,
        transform: None,
    })
}

/// Dashed bounds plus a square at each resize handle.
pub fn selection_commands(e: &Element, cam: &Camera) -> Vec<DrawCmd> {
    let accent = ACCENT;
    let pad_x = 3.0 / cam.cell_w;
    let pad_y = 3.0 / cam.cell_h;
    let tl = cam.doc_to_canvas(e.x, e.y);
    let br = cam.doc_to_canvas(e.x + e.width, e.y + e.height);
    let (x0, y0) = (tl.x - pad_x, tl.y - pad_y);
    let (x1, y1) = (br.x + pad_x, br.y + pad_y);

    let mut cmds = vec![DrawCmd::DrawLineLoop {
        stroke: accent.clone(),
        line_width: 0.06,
        points: vec![
            Point { x: x0, y: y0 },
            Point { x: x1, y: y0 },
            Point { x: x1, y: y1 },
            Point { x: x0, y: y1 },
        ],
    }];

    // Handle squares, sized in pixels so they stay square on the grid.
    let hx = 3.0 / cam.cell_w;
    let hy = 3.0 / cam.cell_h;
    let rects: Vec<Rect> = crate::hit::handles(e)
        .into_iter()
        .map(|(_, p)| {
            let c = cam.doc_to_canvas(p.x, p.y);
            Rect {
                x: c.x - hx,
                y: c.y - hy,
                w: hx * 2.0,
                h: hy * 2.0,
            }
        })
        .collect();
    if !rects.is_empty() {
        cmds.push(DrawCmd::FillRectangles {
            fill: accent,
            rects,
        });
    }
    cmds
}

pub fn document_elements(doc: &Document, cam: &Camera) -> Vec<(Command, u32)> {
    doc.elements
        .iter()
        .filter(|e| !e.is_deleted)
        // Text bound to a container is drawn by that container's label
        // pass, not as a free-standing element.
        .filter(|e| e.container_id.is_none())
        .enumerate()
        .filter_map(|(i, e)| element_body(e, i as i32 + 1, cam))
        .map(|b| (Command::CreateElement(b), REQ_ID_NO_RESPONSE))
        .collect()
}

/// An element's VGE origin in canvas cells.
///
/// Shapes anchor at their centre, so a rotation is a pure rotation
/// matrix and a move is one `UpdateOrigin`. Standalone text anchors at
/// its *top-left* instead: its box grows as characters are typed, and a
/// centre anchor would slide the string sideways on every keystroke.
pub fn element_origin(e: &Element, cam: &Camera) -> Point {
    match e.shape() {
        Some(Shape::Text) => cam.doc_to_canvas(e.x, e.y),
        _ => cam.doc_to_canvas(e.x + e.width / 2.0, e.y + e.height / 2.0),
    }
}

/// A text caret, one cell tall, sitting after the last character.
///
/// The primary font is the terminal's own, so an ASCII character
/// advances exactly one cell — character count is the cursor column.
pub fn caret_command(e: &Element, accent: Style) -> DrawCmd {
    let n = e.text.chars().count() as f32;
    let x = match e.shape() {
        // Container labels are centre-aligned about the origin.
        Some(Shape::Text) => n,
        _ => n / 2.0,
    };
    DrawCmd::FillRectangles {
        fill: accent,
        rects: vec![Rect {
            x,
            y: 0.0,
            w: 0.12,
            h: 1.0,
        }],
    }
}

pub fn element_body(e: &Element, order: i32, cam: &Camera) -> Option<CreateElementBody> {
    let shape = e.shape()?;
    let fill = paint(&e.background_color, e.opacity);
    let stroke = paint(&e.stroke_color, e.opacity);
    let lw = line_width(e.stroke_width, cam);

    let (hw, hh) = (e.width / 2.0, e.height / 2.0);
    // Half-extents in canvas cells.
    let hx = hw / cam.cell_w;
    let hy = hh / cam.cell_h;

    // VGE's OutlineFill* ops draw a continuous stroke and have no dash
    // concept, so a dashed closed shape is drawn in two passes instead:
    // a plain fill, then the outline flattened to a polyline and
    // chopped into segments.
    if is_dashed(&e.stroke_style) && shape.is_closed() {
        let mut cmds = Vec::new();
        if let Some(f) = fill.clone() {
            cmds.push(closed_fill(shape, hx, hy, e.corner_radius(), cam, f));
        }
        if let Some(st) = stroke.clone() {
            let loop_pts = perimeter(shape, hx, hy, e.corner_radius(), cam);
            cmds.push(stroked(loop_pts, st, lw, &e.stroke_style, cam));
        }
        if cmds.is_empty() {
            return None;
        }
        return Some(finish(e, cmds, order, cam));
    }

    let cmds: Vec<DrawCmd> = match shape {
        Shape::Rectangle => {
            let r = e.corner_radius();
            if r > 0.0 {
                vec![outline_fill_path(
                    rounded_rect(hw, hh, r, cam),
                    fill.clone(),
                    stroke.clone(),
                    lw,
                )?]
            } else {
                vec![DrawCmd::OutlineFillRectangles {
                    fill: fill.clone().unwrap_or(TRANSPARENT),
                    stroke: stroke.clone().or_else(|| fill.clone())?,
                    line_width: lw,
                    rects: vec![Rect {
                        x: -hx,
                        y: -hy,
                        w: hx * 2.0,
                        h: hy * 2.0,
                    }],
                }]
            }
        }
        Shape::Diamond => vec![DrawCmd::OutlineFillPolygon {
            fill: fill.clone().unwrap_or(TRANSPARENT),
            stroke: stroke.clone().or_else(|| fill.clone())?,
            line_width: lw,
            points: vec![
                Point { x: 0.0, y: -hy },
                Point { x: hx, y: 0.0 },
                Point { x: 0.0, y: hy },
                Point { x: -hx, y: 0.0 },
            ],
        }],
        // Two half-arcs. rx/ry are converted per-axis, so this is a true
        // visual ellipse without the §7.2 compensation dance.
        Shape::Ellipse => vec![outline_fill_path(
            vec![PathSegment {
                start: Point { x: -hx, y: 0.0 },
                nodes: vec![
                    arc(hx, hy, Point { x: hx, y: 0.0 }),
                    arc(hx, hy, Point { x: -hx, y: 0.0 }),
                    PathNode::ClosePath,
                ],
            }],
            fill.clone(),
            stroke.clone(),
            lw,
        )?],
        Shape::Line | Shape::Arrow => {
            let pts = polyline(e, hw, hh, cam);
            if pts.len() < 2 {
                return None;
            }
            let st = stroke.clone()?;
            let mut out = vec![stroked(pts.clone(), st.clone(), lw, &e.stroke_style, cam)];
            if shape == Shape::Arrow {
                if let Some(head) = arrowhead(&pts, e.stroke_width, cam) {
                    out.push(DrawCmd::FillPolygon {
                        fill: st,
                        points: head,
                    });
                }
            }
            out
        }
        Shape::Text => vec![DrawCmd::DrawText {
            origin: Point { x: 0.0, y: 0.0 },
            align: align_of(&e.text_align),
            fill: stroke.clone()?,
            font_style: FontStyle::default(),
            text: e.text.clone(),
        }],
    };

    Some(finish(e, cmds, order, cam))
}

/// Append the container label and wrap the commands into an element.
/// Shared by the normal and the dashed-outline paths so a dashed box
/// keeps its caption.
fn finish(e: &Element, mut cmds: Vec<DrawCmd>, order: i32, cam: &Camera) -> CreateElementBody {
    // Drawn inside the same element so the shape and its text move as
    // one. Text is cell-sized, so it is centred rather than laid out.
    if e.shape() != Some(Shape::Text) && !e.text.is_empty() {
        if let Some(st) = paint(&e.stroke_color, e.opacity) {
            cmds.push(DrawCmd::DrawText {
                origin: Point { x: 0.0, y: 0.0 },
                align: Align::Center,
                fill: st,
                font_style: FontStyle::default(),
                text: e.text.clone(),
            });
        }
    }
    CreateElementBody {
        id: e.id.clone(),
        commands: cmds,
        origin: element_origin(e, cam),
        is_visible: true,
        draw_order: order,
        parent: Some(CANVAS_ID.into()),
        size: None,
        transform: None,
    }
}

fn is_dashed(style: &str) -> bool {
    matches!(style, "dashed" | "dotted")
}

/// Fill only, no stroke — the dashed path draws the outline separately.
fn closed_fill(
    shape: Shape,
    hx: f32,
    hy: f32,
    radius_px: f32,
    cam: &Camera,
    fill: Style,
) -> DrawCmd {
    match shape {
        Shape::Rectangle if radius_px > 0.0 => DrawCmd::FillPath {
            fill,
            segments: rounded_rect(hx * cam.cell_w, hy * cam.cell_h, radius_px, cam),
        },
        Shape::Rectangle => DrawCmd::FillRectangles {
            fill,
            rects: vec![Rect {
                x: -hx,
                y: -hy,
                w: hx * 2.0,
                h: hy * 2.0,
            }],
        },
        // Ellipse and diamond both reduce to their flattened outline.
        _ => DrawCmd::FillPolygon {
            fill,
            points: perimeter(shape, hx, hy, radius_px, cam),
        },
    }
}

/// Segments per quarter-turn when flattening a curve for dashing.
const ARC_STEPS: usize = 8;

/// A closed shape's outline as a polyline, in element-local canvas
/// cells. Curves are flattened because dashing needs arc-length walking
/// along straight segments. The first point is repeated at the end so
/// the loop closes.
fn perimeter(shape: Shape, hx: f32, hy: f32, radius_px: f32, cam: &Camera) -> Vec<Point> {
    let mut pts = match shape {
        Shape::Ellipse => (0..ARC_STEPS * 4)
            .map(|i| {
                let t = i as f32 / (ARC_STEPS * 4) as f32 * std::f32::consts::TAU;
                Point {
                    x: hx * t.cos(),
                    y: hy * t.sin(),
                }
            })
            .collect(),
        Shape::Diamond => vec![
            Point { x: 0.0, y: -hy },
            Point { x: hx, y: 0.0 },
            Point { x: 0.0, y: hy },
            Point { x: -hx, y: 0.0 },
        ],
        _ => {
            let rx = (radius_px / cam.cell_w).min(hx);
            let ry = (radius_px / cam.cell_h).min(hy);
            if rx <= 0.0 || ry <= 0.0 {
                vec![
                    Point { x: -hx, y: -hy },
                    Point { x: hx, y: -hy },
                    Point { x: hx, y: hy },
                    Point { x: -hx, y: hy },
                ]
            } else {
                let mut v = Vec::with_capacity(ARC_STEPS * 4 + 4);
                // Corner arc centres, walked clockwise from top-left.
                let corners = [
                    (-hx + rx, -hy + ry, std::f32::consts::PI),
                    (hx - rx, -hy + ry, std::f32::consts::FRAC_PI_2 * 3.0),
                    (hx - rx, hy - ry, 0.0),
                    (-hx + rx, hy - ry, std::f32::consts::FRAC_PI_2),
                ];
                for (cx, cy, start) in corners {
                    for i in 0..=ARC_STEPS {
                        let t = start
                            + std::f32::consts::FRAC_PI_2 * (i as f32 / ARC_STEPS as f32);
                        v.push(Point {
                            x: cx + rx * t.cos(),
                            y: cy + ry * t.sin(),
                        });
                    }
                }
                v
            }
        }
    };
    // Close the loop so `dash` walks the final edge too.
    if let Some(first) = pts.first().copied() {
        pts.push(first);
    }
    pts
}

/// Shared UI accent for the selection outline, handles and text caret.
pub const ACCENT: Style = Style::Flat(Color {
    r: 0.36,
    g: 0.55,
    b: 0.92,
    a: 1.0,
});

const TRANSPARENT: Style = Style::Flat(Color {
    r: 0.0,
    g: 0.0,
    b: 0.0,
    a: 0.0,
});

fn align_of(s: &str) -> Align {
    match s {
        "center" => Align::Center,
        "right" => Align::Right,
        _ => Align::Left,
    }
}

/// Stroke width is a single scalar in an anisotropic space; cell width
/// is the conventional reading. Not scaled by zoom — the camera
/// transform already scales rendered geometry.
fn line_width(px: f32, cam: &Camera) -> f32 {
    (px / cam.cell_w).max(0.05)
}

fn outline_fill_path(
    segments: Vec<PathSegment>,
    fill: Option<Style>,
    stroke: Option<Style>,
    lw: f32,
) -> Option<DrawCmd> {
    Some(DrawCmd::OutlineFillPath {
        stroke: stroke.clone().or_else(|| fill.clone())?,
        fill: fill.unwrap_or(TRANSPARENT),
        line_width: lw,
        segments,
    })
}

fn arc(rx: f32, ry: f32, dst: Point) -> PathNode {
    PathNode::ArcEllipseTo {
        large: false,
        sweep: true,
        rx,
        ry,
        rotation: 0.0,
        dst,
    }
}

/// Rounded rectangle from its corners, with per-axis radii already in
/// cell units. Shared with the chrome panels.
///
/// The path deliberately starts at the *midpoint of the top edge*
/// rather than at a corner. A path's open/close seam is where
/// anti-aliasing coverage is least well defined; putting it between two
/// collinear straight segments hides it, whereas starting on a corner
/// arc leaves a visible notch at that corner.
///
/// The last straight run back to the seam is left to `ClosePath` rather
/// than emitted explicitly. Emitting it *and* closing appends a
/// zero-length closing segment, a degenerate edge the rasteriser has to
/// make something of.
pub fn rounded_rect_path(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rx: f32,
    ry: f32,
) -> Vec<PathSegment> {
    let rx = rx.min((x1 - x0) / 2.0).max(0.0);
    let ry = ry.min((y1 - y0) / 2.0).max(0.0);
    // Seam point, kept clear of both top corners.
    let seam = ((x0 + x1) / 2.0).clamp(x0 + rx, x1 - rx);
    vec![PathSegment {
        start: Point { x: seam, y: y0 },
        nodes: vec![
            PathNode::HorizontalLineTo { x: x1 - rx },
            arc(rx, ry, Point { x: x1, y: y0 + ry }),
            PathNode::VerticalLineTo { y: y1 - ry },
            arc(rx, ry, Point { x: x1 - rx, y: y1 }),
            PathNode::HorizontalLineTo { x: x0 + rx },
            arc(rx, ry, Point { x: x0, y: y1 - ry }),
            PathNode::VerticalLineTo { y: y0 + ry },
            arc(rx, ry, Point { x: x0 + rx, y: y0 }),
            // ClosePath draws the run from here back to `seam`.
            PathNode::ClosePath,
        ],
    }]
}

fn rounded_rect(hw: f32, hh: f32, r_px: f32, cam: &Camera) -> Vec<PathSegment> {
    let r = r_px.min(hw).min(hh);
    rounded_rect_path(
        -hw / cam.cell_w,
        -hh / cam.cell_h,
        hw / cam.cell_w,
        hh / cam.cell_h,
        r / cam.cell_w,
        r / cam.cell_h,
    )
}

fn polyline(e: &Element, hw: f32, hh: f32, cam: &Camera) -> Vec<Point> {
    e.points
        .iter()
        .map(|[px, py]| Point {
            x: (px - hw) / cam.cell_w,
            y: (py - hh) / cam.cell_h,
        })
        .collect()
}

/// VGE has no dash pattern, so dashed/dotted strokes are chopped into
/// explicit segments here.
/// Dash geometry scaled to the stroke width, the way SVG's
/// `stroke-dasharray` is conventionally authored — a 4px line needs
/// much longer dashes than a 1px one to read as dashed. The floors keep
/// thin strokes from collapsing into something indistinguishable from
/// solid.
fn dash_pattern(style: &str, lw_px: f32) -> (f32, f32) {
    if style == "dotted" {
        // A "dot" under butt caps is a square: length == width.
        (lw_px.max(1.5), (lw_px * 2.5).max(3.5))
    } else {
        ((lw_px * 4.0).max(7.0), (lw_px * 3.0).max(5.0))
    }
}

fn stroked(pts: Vec<Point>, stroke: Style, lw: f32, style: &str, cam: &Camera) -> DrawCmd {
    if is_dashed(style) {
        let (on, off) = dash_pattern(style, lw * cam.cell_w);
        return DrawCmd::DrawLines {
            stroke,
            line_width: lw,
            lines: dash(&pts, on, off, cam),
        };
    }
    DrawCmd::DrawLineStrip {
        stroke,
        line_width: lw,
        points: pts,
    }
}

/// Chop a polyline into on/off runs. `on_px` and `off_px` are in
/// *pixels*: cells are anisotropic, so walking arc length in cell units
/// would make a vertical dash physically ~2x longer than a horizontal
/// one — obvious on a rectangle perimeter, where both meet.
fn dash(pts: &[Point], on: f32, off: f32, cam: &Camera) -> Vec<(Point, Point)> {
    let mut out = Vec::new();
    if on <= 0.0 || off <= 0.0 {
        return out;
    }
    let (mut pen, mut drawing) = (0.0f32, true);
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        // Arc length in pixels, so the pattern is isotropic on screen.
        let len = (((b.x - a.x) * cam.cell_w).powi(2)
            + ((b.y - a.y) * cam.cell_h).powi(2))
        .sqrt();
        if len <= f32::EPSILON {
            continue;
        }
        let mut t = 0.0;
        while t < len {
            let span = if drawing { on } else { off };
            let step = (span - pen).min(len - t);
            if drawing {
                out.push((lerp(a, b, t / len), lerp(a, b, (t + step) / len)));
            }
            t += step;
            pen += step;
            if pen >= span - f32::EPSILON {
                drawing = !drawing;
                pen = 0.0;
            }
        }
    }
    out
}

fn lerp(a: Point, b: Point, t: f32) -> Point {
    Point {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
    }
}

/// Arrowheads have no VGE primitive — a filled triangle built from the
/// last segment's direction.
///
/// The whole construction runs in *pixel* space. Cells are anisotropic,
/// so a direction unit vector derived in cell units doesn't point where
/// the line visually points, and its perpendicular `(-uy, ux)` isn't
/// perpendicular on screen. Doing it in cell space skews the head on
/// every arrow that isn't exactly horizontal or vertical.
fn arrowhead(pts: &[Point], stroke_px: f32, cam: &Camera) -> Option<Vec<Point>> {
    let tip = *pts.last()?;
    let prev = *pts.get(pts.len().checked_sub(2)?)?;
    let to_px = |p: Point| (p.x * cam.cell_w, p.y * cam.cell_h);
    let to_cell = |x: f32, y: f32| Point {
        x: x / cam.cell_w,
        y: y / cam.cell_h,
    };

    let (tx, ty) = to_px(tip);
    let (px, py) = to_px(prev);
    let (dx, dy) = (tx - px, ty - py);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-6 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let back = (10.0 + stroke_px * 2.0).min(len);
    let half = back * 0.45;
    let (bx, by) = (tx - ux * back, ty - uy * back);
    Some(vec![
        tip,
        to_cell(bx - uy * half, by + ux * half),
        to_cell(bx + uy * half, by - ux * half),
    ])
}

/// `"transparent"` -> `None` so callers can skip the stroke or fill
/// entirely. Hachure and cross-hatch collapse to solid: VGE has no
/// pattern fill, and faking one costs real geometry per shape.
fn paint(css: &str, opacity: f32) -> Option<Style> {
    if css.eq_ignore_ascii_case("transparent") {
        return None;
    }
    let hex = css.strip_prefix('#')?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    let (r, g, b) = match hex.len() {
        6 => ((v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF),
        3 => {
            let (r, g, b) = ((v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
            (r * 17, g * 17, b * 17)
        }
        _ => return None,
    };
    Some(Style::Flat(Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: (opacity / 100.0).clamp(0.0, 1.0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors() {
        let Some(Style::Flat(c)) = paint("#ff8000", 100.0) else {
            panic!("expected flat");
        };
        assert!((c.r - 1.0).abs() < 1e-6);
        assert!((c.g - 0.5019608).abs() < 1e-4);
        assert!((c.b - 0.0).abs() < 1e-6);
        assert!(paint("transparent", 100.0).is_none());
        assert!(paint("#abc", 100.0).is_some());
    }

    #[test]
    fn dash_alternates_and_stays_within_the_line() {
        let pts = [Point { x: 0.0, y: 0.0 }, Point { x: 10.0, y: 0.0 }];
        let cam = Camera::new(8.0, 17.0);
        // 16px on / 8px off across a 10-cell (80px) horizontal run.
        let segs = dash(&pts, 16.0, 8.0, &cam);
        assert!(!segs.is_empty());
        for (a, b) in &segs {
            assert!(a.x >= -1e-6 && b.x <= 10.0 + 1e-6);
            assert!(b.x >= a.x);
        }
    }

    /// The head is built in pixel space, so its two wings must be
    /// symmetric about the shaft *on screen* at any angle — not just on
    /// the axis-aligned cases where cell anisotropy cancels out.
    #[test]
    fn arrowhead_is_symmetric_at_any_angle() {
        let cam = Camera::new(8.0, 17.0);
        for (ex, ey) in [
            (20.0, 0.0),
            (0.0, 20.0),
            (20.0, 20.0),
            (-14.0, 9.0),
            (7.0, -19.0),
        ] {
            let pts = [Point { x: 0.0, y: 0.0 }, Point { x: ex, y: ey }];
            let head = arrowhead(&pts, 1.0, &cam).expect("head");
            let px = |p: Point| (p.x * cam.cell_w, p.y * cam.cell_h);
            let (tx, ty) = px(head[0]);
            let (ax, ay) = px(head[1]);
            let (bx, by) = px(head[2]);

            // Both wings equidistant from the tip, in pixels.
            let da = ((ax - tx).powi(2) + (ay - ty).powi(2)).sqrt();
            let db = ((bx - tx).powi(2) + (by - ty).powi(2)).sqrt();
            assert!(
                (da - db).abs() < 0.01,
                "wings asymmetric for ({ex},{ey}): {da} vs {db}"
            );

            // The wing midpoint must lie on the shaft: the vector from
            // tip to midpoint is antiparallel to the shaft direction.
            let (mx, my) = ((ax + bx) / 2.0, (ay + by) / 2.0);
            let (sx, sy) = (ex * cam.cell_w, ey * cam.cell_h);
            let slen = (sx * sx + sy * sy).sqrt();
            let (vx, vy) = (mx - tx, my - ty);
            let vlen = (vx * vx + vy * vy).sqrt();
            let cos = (vx * sx + vy * sy) / (vlen * slen);
            assert!(
                (cos + 1.0).abs() < 1e-3,
                "head not aligned with shaft for ({ex},{ey}): cos={cos}"
            );
        }
    }

    #[test]
    fn arrowhead_points_along_the_last_segment() {
        let cam = Camera::new(8.0, 17.0);
        let pts = [Point { x: 0.0, y: 0.0 }, Point { x: 20.0, y: 0.0 }];
        let head = arrowhead(&pts, 1.0, &cam).expect("head");
        assert_eq!(head.len(), 3);
        // Tip is the final vertex; the base sits behind it in x.
        assert!((head[0].x - 20.0).abs() < 1e-6);
        assert!(head[1].x < head[0].x && head[2].x < head[0].x);
    }

    #[test]
    fn text_anchors_top_left_so_it_does_not_slide_while_typing() {
        let cam = Camera::new(8.0, 17.0);
        let mut e = Element::new("t", Shape::Text, 80.0, 34.0, 0.0, 17.0);
        e.text = "ab".into();
        let short = element_origin(&e, &cam);
        // Growing the string widens the box; the anchor must not move.
        e.text = "abcdefghij".into();
        e.width = 10.0 * cam.cell_w;
        let long = element_origin(&e, &cam);
        assert_eq!((short.x, short.y), (long.x, long.y));
        assert_eq!((short.x, short.y), (10.0, 2.0));
    }

    #[test]
    fn shapes_still_anchor_at_their_centre() {
        let cam = Camera::new(8.0, 17.0);
        let e = Element::new("r", Shape::Rectangle, 80.0, 34.0, 160.0, 68.0);
        let o = element_origin(&e, &cam);
        assert_eq!((o.x, o.y), (160.0 / 8.0, 68.0 / 17.0));
    }

    #[test]
    fn caret_follows_the_character_count() {
        let mut e = Element::new("t", Shape::Text, 0.0, 0.0, 0.0, 0.0);
        e.text = "hello".into();
        let DrawCmd::FillRectangles { rects, .. } = caret_command(&e, ACCENT) else {
            panic!("expected caret rect");
        };
        assert_eq!(rects[0].x, 5.0, "one cell per ASCII character");

        // Container labels are centred, so the caret sits at half-width.
        let mut b = Element::new("b", Shape::Rectangle, 0.0, 0.0, 100.0, 40.0);
        b.text = "hello".into();
        let DrawCmd::FillRectangles { rects, .. } = caret_command(&b, ACCENT) else {
            panic!("expected caret rect");
        };
        assert_eq!(rects[0].x, 2.5);
    }

    fn dashed_box(kind: &str, rounded: bool) -> Element {
        let mut e = Element::new("b", Shape::Rectangle, 0.0, 0.0, 160.0, 68.0);
        if rounded {
            e = e.with_adaptive_rounding();
        }
        e.stroke_style = kind.into();
        e.stroke_color = "#000000".into();
        e
    }

    /// The regression this whole path exists for: VGE's OutlineFill*
    /// ops have no dash concept, so a dashed closed shape must not go
    /// through them or the style is silently dropped.
    #[test]
    fn dashed_closed_shapes_emit_segments_not_an_outline_op() {
        let cam = Camera::new(8.0, 17.0);
        for rounded in [false, true] {
            for kind in ["dashed", "dotted"] {
                let body =
                    element_body(&dashed_box(kind, rounded), 1, &cam).expect("body");
                assert!(
                    !body.commands.iter().any(|c| matches!(
                        c,
                        DrawCmd::OutlineFillRectangles { .. }
                            | DrawCmd::OutlineFillPath { .. }
                            | DrawCmd::OutlineFillPolygon { .. }
                    )),
                    "{kind} rounded={rounded} still uses a continuous outline op"
                );
                let segs = body
                    .commands
                    .iter()
                    .find_map(|c| match c {
                        DrawCmd::DrawLines { lines, .. } => Some(lines.len()),
                        _ => None,
                    })
                    .expect("expected dashed segments");
                assert!(segs > 4, "{kind} rounded={rounded}: only {segs} segments");
            }
        }
    }

    #[test]
    fn solid_closed_shapes_still_use_the_cheap_outline_op() {
        let cam = Camera::new(8.0, 17.0);
        let body = element_body(&dashed_box("solid", false), 1, &cam).expect("body");
        assert!(matches!(
            body.commands[0],
            DrawCmd::OutlineFillRectangles { .. }
        ));
    }

    #[test]
    fn a_dashed_box_keeps_its_fill_and_its_label() {
        let cam = Camera::new(8.0, 17.0);
        let mut e = dashed_box("dashed", true);
        e.background_color = "#a5d8ff".into();
        e.text = "ingest".into();
        let body = element_body(&e, 1, &cam).expect("body");
        assert!(
            body.commands
                .iter()
                .any(|c| matches!(c, DrawCmd::FillPath { .. })),
            "fill must survive the two-pass path"
        );
        assert!(
            body.commands
                .iter()
                .any(|c| matches!(c, DrawCmd::DrawText { .. })),
            "label must survive the two-pass path"
        );
    }

    /// Cells are ~8x17px, so measuring dash arc length in cell units
    /// made vertical dashes physically twice as long as horizontal
    /// ones — very visible on a rectangle perimeter.
    #[test]
    fn dashes_are_the_same_physical_length_in_both_axes() {
        let cam = Camera::new(8.0, 17.0);
        // A 160px run each way: 20 cells across, 160/17 cells down.
        let across = [Point { x: 0.0, y: 0.0 }, Point { x: 20.0, y: 0.0 }];
        let down = [
            Point { x: 0.0, y: 0.0 },
            Point {
                x: 0.0,
                y: 160.0 / 17.0,
            },
        ];
        let h = dash(&across, 16.0, 8.0, &cam);
        let v = dash(&down, 16.0, 8.0, &cam);
        assert_eq!(h.len(), v.len(), "same run length, same dash count");

        let h_px = (h[0].1.x - h[0].0.x) * cam.cell_w;
        let v_px = (v[0].1.y - v[0].0.y) * cam.cell_h;
        assert!(
            (h_px - v_px).abs() < 0.1,
            "horizontal dash {h_px}px vs vertical {v_px}px"
        );
    }

    /// Dotted used to be 3px on / 1.8px off, which reads as solid.
    #[test]
    fn dotted_has_more_gap_than_ink() {
        for lw in [1.0, 2.0, 4.0] {
            let (on, off) = dash_pattern("dotted", lw);
            assert!(off > on, "dotted at {lw}px: {on} on vs {off} off");
        }
        // Dashes scale with stroke width rather than being fixed.
        let (thin, _) = dash_pattern("dashed", 1.0);
        let (thick, _) = dash_pattern("dashed", 4.0);
        assert!(thick > thin);
    }

    /// The open/close seam is where AA coverage is least well defined.
    /// Starting the path on a corner arc left a visible notch there, so
    /// the seam must sit on the flat part of the top edge, clear of
    /// both corner radii.
    #[test]
    fn rounded_rect_seam_sits_on_a_straight_edge() {
        let segs = rounded_rect_path(0.0, 0.0, 20.0, 6.0, 0.8, 0.4);
        let seg = &segs[0];
        assert_eq!(seg.start.y, 0.0, "seam should be on the top edge");
        assert!(
            seg.start.x > 0.8 && seg.start.x < 20.0 - 0.8,
            "seam at {} is inside a corner radius",
            seg.start.x
        );
        // ClosePath itself draws the run back to the seam, so the last
        // real node must land on the top edge — collinear with the
        // closing segment — and there must be no explicit (and then
        // zero-length) run to the seam before it.
        let n = seg.nodes.len();
        assert!(matches!(seg.nodes[n - 1], PathNode::ClosePath));
        match seg.nodes[n - 2] {
            PathNode::ArcEllipseTo { dst, .. } => {
                assert_eq!(dst.y, seg.start.y, "closing segment must be horizontal");
                assert!(dst.x < seg.start.x, "closing run should have real length");
            }
            ref other => panic!("expected the top-left arc before the close, got {other:?}"),
        }
    }

    #[test]
    fn rounded_rect_radii_clamp_to_the_box() {
        // Radii larger than the box must not invert the geometry.
        let segs = rounded_rect_path(0.0, 0.0, 4.0, 2.0, 99.0, 99.0);
        let seg = &segs[0];
        assert!(seg.start.x >= 0.0 && seg.start.x <= 4.0);
        for n in &seg.nodes {
            if let PathNode::ArcEllipseTo { rx, ry, .. } = n {
                assert!(*rx <= 2.0 + 1e-6 && *ry <= 1.0 + 1e-6, "rx={rx} ry={ry}");
            }
        }
    }

    #[test]
    fn perimeters_close_the_loop_and_stay_within_bounds() {
        let cam = Camera::new(8.0, 17.0);
        let (hx, hy) = (10.0, 2.0);
        for (shape, r) in [
            (Shape::Rectangle, 0.0),
            (Shape::Rectangle, 16.0),
            (Shape::Ellipse, 0.0),
            (Shape::Diamond, 0.0),
        ] {
            let pts = perimeter(shape, hx, hy, r, &cam);
            assert!(pts.len() >= 5, "{shape:?} perimeter too short");
            assert_eq!(
                (pts[0].x, pts[0].y),
                (pts[pts.len() - 1].x, pts[pts.len() - 1].y),
                "{shape:?} perimeter must be a closed loop"
            );
            for p in &pts {
                assert!(
                    p.x.abs() <= hx + 1e-3 && p.y.abs() <= hy + 1e-3,
                    "{shape:?} point {p:?} escapes the bounding box"
                );
            }
        }
    }

    #[test]
    fn element_centres_geometry_on_its_origin() {
        let cam = Camera::new(8.0, 17.0);
        let e = Element::new("r", Shape::Rectangle, 80.0, 40.0, 160.0, 80.0);
        let body = element_body(&e, 1, &cam).expect("body");
        // Origin is the shape centre in canvas cells.
        assert!((body.origin.x - (160.0 / 8.0)).abs() < 1e-4);
        assert!((body.origin.y - (80.0 / 17.0)).abs() < 1e-4);
        let DrawCmd::OutlineFillRectangles { rects, .. } = &body.commands[0] else {
            panic!("expected rects");
        };
        assert!((rects[0].x + rects[0].w / 2.0).abs() < 1e-4);
    }
}

