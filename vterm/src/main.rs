mod clipboard;
mod prt;
mod pty;
mod renderer;
mod vge;

use std::io::Read;
use std::num::NonZeroU32;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use femtovg::{renderer::OpenGl, Canvas, Color};
use glutin::config::ConfigTemplateBuilder;
use glutin::context::{ContextAttributesBuilder, PossiblyCurrentContext};
use glutin::display::GetGlDisplay;
use glutin::prelude::*;
use glutin::surface::{SurfaceAttributesBuilder, WindowSurface};
use glutin_winit::DisplayBuilder;
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId};

/// Which vt100 grid a selection belongs to. The host's vt100 is one
/// option; the rest are portals identified by their full path of IDs
/// from the host root down to the leaf, so a selection in a nested
/// portal still resolves uniquely.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SelectionTarget {
    Host,
    Portal(Vec<String>),
}

/// One end-cap of a mouse selection. Anchor/head are stored in
/// **absolute scrollback line coords of the target's vt100** (same
/// units as that engine's `top_of_live_screen`), so the highlight
/// stays pinned to text as the viewport scrolls under it.
#[derive(Clone, Debug)]
struct Selection {
    target: SelectionTarget,
    anchor_line: i64,
    anchor_col: u16,
    head_line: i64,
    head_col: u16,
    /// True while the user is still holding the mouse button. Cleared
    /// on release; the selection itself stays so Ctrl+Shift+C can
    /// re-copy it later.
    dragging: bool,
}

impl Selection {
    fn is_empty(&self) -> bool {
        (self.anchor_line, self.anchor_col) == (self.head_line, self.head_col)
    }

    fn normalized(&self) -> ((i64, u16), (i64, u16)) {
        let a = (self.anchor_line, self.anchor_col);
        let b = (self.head_line, self.head_col);
        if a <= b { (a, b) } else { (b, a) }
    }
}

/// Borrowed snapshot of a portal-tree walk: the leaf portal targeted
/// by a selection plus its pixel origin in canvas coords. Used for
/// hit-testing and pointer→cell projection during a drag.
struct PortalTargetInfo<'a> {
    portal: &'a prt::Portal,
    origin_x_px: f32,
    origin_y_px: f32,
}

/// Walk the portal tree along `path`, returning the leaf portal and
/// its pixel origin. Mirrors the origin math in
/// `prt::render::render_portal_at` so the visible region used by
/// rendering and hit-testing always agrees.
fn resolve_portal_target<'a, CB: vt100::Callbacks>(
    prt: &'a prt::PrtEngine,
    parser: &vt100::Parser<CB>,
    cell_w: f32,
    cell_h: f32,
    path: &[String],
) -> Option<PortalTargetInfo<'a>> {
    if path.is_empty() {
        return None;
    }
    let mut origin_x = 0.0_f32;
    let mut origin_y = 0.0_f32;
    let mut parent_top = prt.top_of_live_screen();
    let mut parent_scrollback = parser.screen().scrollback();
    let mut current_set = prt.state.current();
    let mut last: Option<&prt::Portal> = None;

    for id in path {
        let portal = current_set.portals.get(id)?;
        let visible_top = parent_top - parent_scrollback as i64;
        let row_f = match portal.anchor {
            prt::PortalAnchor::Live { origin_y: oy } => oy as f32,
            prt::PortalAnchor::Scrollback { anchor_line } => {
                (anchor_line - visible_top) as f32
            }
        };
        origin_x += portal.origin_x as f32 * cell_w;
        origin_y += row_f * cell_h;

        last = Some(portal);
        parent_top = portal.children.top_of_live_screen();
        parent_scrollback = portal.vt.screen().scrollback();
        current_set = portal.children.state.current();
    }
    last.map(|p| PortalTargetInfo {
        portal: p,
        origin_x_px: origin_x,
        origin_y_px: origin_y,
    })
}

/// Like `resolve_portal_target` but returns an exclusive borrow so the
/// caller can mutate the leaf portal's vt100 (used by selection-text
/// extraction, which temporarily moves scrollback). Recurses to satisfy
/// the borrow checker — each step only borrows the field it descends
/// into.
fn resolve_portal_target_mut<'a>(
    prt: &'a mut prt::PrtEngine,
    path: &[String],
) -> Option<&'a mut prt::Portal> {
    descend_portal_set_mut(prt.state.current_mut(), path)
}

/// Lightweight check that every id along `path` still exists in the
/// engine. Used to drop a selection whose target portal was destroyed
/// (DeletePortal, RIS/DECSTR, scrollback eviction, alt-screen swap)
/// while the selection was still active.
fn portal_path_exists(prt: &prt::PrtEngine, path: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    let mut current_set = prt.state.current();
    for id in path {
        let Some(portal) = current_set.portals.get(id) else {
            return false;
        };
        current_set = portal.children.state.current();
    }
    true
}

fn descend_portal_set_mut<'a>(
    set: &'a mut prt::PortalSet,
    path: &[String],
) -> Option<&'a mut prt::Portal> {
    if path.is_empty() {
        return None;
    }
    let portal = set.portals.get_mut(&path[0])?;
    if path.len() == 1 {
        Some(portal)
    } else {
        descend_portal_set_mut(portal.children.state.current_mut(), &path[1..])
    }
}

