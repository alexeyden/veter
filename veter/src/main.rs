// Modules live in the library face (see `src/lib.rs`); the binary
// re-imports them for in-file references. veterd and other workspace
// crates pull the same code through `veter::*`.
use veter::{clipboard, prt, pty, renderer, ses, vft, vge, vss};

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
use winit::window::{Icon, Window, WindowAttributes, WindowId};

/// PNG-encoded window icon, rasterised from `assets/veter.svg`.
/// Decoded once at window creation; failure is non-fatal (the platform
/// just falls back to its default icon). Also note that on Wayland the
/// per-window icon protocol doesn't exist — the compositor resolves the
/// title-bar icon from the window's app_id matching `veter.desktop`,
/// which is set via the platform-Wayland `with_name` extension below.
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../assets/icons/128x128/veter.png");

fn load_window_icon() -> Option<Icon> {
    let img = image::load_from_memory(WINDOW_ICON_PNG).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).ok()
}

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
    /// Smart pane select. When `Some((left, right))`, this is an
    /// ordinary stream selection clipped to columns `[left, right]` —
    /// the head still tracks the pointer, but its column is clamped to
    /// these bounds and middle-row spans are clipped to them too, so
    /// the highlight stays inside the pane (tmux split, opencode side
    /// panel, …) detected at drag-start (Shift+Alt) instead of
    /// bleeding across borders.
    block_cols: Option<(u16, u16)>,
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
    // Smart pane select replaces the "0..cols" full-row span with the
    // detected pane bounds so middle-row content from outside the pane
    // isn't pulled in. Wrap-joining is also disabled — the host vt100's
    // wrap flag reflects the full host width, which is meaningless when
    // we're slicing a narrower pane band out of it.
    let (clip_left, clip_right_open, in_pane) = match sel.block_cols {
        Some((l, r)) => (l, r.saturating_add(1).min(cols), true),
        None => (0u16, cols, false),
    };

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
            (s_col, clip_right_open)
        } else if line == e_line {
            (clip_left, e_col.saturating_add(1).min(cols))
        } else {
            (clip_left, clip_right_open)
        };
        let row_text = parser
            .screen()
            .contents_between(row, col_start, row, col_end_open);
        let wrapped = !in_pane && parser.screen().row_wrapped(row);
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

/// Word-boundary classifier. A "word" is any contiguous run of
/// non-whitespace cells, so double-click grabs paths, URLs, flags,
/// punctuation-glued identifiers, etc. — anything visually contiguous.
/// Empty cells fall through `cell_char_at` as `None` and naturally
/// stop the walk.
fn is_word_char(c: char) -> bool {
    !c.is_whitespace()
}

/// Read the character at (line, col) in `parser`'s grid, using the
/// same temporarily-move-scrollback trick as `extract_text_from_parser`
/// so any absolute line is reachable. Wide-character continuation
/// cells return the lead cell's char so a double-click anywhere on a
/// wide char treats it as one word atom.
fn cell_char_at<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    line: i64,
    col: u16,
) -> Option<char> {
    let (rows, _) = parser.screen().size();
    let target = (top_of_live_screen - line).max(0) as usize;
    parser.screen_mut().set_scrollback(target);
    let actual = parser.screen().scrollback() as i64;
    let viewport_top = top_of_live_screen - actual;
    let row_in_view = line - viewport_top;
    if row_in_view < 0 || row_in_view >= rows as i64 {
        return None;
    }
    let row = row_in_view as u16;
    let cell = parser.screen().cell(row, col)?;
    if cell.is_wide_continuation() && col > 0 {
        return parser.screen().cell(row, col - 1)?.contents().chars().next();
    }
    cell.contents().chars().next()
}

