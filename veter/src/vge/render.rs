// Render VGE elements to the femtovg canvas. This sits between glyph
// rendering and the scrollbar in TerminalRenderer::render.

use std::collections::HashMap;

use femtovg::{Canvas, Color as FemtoColor, ImageFlags, ImageSource, LineCap, LineJoin, Paint, Path, Renderer, Transform2D};
use imgref::ImgRef;

use vge_protocol::command::{Color, ConcreteStyle, DrawCmd, Style};
use vge_protocol::path::{arc_to_beziers, PathNode, PathSegment};

use super::pick::{PickItem, PickKind, PickLocator, PickRect, VgeSelKind, VgeSelection};
use veter_host::vge::state::{UploadedImage, VgeState};
use crate::renderer::TerminalRenderer;

/// Scope a render walk is running in, carried alongside the geometry so
/// each `DrawText` / `DrawImage` can record where it came from and what
/// was clipping it (see `vge::pick`).
///
/// The active text selection rides along here too: it is addressed by
/// exactly the coordinates this context supplies, so the run that
/// carries the highlight is recognised at the moment it is painted —
/// same transform, same clip, same layout as the glyphs themselves.
#[derive(Clone, Copy)]
pub struct PickCtx<'a> {
    /// Interned portal path; `0` is the host scope.
    pub path_id: u16,
    /// Which screen's element set this walk is over (§5.4).
    pub on_alt: bool,
    /// Every clip in force, intersected, in device pixels.
    pub clip: PickRect,
    /// The selected text run, if any — not necessarily in this scope.
    pub sel: Option<&'a VgeSelection>,
}

impl<'a> PickCtx<'a> {
    /// The host scope with no clip — the top of a render pass.
    pub fn host(on_alt: bool, sel: Option<&'a VgeSelection>) -> Self {
        Self {
            path_id: 0,
            on_alt,
            clip: PickRect::UNBOUNDED,
            sel,
        }
    }
}

/// How the command about to be painted is selected, if it is.
#[derive(Clone, Copy)]
enum Selected {
    /// A byte range of a text run, drawn reverse-video.
    TextRange(usize, usize),
    /// A whole image, drawn with a selection outline.
    Whole,
}

/// What `render_cmd` reports back about a command it just drew, for
/// the pick index. `None` for shapes, which stay transparent to the
/// pointer.
struct PickShape {
    kind: PickKind,
    /// The command's box in the coordinate space it was drawn in.
    local: PickRect,
}

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
///
/// Renders the element tree depth-first per §9.8: each top-level
/// element in (draw_order, creation_seq) order; for each element, its
/// children render first (recursively), then the element's own
/// commands ON TOP, all inside the element's clip rect (if any).
///
/// Kept as a VGE-only convenience entry point. With PRT also active,
/// `prt::render::render_layers` is used instead so VGE elements and
/// portals can interleave by `(draw_order, creation_seq)` (§10).
#[allow(dead_code)]
pub fn render_elements<T: Renderer>(
    canvas: &mut Canvas<T>,
    renderer: &mut TerminalRenderer,
    state: &VgeState,
    top_of_live_screen: i64,
    screen_rows: u16,
    _screen_cols: u16,
    scrollback: usize,
) {
    let ctx = PickCtx::host(state.on_alt(), None);
    for el in state.top_level_sorted() {
        render_one_top_level(
            canvas,
            renderer,
            state,
            el,
            top_of_live_screen,
            screen_rows,
            scrollback,
            ctx,
        );
    }
}