/// Walk a selection range over `parser`'s grid, reading rows by
/// temporarily moving its scrollback offset so each absolute line
/// lands at a known visible row. Generic over the parser's callback
/// type so it works for both the host's `HostCallbacks` and a
/// portal's `PortalCallbacks` parser. `top_of_live_screen` is the
/// parser's engine-tracked anchor (host: `prt.top_of_live_screen()`;
/// portal: `portal.children.top_of_live_screen()`).
fn extract_text_from_parser<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    sel: &Selection,
) -> String {
    let (rows, cols) = parser.screen().size();
    let saved = parser.screen().scrollback();
    let ((s_line, s_col), (e_line, e_col)) = sel.normalized();

    let mut text = String::new();
    for line in s_line..=e_line {
        let target_scrollback = (top_of_live_screen - line).max(0) as usize;
        parser.screen_mut().set_scrollback(target_scrollback);
        let actual = parser.screen().scrollback() as i64;
        let viewport_top = top_of_live_screen - actual;
        let row_in_view = line - viewport_top;
        if row_in_view < 0 || row_in_view >= rows as i64 {
            continue;
        }
        let row = row_in_view as u16;
        let (col_start, col_end_open) = if line == s_line && line == e_line {
            (s_col, e_col.saturating_add(1).min(cols))
        } else if line == s_line {
            (s_col, cols)
        } else if line == e_line {
            (0, e_col.saturating_add(1).min(cols))
        } else {
            (0u16, cols)
        };
        let row_text = parser
            .screen()
            .contents_between(row, col_start, row, col_end_open);
        let wrapped = parser.screen().row_wrapped(row);
        let is_last = line == e_line;
        if is_last || wrapped {
            text.push_str(&row_text);
        } else {
            text.push_str(row_text.trim_end_matches(' '));
            text.push('\n');
        }
    }
    parser.screen_mut().set_scrollback(saved);
    text
}

struct App {
    // Terminal state (dropped first — no GL dependency)
    parser: Option<vt100::Parser<clipboard::HostCallbacks>>,
    pty: Option<pty::Pty>,
    term_renderer: Option<renderer::TerminalRenderer>,
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    vge: Option<vge::VgeEngine>,
    prt: Option<prt::PrtEngine>,
    clipboard: clipboard::ClipboardManager,

    // GL state (dropped in reverse-creation order so the EGL surface
    // is destroyed while the Wayland window still exists)
    canvas: Option<Canvas<OpenGl>>,
    gl_surface: Option<glutin::surface::Surface<WindowSurface>>,
    gl_context: Option<PossiblyCurrentContext>,
    window: Option<Arc<Window>>,

    // Input
    proxy: EventLoopProxy<()>,
    modifiers: ModifiersState,
    /// Last seen pointer position in physical pixels. Set by
    /// `WindowEvent::CursorMoved`, read by the `MouseWheel` handler so
    /// it can convert to `(col, row)` cells when forwarding wheel
    /// events to the PTY.
    cursor_pos: Option<winit::dpi::PhysicalPosition<f64>>,
    /// Active or last-finalized text selection. Some during/after a
    /// Shift+drag; cleared on next non-shift click. `dragging` field
    /// tracks whether the mouse is still held.
    selection: Option<Selection>,
    /// Deadline for the next auto-scroll step while dragging past the
    /// viewport edge. None when not auto-scrolling.
    autoscroll_deadline: Option<Instant>,
}

impl App {
    fn new(proxy: EventLoopProxy<()>) -> Self {
        Self {
            window: None,
            gl_surface: None,
            gl_context: None,
            canvas: None,
            parser: None,
            pty: None,
            term_renderer: None,
            rx: None,
            vge: None,
            prt: None,
            proxy,
            modifiers: ModifiersState::empty(),
            cursor_pos: None,
            clipboard: clipboard::ClipboardManager::new(),
            selection: None,
            autoscroll_deadline: None,
        }
    }

    /// Bracketed-paste mode of the focused vt — host parser, unless a
    /// portal is focused, in which case its inner vt. Same lookup
    /// pattern as DECCKM in `handle_key_input`.
    fn focused_vt_bracketed_paste(&self) -> bool {
        self.prt
            .as_ref()
            .and_then(|p| {
                p.state
                    .focus_chain()
                    .first()
                    .and_then(|id| p.state.current().portals.get(*id))
                    .map(|portal| portal.vt.screen().bracketed_paste())
            })
            .unwrap_or_else(|| {
                self.parser
                    .as_ref()
                    .map(|p| p.screen().bracketed_paste())
                    .unwrap_or(false)
            })
    }