/// Read the background color at (line, col), with the same scrollback
/// trick as `cell_char_at`. Wide continuation cells inherit the lead
/// cell's bg so the two halves don't look like a colour transition to
/// `detect_pane_cols`.
fn cell_bg_at<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    line: i64,
    col: u16,
) -> Option<vt100::Color> {
    let (rows, _) = parser.screen().size();
    let target = (top_of_live_screen - line).max(0) as usize;
    parser.screen_mut().set_scrollback(target);
    let actual = parser.screen().scrollback() as i64;
    let viewport_top = top_of_live_screen - actual;
    let row_in_view = line - viewport_top;
    if row_in_view < 0 || row_in_view >= rows as i64 {
        return None;
    }
    let row = row_in_view as u16;
    let cell = parser.screen().cell(row, col)?;
    if cell.is_wide_continuation() && col > 0 {
        return Some(parser.screen().cell(row, col - 1)?.bgcolor());
    }
    Some(cell.bgcolor())
}

/// A cell counts as a pane boundary if its glyph is a box-drawing
/// (U+2500..U+257F) or block-element (U+2580..U+259F) character. These
/// are what tmux, vmux's own chrome, and TUIs like opencode draw their
/// frames with, so walking until we hit one is a reliable way to find
/// the side of the pane the click landed in.
fn is_pane_boundary_char(c: char) -> bool {
    let code = c as u32;
    (0x2500..=0x257F).contains(&code) || (0x2580..=0x259F).contains(&code)
}

/// Detect the horizontal pane bounds for a smart block selection.
/// Starting at (line, col), walk left and right along the same row,
/// stopping on either a pane-boundary glyph or a background-colour
/// change relative to the anchor cell. The returned `(left, right)` is
/// an inclusive cell range that the drag's column extent will be
/// locked to. Returns `None` if the anchor cell can't be read at all
/// (off-screen, no cell), which leaves the caller to fall back to
/// ordinary stream selection.
fn detect_pane_cols<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    line: i64,
    col: u16,
) -> Option<(u16, u16)> {
    let (_, cols) = parser.screen().size();
    let saved = parser.screen().scrollback();
    let anchor_bg = cell_bg_at(parser, top_of_live_screen, line, col)?;
    let anchor_ch = cell_char_at(parser, top_of_live_screen, line, col);
    // Clicking *on* a border itself: collapse to that single column so
    // the user gets some visible feedback rather than a surprise full-
    // row selection.
    if anchor_ch.is_some_and(is_pane_boundary_char) {
        parser.screen_mut().set_scrollback(saved);
        return Some((col, col));
    }

    let stop = |parser: &mut vt100::Parser<CB>, probe_col: u16| -> bool {
        let ch = cell_char_at(parser, top_of_live_screen, line, probe_col);
        if ch.is_some_and(is_pane_boundary_char) {
            return true;
        }
        match cell_bg_at(parser, top_of_live_screen, line, probe_col) {
            Some(bg) => bg != anchor_bg,
            // Unreadable cell means we've fallen off the grid: stop.
            None => true,
        }
    };

    let mut left = col;
    while left > 0 {
        let probe = left - 1;
        if stop(parser, probe) {
            break;
        }
        left = probe;
    }
    let mut right = col;
    while right + 1 < cols {
        let probe = right + 1;
        if stop(parser, probe) {
            break;
        }
        right = probe;
    }

    parser.screen_mut().set_scrollback(saved);
    Some((left, right))
}

/// True if the row containing `line` has its wrap flag set, meaning
/// the next absolute line is a visual continuation of this one. Used
/// by word-boundary walking to span wrapped paragraphs.
fn row_wrapped_at<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    line: i64,
) -> bool {
    let (rows, _) = parser.screen().size();
    let target = (top_of_live_screen - line).max(0) as usize;
    parser.screen_mut().set_scrollback(target);
    let actual = parser.screen().scrollback() as i64;
    let viewport_top = top_of_live_screen - actual;
    let row_in_view = line - viewport_top;
    if row_in_view < 0 || row_in_view >= rows as i64 {
        return false;
    }
    parser.screen().row_wrapped(row_in_view as u16)
}