/// Render a single top-level element. Used by the §10 unified layer
/// walker in `prt::render` to interleave VGE elements and portals by
/// `(draw_order, creation_seq)`.
#[allow(clippy::too_many_arguments)]
pub fn render_one_top_level<T: Renderer>(
    canvas: &mut Canvas<T>,
    renderer: &mut TerminalRenderer,
    state: &VgeState,
    el: &super::state::Element,
    top_of_live_screen: i64,
    screen_rows: u16,
    scrollback: usize,
    ctx: PickCtx<'_>,
) {
    if !el.is_visible {
        return;
    }
    let cell_w = renderer.cell_width;
    let cell_h = renderer.cell_height;
    let stroke_scale = (cell_w + cell_h) * 0.5;
    let visible_top = top_of_live_screen - scrollback as i64;
    let max_row = screen_rows as f32;

    let row_f = (el.anchor_line - visible_top) as f32 + el.sub_row;
    if row_f < -1024.0 || row_f > max_row + 1024.0 {
        return;
    }
    let ox = el.origin_x * cell_w;
    let oy = row_f * cell_h;
    render_element(
        canvas,
        renderer,
        state,
        el,
        ox,
        oy,
        cell_w,
        cell_h,
        stroke_scale,
        1.0,
        ctx,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_element<T: Renderer>(
    canvas: &mut Canvas<T>,
    renderer: &mut TerminalRenderer,
    state: &VgeState,
    el: &super::state::Element,
    ox: f32,
    oy: f32,
    cell_w: f32,
    cell_h: f32,
    stroke_scale: f32,
    scale: f32,
    mut ctx: PickCtx<'_>,
) {
    if !el.is_visible {
        return;
    }
    canvas.save();
    // Clip rect first: it is exempt from the element's own transform
    // (§9.11) — always axis-aligned in the untransformed space.
    if let Some(size) = el.clip_size {
        canvas.intersect_scissor(ox, oy, size.x * cell_w, size.y * cell_h);
        // Mirror the scissor into the pick scope. Read the canvas
        // matrix *here*, before this element's own transform is pushed
        // below, so the clip lands in device space the same way
        // femtovg's does.
        let local = PickRect::normalized(ox, oy, size.x * cell_w, size.y * cell_h);
        ctx.clip = ctx.clip.intersect(local.transformed_bounds(&canvas.transform()));
    }
    // Element transform (§9.11). A pure axis-aligned scale + translate
    // (`b == c == 0` — the pan/zoom case) is *folded into the baked
    // coordinate system* rather than pushed onto the canvas matrix: we
    // scale the cell size, shift the origin, and carry a uniform `scale`
    // for glyph rasterisation, leaving the matrix untouched. femtovg then
    // tessellates strokes and rasterises text at the final on-screen
    // resolution, so a magnified element keeps a ~1px AA fringe and crisp
    // glyphs — instead of scaling a result built at zoom 1, which showed up
    // as a notch on stroked seams and blurry text.
    //
    // Rotation / shear still go through the matrix (which premultiplies
    // onto the ancestor stack): rare, and the residual softness on rotated
    // content is acceptable. Baking uses `translate(ox,oy) · M_px ·
    // translate(−ox,−oy)` with `M_px = [a,b,c,d, e·cell_w, f·cell_h]` — the
    // linear part acts on pixel geometry about the origin, the translation
    // is in cell units.
    let (ox, oy, cell_w, cell_h, stroke_scale, scale) = match &el.transform {
        Some(t) if t.b == 0.0 && t.c == 0.0 => {
            let ox = ox + t.e * cell_w;
            let oy = oy + t.f * cell_h;
            let cell_w = t.a * cell_w;
            let cell_h = t.d * cell_h;
            let stroke_scale = (cell_w.abs() + cell_h.abs()) * 0.5;
            let scale = scale * (t.a.abs() + t.d.abs()) * 0.5;
            (ox, oy, cell_w, cell_h, stroke_scale, scale)
        }
        Some(t) => {
            let tx = ox - t.a * ox - t.c * oy + t.e * cell_w;
            let ty = oy - t.b * ox - t.d * oy + t.f * cell_h;
            canvas.set_transform(&Transform2D::new(t.a, t.b, t.c, t.d, tx, ty));
            (ox, oy, cell_w, cell_h, stroke_scale, scale)
        }
        None => (ox, oy, cell_w, cell_h, stroke_scale, scale),
    };

    // Children first, in (draw_order, creation_seq) order.
    let key = element_storage_key(state, el);
    if let Some(k) = key.as_deref() {
        for child in state.children_sorted(k) {
            // Child origins are parent-relative; child-of-child computes
            // the same way recursively.
            let child_ox = ox + child.origin_x * cell_w;
            let child_oy = oy + child.origin_y * cell_h;
            render_element(
                canvas,
                renderer,
                state,
                child,
                child_ox,
                child_oy,
                cell_w,
                cell_h,
                stroke_scale,
                scale,
                ctx,
            );
        }
    }

    // Element's own commands render ON TOP of children (§9.2).
    for (cmd_index, cmd) in el.commands.iter().enumerate() {
        // Does the active text selection live in this command? The
        // interned path is only meaningful during the frame that
        // interned it, which is exactly now.
        let selected = ctx.sel.and_then(|s| {
            let path = renderer.pick.path(ctx.path_id);
            if !s.targets(path, ctx.on_alt, el.creation_seq, cmd_index as u32) {
                return None;
            }
            match s.kind {
                VgeSelKind::Text { .. } => {
                    let (a, b) = s.text_range()?;
                    Some(Selected::TextRange(a, b))
                }
                VgeSelKind::Image => Some(Selected::Whole),
            }
        });
        let shape = render_cmd(
            canvas,
            renderer,
            cmd,
            &state.shared.styles,
            &state.shared.images,
            ox,
            oy,
            cell_w,
            cell_h,
            stroke_scale,
            scale,
            selected,
        );
        // Index what was just painted, taking the device matrix off the
        // canvas rather than recomputing it — `set_transform`
        // premultiplies, so this already carries the portal translation
        // and every ancestor transform.
        if let Some(shape) = shape {
            renderer.pick.push(PickItem {
                loc: PickLocator {
                    path_id: ctx.path_id,
                    on_alt: ctx.on_alt,
                    creation_seq: el.creation_seq,
                    cmd_index: cmd_index as u32,
                },
                kind: shape.kind,
                local: shape.local,
                to_device: canvas.transform(),
                clip: ctx.clip,
            });
        }
    }

    canvas.restore();
}

/// Recover the storage key under which `el` lives in the element table.
/// Named elements use their id as key; anonymous elements use a synthetic
/// key. Since anonymous elements can't be parents, we look up by id when
/// known and otherwise scan (only needed for anonymous elements with
/// children, which the protocol doesn't permit — anonymous elements
/// can't be referenced as parent by anyone).
fn element_storage_key(state: &VgeState, el: &super::state::Element) -> Option<String> {
    if let Some(id) = &el.id
        && state.elements().contains_key(id)
    {
        return Some(id.clone());
    }
    // Anonymous: walk the table to find the matching reference. This is
    // only reached when an anonymous element is rendered, which means it
    // has no children to look up — so we can safely return None and skip
    // the children pass.
    None
}

/// Draw one command. Returns a [`PickShape`] for the two commands that
/// carry user-visible content — text and images — and `None` for the
/// shapes, which stay transparent to the pointer so a full-screen
/// background doesn't swallow grid selection under it.
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
    scale: f32,
    selected: Option<Selected>,
) -> Option<PickShape> {
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
                return None;
            }
            let color = match resolved_color(fill, styles) {
                Some(c) => flat_to_femto(c),
                None => MAGENTA,
            };
            let baseline_x = ox + origin.x * cell_w;
            // Baseline drop scales with the glyphs so the text stays pinned
            // to its origin under zoom.
            let baseline_y = oy + origin.y * cell_h + renderer.ascent() * scale;
            let extent = renderer.draw_vge_text_selected(
                canvas,
                baseline_x,
                baseline_y,
                text,
                color,
                *align,
                *font_style,
                scale,
                match selected {
                    Some(Selected::TextRange(a, b)) => Some((a, b)),
                    _ => None,
                },
            );
            // The pickable box is the run's line box: its top edge is
            // the text origin row (the baseline sits exactly
            // `ascent · scale` below it) and its height is one cell,
            // scaled with the glyphs rather than with `cell_h`, which an
            // anisotropic transform would have stretched independently.
            return Some(PickShape {
                kind: PickKind::Text {
                    byte_len: text.len() as u32,
                    scale,
                },
                local: PickRect::normalized(
                    extent.start_x,
                    baseline_y - renderer.ascent() * scale,
                    extent.total_width,
                    renderer.cell_height * scale,
                ),
            });
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
            source_rect,
        } => {
            let target_x = ox + target_rect.x * cell_w;
            let target_y = oy + target_rect.y * cell_h;
            let target_w = target_rect.w * cell_w;
            let target_h = target_rect.h * cell_h;

            let mut path = Path::new();
            path.rect(target_x, target_y, target_w, target_h);

            let paint = ensure_image_paint(
                canvas,
                renderer,
                images,
                image_id,
                target_x,
                target_y,
                target_w,
                target_h,
                *source_rect,
            );
            canvas.fill_path(&path, &paint);

            // A selected image gets an outline rather than a wash: the
            // point is to say *which* image is about to be copied
            // without recolouring it. Inset by half the stroke so the
            // frame stays inside the target rect, and therefore inside
            // whatever clip the element is under.
            if matches!(selected, Some(Selected::Whole)) {
                let w = (2.0 * scale).max(1.0);
                let mut outline = Path::new();
                outline.rect(
                    target_x + w * 0.5,
                    target_y + w * 0.5,
                    (target_w - w).max(0.0),
                    (target_h - w).max(0.0),
                );
                canvas.stroke_path(
                    &outline,
                    &stroke_paint(Paint::color(renderer.selection_accent()), w),
                );
            }

            return Some(PickShape {
                kind: PickKind::Image,
                local: PickRect::normalized(target_x, target_y, target_w, target_h),
            });
        }
    }
    None
}

