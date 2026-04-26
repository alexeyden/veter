// Render VGE elements to the femtovg canvas. This sits between glyph
// rendering and the scrollbar in TerminalRenderer::render.

use std::collections::HashMap;

use femtovg::{Canvas, Color as FemtoColor, ImageFlags, ImageSource, LineCap, LineJoin, Paint, Path, Renderer};
use imgref::ImgRef;

use vge_protocol::command::{Color, ConcreteStyle, DrawCmd, Style};
use vge_protocol::path::{arc_to_beziers, PathNode, PathSegment};

use super::state::{UploadedImage, VgeState};
use crate::renderer::TerminalRenderer;

const MAGENTA: FemtoColor = FemtoColor {
    r: 1.0,
    g: 0.0,
    b: 1.0,
    a: 1.0,
};

fn flat_to_femto(c: Color) -> FemtoColor {
    FemtoColor::rgbaf(c.r, c.g, c.b, c.a)
}

fn to_px(p: super::codec::Point, ox: f32, oy: f32, cell_w: f32, cell_h: f32) -> (f32, f32) {
    (ox + p.x * cell_w, oy + p.y * cell_h)
}

/// Resolve a `Style` to a femtovg `Paint` at render time. `Style::Ref`
/// resolution against the global table happens here; unresolved refs
/// produce a magenta paint and an `eprintln!` (no response frame, per
/// §7.3).
fn resolve_style_paint(
    style: &Style,
    styles: &HashMap<String, ConcreteStyle>,
    ox: f32,
    oy: f32,
    cell_w: f32,
    cell_h: f32,
) -> Paint {
    match style {
        Style::Flat(c) => Paint::color(flat_to_femto(*c)),
        Style::LinearGradient { p0, p1, c0, c1 } => {
            let (sx, sy) = to_px(*p0, ox, oy, cell_w, cell_h);
            let (ex, ey) = to_px(*p1, ox, oy, cell_w, cell_h);
            Paint::linear_gradient(sx, sy, ex, ey, flat_to_femto(*c0), flat_to_femto(*c1))
        }
        Style::RadialGradient {
            center,
            outer,
            c_inner,
            c_outer,
        } => {
            let (cx, cy) = to_px(*center, ox, oy, cell_w, cell_h);
            let (ox_px, oy_px) = to_px(*outer, ox, oy, cell_w, cell_h);
            let dx = ox_px - cx;
            let dy = oy_px - cy;
            let r = (dx * dx + dy * dy).sqrt().max(1.0);
            Paint::radial_gradient(
                cx,
                cy,
                0.0,
                r,
                flat_to_femto(*c_inner),
                flat_to_femto(*c_outer),
            )
        }
        Style::Ref(id) => match styles.get(id) {
            Some(concrete) => {
                resolve_style_paint(&concrete.as_style(), styles, ox, oy, cell_w, cell_h)
            }
            None => {
                eprintln!("vge: unresolved style ref `{id}` — rendering magenta");
                Paint::color(MAGENTA)
            }
        },
    }
}