/// Find the (start, end) cell range of the word under (click_line,
/// click_col) in `parser`'s grid. Returns inclusive endpoints in the
/// same absolute-line coords used by `Selection`. If the click lands
/// on a non-word cell, returns `None` so the caller can leave the
/// existing selection untouched.
fn find_word_range_in_parser<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    click_line: i64,
    click_col: u16,
) -> Option<((i64, u16), (i64, u16))> {
    let (_, cols) = parser.screen().size();
    let saved = parser.screen().scrollback();

    let click_ch = cell_char_at(parser, top_of_live_screen, click_line, click_col);
    let result = match click_ch {
        Some(c) if is_word_char(c) => {
            let mut s_line = click_line;
            let mut s_col = click_col;
            loop {
                let (prev_line, prev_col) = if s_col == 0 {
                    if !row_wrapped_at(parser, top_of_live_screen, s_line - 1) {
                        break;
                    }
                    (s_line - 1, cols.saturating_sub(1))
                } else {
                    (s_line, s_col - 1)
                };
                match cell_char_at(parser, top_of_live_screen, prev_line, prev_col) {
                    Some(c) if is_word_char(c) => {
                        s_line = prev_line;
                        s_col = prev_col;
                    }
                    _ => break,
                }
            }

            let mut e_line = click_line;
            let mut e_col = click_col;
            loop {
                let (next_line, next_col) = if e_col + 1 >= cols {
                    if !row_wrapped_at(parser, top_of_live_screen, e_line) {
                        break;
                    }
                    (e_line + 1, 0)
                } else {
                    (e_line, e_col + 1)
                };
                match cell_char_at(parser, top_of_live_screen, next_line, next_col) {
                    Some(c) if is_word_char(c) => {
                        e_line = next_line;
                        e_col = next_col;
                    }
                    _ => break,
                }
            }

            Some(((s_line, s_col), (e_line, e_col)))
        }
        _ => None,
    };

    parser.screen_mut().set_scrollback(saved);
    result
}

/// Host-level pre-attach state stashed on the first VSS
/// `SnapshotBegin` so a `DetachNotify` later can roll back to the
/// view the user had before they attached. Same idea as
/// `veter_host::prt::portal::PreAttachBackup` but for the outermost
/// host engines.
struct HostVssBackup {
    vt: Vec<u8>,
    vge: Vec<u8>,
    prt: Vec<u8>,
}

struct App {
    // Terminal state (dropped first — no GL dependency)
    parser: Option<vt100::Parser<clipboard::HostCallbacks>>,
    pty: Option<pty::Pty>,
    term_renderer: Option<renderer::TerminalRenderer>,
    rx: Option<mpsc::Receiver<Vec<u8>>>,
    vge: Option<vge::VgeEngine>,
    prt: Option<prt::PrtEngine>,
    vft: Option<vft::VftEngine>,
    vss: Option<vss::VssEngine>,
    /// Host-level SES engine. The local renderer is never itself a
    /// session, so this is always a non-session engine: it answers a
    /// `vmux` SES probe with `in_session = false` (a fast definitive
    /// "no session" instead of a probe timeout) and refuses detach.
    ses: Option<ses::SesEngine>,
    /// Host-level pre-attach backup: same role as the per-portal
    /// `Portal::pre_attach_backup`, but for the rare case where a
    /// `veterd attach` writes to the host's outermost vt100 / VGE /
    /// PRT instead of into a vmux pane portal. Saved on the first
    /// VSS `SnapshotBegin` of an attach; restored on `DetachNotify`.
    vss_pre_attach_backup: Option<HostVssBackup>,
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
    /// Most recent left-press recorded in the local-selection branch
    /// (time, target, pixel position). A subsequent press within
    /// `DOUBLE_CLICK_INTERVAL` and `DOUBLE_CLICK_RADIUS_PX` of the
    /// same target counts as a double-click and triggers word
    /// selection. Pixel-distance is used instead of cell equality so a
    /// couple of pixels of inter-click jitter doesn't defeat the
    /// detection by crossing a cell boundary. Presses that were
    /// forwarded to the inner program (mouse-mode-on, no shift) don't
    /// update this — they belong to the app, not to the host.
    last_click: Option<(Instant, SelectionTarget, winit::dpi::PhysicalPosition<f64>)>,
    /// Bitmask of mouse buttons currently held: bit 0 = left, 1 =
    /// middle, 2 = right. Used to decide whether motion in
    /// `ButtonMotion` mode should be forwarded and which button code
    /// to report on a motion event.
    mouse_buttons_held: u8,
    /// Last host-cell coords reported as a motion event, used to
    /// dedup so the inner program receives at most one motion event
    /// per cell. `None` means "no motion forwarded yet".
    last_motion_cell: Option<(u32, u32)>,
}

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_RADIUS_PX: f64 = 6.0;