    /// Hit-test the cursor against the portal tree to decide which
    /// vt100 a fresh selection should target. Picks the deepest
    /// visible portal containing the pixel, breaking draw-order ties
    /// using `(draw_order, creation_seq)` to match the render ordering
    /// — i.e. the visually-topmost portal under the cursor wins.
    /// Falls back to `Host` if no portal contains the pointer.
    fn hit_test_target(&self, pos: winit::dpi::PhysicalPosition<f64>) -> SelectionTarget {
        let (Some(prt), Some(parser), Some(tr)) =
            (self.prt.as_ref(), self.parser.as_ref(), self.term_renderer.as_ref())
        else {
            return SelectionTarget::Host;
        };
        let cell_w = tr.cell_width;
        let cell_h = tr.cell_height;
        let px = pos.x as f32;
        let py = pos.y as f32;

        let mut path: Vec<String> = Vec::new();
        let mut origin_x = 0.0_f32;
        let mut origin_y = 0.0_f32;
        let mut parent_top = prt.top_of_live_screen();
        let mut parent_scrollback = parser.screen().scrollback();
        let mut current_set = prt.state.current();

        loop {
            let mut best: Option<(&prt::Portal, f32, f32, (i32, u64))> = None;
            for portal in current_set.portals.values() {
                if !portal.is_visible {
                    continue;
                }
                let visible_top = parent_top - parent_scrollback as i64;
                let row_f = match portal.anchor {
                    prt::PortalAnchor::Live { origin_y: oy } => oy as f32,
                    prt::PortalAnchor::Scrollback { anchor_line } => {
                        (anchor_line - visible_top) as f32
                    }
                };
                let ox = origin_x + portal.origin_x as f32 * cell_w;
                let oy = origin_y + row_f * cell_h;
                let w = portal.size_w as f32 * cell_w;
                let h = portal.size_h as f32 * cell_h;
                if px >= ox && px < ox + w && py >= oy && py < oy + h {
                    let score = (portal.draw_order, portal.creation_seq);
                    if best.map(|b| score > b.3).unwrap_or(true) {
                        best = Some((portal, ox, oy, score));
                    }
                }
            }

            match best {
                Some((portal, ox, oy, _)) => {
                    path.push(portal.id.clone());
                    origin_x = ox;
                    origin_y = oy;
                    parent_top = portal.children.top_of_live_screen();
                    parent_scrollback = portal.vt.screen().scrollback();
                    current_set = portal.children.state.current();
                }
                None => break,
            }
        }

        if path.is_empty() {
            SelectionTarget::Host
        } else {
            SelectionTarget::Portal(path)
        }
    }

    /// Convert a physical-pixel cursor position to (abs_line, col) in
    /// the given target's vt100, clamped to that grid's bounds. None
    /// when state isn't ready or the target portal no longer exists.
    fn cursor_to_abs(
        &self,
        pos: winit::dpi::PhysicalPosition<f64>,
        target: &SelectionTarget,
    ) -> Option<(i64, u16)> {
        let parser = self.parser.as_ref()?;
        let prt = self.prt.as_ref()?;
        let tr = self.term_renderer.as_ref()?;
        let cell_w = tr.cell_width as f64;
        let cell_h = tr.cell_height as f64;

        match target {
            SelectionTarget::Host => {
                let (rows, cols) = parser.screen().size();
                let scrollback = parser.screen().scrollback();
                let row_f = (pos.y / cell_h).floor() as i32;
                let col_f = (pos.x / cell_w).floor() as i32;
                let row = row_f.clamp(0, rows as i32 - 1) as u16;
                let col = col_f.clamp(0, cols as i32 - 1) as u16;
                let viewport_top = prt.top_of_live_screen() - scrollback as i64;
                Some((viewport_top + row as i64, col))
            }
            SelectionTarget::Portal(path) => {
                let info =
                    resolve_portal_target(prt, parser, cell_w as f32, cell_h as f32, path)?;
                let portal = info.portal;
                let local_x = pos.x - info.origin_x_px as f64;
                let local_y = pos.y - info.origin_y_px as f64;
                let row_f = (local_y / cell_h).floor() as i32;
                let col_f = (local_x / cell_w).floor() as i32;
                let row = row_f.clamp(0, portal.size_h as i32 - 1) as u16;
                let col = col_f.clamp(0, portal.size_w as i32 - 1) as u16;
                let portal_top = portal.children.top_of_live_screen();
                let portal_scrollback = portal.vt.screen().scrollback();
                let viewport_top = portal_top - portal_scrollback as i64;
                Some((viewport_top + row as i64, col))
            }
        }
    }

    /// Update the dragging selection's head from the current cursor
    /// position. Re-resolves against the *target* the drag started
    /// against — the cursor cannot escape into a different portal mid-
    /// drag (it's clamped to the target's grid).
    fn update_selection_head(&mut self) {
        let Some(pos) = self.cursor_pos else { return };
        let target = match &self.selection {
            Some(s) if s.dragging => s.target.clone(),
            _ => return,
        };
        let Some((line, col)) = self.cursor_to_abs(pos, &target) else { return };
        if let Some(s) = &mut self.selection
            && s.dragging
        {
            s.head_line = line;
            s.head_col = col;
        }
    }

