// Modules live in the library face (see `src/lib.rs`); the binary
// re-imports them for in-file references. vsd and other workspace
// crates pull the same code through `veter::*`.
use veter::{clipboard, prt, pty, renderer, search, ses, vft, vge, vss};

mod config;
use config::{HostAction, SearchAction};

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

/// Host accent palette published into the reserved `host.*` VGE style
/// namespace (`doc/vector-graphics-extension.md` §7.3). Slot N maps to
/// `host.accent.{N+1}`; the contextual `host.accent` rotates through
/// these by portal nesting depth, so a top-level vmux, a vmux nested
/// inside it, and one nested again render their chrome in slots 0/1/2.
/// The concrete colors come from the user's config (`[accent]`), which
/// defaults to the built-in blue/olive/violet triple in `config.rs`.
fn host_accent_palette(config: &config::Config) -> vge::HostThemePalette {
    vge::HostThemePalette {
        accents: config.accent_palette(),
    }
}

/// Run a user-defined selection command (`config.selection_commands`) on
/// the selected `selection` text. The command runs via `$SHELL -c`, with
/// the selection exported as `$VETER_SELECTION` and passed as positional
/// `$1` (so `xdg-open "$1"` and `xdg-open "$VETER_SELECTION"` both work).
///
/// Fire-and-forget: the child is detached into its own session
/// (`setsid`) with null stdio so a browser/`xdg-open` outlives veter and
/// isn't hit by terminal signals; a short reaper thread waits on it so it
/// doesn't linger as a zombie. Never blocks the UI thread.
fn run_selection_command(command: &str, selection: &str) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = Command::new(shell);
    cmd.arg("-c")
        .arg(command)
        // `$0` = "veter-selection", `$1` / `"$@"` = the selection text.
        .arg("veter-selection")
        .arg(selection)
        .env("VETER_SELECTION", selection)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Expose this exact veter binary so commands can run `veter -e …`
    // (e.g. edit-then-open) even when it isn't on the launcher's PATH.
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("VETER_EXE", exe);
    }
    // SAFETY: setsid() is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("veter: selection command failed to spawn: {e}"),
    }
}

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
    /// on release; the selection itself stays (e.g. for middle-click
    /// paste) until a click or Ctrl+Shift+C clears it.
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

/// End (inclusive last cell) of the next whitespace-delimited token
/// after `(from_line, from_col)`, following soft-wrap and stopping at
/// the end of the logical (wrapped) line. `None` when only whitespace
/// remains to end of line — i.e. the head already covers the last word,
/// which is how incremental expansion detects "whole line selected".
/// Drives [`App::extend_expand`]; a word is any run of non-whitespace
/// cells, matching [`find_word_range_in_parser`]'s [`is_word_char`].
fn next_word_end_in_parser<CB: vt100::Callbacks>(
    parser: &mut vt100::Parser<CB>,
    top_of_live_screen: i64,
    from_line: i64,
    from_col: u16,
) -> Option<(i64, u16)> {
    let (_, cols) = parser.screen().size();
    let saved = parser.screen().scrollback();

    let mut line = from_line;
    let mut col = from_col;
    let mut in_word = false;
    let mut result: Option<(i64, u16)> = None;

    loop {
        // Step to the next cell, following soft-wrap; stop at the end of
        // the logical line (an unwrapped row's last column).
        if col + 1 >= cols {
            if row_wrapped_at(parser, top_of_live_screen, line) {
                line += 1;
                col = 0;
            } else {
                break;
            }
        } else {
            col += 1;
        }
        let is_word = matches!(
            cell_char_at(parser, top_of_live_screen, line, col),
            Some(ch) if is_word_char(ch)
        );
        if is_word {
            in_word = true;
            result = Some((line, col));
        } else if in_word {
            // Reached the end of the next word — done.
            break;
        }
    }

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
    /// `vsd attach` writes to the host's outermost vt100 / VGE /
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
    /// Active scrollback search (Some while the search bar is open).
    /// Opened by `/` when the focused-leaf parser has scrollback > 0;
    /// closed by Esc (restores scrollback) or once the user is done.
    search: Option<SearchState>,

    /// User configuration loaded once at startup (accent palette,
    /// search-chrome colors, host key chords, selection commands).
    /// Missing file / parse error → built-in defaults; see `config.rs`.
    config: config::Config,
    /// Compiled key chord → action tables, derived from `config.keys`.
    keys: config::KeyBindings,
    /// True while the current selection is "fresh" — made since the last
    /// key was sent to the shell. Gates `[[selection_commands]]` so a
    /// lingering highlight can't hijack normal typing: the first key
    /// forwarded to the PTY clears this.
    selection_fresh: bool,
    /// `veter -e <command>` entry-point command: exec this instead of the
    /// default vmux/`$SHELL`. `None` for a normal launch.
    entry_command: Option<Vec<String>>,
}

/// One scrollback-search session, owned by `App::search`.
///
/// The session captures `target_path` at open time so focus changes
/// during the search don't redirect matches mid-session. While
/// `editing`, typed characters edit the query and matches recompute
/// live; after the first Enter, n/N navigate without further query
/// edits (typing a fresh char re-enters editing with a new query).
struct SearchState {
    query: String,
    /// Default true — terminal text is usually skimmed, not grepped.
    /// Toggled by Alt+C in the search bar.
    case_insensitive: bool,
    /// Sub-mode: `true` = typing query, `false` = navigation (n/N).
    editing: bool,
    /// Portal id chain to the target leaf parser at open time. Empty
    /// = host vt100. Resolved to a `&mut Screen` via
    /// `with_target_leaf_screen_mut`.
    target_path: Vec<String>,
    /// Scrollback offset of the target parser when search opened.
    /// Restored on Esc if the user never committed (Enter) a match.
    saved_scrollback: usize,
    /// Lazily-built per-row text index for the target parser. `None`
    /// after a PTY-output invalidation; rebuilt on the next
    /// `recompute_matches` call.
    cache: Option<search::TextIndex>,
    /// Matches for the current `(query, case_insensitive)` against
    /// `cache`. Recomputed whenever query/case changes.
    matches: Vec<search::MatchSpan>,
    /// Index into `matches` of the active match (the one the
    /// viewport is scrolled to). Always `< matches.len()` when
    /// non-empty; `0` if empty.
    current: usize,
    /// Flash.nvim-style jump labels for the currently-visible matches.
    /// Recomputed every frame in `recompute_labels`. Drives both
    /// rendering (`draw_jump_labels`) and the label-key dispatch in
    /// `handle_search_key_input` so the drawn labels and the keymap
    /// always agree.
    labels: Vec<JumpLabel>,
    /// Set once a jump label is first pressed: the overlay stays open in
    /// incremental word-by-word expansion mode. While `Some`, all other
    /// labels vanish and repeated presses of the committed key grow the
    /// selection toward end of line. `None` in the normal query/navigate
    /// phase. See [`ExpandState`].
    expand: Option<ExpandState>,
}

/// Live incremental-expansion state (word-by-word selection growth).
/// Entered by the first jump-label press: the label's word becomes the
/// initial selection, and every subsequent press of `ch` extends the
/// selection head to the end of the next whitespace-delimited token,
/// following soft-wrap and stopping at the end of the logical line.
#[derive(Clone, Debug)]
struct ExpandState {
    /// Leaf the selection lives in — same target the selection carries.
    target: SelectionTarget,
    /// The committed label key; only this key extends the selection.
    ch: char,
    /// End (inclusive last cell) of the currently-selected span. Marches
    /// forward one word per press; mirrors the live selection head (the
    /// selection's anchor — the first word's start — never moves).
    head: (i64, u16),
}