/// Resolve an image id to a femtovg `Paint::image(...)`. Lazy-creates
/// the GPU texture on first use; falls back to magenta on missing or
/// failed-to-create images. The renderer-side `GpuImageId` mapping
/// is owned by `TerminalRenderer` so the engine state stays GUI-free
/// (host engines store only the opaque `GpuImageId`).
#[allow(clippy::too_many_arguments)]
fn ensure_image_paint<T: Renderer>(
    canvas: &mut Canvas<T>,
    renderer: &mut TerminalRenderer,
    images: &HashMap<String, UploadedImage>,
    image_id: &str,
    target_x: f32,
    target_y: f32,
    target_w: f32,
    target_h: f32,
    source_rect: Option<super::codec::Rect>,
) -> Paint {
    let img = match images.get(image_id) {
        Some(i) => i,
        None => {
            eprintln!("vge: DrawImage references missing image `{image_id}` — rendering magenta");
            return Paint::color(MAGENTA);
        }
    };
    let femto_id = match img.gpu.get().and_then(|gpu| renderer.lookup_gpu_image(gpu)) {
        Some(id) => id,
        None => {
            let src = ImageSource::from(ImgRef::new(
                &img.pixels,
                img.width as usize,
                img.height as usize,
            ));
            match canvas.create_image(src, ImageFlags::empty()) {
                Ok(femto_id) => {
                    let gpu = renderer.register_gpu_image(femto_id);
                    img.gpu.set(Some(gpu));
                    femto_id
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
    // is the size of one image tile.
    //
    // With no source rect the whole image fills the target rect exactly.
    // With a source rect, we enlarge the pattern so that the requested
    // source sub-rectangle (in source pixels) lands precisely on the
    // target rect, and offset the pattern origin so the sub-rect's
    // top-left aligns with the target's top-left. The fill path (the
    // target rect) scissors away everything outside it, so only the
    // sub-rect shows — stretched to fill the target.
    let (px, py, pw, ph) = match source_rect {
        None => (target_x, target_y, target_w, target_h),
        Some(sr) => {
            let iw = img.width as f32;
            let ih = img.height as f32;
            // Clamp the source rect to the texture so an over-large
            // request can't sample outside it.
            let sx = sr.x.clamp(0.0, iw);
            let sy = sr.y.clamp(0.0, ih);
            let sw = sr.w.min(iw - sx).max(1.0);
            let sh = sr.h.min(ih - sy).max(1.0);
            let scale_x = target_w / sw;
            let scale_y = target_h / sh;
            (
                target_x - sx * scale_x,
                target_y - sy * scale_y,
                iw * scale_x,
                ih * scale_y,
            )
        }
    };
    Paint::image(femto_id, px, py, pw, ph, 0.0, 1.0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use femtovg::renderer::Void;
    use std::cell::Cell;
    use veter_host::vge::state::{Element, UploadedImage};
    use vge_protocol::codec::{Point, Rect, Transform};
    use vge_protocol::command::{Align, FontStyle};

    fn element(commands: Vec<DrawCmd>) -> Element {
        Element {
            id: Some("e".into()),
            commands,
            parent: None,
            children: Vec::new(),
            clip_size: None,
            transform: None,
            anchor_line: 0,
            sub_row: 0.0,
            origin_x: 0.0,
            origin_y: 0.0,
            is_visible: true,
            draw_order: 0,
            creation_seq: 7,
        }
    }

    fn image(w: u32, h: u32) -> UploadedImage {
        UploadedImage {
            width: w,
            height: h,
            pixels: vec![rgb::RGBA8::new(255, 0, 0, 255); (w * h) as usize],
            gpu: Cell::new(None),
            source_encoding: 0x01,
            source_data: Vec::new(),
            refs: 1,
            was_referenced: true,
            pinned: false,
        }
    }

    fn draw_image_at(x: f32, y: f32, w: f32, h: f32) -> DrawCmd {
        DrawCmd::DrawImage {
            target_rect: Rect { x, y, w, h },
            image_id: "img".into(),
            source_rect: None,
        }
    }

    fn draw_text_at(x: f32, y: f32, text: &str) -> DrawCmd {
        DrawCmd::DrawText {
            origin: Point { x, y },
            align: Align::Left,
            fill: Style::Flat(Color {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }),
            font_style: FontStyle(0),
            text: text.into(),
        }
    }

    /// A canvas + renderer over femtovg's `Void` backend, so a render
    /// pass can be driven with no GPU.
    fn harness() -> (Canvas<Void>, TerminalRenderer) {
        let mut canvas = Canvas::new(Void).unwrap();
        canvas.set_size(800, 600, 1.0);
        let tr = TerminalRenderer::new(&mut canvas, 14.0);
        (canvas, tr)
    }

    fn render(canvas: &mut Canvas<Void>, tr: &mut TerminalRenderer, state: &VgeState) {
        render_with_selection(canvas, tr, state, None);
    }

    fn render_with_selection(
        canvas: &mut Canvas<Void>,
        tr: &mut TerminalRenderer,
        state: &VgeState,
        sel: Option<&VgeSelection>,
    ) {
        tr.pick.clear();
        let ctx = PickCtx::host(state.on_alt(), sel);
        for el in state.top_level_sorted() {
            render_one_top_level(canvas, tr, state, el, 0, 24, 0, ctx);
        }
    }

    fn text_selection(seq: u64, anchor: usize, head: usize) -> VgeSelection {
        VgeSelection {
            path: Vec::new(),
            on_alt: false,
            creation_seq: seq,
            cmd_index: 0,
            kind: VgeSelKind::Text {
                anchor,
                head,
                dragging: false,
            },
        }
    }

    fn image_selection(seq: u64) -> VgeSelection {
        VgeSelection {
            path: Vec::new(),
            on_alt: false,
            creation_seq: seq,
            cmd_index: 0,
            kind: VgeSelKind::Image,
        }
    }

    #[test]
    fn image_lands_where_it_was_drawn() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_image_at(2.0, 3.0, 10.0, 5.0)]));

        render(&mut canvas, &mut tr, &state);

        assert_eq!(tr.pick.len(), 1);
        // A point in the middle of the target rect hits; one just
        // outside its right edge does not.
        let hit = tr
            .pick
            .hit((2.0 + 5.0) * cw, (3.0 + 2.5) * ch)
            .expect("centre of the image");
        assert!(matches!(hit.item.kind, PickKind::Image));
        assert_eq!(hit.item.loc.creation_seq, 7);
        assert_eq!(hit.item.loc.cmd_index, 0);
        assert!(tr.pick.hit((2.0 + 10.5) * cw, (3.0 + 2.5) * ch).is_none());
    }

    #[test]
    fn shapes_are_transparent_to_the_pointer() {
        let (mut canvas, mut tr) = harness();
        let mut state = VgeState::new();
        state.elements_mut().insert(
            "e".into(),
            element(vec![DrawCmd::FillRectangles {
                fill: Style::Flat(Color {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                }),
                rects: vec![Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 80.0,
                    h: 24.0,
                }],
            }]),
        );

        render(&mut canvas, &mut tr, &state);
        assert!(tr.pick.is_empty(), "a filled rect must not be pickable");
    }

    #[test]
    fn text_run_is_pickable_and_resolves_back() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_text_at(1.0, 2.0, "hello")]));

        render(&mut canvas, &mut tr, &state);
        assert_eq!(tr.pick.len(), 1);

        // The run's line box starts at the text origin row and runs one
        // cell tall, so a point just inside its top-left corner hits.
        let hit = tr
            .pick
            .hit(1.0 * cw + 1.0, 2.0 * ch + 1.0)
            .expect("start of the run");
        assert!(matches!(hit.item.kind, PickKind::Text { byte_len: 5, .. }));

        // The locator finds the command again in live state.
        match hit.item.resolve(&state) {
            Some(DrawCmd::DrawText { text, .. }) => assert_eq!(text, "hello"),
            other => panic!("expected the text command back, got {other:?}"),
        }

        // A row above the run is a miss.
        assert!(tr.pick.hit(1.0 * cw + 1.0, 1.0 * ch).is_none());
    }

    #[test]
    fn stale_locator_stops_resolving() {
        let (mut canvas, mut tr) = harness();
        let ch = tr.cell_height;

        let mut state = VgeState::new();
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_text_at(0.0, 0.0, "abc")]));
        render(&mut canvas, &mut tr, &state);
        let item = *tr.pick.hit(1.0, ch * 0.5).expect("the run").item;

        // Same element, different text: the recorded byte length no
        // longer matches, so the pick is refused rather than pointing
        // into a string that has moved under it.
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_text_at(0.0, 0.0, "abcdef")]));
        assert!(item.resolve(&state).is_none());

        // Element gone entirely.
        state.elements_mut().clear();
        assert!(item.resolve(&state).is_none());
    }

    #[test]
    fn clip_rect_shrinks_the_pickable_area() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        let mut parent = element(vec![]);
        parent.clip_size = Some(Point { x: 4.0, y: 2.0 });
        parent.children = vec!["child".into()];
        let mut child = element(vec![draw_image_at(0.0, 0.0, 20.0, 10.0)]);
        child.id = Some("child".into());
        child.parent = Some("e".into());
        child.creation_seq = 8;
        state.elements_mut().insert("e".into(), parent);
        state.elements_mut().insert("child".into(), child);

        render(&mut canvas, &mut tr, &state);
        assert_eq!(tr.pick.len(), 1);
        // Inside both the image and the parent's 4x2-cell clip.
        assert!(tr.pick.hit(cw * 1.0, ch * 1.0).is_some());
        // Inside the image, scissored away by the clip.
        assert!(tr.pick.hit(cw * 6.0, ch * 1.0).is_none());
    }

    #[test]
    fn baked_scale_transform_moves_the_box() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        let mut el = element(vec![draw_image_at(0.0, 0.0, 2.0, 2.0)]);
        // Axis-aligned 2x zoom — folded into the baked coordinate
        // system rather than the canvas matrix, so the pick box has to
        // follow the geometry, not the matrix.
        el.transform = Some(Transform {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            e: 0.0,
            f: 0.0,
        });
        state.elements_mut().insert("e".into(), el);

        render(&mut canvas, &mut tr, &state);
        // The 2x2-cell image now covers 4x4 cells.
        assert!(tr.pick.hit(cw * 3.5, ch * 3.5).is_some());
        assert!(tr.pick.hit(cw * 4.5, ch * 3.5).is_none());
    }

    #[test]
    fn rotation_goes_through_the_canvas_matrix() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        let mut el = element(vec![draw_image_at(0.0, 0.0, 4.0, 1.0)]);
        // 90° about the element origin: the wide strip becomes tall.
        el.transform = Some(Transform {
            a: 0.0,
            b: 1.0,
            c: -1.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        });
        state.elements_mut().insert("e".into(), el);

        render(&mut canvas, &mut tr, &state);
        let hit = tr.pick.hit(-ch * 0.5, cw * 2.0);
        assert!(hit.is_some(), "rotated strip should be pickable");
        // The un-rotated position is now empty.
        assert!(tr.pick.hit(cw * 2.0, ch * 0.5).is_none());
    }

    #[test]
    fn paint_order_decides_the_winner() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        let mut under = element(vec![draw_image_at(0.0, 0.0, 4.0, 4.0)]);
        under.draw_order = 0;
        under.creation_seq = 1;
        let mut over = element(vec![draw_image_at(0.0, 0.0, 4.0, 4.0)]);
        over.id = Some("over".into());
        over.draw_order = 5;
        over.creation_seq = 2;
        state.elements_mut().insert("e".into(), under);
        state.elements_mut().insert("over".into(), over);

        render(&mut canvas, &mut tr, &state);
        assert_eq!(tr.pick.len(), 2);
        let hit = tr.pick.hit(cw * 2.0, ch * 2.0).unwrap();
        assert_eq!(hit.item.loc.creation_seq, 2, "higher draw_order wins");
    }

    #[test]
    fn highlighting_a_run_does_not_move_it() {
        let (mut canvas, mut tr) = harness();

        let mut state = VgeState::new();
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_text_at(1.0, 2.0, "hello")]));

        render(&mut canvas, &mut tr, &state);
        let plain = *tr.pick.hit(1.0 * tr.cell_width + 1.0, 2.0 * tr.cell_height + 1.0)
            .expect("the run")
            .item;

        // Same frame, now with two of its characters selected. The
        // highlight paints extra geometry; the pickable box must not
        // move, or a second click would land somewhere else.
        render_with_selection(
            &mut canvas,
            &mut tr,
            &state,
            Some(&text_selection(7, 1, 3)),
        );
        let selected = *tr.pick.hit(1.0 * tr.cell_width + 1.0, 2.0 * tr.cell_height + 1.0)
            .expect("the run")
            .item;

        assert_eq!(plain.local, selected.local);
        assert_eq!(plain.loc, selected.loc);
        assert_eq!(tr.pick.len(), 1);
    }

    #[test]
    fn a_selection_elsewhere_leaves_the_run_alone() {
        let (mut canvas, mut tr) = harness();
        let mut state = VgeState::new();
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_text_at(0.0, 0.0, "abc")]));

        // A selection naming a different element, and one naming a
        // command index this element doesn't have: neither may match.
        for sel in [text_selection(999, 0, 3), {
            let mut s = text_selection(7, 0, 3);
            s.cmd_index = 4;
            s
        }] {
            render_with_selection(&mut canvas, &mut tr, &state, Some(&sel));
            assert_eq!(tr.pick.len(), 1);
        }
    }

    #[test]
    fn out_of_range_selection_offsets_do_not_panic() {
        let (mut canvas, mut tr) = harness();
        let mut state = VgeState::new();
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_text_at(0.0, 0.0, "ab")]));

        // Offsets past the end of the run — what a stale selection
        // looks like for the one frame before validation drops it.
        render_with_selection(
            &mut canvas,
            &mut tr,
            &state,
            Some(&text_selection(7, 0, 99)),
        );
        assert_eq!(tr.pick.len(), 1);
    }

    #[test]
    fn a_selected_image_keeps_its_pick_box() {
        let (mut canvas, mut tr) = harness();
        let (cw, ch) = (tr.cell_width, tr.cell_height);

        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        state
            .elements_mut()
            .insert("e".into(), element(vec![draw_image_at(1.0, 1.0, 6.0, 4.0)]));

        render(&mut canvas, &mut tr, &state);
        let plain = *tr.pick.hit(cw * 3.0, ch * 2.0).expect("the image").item;

        // The outline is stroked inside the target rect, so the
        // pickable box must be unchanged — clicking the image again has
        // to hit the same thing.
        render_with_selection(&mut canvas, &mut tr, &state, Some(&image_selection(7)));
        let selected = *tr.pick.hit(cw * 3.0, ch * 2.0).expect("the image").item;

        assert_eq!(plain.local, selected.local);
        assert_eq!(plain.loc, selected.loc);
        assert_eq!(tr.pick.len(), 1);
    }

    #[test]
    fn an_image_selection_never_highlights_a_text_run() {
        let (mut canvas, mut tr) = harness();
        let mut state = VgeState::new();
        // Text and image share an element, so they share a
        // creation_seq; only the command index and the selection's kind
        // tell them apart.
        state.shared.images.insert("img".into(), image(4, 4));
        state.elements_mut().insert(
            "e".into(),
            element(vec![
                draw_text_at(0.0, 0.0, "label"),
                draw_image_at(0.0, 2.0, 4.0, 4.0),
            ]),
        );

        let mut sel = image_selection(7);
        sel.cmd_index = 0; // points at the *text* command
        render_with_selection(&mut canvas, &mut tr, &state, Some(&sel));
        assert_eq!(tr.pick.len(), 2, "both drawables still indexed");
    }

    #[test]
    fn invisible_elements_are_not_indexed() {
        let (mut canvas, mut tr) = harness();
        let mut state = VgeState::new();
        state.shared.images.insert("img".into(), image(4, 4));
        let mut el = element(vec![draw_image_at(0.0, 0.0, 4.0, 4.0)]);
        el.is_visible = false;
        state.elements_mut().insert("e".into(), el);

        render(&mut canvas, &mut tr, &state);
        assert!(tr.pick.is_empty());
    }
}