/// Build a femtovg Path from a list of TinyVG-style PathSegments. Cell
/// coordinates are mapped to pixels using the supplied origin + cell sizes.
fn build_path(segments: &[PathSegment], ox: f32, oy: f32, cell_w: f32, cell_h: f32) -> Path {
    let mut path = Path::new();
    for seg in segments {
        let (mut cur_x, mut cur_y) = to_px(seg.start, ox, oy, cell_w, cell_h);
        path.move_to(cur_x, cur_y);
        for node in &seg.nodes {
            match node {
                PathNode::LineTo { dst } => {
                    let (x, y) = to_px(*dst, ox, oy, cell_w, cell_h);
                    path.line_to(x, y);
                    cur_x = x;
                    cur_y = y;
                }
                PathNode::HorizontalLineTo { x } => {
                    let nx = ox + x * cell_w;
                    path.line_to(nx, cur_y);
                    cur_x = nx;
                }
                PathNode::VerticalLineTo { y } => {
                    let ny = oy + y * cell_h;
                    path.line_to(cur_x, ny);
                    cur_y = ny;
                }
                PathNode::CubicBezierTo { c0, c1, dst } => {
                    let (c0x, c0y) = to_px(*c0, ox, oy, cell_w, cell_h);
                    let (c1x, c1y) = to_px(*c1, ox, oy, cell_w, cell_h);
                    let (x, y) = to_px(*dst, ox, oy, cell_w, cell_h);
                    path.bezier_to(c0x, c0y, c1x, c1y, x, y);
                    cur_x = x;
                    cur_y = y;
                }
                PathNode::QuadraticBezierTo { c, dst } => {
                    let (cx, cy) = to_px(*c, ox, oy, cell_w, cell_h);
                    let (x, y) = to_px(*dst, ox, oy, cell_w, cell_h);
                    path.quad_to(cx, cy, x, y);
                    cur_x = x;
                    cur_y = y;
                }
                PathNode::ArcCircleTo {
                    large,
                    sweep,
                    radius,
                    dst,
                } => {
                    let p0 = super::codec::Point {
                        x: cur_x,
                        y: cur_y,
                    };
                    let (dx, dy) = to_px(*dst, ox, oy, cell_w, cell_h);
                    // For ArcCircleTo we keep the radius in cell units and
                    // approximate using the average cell dimension. Cells
                    // are anisotropic, so the rendered "circle" is slightly
                    // elliptical — clients that need exact circles should
                    // use ArcEllipseTo with rx/ry compensated for cell ratio
                    // (see §5.1). The average is a reasonable middle.
                    let r_px = radius * (cell_w + cell_h) * 0.5;
                    let beziers = arc_to_beziers(
                        p0,
                        super::codec::Point { x: dx, y: dy },
                        r_px,
                        r_px,
                        0.0,
                        *large,
                        *sweep,
                    );
                    for (c1, c2, end) in beziers {
                        path.bezier_to(c1.x, c1.y, c2.x, c2.y, end.x, end.y);
                    }
                    cur_x = dx;
                    cur_y = dy;
                }
                PathNode::ArcEllipseTo {
                    large,
                    sweep,
                    rx,
                    ry,
                    rotation,
                    dst,
                } => {
                    let p0 = super::codec::Point {
                        x: cur_x,
                        y: cur_y,
                    };
                    let (dx, dy) = to_px(*dst, ox, oy, cell_w, cell_h);
                    // rx/ry are in cell units along x/y respectively.
                    let rx_px = rx * cell_w;
                    let ry_px = ry * cell_h;
                    let beziers = arc_to_beziers(
                        p0,
                        super::codec::Point { x: dx, y: dy },
                        rx_px,
                        ry_px,
                        *rotation,
                        *large,
                        *sweep,
                    );
                    for (c1, c2, end) in beziers {
                        path.bezier_to(c1.x, c1.y, c2.x, c2.y, end.x, end.y);
                    }
                    cur_x = dx;
                    cur_y = dy;
                }
                PathNode::ClosePath => {
                    path.close();
                }
            }
        }
    }
    path
}

fn stroke_paint(base: Paint, line_width_px: f32) -> Paint {
    base.with_line_width(line_width_px)
        .with_line_cap(LineCap::Butt)
        .with_line_join(LineJoin::Round)
}