/// One flash jump label: a key, the match it acts on, and the cell the
/// glyph is drawn at (just past the end of that match's word, in the
/// target leaf's absolute scrollback coords).
#[derive(Clone, Copy, Debug)]
struct JumpLabel {
    match_idx: usize,
    ch: char,
    anchor_line: i64,
    anchor_col: u16,
}

impl SearchState {
    fn new(target_path: Vec<String>, saved_scrollback: usize) -> Self {
        Self {
            query: String::new(),
            case_insensitive: true,
            editing: true,
            target_path,
            saved_scrollback,
            cache: None,
            matches: Vec::new(),
            current: 0,
            labels: Vec::new(),
            expand: None,
        }
    }
}

/// Home-row alphabet for flash-style jump labels, assigned in order to
/// visible matches. Chars that could continue the current query (the
/// cell immediately after a match) are excluded at assignment time so a
/// keypress is unambiguous: an active label selects, anything else edits
/// the query. `n`/`N` are reserved for match navigation and never used.
const JUMP_LABEL_ALPHABET: &[char] =
    &['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l', 'q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p'];

/// Character starting at cell column `col` in an indexed search row, or
/// `None` if `col` is past the row's content (e.g. the match ends at the
/// last non-space cell). Used to build the jump-label exclusion set —
/// the char that would *continue* a match, which must not double as a
/// label key. `byte_to_col` is monotonic, so the first byte mapping to
/// `col` is that cell's lead byte.
fn char_at_col(row: &search::IndexedRow, col: u16) -> Option<char> {
    let b = row.byte_to_col.iter().position(|&c| c == col)?;
    if b < row.text.len() && row.text.is_char_boundary(b) {
        row.text[b..].chars().next()
    } else {
        None
    }
}

/// Assign jump labels to `visible` match indices (in reading order) from
/// [`JUMP_LABEL_ALPHABET`], skipping any char in `excluded` (chars that
/// could continue the query). Stops when the usable alphabet runs out,
/// leaving surplus matches unlabelled. Pure — the I/O-free core of
/// [`App::recompute_labels`], split out so it can be unit-tested.
fn assign_jump_labels(
    visible: &[usize],
    excluded: &std::collections::HashSet<char>,
) -> Vec<(usize, char)> {
    let mut out = Vec::new();
    let mut alpha = JUMP_LABEL_ALPHABET
        .iter()
        .copied()
        .filter(|c| !excluded.contains(c));
    for &idx in visible {
        match alpha.next() {
            Some(ch) => out.push((idx, ch)),
            None => break,
        }
    }
    out
}

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_RADIUS_PX: f64 = 6.0;

/// Encode keyboard modifiers into the bits xterm-style SGR mouse
/// encoding uses (shift=4, alt=8, ctrl=16). Used by both button and
/// motion forwarders. Shift is gated out of forwarding above, but the
/// bit is encoded defensively in case that ever changes.
/// Render the search input bar over the bottom row of the window.
/// One-line strip: `/<query>  M/N  [Aa|aA]` left-aligned, with a
/// trailing `(no matches)` hint when the query is non-empty but no
/// matches exist. The bar overlays whatever was drawn below — the
/// search session is modal, so occluding the bottom row is fine.
fn draw_search_bar<T: femtovg::Renderer>(
    canvas: &mut Canvas<T>,
    tr: &mut renderer::TerminalRenderer,
    search: &SearchState,
    window_w_px: f32,
    window_h_px: f32,
) {
    let bar_h = tr.cell_height;
    let bar_y = window_h_px - bar_h;

    let mut bg_path = femtovg::Path::new();
    bg_path.rect(0.0, bar_y, window_w_px, bar_h);
    canvas.fill_path(&bg_path, &femtovg::Paint::color(tr.search_bar_bg()));

    let case_indicator = if search.case_insensitive { "[aA]" } else { "[Aa]" };
    let counter = if search.matches.is_empty() {
        if search.query.is_empty() {
            String::new()
        } else {
            "(no matches)".to_string()
        }
    } else {
        format!("{}/{}", search.current + 1, search.matches.len())
    };
    let mode_marker = if search.editing { "" } else { " " };
    let text = if counter.is_empty() {
        format!(" /{}{}  {}", search.query, mode_marker, case_indicator)
    } else {
        format!(
            " /{}{}  {}  {}",
            search.query, mode_marker, counter, case_indicator
        )
    };

    let ascent = tr.ascent();
    let text_y = bar_y + ascent + (bar_h - ascent) * 0.5;
    let text_color = tr.search_bar_text();
    tr.draw_vge_text(
        canvas,
        0.0,
        text_y,
        &text,
        text_color,
        vge::command::Align::Left,
        vge::command::FontStyle::default(),
    );
}

/// Draw the flash-style jump labels at the end of each labelled match's
/// word. Runs after the grid/portals so labels sit on top, but before
/// `draw_search_bar` so the bottom bar wins any overlap. Reuses
/// `resolve_portal_target` for the leaf's pixel origin, so labels land
/// correctly inside a vmux pane (portal) and not just the host grid.
/// Label positions come from `search.labels`, which was computed this
/// same frame in `recompute_labels`, so what's drawn here matches what
/// the key handler will dispatch.
fn draw_jump_labels<T: femtovg::Renderer, CB: vt100::Callbacks>(
    canvas: &mut Canvas<T>,
    tr: &mut renderer::TerminalRenderer,
    search: &SearchState,
    prt: &prt::PrtEngine,
    parser: &vt100::Parser<CB>,
) {
    if search.labels.is_empty() {
        return;
    }
    let cell_w = tr.cell_width;
    let cell_h = tr.cell_height;
    let path = search.target_path.as_slice();

    // Leaf geometry: pixel origin + the coords to project a match line
    // into a visible row. Host grid is at (0,0); a portal leaf uses the
    // same origin math as its search highlights.
    let (origin_x, origin_y, top, scrollback, rows) = if path.is_empty() {
        let (rows, _) = parser.screen().size();
        (
            0.0_f32,
            0.0_f32,
            prt.top_of_live_screen(),
            parser.screen().scrollback(),
            rows,
        )
    } else {
        let Some(info) = resolve_portal_target(prt, parser, cell_w, cell_h, path) else {
            return;
        };
        let top = info.portal.children.top_of_live_screen();
        let sb = info.portal.vt.screen().scrollback();
        let (rows, _) = info.portal.vt.screen().size();
        (info.origin_x_px, info.origin_y_px, top, sb, rows)
    };
    let viewport_top = top - scrollback as i64;
    let ascent = tr.ascent();

    for label_info in &search.labels {
        let row_i = label_info.anchor_line - viewport_top;
        if row_i < 0 || row_i >= rows as i64 {
            continue;
        }
        let x = origin_x + label_info.anchor_col as f32 * cell_w;
        let y = origin_y + row_i as f32 * cell_h;

        let (bg_color, fg_color) = jump_label_colors(label_info.ch);
        let mut bg = femtovg::Path::new();
        bg.rect(x, y, cell_w, cell_h);
        canvas.fill_path(&bg, &femtovg::Paint::color(bg_color));

        let mut buf = [0u8; 4];
        let label = label_info.ch.encode_utf8(&mut buf);
        // Baseline at `y + ascent` centres the glyph vertically: the cell is
        // exactly `ceil(ascent + descent)` tall, so the font em-box fills it
        // like any other terminal cell (see renderer's `cy + ascent`).
        let text_y = y + ascent;
        tr.draw_vge_text(
            canvas,
            x,
            text_y,
            label,
            fg_color,
            vge::command::Align::Left,
            vge::command::FontStyle::default(),
        );
    }
}

