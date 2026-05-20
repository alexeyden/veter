// Unified §10 layer renderer: interleaves top-level VGE elements and
// host portals by `(draw_order, creation_seq)`, then for each portal
// pushes a clip rect, draws the portal's vt100 grid (via
// `TerminalRenderer::draw_screen_at`), draws an unfocused-style cursor
// per `SetCursorStyle.unfocused_style` (§9.2), and recurses into
// sub-portals.
//
// Phase 7 resolves the focused-leaf chain (§9.1, §13.5): only the
// deepest portal whose own engine focus is `Host` renders the focused
// cursor (inverted bg cell, via `draw_screen_at`'s `focused_cursor`
// argument). Every other portal renders the unfocused-style cursor
// from `SetCursorStyle.unfocused_style` (§9.2).

use femtovg::{Canvas, Color, Paint, Path, Renderer};

use prt_protocol::command::CursorStyle;

use veter_host::prt::portal::{Portal, PortalAnchor};
use veter_host::prt::state::PrtState;
use crate::renderer::TerminalRenderer;
use crate::vge;

/// Drives portal-targeted selection rendering. Carries the absolute
/// (target-vt100) selection coords plus the path of portal IDs from
/// here down to the leaf where the highlight should actually paint.
/// At each render frame the path shrinks by one element until it's
/// empty, at which point the current portal *is* the target and a
/// `SelectionRange` is computed against its visible viewport.
pub struct PortalSelectionCtx<'a> {
    pub remaining_path: &'a [String],
    pub anchor_line: i64,
    pub anchor_col: u16,
    pub head_line: i64,
    pub head_col: u16,
    /// Mirror of `Selection::block_cols` so the per-portal render
    /// path can resolve smart pane selections the same way the host
    /// does (rectangle, not stream).
    pub block_cols: Option<(u16, u16)>,
}