/// Render every VGE element to `canvas`, anchored to the live screen via
/// `top_of_live_screen` and clipped to the visible viewport.
pub fn render_elements<T: Renderer>(
    canvas: &mut Canvas<T>,
    renderer: &mut TerminalRenderer,
    state: &VgeState,
    top_of_live_screen: i64,
    screen_rows: u16,
    screen_cols: u16,
    scrollback: usize,
) {
    let cell_w = renderer.cell_width;
    let cell_h = renderer.cell_height;
    let stroke_scale = (cell_w + cell_h) * 0.5;

    let visible_top = top_of_live_screen - scrollback as i64;
    let max_row = screen_rows as f32;

    for el in state.render_sorted() {
        if !el.is_visible {
            continue;
        }
        let row_f = (el.anchor_line - visible_top) as f32 + el.sub_row;
        if row_f < -64.0 || row_f > max_row + 64.0 {
            continue;
        }

        let ox = el.origin_x * cell_w;
        let oy = row_f * cell_h;
        let _ = screen_cols;

        for cmd in &el.commands {
            render_cmd(
                canvas,
                renderer,
                cmd,
                &state.styles,
                &state.images,
                ox,
                oy,
                cell_w,
                cell_h,
                stroke_scale,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_cmd<T: Renderer>(
    canvas: &mut Canvas<T>,
    renderer: &mut TerminalRenderer,
    cmd: &DrawCmd,
    styles: &HashMap<String, ConcreteStyle>,
    images: &HashMap<String, UploadedImage>,
    ox: f32,
    oy: f32,
    cell_w: f32,
    cell_h: f32,
    stroke_scale: f32,
) {
    match cmd {
        DrawCmd::FillRectangles { fill, rects } => {
            let paint = resolve_style_paint(fill, styles, ox, oy, cell_w, cell_h);
            let mut path = Path::new();
            for r in rects {
                path.rect(
                    ox + r.x * cell_w,
                    oy + r.y * cell_h,
                    r.w * cell_w,
                    r.h * cell_h,
                );
            }
            canvas.fill_path(&path, &paint);
        }
        DrawCmd::DrawText {
            origin,
            align,
            fill,
            font_style,
            text,
        } => {
            if text.is_empty() {
                return;
            }
            let color = match resolved_color(fill, styles) {
                Some(c) => flat_to_femto(c),
                None => MAGENTA,
            };
            let baseline_x = ox + origin.x * cell_w;
            let baseline_y = oy + origin.y * cell_h + renderer.ascent();
            renderer.draw_vge_text(
                canvas,
                baseline_x,
                baseline_y,
                text,
                color,
                *align,
                *font_style,
            );
        }
        DrawCmd::FillPolygon { fill, points } => {
            let paint = resolve_style_paint(fill, styles, ox, oy, cell_w, cell_h);
            let path = polygon_path(points, ox, oy, cell_w, cell_h, true);
            canvas.fill_path(&path, &paint);
        }
        DrawCmd::FillPath { fill, segments } => {
            let paint = resolve_style_paint(fill, styles, ox, oy, cell_w, cell_h);
            let path = build_path(segments, ox, oy, cell_w, cell_h);
            canvas.fill_path(&path, &paint);
        }
        DrawCmd::DrawLines {
            stroke,
            line_width,
            lines,
        } => {
            let paint = stroke_paint(
                resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                line_width * stroke_scale,
            );
            let mut path = Path::new();
            for (a, b) in lines {
                let (ax, ay) = to_px(*a, ox, oy, cell_w, cell_h);
                let (bx, by) = to_px(*b, ox, oy, cell_w, cell_h);
                path.move_to(ax, ay);
                path.line_to(bx, by);
            }
            canvas.stroke_path(&path, &paint);
        }
        DrawCmd::DrawLineLoop {
            stroke,
            line_width,
            points,
        } => {
            let paint = stroke_paint(
                resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                line_width * stroke_scale,
            );
            let path = polygon_path(points, ox, oy, cell_w, cell_h, true);
            canvas.stroke_path(&path, &paint);
        }
        DrawCmd::DrawLineStrip {
            stroke,
            line_width,
            points,
        } => {
            let paint = stroke_paint(
                resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                line_width * stroke_scale,
            );
            let path = polygon_path(points, ox, oy, cell_w, cell_h, false);
            canvas.stroke_path(&path, &paint);
        }
        DrawCmd::DrawLinePath {
            stroke,
            line_width,
            segments,
        } => {
            let paint = stroke_paint(
                resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                line_width * stroke_scale,
            );
            let path = build_path(segments, ox, oy, cell_w, cell_h);
            canvas.stroke_path(&path, &paint);
        }
        DrawCmd::OutlineFillPolygon {
            fill,
            stroke,
            line_width,
            points,
        } => {
            let path = polygon_path(points, ox, oy, cell_w, cell_h, true);
            canvas.fill_path(
                &path,
                &resolve_style_paint(fill, styles, ox, oy, cell_w, cell_h),
            );
            canvas.stroke_path(
                &path,
                &stroke_paint(
                    resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                    line_width * stroke_scale,
                ),
            );
        }
        DrawCmd::OutlineFillRectangles {
            fill,
            stroke,
            line_width,
            rects,
        } => {
            let mut path = Path::new();
            for r in rects {
                path.rect(
                    ox + r.x * cell_w,
                    oy + r.y * cell_h,
                    r.w * cell_w,
                    r.h * cell_h,
                );
            }
            canvas.fill_path(
                &path,
                &resolve_style_paint(fill, styles, ox, oy, cell_w, cell_h),
            );
            canvas.stroke_path(
                &path,
                &stroke_paint(
                    resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                    line_width * stroke_scale,
                ),
            );
        }
        DrawCmd::OutlineFillPath {
            fill,
            stroke,
            line_width,
            segments,
        } => {
            let path = build_path(segments, ox, oy, cell_w, cell_h);
            canvas.fill_path(
                &path,
                &resolve_style_paint(fill, styles, ox, oy, cell_w, cell_h),
            );
            canvas.stroke_path(
                &path,
                &stroke_paint(
                    resolve_style_paint(stroke, styles, ox, oy, cell_w, cell_h),
                    line_width * stroke_scale,
                ),
            );
        }
        DrawCmd::DrawImage {
            target_rect,
            image_id,
        } => {
            let target_x = ox + target_rect.x * cell_w;
            let target_y = oy + target_rect.y * cell_h;
            let target_w = target_rect.w * cell_w;
            let target_h = target_rect.h * cell_h;

            let mut path = Path::new();
            path.rect(target_x, target_y, target_w, target_h);

            let paint = ensure_image_paint(
                canvas,
                images,
                image_id,
                target_x,
                target_y,
                target_w,
                target_h,
            );
            canvas.fill_path(&path, &paint);
        }
    }
}

/// Resolve an image id to a femtovg `Paint::image(...)`. Lazy-creates
/// the GPU texture on first use; falls back to magenta on missing or
/// failed-to-create images.
fn ensure_image_paint<T: Renderer>(
    canvas: &mut Canvas<T>,
    images: &HashMap<String, UploadedImage>,
    image_id: &str,
    target_x: f32,
    target_y: f32,
    target_w: f32,
    target_h: f32,
) -> Paint {
    let img = match images.get(image_id) {
        Some(i) => i,
        None => {
            eprintln!("vge: DrawImage references missing image `{image_id}` — rendering magenta");
            return Paint::color(MAGENTA);
        }
    };
    let id = match img.gpu.get() {
        Some(id) => id,
        None => {
            let src = ImageSource::from(ImgRef::new(
                &img.pixels,
                img.width as usize,
                img.height as usize,
            ));
            match canvas.create_image(src, ImageFlags::empty()) {
                Ok(id) => {
                    img.gpu.set(Some(id));
                    id
                }
                Err(e) => {
                    eprintln!("vge: create_image failed for `{image_id}`: {e}");
                    return Paint::color(MAGENTA);
                }
            }
        }
    };
    // femtovg's Paint::image inherits NanoVG's nvgImagePattern: the
    // first two args are the **top-left** of the image pattern (the
    // parameter names `cx`/`cy` are misleading), and (width, height)
    // is the size of one image tile. Anchor at the target rect's
    // top-left so the image fills the rect exactly.
    Paint::image(id, target_x, target_y, target_w, target_h, 0.0, 1.0)
}

fn polygon_path(
    points: &[super::codec::Point],
    ox: f32,
    oy: f32,
    cell_w: f32,
    cell_h: f32,
    close: bool,
) -> Path {
    let mut path = Path::new();
    if let Some((first, rest)) = points.split_first() {
        let (sx, sy) = to_px(*first, ox, oy, cell_w, cell_h);
        path.move_to(sx, sy);
        for p in rest {
            let (x, y) = to_px(*p, ox, oy, cell_w, cell_h);
            path.line_to(x, y);
        }
        if close {
            path.close();
        }
    }
    path
}

/// For DrawText: extract a flat color from a Style, resolving `Ref` once.
/// Gradients aren't supported as text fills (Phase II keeps text Flat-only
/// to match Phase I behavior); a gradient style on text falls back to the
/// gradient's first color.
fn resolved_color(style: &Style, styles: &HashMap<String, ConcreteStyle>) -> Option<Color> {
    match style {
        Style::Flat(c) => Some(*c),
        Style::LinearGradient { c0, .. } => Some(*c0),
        Style::RadialGradient { c_inner, .. } => Some(*c_inner),
        Style::Ref(id) => match styles.get(id) {
            Some(concrete) => match concrete {
                ConcreteStyle::Flat(c) => Some(*c),
                ConcreteStyle::LinearGradient { c0, .. } => Some(*c0),
                ConcreteStyle::RadialGradient { c_inner, .. } => Some(*c_inner),
            },
            None => None,
        },
    }
}