/// Distinct, readable background colour for a jump label with key `ch`,
/// paired with a glyph colour (black or white) chosen for contrast. Hues are
/// spread by the golden angle so different keys stay easy to tell apart. The
/// mapping is a pure function of `ch`, so a given key always renders the same
/// colour (stable across frames and match sets) yet the palette looks random.
fn jump_label_colors(ch: char) -> (Color, Color) {
    let hue = (ch as u32 as f32 * 137.508) % 360.0;
    let (r, g, b) = hsl_to_rgb(hue, 0.85, 0.55);
    // sRGB-weighted luminance picks the higher-contrast glyph colour.
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    let fg = if luma > 140.0 {
        Color::rgb(0, 0, 0)
    } else {
        Color::rgb(255, 255, 255)
    };
    (Color::rgb(r, g, b), fg)
}

/// Convert HSL (hue in degrees, saturation/lightness in `0..=1`) to 8-bit RGB.
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f32| (((v + m) * 255.0).round()).clamp(0.0, 255.0) as u8;
    (to(r1), to(g1), to(b1))
}

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
    fn new(
        proxy: EventLoopProxy<()>,
        config: config::Config,
        entry_command: Option<Vec<String>>,
    ) -> Self {
        let keys = config.key_bindings();
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
            search: None,
            config,
            keys,
            selection_fresh: false,
            entry_command,
        }
    }

    /// Apply `op` to the focused-leaf parser's `Screen`. Walks
    /// `PrtState::focus_chain()` to the deepest portal whose own focus
    /// is `Host`; that portal's vt is the focused leaf. An empty chain
    /// means the host vt100 is the focused leaf. The closure takes
    /// `&mut Screen` (callback-agnostic) so the same helper works for
    /// both host and portal parsers. Returns `None` if state isn't
    /// ready or a chain id no longer resolves.
    fn with_focused_leaf_screen_mut<R>(
        &mut self,
        op: impl FnOnce(&mut vt100::Screen) -> R,
    ) -> Option<R> {
        let path = self.focused_leaf_path();
        self.with_target_leaf_screen_mut(&path, op)
    }

    /// Like [`with_focused_leaf_screen_mut`], but the target leaf is
    /// named explicitly by `path` instead of resolved from current
    /// focus. Used by search sessions that captured the path at open
    /// time (so focus changes during search don't redirect the target).
    fn with_target_leaf_screen_mut<R>(
        &mut self,
        path: &[String],
        op: impl FnOnce(&mut vt100::Screen) -> R,
    ) -> Option<R> {
        if path.is_empty() {
            let parser = self.parser.as_mut()?;
            return Some(op(parser.screen_mut()));
        }
        let prt = self.prt.as_mut()?;
        let portal = resolve_portal_target_mut(prt, path)?;
        Some(op(portal.vt.screen_mut()))
    }

    /// Snapshot the focus chain as an owned `Vec<String>` so callers
    /// can drop the borrow on `self.prt` before doing further mutation.
    fn focused_leaf_path(&self) -> Vec<String> {
        self.prt
            .as_ref()
            .map(|p| p.state.focus_chain().iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }

    /// Rebuild the search text index against the target parser
    /// named by the current `SearchState::target_path`. Returns None
    /// if state is missing or the path no longer resolves; in that
    /// case the caller leaves `cache` as None and `matches` empty.
    fn rebuild_search_index(&mut self) -> Option<search::TextIndex> {
        let path = self.search.as_ref()?.target_path.clone();
        if path.is_empty() {
            let top = self.prt.as_ref()?.top_of_live_screen();
            let parser = self.parser.as_mut()?;
            return Some(search::extract_indexed_text(parser, top));
        }
        let prt = self.prt.as_mut()?;
        let portal = resolve_portal_target_mut(prt, &path)?;
        let top = portal.children.top_of_live_screen();
        Some(search::extract_indexed_text(&mut portal.vt, top))
    }

    /// `top_of_live_screen` of the engine that owns the target leaf
    /// parser at `path`. Host parser: top-level `prt.top_of_live_screen()`.
    /// Portal parser: the *containing* portal's `children` sub-engine —
    /// i.e. `portal.children.top_of_live_screen()`, matching the
    /// convention documented on `extract_text_from_parser`.
    fn target_top_of_live_screen(&self, path: &[String]) -> Option<i64> {
        let prt = self.prt.as_ref()?;
        if path.is_empty() {
            return Some(prt.top_of_live_screen());
        }
        let mut current_set = prt.state.current();
        for id in &path[..path.len() - 1] {
            let portal = current_set.portals.get(id.as_str())?;
            current_set = portal.children.state.current();
        }
        let last = path[path.len() - 1].as_str();
        current_set
            .portals
            .get(last)
            .map(|p| p.children.top_of_live_screen())
    }

    /// Set `search.current` to the first match that falls within the
    /// currently-visible viewport rows. Returns `true` and leaves the
    /// scroll unchanged on success; returns `false` if there is no
    /// active search, no matches, or no match is visible right now.
    fn select_first_visible_match(&mut self) -> bool {
        let (path, is_empty) = match self.search.as_ref() {
            Some(s) => (s.target_path.clone(), s.matches.is_empty()),
            None => return false,
        };
        if is_empty {
            return false;
        }
        let Some(top) = self.target_top_of_live_screen(&path) else { return false };
        let Some((rows, scrollback)) = self
            .with_target_leaf_screen_mut(&path, |s| (s.size().0 as i64, s.scrollback() as i64))
        else {
            return false;
        };
        let viewport_top = top - scrollback;
        let viewport_bot = viewport_top + rows;
        let Some(search) = self.search.as_mut() else { return false };
        for (i, m) in search.matches.iter().enumerate() {
            if m.line >= viewport_top && m.line < viewport_bot {
                search.current = i;
                return true;
            }
        }
        false
    }

    /// Scroll the target parser so the current match is centered in
    /// the viewport. No-op if no search session, no matches, or the
    /// target path no longer resolves.
    fn scroll_to_current_match(&mut self) {
        let Some(search) = self.search.as_ref() else { return };
        if search.matches.is_empty() {
            return;
        }
        let m = search.matches[search.current];
        let path = search.target_path.clone();
        let Some(top) = self.target_top_of_live_screen(&path) else { return };
        let rows = self
            .with_target_leaf_screen_mut(&path, |s| s.size().0 as i64)
            .unwrap_or(0);
        let target = (top - m.line + rows / 2).max(0) as usize;
        self.set_target_scrollback(&path, target);
    }

    /// Set the target leaf parser's scrollback to `offset`. For the
    /// host parser this mutates `screen().set_scrollback` directly; for
    /// a portal parser, the offset is owned by the portal's client
    /// (e.g. vmux's `PaneScroll`), so we emit `EVT_PORTAL_SCROLL_SET`
    /// and let the client round-trip back with a `SetPortalScrollback`.
    /// Same desync-avoidance discipline as the drag-select autoscroll
    /// path; see [`Self::autoscroll_step`].
    fn set_target_scrollback(&mut self, path: &[String], offset: usize) {
        if path.is_empty() {
            if let Some(parser) = &mut self.parser {
                parser.screen_mut().set_scrollback(offset);
            }
            return;
        }
        let offset_u32 = offset.min(u32::MAX as usize) as u32;
        if let (Some(prt), Some(pty)) = (self.prt.as_mut(), self.pty.as_ref()) {
            if prt.emit_scroll_set_for_path(path, offset_u32) {
                prt.flush_pending_events();
                let bytes = prt.take_responses();
                if !bytes.is_empty() {
                    let _ = pty.write_all(&bytes);
                }
            }
        }
    }

    /// Adjust the search-target parser's scrollback by `pages` *pages*
    /// (one page = the parser's visible rows). Positive = back into
    /// history, negative = toward live. No-op when no search session
    /// is active. Routed through [`Self::set_target_scrollback`] so a
    /// portal-target session stays in sync with the portal's client.
    fn search_scroll_by_pages(&mut self, pages: isize) {
        let Some(search) = self.search.as_ref() else { return };
        let path = search.target_path.clone();
        let Some((rows, current)) = self.with_target_leaf_screen_mut(&path, |s| {
            (s.size().0 as usize, s.scrollback())
        }) else {
            return;
        };
        let signed = current as isize + pages * rows as isize;
        let target = signed.max(0) as usize;
        self.set_target_scrollback(&path, target);
    }

    /// Adjust the search-target parser's scrollback by raw `lines`.
    /// Positive = back into history. Used by the wheel handler while
    /// the search bar is open.
    fn search_scroll_by_lines(&mut self, lines: isize) {
        let Some(search) = self.search.as_ref() else { return };
        let path = search.target_path.clone();
        let Some(current) =
            self.with_target_leaf_screen_mut(&path, |s| s.scrollback())
        else {
            return;
        };
        let signed = current as isize + lines;
        let target = signed.max(0) as usize;
        self.set_target_scrollback(&path, target);
    }

    /// Drop the cached search index. Called after a PTY-output tick
    /// so the next `recompute_search_matches` rebuilds against the
    /// updated parser state. Conservative — invalidates even if the
    /// target parser was untouched this tick, since re-extracting
    /// at typical scrollback sizes is sub-ms. No-op when no search
    /// session is active.
    fn invalidate_search_cache(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.cache = None;
        }
        // Recompute eagerly so the match list (and `current`) reflect
        // any new lines without waiting for the next keystroke.
        if self.search.is_some() {
            self.recompute_search_matches();
        }
    }

    /// Recompute `SearchState::matches` against the cached (or freshly
    /// rebuilt) text index. Clamps `current` to the new match count
    /// (preserves navigation position across PTY-driven invalidations).
    /// Cheap when the cache is warm (just runs `memmem` over each
    /// row); rebuilds the cache only on first call after open or
    /// invalidation.
    fn recompute_search_matches(&mut self) {
        if self.search.is_none() {
            return;
        }
        if self.search.as_ref().unwrap().cache.is_none() {
            let idx = self.rebuild_search_index();
            if let Some(search) = self.search.as_mut() {
                search.cache = idx;
            }
        }
        let Some(search) = self.search.as_mut() else { return };
        let matches = match &search.cache {
            Some(idx) => search::find_matches(idx, &search.query, search.case_insensitive),
            None => Vec::new(),
        };
        search.matches = matches;
        if search.matches.is_empty() {
            search.current = 0;
        } else if search.current >= search.matches.len() {
            search.current = search.matches.len() - 1;
        }
    }

    /// Recompute the flash-style jump labels for the currently-visible
    /// matches of the target leaf. Each visible match (in reading order)
    /// gets a char from [`JUMP_LABEL_ALPHABET`], skipping any char that
    /// could *continue* the query — the cell right after a match — so a
    /// label keypress can never be mistaken for query input. Off-screen
    /// matches get no label (still reachable via `n`/`N`); if visible
    /// matches outnumber the usable alphabet the leftmost ones win and
    /// the rest stay highlighted-but-unlabelled.
    ///
    /// Called once per frame at the top of `RedrawRequested`, so the
    /// stored labels always equal what was last drawn — which is exactly
    /// what the user sees when they press a label key.
    fn recompute_labels(&mut self) {
        // Incremental-expansion mode: a single committed label sits just
        // past the *next* word — the one the next press would add — so it
        // visibly marches forward as the selection grows. Dropped once no
        // word remains (whole line selected), the terminal state.
        if let Some(exp) = self.search.as_ref().and_then(|s| s.expand.clone()) {
            let mut labels = Vec::new();
            if let Some((next_line, next_col)) = self.next_word_end(&exp.target, exp.head) {
                let path = match &exp.target {
                    SelectionTarget::Host => Vec::new(),
                    SelectionTarget::Portal(p) => p.clone(),
                };
                let cols =
                    self.with_target_leaf_screen_mut(&path, |s| s.size().1).unwrap_or(0);
                let anchor_col = if next_col + 1 < cols { next_col + 1 } else { next_col };
                labels.push(JumpLabel {
                    match_idx: 0,
                    ch: exp.ch,
                    anchor_line: next_line,
                    anchor_col,
                });
            }
            if let Some(s) = self.search.as_mut() {
                s.labels = labels;
            }
            return;
        }
        let (path, has_matches) = match self.search.as_ref() {
            Some(s) => (s.target_path.clone(), !s.matches.is_empty()),
            None => return,
        };
        let clear = |app: &mut Self| {
            if let Some(s) = app.search.as_mut() {
                s.labels.clear();
            }
        };
        if !has_matches {
            clear(self);
            return;
        }
        let Some(top) = self.target_top_of_live_screen(&path) else {
            clear(self);
            return;
        };
        let Some((rows, cols, scrollback)) = self
            .with_target_leaf_screen_mut(&path, |s| (s.size().0, s.size().1, s.scrollback()))
        else {
            clear(self);
            return;
        };
        let viewport_top = top - scrollback as i64;

        let assigned = {
            let Some(search) = self.search.as_ref() else { return };
            // line -> cache row, for O(1) exclusion-char lookup.
            let mut line_row: std::collections::HashMap<i64, usize> =
                std::collections::HashMap::new();
            if let Some(cache) = &search.cache {
                for (i, r) in cache.rows.iter().enumerate() {
                    line_row.insert(r.line, i);
                }
            }
            let mut excluded: std::collections::HashSet<char> =
                std::collections::HashSet::new();
            let mut visible: Vec<usize> = Vec::new();
            for (idx, m) in search.matches.iter().enumerate() {
                // Build the exclusion set from *every* match in the buffer,
                // not just the on-screen ones. A label char is only genuine
                // if appending it to the query narrows to nothing anywhere —
                // otherwise `would_narrow` in handle_search_key_input (which
                // searches the whole buffer) swallows the keystroke as query
                // input, and the label the user sees does nothing. The
                // continuation char of any match — visible or scrolled off —
                // is exactly such a query-refining key, so exclude it.
                if let Some(cache) = &search.cache
                    && let Some(&ri) = line_row.get(&m.line)
                    && let Some(c) = char_at_col(&cache.rows[ri], m.col_end)
                {
                    let c = if search.case_insensitive {
                        c.to_ascii_lowercase()
                    } else {
                        c
                    };
                    excluded.insert(c);
                }
                let row_i = m.line - viewport_top;
                if row_i < 0 || row_i >= rows as i64 {
                    continue;
                }
                visible.push(idx);
            }
            assign_jump_labels(&visible, &excluded)
        };

        // Anchor each label just past the end of its match's word, so the
        // glyph sits on the trailing whitespace rather than over the text.
        // The word range is the same one the copy action will grab.
        let target = if path.is_empty() {
            SelectionTarget::Host
        } else {
            SelectionTarget::Portal(path)
        };
        let mut jump_labels: Vec<JumpLabel> = Vec::with_capacity(assigned.len());
        for (idx, ch) in assigned {
            let Some(m) = self.search.as_ref().and_then(|s| s.matches.get(idx).copied())
            else {
                continue;
            };
            let (anchor_line, anchor_col) =
                match self.find_word_range(&target, m.line, m.col_start) {
                    Some((_, (e_line, e_col))) if e_col + 1 < cols => (e_line, e_col + 1),
                    Some((_, (e_line, e_col))) => (e_line, e_col),
                    // Match start isn't a word cell (e.g. query was
                    // whitespace) — fall back to the match start.
                    None => (m.line, m.col_start),
                };
            jump_labels.push(JumpLabel { match_idx: idx, ch, anchor_line, anchor_col });
        }
        if let Some(s) = self.search.as_mut() {
            s.labels = jump_labels;
        }
    }

    /// Current scrollback offset of the focused-leaf parser; 0 if no
    /// state is loaded or a chain id no longer resolves.
    fn focused_leaf_scrollback(&self) -> usize {
        let Some(prt) = self.prt.as_ref() else { return 0 };
        let chain = prt.state.focus_chain();
        if chain.is_empty() {
            return self
                .parser
                .as_ref()
                .map(|p| p.screen().scrollback())
                .unwrap_or(0);
        }
        let mut current_set = prt.state.current();
        for id in &chain[..chain.len() - 1] {
            let Some(portal) = current_set.portals.get(*id) else { return 0 };
            current_set = portal.children.state.current();
        }
        current_set
            .portals
            .get(chain[chain.len() - 1])
            .map(|p| p.vt.screen().scrollback())
            .unwrap_or(0)
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

    /// End of the next whitespace token after `from` on `target`, for
    /// incremental selection expansion. Same host/portal dispatch as
    /// [`Self::find_word_range`]; see [`next_word_end_in_parser`].
    fn next_word_end(
        &mut self,
        target: &SelectionTarget,
        from: (i64, u16),
    ) -> Option<(i64, u16)> {
        match target {
            SelectionTarget::Host => {
                let parser = self.parser.as_mut()?;
                let top_live = self.prt.as_ref()?.top_of_live_screen();
                next_word_end_in_parser(parser, top_live, from.0, from.1)
            }
            SelectionTarget::Portal(path) => {
                let prt = self.prt.as_mut()?;
                let portal = resolve_portal_target_mut(prt, path)?;
                let top_live = portal.children.top_of_live_screen();
                next_word_end_in_parser(&mut portal.vt, top_live, from.0, from.1)
            }
        }
    }

    /// First jump-label press for key `ch`: select the word at match
    /// `idx` — the same word a double-click on its first cell would —
    /// then enter incremental-expansion mode instead of closing the
    /// overlay, so pressing `ch` again grows the selection word-by-word
    /// toward end of line (see [`Self::extend_expand`]). Updates PRIMARY
    /// so middle-click paste works, like any selection. The scroll
    /// position is left as-is since the match is already on screen. If
    /// the match doesn't sit on a word (a whitespace query), nothing is
    /// selected and the overlay just closes, as before.
    fn select_word_at_match(&mut self, idx: usize, ch: char) {
        let (m, target) = match self.search.as_ref() {
            Some(s) => {
                let Some(m) = s.matches.get(idx).copied() else { return };
                let target = if s.target_path.is_empty() {
                    SelectionTarget::Host
                } else {
                    SelectionTarget::Portal(s.target_path.clone())
                };
                (m, target)
            }
            None => return,
        };
        let Some(((s_line, s_col), (e_line, e_col))) =
            self.find_word_range(&target, m.line, m.col_start)
        else {
            self.search = None;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        };
        self.selection = Some(Selection {
            target: target.clone(),
            anchor_line: s_line,
            anchor_col: s_col,
            head_line: e_line,
            head_col: e_col,
            dragging: false,
            block_cols: None,
        });
        self.copy_selection_to_clipboard(true);
        self.selection_fresh = true;
        // Enter expansion mode only if a further word remains on the line
        // to grow into; otherwise this word already *is* the whole line,
        // so close the overlay as the plain select-word action did.
        if self.next_word_end(&target, (e_line, e_col)).is_some() {
            if let Some(s) = self.search.as_mut() {
                s.editing = false;
                s.expand = Some(ExpandState {
                    target,
                    ch,
                    head: (e_line, e_col),
                });
            }
        } else {
            self.search = None;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Incremental-expansion step: grow the active selection by one
    /// whitespace token toward end of line. No-op when the head already
    /// covers the last word on the (wrap-following) logical line — in
    /// that case `recompute_labels` also stops drawing the label, which
    /// is the "whole line selected" terminal state.
    fn extend_expand(&mut self) {
        let Some(exp) = self.search.as_ref().and_then(|s| s.expand.clone()) else {
            return;
        };
        if let Some(new_head) = self.next_word_end(&exp.target, exp.head) {
            if let Some(sel) = self.selection.as_mut() {
                sel.head_line = new_head.0;
                sel.head_col = new_head.1;
            }
            if let Some(exp2) = self.search.as_mut().and_then(|s| s.expand.as_mut()) {
                exp2.head = new_head;
            }
            self.copy_selection_to_clipboard(true);
            self.selection_fresh = true;
            // If that was the last word, the whole line is now selected:
            // close the overlay, leaving the final selection in place.
            if self.next_word_end(&exp.target, new_head).is_none() {
                self.search = None;
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
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

    /// Apply one auto-scroll tick. For a host-target selection the host
    /// parser's scrollback bumps directly. For a portal-target selection
    /// we *cannot* mutate the portal's vt100 ourselves — the portal's
    /// scrollback offset is owned by its client (e.g. vmux's
    /// `PaneScroll`), and a unilateral mutation here would silently
    /// desync the client's view. Instead we emit
    /// `EVT_PORTAL_SCROLL_DELTA` so the client applies the delta and
    /// rounds back to us with a `SetPortalScrollback`. The selection
    /// head re-resolves on the next PTY drain (see `user_event`), once
    /// the client's response has been applied.
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
                self.update_selection_head();
            }
            Some(SelectionTarget::Portal(path)) => {
                // direction == +1 (cursor below viewport) ⇒ scroll
                // *toward live*, i.e. negative delta. Symmetric for the
                // top edge.
                let delta = -direction;
                if let (Some(prt), Some(pty)) = (self.prt.as_mut(), self.pty.as_ref()) {
                    if prt.emit_scroll_delta_for_path(&path, delta) {
                        prt.flush_pending_events();
                        let bytes = prt.take_responses();
                        if !bytes.is_empty() {
                            let _ = pty.write_all(&bytes);
                        }
                    }
                }
                // Don't `update_selection_head` here: the portal's
                // scrollback hasn't changed yet (the client owns it).
                // The head will refresh after the client's
                // `SetPortalScrollback` round-trips, in `user_event`.
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn handle_key_input(&mut self, event: &winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }

        // While a search session is open, swallow every key into the
        // search handler — the search bar is modal w.r.t. the focused
        // PTY. Returns regardless of whether the key was meaningful so
        // typing doesn't leak through to the inner program.
        if self.search.is_some() {
            self.handle_search_key_input(event);
            return;
        }

        // Resolve the configured host chord for this key once; each
        // action below keeps its own extra guards (e.g. OpenSearch only
        // fires while scrolled back). Defaults: `/`, Ctrl+Shift+Space,
        // Shift+PageUp/Down, Ctrl+Shift+C/V — see `config.rs`.
        let host_action = self.keys.resolve_host(&event.logical_key, &self.modifiers);

        // Open search while the focused-leaf parser is scrolled back.
        // Checked before the "any key resets scrollback" reset below so
        // opening search preserves the scroll position.
        if host_action == Some(HostAction::OpenSearch) && self.focused_leaf_scrollback() > 0 {
            let path = self.focused_leaf_path();
            let saved = self.focused_leaf_scrollback();
            self.search = Some(SearchState::new(path, saved));
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        // Scrollback paging. Targets the focused-leaf parser so scrolling
        // inside a vmux pane navigates that pane's scrollback rather than
        // the host vt100 (which inside vmux holds only chrome paint —
        // pane bytes are routed through PRT into per-portal vt100s).
        // Routed through `set_target_scrollback` so a portal leaf stays
        // in sync with its client's stored offset.
        if host_action == Some(HostAction::ScrollPageUp) {
            let path = self.focused_leaf_path();
            if let Some((rows, current)) = self
                .with_target_leaf_screen_mut(&path, |s| (s.size().0 as usize, s.scrollback()))
            {
                self.set_target_scrollback(&path, current + rows);
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }
        if host_action == Some(HostAction::ScrollPageDown) {
            let path = self.focused_leaf_path();
            if let Some((rows, current)) = self
                .with_target_leaf_screen_mut(&path, |s| (s.size().0 as usize, s.scrollback()))
            {
                self.set_target_scrollback(&path, current.saturating_sub(rows));
            }
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        // Open the unified scrollback/search/select overlay at the
        // current bottom *without* scrolling — the keyboard entry point.
        // Works from live output, unlike OpenSearch which requires being
        // scrolled back first. Checked before the any-key-resets-scrollback
        // block below so opening while already scrolled back preserves the
        // position.
        if host_action == Some(HostAction::OpenOverlay) {
            let path = self.focused_leaf_path();
            let saved = self.focused_leaf_scrollback();
            self.search = Some(SearchState::new(path, saved));
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        // Custom selection commands (`[[selection_commands]]`). While a
        // *fresh* selection exists, a bound key runs the user's shell
        // command on the selected text instead of reaching the PTY.
        // Fresh = made since the last key sent to the shell, so a
        // lingering highlight never hijacks normal typing. Checked after
        // host chords (which win on conflict) and before the key reaches
        // the shell. Running consumes the selection, like copy.
        if self.selection_fresh
            && self.selection.is_some()
            && let Some(command) =
                self.keys.resolve_selection_command(&event.logical_key, &self.modifiers)
        {
            if let Some(text) = self.extract_selection_text() {
                let sel = text.trim();
                if !sel.is_empty() {
                    run_selection_command(&command, sel);
                }
            }
            self.selection = None;
            self.selection_fresh = false;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
            return;
        }

        // Any non-scroll key resets scrollback to bottom on the *host*
        // leaf (a plain `veter` shell with no multiplexer), so typing
        // jumps back to live. Skipped when already at live to avoid a
        // no-op every keystroke. Modifier-only presses (Super/Win for
        // KDE virtual-desktop switching, lone Shift/Ctrl/Alt while
        // reaching for a combo) are excluded — they produce no PTY
        // input, so dropping scroll mode on them would be surprising.
        //
        // Portal leaves are deliberately left alone: the owning client
        // (e.g. vmux) owns its scroll mode and decides when to exit it
        // — it keeps the pane scrolled across the prefix key (so tab
        // switches etc. work mid-scroll) and exits only on its own
        // keys (q/Esc/G). Force-resetting here would yank scroll out
        // from under the client and defeat that, so we don't.
        let is_modifier_only = matches!(
            &event.logical_key,
            Key::Named(
                NamedKey::Shift
                    | NamedKey::Control
                    | NamedKey::Alt
                    | NamedKey::AltGraph
                    | NamedKey::Super
                    | NamedKey::Meta
                    | NamedKey::Hyper
                    | NamedKey::Fn
                    | NamedKey::FnLock
                    | NamedKey::CapsLock
                    | NamedKey::NumLock
                    | NamedKey::ScrollLock
                    | NamedKey::Symbol
                    | NamedKey::SymbolLock
            )
        );
        // Copy copies / clears the selection but sends nothing to the
        // PTY, so — like modifier-only presses — it must not drop scroll
        // mode: the user expects to keep reading scrollback after
        // grabbing a snippet.
        let is_copy = host_action == Some(HostAction::Copy);
        // A real keystroke bound for the shell disarms selection commands,
        // so a lingering highlight can't hijack later typing. Modifier-only
        // presses (reaching for a chord) and host actions — which returned
        // above — don't disarm.
        if !is_modifier_only {
            self.selection_fresh = false;
        }
        let path = self.focused_leaf_path();
        if path.is_empty() && !is_modifier_only && !is_copy && self.focused_leaf_scrollback() > 0 {
            self.set_target_scrollback(&path, 0);
        }

        let pty = match &self.pty {
            Some(p) => p,
            None => return,
        };

        // Paste / copy. Handled before the generic Ctrl+letter block so
        // the default Ctrl+Shift+V/C don't get clobbered into ^V/^C.
        if host_action == Some(HostAction::Paste) {
            if let Some(text) = self.clipboard.get_text() {
                let bracketed = self.focused_vt_bracketed_paste();
                let bytes = clipboard::build_paste_bytes(&text, bracketed);
                let _ = pty.write_all(&bytes);
            }
            return;
        }
        if host_action == Some(HostAction::Copy) {
            self.copy_selection_to_clipboard(false);
            // Reset the selection after copying so the highlight clears.
            if self.selection.is_some() {
                self.selection = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            return;
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

    /// Modal key handler for the search bar. Sub-modes:
    ///
    /// * `editing` (just after open, or after a fresh char while
    ///   navigating) — typed chars edit the query, Backspace pops,
    ///   Alt+C toggles case, Enter commits and switches to
    ///   navigation, Esc closes and restores the saved scroll.
    /// * navigating (`!editing`, after the first Enter) — n/N step
    ///   the current match, any printable other than n/N re-enters
    ///   editing with a fresh query.
    ///
    /// Match computation and scroll-to-match are wired in steps 2/4
    /// of the search work; this scaffold only owns the state
    /// transitions and redraw triggers.
    fn handle_search_key_input(&mut self, event: &winit::event::KeyEvent) {
        if self.search.is_none() {
            return;
        }
        // Resolve the configured in-search chord once, before borrowing
        // `search` mutably. Close and paging are safe to dispatch up front
        // — their chords don't collide with query typing, jump labels, or
        // expand mode. NextMatch/PrevMatch/ToggleCase are handled at their
        // proper precedence inside the character arm below.
        let search_action = self.keys.resolve_search(&event.logical_key, &self.modifiers);
        match search_action {
            Some(SearchAction::Close) => {
                let (path, saved, was_editing) = {
                    let search = self.search.as_ref().unwrap();
                    (
                        search.target_path.clone(),
                        search.saved_scrollback,
                        search.editing,
                    )
                };
                self.search = None;
                if was_editing {
                    self.set_target_scrollback(&path, saved);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                return;
            }
            Some(SearchAction::PageUp) => {
                self.search_scroll_by_pages(1);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                return;
            }
            Some(SearchAction::PageDown) => {
                self.search_scroll_by_pages(-1);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                return;
            }
            _ => {}
        }

        let Some(search) = self.search.as_mut() else { return };

        match &event.logical_key {
            Key::Named(NamedKey::Enter) => {
                // In expansion mode Enter commits: close the overlay,
                // leaving the grown selection in place.
                if search.expand.is_some() {
                    self.search = None;
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                if search.editing {
                    search.editing = false;
                } else if !search.matches.is_empty() {
                    search.current = (search.current + 1) % search.matches.len();
                }
                self.scroll_to_current_match();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if search.editing {
                    search.query.pop();
                    search.current = 0;
                    self.recompute_search_matches();
                    self.scroll_to_current_match();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            Key::Character(c) => {
                let s = c.as_str();
                // Incremental-expansion mode: the committed label key
                // grows the selection one word at a time toward EOL; any
                // other plain key ends the mode and closes the overlay,
                // leaving the selection. Intercepted before the generic
                // label/query logic below (whose label lookup would
                // otherwise re-select the original word).
                if !self.modifiers.alt_key()
                    && !self.modifiers.control_key()
                    && s.chars().count() == 1
                    && let Some(ch) = s.chars().next()
                    && let Some(exp_ch) = search.expand.as_ref().map(|e| e.ch)
                {
                    if ch == exp_ch {
                        self.extend_expand();
                    } else {
                        self.search = None;
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
                // Flash jump label vs. query input. A label only fires
                // when typing it would *not* keep narrowing the search —
                // i.e. appending it to the query matches nothing (or
                // we're in navigation mode, where there's no query to
                // extend). This guarantees a keystroke that still refines
                // the query always refines it: typing "Pi" to reach
                // "Pictures" is never hijacked even if "i" labels some
                // other match. (The exclusion set in `recompute_labels`
                // already keeps most label chars off continuation keys;
                // this rule makes it robust against any it misses.)
                if !self.modifiers.alt_key()
                    && !self.modifiers.control_key()
                    && let Some(ch) = s.chars().next()
                    && s.chars().count() == 1
                    && let Some(match_idx) =
                        search.labels.iter().find(|l| l.ch == ch).map(|l| l.match_idx)
                {
                    let would_narrow = search.editing
                        && match &search.cache {
                            Some(cache) => {
                                let mut q = search.query.clone();
                                q.push(ch);
                                !search::find_matches(cache, &q, search.case_insensitive)
                                    .is_empty()
                            }
                            None => false,
                        };
                    if !would_narrow {
                        self.select_word_at_match(match_idx, ch);
                        return;
                    }
                }
                if search_action == Some(SearchAction::ToggleCase) {
                    search.case_insensitive = !search.case_insensitive;
                    search.current = 0;
                    self.recompute_search_matches();
                    self.scroll_to_current_match();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                // Swallow any other Alt combo so it doesn't leak into the
                // query (matches the pre-config behavior where only Alt+C
                // did anything and every other Alt press was ignored).
                if self.modifiers.alt_key() {
                    return;
                }
                if !search.editing {
                    if search_action == Some(SearchAction::NextMatch) {
                        if !search.matches.is_empty() {
                            search.current = (search.current + 1) % search.matches.len();
                        }
                        self.scroll_to_current_match();
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                        return;
                    }
                    if search_action == Some(SearchAction::PrevMatch) {
                        if !search.matches.is_empty() {
                            let n = search.matches.len();
                            search.current = (search.current + n - 1) % n;
                        }
                        self.scroll_to_current_match();
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                        return;
                    }
                    // Any other key starts a fresh query.
                    search.editing = true;
                    search.query.clear();
                }
                search.query.push_str(s);
                search.current = 0;
                self.recompute_search_matches();
                if !self.select_first_visible_match() {
                    self.scroll_to_current_match();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
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
                    // used when vsd runs directly under a veter
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
        let mut term_renderer = renderer::TerminalRenderer::new(&mut canvas, font_size);
        // Apply configured search-chrome colors (bar background falls
        // back to accent slot 0 when unset).
        term_renderer.set_search_colors(
            self.config.search_bar_bg().to_femto(),
            self.config.search.bar_text.to_femto(),
            self.config.search.current_match.to_femto(),
            self.config.search.match_color.to_femto(),
        );
        let (term_cols, term_rows) = term_renderer.terminal_size(size.width, size.height);

        // VGE engine: needs cell pixel dimensions and HiDPI scale factor.
        let cell_px = (
            term_renderer.cell_width.round() as u16,
            term_renderer.cell_height.round() as u16,
        );
        let scale = window.scale_factor() as f32;
        let mut vge_engine = vge::VgeEngine::new(cell_px, scale);
        // §7.3 — publish the host accent palette into the top-level VGE
        // engine's reserved `host.*` namespace (depth 0). vmux and other
        // clients reference `host.accent` instead of hardcoding colors;
        // per-portal engines get their own depth-keyed copy on creation.
        vge_engine.seed_host_styles(host_accent_palette(&self.config), 0);
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

        let mut prt_engine =
            prt::PrtEngine::with_metrics_and_wakeup(cell_px, scale, vft_wakeup);
        // Same palette as the top-level VGE engine, inherited by every
        // per-portal VGE engine PRT spawns; each portal keys its
        // contextual `host.accent` on its own nesting depth.
        prt_engine.set_host_palette(host_accent_palette(&self.config));

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
        let pty = pty::Pty::new(term_rows, term_cols, self.entry_command.clone())
            .expect("Failed to create PTY");

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
                        // A vertical resize moves the live screen
                        // relative to scrollback (xterm-style push/
                        // pull); sync the engines' line trackers now
                        // rather than waiting for the next PTY chunk,
                        // or anchored elements/portals render shifted
                        // until the shell redraws.
                        if let Some(prt) = &mut self.prt {
                            prt.after_vt100_process(parser);
                        }
                        if let Some(vge) = &mut self.vge {
                            vge.after_vt100_process(parser);
                        }
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
                            self.selection_fresh = true;
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
                            // A just-finished selection arms selection commands.
                            self.selection_fresh = true;
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
                // Search is modal: while the bar is open, the wheel
                // navigates the search-target's scrollback directly
                // (one tick = 3 lines, mirroring the standard host
                // wheel-scroll step). Never forwards to the inner
                // program — keeps the search session in control of the
                // viewport so the n/N highlight follows what the user
                // wheels to.
                if self.search.is_some() {
                    let cell_h = self
                        .term_renderer
                        .as_ref()
                        .map(|t| t.cell_height)
                        .unwrap_or(20.0);
                    let lines = match delta {
                        winit::event::MouseScrollDelta::LineDelta(_, y) => (y * 3.0) as isize,
                        winit::event::MouseScrollDelta::PixelDelta(pos) => {
                            (pos.y as f32 / cell_h) as isize
                        }
                    };
                    if lines != 0 {
                        self.search_scroll_by_lines(lines);
                        if let Some(w) = &self.window {
                            w.request_redraw();
                        }
                    }
                    return;
                }
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
                    self.with_focused_leaf_screen_mut(|screen| {
                        let current = screen.scrollback() as isize;
                        screen.set_scrollback((current + lines).max(0) as usize);
                    });
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
                // Refresh jump labels to match this frame's scroll/match
                // state before drawing. Doing it here (rather than in each
                // key/scroll handler) guarantees the stored `labels` equal
                // what gets drawn, so a label keypress always selects the
                // match the user is looking at. No-op when not searching.
                self.recompute_labels();

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
                    let search_ctx =
                        self.search.as_ref().map(|s| prt::render::PortalSearchCtx {
                            remaining_path: s.target_path.as_slice(),
                            matches: s.matches.as_slice(),
                            current: s.current,
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
                        search_ctx.as_ref(),
                    );

                    // Jump labels, then the search input bar — both drawn
                    // last so they overlay the grid/portals/VGE chrome.
                    // Labels first so the bottom bar wins on the last row.
                    if let Some(search) = self.search.as_ref() {
                        draw_jump_labels(canvas, tr, search, prt, parser);
                        draw_search_bar(canvas, tr, search, size.width as f32, size.height as f32);
                    }
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
        // PTY output may have advanced the target parser's scrollback;
        // rebuild the search index so live matches stay accurate.
        self.invalidate_search_cache();
        // A drag-select autoscroll on a portal target emits
        // `EVT_PORTAL_SCROLL_DELTA`; the client (e.g. vmux) round-trips
        // back with `SetPortalScrollback`, which we just applied. Refresh
        // the head so the highlight follows the content under the cursor.
        // No-op when no drag is active.
        self.update_selection_head();
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
    let entry_command = parse_entry_command();
    let config = config::Config::load();
    let event_loop = EventLoop::new().unwrap();
    let proxy = event_loop.create_proxy();
    let mut app = App::new(proxy, config, entry_command);
    event_loop.run_app(&mut app).unwrap();
}

/// Parse a `veter -e <command> [args…]` entry-point command. Everything
/// after `-e` (or `--command`) is the program + args to run instead of
/// the default vmux/`$SHELL`. Returns `None` when the flag is absent (or
/// has no command after it), leaving the default behaviour.
fn parse_entry_command() -> Option<Vec<String>> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "-e" || arg == "--command" {
            let rest: Vec<String> = args.collect();
            return (!rest.is_empty()).then_some(rest);
        }
    }
    None
}

/// Diagnostic-only trace of keyboard bytes about to be written to the
/// inner PTY. Enable with `VETER_DEBUG_INPUT=1`; output goes to
/// `/tmp/veter-input.log` with the same hexdump format as vsd's
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

#[cfg(test)]
mod jump_label_tests {
    use super::*;
    use std::collections::HashSet;

    fn parse(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, cols, 100);
        p.process(bytes);
        p
    }

    #[test]
    fn char_at_col_reads_next_cell() {
        // "foo bar" — the char right after the match "foo" (col_end 3)
        // is a space; after "bar" (col_end 7) the row ends -> None.
        let mut p = parse(b"foo bar", 1, 20);
        let idx = search::extract_indexed_text(&mut p, 0);
        let row = &idx.rows[0];
        assert_eq!(char_at_col(row, 0), Some('f'));
        assert_eq!(char_at_col(row, 3), Some(' '));
        assert_eq!(char_at_col(row, 4), Some('b'));
        // Past the trailing-stripped content -> no continuation char.
        assert_eq!(char_at_col(row, 7), None);
    }

    #[test]
    fn char_at_col_handles_wide_chars() {
        // "あb": あ is wide (cols 0..2), b at col 2. col 1 is the wide
        // continuation and has no lead byte -> None; col 2 is 'b'.
        let mut p = parse("あb".as_bytes(), 1, 10);
        let idx = search::extract_indexed_text(&mut p, 0);
        let row = &idx.rows[0];
        assert_eq!(char_at_col(row, 0), Some('あ'));
        assert_eq!(char_at_col(row, 1), None);
        assert_eq!(char_at_col(row, 2), Some('b'));
    }

    #[test]
    fn next_word_end_steps_one_token() {
        // "ls $HOME": from the end of "ls" (col 1) the next word is
        // "$HOME", ending on 'E' at col 7. Stepping again finds only
        // trailing space -> None (whole line covered).
        let mut p = parse(b"ls $HOME", 1, 20);
        assert_eq!(next_word_end_in_parser(&mut p, 0, 0, 1), Some((0, 7)));
        assert_eq!(next_word_end_in_parser(&mut p, 0, 0, 7), None);
    }

    #[test]
    fn next_word_end_skips_runs_of_whitespace() {
        // Multiple spaces between tokens are skipped; the end lands on
        // the last cell of the following token.
        let mut p = parse(b"a   bb", 1, 20);
        assert_eq!(next_word_end_in_parser(&mut p, 0, 0, 0), Some((0, 5)));
        assert_eq!(next_word_end_in_parser(&mut p, 0, 0, 5), None);
    }

    #[test]
    fn next_word_end_follows_soft_wrap() {
        // cols=4: "abcd" fills row 0 (wrapped), " ef" continues on row
        // 1. From the end of "abcd" (0,3) the next word "ef" ends at
        // (1,2), proving the walk follows the wrap into the next row.
        let mut p = parse(b"abcd ef", 2, 4);
        assert!(p.screen().row_wrapped(0));
        assert_eq!(next_word_end_in_parser(&mut p, 0, 0, 3), Some((1, 2)));
        assert_eq!(next_word_end_in_parser(&mut p, 0, 1, 2), None);
    }

    #[test]
    fn labels_assigned_in_order() {
        let labels = assign_jump_labels(&[0, 1, 2], &HashSet::new());
        let chars: Vec<char> = labels.iter().map(|(_, c)| *c).collect();
        let idxs: Vec<usize> = labels.iter().map(|(i, _)| *i).collect();
        assert_eq!(idxs, vec![0, 1, 2]);
        // First three of the home-row alphabet.
        assert_eq!(chars, vec!['a', 's', 'd']);
    }

    #[test]
    fn excluded_chars_are_skipped() {
        // Exclude 'a' and 'd' (chars that could continue a match): the
        // assignment must not hand them out, falling through to 's', 'f'.
        let excluded: HashSet<char> = ['a', 'd'].into_iter().collect();
        let labels = assign_jump_labels(&[10, 11], &excluded);
        let chars: Vec<char> = labels.iter().map(|(_, c)| *c).collect();
        assert_eq!(chars, vec!['s', 'f']);
        // The original match indices are preserved.
        assert_eq!(labels[0].0, 10);
        assert_eq!(labels[1].0, 11);
    }

    /// Replicate `recompute_labels`' exclusion + assignment for a
    /// realistic `ls` screen, querying "p". The cell after the "P" in
    /// "Pictures" is "i", so "i" must be excluded and never handed out
    /// as a label — otherwise typing "Pi" to narrow would be hijacked.
    #[test]
    fn continuation_char_excluded_from_labels() {
        let mut p = parse(b"Desktop Documents Pictures Public Templates Videos", 1, 60);
        let idx = search::extract_indexed_text(&mut p, 0);
        let matches = search::find_matches(&idx, "p", true);
        assert!(!matches.is_empty());

        let mut line_row = std::collections::HashMap::new();
        for (i, r) in idx.rows.iter().enumerate() {
            line_row.insert(r.line, i);
        }
        let mut excluded = HashSet::new();
        let mut visible = Vec::new();
        for (i, m) in matches.iter().enumerate() {
            visible.push(i);
            if let Some(&ri) = line_row.get(&m.line)
                && let Some(c) = char_at_col(&idx.rows[ri], m.col_end)
            {
                excluded.insert(c.to_ascii_lowercase());
            }
        }
        assert!(excluded.contains(&'i'), "expected 'i' excluded, got {excluded:?}");
        let labels = assign_jump_labels(&visible, &excluded);
        assert!(
            labels.iter().all(|(_, c)| *c != 'i'),
            "no label should be 'i': {labels:?}"
        );
    }

    #[test]
    fn surplus_matches_left_unlabelled() {
        // More visible matches than usable alphabet -> the leftmost are
        // labelled, the rest are dropped (still nav-reachable via n/N).
        let n = JUMP_LABEL_ALPHABET.len();
        let visible: Vec<usize> = (0..n + 5).collect();
        let labels = assign_jump_labels(&visible, &HashSet::new());
        assert_eq!(labels.len(), n);
        assert_eq!(labels.last().unwrap().0, n - 1);
    }
}