    /// Walk the selection range and pull plain text out of the target
    /// vt100 grid, line by line. Wrapped rows are joined without a
    /// newline (so a paragraph that wrapped visually copies as one
    /// line); non-wrapped rows have their trailing blanks trimmed and
    /// get a `\n`. Saves and restores the target's scrollback offset
    /// so the user's view isn't disturbed.
    fn extract_selection_text(&mut self) -> Option<String> {
        let sel = self.selection.as_ref()?.clone();
        if sel.is_empty() {
            return None;
        }
        match &sel.target {
            SelectionTarget::Host => {
                let parser = self.parser.as_mut()?;
                let top_live = self.prt.as_ref()?.top_of_live_screen();
                Some(extract_text_from_parser(parser, top_live, &sel))
            }
            SelectionTarget::Portal(path) => {
                let prt = self.prt.as_mut()?;
                let portal = resolve_portal_target_mut(prt, path)?;
                let top_live = portal.children.top_of_live_screen();
                Some(extract_text_from_parser(&mut portal.vt, top_live, &sel))
            }
        }
    }

    /// Copy the current selection to the system clipboard. When
    /// `primary_only` is true (mouse drag completion), only the Linux
    /// PRIMARY selection is updated (so middle-click paste works);
    /// Ctrl+Shift+C and similar explicit copy actions write the main
    /// clipboard too.
    fn copy_selection_to_clipboard(&mut self, primary_only: bool) {
        let Some(text) = self.extract_selection_text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        if !primary_only {
            self.clipboard.set_text(&text);
        }
        self.clipboard.set_primary(&text);
    }

    /// Decide whether the current cursor position warrants an
    /// auto-scroll tick (cursor above or below the viewport while
    /// dragging) and arm/disarm `autoscroll_deadline` accordingly.
    fn maybe_arm_autoscroll(&mut self) {
        let dragging = matches!(&self.selection, Some(s) if s.dragging);
        let out_of_viewport = self.cursor_out_of_viewport();
        self.autoscroll_deadline = if dragging && out_of_viewport.is_some() {
            Some(Instant::now() + Duration::from_millis(50))
        } else {
            None
        };
    }

    /// `Some(direction)` if cursor is above (`-1`) or below (`+1`) the
    /// target's viewport. Returns `None` when inside or unknown. For
    /// portal targets the viewport is the portal's pixel rect, not the
    /// whole window.
    fn cursor_out_of_viewport(&self) -> Option<i32> {
        let pos = self.cursor_pos?;
        let tr = self.term_renderer.as_ref()?;
        let target = self.selection.as_ref().map(|s| &s.target)?;
        match target {
            SelectionTarget::Host => {
                let parser = self.parser.as_ref()?;
                let (rows, _) = parser.screen().size();
                let row_f = (pos.y / tr.cell_height as f64).floor() as i32;
                if row_f < 0 {
                    Some(-1)
                } else if row_f >= rows as i32 {
                    Some(1)
                } else {
                    None
                }
            }
            SelectionTarget::Portal(path) => {
                let prt = self.prt.as_ref()?;
                let parser = self.parser.as_ref()?;
                let info =
                    resolve_portal_target(prt, parser, tr.cell_width, tr.cell_height, path)?;
                let oy = info.origin_y_px as f64;
                let h = info.portal.size_h as f64 * tr.cell_height as f64;
                if pos.y < oy {
                    Some(-1)
                } else if pos.y >= oy + h {
                    Some(1)
                } else {
                    None
                }
            }
        }
    }