/// One top-level layer in the unified render order: either a host VGE
/// element or a host portal.
enum Layer<'a> {
    Vge(&'a vge::state::Element),
    Portal(&'a Portal),
}

/// Walk top-level VGE elements and host portals in
/// `(draw_order, creation_seq)` order, rendering each. Portals
/// recursively render their per-portal sub-portals from inside.
#[allow(clippy::too_many_arguments)]
pub fn render_layers<T: Renderer>(
    canvas: &mut Canvas<T>,
    term_renderer: &mut TerminalRenderer,
    vge_state: &vge::VgeState,
    prt_state: &PrtState,
    top_of_live_screen: i64,
    screen_rows: u16,
    _screen_cols: u16,
    scrollback: usize,
    portal_selection: Option<&PortalSelectionCtx>,
) {
    let mut layers: Vec<(i32, u64, Layer)> = Vec::new();
    for el in vge_state.top_level_sorted() {
        layers.push((el.draw_order, el.creation_seq, Layer::Vge(el)));
    }
    for portal in prt_state.current().portals.values() {
        layers.push((portal.draw_order, portal.creation_seq, Layer::Portal(portal)));
    }
    layers.sort_by_key(|(d, c, _)| (*d, *c));

    let cell_w = term_renderer.cell_width;
    let cell_h = term_renderer.cell_height;
    // Resolved focus chain: empty ⇒ host is the focused leaf; otherwise
    // the last id is the focused-leaf portal, and intermediate ids are
    // ancestors that have passed focus through (their own cursors render
    // unfocused per §13.5).
    let focus_chain = prt_state.focus_chain();

    for (_, _, layer) in layers {
        match layer {
            Layer::Vge(el) => {
                vge::render::render_one_top_level(
                    canvas,
                    term_renderer,
                    vge_state,
                    el,
                    top_of_live_screen,
                    screen_rows,
                    scrollback,
                );
            }
            Layer::Portal(portal) => {
                let sub_sel = portal_selection.and_then(|s| {
                    let first = s.remaining_path.first().map(|s| s.as_str())?;
                    if first != portal.id.as_str() {
                        return None;
                    }
                    Some(PortalSelectionCtx {
                        remaining_path: &s.remaining_path[1..],
                        anchor_line: s.anchor_line,
                        anchor_col: s.anchor_col,
                        head_line: s.head_line,
                        head_col: s.head_col,
                        block_cols: s.block_cols,
                    })
                });
                render_portal_at(
                    canvas,
                    term_renderer,
                    portal,
                    0.0,
                    0.0,
                    top_of_live_screen,
                    scrollback,
                    cell_w,
                    cell_h,
                    prt_state.cursor_style,
                    &focus_chain,
                    sub_sel.as_ref(),
                );
            }
        }
    }
}

/// Render `portal` (and its sub-portal subtree) inside its bounds.
/// `parent_origin_*_px` is the canvas-pixel position of the portal's
/// scope origin (top-left of the parent's grid; canvas origin at the
/// host level). `parent_top_of_live_screen` and `parent_scrollback`
/// describe the parent's vt100 anchor frame, used for Scrollback-mode
/// portals (§5.2).
#[allow(clippy::too_many_arguments)]
fn render_portal_at<T: Renderer>(
    canvas: &mut Canvas<T>,
    term_renderer: &mut TerminalRenderer,
    portal: &Portal,
    parent_origin_x_px: f32,
    parent_origin_y_px: f32,
    parent_top_of_live_screen: i64,
    parent_scrollback: usize,
    cell_w: f32,
    cell_h: f32,
    unfocused_style: CursorStyle,
    focus_chain: &[&str],
    portal_selection: Option<&PortalSelectionCtx>,
) {
    if !portal.is_visible {
        return;
    }

    let visible_top = parent_top_of_live_screen - parent_scrollback as i64;
    let row_f = match portal.anchor {
        PortalAnchor::Live { origin_y } => origin_y as f32,
        PortalAnchor::Scrollback { anchor_line } => (anchor_line - visible_top) as f32,
    };
    let ox_px = parent_origin_x_px + portal.origin_x as f32 * cell_w;
    let oy_px = parent_origin_y_px + row_f * cell_h;
    let w_px = portal.size_w as f32 * cell_w;
    let h_px = portal.size_h as f32 * cell_h;

    // Resolve this portal's role in the focus chain.
    //   chain == [..., self.id]      ⇒ self is the focused leaf.
    //   chain == [self.id, ...rest]  ⇒ self is on the chain but a
    //                                  descendant is the leaf — render
    //                                  unfocused, descend with `rest`.
    //   otherwise                    ⇒ self is on a different branch,
    //                                  render unfocused, descend empty.
    let is_focused_leaf =
        focus_chain.len() == 1 && focus_chain[0] == portal.id.as_str();
    let on_chain = !focus_chain.is_empty() && focus_chain[0] == portal.id.as_str();
    let sub_focus_chain: &[&str] = if on_chain && !is_focused_leaf {
        &focus_chain[1..]
    } else {
        &[]
    };

    canvas.save();
    // §5.3 — clipping is automatic; logical size of the portal does not
    // shrink, only its rendering is masked. `intersect_scissor` ANDs
    // with any enclosing clip already in force (e.g. a parent portal's
    // bounds), so nested clips compose correctly.
    canvas.intersect_scissor(ox_px, oy_px, w_px, h_px);

    // 1. Cells. The focused leaf passes its cursor cell down so
    //    `draw_screen_at` inverts that cell's fg/bg the same way the
    //    host cursor renders. Non-leaf portals pass `None` and draw
    //    their unfocused-style cursor below.
    let cursor_pos = portal.vt.screen().cursor_position();
    let cursor_visible =
        portal.state_cache.cursor_visible && !portal.vt.screen().hide_cursor();
    let focused_cursor = if is_focused_leaf && cursor_visible {
        Some(cursor_pos)
    } else {
        None
    };
    // Selection only renders when this portal *is* the target — i.e.
    // the remaining path is empty. Otherwise we keep walking down.
    let portal_sel_range = portal_selection.and_then(|s| {
        if !s.remaining_path.is_empty() {
            return None;
        }
        let portal_top = portal.children.top_of_live_screen();
        let portal_scrollback = portal.vt.screen().scrollback();
        crate::renderer::selection_range_from_abs(
            s.anchor_line,
            s.anchor_col,
            s.head_line,
            s.head_col,
            s.block_cols,
            portal_top,
            portal_scrollback,
            portal.size_h as u16,
            portal.size_w as u16,
        )
    });
    term_renderer.draw_screen_at(
        canvas,
        portal.vt.screen(),
        ox_px,
        oy_px,
        focused_cursor,
        portal_sel_range.as_ref(),
    );

    // 2. Unfocused-style cursor (everyone except the focused leaf).
    //    Drawn before any overlays so VGE chrome / images / sub-portals
    //    layer on top of it consistently.
    if !is_focused_leaf && cursor_visible {
        draw_unfocused_cursor(
            canvas,
            ox_px,
            oy_px,
            cursor_pos.0,
            cursor_pos.1,
            cell_w,
            cell_h,
            unfocused_style,
        );
    }

    // 3. Per-portal VGE elements + sub-portals, interleaved in a single
    //    `(draw_order, creation_seq)` order. Mirrors what
    //    `render_layers` does at the top level. Doing this here means
    //    e.g. a vmux pane (PORTAL_DRAW_ORDER=0) carrying an image
    //    correctly renders UNDER vmux's chrome scrollbar (drawn as
    //    CHROME_DRAW_ORDER=10 VGE elements) — without this merge,
    //    sub-portals were drawn after VGE so any image inside a sub-
    //    portal painted over the chrome.
    //
    //    Sub-portal anchor frame is the parent portal's vt100, not the
    //    host's. Sub-portal `origin_x` / anchor row are in cells from
    //    the parent portal's top-left, so pixel origin is
    //    `(ox_px, oy_px)` plus the cell offset.
    let portal_scrollback = portal.vt.screen().scrollback();
    let sub_top = portal.children.top_of_live_screen();
    let sub_scrollback = portal.vt.screen().scrollback();

    let mut layers: Vec<(i32, u64, Layer<'_>)> = Vec::new();
    for el in portal.vge.state.top_level_sorted() {
        layers.push((el.draw_order, el.creation_seq, Layer::Vge(el)));
    }
    for sub in portal.children.state.current().portals.values() {
        layers.push((sub.draw_order, sub.creation_seq, Layer::Portal(sub)));
    }
    // `top_level_sorted` already returns elements in (draw_order,
    // creation_seq) order, but we re-sort the merged list to interleave
    // with sub-portals correctly.
    layers.sort_by_key(|(d, c, _)| (*d, *c));

    for (_, _, layer) in layers {
        match layer {
            Layer::Vge(el) => {
                // VGE elements assume canvas-origin coords, so translate
                // to the portal's pixel origin for the duration of one
                // element. The enclosing `intersect_scissor` stays in
                // effect across transforms.
                canvas.save();
                canvas.translate(ox_px, oy_px);
                vge::render::render_one_top_level(
                    canvas,
                    term_renderer,
                    &portal.vge.state,
                    el,
                    portal.vge.top_of_live_screen(),
                    portal.size_h as u16,
                    portal_scrollback,
                );
                canvas.restore();
            }
            Layer::Portal(sub) => {
                let next_sel = portal_selection.and_then(|s| {
                    let first = s.remaining_path.first().map(|s| s.as_str())?;
                    if first != sub.id.as_str() {
                        return None;
                    }
                    Some(PortalSelectionCtx {
                        remaining_path: &s.remaining_path[1..],
                        anchor_line: s.anchor_line,
                        anchor_col: s.anchor_col,
                        head_line: s.head_line,
                        head_col: s.head_col,
                    })
                });
                render_portal_at(
                    canvas,
                    term_renderer,
                    sub,
                    ox_px,
                    oy_px,
                    sub_top,
                    sub_scrollback,
                    cell_w,
                    cell_h,
                    unfocused_style,
                    sub_focus_chain,
                    next_sel.as_ref(),
                );
            }
        }
    }

    canvas.restore();
}

/// Draw the unfocused-cursor visual at the portal's cursor cell, per
/// §9.2 host-wide policy: `Hidden` skips, `Hollow` draws an outlined
/// rect, `Dim` draws a translucent fill. Phase 7 will pass a different
/// rendering for the focused-leaf portal.
#[allow(clippy::too_many_arguments)]
fn draw_unfocused_cursor<T: Renderer>(
    canvas: &mut Canvas<T>,
    ox_px: f32,
    oy_px: f32,
    row: u16,
    col: u16,
    cell_w: f32,
    cell_h: f32,
    style: CursorStyle,
) {
    let x = ox_px + col as f32 * cell_w;
    let y = oy_px + row as f32 * cell_h;
    match style {
        CursorStyle::Hidden => {}
        CursorStyle::Hollow => {
            let mut path = Path::new();
            // Inset by 0.5 px so the outline lands on a half-pixel grid
            // and the stroke fills the cell width without spilling.
            path.rect(x + 0.5, y + 0.5, (cell_w - 1.0).max(0.0), (cell_h - 1.0).max(0.0));
            let mut paint = Paint::color(Color::rgba(204, 204, 204, 200));
            paint.set_line_width(1.0);
            canvas.stroke_path(&path, &paint);
        }
        CursorStyle::Dim => {
            let mut path = Path::new();
            path.rect(x, y, cell_w, cell_h);
            canvas.fill_path(&path, &Paint::color(Color::rgba(204, 204, 204, 80)));
        }
    }
}