/// Encode keyboard modifiers into the bits xterm-style SGR mouse
/// encoding uses (shift=4, alt=8, ctrl=16). Used by both button and
/// motion forwarders. Shift is gated out of forwarding above, but the
/// bit is encoded defensively in case that ever changes.
fn encode_mouse_modifier_bits(modifiers: ModifiersState) -> u32 {
    let mut bits = 0;
    if modifiers.shift_key() {
        bits |= 4;
    }
    if modifiers.alt_key() {
        bits |= 8;
    }
    if modifiers.control_key() {
        bits |= 16;
    }
    bits
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
            vft: None,
            vss: None,
            ses: None,
            vss_pre_attach_backup: None,
            proxy,
            modifiers: ModifiersState::empty(),
            cursor_pos: None,
            clipboard: clipboard::ClipboardManager::new(),
            selection: None,
            autoscroll_deadline: None,
            last_click: None,
            mouse_buttons_held: 0,
            last_motion_cell: None,
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
            // In a smart pane selection the head still tracks the
            // pointer, but the column is clamped to the pane bounds
            // detected at drag-start so the highlight can't escape.
            s.head_col = match s.block_cols {
                Some((left, right)) => col.clamp(left, right),
                None => col,
            };
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

    /// Host parser's mouse protocol mode and encoding. The host
    /// parser is authoritative because the inner program that toggles
    /// mouse mode (e.g. vmux, vim) speaks to the host's PTY. Defaults
    /// to None/Default before the parser exists.
    fn host_mouse_proto(&self) -> (vt100::MouseProtocolMode, vt100::MouseProtocolEncoding) {
        self.parser
            .as_ref()
            .map(|p| {
                let s = p.screen();
                (s.mouse_protocol_mode(), s.mouse_protocol_encoding())
            })
            .unwrap_or((
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Default,
            ))
    }

    /// 1-indexed host-grid cell under the current cursor, in the form
    /// SGR mouse encoding expects. None if state isn't initialized.
    fn cursor_to_host_cell(&self) -> Option<(u32, u32)> {
        let pos = self.cursor_pos?;
        let tr = self.term_renderer.as_ref()?;
        let cw = tr.cell_width as f64;
        let ch = tr.cell_height as f64;
        let col = (pos.x / cw).floor().max(0.0) as u32 + 1;
        let row = (pos.y / ch).floor().max(0.0) as u32 + 1;
        Some((col, row))
    }

    /// Forward a button press/release to the inner program via SGR
    /// mouse encoding (`\e[<b;c;r{M,m}`). Returns true if the event
    /// was forwarded (caller skips local handling); false if mouse
    /// mode isn't on or the encoding isn't SGR. Mirrors the gating
    /// the wheel handler already does.
    fn try_forward_mouse_button(&self, button: u32, press: bool) -> bool {
        let (mode, encoding) = self.host_mouse_proto();
        if mode == vt100::MouseProtocolMode::None
            || !matches!(encoding, vt100::MouseProtocolEncoding::Sgr)
        {
            return false;
        }
        let Some((col, row)) = self.cursor_to_host_cell() else {
            return false;
        };
        let code = button | encode_mouse_modifier_bits(self.modifiers);
        let suffix = if press { 'M' } else { 'm' };
        let payload = format!("\x1b[<{code};{col};{row}{suffix}");
        if let Some(pty) = &self.pty {
            let _ = pty.write_all(payload.as_bytes());
        }
        true
    }

    /// Forward a pointer-motion event to the inner program when it
    /// has asked for one — `ButtonMotion` (1002) requires a held
    /// button, `AnyMotion` (1003) always reports. Deduplicates per
    /// host cell so the inner program sees at most one event per cell
    /// crossing. Returns true if forwarded.
    fn try_forward_mouse_motion(&mut self) -> bool {
        let (mode, encoding) = self.host_mouse_proto();
        if !matches!(encoding, vt100::MouseProtocolEncoding::Sgr) {
            return false;
        }
        let held = self.mouse_buttons_held;
        let report = match mode {
            vt100::MouseProtocolMode::ButtonMotion => held != 0,
            vt100::MouseProtocolMode::AnyMotion => true,
            _ => false,
        };
        if !report {
            return false;
        }
        let Some((col, row)) = self.cursor_to_host_cell() else {
            return false;
        };
        if self.last_motion_cell == Some((col, row)) {
            return false;
        }
        self.last_motion_cell = Some((col, row));
        let button = if held & 1 != 0 {
            0
        } else if held & 2 != 0 {
            1
        } else if held & 4 != 0 {
            2
        } else {
            3 // AnyMotion with nothing pressed: button-released code
        };
        // Bit 5 (0x20) is the SGR motion flag.
        let code = button | 0x20 | encode_mouse_modifier_bits(self.modifiers);
        let payload = format!("\x1b[<{code};{col};{row}M");
        if let Some(pty) = &self.pty {
            let _ = pty.write_all(payload.as_bytes());
        }
        true
    }

    /// Detect the pane-locked column range for a smart block selection
    /// at (line, col) on `target`. Same host/portal dispatch as
    /// `find_word_range`.
    fn detect_pane_cols_for(
        &mut self,
        target: &SelectionTarget,
        line: i64,
        col: u16,
    ) -> Option<(u16, u16)> {
        match target {
            SelectionTarget::Host => {
                let parser = self.parser.as_mut()?;
                let top_live = self.prt.as_ref()?.top_of_live_screen();
                detect_pane_cols(parser, top_live, line, col)
            }
            SelectionTarget::Portal(path) => {
                let prt = self.prt.as_mut()?;
                let portal = resolve_portal_target_mut(prt, path)?;
                let top_live = portal.children.top_of_live_screen();
                detect_pane_cols(&mut portal.vt, top_live, line, col)
            }
        }
    }

    /// Resolve a double-click at (line, col) on `target` to the word
    /// range under the pointer. Mirrors `extract_selection_text` in
    /// how it dispatches between the host parser and a portal's vt.
    fn find_word_range(
        &mut self,
        target: &SelectionTarget,
        line: i64,
        col: u16,
    ) -> Option<((i64, u16), (i64, u16))> {
        match target {
            SelectionTarget::Host => {
                let parser = self.parser.as_mut()?;
                let top_live = self.prt.as_ref()?.top_of_live_screen();
                find_word_range_in_parser(parser, top_live, line, col)
            }
            SelectionTarget::Portal(path) => {
                let prt = self.prt.as_mut()?;
                let portal = resolve_portal_target_mut(prt, path)?;
                let top_live = portal.children.top_of_live_screen();
                find_word_range_in_parser(&mut portal.vt, top_live, line, col)
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
                        trace_keyboard_send(&[ctrl]);
                        let _ = pty.write_all(&[ctrl]);
                        return;
                    }
                }
                Key::Named(NamedKey::Space) => {
                    trace_keyboard_send(&[0x00]);
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
            trace_keyboard_send(&bytes);
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
            trace_keyboard_send(b);
            let _ = pty.write_all(b);
            return;
        }

        // Text input
        if let Some(text) = &event.text {
            trace_keyboard_send(text.as_bytes());
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
        let vft = match &mut self.vft {
            Some(v) => v,
            None => return false,
        };
        let vss = match &mut self.vss {
            Some(v) => v,
            None => return false,
        };
        let ses = match &mut self.ses {
            Some(s) => s,
            None => return false,
        };
        let vss_backup = &mut self.vss_pre_attach_backup;
        let pty = match &self.pty {
            Some(p) => p,
            None => return false,
        };

        // Drain VFT worker channels first so async events (download
        // chunks, finalisation responses, aborts) join the next
        // outgoing envelope even when no PTY input arrived this tick.
        vft.drive();

        loop {
            match rx.try_recv() {
                Ok(data) => {
                    // Pipeline: PRT extracts ESC_PRT envelopes and observes
                    // RIS/DECSTR/2J/3J events; VGE then extracts ESC_VGE
                    // envelopes from PRT's passthrough; VFT does the same
                    // for ESC_VFT; the rest goes to the host vt100. Each
                    // engine's apc passes the others' markers through
                    // verbatim, so order is independent of correctness.
                    let prt_chunk = prt.process_pty_chunk_full(&data);
                    let vge_passthrough = engine.process_pty_chunk(&prt_chunk.passthrough);
                    let vft_passthrough = vft.process_pty_chunk(&vge_passthrough);
                    let vss_passthrough = vss.process_pty_chunk(&vft_passthrough);
                    // SES is consumed by the immediate host. The local
                    // renderer is not a session, so this just answers a
                    // vmux probe with "no session"; envelopes never
                    // reach the host vt100.
                    let ses_passthrough = ses.process_pty_chunk(&vss_passthrough);
                    if !ses_passthrough.is_empty() {
                        parser.process(&ses_passthrough);
                    }
                    // Apply any completed host-level VSS snapshots.
                    // A snapshot arriving at this level replaces the
                    // host's vt100 / VGE / PRT engines wholesale —
                    // used when veterd runs directly under a veter
                    // host with no intervening vmux pane. The more
                    // common case is per-portal snapshots, handled
                    // by `prt::WritePortal` recursively.
                    let completed = vss.take_completed_snapshots();
                    for cs in completed {
                        // First snapshot of an attach: stash the
                        // pre-attach state so DetachNotify can roll
                        // it back later.
                        if vss_backup.is_none() {
                            *vss_backup = Some(HostVssBackup {
                                vt: parser.screen().binary_snapshot(),
                                vge: engine.binary_snapshot(),
                                prt: prt.binary_snapshot(),
                            });
                        }
                        if let Err(e) =
                            parser.screen_mut().restore_from_binary_snapshot(&cs.vt_bytes)
                        {
                            eprintln!("veter: host VSS vt100 restore failed: {e}");
                        }
                        if let Err(e) = engine.restore_from_binary_snapshot(&cs.vge_bytes) {
                            eprintln!("veter: host VSS VGE restore failed: {e}");
                        }
                        if let Err(e) = prt.restore_from_binary_snapshot(&cs.prt_bytes) {
                            eprintln!("veter: host VSS PRT restore failed: {e}");
                        }
                    }
                    // DetachNotify: roll back to whatever we stashed
                    // on the first snapshot of this attach.
                    if vss.take_detach_signals() > 0 {
                        if let Some(backup) = vss_backup.take() {
                            if let Err(e) =
                                parser.screen_mut().restore_from_binary_snapshot(&backup.vt)
                            {
                                eprintln!("veter: host VSS detach-restore vt100 failed: {e}");
                            }
                            if let Err(e) = engine.restore_from_binary_snapshot(&backup.vge) {
                                eprintln!("veter: host VSS detach-restore VGE failed: {e}");
                            }
                            if let Err(e) = prt.restore_from_binary_snapshot(&backup.prt) {
                                eprintln!("veter: host VSS detach-restore PRT failed: {e}");
                            }
                        }
                    }
                    // PRT host-screen reactions: scope_reset / cull on
                    // observed RIS/DECSTR/2J/3J, then alt-screen swap +
                    // line tracker + scrollback eviction.
                    prt.handle_terminal_events(&prt_chunk.terminal_events);
                    // §5.6 — VFT has no apc-side observation of resets,
                    // so it relies on PRT's terminal event stream.
                    for ev in &prt_chunk.terminal_events {
                        match ev {
                            prt::TerminalEvent::HardReset | prt::TerminalEvent::SoftReset => {
                                vft.abort_all(vft_protocol::frame::ABORT_HOST_RESET, "");
                            }
                            _ => {}
                        }
                    }
                    prt.after_vt100_process(parser);
                    prt.flush_pending_events();
                    engine.after_vt100_process(parser);
                    vft.drive();
                    // Drive every per-portal VFT engine and surface
                    // their async events as RawReply on each portal's
                    // wire (§10 vft-in-portal).
                    prt.drive_and_flush_vft();

                    let prt_resp = prt.take_responses();
                    if !prt_resp.is_empty() {
                        let _ = pty.write_all(&prt_resp);
                    }
                    let resp = engine.take_responses();
                    if !resp.is_empty() {
                        let _ = pty.write_all(&resp);
                    }
                    let vft_resp = vft.take_responses();
                    if !vft_resp.is_empty() {
                        let _ = pty.write_all(&vft_resp);
                    }
                    let vss_resp = vss.take_responses();
                    if !vss_resp.is_empty() {
                        let _ = pty.write_all(&vss_resp);
                    }
                    let ses_resp = ses.take_responses();
                    if !ses_resp.is_empty() {
                        let _ = pty.write_all(&ses_resp);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Even with no incoming bytes, the host VFT engine
                    // and per-portal engines may have produced events
                    // since the `vft.drive()` call above. Drive the
                    // portal tree once more and flush any new bytes.
                    prt.drive_and_flush_vft();
                    let vft_resp = vft.take_responses();
                    if !vft_resp.is_empty() {
                        let _ = pty.write_all(&vft_resp);
                    }
                    let prt_resp = prt.take_responses();
                    if !prt_resp.is_empty() {
                        let _ = pty.write_all(&prt_resp);
                    }
                    return true;
                }
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
            .with_title("Veter")
            .with_window_icon(load_window_icon())
            .with_inner_size(winit::dpi::LogicalSize::new(800u32, 600u32));

        // On Wayland (and X11 fallback) the title-bar icon is resolved
        // from the window's app_id / WM_CLASS matching a .desktop file
        // — `with_window_icon` is a no-op there. Set the name to
        // "veter" so the compositor finds `veter.desktop`.
        #[cfg(target_os = "linux")]
        let window_attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            window_attrs.with_name("veter", "veter")
        };

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
        // VFT wakeup: shared between the host's top-level VFT engine
        // and every per-portal VFT engine spawned via PRT, so worker
        // threads at any nesting depth tick the event loop after
        // pushing chunk events.
        let wakeup_proxy = self.proxy.clone();
        let vft_wakeup: vft::Wakeup = std::sync::Arc::new(move || {
            let _ = wakeup_proxy.send_event(());
        });
        let vft_engine = vft::VftEngine::with_wakeup(vft_wakeup.clone());

        let prt_engine =
            prt::PrtEngine::with_metrics_and_wakeup(cell_px, scale, vft_wakeup);

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
        self.vft = Some(vft_engine);
        self.vss = Some(vss::VssEngine::new());
        self.ses = Some(ses::SesEngine::new());

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
                let local_drag = matches!(&self.selection, Some(s) if s.dragging);
                // When a local drag-select is in progress, motion
                // belongs to the host (selection extension), not the
                // inner program. Shift held means the user is in
                // local-interaction mode regardless of mouse mode.
                if !self.modifiers.shift_key() && !local_drag {
                    self.try_forward_mouse_motion();
                }
                if local_drag {
                    self.update_selection_head();
                    self.maybe_arm_autoscroll();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let shift = self.modifiers.shift_key();
                match (state, button) {
                    (ElementState::Pressed, MouseButton::Left) => {
                        self.mouse_buttons_held |= 1;
                        if !shift && self.try_forward_mouse_button(0, true) {
                            return;
                        }
                        let Some(pos) = self.cursor_pos else { return };
                        let target = self.hit_test_target(pos);
                        let now = Instant::now();
                        let is_double =
                            self.last_click.as_ref().is_some_and(|(t, prev_target, prev_pos)| {
                                if now.duration_since(*t) >= DOUBLE_CLICK_INTERVAL {
                                    return false;
                                }
                                if prev_target != &target {
                                    return false;
                                }
                                let dx = pos.x - prev_pos.x;
                                let dy = pos.y - prev_pos.y;
                                dx * dx + dy * dy
                                    <= DOUBLE_CLICK_RADIUS_PX * DOUBLE_CLICK_RADIUS_PX
                            });
                        if is_double
                            && let Some((line, col)) = self.cursor_to_abs(pos, &target)
                            && let Some(((s_line, s_col), (e_line, e_col))) =
                                self.find_word_range(&target, line, col)
                        {
                            self.selection = Some(Selection {
                                target,
                                anchor_line: s_line,
                                anchor_col: s_col,
                                head_line: e_line,
                                head_col: e_col,
                                dragging: false,
                                block_cols: None,
                            });
                            self.copy_selection_to_clipboard(true);
                            self.last_click = None;
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                            return;
                        }
                        self.last_click = Some((now, target.clone(), pos));
                        let alt = self.modifiers.alt_key();
                        if shift && alt {
                            // Smart pane-aware select: detect the column
                            // bounds of the pane under the click and use
                            // them as a clip range for an otherwise
                            // ordinary stream selection — the drag flows
                            // row-by-row like Shift+drag but never bleeds
                            // outside the detected pane.
                            if let Some((line, col)) = self.cursor_to_abs(pos, &target)
                                && let Some((left, right)) =
                                    self.detect_pane_cols_for(&target, line, col)
                            {
                                let c = col.clamp(left, right);
                                self.selection = Some(Selection {
                                    target,
                                    anchor_line: line,
                                    anchor_col: c,
                                    head_line: line,
                                    head_col: c,
                                    dragging: true,
                                    block_cols: Some((left, right)),
                                });
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                            }
                        } else if shift {
                            if let Some((line, col)) = self.cursor_to_abs(pos, &target) {
                                self.selection = Some(Selection {
                                    target,
                                    anchor_line: line,
                                    anchor_col: col,
                                    head_line: line,
                                    head_col: col,
                                    dragging: true,
                                    block_cols: None,
                                });
                                if let Some(w) = &self.window {
                                    w.request_redraw();
                                }
                            }
                        } else if self.selection.is_some() {
                            // Plain click in a non-mouse-mode terminal
                            // clears the highlight; matches standard
                            // terminal/text-editor behavior.
                            self.selection = None;
                            if let Some(w) = &self.window {
                                w.request_redraw();
                            }
                        }
                    }
                    (ElementState::Released, MouseButton::Left) => {
                        self.mouse_buttons_held &= !1;
                        if !shift && self.try_forward_mouse_button(0, false) {
                            return;
                        }
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
                        self.mouse_buttons_held |= 2;
                        if !shift && self.try_forward_mouse_button(1, true) {
                            return;
                        }
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
                    (ElementState::Released, MouseButton::Middle) => {
                        self.mouse_buttons_held &= !2;
                        if !shift {
                            let _ = self.try_forward_mouse_button(1, false);
                        }
                    }
                    (ElementState::Pressed, MouseButton::Right) => {
                        self.mouse_buttons_held |= 4;
                        if !shift {
                            let _ = self.try_forward_mouse_button(2, true);
                        }
                    }
                    (ElementState::Released, MouseButton::Right) => {
                        self.mouse_buttons_held &= !4;
                        if !shift {
                            let _ = self.try_forward_mouse_button(2, false);
                        }
                    }
                    _ => {}
                }
            }

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
                        tr.release_gpu_image(canvas, gpu_id);
                    }
                    for gpu_id in prt.take_all_pending_image_deletes() {
                        tr.release_gpu_image(canvas, gpu_id);
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
                            s.block_cols,
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
                                block_cols: s.block_cols,
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

/// Diagnostic-only trace of keyboard bytes about to be written to the
/// inner PTY. Enable with `VETER_DEBUG_INPUT=1`; output goes to
/// `/tmp/veter-input.log` with the same hexdump format as veterd's
/// renderer-input trace, so the two logs can be lined up
/// timestamp-wise to find where bytes go missing or get reordered.
fn trace_keyboard_send(bytes: &[u8]) {
    use std::io::Write;
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| {
        std::env::var_os("VETER_DEBUG_INPUT")
            .map(|v| v != "0" && !v.is_empty())
            == Some(true)
    });
    if !enabled {
        return;
    }
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/veter-input.log")
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut line = format!(
        "[{:>10}.{:03}] {:3} bytes: ",
        ts.as_secs(),
        ts.subsec_millis(),
        bytes.len()
    );
    for &b in bytes {
        line.push_str(&format!("{:02X} ", b));
    }
    line.push('|');
    for &b in bytes {
        line.push(if b.is_ascii_graphic() || b == b' ' {
            b as char
        } else {
            '.'
        });
    }
    line.push_str("|\n");
    let _ = file.write_all(line.as_bytes());
}