    /// Apply one auto-scroll tick: bump the *target's* vt100
    /// scrollback by one row and re-resolve the selection head against
    /// the new viewport. For host the host parser scrolls; for a
    /// portal the portal's own vt100 scrolls (its scrollback ring is
    /// independent).
    fn autoscroll_step(&mut self) {
        let Some(direction) = self.cursor_out_of_viewport() else {
            return;
        };
        let target = self.selection.as_ref().map(|s| s.target.clone());
        match target {
            Some(SelectionTarget::Host) | None => {
                if let Some(parser) = &mut self.parser {
                    let cur = parser.screen().scrollback() as isize;
                    // direction == -1 (cursor above viewport) ⇒ scroll
                    // *up* in the document, i.e. *increase* scrollback.
                    let next = (cur - direction as isize).max(0) as usize;
                    parser.screen_mut().set_scrollback(next);
                }
            }
            Some(SelectionTarget::Portal(path)) => {
                if let Some(prt) = self.prt.as_mut()
                    && let Some(portal) = resolve_portal_target_mut(prt, &path)
                {
                    let cur = portal.vt.screen().scrollback() as isize;
                    let next = (cur - direction as isize).max(0) as usize;
                    portal.vt.screen_mut().set_scrollback(next);
                }
            }
        }
        self.update_selection_head();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn handle_key_input(&mut self, event: &winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }

        // Shift+PageUp/Down for scrollback
        if self.modifiers.shift_key() {
            match &event.logical_key {
                Key::Named(NamedKey::PageUp) => {
                    if let Some(parser) = &mut self.parser {
                        let rows = parser.screen().size().0 as usize;
                        let screen = parser.screen_mut();
                        screen.set_scrollback(screen.scrollback() + rows);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                Key::Named(NamedKey::PageDown) => {
                    if let Some(parser) = &mut self.parser {
                        let rows = parser.screen().size().0 as usize;
                        let screen = parser.screen_mut();
                        screen.set_scrollback(screen.scrollback().saturating_sub(rows));
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                _ => {}
            }
        }

        // Any non-scroll key resets scrollback to bottom
        if let Some(parser) = &mut self.parser {
            parser.screen_mut().set_scrollback(0);
        }

        let pty = match &self.pty {
            Some(p) => p,
            None => return,
        };

        // Ctrl+Shift+{V,C}: paste / copy. Handled before the generic
        // Ctrl+letter block so V/C don't get clobbered into ^V/^C.
        if self.modifiers.control_key()
            && self.modifiers.shift_key()
            && let Key::Character(c) = &event.logical_key
            && let Some(ch) = c.chars().next()
        {
            if ch.eq_ignore_ascii_case(&'v') {
                if let Some(text) = self.clipboard.get_text() {
                    let bracketed = self.focused_vt_bracketed_paste();
                    let bytes = clipboard::build_paste_bytes(&text, bracketed);
                    let _ = pty.write_all(&bytes);
                }
                return;
            }
            if ch.eq_ignore_ascii_case(&'c') {
                self.copy_selection_to_clipboard(false);
                return;
            }
        }

        // Ctrl+key
        if self.modifiers.control_key() {
            match &event.logical_key {
                Key::Character(c) => {
                    if let Some(ch) = c.chars().next()
                        && ch.is_ascii_alphabetic()
                    {
                        let ctrl = (ch.to_ascii_lowercase() as u8) - b'a' + 1;
                        let _ = pty.write_all(&[ctrl]);
                        return;
                    }
                }
                Key::Named(NamedKey::Space) => {
                    let _ = pty.write_all(&[0x00]);
                    return;
                }
                _ => {}
            }
        }

        // Alt+key: send ESC prefix
        if self.modifiers.alt_key()
            && !self.modifiers.control_key()
            && let Some(text) = &event.text
        {
            let mut bytes = vec![0x1b];
            bytes.extend_from_slice(text.as_bytes());
            let _ = pty.write_all(&bytes);
            return;
        }

        // DECCKM (application cursor mode): if the focused vt100 has
        // it set, send SS3 form (`\eOA`/etc) instead of CSI form.
        // vim/less/etc. enable DECCKM via terminfo's `smkx` and won't
        // recognise the CSI form. The "focused vt" is the host's parser
        // unless a portal owns focus, in which case it's that portal's
        // vt — that's how a vmux child keeps its inner DECCKM separate
        // from the host's.
        let app_cursor = self
            .prt
            .as_ref()
            .and_then(|p| {
                p.state
                    .focus_chain()
                    .first()
                    .and_then(|id| p.state.current().portals.get(*id))
                    .map(|portal| portal.vt.screen().application_cursor())
            })
            .unwrap_or_else(|| {
                self.parser
                    .as_ref()
                    .map(|p| p.screen().application_cursor())
                    .unwrap_or(false)
            });

        // Named keys
        let bytes: Option<&[u8]> = match &event.logical_key {
            Key::Named(named) => match named {
                NamedKey::Enter => Some(b"\r"),
                NamedKey::Backspace => Some(b"\x7f"),
                NamedKey::Tab => Some(b"\t"),
                NamedKey::Escape => Some(b"\x1b"),
                NamedKey::ArrowUp => Some(if app_cursor { b"\x1bOA" } else { b"\x1b[A" }),
                NamedKey::ArrowDown => Some(if app_cursor { b"\x1bOB" } else { b"\x1b[B" }),
                NamedKey::ArrowRight => Some(if app_cursor { b"\x1bOC" } else { b"\x1b[C" }),
                NamedKey::ArrowLeft => Some(if app_cursor { b"\x1bOD" } else { b"\x1b[D" }),
                NamedKey::Home => Some(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
                NamedKey::End => Some(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
                NamedKey::Delete => Some(b"\x1b[3~"),
                NamedKey::PageUp => Some(b"\x1b[5~"),
                NamedKey::PageDown => Some(b"\x1b[6~"),
                NamedKey::Insert => Some(b"\x1b[2~"),
                NamedKey::F1 => Some(b"\x1bOP"),
                NamedKey::F2 => Some(b"\x1bOQ"),
                NamedKey::F3 => Some(b"\x1bOR"),
                NamedKey::F4 => Some(b"\x1bOS"),
                NamedKey::F5 => Some(b"\x1b[15~"),
                NamedKey::F6 => Some(b"\x1b[17~"),
                NamedKey::F7 => Some(b"\x1b[18~"),
                NamedKey::F8 => Some(b"\x1b[19~"),
                NamedKey::F9 => Some(b"\x1b[20~"),
                NamedKey::F10 => Some(b"\x1b[21~"),
                NamedKey::F11 => Some(b"\x1b[23~"),
                NamedKey::F12 => Some(b"\x1b[24~"),
                _ => None,
            },
            _ => None,
        };

        if let Some(b) = bytes {
            let _ = pty.write_all(b);
            return;
        }

        // Text input
        if let Some(text) = &event.text {
            let _ = pty.write_all(text.as_bytes());
        }
    }

    /// Invalidate the active selection if its target no longer
    /// exists. Called after each PTY-output tick — that's when portals
    /// can be destroyed by the inner program (DeletePortal, RIS,
    /// scrollback eviction, alt-screen swap).
    fn validate_selection(&mut self) {
        let still_valid = match &self.selection {
            None => true,
            Some(s) => match &s.target {
                SelectionTarget::Host => true,
                SelectionTarget::Portal(path) => self
                    .prt
                    .as_ref()
                    .map(|prt| portal_path_exists(prt, path))
                    .unwrap_or(false),
            },
        };
        if !still_valid {
            self.selection = None;
            self.autoscroll_deadline = None;
        }
    }

    /// Apply OSC 52 set requests buffered since the last drain. Two
    /// sources: the host parser's `HostCallbacks` (host-direct children)
    /// and the PRT engine (anything inside a portal). For each text
    /// payload we just push it to the system clipboard; the last write
    /// wins, matching xterm's behavior.
    fn drain_pending_clipboard(&mut self) {
        if let Some(parser) = &mut self.parser {
            for text in parser.callbacks_mut().pending_set.drain(..) {
                self.clipboard.set_text(&text);
            }
        }
        if let Some(prt) = &mut self.prt {
            for text in prt.take_pending_clipboard_writes() {
                self.clipboard.set_text(&text);
            }
        }
    }

    /// Process PTY output. Returns false if the child process has exited.
    fn process_pty_output(&mut self) -> bool {
        let rx = match &self.rx {
            Some(r) => r,
            None => return false,
        };
        let parser = match &mut self.parser {
            Some(p) => p,
            None => return false,
        };
        let engine = match &mut self.vge {
            Some(e) => e,
            None => return false,
        };
        let prt = match &mut self.prt {
            Some(p) => p,
            None => return false,
        };
        let pty = match &self.pty {
            Some(p) => p,
            None => return false,
        };

        loop {
            match rx.try_recv() {
                Ok(data) => {
                    // Pipeline: PRT extracts ESC_PRT envelopes and observes
                    // RIS/DECSTR/2J/3J events; VGE then extracts ESC_VGE
                    // envelopes from PRT's passthrough; the rest goes to
                    // the host vt100. Each engine's apc passes the other
                    // extension's marker through verbatim, so order is
                    // independent of correctness.
                    let prt_chunk = prt.process_pty_chunk_full(&data);
                    let vge_passthrough = engine.process_pty_chunk(&prt_chunk.passthrough);
                    if !vge_passthrough.is_empty() {
                        parser.process(&vge_passthrough);
                    }
                    // PRT host-screen reactions: scope_reset / cull on
                    // observed RIS/DECSTR/2J/3J, then alt-screen swap +
                    // line tracker + scrollback eviction.
                    prt.handle_terminal_events(&prt_chunk.terminal_events);
                    prt.after_vt100_process(parser);
                    prt.flush_pending_events();
                    engine.after_vt100_process(parser);

                    let prt_resp = prt.take_responses();
                    if !prt_resp.is_empty() {
                        let _ = pty.write_all(&prt_resp);
                    }
                    let resp = engine.take_responses();
                    if !resp.is_empty() {
                        let _ = pty.write_all(&resp);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return true,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let window_attrs = WindowAttributes::default()
            .with_title("vterm")
            .with_inner_size(winit::dpi::LogicalSize::new(800u32, 600u32));

        let template = ConfigTemplateBuilder::new().with_alpha_size(8);
        let display_builder = DisplayBuilder::new().with_window_attributes(Some(window_attrs));

        let (window, gl_config) = display_builder
            .build(event_loop, template, |mut configs| configs.next().unwrap())
            .unwrap();

        let window = Arc::new(window.unwrap());
        let gl_display = gl_config.display();
        let raw_handle = window.window_handle().unwrap().as_raw();

        let context_attrs = ContextAttributesBuilder::new().build(Some(raw_handle));
        let gl_context = unsafe { gl_display.create_context(&gl_config, &context_attrs).unwrap() };

        let size = window.inner_size();
        let surface_attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            raw_handle,
            NonZeroU32::new(size.width.max(1)).unwrap(),
            NonZeroU32::new(size.height.max(1)).unwrap(),
        );
        let gl_surface =
            unsafe { gl_display.create_window_surface(&gl_config, &surface_attrs).unwrap() };
        let gl_context = gl_context.make_current(&gl_surface).unwrap();

        let gl_renderer = unsafe {
            OpenGl::new_from_function_cstr(|s| gl_display.get_proc_address(s) as *const _)
        }
        .unwrap();

        let mut canvas = Canvas::new(gl_renderer).unwrap();
        canvas.set_size(size.width, size.height, 1.0);

        // Initialize terminal renderer and measure cell dimensions
        let font_size = 16.0 * window.scale_factor() as f32;
        let term_renderer = renderer::TerminalRenderer::new(&mut canvas, font_size);
        let (term_cols, term_rows) = term_renderer.terminal_size(size.width, size.height);

        // VGE engine: needs cell pixel dimensions and HiDPI scale factor.
        let cell_px = (
            term_renderer.cell_width.round() as u16,
            term_renderer.cell_height.round() as u16,
        );
        let scale = window.scale_factor() as f32;
        let vge_engine = vge::VgeEngine::new(cell_px, scale);
        // PRT engine: top-level scope (depth 0). Limits default to the
        // recommended caps from §12 (64 portals, 1024×512, 100k
        // scrollback, 1MiB writes, depth 8) and feature bits for every
        // event category Phase 3 wires (bell/title/icon/cwd/clipboard/
        // mouse mode + alt-screen-in-portal). Cell metrics are passed
        // through so per-portal VGE engines (§10) inherit them.
        let prt_engine = prt::PrtEngine::with_metrics(cell_px, scale);

        // Create PTY and parser. Host-direct children (programs not
        // wrapped by a portal) reach the host vt100, so install a
        // Callbacks impl that catches OSC 52 set requests for the
        // system clipboard.
        let parser = vt100::Parser::new_with_callbacks(
            term_rows,
            term_cols,
            10000,
            clipboard::HostCallbacks::default(),
        );
        let pty = pty::Pty::new(term_rows, term_cols).expect("Failed to create PTY");

        // Start PTY reader thread
        let (tx, rx) = mpsc::channel();
        let reader_fd = pty.dup_master().expect("Failed to dup master fd");
        let proxy = self.proxy.clone();

        std::thread::spawn(move || {
            let mut file = std::fs::File::from(reader_fd);
            let mut buf = [0u8; 4096];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                        let _ = proxy.send_event(());
                    }
                    Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
                    Err(_) => break,
                }
            }
            // Drop sender so the main thread sees Disconnected, then wake it up
            drop(tx);
            let _ = proxy.send_event(());
        });

        self.window = Some(window);
        self.gl_surface = Some(gl_surface);
        self.gl_context = Some(gl_context);
        self.canvas = Some(canvas);
        self.parser = Some(parser);
        self.pty = Some(pty);
        self.term_renderer = Some(term_renderer);
        self.rx = Some(rx);
        self.vge = Some(vge_engine);
        self.prt = Some(prt_engine);

        self.window.as_ref().unwrap().request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let (Some(surface), Some(context)) = (&self.gl_surface, &self.gl_context) {
                    surface.resize(
                        context,
                        NonZeroU32::new(size.width.max(1)).unwrap(),
                        NonZeroU32::new(size.height.max(1)).unwrap(),
                    );
                }
                if let Some(tr) = &self.term_renderer {
                    let (cols, rows) = tr.terminal_size(size.width, size.height);
                    if let Some(parser) = &mut self.parser {
                        parser.screen_mut().set_size(rows, cols);
                    }
                    if let Some(pty) = &self.pty {
                        pty.resize(rows, cols);
                    }
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = Some(position);
                if matches!(&self.selection, Some(s) if s.dragging) {
                    self.update_selection_head();
                    self.maybe_arm_autoscroll();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => match (state, button) {
                (ElementState::Pressed, MouseButton::Left) => {
                    if self.modifiers.shift_key() {
                        if let Some(pos) = self.cursor_pos {
                            let target = self.hit_test_target(pos);
                            if let Some((line, col)) = self.cursor_to_abs(pos, &target) {
                                self.selection = Some(Selection {
                                    target,
                                    anchor_line: line,
                                    anchor_col: col,
                                    head_line: line,
                                    head_col: col,
                                    dragging: true,
                                });
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                            }
                        }
                    } else if self.selection.is_some() {
                        // Plain click clears the highlight; matches
                        // standard terminal/text-editor behavior.
                        self.selection = None;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                }
                (ElementState::Released, MouseButton::Left) => {
                    let was_dragging = matches!(&self.selection, Some(s) if s.dragging);
                    if let Some(s) = &mut self.selection {
                        s.dragging = false;
                    }
                    self.autoscroll_deadline = None;
                    if was_dragging {
                        // Linux convention: selection auto-populates the
                        // PRIMARY selection only. Ctrl+Shift+C also
                        // updates the main clipboard.
                        self.copy_selection_to_clipboard(true);
                    }
                }
                (ElementState::Pressed, MouseButton::Middle) => {
                    if let Some(text) = self.clipboard.get_primary()
                        && !text.is_empty()
                    {
                        let bracketed = self.focused_vt_bracketed_paste();
                        let bytes = clipboard::build_paste_bytes(&text, bracketed);
                        if let Some(pty) = &self.pty {
                            let _ = pty.write_all(&bytes);
                        }
                    }
                }
                _ => {}
            },

            WindowEvent::MouseWheel { delta, .. } => {
                // Decide whether to forward the wheel as a mouse-button
                // event to the PTY (when the inner program has enabled
                // mouse reporting in SGR encoding) or use it for the
                // host's own scrollback.
                let (mode, encoding) = self
                    .parser
                    .as_ref()
                    .map(|p| {
                        let s = p.screen();
                        (s.mouse_protocol_mode(), s.mouse_protocol_encoding())
                    })
                    .unwrap_or((
                        vt100::MouseProtocolMode::None,
                        vt100::MouseProtocolEncoding::Default,
                    ));
                let forward = mode != vt100::MouseProtocolMode::None
                    && matches!(encoding, vt100::MouseProtocolEncoding::Sgr);

                if forward {
                    // Convert pointer position + delta into wheel ticks
                    // (1 line = 1 tick). LineDelta is platform-native;
                    // PixelDelta (touchpads) gets binned by cell height.
                    let cell_h = self
                        .term_renderer
                        .as_ref()
                        .map(|t| t.cell_height)
                        .unwrap_or(20.0);
                    let cell_w = self
                        .term_renderer
                        .as_ref()
                        .map(|t| t.cell_width)
                        .unwrap_or(9.0);
                    let ticks = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => y as i32,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            (pos.y / cell_h as f64).round() as i32
                        }
                    };
                    if ticks != 0 {
                        let (col, row) = self
                            .cursor_pos
                            .map(|p| {
                                let c = (p.x / cell_w as f64).floor().max(0.0) as u32 + 1;
                                let r = (p.y / cell_h as f64).floor().max(0.0) as u32 + 1;
                                (c, r)
                            })
                            .unwrap_or((1, 1));
                        let button = if ticks > 0 { 64 } else { 65 };
                        let mut payload = Vec::with_capacity(16 * ticks.unsigned_abs() as usize);
                        for _ in 0..ticks.unsigned_abs() {
                            payload.extend_from_slice(
                                format!("\x1b[<{button};{col};{row}M").as_bytes(),
                            );
                        }
                        if let Some(pty) = &self.pty {
                            let _ = pty.write_all(&payload);
                        }
                    }
                } else {
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            let ch = self
                                .term_renderer
                                .as_ref()
                                .map(|t| t.cell_height)
                                .unwrap_or(20.0);
                            (pos.y as f32 / ch) as isize
                        }
                    };
                    if let Some(parser) = &mut self.parser {
                        let screen = parser.screen_mut();
                        let current = screen.scrollback() as isize;
                        screen.set_scrollback((current + lines).max(0) as usize);
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_key_input(&event);
            }

            WindowEvent::RedrawRequested => {
                let size = self.window.as_ref().unwrap().inner_size();
                let canvas = self.canvas.as_mut().unwrap();
                canvas.set_size(size.width, size.height, 1.0);
                canvas.clear_rect(0, 0, size.width, size.height, Color::rgb(30, 30, 30));

                if let (Some(parser), Some(tr), Some(engine), Some(prt)) = (
                    &mut self.parser,
                    &mut self.term_renderer,
                    &mut self.vge,
                    &mut self.prt,
                ) {
                    // Drop GPU resources for any images that were
                    // dropped since the last frame — both the host
                    // VGE engine's queue and every per-portal VGE
                    // engine's queue, plus anything the PRT engine
                    // accumulated when portals were torn down (delete
                    // / clear / scope_reset / 2J / 3J / scrollback
                    // eviction / alt-swap leave).
                    for gpu_id in engine.take_pending_image_deletes() {
                        canvas.delete_image(gpu_id);
                    }
                    for gpu_id in prt.take_all_pending_image_deletes() {
                        canvas.delete_image(gpu_id);
                    }

                    // Probe actual scrollback buffer size (no public accessor)
                    let current = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(usize::MAX);
                    let max_scrollback = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(current);

                    // Resolve the host-targeted selection (if any) into
                    // visible coords for this frame. Anchor/head are
                    // absolute scrollback line indices, so the highlight
                    // stays pinned to text as the viewport scrolls.
                    // Portal-targeted selections are passed separately
                    // and rendered inside the matching portal.
                    let (rows, cols) = parser.screen().size();
                    let host_sel_range = self.selection.as_ref().and_then(|s| {
                        if !matches!(s.target, SelectionTarget::Host) {
                            return None;
                        }
                        renderer::selection_range_from_abs(
                            s.anchor_line,
                            s.anchor_col,
                            s.head_line,
                            s.head_col,
                            prt.top_of_live_screen(),
                            current,
                            rows,
                            cols,
                        )
                    });
                    let portal_sel_ctx = self.selection.as_ref().and_then(|s| {
                        if let SelectionTarget::Portal(path) = &s.target {
                            Some(prt::render::PortalSelectionCtx {
                                remaining_path: path.as_slice(),
                                anchor_line: s.anchor_line,
                                anchor_col: s.anchor_col,
                                head_line: s.head_line,
                                head_col: s.head_col,
                            })
                        } else {
                            None
                        }
                    });

                    tr.render(
                        canvas,
                        parser.screen(),
                        max_scrollback,
                        &engine.state,
                        engine.top_of_live_screen(),
                        &prt.state,
                        host_sel_range.as_ref(),
                        portal_sel_ctx.as_ref(),
                    );
                }

                canvas.flush();
                self.gl_surface
                    .as_ref()
                    .unwrap()
                    .swap_buffers(self.gl_context.as_ref().unwrap())
                    .unwrap();
            }

            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _event: ()) {
        let alive = self.process_pty_output();
        self.drain_pending_clipboard();
        self.validate_selection();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        if !alive {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Auto-scroll while a Shift+drag is in progress and the cursor
        // sits past the viewport edge. Schedule a wakeup at
        // `autoscroll_deadline`; once it elapses, scroll one row,
        // re-arm if still out of bounds, and reschedule.
        let Some(deadline) = self.autoscroll_deadline else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            self.autoscroll_step();
            self.maybe_arm_autoscroll();
        }
        match self.autoscroll_deadline {
            Some(d) => event_loop.set_control_flow(ControlFlow::WaitUntil(d)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

fn main() {
    let event_loop = EventLoop::new().unwrap();
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy);
    event_loop.run_app(&mut app).unwrap();
}
