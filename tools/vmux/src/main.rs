//! vmux — terminal multiplexer over the Portal Extension (PRT) plus
//! Vector Graphics Extension (VGE) for chrome.
//!
//! Run inside a veter session that advertises both extensions. Default
//! prefix key is **Ctrl+Space**. After pressing it once:
//!
//!   v  split the focused pane vertically (new pane to the right)
//!   h  split horizontally (new pane below)
//!   x  close the focused pane
//!   r  rename the focused pane (modal edit window)
//!   o  cycle focus to the next pane
//!   q  quit vmux (asks for confirmation first)
//!
//! When the last pane closes, vmux exits.
//!
//! Mouse reporting is on: a left click focuses the pane under the
//! pointer or, on the top row, switches to the clicked tab; the wheel
//! scrolls the pane it is over.
//!
//! Each pane is backed by a host PRT portal that receives the inner
//! shell's output. The pane's outline (rounded rect) and title strip are
//! drawn with VGE elements. Keystrokes go to the focused pane's PTY
//! master directly — no input crosses the PRT wire (§9.1).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::unistd::{execvp, Pid};

use prt_protocol::apc::ApcStream as PrtApcStream;
use prt_protocol::codec::Reader as PrtReader;
use prt_protocol::command::{
    AnchorMode, Command as PrtCommand, CreatePortalBody, FocusTarget, UpdateOriginBody,
    WritePortalBody,
};
use prt_protocol::encode::build_envelope as build_prt_envelope;
use prt_protocol::frame::{
    EVT_ICON_NAME_CHANGE, EVT_MOUSE_MODE_CHANGE, EVT_PORTAL_ACTIVITY, EVT_PORTAL_SCROLL_DELTA,
    EVT_PORTAL_SCROLL_SET, EVT_RAW_REPLY, EVT_TITLE_CHANGE, FEAT_VGE_HOST_THEMED_STYLES,
    MARKER_T2C as PRT_MARKER_T2C, RSP_PROBE as PRT_RSP_PROBE,
};

use ses_protocol::apc::ApcStream as SesApcStream;
use ses_protocol::frame::MARKER_H2C as SES_MARKER_H2C;
use ses_protocol::{
    Command as SesCommand, HostFrame as SesHostFrame, encode_command as build_ses_envelope,
    for_each_frame as ses_for_each_frame,
};

use vge_protocol::apc::ApcStream as VgeApcStream;
use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Align, Color, Command as VgeCommand, CreateElementBody, DrawCmd, FontStyle, Style,
};
use vge_protocol::encode::build_envelope as build_vge_envelope;
use vge_protocol::frame::{MARKER_T2C as VGE_MARKER_T2C, RSP_PROBE as VGE_RSP_PROBE};
use vge_protocol::path::{PathNode, PathSegment};

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Shell to spawn for each pane. Falls back to `/bin/sh` if `$SHELL` is
/// unset — same convention tmux uses.
fn user_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

// ─────────────────────────────────────────────────────────────────────────
// Debug logging — VMUX_LOG=/path enables byte-level tracing of all four
// directions: user keystrokes, bytes forwarded to a pane PTY, bytes read
// from a pane PTY, and the WritePortal payloads we send to the host.
// ─────────────────────────────────────────────────────────────────────────

static DEBUG_LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn init_debug_log() {
    if let Ok(path) = std::env::var("VMUX_LOG") {
        if let Ok(f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            *DEBUG_LOG.lock().unwrap() = Some(f);
        }
    }
}

fn dlog(tag: &str, bytes: &[u8]) {
    let mut guard = match DEBUG_LOG.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let Some(f) = guard.as_mut() else { return };
    let mut line = format!("[{tag}] {} bytes: ", bytes.len());
    for &b in bytes {
        match b {
            0x1B => line.push_str("\\e"),
            b'\r' => line.push_str("\\r"),
            b'\n' => line.push_str("\\n"),
            b'\t' => line.push_str("\\t"),
            0x20..=0x7E => line.push(b as char),
            _ => line.push_str(&format!("\\x{b:02x}")),
        }
    }
    line.push('\n');
    let _ = f.write_all(line.as_bytes());
    let _ = f.flush();
}

// ─────────────────────────────────────────────────────────────────────────
// Layout tree
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitDir {
    /// Vertical split — children stacked horizontally, `a` left, `b` right.
    Vertical,
    /// Horizontal split — children stacked vertically, `a` top, `b` bottom.
    Horizontal,
}

#[derive(Debug)]
enum Layout {
    Leaf(String),
    Split {
        dir: SplitDir,
        ratio: f32,
        a: Box<Layout>,
        b: Box<Layout>,
    },
}

impl Layout {
    /// Replace the leaf matching `target_id` with a split that contains
    /// the original leaf and a freshly-created one carrying `new_id`.
    /// Returns true if the target was found.
    fn split_leaf(&mut self, target_id: &str, dir: SplitDir, new_id: &str) -> bool {
        match self {
            Layout::Leaf(id) if id == target_id => {
                let old = std::mem::replace(self, Layout::Leaf(String::new()));
                let Layout::Leaf(orig) = old else {
                    unreachable!()
                };
                *self = Layout::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Layout::Leaf(orig)),
                    b: Box::new(Layout::Leaf(new_id.to_string())),
                };
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { a, b, .. } => {
                a.split_leaf(target_id, dir, new_id) || b.split_leaf(target_id, dir, new_id)
            }
        }
    }

    /// Remove the leaf matching `target_id`. If the leaf is at the root,
    /// returns `RemoveResult::Empty`. Otherwise the parent split is
    /// collapsed to its surviving child. Returns the surviving sibling's
    /// representative pane id (caller uses it to retarget focus).
    fn remove_leaf(&mut self, target_id: &str) -> RemoveResult {
        // Two-phase: detect-and-mutate at this level, otherwise recurse.
        let (new_root, surviving) = match self {
            Layout::Leaf(id) if id == target_id => return RemoveResult::Empty,
            Layout::Leaf(_) => return RemoveResult::NotFound,
            Layout::Split { a, b, .. } => {
                if matches!(a.as_ref(), Layout::Leaf(id) if id == target_id) {
                    let take = std::mem::replace(b.as_mut(), Layout::Leaf(String::new()));
                    let surv = take.first_leaf().to_string();
                    (take, surv)
                } else if matches!(b.as_ref(), Layout::Leaf(id) if id == target_id) {
                    let take = std::mem::replace(a.as_mut(), Layout::Leaf(String::new()));
                    let surv = take.first_leaf().to_string();
                    (take, surv)
                } else {
                    let r = a.remove_leaf(target_id);
                    if !matches!(r, RemoveResult::NotFound) {
                        return r;
                    }
                    return b.remove_leaf(target_id);
                }
            }
        };
        *self = new_root;
        RemoveResult::Removed {
            new_focus: surviving,
        }
    }

    fn first_leaf(&self) -> &str {
        match self {
            Layout::Leaf(id) => id,
            Layout::Split { a, .. } => a.first_leaf(),
        }
    }

    fn collect_leaves(&self, out: &mut Vec<String>) {
        match self {
            Layout::Leaf(id) => out.push(id.clone()),
            Layout::Split { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }

    /// True if `target` is one of the leaves under this node.
    fn contains_leaf(&self, target: &str) -> bool {
        match self {
            Layout::Leaf(id) => id == target,
            Layout::Split { a, b, .. } => {
                a.contains_leaf(target) || b.contains_leaf(target)
            }
        }
    }

    /// Mutable ratio of the split reached by following `path` (true = `a`,
    /// false = `b`) from this node. An empty path targets this node
    /// itself. `None` if the path runs off a leaf — used by mouse-drag
    /// resize to re-find the dragged divider each motion event.
    fn ratio_at_path_mut(&mut self, path: &[bool]) -> Option<&mut f32> {
        match self {
            Layout::Leaf(_) => None,
            Layout::Split { ratio, a, b, .. } => match path.split_first() {
                None => Some(ratio),
                Some((true, rest)) => a.ratio_at_path_mut(rest),
                Some((false, rest)) => b.ratio_at_path_mut(rest),
            },
        }
    }
}

enum RemoveResult {
    Removed { new_focus: String },
    Empty,
    NotFound,
}

#[derive(Debug, Clone, Copy)]
struct PaneRect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// Walk the layout tree and lay every leaf out within `bounds`.
/// Inner grid of a portal placed at `rect`. The portal fills the entire
/// rect — separators sit on the cell edges *between* panes, not inside
/// them, so we don't reserve a chrome border. The title text overlays
/// the bottom-right portal cell rather than reserving a row. Returns
/// `(rows, cols)`.
fn inner_grid_for(rect: PaneRect) -> (u32, u32) {
    (rect.h.max(1), rect.w.max(1))
}

/// Inner-portal origin within the host grid for `rect`.
fn inner_origin_for(rect: PaneRect) -> (i32, i32) {
    (rect.x, rect.y)
}

/// A thin line drawn between two adjacent panes. Coordinates are in
/// host-cell units; the line lies on a cell edge.
#[derive(Debug, Clone, Copy)]
enum Separator {
    /// Vertical line at `x`, spanning `y0..y1`.
    Vertical { x: f32, y0: f32, y1: f32 },
    /// Horizontal line at `y`, spanning `x0..x1`.
    Horizontal { y: f32, x0: f32, x1: f32 },
}

/// Walk the layout tree and emit one separator at each split boundary.
/// Each separator stops 1 cell short of the split's bounds on both ends
/// so adjacent separators (in nested splits) and pane corners get a
/// little visual breathing room.
fn collect_separators(node: &Layout, bounds: PaneRect, out: &mut Vec<Separator>) {
    if let Layout::Split { dir, ratio, a, b } = node {
        let (rect_a, rect_b) = split_child_rects(*dir, *ratio, bounds);
        match dir {
            SplitDir::Vertical => {
                let y0 = bounds.y as f32;
                let y1 = bounds.y as f32 + bounds.h as f32;
                if y1 > y0 {
                    out.push(Separator::Vertical {
                        x: rect_b.x as f32,
                        y0,
                        y1,
                    });
                }
            }
            SplitDir::Horizontal => {
                let x0 = bounds.x as f32;
                let x1 = bounds.x as f32 + bounds.w as f32;
                if x1 > x0 {
                    out.push(Separator::Horizontal {
                        y: rect_b.y as f32,
                        x0,
                        x1,
                    });
                }
            }
        }
        collect_separators(a, rect_a, out);
        collect_separators(b, rect_b, out);
    }
}

/// Build the VGE element body that draws between-pane separators for
/// the active tab's layout. With a single pane, `commands` is empty —
/// we still emit the element so visibility tracking is uniform.
fn build_separators_body(layout: &Layout, full: PaneRect) -> CreateElementBody {
    let mut seps = Vec::new();
    collect_separators(layout, full, &mut seps);
    let mut cmds: Vec<DrawCmd> = Vec::new();
    if !seps.is_empty() {
        // veter floors the window pixels into whole cells, so a sub-cell
        // strip is left uncovered along the bottom and right edges of the
        // window. A separator that reaches the layout's bottom/right edge
        // ends at the last cell boundary and so stops a few pixels short of
        // the physical window edge. Overshoot those by one cell: VGE
        // doesn't clip an unsized top-level element, so the surface edge
        // trims the excess and the line runs flush to the window edge.
        let full_bottom = (full.y + full.h as i32) as f32;
        let full_right = (full.x + full.w as i32) as f32;
        const OVERSHOOT: f32 = 1.0;
        let lines: Vec<(Point, Point)> = seps
            .into_iter()
            .map(|s| match s {
                Separator::Vertical { x, y0, y1 } => {
                    let y1 = if y1 >= full_bottom { y1 + OVERSHOOT } else { y1 };
                    (Point { x, y: y0 }, Point { x, y: y1 })
                }
                Separator::Horizontal { y, x0, x1 } => {
                    let x1 = if x1 >= full_right { x1 + OVERSHOOT } else { x1 };
                    (Point { x: x0, y }, Point { x: x1, y })
                }
            })
            .collect();
        cmds.push(DrawCmd::DrawLines {
            stroke: accent_style(),
            line_width: 0.1,
            lines,
        });
    }
    CreateElementBody {
        id: SEPARATORS_ELEMENT_ID.to_string(),
        commands: cmds,
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: CHROME_DRAW_ORDER,
        parent: None,
        size: None,
    }
}

/// Minimum / maximum split ratio. Resizing (keyboard or mouse-drag)
/// clamps to this range so a pane can never be squeezed to zero width /
/// height — there's always at least a sliver of each child left.
const MIN_RATIO: f32 = 0.05;
const MAX_RATIO: f32 = 0.95;

/// Partition `bounds` into the two child rects of a split. Single source
/// of truth for the divider position, shared by layout, separator
/// drawing, and mouse hit-testing so they can never disagree on where a
/// boundary sits. The `a` child takes `ratio` of the split axis (rounded
/// to whole cells, clamped to leave `b` at least one cell), `b` takes the
/// remainder.
fn split_child_rects(dir: SplitDir, ratio: f32, bounds: PaneRect) -> (PaneRect, PaneRect) {
    match dir {
        SplitDir::Vertical => {
            let w_a = ((bounds.w as f32 * ratio).round() as u32)
                .max(1)
                .min(bounds.w.saturating_sub(1));
            let w_b = bounds.w - w_a;
            (
                PaneRect { x: bounds.x, y: bounds.y, w: w_a, h: bounds.h },
                PaneRect { x: bounds.x + w_a as i32, y: bounds.y, w: w_b, h: bounds.h },
            )
        }
        SplitDir::Horizontal => {
            let h_a = ((bounds.h as f32 * ratio).round() as u32)
                .max(1)
                .min(bounds.h.saturating_sub(1));
            let h_b = bounds.h - h_a;
            (
                PaneRect { x: bounds.x, y: bounds.y, w: bounds.w, h: h_a },
                PaneRect { x: bounds.x, y: bounds.y + h_a as i32, w: bounds.w, h: h_b },
            )
        }
    }
}

fn layout_rects(node: &Layout, bounds: PaneRect, out: &mut HashMap<String, PaneRect>) {
    match node {
        Layout::Leaf(id) => {
            out.insert(id.clone(), bounds);
        }
        Layout::Split { dir, ratio, a, b } => {
            let (rect_a, rect_b) = split_child_rects(*dir, *ratio, bounds);
            layout_rects(a, rect_a, out);
            layout_rects(b, rect_b, out);
        }
    }
}

/// Which way a keyboard resize nudges the focused pane's nearest divider.
#[derive(Debug, Clone, Copy)]
enum ResizeDir {
    Left,
    Right,
    Up,
    Down,
}

/// Number of cells one keyboard resize step moves a divider.
const RESIZE_STEP: i32 = 2;

/// Move the divider of the nearest enclosing split of the matching
/// orientation in the arrow's direction: `cells > 0` for Right/Down pushes
/// it right/down (the `a` child grows), `cells < 0` for Left/Up pushes it
/// up/left (`a` shrinks). The sign is purely the arrow direction — it does
/// not depend on which side the focused pane is on — so the shared border
/// always tracks the key pressed (e.g. with the bottom pane focused, Up
/// moves the border up and grows it). `want` selects the orientation:
/// `Vertical` for left/right (width), `Horizontal` for up/down (height).
/// Recurses toward `target` so the *innermost* matching split — the one
/// directly bordering the focused pane — wins. Returns true if a split was
/// adjusted.
fn resize_split(
    node: &mut Layout,
    target: &str,
    want: SplitDir,
    cells: i32,
    bounds: PaneRect,
) -> bool {
    let Layout::Split { dir, ratio, a, b } = node else {
        return false;
    };
    let in_a = a.contains_leaf(target);
    if !in_a && !b.contains_leaf(target) {
        return false;
    }
    let (rect_a, rect_b) = split_child_rects(*dir, *ratio, bounds);
    let (child, child_bounds) = if in_a { (a.as_mut(), rect_a) } else { (b.as_mut(), rect_b) };
    // Innermost matching split wins — try deeper before adjusting here.
    if resize_split(child, target, want, cells, child_bounds) {
        return true;
    }
    if *dir == want {
        let region = match want {
            SplitDir::Vertical => bounds.w,
            SplitDir::Horizontal => bounds.h,
        } as f32;
        if region < 2.0 {
            return false;
        }
        // `ratio` is the `a` child's fraction. The divider moves in the
        // arrow's direction regardless of which child is focused: Right/
        // Down (+cells) grow `a`, Left/Up (−cells) shrink it. `in_a` only
        // selected which split to move, not the sign.
        *ratio = (*ratio + cells as f32 / region).clamp(MIN_RATIO, MAX_RATIO);
        return true;
    }
    false
}

/// Hit-test a pointer cell against every split divider in the active
/// layout. Returns the path (a/b descent steps from the root) to the
/// split whose divider lies under `(col, row)`, its orientation, and the
/// bounds the split was laid out in — enough to re-find the split and
/// recompute its ratio as the pointer drags. Children are tested first so
/// the innermost (visually topmost) divider wins when nested boundaries
/// overlap a cell.
fn separator_hit(
    node: &Layout,
    bounds: PaneRect,
    col: i32,
    row: i32,
    path: &mut Vec<bool>,
) -> Option<(Vec<bool>, SplitDir, PaneRect)> {
    let Layout::Split { dir, ratio, a, b } = node else {
        return None;
    };
    let (rect_a, rect_b) = split_child_rects(*dir, *ratio, bounds);
    path.push(true);
    if let Some(hit) = separator_hit(a, rect_a, col, row, path) {
        return Some(hit);
    }
    path.pop();
    path.push(false);
    if let Some(hit) = separator_hit(b, rect_b, col, row, path) {
        return Some(hit);
    }
    path.pop();
    // This split's own divider: the cell edge between `a` and `b`. The two
    // cells straddling that edge count as a grab so a 1-px line stays
    // clickable at cell granularity.
    let on = match dir {
        SplitDir::Vertical => {
            let dx = rect_b.x;
            row >= bounds.y
                && row < bounds.y + bounds.h as i32
                && (col == dx || col == dx - 1)
        }
        SplitDir::Horizontal => {
            let dy = rect_b.y;
            col >= bounds.x
                && col < bounds.x + bounds.w as i32
                && (row == dy || row == dy - 1)
        }
    };
    if on {
        Some((path.clone(), *dir, bounds))
    } else {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────
// PTY-per-pane plumbing
// ─────────────────────────────────────────────────────────────────────────

struct PanePty {
    master: OwnedFd,
    child: Pid,
}

impl PanePty {
    fn spawn(rows: u16, cols: u16) -> Result<Self> {
        let winsize = Winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result =
            unsafe { forkpty(Some(&winsize), None) }.context("forkpty")?;
        match result {
            ForkptyResult::Child => {
                let shell = user_shell();
                unsafe { std::env::set_var("TERM", "xterm-256color") };
                let cshell = std::ffi::CString::new(shell.as_str()).unwrap();
                let _ = execvp(&cshell, &[&cshell]);
                // exec failed — exit child immediately so we don't run
                // through the parent's drop handlers.
                std::process::exit(127);
            }
            ForkptyResult::Parent { child, master } => Ok(Self { master, child }),
        }
    }

    fn raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    fn write_all(&self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            match nix::unistd::write(&self.master, data) {
                Ok(0) => bail!("pty write returned 0"),
                Ok(n) => data = &data[n..],
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(anyhow!("pty write: {e}")),
            }
        }
        Ok(())
    }

    fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }
}

impl Drop for PanePty {
    fn drop(&mut self) {
        let _ = kill(self.child, Signal::SIGHUP);
    }
}

/// Per-pane scrollback navigation state. Each pane scrolls
/// independently — the user can leave one pane scrolled back, switch
/// tabs or focus another pane, and the original pane keeps its offset
/// (host-side) and indicator (client-side). Cleared on exit / when the
/// offset hits live.
#[derive(Debug)]
struct PaneScroll {
    offset: u32,
    /// Latest history-depth reported by the host in the
    /// `SetPortalScrollback` ack (§9.3). Used to size the scrollbar
    /// thumb. 0 until the first ack arrives.
    history_depth: u32,
    /// Half the pane's portal-row count, cached on entry. Used for
    /// PgUp/PgDn / d / u "half-page" jumps.
    half_page: u32,
    /// Partial CSI sequence buffer (`\e`, `\e[`, `\e[5`, …). Lets us
    /// recognise multi-byte arrow / PgUp / PgDn sequences while
    /// scroll-mode keys are dispatched to this pane.
    csi_buf: Vec<u8>,
}

struct Pane {
    /// Stable default label, equal to the portal id (`p1`, `p2`, …).
    /// Never mutated after construction; renames live in `manual_title`
    /// and OSC 0/1/2 in `osc_title` so the portal id stays usable as
    /// both the wire id and the chrome fallback.
    title: String,
    /// User rename via `prefix-r`. Wins over `osc_title`. Cleared by
    /// renaming to an empty string, which falls back to OSC / default.
    manual_title: Option<String>,
    /// Most recent OSC 0/2 (window title) the inner program emitted,
    /// delivered via `EVT_TITLE_CHANGE`. Empty payloads clear it.
    osc_title: Option<String>,
    /// Most recent OSC 1 (icon name). Used as a tertiary fallback in
    /// case the program only sets the icon name (rare, but mutt and a
    /// couple of others do it).
    osc_icon: Option<String>,
    pty: PanePty,
    /// Most recent host-cell rect this pane was laid out in. Used to
    /// detect "actually changed" and skip redundant PRT updates.
    last_rect: Option<PaneRect>,
    /// Most recent inner-grid size (cols, rows) plumbed through
    /// CreatePortal / UpdateSize. Used for the same dedup as
    /// `last_rect`, but tracked separately because the inner size is
    /// `outer_rect_size minus chrome`.
    last_inner: Option<(u32, u32)>,
    /// Mouse-protocol mode the inner program last opted into, as
    /// reported by PRT `MouseModeChange` events (§8.9). 0 = off, 1
    /// = X10, 2 = normal, 3 = button, 4 = any. Used by the wheel
    /// dispatcher to decide whether wheel events drive vmux's
    /// per-pane scrollback or get forwarded to the inner program.
    inner_mouse_protocol: u8,
    /// `Some` while this pane is navigating its scrollback. Independent
    /// per-pane so two panes can sit at different offsets at the same
    /// time and tab switches don't clobber state.
    scroll: Option<PaneScroll>,
    /// Sticky activity flag: set when the host reports the pane
    /// produced meaningful (scrolled) output via PRT
    /// `EVT_PORTAL_ACTIVITY` while the pane's tab was not in view.
    /// Cleared when that tab is next activated. Drives the tab-bar
    /// activity marker.
    activity: bool,
}

impl Pane {
    /// Effective display title, highest precedence first.
    fn effective_title(&self) -> &str {
        self.manual_title
            .as_deref()
            .or(self.osc_title.as_deref())
            .or(self.osc_icon.as_deref())
            .unwrap_or(&self.title)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// VGE chrome
// ─────────────────────────────────────────────────────────────────────────

const PORTAL_DRAW_ORDER: i32 = 0;
/// Pane chrome (outline + title) renders above the portal so the title
/// overlay shows on top of the shell text in the bottom-right cell.
/// The outline itself only draws on border cells the portal doesn't
/// touch, so the higher draw order is harmless there.
const CHROME_DRAW_ORDER: i32 = 10;
const MODAL_DRAW_ORDER: i32 = 100;

/// Per-pane scrollback size in lines. The host caps this at its own
/// `max_scrollback_lines` (the reference impl advertises 100k); 5k is a
/// reasonable default that's plenty for shell history without burning
/// memory on dozens of panes.
const PORTAL_SCROLLBACK_LINES: u32 = 5_000;

/// Reserved high half of the request-id space used for our
/// `SetPortalScrollback` requests. We allocate one fresh id per request
/// (counting up from this base) and remember which pane it belongs to
/// in `State.pending_scrolls`, so two panes can have outstanding scroll
/// acks at the same time and the response handler routes each ack to
/// the right pane.
const SCROLL_REQUEST_ID_BASE: u32 = 0x5C_00_00_00;

/// Brand color (#56799f) shared by separators, the title thumb, the
/// active-tab gradient, and the modal outline. Muted blue — distinctive
/// against the terminal background. Only used as a fallback when the host
/// does not theme `host.*`; otherwise the host's accent palette wins.
const COLOR_BRAND: Color = Color {
    r: 0x56 as f32 / 255.0,
    g: 0x79 as f32 / 255.0,
    b: 0x9f as f32 / 255.0,
    a: 1.0,
};

/// Reserved host-provided accent style id (VGE spec §7.3). Resolves to
/// veter's configured accent for this portal's nesting depth.
const HOST_ACCENT_STYLE_ID: &str = "host.accent";

/// Set once at startup from the PRT probe (`FEAT_VGE_HOST_THEMED_STYLES`).
/// When true, chrome accents reference the host `host.accent` style so
/// they follow veter's theme and signal nesting depth; otherwise they
/// use the compiled-in `COLOR_BRAND`.
static HOST_THEMED_STYLES: AtomicBool = AtomicBool::new(false);

/// Accent fill/stroke style for vmux chrome — a host `StyleRef` when the
/// host themes `host.*`, else the compiled-in brand color.
fn accent_style() -> Style {
    if CLI_ACCENT_SET.load(Ordering::Relaxed) {
        Style::Flat(accent_color())
    } else if HOST_THEMED_STYLES.load(Ordering::Relaxed) {
        Style::Ref(HOST_ACCENT_STYLE_ID.to_string())
    } else {
        Style::Flat(COLOR_BRAND)
    }
}

/// Straight RGBA8 accent the host reported for this vmux's nesting depth,
/// packed `0xRRGGBBAA`. Valid only when `HOST_THEMED_STYLES` is set; it is
/// the concrete value `host.accent` resolves to, so locally-derived shades
/// match the `StyleRef` chrome exactly.
static HOST_ACCENT_RGBA: AtomicU32 = AtomicU32::new(0);

/// CLI accent override (`--accent`/`-A`), packed `0xRRGGBBAA`. When
/// `CLI_ACCENT_SET` is true this wins over both the host's reported accent
/// and the compiled-in brand, letting an outer `ssh`/`vmux` give a nested
/// session a distinct chrome color. Set once at startup.
static CLI_ACCENT_SET: AtomicBool = AtomicBool::new(false);
static CLI_ACCENT_RGBA: AtomicU32 = AtomicU32::new(0);

/// Unpack a `0xRRGGBBAA` value into a normalized `Color`.
fn color_from_rgba(rgba: u32) -> Color {
    let [r, g, b, a] = rgba.to_be_bytes();
    Color {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: a as f32 / 255.0,
    }
}

/// The accent as a concrete color — the host's reported accent when it
/// themes `host.*`, else the compiled-in brand. Used to derive shades the
/// host does not provide as their own styles.
fn accent_color() -> Color {
    if CLI_ACCENT_SET.load(Ordering::Relaxed) {
        color_from_rgba(CLI_ACCENT_RGBA.load(Ordering::Relaxed))
    } else if HOST_THEMED_STYLES.load(Ordering::Relaxed) {
        color_from_rgba(HOST_ACCENT_RGBA.load(Ordering::Relaxed))
    } else {
        COLOR_BRAND
    }
}

/// Translucent accent for the thumb behind a pane's title text.
fn title_thumb_style() -> Style {
    if CLI_ACCENT_SET.load(Ordering::Relaxed) || HOST_THEMED_STYLES.load(Ordering::Relaxed) {
        Style::Flat(Color { a: 0.35, ..accent_color() })
    } else {
        Style::Flat(COLOR_TITLE_THUMB)
    }
}

/// Dark accent-tinted surface for modal/dialog backgrounds, the tab-bar
/// session segment, and inactive tab-number cells. Scaled toward black so
/// light foreground text stays legible over arbitrary shell content.
fn surface_style() -> Style {
    if CLI_ACCENT_SET.load(Ordering::Relaxed) || HOST_THEMED_STYLES.load(Ordering::Relaxed) {
        let c = accent_color();
        const K: f32 = 0.20;
        Style::Flat(Color { r: c.r * K, g: c.g * K, b: c.b * K, a: 0.96 })
    } else {
        Style::Flat(COLOR_MODAL_BG)
    }
}

/// Same hue as COLOR_BRAND but semi-transparent — the fallback thumb fill
/// behind a pane's title text when the host does not theme `host.*`.
const COLOR_TITLE_THUMB: Color = Color {
    r: COLOR_BRAND.r,
    g: COLOR_BRAND.g,
    b: COLOR_BRAND.b,
    a: 0.35,
};
const COLOR_TAB_INACTIVE_TEXT: Color = Color {
    r: 0.55,
    g: 0.58,
    b: 0.65,
    a: 1.0,
};
const COLOR_TAB_ACTIVE_TEXT: Color = Color {
    r: 0.98,
    g: 0.94,
    b: 0.85,
    a: 1.0,
};
const COLOR_TITLE_TEXT: Color = Color {
    r: 0.92,
    g: 0.94,
    b: 0.98,
    a: 1.0,
};
const COLOR_SCROLLBAR: Color = Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 0.35,
};
/// Modal background: a dark, mostly-opaque base for legibility — the
/// modal can sit over arbitrary shell text. Tinted slightly toward
/// COLOR_BRAND so it reads as part of the same palette as the brand
/// accents (separators, title thumb, active tab) instead of looking
/// like a leftover from another design.
const COLOR_MODAL_BG: Color = Color {
    r: 0.09,
    g: 0.06,
    b: 0.16,
    a: 0.96,
};
const COLOR_MODAL_TEXT: Color = Color {
    r: 0.96,
    g: 0.96,
    b: 0.98,
    a: 1.0,
};

/// Slight corner rounding applied uniformly across vmux chrome (active
/// tab top edges, modal corners, pane title pills). One value so all
/// chrome reads as part of the same visual language; conservative on
/// purpose — too aggressive and the chrome turns into a button.
/// Y units only; the matching `rx` is computed per call site so
/// anisotropic cells don't produce visually elliptical corners.
const CHROME_CORNER_RY: f32 = 0.175;

/// Compute an `rx` in cell units that yields a visually-circular arc
/// for `ry` given the host's pixel-per-cell ratios. Clamps so the corner
/// never exceeds 40% of the rect's smaller dimension (prevents arcs
/// from collapsing into each other on narrow shapes).
fn chrome_corner_radii(width: f32, height: f32, cell_pw: f32, cell_ph: f32) -> (f32, f32) {
    let ry = CHROME_CORNER_RY.min(height * 0.4);
    let rx = (CHROME_CORNER_RY * cell_ph / cell_pw).min(width * 0.4);
    (rx, ry)
}

/// Build a closed rectangle path with selectable rounded corners.
/// Corner toggles are `(top_left, top_right, bottom_right, bottom_left)`
/// — `false` produces a square corner. Traversal is CCW in y-down
/// coords (matches femtovg's tessellator preference — see `brick_drawcmd`
/// in tools/breakout).
fn rounded_rect_path_corners(
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

/// Build a closed rounded-rectangle path with all four corners rounded.
fn rounded_rect_path(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rx: f32,
    ry: f32,
) -> Vec<PathSegment> {
    rounded_rect_path_corners(x0, y0, x1, y1, rx, ry, true, true, true, true)
}

/// VGE element ID used for a pane's chrome (title thumb + scroll thumb).
fn chrome_element_id(pane_id: &str) -> String {
    format!("vmux-chrome-{pane_id}")
}

const MODAL_ELEMENT_ID: &str = "vmux-modal";
/// Children of the help modal. Each scrolls or sits at a fixed offset
/// relative to MODAL_ELEMENT_ID; deleting the parent cascades to all
/// of them (§9.6).
const MODAL_BODY_FILL_ID: &str = "vmux-modal-bg";
const MODAL_BODY_LINES_ID: &str = "vmux-modal-body";
const MODAL_TRACK_ID: &str = "vmux-modal-track";
const MODAL_THUMB_ID: &str = "vmux-modal-thumb";
const TABBAR_ELEMENT_ID: &str = "vmux-tabbar";
/// Floor on the per-tab label length when the bar is crowded. Labels
/// shrink uniformly to reclaim space but never below this many
/// characters (one glyph + `…`); past that the bar scrolls instead.
const TABBAR_MIN_LABEL: usize = 2;
/// Cells reserved for an overflow chevron (`‹` / `›`) on a clipped edge
/// of the tab bar.
const TABBAR_CHEVRON_W: f32 = 2.0;
/// Single VGE element holding all between-pane separator strokes for the
/// active tab. Recreated on every relayout — the layout tree determines
/// which split boundaries to draw.
const SEPARATORS_ELEMENT_ID: &str = "vmux-separators";

/// Elide `label` to at most `max` characters, appending `…` when it had
/// to be cut. `max` is expected to be at least [`TABBAR_MIN_LABEL`].
fn elide_tab_label(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        return label.to_string();
    }
    let mut out: String = label.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Largest uniform label-character cap that still lets every tab fit on
/// row 0 at full width. Labels are only shortened to reclaim space:
/// when they all fit, the cap covers the longest label (no truncation);
/// when they don't, it shrinks but never below [`TABBAR_MIN_LABEL`].
/// A cap pinned to the minimum means even minimal labels overflow and
/// the bar will have to scroll.
fn tabbar_label_cap(labels: &[String], num_widths: &[f32], host_w: f32) -> usize {
    let n = labels.len();
    if n == 0 {
        return TABBAR_MIN_LABEL;
    }
    let lens: Vec<usize> = labels.iter().map(|l| l.chars().count()).collect();
    // Fixed per-tab cost independent of the cap: the " N " number rect
    // plus the name rect's two padding cells. What's left is the budget
    // for label glyphs.
    let base: f32 = num_widths.iter().sum::<f32>() + 2.0 * n as f32;
    let budget = host_w - base;
    // Total glyph width at a given cap: Σ min(len_i, cap). Monotonic
    // non-decreasing in `cap`.
    let glyph_width = |cap: usize| -> f32 {
        lens.iter().map(|&l| l.min(cap) as f32).sum()
    };
    let max_len = lens.iter().copied().max().unwrap_or(0);
    if glyph_width(TABBAR_MIN_LABEL) > budget {
        return TABBAR_MIN_LABEL;
    }
    if glyph_width(max_len) <= budget {
        return max_len.max(TABBAR_MIN_LABEL);
    }
    // Largest cap in [MIN, max_len] whose glyph width fits the budget.
    let mut lo = TABBAR_MIN_LABEL;
    let mut hi = max_len;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if glyph_width(mid) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Given `first` (index of the leftmost visible tab), return the
/// inclusive index of the last tab that fits on row 0. Mirrors the
/// budgeting used when the bar is drawn: a left chevron costs
/// [`TABBAR_CHEVRON_W`] when `first > 0`, and a right chevron costs the
/// same whenever some tabs still spill past the right edge.
fn tabbar_last_visible(widths: &[f32], host_w: f32, first: usize) -> usize {
    let n = widths.len();
    if n == 0 {
        return 0;
    }
    let left = if first > 0 { TABBAR_CHEVRON_W } else { 0.0 };
    // First pass: assume no right chevron and check whether every
    // remaining tab fits.
    let full_budget = host_w - left;
    let mut x = 0.0;
    let mut all_fit = true;
    for w in &widths[first..] {
        if x + w > full_budget {
            all_fit = false;
            break;
        }
        x += w;
    }
    if all_fit {
        return n - 1;
    }
    // Some tabs spill past the edge → a right chevron is needed, so
    // re-pack against the reduced budget. The leftmost visible tab is
    // always shown even if it alone overflows (degenerate narrow host).
    let budget = full_budget - TABBAR_CHEVRON_W;
    let mut x = widths[first];
    let mut last = first;
    for (i, w) in widths.iter().enumerate().skip(first + 1) {
        if x + w > budget {
            break;
        }
        x += w;
        last = i;
    }
    last
}

/// Nudge `scroll` (the leftmost visible tab, carried across renders) the
/// minimum amount needed to keep the `active` tab on screen, then return
/// the inclusive `[first, last]` window of tabs to draw.
///
/// The bar jumps left when the active tab fell off the left edge,
/// advances right until the active tab fits, then is pulled back left to
/// reclaim empty space — so closing tabs collapses the bar instead of
/// leaving it stuck mid-scroll.
fn tabbar_window(
    widths: &[f32],
    active: usize,
    host_w: f32,
    scroll: &mut usize,
) -> (usize, usize) {
    let n = widths.len();
    if n == 0 {
        *scroll = 0;
        return (0, 0);
    }
    *scroll = (*scroll).min(n - 1);
    if active < *scroll {
        *scroll = active;
    }
    while active > tabbar_last_visible(widths, host_w, *scroll) {
        *scroll += 1;
    }
    while *scroll > 0 && tabbar_last_visible(widths, host_w, *scroll - 1) == n - 1 {
        *scroll -= 1;
    }
    (*scroll, tabbar_last_visible(widths, host_w, *scroll))
}

/// One visible tab's placement on row 0 of the tab bar. The number
/// sub-rect spans `[x0, x0 + num_w)` and the name sub-rect
/// `[x0 + num_w, x0 + num_w + name_w)`; together they form the tab's
/// clickable extent. `index` is the tab's position in `State.tabs`.
struct TabSlot {
    index: usize,
    x0: f32,
    num_w: f32,
    name_w: f32,
    /// Label already elided to the bar's current per-tab character cap.
    label: String,
}

/// Geometry of one tab-bar render: the visible tabs with their row-0
/// placement, the optional session segment, and which overflow
/// chevrons are shown. Computed once by [`compute_tabbar_layout`] and
/// shared by the draw path ([`build_tabbar_commands`]) and the click
/// hit-test ([`tabbar_hit`]) so the two can never disagree about where
/// a tab sits.
struct TabbarLayout {
    slots: Vec<TabSlot>,
    /// `(elided label, width)` of the session-name segment, or `None`
    /// for a plain `veter` host or a bar too narrow to spare the room.
    session_seg: Option<(String, f32)>,
    has_left: bool,
    has_right: bool,
}

impl TabbarLayout {
    /// Width of the session segment (0 when absent) — also the
    /// x-coordinate at which the left overflow chevron is drawn.
    fn session_w(&self) -> f32 {
        self.session_seg.as_ref().map(|(_, w)| *w).unwrap_or(0.0)
    }
}

/// Resolve every visible tab's row-0 placement for one tab-bar render.
/// This is the single source of truth for tab geometry — label cap,
/// elision and scroll window are decided here once — so the drawn bar
/// and the click hit-test always agree. `scroll` is nudged exactly as
/// the draw path needs (see [`tabbar_window`]).
fn compute_tabbar_layout(
    labels: &[String],
    active: usize,
    scroll: &mut usize,
    host_w: u32,
    session: Option<&str>,
) -> TabbarLayout {
    let host_wf = host_w as f32;
    let n = labels.len();

    // Optional session-name segment, pinned to the left of the bar.
    // Capped so it never eats more than ~40% of row 0; the tabs get
    // the remaining `bar_w` and every x-coordinate below is offset by
    // `session_w`.
    let session_seg: Option<(String, f32)> = session.and_then(|name| {
        if name.is_empty() {
            return None;
        }
        let max_chars = (((host_wf * 0.4) as usize).saturating_sub(2)).min(24);
        if max_chars < TABBAR_MIN_LABEL {
            return None;
        }
        let label = elide_tab_label(name, max_chars);
        let w = (label.chars().count() + 2) as f32;
        Some((label, w))
    });
    let session_w = session_seg.as_ref().map(|(_, w)| *w).unwrap_or(0.0);
    let bar_w = (host_wf - session_w).max(0.0);

    // Per-tab sub-rect widths: " N " number rect + " label " name rect.
    // The " N " number rect depends only on the index.
    let num_widths: Vec<f32> = (0..n)
        .map(|i| format!(" {} ", i + 1).chars().count() as f32)
        .collect();
    // Pick the widest label cap the bar can afford, then elide to it —
    // so labels are only shortened to reclaim space, never pre-emptively,
    // and the widths used for scrolling match what gets drawn.
    let cap = tabbar_label_cap(labels, &num_widths, bar_w);
    let elided: Vec<String> =
        labels.iter().map(|l| elide_tab_label(l, cap)).collect();
    let name_widths: Vec<f32> = elided
        .iter()
        .map(|l| (l.chars().count() + 2) as f32)
        .collect();
    let tab_widths: Vec<f32> =
        (0..n).map(|i| num_widths[i] + name_widths[i]).collect();

    let (first, last) = tabbar_window(&tab_widths, active, bar_w, scroll);
    let has_left = first > 0;
    let has_right = n > 0 && last + 1 < n;

    // Walk the visible window left-to-right, mirroring the draw loop:
    // the first tab starts past the session segment and any left
    // chevron, and each subsequent tab abuts the previous one.
    let mut slots: Vec<TabSlot> = Vec::new();
    let mut x = session_w + if has_left { TABBAR_CHEVRON_W } else { 0.0 };
    for i in first..(last + 1).min(n) {
        slots.push(TabSlot {
            index: i,
            x0: x,
            num_w: num_widths[i],
            name_w: name_widths[i],
            label: elided[i].clone(),
        });
        x += num_widths[i] + name_widths[i];
    }

    TabbarLayout {
        slots,
        session_seg,
        has_left,
        has_right,
    }
}

/// What a row-0 (tab-bar) click landed on, resolved against a
/// [`TabbarLayout`].
enum TabbarHit {
    /// A visible tab — switch to it.
    Tab(usize),
    /// The left overflow chevron — step to the previous tab.
    PrevTab,
    /// The right overflow chevron — step to the next tab.
    NextTab,
}

/// Hit-test a row-0 click at host column `col` against a tab bar's
/// geometry. The overflow chevrons own the bar's two edges; everything
/// between resolves to whichever tab's extent contains `col`.
fn tabbar_hit(layout: &TabbarLayout, host_w: u32, col: f32) -> Option<TabbarHit> {
    if layout.has_left {
        let x0 = layout.session_w();
        if col >= x0 && col < x0 + TABBAR_CHEVRON_W {
            return Some(TabbarHit::PrevTab);
        }
    }
    if layout.has_right {
        let x0 = host_w as f32 - TABBAR_CHEVRON_W;
        if col >= x0 && col < host_w as f32 {
            return Some(TabbarHit::NextTab);
        }
    }
    for slot in &layout.slots {
        if col >= slot.x0 && col < slot.x0 + slot.num_w + slot.name_w {
            return Some(TabbarHit::Tab(slot.index));
        }
    }
    None
}

/// Build the VGE element body that renders the host's top-row tab bar.
/// Each tab is split into two adjacent sub-rects:
///   - **number rect** (always present, top-left corner rounded)
///     showing the 1-based tab index. Dim modal-background fill
///     normally; takes the brand-color fill — the same highlight the
///     active tab's name rect uses — when the tab has unseen
///     background activity (`activity[i]`).
///   - **name rect** (filled with brand color only when active, top-
///     right corner rounded) showing the tab title.
///
/// When the tabs don't all fit on row 0, labels are first shortened —
/// uniformly, down to [`TABBAR_MIN_LABEL`] characters — to reclaim
/// space. If even minimal labels overflow, the bar scrolls: `scroll`
/// (the leftmost visible tab, carried across renders) is nudged the
/// minimum amount needed to keep the active tab on screen, and `‹` / `›`
/// chevrons mark tabs clipped off either edge.
///
/// The rule along row 1 separates the bar from the pane area below;
/// active tab fills sit flush on top of it.
fn build_tabbar_commands(
    labels: &[String],
    active: usize,
    scroll: &mut usize,
    host_w: u32,
    cell_pw: f32,
    cell_ph: f32,
    session: Option<&str>,
    activity: &[bool],
) -> CreateElementBody {
    let host_wf = host_w as f32;
    let layout = compute_tabbar_layout(labels, active, scroll, host_w, session);

    // No bar background — row 0 of the host vt100 is left untouched
    // (vmux never writes there) so the terminal's default cell color
    // shows through, matching whatever theme the user runs veter in.
    let mut cmds: Vec<DrawCmd> = Vec::new();

    // Solid rule separating the tab row from the pane area at row 1.
    // A stroked line is centred on its path, so positioning it at y = 1.0
    // would straddle the boundary and the tab fills (which span [0, 1.0])
    // would overpaint its top half — the line then reads ~half as thick
    // under every tab. We want `RULE_W` of *visible* rule below y = 1.0,
    // so the bottom edge sits at 1.0 + RULE_W. Abutting the top edge
    // exactly at y = 1.0 leaves an anti-aliasing seam where the background
    // bleeds through between the fills and the rule, so we extend the
    // stroke up by `SEAM_OVERLAP` into the fills (drawn after the rule, so
    // they paint over the sliver): the overlap closes the seam without
    // thinning the visible rule.
    const RULE_W: f32 = 0.1;
    const SEAM_OVERLAP: f32 = 0.04;
    let stroke_w = RULE_W + SEAM_OVERLAP;
    let rule_y = 1.0 + (RULE_W - SEAM_OVERLAP) / 2.0;
    cmds.push(DrawCmd::DrawLines {
        stroke: accent_style(),
        line_width: stroke_w,
        lines: vec![(
            Point { x: 0.0, y: rule_y },
            Point { x: host_wf, y: rule_y },
        )],
    });

    // Session-name segment: a dim rounded block pinned at x = 0,
    // showing the `vsd` session this vmux lives in.
    if let Some((label, w)) = &layout.session_seg {
        let (rx, ry) = chrome_corner_radii(*w, 1.0, cell_pw, cell_ph);
        cmds.push(DrawCmd::FillPath {
            fill: surface_style(),
            segments: rounded_rect_path_corners(
                0.0, 0.0, *w, 1.0, rx, ry,
                true,  // TL rounded
                false, // TR (meets the first tab / chevron)
                false, // BR
                false, // BL (sits on the row-1 rule)
            ),
        });
        cmds.push(DrawCmd::DrawText {
            origin: Point { x: 0.0, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(COLOR_TAB_ACTIVE_TEXT),
            font_style: FontStyle(0x01),
            text: format!(" {label} "),
        });
    }

    // Left overflow chevron — tabs clipped off the start of the bar.
    if layout.has_left {
        cmds.push(DrawCmd::DrawText {
            origin: Point {
                x: layout.session_w(),
                y: 0.0,
            },
            align: Align::Left,
            fill: accent_style(),
            font_style: FontStyle(0x00),
            text: " ‹".to_string(),
        });
    }

    for slot in &layout.slots {
        let i = slot.index;
        let x = slot.x0;
        let num_w = slot.num_w;
        let name_w = slot.name_w;
        let num_text = format!(" {} ", i + 1);
        let name_text = format!(" {} ", slot.label);
        let is_active = i == active;

        // Number sub-rect, top-left corner rounded. Dim modal
        // background normally; the brand fill — the same highlight the
        // active tab's name rect uses — when the tab has unseen
        // background activity.
        let has_activity = activity.get(i).copied().unwrap_or(false);
        let num_bg = if has_activity {
            accent_style()
        } else {
            surface_style()
        };
        let (num_rx, num_ry) = chrome_corner_radii(num_w, 1.0, cell_pw, cell_ph);
        cmds.push(DrawCmd::FillPath {
            fill: num_bg,
            segments: rounded_rect_path_corners(
                x,
                0.0,
                x + num_w,
                1.0,
                num_rx,
                num_ry,
                true,  // TL rounded
                false, // TR (meets the name rect, kept square)
                false, // BR
                false, // BL (sits on the row-1 rule)
            ),
        });
        // Light text — legible on both the dim and the brand fill.
        cmds.push(DrawCmd::DrawText {
            origin: Point { x, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(COLOR_TAB_ACTIVE_TEXT),
            font_style: FontStyle(0x00),
            text: num_text,
        });

        // Name sub-rect: brand-color fill only on the active tab; top-
        // right corner rounded so the tab silhouette has a tab shape.
        let name_x0 = x + num_w;
        if is_active {
            let (name_rx, name_ry) = chrome_corner_radii(name_w, 1.0, cell_pw, cell_ph);
            cmds.push(DrawCmd::FillPath {
                fill: accent_style(),
                segments: rounded_rect_path_corners(
                    name_x0,
                    0.0,
                    name_x0 + name_w,
                    1.0,
                    name_rx,
                    name_ry,
                    false, // TL (meets the number rect)
                    true,  // TR rounded
                    false, // BR (sits on the row-1 rule)
                    false, // BL
                ),
            });
        }
        cmds.push(DrawCmd::DrawText {
            origin: Point { x: name_x0, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(if is_active {
                COLOR_TAB_ACTIVE_TEXT
            } else {
                COLOR_TAB_INACTIVE_TEXT
            }),
            font_style: FontStyle(if is_active { 0x01 } else { 0x00 }),
            text: name_text,
        });
    }

    // Right overflow chevron — tabs clipped off the end of the bar.
    if layout.has_right {
        cmds.push(DrawCmd::DrawText {
            origin: Point {
                x: host_wf - TABBAR_CHEVRON_W,
                y: 0.0,
            },
            align: Align::Left,
            fill: accent_style(),
            font_style: FontStyle(0x00),
            text: "› ".to_string(),
        });
    }

    CreateElementBody {
        id: TABBAR_ELEMENT_ID.to_string(),
        commands: cmds,
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        // Tab bar lives at row 0, no portals there, so any draw_order
        // works; pick CHROME so it can't be obscured by future chrome.
        draw_order: CHROME_DRAW_ORDER,
        parent: None,
        size: None,
    }
}

/// State the chrome needs to render a scrollbar thumb on the right
/// border of a pane. `history_depth` is the number of rows in the
/// portal's scrollback ring; `offset` is the current scroll offset
/// (0 = live region). When this is `Some`, the thumb is drawn over the
/// outline's right edge.
#[derive(Clone, Copy)]
struct ScrollIndicator {
    offset: u32,
    history_depth: u32,
}

/// Build chrome draw-commands for one pane. Element origin is the
/// pane's top-left in host cells; commands are pane-relative.
///
/// `show_title` is false for the only pane in a tab — there's nothing to
/// disambiguate, so we hide the label. Scroll-indicator titles still
/// show because they convey active state, not just identity.
fn build_chrome_commands(
    rect: PaneRect,
    title: &str,
    focused: bool,
    show_title: bool,
    cell_pw: f32,
    cell_ph: f32,
    scroll: Option<ScrollIndicator>,
) -> Vec<DrawCmd> {
    let pw = rect.w as f32;
    let ph = rect.h as f32;
    let mut cmds: Vec<DrawCmd> = Vec::new();

    // Title text in the bottom-right portal cell, right-aligned, with
    // a semi-transparent rounded "thumb" behind it (no stroke). Drawn
    // at higher draw_order than the portal so it overlays shell text.
    // 0.5-cell inset from the right edge keeps the thumb clear of any
    // separator on the pane boundary. Both thumb and text are nudged
    // up by 1/4 of a cell so the thumb doesn't visually fuse with the
    // bottom edge of the pane.
    if show_title && !title.is_empty() {
        let text_w = title.chars().count() as f32;
        let lift = 0.25;
        let text_origin_x = pw - 1.0;
        let text_origin_y = ph - 1.0 - lift;
        let pad_x = 0.5;
        let thumb_x0 = text_origin_x - text_w - pad_x;
        let thumb_x1 = text_origin_x + pad_x;
        let thumb_y0 = text_origin_y;
        let thumb_y1 = text_origin_y + 1.0;
        // Same slight rounding as the modal/tab chrome, with rx
        // compensated for anisotropic cells.
        let (rx, ry) =
            chrome_corner_radii(thumb_x1 - thumb_x0, thumb_y1 - thumb_y0, cell_pw, cell_ph);
        // Focused pane gets the translucent accent thumb; inactive panes
        // use the dim surface fill — the same color as an inactive tab's
        // number cell — so focus reads as an accent highlight.
        cmds.push(DrawCmd::FillPath {
            fill: if focused {
                title_thumb_style()
            } else {
                surface_style()
            },
            segments: rounded_rect_path(thumb_x0, thumb_y0, thumb_x1, thumb_y1, rx, ry),
        });
        cmds.push(DrawCmd::DrawText {
            origin: Point {
                x: text_origin_x,
                y: text_origin_y,
            },
            align: Align::Right,
            fill: Style::Flat(COLOR_TITLE_TEXT),
            font_style: FontStyle(if focused { 0x01 } else { 0x00 }),
            text: title.to_string(),
        });
    }

    // Scrollbar thumb at the right edge of the pane. Drawn in cell
    // units; sized by visible_rows / (visible_rows + history_depth)
    // and positioned by offset. Track stops above the lifted title
    // row so the thumb never collides with the scroll-indicator title
    // that sits in the bottom-right cell while we're scrolling.
    if let Some(s) = scroll {
        let portal_rows = ph.max(1.0);
        let total = portal_rows + s.history_depth as f32;
        let track_x = pw - 1.30;
        let track_w = 0.35;
        let track_y0 = 0.5;
        let track_y1 = (ph - 1.25).max(track_y0 + 1.0);
        let track_h = (track_y1 - track_y0).max(1.0);

        let thumb_h_norm = (portal_rows / total).clamp(0.05, 1.0);
        let thumb_h = (thumb_h_norm * track_h).max(0.6);
        let available = (track_h - thumb_h).max(0.0);
        // 0 offset = bottom (live); offset = history_depth = top.
        let thumb_norm = if s.history_depth == 0 {
            1.0
        } else {
            (s.history_depth.saturating_sub(s.offset) as f32) / s.history_depth as f32
        };
        let thumb_y = track_y0 + thumb_norm * available;

        cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(COLOR_SCROLLBAR),
            rects: vec![Rect {
                x: track_x,
                y: thumb_y,
                w: track_w,
                h: thumb_h,
            }],
        });
    }

    cmds
}

fn build_modal_commands(
    host_w: u32,
    host_h: u32,
    title: &str,
    line: &str,
    cell_pw: f32,
    cell_ph: f32,
    caret: Option<usize>,
) -> CreateElementBody {
    // Center a fixed-size modal box on the host grid. The 4-cell box
    // is (title strip)(body top pad)(body line)(body bottom pad) —
    // title row is filled with brand color, the rest with the modal
    // bg, and the whole box gets a rounded-edge brand outline that
    // matches the tab and pill styling.
    let chars = line.chars().count() as f32;
    let inner_w = chars.max(20.0);
    let box_w = (inner_w + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = 4.0_f32.min(host_h.saturating_sub(2) as f32);

    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let (rx, ry) = chrome_corner_radii(box_w, box_h, cell_pw, cell_ph);
    let mut cmds = vec![
        // Body fill — full rounded rect; the title strip is drawn over
        // its top region.
        DrawCmd::FillPath {
            fill: surface_style(),
            segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
        },
        // Title strip — rounded only on the top corners so the seam
        // with the body fill below is straight.
        DrawCmd::FillPath {
            fill: accent_style(),
            segments: rounded_rect_path_corners(
                0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false,
            ),
        },
        DrawCmd::DrawLinePath {
            stroke: accent_style(),
            line_width: 0.1,
            segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
        },
        DrawCmd::DrawText {
            origin: Point {
                x: box_w * 0.5,
                y: 0.0,
            },
            align: Align::Center,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(0x01),
            text: title.into(),
        },
        DrawCmd::DrawText {
            origin: Point {
                x: box_w * 0.5,
                y: 2.0,
            },
            align: Align::Center,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(0x00),
            text: line.into(),
        },
    ];

    // Text cursor: a thin vertical bar drawn at the insertion point rather
    // than a literal caret glyph in the string, so it doesn't shift the
    // text. The body line is center-aligned, so its left edge is
    // `box_w/2 - chars/2`; the bar sits `caret` cells to the right of that
    // (one glyph advance ≈ one cell, the same assumption the box sizing
    // above makes).
    if let Some(c) = caret {
        let caret_x = box_w * 0.5 - chars * 0.5 + c.min(line.chars().count()) as f32;
        const HALF_W: f32 = 0.07;
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(accent_color()),
            segments: rounded_rect_path(caret_x - HALF_W, 2.1, caret_x + HALF_W, 2.9, 0.0, 0.0),
        });
    }

    CreateElementBody {
        id: MODAL_ELEMENT_ID.to_string(),
        commands: cmds,
        origin: Point {
            x: origin_x,
            y: origin_y,
        },
        is_visible: true,
        draw_order: MODAL_DRAW_ORDER,
        parent: None,
        size: None,
    }
}

/// Help-modal contents: list of every prefix keybinding. Activated via
/// `prefix-?`, dismissed by any keystroke. Built once on first use so the
/// prefix-key lines reflect `--prefix`; the layout/sizing helpers read the
/// resulting slice exactly as they did the former `const`.
fn help_lines() -> &'static [String] {
    static LINES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LINES.get_or_init(|| {
        let p = prefix_name();
        vec![
            format!("vmux keybindings  —  prefix is {p}"),
            "".into(),
            "Pane".into(),
            "  v        split focused pane vertically".into(),
            "  h        split focused pane horizontally".into(),
            "  o        cycle focus to next pane".into(),
            "  x        close focused pane".into(),
            "  r        rename focused pane".into(),
            "  z        toggle zoom (focused pane fills the tab)".into(),
            "  s        resize mode (hjkl/arrows; q/Esc/Enter exit)".into(),
            "".into(),
            "Tab".into(),
            "  c        new tab".into(),
            "  n / →    next tab".into(),
            "  p / ←    previous tab".into(),
            "  1..9     jump to tab N".into(),
            "  < / >    move current tab left / right".into(),
            "  R        rename current tab".into(),
            "".into(),
            "Scroll  (prefix-[ enters; q/Esc/G exits)".into(),
            "  k / Up        scroll up one line".into(),
            "  j / Down      scroll down one line".into(),
            "  u / d         half page up / down".into(),
            "  PgUp / PgDn   full page up / down".into(),
            "  g / Home      jump to top of scrollback".into(),
            "  0 / End       jump back to live".into(),
            "".into(),
            "Mouse".into(),
            "  click pane     focus it".into(),
            "  click tab      switch to it".into(),
            "  drag divider   resize adjacent panes".into(),
            "  wheel          scroll pane under cursor".into(),
            "".into(),
            "Rename edit  (prefix-r / prefix-R; Enter commits, Esc cancels)".into(),
            "  Ctrl+A/E      start / end of line".into(),
            "  Ctrl+B/F      back / forward one char (or ←/→)".into(),
            "  Alt+B/F       back / forward one word".into(),
            "  Ctrl+W        delete word before cursor".into(),
            "  Alt+D         delete word after cursor".into(),
            "  Ctrl+U/K      delete to start / end of line".into(),
            "  Ctrl+D / Del  delete char under cursor".into(),
            "".into(),
            "Misc".into(),
            "  ?           show this help".into(),
            "  q           quit vmux (asks y/n to confirm)".into(),
            format!("  {p}  send a literal {p}"),
            "".into(),
            "j/k or ↑/↓ scroll · any other key dismisses".into(),
        ]
    })
}

/// Number of body lines a half-page jump moves through.
const HELP_HALF_PAGE: i64 = 6;

/// Returns (visible body rows, max scroll offset in body lines) given a
/// modal box height. Body lines start at row 2 (after the title strip
/// and a one-row gap) and stop at row `box_h - 1`, leaving a one-row
/// bottom pad. When `box_h` is too small for the gap/pad, body content
/// fills whatever is left.
fn help_body_window(box_h: f32) -> (usize, u32) {
    let body_rows = (box_h as i32 - 3).max(1) as usize;
    let body_lines = help_lines().len().saturating_sub(1);
    let max_offset = body_lines.saturating_sub(body_rows) as u32;
    (body_rows, max_offset)
}

/// Y-coordinate of the scrollbar thumb element's origin (parent-local)
/// for a given scroll `offset`. Centralised so initial creation and
/// `UpdateOrigin`-driven scroll updates compute identical values.
fn help_thumb_origin_y(box_h: f32, offset: u32) -> f32 {
    let (body_rows, max_offset) = help_body_window(box_h);
    let body_lines = help_lines().len().saturating_sub(1);
    if body_lines <= body_rows || max_offset == 0 {
        return 2.0;
    }
    let track_h = body_rows as f32;
    let thumb_h = (track_h * body_rows as f32 / body_lines as f32).max(0.5);
    let thumb_max_off = (track_h - thumb_h).max(0.0);
    let off = offset.min(max_offset) as f32;
    2.0 + thumb_max_off * off / max_offset as f32
}

/// Build the help modal as a parent element (chrome + clip rect) plus
/// children for the body fill, the scrollable body lines, and — when
/// content overflows — a scrollbar track and thumb. Scrolling is handled
/// purely by shifting the body-lines and thumb children's origins, so
/// the body's draw commands never need to be rebuilt.
fn build_help_modal_elements(
    host_w: u32,
    host_h: u32,
    offset: u32,
    cell_pw: f32,
    cell_ph: f32,
) -> Vec<CreateElementBody> {
    let max_line = help_lines()
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(20) as f32;
    let inner_w = max_line.max(30.0);
    let box_w = (inner_w + 6.0).min(host_w.saturating_sub(2) as f32);
    let inner_h = help_lines().len() as f32;
    let box_h = (inner_h + 2.0).min(host_h.saturating_sub(2) as f32);

    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let (body_rows, max_offset) = help_body_window(box_h);
    let offset = offset.min(max_offset);
    let body_lines = help_lines().len().saturating_sub(1);
    let scrollable = body_lines > body_rows;

    let mut elements: Vec<CreateElementBody> = Vec::new();

    let (rx, ry) = chrome_corner_radii(box_w, box_h, cell_pw, cell_ph);

    // Parent — its `size` clips every child to the modal's bounds, and
    // its own `commands` (title strip, outline, title text) draw last,
    // on top of every child, so the title strip cleanly masks any body
    // line that scrolls into rows 0/1. Title strip rounds only its top
    // corners; the bottom seam meets the body fill cleanly.
    elements.push(CreateElementBody {
        id: MODAL_ELEMENT_ID.into(),
        commands: vec![
            DrawCmd::FillPath {
                fill: accent_style(),
                segments: rounded_rect_path_corners(
                    0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false,
                ),
            },
            DrawCmd::DrawText {
                origin: Point {
                    x: box_w * 0.5,
                    y: 0.0,
                },
                align: Align::Center,
                fill: Style::Flat(COLOR_MODAL_TEXT),
                font_style: FontStyle(0x01),
                text: help_lines()[0].to_string(),
            },
            DrawCmd::DrawLinePath {
                stroke: accent_style(),
                line_width: 0.1,
                segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
            },
        ],
        origin: Point {
            x: origin_x,
            y: origin_y,
        },
        is_visible: true,
        draw_order: MODAL_DRAW_ORDER,
        parent: None,
        size: Some(Point {
            x: box_w,
            y: box_h,
        }),
    });

    // Body fill — full rounded rect underneath the scrolling body lines.
    // The title strip drawn by the parent (above) covers its top region;
    // we rely on draw order rather than masking the body fill itself.
    elements.push(CreateElementBody {
        id: MODAL_BODY_FILL_ID.into(),
        commands: vec![DrawCmd::FillPath {
            fill: surface_style(),
            segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
        }],
        origin: Point { x: 0.0, y: 0.0 },
        is_visible: true,
        draw_order: 0,
        parent: Some(MODAL_ELEMENT_ID.into()),
        size: None,
    });

    // Body lines — every line drawn at its natural y position; the
    // element's origin shifts the whole stack up to scroll, and the
    // parent's clip rect hides what goes out of view.
    let mut body_cmds = Vec::with_capacity(body_lines);
    for i in 1..help_lines().len() {
        let line = &help_lines()[i];
        let bold = !line.is_empty() && !line.starts_with(' ');
        body_cmds.push(DrawCmd::DrawText {
            origin: Point {
                x: 3.0,
                y: 1.0 + i as f32,
            },
            align: Align::Left,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(if bold { 0x01 } else { 0x00 }),
            text: line.to_string(),
        });
    }
    elements.push(CreateElementBody {
        id: MODAL_BODY_LINES_ID.into(),
        commands: body_cmds,
        origin: Point {
            x: 0.0,
            y: -(offset as f32),
        },
        is_visible: true,
        draw_order: 1,
        parent: Some(MODAL_ELEMENT_ID.into()),
        size: None,
    });

    if scrollable {
        let track_h = body_rows as f32;
        let thumb_h = (track_h * body_rows as f32 / body_lines as f32).max(0.5);

        elements.push(CreateElementBody {
            id: MODAL_TRACK_ID.into(),
            commands: vec![DrawCmd::FillRectangles {
                fill: Style::Flat(COLOR_SCROLLBAR),
                rects: vec![Rect {
                    x: box_w - 1.0,
                    y: 2.0,
                    w: 0.4,
                    h: track_h,
                }],
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: 2,
            parent: Some(MODAL_ELEMENT_ID.into()),
            size: None,
        });

        // Thumb command is anchored at local (box_w-1, 0); the
        // element's origin places it at the right y, so scroll updates
        // are a single `UpdateOrigin` away.
        elements.push(CreateElementBody {
            id: MODAL_THUMB_ID.into(),
            commands: vec![DrawCmd::FillRectangles {
                fill: accent_style(),
                rects: vec![Rect {
                    x: box_w - 1.0,
                    y: 0.0,
                    w: 0.4,
                    h: thumb_h,
                }],
            }],
            origin: Point {
                x: 0.0,
                y: help_thumb_origin_y(box_h, offset),
            },
            is_visible: true,
            draw_order: 3,
            parent: Some(MODAL_ELEMENT_ID.into()),
            size: None,
        });
    }

    elements
}

// ─────────────────────────────────────────────────────────────────────────
// Input mode state machine
// ─────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum RenameTarget {
    Pane(String),
    Tab(usize),
}

/// Result of feeding input bytes to a [`LineEditor`].
enum EditOutcome {
    /// Nothing visible changed (e.g. an unrecognised / incomplete key).
    Noop,
    /// Buffer or cursor moved — the modal should be re-rendered.
    Redraw,
    /// Enter pressed — commit the current buffer.
    Commit,
    /// Esc / Ctrl+G pressed — abandon the edit.
    Cancel,
}

/// A single-line text editor with a readline-flavored keybinding set,
/// backing the rename modal. ASCII-only for now (non-ASCII / non-printable
/// input is dropped), so byte and char indices coincide; even so every
/// operation routes through char positions to stay UTF-8-ready.
#[derive(Debug)]
struct LineEditor {
    /// The edited text.
    buffer: String,
    /// Insertion point as a char index in `0..=char_count`.
    cursor: usize,
}

/// Max title length, in chars. Mirrors the historical rename cap.
const RENAME_MAX_CHARS: usize = 32;

impl LineEditor {
    /// Start editing `buffer` with the cursor at its end.
    fn new(buffer: String) -> Self {
        let cursor = buffer.chars().count();
        LineEditor { buffer, cursor }
    }

    fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Byte offset of char index `idx` (or buffer end if past the end).
    fn byte_offset(&self, idx: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(idx)
            .map(|(i, _)| i)
            .unwrap_or(self.buffer.len())
    }

    /// Remove chars in the half-open range `[a, b)`. Caller fixes up the
    /// cursor afterwards.
    fn delete_range(&mut self, a: usize, b: usize) {
        if a >= b {
            return;
        }
        self.buffer = self
            .buffer
            .chars()
            .enumerate()
            .filter(|(i, _)| *i < a || *i >= b)
            .map(|(_, c)| c)
            .collect();
    }

    fn insert(&mut self, c: char) {
        if self.char_count() >= RENAME_MAX_CHARS {
            return;
        }
        let at = self.byte_offset(self.cursor);
        self.buffer.insert(at, c);
        self.cursor += 1;
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    /// Char index of the previous word start (alphanumeric-delimited).
    fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut i = self.cursor;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    /// Char index of the next word end (alphanumeric-delimited).
    fn next_word(&self) -> usize {
        let chars: Vec<char> = self.buffer.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < n && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_range(self.cursor - 1, self.cursor);
            self.cursor -= 1;
        }
    }

    fn delete_forward(&mut self) {
        self.delete_range(self.cursor, self.cursor + 1);
    }

    fn kill_to_end(&mut self) {
        self.delete_range(self.cursor, self.char_count());
    }

    fn kill_to_start(&mut self) {
        self.delete_range(0, self.cursor);
        self.cursor = 0;
    }

    fn kill_word_back(&mut self) {
        let start = self.prev_word();
        self.delete_range(start, self.cursor);
        self.cursor = start;
    }

    fn kill_word_forward(&mut self) {
        let end = self.next_word();
        self.delete_range(self.cursor, end);
    }

    /// Consume one keystroke from the front of `bytes`, returning how many
    /// bytes were consumed and the resulting outcome. A whole escape
    /// sequence is consumed at once when one is present in this read, so a
    /// lone trailing Esc still reads as an immediate cancel.
    fn feed(&mut self, bytes: &[u8]) -> (usize, EditOutcome) {
        let Some(&b) = bytes.first() else {
            return (1, EditOutcome::Noop);
        };
        match b {
            b'\r' | b'\n' => (1, EditOutcome::Commit),
            0x1B => self.feed_escape(bytes),
            0x01 => {
                self.cursor = 0; // Ctrl+A
                (1, EditOutcome::Redraw)
            }
            0x05 => {
                self.cursor = self.char_count(); // Ctrl+E
                (1, EditOutcome::Redraw)
            }
            0x02 => {
                self.move_left(); // Ctrl+B
                (1, EditOutcome::Redraw)
            }
            0x06 => {
                self.move_right(); // Ctrl+F
                (1, EditOutcome::Redraw)
            }
            0x04 => {
                self.delete_forward(); // Ctrl+D
                (1, EditOutcome::Redraw)
            }
            0x08 | 0x7F => {
                self.backspace(); // Ctrl+H / DEL
                (1, EditOutcome::Redraw)
            }
            0x0B => {
                self.kill_to_end(); // Ctrl+K
                (1, EditOutcome::Redraw)
            }
            0x15 => {
                self.kill_to_start(); // Ctrl+U
                (1, EditOutcome::Redraw)
            }
            0x17 => {
                self.kill_word_back(); // Ctrl+W
                (1, EditOutcome::Redraw)
            }
            0x07 => (1, EditOutcome::Cancel), // Ctrl+G
            0x20..=0x7E => {
                self.insert(b as char);
                (1, EditOutcome::Redraw)
            }
            _ => (1, EditOutcome::Noop),
        }
    }

    /// Handle an ESC-introduced sequence: Alt-<key> bindings, CSI/SS3
    /// cursor keys, or a bare Esc (cancel). `bytes[0]` is `0x1B`.
    fn feed_escape(&mut self, bytes: &[u8]) -> (usize, EditOutcome) {
        // Lone Esc with nothing following in this read → cancel.
        let Some(&next) = bytes.get(1) else {
            return (1, EditOutcome::Cancel);
        };
        match next {
            b'b' | b'B' => {
                self.cursor = self.prev_word(); // Alt+B
                (2, EditOutcome::Redraw)
            }
            b'f' | b'F' => {
                self.cursor = self.next_word(); // Alt+F
                (2, EditOutcome::Redraw)
            }
            b'd' | b'D' => {
                self.kill_word_forward(); // Alt+D
                (2, EditOutcome::Redraw)
            }
            0x7F | 0x08 => {
                self.kill_word_back(); // Alt+Backspace
                (2, EditOutcome::Redraw)
            }
            b'[' | b'O' => self.feed_csi(bytes),
            // ESC + unrecognised byte: treat the ESC as a cancel and leave
            // the trailing byte for the next loop iteration.
            _ => (1, EditOutcome::Cancel),
        }
    }

    /// Handle a CSI/SS3 cursor-key sequence. `bytes[0..2]` is `ESC [` or
    /// `ESC O`.
    fn feed_csi(&mut self, bytes: &[u8]) -> (usize, EditOutcome) {
        // Scan to the final byte (0x40..=0x7E) of the sequence.
        let mut i = 2;
        while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            // Incomplete in this read — drop the partial prefix rather
            // than leak raw bytes; a split sequence is rare for these.
            return (bytes.len(), EditOutcome::Noop);
        }
        let params = &bytes[2..i];
        match bytes[i] {
            b'C' => self.move_right(),         // Right
            b'D' => self.move_left(),          // Left
            b'H' => self.cursor = 0,           // Home
            b'F' => self.cursor = self.char_count(), // End
            b'~' => match params {
                b"1" | b"7" => self.cursor = 0,
                b"4" | b"8" => self.cursor = self.char_count(),
                b"3" => self.delete_forward(), // Delete
                _ => {}
            },
            _ => {}
        }
        (i + 1, EditOutcome::Redraw)
    }
}

#[derive(Debug)]
enum Mode {
    Normal,
    /// Prefix key (Ctrl+Space) was pressed — next byte is interpreted as a
    /// vmux command.
    Prefix,
    /// Modal text editor for `prefix-r` (pane) or `prefix-R` (tab).
    /// Captures keystrokes until Enter (commit) or Esc (cancel). The
    /// `editor` carries a readline-flavored line-editing state (cursor +
    /// motion/kill bindings).
    Rename {
        target: RenameTarget,
        editor: LineEditor,
    },
    /// Help-modal display via `prefix-?`. Recognised navigation keys
    /// (`j`/`k`/Up/Down/PgUp/PgDn/`g`/`G`) scroll when the help text is
    /// taller than the modal; any other keystroke dismisses it.
    Help {
        offset: u32,
        /// Partial CSI sequence buffer — lets us recognise multi-byte
        /// arrow / PgUp / PgDn sequences without the leading ESC
        /// dismissing the modal prematurely.
        csi_buf: Vec<u8>,
    },
    /// Quit-confirmation modal shown by `prefix-q`. `y` confirms and
    /// exits; `n`/Esc cancels; any other key is ignored so a stray
    /// keystroke can neither quit nor dismiss the prompt.
    ConfirmQuit,
    /// Interactive pane-resize, entered via `prefix-s`. `h`/`j`/`k`/`l`
    /// (or arrow keys) nudge the focused pane's nearest divider; `q`/Esc/
    /// Enter exit. The focused pane's title shows a `[resize]` cue while
    /// active. `csi_buf` accumulates multi-byte arrow sequences so the
    /// leading ESC doesn't read as an exit.
    Resize {
        csi_buf: Vec<u8>,
    },
}

/// The prefix key byte. Defaults to Ctrl+Space (`0x00`); overridable via
/// `--prefix`/`-P`. Set once at startup, before the input loop runs, so a
/// relaxed atomic load is sufficient on the keystroke path.
static PREFIX_BYTE: AtomicU8 = AtomicU8::new(0x00);

/// Current prefix key byte.
fn prefix_byte() -> u8 {
    PREFIX_BYTE.load(Ordering::Relaxed)
}

/// Human-readable name for the current prefix key (e.g. `Ctrl+Space`,
/// `Ctrl+A`). Used in the help modal so it reflects `--prefix`.
fn prefix_name() -> String {
    match prefix_byte() {
        0x00 => "Ctrl+Space".to_string(),
        b @ 0x01..=0x1a => format!("Ctrl+{}", (b'A' + b - 1) as char),
        0x1b => "Ctrl+[".to_string(),
        0x1c => "Ctrl+\\".to_string(),
        0x1d => "Ctrl+]".to_string(),
        0x1e => "Ctrl+^".to_string(),
        0x1f => "Ctrl+_".to_string(),
        b => format!("0x{b:02x}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Multiplexer state
// ─────────────────────────────────────────────────────────────────────────

/// A vmux tab: a separate layout tree with its own focus and label.
/// Switching tabs toggles portal/chrome visibility — the portals
/// themselves are shared in `State.panes` and persist across tabs (their
/// inner shells keep running while the tab is hidden, per spec §6.5).
struct Tab {
    /// Numeric default ("1", "2", …). Never overwritten by renames or
    /// auto-titling; falls through as the last resort.
    title: String,
    /// Set by `prefix-R`. Wins over the active pane's effective title.
    /// Empty rename clears it.
    manual_title: Option<String>,
    layout: Layout,
    focus: String,
    /// `Some(pane_id)` while a pane is zoomed (filling the tab bounds and
    /// hiding its siblings); `None` otherwise. Survives tab switches.
    zoomed: Option<String>,
}

/// An in-flight mouse drag on a split divider. Captured on the left-press
/// that grabs a divider and held until release; each motion event recomputes
/// the dragged split's ratio from the pointer position. `bounds` is the
/// region the split was laid out in at grab time — it stays valid for the
/// whole drag because changing this split's own ratio never moves its
/// enclosing bounds.
#[derive(Debug, Clone)]
struct ResizeDrag {
    /// a/b descent steps from the active layout root to the dragged split.
    path: Vec<bool>,
    dir: SplitDir,
    bounds: PaneRect,
}

struct State {
    panes: HashMap<String, Pane>,
    tabs: Vec<Tab>,
    active_tab: usize,
    next_pane_id: u32,
    next_tab_id: u32,
    mode: Mode,
    host_w: u32,
    host_h: u32,
    cell_pw: f32,
    cell_ph: f32,
    quit: bool,
    /// Set by SIGWINCH handler — main loop drains it.
    needs_resize_check: bool,
    /// True while a modal element exists in the host's VGE table.
    modal_visible: bool,
    /// Currently-focused portal id, as last sent via `SetFocus`.
    last_focus_sent: Option<String>,
    /// Pane ids the host last saw as visible. Used to emit minimal
    /// `UpdateVisibility` deltas across tab switches.
    visible_panes: HashSet<String>,
    /// True once the tab bar VGE element has been created on the host.
    tabbar_created: bool,
    /// Index of the leftmost tab currently drawn in the tab bar. When
    /// the tabs don't all fit on row 0 the bar scrolls; this offset is
    /// nudged so the active tab stays visible (see `build_tabbar_commands`).
    tab_scroll: usize,
    /// True once the separators VGE element has been created on the
    /// host. Tracked so the first relayout emits a CreateElement (not a
    /// stale-DeleteElement-then-Create) and subsequent passes can
    /// idempotently replace the element.
    separators_created: bool,
    /// Outstanding `SetPortalScrollback` requests, keyed by request id
    /// → pane id. Lets the ack handler route the host's reply (which
    /// only carries `applied_lines` + `history_depth`, no pane id) back
    /// to the right pane's scroll state.
    pending_scrolls: HashMap<u32, String>,
    /// Monotonic counter for allocating scroll request ids. Starts at
    /// `SCROLL_REQUEST_ID_BASE` and increments per request.
    next_scroll_req_id: u32,
    /// `Some(name)` when the host answered the SES probe as a `vsd`
    /// session — drives the tab-bar session segment and enables
    /// `prefix-D`. `None` for a plain local `veter` host.
    session_name: Option<String>,
    /// `Some` while the user is dragging a split divider with the mouse.
    /// All mouse events route to the drag until the button releases.
    resize_drag: Option<ResizeDrag>,
}

impl State {
    fn new(host_w: u32, host_h: u32, cell_pw: f32, cell_ph: f32) -> Result<Self> {
        let id = "p1".to_string();
        let initial_tab = Tab {
            title: "1".to_string(),
            manual_title: None,
            layout: Layout::Leaf(id.clone()),
            focus: id.clone(),
            zoomed: None,
        };
        let mut s = Self {
            panes: HashMap::new(),
            tabs: vec![initial_tab],
            active_tab: 0,
            next_pane_id: 2,
            next_tab_id: 2,
            mode: Mode::Normal,
            host_w,
            host_h,
            cell_pw,
            cell_ph,
            quit: false,
            needs_resize_check: false,
            modal_visible: false,
            last_focus_sent: None,
            visible_panes: HashSet::new(),
            tabbar_created: false,
            tab_scroll: 0,
            separators_created: false,
            pending_scrolls: HashMap::new(),
            next_scroll_req_id: SCROLL_REQUEST_ID_BASE,
            session_name: None,
            resize_drag: None,
        };
        let (rows, cols) = inner_grid_for(s.full_bounds());
        let pty = PanePty::spawn(rows as u16, cols as u16)?;
        s.panes.insert(
            id.clone(),
            Pane {
                title: id,
                manual_title: None,
                osc_title: None,
                osc_icon: None,
                pty,
                last_rect: None,
                // Pre-record the inner size so the first `relayout_and_render`
                // doesn't redundantly TIOCSWINSZ — that would fire SIGWINCH at
                // the freshly-started shell, prompting some shells (notably
                // bash with `checkwinsize`) to redraw their PS1 and leak an
                // empty line into the pane.
                last_inner: Some((cols, rows)),
                inner_mouse_protocol: 0,
                scroll: None,
                activity: false,
            },
        );
        Ok(s)
    }

    fn full_bounds(&self) -> PaneRect {
        // Row 0 is reserved for the tab bar; panes live in rows 1..host_h.
        PaneRect {
            x: 0,
            y: 1,
            w: self.host_w,
            h: self.host_h.saturating_sub(1).max(1),
        }
    }

    fn focus(&self) -> &str {
        &self.tabs[self.active_tab].focus
    }

    fn set_focus(&mut self, id: String) {
        self.tabs[self.active_tab].focus = id;
    }

    fn layout_mut(&mut self) -> &mut Layout {
        &mut self.tabs[self.active_tab].layout
    }

    fn active_layout(&self) -> &Layout {
        &self.tabs[self.active_tab].layout
    }

    /// Set of pane ids that should be visible right now (= panes in the
    /// active tab's layout).
    fn active_pane_ids(&self) -> HashSet<String> {
        let mut leaves = Vec::new();
        self.tabs[self.active_tab].layout.collect_leaves(&mut leaves);
        leaves.into_iter().collect()
    }

    fn allocate_tab_id(&mut self) -> String {
        let id = format!("{}", self.next_tab_id);
        self.next_tab_id += 1;
        id
    }

    fn allocate_pane_id(&mut self) -> String {
        let id = format!("p{}", self.next_pane_id);
        self.next_pane_id += 1;
        id
    }

    fn split(&mut self, dir: SplitDir) -> Result<Vec<u8>> {
        // Splitting while zoomed is meaningless — the user wouldn't see
        // the new sibling. Exit zoom first; the relayout below covers it.
        self.tabs[self.active_tab].zoomed = None;
        let target = self.focus().to_string();
        let new_id = self.allocate_pane_id();
        if !self.layout_mut().split_leaf(&target, dir, &new_id) {
            return Ok(Vec::new());
        }

        // Compute the new pane's actual rect from the post-split layout so
        // we spawn its PTY at the right size on the first try. Spawning at
        // a placeholder size and resizing through TIOCSWINSZ would fire
        // SIGWINCH at a shell that hadn't yet drawn its PS1, leaking an
        // extra line into the pane.
        let mut rects = HashMap::new();
        layout_rects(self.active_layout(), self.full_bounds(), &mut rects);
        let rect = *rects
            .get(&new_id)
            .expect("new pane must appear in post-split layout");
        let (rows, cols) = inner_grid_for(rect);
        let pty = PanePty::spawn(rows as u16, cols as u16)?;
        self.panes.insert(
            new_id.clone(),
            Pane {
                title: new_id.clone(),
                manual_title: None,
                osc_title: None,
                osc_icon: None,
                pty,
                last_rect: None,
                last_inner: Some((cols, rows)),
                inner_mouse_protocol: 0,
                scroll: None,
                activity: false,
            },
        );
        // New pane gets focus.
        self.set_focus(new_id);
        self.relayout_and_render()
    }

    fn close_focused(&mut self) -> Result<Vec<u8>> {
        let target = self.focus().to_string();
        self.close_pane(&target)
    }

    /// Find which tab contains `target`, remove the leaf from that tab,
    /// emit the wire deltas (DeletePortal + chrome DeleteElement), and
    /// relayout. If the tab becomes empty, the tab is removed too; if
    /// the last tab goes, vmux quits.
    fn close_pane(&mut self, target: &str) -> Result<Vec<u8>> {
        // Drop any in-flight scroll acks for the pane we're tearing
        // down — once `DeletePortal` lands the host can't deliver them
        // anyway, and we don't want a stale ack to resurrect state on
        // a freshly-allocated pane id (unlikely but cheap to defend).
        self.pending_scrolls.retain(|_, pid| pid != target);
        let mut tab_idx: Option<usize> = None;
        for (i, tab) in self.tabs.iter().enumerate() {
            let mut leaves = Vec::new();
            tab.layout.collect_leaves(&mut leaves);
            if leaves.iter().any(|l| l == target) {
                tab_idx = Some(i);
                break;
            }
        }
        let Some(tab_idx) = tab_idx else {
            return Ok(Vec::new());
        };

        let result = self.tabs[tab_idx].layout.remove_leaf(target);
        let mut out = Vec::new();
        match result {
            RemoveResult::Empty => {
                self.panes.remove(target);
                self.visible_panes.remove(target);
                out.extend(build_prt_envelope(&[(
                    PrtCommand::DeletePortal { id: target.to_string() },
                    0,
                )]));
                out.extend(build_vge_envelope(&[(
                    VgeCommand::DeleteElement {
                        id: chrome_element_id(target),
                    },
                    0,
                )]));

                self.tabs.remove(tab_idx);
                if self.tabs.is_empty() {
                    self.quit = true;
                } else {
                    if self.active_tab == tab_idx {
                        // Active tab vanished — clamp.
                        if self.active_tab >= self.tabs.len() {
                            self.active_tab = self.tabs.len() - 1;
                        }
                    } else if self.active_tab > tab_idx {
                        self.active_tab -= 1;
                    }
                    out.extend(self.relayout_and_render()?);
                }
            }
            RemoveResult::Removed { new_focus } => {
                self.panes.remove(target);
                self.visible_panes.remove(target);
                if self.tabs[tab_idx].focus == target {
                    self.tabs[tab_idx].focus = new_focus;
                }
                // If the closed pane was the zoomed one, drop zoom so
                // the surviving siblings come back on the next relayout.
                if self.tabs[tab_idx].zoomed.as_deref() == Some(target) {
                    self.tabs[tab_idx].zoomed = None;
                }
                out.extend(build_prt_envelope(&[(
                    PrtCommand::DeletePortal { id: target.to_string() },
                    0,
                )]));
                out.extend(build_vge_envelope(&[(
                    VgeCommand::DeleteElement {
                        id: chrome_element_id(target),
                    },
                    0,
                )]));
                out.extend(self.relayout_and_render()?);
            }
            RemoveResult::NotFound => {}
        }
        Ok(out)
    }

    fn cycle_focus(&mut self) -> Result<Vec<u8>> {
        let mut leaves = Vec::new();
        self.tabs[self.active_tab].layout.collect_leaves(&mut leaves);
        if leaves.len() < 2 {
            return Ok(Vec::new());
        }
        let cur = self.focus().to_string();
        let pos = leaves.iter().position(|x| x == &cur).unwrap_or(0);
        let next = leaves[(pos + 1) % leaves.len()].clone();
        // Cycling out of a zoomed pane exits zoom (matches tmux): the
        // point of zoom is to see one pane fullscreen, so switching to
        // another implies the user wants the normal layout back.
        self.tabs[self.active_tab].zoomed = None;
        self.set_focus(next);
        // Re-emit chrome (focused pane changes color) and SetFocus.
        self.relayout_and_render()
    }

    /// Toggle pane-zoom on the active tab. With zoom on, the focused
    /// pane fills the whole tab and its siblings are hidden (their PTYs
    /// keep running). No-op when the tab has fewer than two panes —
    /// there's nothing to zoom over. Each tab carries its own zoom
    /// state, so switching tabs and back returns to the same zoom.
    fn toggle_zoom(&mut self) -> Result<Vec<u8>> {
        let idx = self.active_tab;
        if self.tabs[idx].zoomed.is_some() {
            self.tabs[idx].zoomed = None;
        } else {
            let mut leaves = Vec::new();
            self.tabs[idx].layout.collect_leaves(&mut leaves);
            if leaves.len() < 2 {
                return Ok(Vec::new());
            }
            let focus = self.focus().to_string();
            self.tabs[idx].zoomed = Some(focus);
        }
        self.relayout_and_render()
    }

    /// Focus `id` directly — the pointer-driven counterpart of
    /// `cycle_focus`. No-op when `id` is already focused or is not a
    /// pane in the active tab. Re-emits chrome and `SetFocus`.
    fn focus_pane(&mut self, id: &str) -> Result<Vec<u8>> {
        if self.focus() == id || !self.active_pane_ids().contains(id) {
            return Ok(Vec::new());
        }
        self.set_focus(id.to_string());
        self.relayout_and_render()
    }

    /// Enter interactive resize mode for the active tab. No-op when there
    /// is nothing to resize (a single pane, or the tab is zoomed so only
    /// one pane is visible). Re-emits the focused pane's chrome so the
    /// `[resize]` title cue shows immediately.
    fn enter_resize(&mut self) -> Result<Vec<u8>> {
        if self.tabs[self.active_tab].zoomed.is_some() {
            return Ok(Vec::new());
        }
        let mut leaves = Vec::new();
        self.active_layout().collect_leaves(&mut leaves);
        if leaves.len() < 2 {
            return Ok(Vec::new());
        }
        self.mode = Mode::Resize {
            csi_buf: Vec::new(),
        };
        let focus = self.focus().to_string();
        Ok(self.render_one_chrome(&focus))
    }

    /// Leave resize mode and clear the focused pane's `[resize]` cue.
    fn exit_resize(&mut self) -> Result<Vec<u8>> {
        self.mode = Mode::Normal;
        let focus = self.focus().to_string();
        Ok(self.render_one_chrome(&focus))
    }

    /// Nudge the focused pane's nearest divider one step in `dir`, moving
    /// the shared border the way the arrow points. Width (left/right) walks
    /// the nearest vertical split, height (up/down) the nearest horizontal
    /// one. So with the bottom pane focused, Up moves the border up and
    /// grows it; Down shrinks it. No-op (empty output) when there's no
    /// matching split to move.
    fn resize_focus(&mut self, dir: ResizeDir) -> Result<Vec<u8>> {
        if self.tabs[self.active_tab].zoomed.is_some() {
            return Ok(Vec::new());
        }
        let (want, cells) = match dir {
            ResizeDir::Left => (SplitDir::Vertical, -RESIZE_STEP),
            ResizeDir::Right => (SplitDir::Vertical, RESIZE_STEP),
            ResizeDir::Up => (SplitDir::Horizontal, -RESIZE_STEP),
            ResizeDir::Down => (SplitDir::Horizontal, RESIZE_STEP),
        };
        let target = self.focus().to_string();
        let bounds = self.full_bounds();
        if resize_split(self.layout_mut(), &target, want, cells, bounds) {
            self.relayout_and_render()
        } else {
            Ok(Vec::new())
        }
    }

    /// Recompute the dragged divider's ratio from the current pointer cell
    /// and relayout. Called for every mouse-motion event while a divider
    /// drag is active. Silently does nothing when the pointer hasn't moved
    /// the divider (same ratio) so we don't spam redundant relayouts.
    fn update_resize_drag(&mut self, col: i32, row: i32) -> Result<Vec<u8>> {
        let Some(drag) = self.resize_drag.clone() else {
            return Ok(Vec::new());
        };
        let new_ratio = match drag.dir {
            SplitDir::Vertical => {
                if drag.bounds.w < 2 {
                    return Ok(Vec::new());
                }
                ((col - drag.bounds.x) as f32 / drag.bounds.w as f32)
                    .clamp(MIN_RATIO, MAX_RATIO)
            }
            SplitDir::Horizontal => {
                if drag.bounds.h < 2 {
                    return Ok(Vec::new());
                }
                ((row - drag.bounds.y) as f32 / drag.bounds.h as f32)
                    .clamp(MIN_RATIO, MAX_RATIO)
            }
        };
        match self.layout_mut().ratio_at_path_mut(&drag.path) {
            Some(r) if (*r - new_ratio).abs() >= f32::EPSILON => *r = new_ratio,
            _ => return Ok(Vec::new()),
        }
        self.relayout_and_render()
    }

    /// Spawn a brand new tab containing one fresh pane and switch to it.
    fn new_tab(&mut self) -> Result<Vec<u8>> {
        let pane_id = self.allocate_pane_id();
        let tab_title = self.allocate_tab_id();
        let new_tab = Tab {
            title: tab_title,
            manual_title: None,
            layout: Layout::Leaf(pane_id.clone()),
            focus: pane_id.clone(),
            zoomed: None,
        };
        self.tabs.push(new_tab);
        self.active_tab = self.tabs.len() - 1;

        // Spawn the new pane's PTY at the correct size for the active
        // tab so we don't trigger a spurious initial SIGWINCH.
        let bounds = self.full_bounds();
        let (rows, cols) = inner_grid_for(bounds);
        let pty = PanePty::spawn(rows as u16, cols as u16)?;
        self.panes.insert(
            pane_id.clone(),
            Pane {
                title: pane_id,
                manual_title: None,
                osc_title: None,
                osc_icon: None,
                pty,
                last_rect: None,
                last_inner: Some((cols, rows)),
                inner_mouse_protocol: 0,
                scroll: None,
                activity: false,
            },
        );
        self.relayout_and_render()
    }

    fn goto_tab(&mut self, index: usize) -> Result<Vec<u8>> {
        if index >= self.tabs.len() || index == self.active_tab {
            return Ok(Vec::new());
        }
        // Per-pane scroll state survives tab switches: scrolled panes
        // stay scrolled in the background and the chrome indicator
        // returns when the user tabs back to that tab.
        self.active_tab = index;
        // The user is now looking at this tab — its panes are no
        // longer "background", so drop their activity markers.
        self.clear_tab_activity(index);
        self.relayout_and_render()
    }

    fn next_tab(&mut self) -> Result<Vec<u8>> {
        if self.tabs.len() < 2 {
            return Ok(Vec::new());
        }
        let next = (self.active_tab + 1) % self.tabs.len();
        self.goto_tab(next)
    }

    fn prev_tab(&mut self) -> Result<Vec<u8>> {
        if self.tabs.len() < 2 {
            return Ok(Vec::new());
        }
        let prev = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.goto_tab(prev)
    }

    /// Swap the active tab with the one to its left, wrapping the
    /// leftmost tab to the end. Focus follows the tab, so `active_tab`
    /// moves with it.
    fn move_tab_left(&mut self) -> Result<Vec<u8>> {
        if self.tabs.len() < 2 {
            return Ok(Vec::new());
        }
        let dst = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.tabs.swap(self.active_tab, dst);
        self.active_tab = dst;
        self.relayout_and_render()
    }

    /// Swap the active tab with the one to its right, wrapping the
    /// rightmost tab to position 0.
    fn move_tab_right(&mut self) -> Result<Vec<u8>> {
        if self.tabs.len() < 2 {
            return Ok(Vec::new());
        }
        let dst = (self.active_tab + 1) % self.tabs.len();
        self.tabs.swap(self.active_tab, dst);
        self.active_tab = dst;
        self.relayout_and_render()
    }

    /// Recompute the active tab's pane rects, dispatching the diffs as
    /// PRT (CreatePortal/UpdateSize/UpdateOrigin/UpdateVisibility) and
    /// VGE (chrome + tab bar) envelopes. Panes in non-active tabs are
    /// hidden. Idempotent.
    fn relayout_and_render(&mut self) -> Result<Vec<u8>> {
        // If the tab is zoomed, override the per-pane rects: only the
        // zoomed pane is laid out, filling the full bounds; siblings
        // get hidden via the `to_hide` loop below. Guard against a
        // stale `zoomed` referencing a pane that has since left the
        // layout (defensive — callers that remove panes clear zoom).
        let leaves_set = self.active_pane_ids();
        let zoomed: Option<String> = self.tabs[self.active_tab]
            .zoomed
            .as_ref()
            .filter(|z| leaves_set.contains(z.as_str()))
            .cloned();
        if zoomed.is_none() {
            self.tabs[self.active_tab].zoomed = None;
        }

        let mut rects = HashMap::new();
        if let Some(zid) = &zoomed {
            rects.insert(zid.clone(), self.full_bounds());
        } else {
            layout_rects(self.active_layout(), self.full_bounds(), &mut rects);
        }

        let mut prt_cmds: Vec<(PrtCommand, u32)> = Vec::new();
        let mut vge_cmds: Vec<(VgeCommand, u32)> = Vec::new();
        let focus = self.focus().to_string();
        // When zoomed, only the zoomed pane is "active" for the visibility
        // diff — its siblings exist in the layout but are off-screen.
        let active_set: HashSet<String> = rects.keys().cloned().collect();

        // Stable iteration order so the wire trace is deterministic.
        let mut ordered: Vec<String> = rects.keys().cloned().collect();
        ordered.sort();
        // Hide the per-pane title when the active tab has only one pane
        // (there's nothing to disambiguate). A scroll indicator still
        // surfaces because `display_title != raw_title` in that case.
        let single_pane = ordered.len() <= 1;

        for pane_id in &ordered {
            let rect = rects[pane_id];
            let (rows, cols) = inner_grid_for(rect);
            let (inner_x, inner_y) = inner_origin_for(rect);
            // Compute display title BEFORE borrowing pane mutably so we
            // don't fight the borrow checker over the `self.mode` read
            // inside `display_title_for`.
            let pane_title_raw = self
                .panes
                .get(pane_id)
                .map(|p| p.effective_title().to_string())
                .unwrap_or_default();
            let display_title = self.display_title_for(pane_id, &pane_title_raw);
            let show_title = !single_pane || display_title != pane_title_raw;
            let scroll_ind = self.scroll_indicator_for(pane_id);
            let pane = self.panes.get_mut(pane_id).expect("layout/panes mismatch");

            let create_portal = pane.last_rect.is_none();
            if create_portal {
                prt_cmds.push((
                    PrtCommand::CreatePortal(CreatePortalBody {
                        id: pane_id.clone(),
                        size_w: cols,
                        size_h: rows,
                        origin_x: inner_x,
                        origin_y: inner_y,
                        anchor_mode: AnchorMode::Live,
                        is_visible: true,
                        draw_order: PORTAL_DRAW_ORDER,
                        flags: 0,
                        // Reasonable scrollback so content scrolled away
                        // by tall vcat images / long output is recoverable
                        // and so VGE elements with negative `origin_y`
                        // (anchored above the live region — see vcat's
                        // tall-image case) don't immediately evict.
                        scrollback_lines: PORTAL_SCROLLBACK_LINES,
                    }),
                    0,
                ));
            } else {
                let prev = pane.last_rect.unwrap();
                if pane.last_inner != Some((cols, rows)) {
                    prt_cmds.push((
                        PrtCommand::UpdateSize {
                            id: pane_id.clone(),
                            new_w: cols,
                            new_h: rows,
                        },
                        0,
                    ));
                }
                let (px, py) = inner_origin_for(prev);
                if (px, py) != (inner_x, inner_y) {
                    prt_cmds.push((
                        PrtCommand::UpdateOrigin(UpdateOriginBody {
                            id: pane_id.clone(),
                            new_origin_x: inner_x,
                            new_origin_y: inner_y,
                            anchor_mode: AnchorMode::Live,
                        }),
                        0,
                    ));
                }
            }

            // Make sure this pane's portal+chrome are visible (it's in
            // the active tab). Emit only on changes.
            if !self.visible_panes.contains(pane_id) {
                prt_cmds.push((
                    PrtCommand::UpdateVisibility {
                        id: pane_id.clone(),
                        is_visible: true,
                    },
                    0,
                ));
                vge_cmds.push((
                    VgeCommand::UpdateVisibility {
                        id: chrome_element_id(pane_id),
                        is_visible: true,
                    },
                    0,
                ));
            }

            // Re-create chrome each layout pass — DeleteElement on a
            // fresh pane is a no-op error which we just absorb.
            let chrome_id = chrome_element_id(pane_id);
            let cmds = build_chrome_commands(
                rect,
                &display_title,
                pane_id == &focus,
                show_title,
                self.cell_pw,
                self.cell_ph,
                scroll_ind,
            );
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: chrome_id.clone(),
                },
                0,
            ));
            vge_cmds.push((
                VgeCommand::CreateElement(CreateElementBody {
                    id: chrome_id,
                    commands: cmds,
                    origin: Point {
                        x: rect.x as f32,
                        y: rect.y as f32,
                    },
                    is_visible: true,
                    draw_order: CHROME_DRAW_ORDER,
                    parent: None,
                    size: None,
                }),
                0,
            ));

            // Only signal SIGWINCH to the inner shell when the inner
            // grid genuinely changed. Re-resizing on every focus or
            // rename pass would re-fire SIGWINCH and prompt many shells
            // to redraw their PS1, leaking an extra blank line into the
            // pane after every command.
            if pane.last_inner != Some((cols, rows)) {
                pane.pty.resize(rows as u16, cols as u16);
            }
            pane.last_rect = Some(rect);
            pane.last_inner = Some((cols, rows));
        }

        // Hide any portal+chrome that was visible last time but isn't in
        // the active tab now (i.e. tab switched away from it).
        let to_hide: Vec<String> = self
            .visible_panes
            .iter()
            .filter(|id| !active_set.contains(id.as_str()))
            .cloned()
            .collect();
        for pane_id in &to_hide {
            prt_cmds.push((
                PrtCommand::UpdateVisibility {
                    id: pane_id.clone(),
                    is_visible: false,
                },
                0,
            ));
            vge_cmds.push((
                VgeCommand::UpdateVisibility {
                    id: chrome_element_id(pane_id),
                    is_visible: false,
                },
                0,
            ));
        }
        self.visible_panes = active_set;

        // Focus update — always emit so an unrelated focus change still
        // shows up. Only valid when the focused pane is currently visible.
        if self.visible_panes.contains(&focus)
            && self.last_focus_sent.as_deref() != Some(&focus)
        {
            prt_cmds.push((
                PrtCommand::SetFocus {
                    target: FocusTarget::Portal(focus.clone()),
                },
                0,
            ));
            self.last_focus_sent = Some(focus.clone());
        }

        // Between-pane separators for the active tab. Rebuilt every
        // relayout — the layout tree determines which split boundaries
        // need a stroke. While zoomed, the visible layout is a single
        // leaf, so feed a leaf to `build_separators_body` and the
        // emitted element has no commands.
        let zoom_leaf = Layout::Leaf(String::new());
        let sep_layout_ref: &Layout = if zoomed.is_some() {
            &zoom_leaf
        } else {
            self.active_layout()
        };
        let sep_body = build_separators_body(sep_layout_ref, self.full_bounds());
        if self.separators_created {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: SEPARATORS_ELEMENT_ID.into(),
                },
                0,
            ));
        }
        vge_cmds.push((VgeCommand::CreateElement(sep_body), 0));
        self.separators_created = true;

        // Tab bar: re-create on every relayout so tab adds/removes,
        // active-tab changes, and renames all show up. Cheap — single
        // small VGE element.
        let tabbar_labels = self.tabbar_labels();
        let tabbar_activity = self.tab_activity_flags();
        let tabbar_body = build_tabbar_commands(
            &tabbar_labels,
            self.active_tab,
            &mut self.tab_scroll,
            self.host_w,
            self.cell_pw,
            self.cell_ph,
            self.session_name.as_deref(),
            &tabbar_activity,
        );
        if self.tabbar_created {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: TABBAR_ELEMENT_ID.into(),
                },
                0,
            ));
        }
        vge_cmds.push((VgeCommand::CreateElement(tabbar_body), 0));
        self.tabbar_created = true;

        // Re-emit modal on top, if active.
        let modal_elements = self.build_modal_elements();
        if !modal_elements.is_empty() {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: MODAL_ELEMENT_ID.into(),
                },
                0,
            ));
            for el in modal_elements {
                vge_cmds.push((VgeCommand::CreateElement(el), 0));
            }
            self.modal_visible = true;
        } else if self.modal_visible {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: MODAL_ELEMENT_ID.into(),
                },
                0,
            ));
            self.modal_visible = false;
        }

        let mut out = Vec::new();
        if !prt_cmds.is_empty() {
            out.extend(build_prt_envelope(&prt_cmds));
        }
        if !vge_cmds.is_empty() {
            out.extend(build_vge_envelope(&vge_cmds));
        }
        Ok(out)
    }

    fn render_modal_overlay(&mut self) -> Vec<u8> {
        let mut vge_cmds: Vec<(VgeCommand, u32)> = Vec::new();
        let elements = self.build_modal_elements();
        if !elements.is_empty() {
            // Deleting the parent cascades to every child (§9.6), so a
            // single delete clears whatever modal was up before.
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: MODAL_ELEMENT_ID.into(),
                },
                0,
            ));
            for el in elements {
                vge_cmds.push((VgeCommand::CreateElement(el), 0));
            }
            self.modal_visible = true;
        } else if self.modal_visible {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: MODAL_ELEMENT_ID.into(),
                },
                0,
            ));
            self.modal_visible = false;
        }
        if vge_cmds.is_empty() {
            Vec::new()
        } else {
            build_vge_envelope(&vge_cmds)
        }
    }

    /// Scroll-only update for the help modal: shift the body-lines
    /// child's origin and the thumb's origin in place. No element is
    /// recreated, no draw commands are rebuilt — this is the showcase
    /// for VGE's parent/clip + per-element origin features.
    fn render_help_scroll(&self) -> Vec<u8> {
        let Mode::Help { offset, .. } = &self.mode else {
            return Vec::new();
        };
        let inner_h = help_lines().len() as f32;
        let box_h = (inner_h + 2.0).min(self.host_h.saturating_sub(2) as f32);
        let (body_rows, max_offset) = help_body_window(box_h);
        let body_lines = help_lines().len().saturating_sub(1);
        if body_lines <= body_rows {
            return Vec::new();
        }
        let off = (*offset).min(max_offset);
        let cmds = vec![
            (
                VgeCommand::UpdateOrigin {
                    id: MODAL_BODY_LINES_ID.into(),
                    origin: Point {
                        x: 0.0,
                        y: -(off as f32),
                    },
                },
                0,
            ),
            (
                VgeCommand::UpdateOrigin {
                    id: MODAL_THUMB_ID.into(),
                    origin: Point {
                        x: 0.0,
                        y: help_thumb_origin_y(box_h, off),
                    },
                },
                0,
            ),
        ];
        build_vge_envelope(&cmds)
    }

    /// Build the modal as a list of elements (parent first, children
    /// after) for whichever overlay the current mode wants up — rename
    /// prompt or help — or empty if no modal should be visible.
    fn build_modal_elements(&self) -> Vec<CreateElementBody> {
        match &self.mode {
            Mode::Rename { target, editor } => {
                let title = match target {
                    RenameTarget::Pane(_) => "Rename pane",
                    RenameTarget::Tab(_) => "Rename tab",
                };
                vec![build_modal_commands(
                    self.host_w,
                    self.host_h,
                    title,
                    &editor.buffer,
                    self.cell_pw,
                    self.cell_ph,
                    Some(editor.cursor),
                )]
            }
            Mode::Help { offset, .. } => build_help_modal_elements(
                self.host_w,
                self.host_h,
                *offset,
                self.cell_pw,
                self.cell_ph,
            ),
            Mode::ConfirmQuit => vec![build_modal_commands(
                self.host_w,
                self.host_h,
                "Quit vmux?",
                "y = quit     n = cancel",
                self.cell_pw,
                self.cell_ph,
                None,
            )],
            // Resize shows its cue in the focused pane's title, not a
            // center modal — a center box would hide the layout the user
            // is adjusting.
            Mode::Normal | Mode::Prefix | Mode::Resize { .. } => Vec::new(),
        }
    }

    /// Title to display in `pane_id`'s chrome — usually the pane's own
    /// label, but a `[scroll: N]` indicator while that pane is being
    /// scrolled.
    fn display_title_for(&self, pane_id: &str, raw_title: &str) -> String {
        if let Some(s) = self.panes.get(pane_id).and_then(|p| p.scroll.as_ref()) {
            return format!("[scroll: {}]", s.offset);
        }
        if self.tabs[self.active_tab].zoomed.as_deref() == Some(pane_id) {
            return format!("Z  {}", raw_title);
        }
        // While resizing, mark the focused pane so the mode is visible
        // even though no center modal is shown.
        if matches!(self.mode, Mode::Resize { .. }) && self.focus() == pane_id {
            return format!("[resize]  {}", raw_title);
        }
        raw_title.to_string()
    }

    /// Scroll-indicator state to draw a thumb in `pane_id`'s chrome,
    /// or `None` if we're not currently scrolling that pane.
    fn scroll_indicator_for(&self, pane_id: &str) -> Option<ScrollIndicator> {
        let s = self.panes.get(pane_id)?.scroll.as_ref()?;
        Some(ScrollIndicator {
            offset: s.offset,
            history_depth: s.history_depth,
        })
    }

    /// Allocate a fresh request id for a `SetPortalScrollback` command
    /// targeting `pane_id`, register it in `pending_scrolls`, and
    /// return the id. The ack handler uses the registration to route
    /// the response back to the right pane.
    fn alloc_scroll_request(&mut self, pane_id: &str) -> u32 {
        let id = self.next_scroll_req_id;
        self.next_scroll_req_id = self.next_scroll_req_id.wrapping_add(1);
        // Wrap around back into the reserved high half — we never need
        // to overlap with rid=0 (used for fire-and-forget commands)
        // and we want the ack handler's filter to keep working.
        if self.next_scroll_req_id < SCROLL_REQUEST_ID_BASE {
            self.next_scroll_req_id = SCROLL_REQUEST_ID_BASE;
        }
        self.pending_scrolls.insert(id, pane_id.to_string());
        id
    }

    /// Begin scrollback navigation for `pane_id`. No-op if that pane is
    /// already scrolling (so re-pressing `prefix-[` is harmless). Caches
    /// the half-page step and re-emits chrome so the title turns into
    /// the scroll indicator.
    fn enter_scroll(&mut self, pane_id: &str) -> Result<Vec<u8>> {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return Ok(Vec::new());
        };
        if pane.scroll.is_some() {
            return Ok(Vec::new());
        }
        let rows = pane.last_inner.map(|(_c, r)| r).unwrap_or(10);
        let half_page = (rows / 2).max(1);
        pane.scroll = Some(PaneScroll {
            offset: 0,
            history_depth: 0,
            half_page,
            csi_buf: Vec::new(),
        });
        // Probe the host once for the current history depth so the
        // scrollbar shows up right away (offset stays 0).
        let req_id = self.alloc_scroll_request(pane_id);
        let mut out = build_prt_envelope(&[(
            PrtCommand::SetPortalScrollback {
                id: pane_id.to_string(),
                lines: 0,
            },
            req_id,
        )]);
        out.extend(self.render_one_chrome(pane_id));
        Ok(out)
    }

    /// End scrollback navigation for `pane_id`: reset the host-side
    /// offset to live and clear the pane's local scroll state.
    fn exit_scroll(&mut self, pane_id: &str) -> Result<Vec<u8>> {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return Ok(Vec::new());
        };
        if pane.scroll.is_none() {
            return Ok(Vec::new());
        }
        pane.scroll = None;
        let req_id = self.alloc_scroll_request(pane_id);
        let mut out = build_prt_envelope(&[(
            PrtCommand::SetPortalScrollback {
                id: pane_id.to_string(),
                lines: 0,
            },
            req_id,
        )]);
        out.extend(self.render_one_chrome(pane_id));
        Ok(out)
    }

    /// Apply a delta to `pane_id`'s scroll offset (positive = scroll
    /// further back, negative = closer to live). Clamped to
    /// `[0, PORTAL_SCROLLBACK_LINES]`. Silent no-op if the pane isn't
    /// in scroll mode.
    fn scroll_delta(&mut self, pane_id: &str, delta: i64) -> Result<Vec<u8>> {
        let new_offset = match self.panes.get(pane_id).and_then(|p| p.scroll.as_ref()) {
            Some(s) => {
                let cur = s.offset as i64;
                let next = (cur + delta).max(0);
                (next.min(PORTAL_SCROLLBACK_LINES as i64)) as u32
            }
            None => return Ok(Vec::new()),
        };
        self.scroll_set(pane_id, new_offset)
    }

    /// Jump `pane_id` to an absolute scroll offset.
    fn scroll_set(&mut self, pane_id: &str, mut offset: u32) -> Result<Vec<u8>> {
        if offset > PORTAL_SCROLLBACK_LINES {
            offset = PORTAL_SCROLLBACK_LINES;
        }
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return Ok(Vec::new());
        };
        let Some(s) = pane.scroll.as_mut() else {
            return Ok(Vec::new());
        };
        if s.offset == offset {
            return Ok(Vec::new());
        }
        s.offset = offset;
        let req_id = self.alloc_scroll_request(pane_id);
        let mut out = build_prt_envelope(&[(
            PrtCommand::SetPortalScrollback {
                id: pane_id.to_string(),
                lines: offset,
            },
            req_id,
        )]);
        out.extend(self.render_one_chrome(pane_id));
        Ok(out)
    }

    /// Re-emit only the chrome for one specific pane (e.g. after rename).
    fn render_one_chrome(&mut self, pane_id: &str) -> Vec<u8> {
        // Only the active tab's panes have visible chrome. A pane in a
        // background tab can still trigger this path — a late
        // `SetPortalScrollback` ack or an OSC title change — and the
        // `CreateElement` below is unconditionally `is_visible: true`,
        // which would re-show that pane's chrome (e.g. its scroll
        // indicator) on top of whatever tab is currently displayed.
        // Skip it: `relayout_and_render` rebuilds the chrome from
        // scratch when the user switches back to that tab.
        if !self.visible_panes.contains(pane_id) {
            return Vec::new();
        }
        let (rect, title_raw) = match self.panes.get(pane_id) {
            Some(pane) => match pane.last_rect {
                Some(rect) => (rect, pane.effective_title().to_string()),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        let display_title = self.display_title_for(pane_id, &title_raw);
        let scroll_ind = self.scroll_indicator_for(pane_id);
        // Match the same single-pane suppression as the relayout path:
        // a one-pane tab hides the title unless display_title is in
        // scroll-indicator form (different from the raw label).
        let mut leaves = Vec::new();
        self.tabs[self.active_tab].layout.collect_leaves(&mut leaves);
        let single_pane = leaves.len() <= 1;
        let show_title = !single_pane || display_title != title_raw;
        let cmds = build_chrome_commands(
            rect,
            &display_title,
            pane_id == self.focus(),
            show_title,
            self.cell_pw,
            self.cell_ph,
            scroll_ind,
        );
        let chrome_id = chrome_element_id(pane_id);
        let vge_cmds = vec![
            (
                VgeCommand::DeleteElement {
                    id: chrome_id.clone(),
                },
                0,
            ),
            (
                VgeCommand::CreateElement(CreateElementBody {
                    id: chrome_id,
                    commands: cmds,
                    origin: Point {
                        x: rect.x as f32,
                        y: rect.y as f32,
                    },
                    is_visible: true,
                    draw_order: CHROME_DRAW_ORDER,
                    parent: None,
                    size: None,
                }),
                0,
            ),
        ];
        build_vge_envelope(&vge_cmds)
    }

    /// Effective tab title: manual rename wins; otherwise the active
    /// pane's effective title; otherwise the numeric default ("1", …).
    fn tab_effective_title(&self, tab_idx: usize) -> String {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return String::new();
        };
        if let Some(name) = &tab.manual_title {
            return name.clone();
        }
        if let Some(pane) = self.panes.get(&tab.focus) {
            // Only use the pane's title when it's been customised
            // (manual rename or OSC). The default `pN` label would
            // bubble up as e.g. "p3" which is meaningless on a tab.
            if pane.manual_title.is_some()
                || pane.osc_title.is_some()
                || pane.osc_icon.is_some()
            {
                return pane.effective_title().to_string();
            }
        }
        tab.title.clone()
    }

    /// Whether any pane in tab `tab_idx` carries the sticky activity
    /// flag (set by PRT `EVT_PORTAL_ACTIVITY` for a background pane).
    fn tab_activity(&self, tab_idx: usize) -> bool {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return false;
        };
        let mut leaves = Vec::new();
        tab.layout.collect_leaves(&mut leaves);
        leaves
            .iter()
            .any(|id| self.panes.get(id).is_some_and(|p| p.activity))
    }

    /// Clear the activity flag on every pane in tab `tab_idx`. Called
    /// when the tab is activated — the user is now looking at it.
    fn clear_tab_activity(&mut self, tab_idx: usize) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let mut leaves = Vec::new();
        tab.layout.collect_leaves(&mut leaves);
        for id in leaves {
            if let Some(p) = self.panes.get_mut(&id) {
                p.activity = false;
            }
        }
    }

    /// Tab-bar labels: each tab's effective title, verbatim. The
    /// activity indicator is rendered as a highlight on the tab number
    /// (see `tab_activity_flags`), not baked into the label text.
    fn tabbar_labels(&self) -> Vec<String> {
        (0..self.tabs.len())
            .map(|i| self.tab_effective_title(i))
            .collect()
    }

    /// Per-tab activity flags for the tab bar — `true` when a tab
    /// other than the active one has a pane with unseen activity.
    fn tab_activity_flags(&self) -> Vec<bool> {
        (0..self.tabs.len())
            .map(|i| i != self.active_tab && self.tab_activity(i))
            .collect()
    }

    /// Re-emit just the tab bar element. Used when an OSC title or a
    /// rename changes the active pane's effective title without
    /// otherwise affecting layout.
    fn render_tabbar(&mut self) -> Vec<u8> {
        let labels = self.tabbar_labels();
        let activity = self.tab_activity_flags();
        let body = build_tabbar_commands(
            &labels,
            self.active_tab,
            &mut self.tab_scroll,
            self.host_w,
            self.cell_pw,
            self.cell_ph,
            self.session_name.as_deref(),
            &activity,
        );
        let mut cmds = Vec::new();
        if self.tabbar_created {
            cmds.push((
                VgeCommand::DeleteElement {
                    id: TABBAR_ELEMENT_ID.into(),
                },
                0,
            ));
        }
        cmds.push((VgeCommand::CreateElement(body), 0));
        self.tabbar_created = true;
        build_vge_envelope(&cmds)
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Terminal / TTY plumbing
// ─────────────────────────────────────────────────────────────────────────

/// SIGWINCH flag — set by the signal handler, drained by the main loop.
static WINCH_FLAG: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: libc::c_int) {
    WINCH_FLAG.store(true, Ordering::SeqCst);
}

fn install_winch_handler() -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(handle_sigwinch),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe { sigaction(Signal::SIGWINCH, &action) }.context("sigaction(SIGWINCH)")?;
    Ok(())
}

struct TtyGuard {
    fd: RawFd,
    saved: Option<nix::sys::termios::Termios>,
    in_alt: bool,
}

impl TtyGuard {
    fn enable() -> Result<Self> {
        use nix::sys::termios::{
            tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg,
        };
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        let saved = tcgetattr(&stdin).context("tcgetattr")?;
        let mut raw = saved.clone();
        raw.local_flags &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ISIG);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.input_flags &= !(InputFlags::IXON
            | InputFlags::IXOFF
            | InputFlags::INLCR
            | InputFlags::ICRNL
            | InputFlags::IGNCR);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr (raw)")?;
        Ok(Self {
            fd,
            saved: Some(saved),
            in_alt: false,
        })
    }

    fn enter_alt_screen(&mut self) -> Result<()> {
        // Alt screen + hide cursor + clear. Then enable SGR-encoded
        // mouse reporting (DECSET 1002 + 1006) so veter forwards every
        // wheel/click/drag to us; 1002 (button-event tracking) adds
        // motion-while-pressed on top of 1000's press/release, which is
        // what powers divider drag-to-resize. We hit-test against pane
        // bounds in `handle_mouse_event` and either drive a resize drag,
        // drive scrollback, or re-encode and forward to the inner
        // program's PTY.
        write_all_stdout(
            b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h",
        )?;
        self.in_alt = true;
        Ok(())
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        if self.in_alt {
            // Disable mouse reporting first so leftover wheel events
            // don't leak into the outer shell, then leave alt screen
            // and re-show the cursor.
            let _ = write_all_stdout(
                b"\x1b[?1006l\x1b[?1002l\x1b[?1049l\x1b[?25h",
            );
        }
        if let Some(saved) = self.saved.take() {
            use nix::sys::termios::{tcsetattr, SetArg};
            let _ = unsafe {
                let borrowed = BorrowedFd::borrow_raw(self.fd);
                tcsetattr(borrowed, SetArg::TCSANOW, &saved)
            };
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Buffered, non-blocking stdout
//
// Output to the host (PRT/VGE envelopes) used to be written with a blocking
// `write_all` + `flush` on every call. Over a slow ssh link that flush stalls
// until the ssh upload window accepts the bytes, and because vmux's event loop
// is single-threaded the stall freezes input handling too — the prefix key and
// pane switches stop responding while a chatty pane floods output. To avoid
// that, all stdout goes through `OutQueue`: writes are appended to an in-memory
// buffer and drained opportunistically; the main loop polls the stdout fd for
// `POLLOUT` whenever the buffer is non-empty and keeps servicing input in the
// meantime. See doc note in `main()` for the matching backpressure cap.
// ─────────────────────────────────────────────────────────────────────────

/// Compact the queue (drop already-written bytes from the front) once the
/// written prefix grows past this, so sustained partial draining can't leak.
const OUT_COMPACT_THRESHOLD: usize = 64 * 1024;

/// Once this many bytes are queued for the host, stop reading pane PTYs so the
/// inner shells block instead of vmux buffering without bound (backpressure).
const OUT_HIGH_WATER: usize = 512 * 1024;

struct OutQueue {
    fd: RawFd,
    buf: Vec<u8>,
    head: usize,
}

impl OutQueue {
    fn new() -> Self {
        OutQueue {
            fd: std::io::stdout().as_raw_fd(),
            buf: Vec::new(),
            head: 0,
        }
    }

    fn enqueue(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Try to drain the queue to the fd. On a non-blocking fd this stops at
    /// the first `EAGAIN`, leaving the remainder queued for the next
    /// `POLLOUT`; on a blocking fd (probe + teardown phases) it writes
    /// everything. Returns `Err` only on a real write error.
    fn flush(&mut self) -> Result<()> {
        while self.head < self.buf.len() {
            let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
            match nix::unistd::write(borrowed, &self.buf[self.head..]) {
                Ok(0) => break,
                Ok(n) => self.head += n,
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => break,
                Err(e) => return Err(anyhow!("stdout write: {e}")),
            }
        }
        if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
        } else if self.head >= OUT_COMPACT_THRESHOLD {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        Ok(())
    }

    fn pending(&self) -> usize {
        self.buf.len() - self.head
    }
}

thread_local! {
    static OUT: RefCell<OutQueue> = RefCell::new(OutQueue::new());
}

/// Queue `bytes` for the host and attempt an immediate (non-blocking) drain.
/// Replaces the former blocking `write_all`+`flush`; the main loop finishes
/// draining via `POLLOUT`. During the blocking probe/teardown phases the fd is
/// blocking, so the drain here completes the write in full just like before.
fn write_all_stdout(bytes: &[u8]) -> Result<()> {
    OUT.with(|q| {
        let mut q = q.borrow_mut();
        q.enqueue(bytes);
        q.flush()
    })
}

/// Bytes still buffered for the host (0 once fully drained).
fn out_pending() -> usize {
    OUT.with(|q| q.borrow().pending())
}

/// Drain whatever the `POLLOUT` poll says is writable.
fn out_flush() -> Result<()> {
    OUT.with(|q| q.borrow_mut().flush())
}

fn set_stdout_nonblocking(nonblocking: bool) -> Result<()> {
    let fd = std::io::stdout().as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        bail!("fcntl(F_GETFL): {}", std::io::Error::last_os_error());
    }
    let new = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFL, new) } < 0 {
        bail!("fcntl(F_SETFL): {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn get_host_winsize() -> Result<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdin().as_raw_fd();
    let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if r != 0 {
        bail!("TIOCGWINSZ failed");
    }
    if ws.ws_row == 0 || ws.ws_col == 0 {
        bail!("host reports zero rows/cols");
    }
    Ok((ws.ws_row, ws.ws_col))
}

// ─────────────────────────────────────────────────────────────────────────
// Probe helpers (PRT + VGE)
// ─────────────────────────────────────────────────────────────────────────

/// Run a synchronous PRT probe and return whether a probe response was
/// observed within `timeout`. Body is otherwise ignored — vmux uses its
/// own conservative limits.
struct VgeProbeData {
    cell_pw: u16,
    cell_ph: u16,
}

/// Outcome of the startup probe round: which extensions the host
/// speaks, plus the session name when the host is a `vsd` session.
struct ProbeResults {
    /// Host advertised the Portal Extension (a hard requirement).
    prt_ok: bool,
    /// Host pre-populates the reserved `host.*` VGE style namespace
    /// (PRT probe `vge_features` bit, §10). When set, chrome uses
    /// `StyleRef("host.accent")`; otherwise it falls back to `COLOR_BRAND`.
    host_themed_styles: bool,
    /// The accent `host.accent` resolves to at this vmux's depth, when the
    /// host themes `host.*`. Used to derive shades (thumb, surface).
    accent_rgba: Option<[u8; 4]>,
    /// VGE probe answer, if any (cell pixel metrics).
    vge: Option<VgeProbeData>,
    /// `Some(name)` when the host answered the SES probe `in_session`.
    session_name: Option<String>,
}

/// Parse a PRT ProbeResponse payload. Returns `(host_themed_styles,
/// accent_rgba)`, or `None` if the payload is not a probe response. A body
/// too short to contain the trailing `vge_features`/accent fields reads
/// them as absent per §2.1 (missing trailing fields = zero).
fn parse_prt_probe(payload: &[u8]) -> Option<(bool, Option<[u8; 4]>)> {
    let mut r = PrtReader::new(payload);
    let _ = r.u8(); // payload protocol_version
    let _ = r.u32(); // payload_length
    let ft = r.u8().ok()?; // frame_type
    if ft != PRT_RSP_PROBE {
        return None;
    }
    let _ = r.u32(); // request_id
    let _ = r.u32(); // body_length
    // ProbeBody (§2.1).
    let _ = r.u16(); // protocol_version
    let _ = r.u32(); // max_portals
    let _ = r.u32(); // max_portal_cells_w
    let _ = r.u32(); // max_portal_cells_h
    let _ = r.u32(); // max_scrollback_lines
    let _ = r.u32(); // max_write_bytes
    let _ = r.u8(); // features
    let _ = r.u8(); // max_nesting_depth
    let vge_features = r.u8().unwrap_or(0); // trailing §10 byte
    let themed = vge_features & FEAT_VGE_HOST_THEMED_STYLES != 0;
    // The accent RGBA follows `vge_features` only when the host themes.
    let accent = if themed {
        match (r.u8(), r.u8(), r.u8(), r.u8()) {
            (Ok(red), Ok(green), Ok(blue), Ok(alpha)) => Some([red, green, blue, alpha]),
            _ => None,
        }
    } else {
        None
    };
    Some((themed, accent))
}

/// Parse one decoded VGE response payload, returning probe metrics if
/// it is a ProbeResponse frame.
fn parse_vge_probe(payload: &[u8]) -> Option<VgeProbeData> {
    let mut r = vge_protocol::codec::Reader::new(payload);
    let _ = r.u8();
    let _ = r.u32();
    let ft = r.u8().ok()?;
    if ft != VGE_RSP_PROBE {
        return None;
    }
    let _ = r.u32();
    let _ = r.u32();
    let _proto = r.u16().ok();
    let cw = r.u16().unwrap_or(9);
    let ch = r.u16().unwrap_or(20);
    Some(VgeProbeData {
        cell_pw: cw,
        cell_ph: ch,
    })
}

/// Probe PRT, VGE and SES in a single timeout window. All three probe
/// envelopes are written up front; responses are collected until every
/// extension has answered or the deadline passes — so a host that does
/// not speak SES (an older `veter`/`vsd`) costs no extra latency
/// beyond the shared window, and a missing SES answer is simply "no
/// session", never fatal. Each APC parser ignores the other markers,
/// so the raw stream is fed to all three independently.
fn probe_all(timeout: Duration) -> Result<ProbeResults> {
    let mut env = build_prt_envelope(&[(PrtCommand::Probe, 0)]);
    env.extend(build_vge_envelope(&[(VgeCommand::Probe, 0)]));
    env.extend(build_ses_envelope(&SesCommand::Probe, 0));
    write_all_stdout(&env)?;

    let mut prt_apc = PrtApcStream::with_marker(*PRT_MARKER_T2C);
    let mut vge_apc = VgeApcStream::with_marker(*VGE_MARKER_T2C);
    let mut ses_apc = SesApcStream::with_marker(*SES_MARKER_H2C);

    let mut res = ProbeResults {
        prt_ok: false,
        host_themed_styles: false,
        accent_rgba: None,
        vge: None,
        session_name: None,
    };
    let mut ses_seen = false;

    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        if res.prt_ok && res.vge.is_some() && ses_seen {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if !poll_stdin_for(deadline - now)? {
            break;
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            break;
        }

        for payload in prt_apc.feed(&buf[..n]).payloads {
            if let Some((themed, accent)) = parse_prt_probe(&payload) {
                res.prt_ok = true;
                res.host_themed_styles = themed;
                res.accent_rgba = accent;
            }
        }
        for payload in vge_apc.feed(&buf[..n]).payloads {
            if let Some(p) = parse_vge_probe(&payload) {
                res.vge = Some(p);
            }
        }
        for payload in ses_apc.feed(&buf[..n]).payloads {
            let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
            let _ = ses_for_each_frame(&payload, |ft, _rid, body| {
                frames.push((ft, body.to_vec()));
                Ok::<(), u16>(())
            });
            for (ft, body) in frames {
                if let Ok(SesHostFrame::ProbeResponse {
                    in_session, name, ..
                }) = SesHostFrame::parse(ft, &body)
                {
                    ses_seen = true;
                    if in_session && !name.is_empty() {
                        res.session_name = Some(name);
                    }
                }
            }
        }
    }
    Ok(res)
}

fn poll_stdin_for(timeout: Duration) -> Result<bool> {
    let ms: u16 = timeout.as_millis().min(i32::MAX as u128) as u16;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let n = poll(&mut fds, PollTimeout::from(ms)).context("poll(stdin)")?;
    Ok(n > 0)
}

fn read_stdin(buf: &mut [u8]) -> Result<usize> {
    let fd = std::io::stdin().as_raw_fd();
    loop {
        match nix::unistd::read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("read(stdin): {e}")),
        }
    }
}

/// Non-blocking read of stdin for the main loop. Once stdout is switched to
/// non-blocking, stdin shares the same open file description (fd 0/1/2 are dups
/// of the host PTY slave), so reads can return `EAGAIN` even after `POLLIN`
/// reported readable (a spurious wakeup or a racing drain). `Ok(None)` means
/// "would block, nothing read" — distinct from `Ok(Some(0))`, which is EOF.
fn read_stdin_nb(buf: &mut [u8]) -> Result<Option<usize>> {
    let fd = std::io::stdin().as_raw_fd();
    loop {
        match nix::unistd::read(fd, buf) {
            Ok(n) => return Ok(Some(n)),
            Err(Errno::EINTR) => continue,
            Err(Errno::EAGAIN) => return Ok(None),
            Err(e) => return Err(anyhow!("read(stdin): {e}")),
        }
    }
}

fn drain_stale_stdin() {
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::ZERO) {
            Ok(n) if n > 0 => {
                if read_stdin(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
            _ => break,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Main loop
// ─────────────────────────────────────────────────────────────────────────

const USAGE: &str = "\
vmux — terminal multiplexer for veter

Usage: vmux [OPTIONS]

Options:
  -A, --accent <COLOR>   chrome accent color: a name (red, green, blue,
                         yellow, orange, magenta, cyan, purple, white) or
                         hex (#rgb, #rrggbb, #rrggbbaa)
  -P, --prefix <KEY>     prefix key (default Ctrl+Space). Accepts C-a,
                         ctrl+a, ^a, or a bare letter; 'space' for Ctrl+Space
  -h, --help             print this help and exit

The accent and prefix options give nested sessions (e.g. over ssh) a
distinct color and prefix so they are easy to tell apart.
";

/// Parsed command-line options. `accent` is a packed `0xRRGGBBAA`.
struct CliOptions {
    accent: Option<u32>,
    prefix: Option<u8>,
}

/// Parse vmux's command-line arguments. `--accent`/`-A` and `--prefix`/`-P`
/// each take a value (also accepted as `--flag=value`); `--help`/`-h` prints
/// usage and exits. Unknown flags are errors so typos surface loudly.
fn parse_cli_args() -> Result<CliOptions> {
    let mut opts = CliOptions { accent: None, prefix: None };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let take = |args: &mut std::iter::Skip<std::env::Args>| {
            args.next().ok_or_else(|| anyhow!("{arg} requires a value"))
        };
        match arg.as_str() {
            "--accent" | "-A" => opts.accent = Some(parse_accent_color(&take(&mut args)?)?),
            "--prefix" | "-P" => opts.prefix = Some(parse_prefix_key(&take(&mut args)?)?),
            "--help" | "-h" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other if other.starts_with("--accent=") => {
                opts.accent = Some(parse_accent_color(&other["--accent=".len()..])?);
            }
            other if other.starts_with("--prefix=") => {
                opts.prefix = Some(parse_prefix_key(&other["--prefix=".len()..])?);
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }
    Ok(opts)
}

/// Named accent palette, packed `0xRRGGBBAA`. Muted shades that stay legible
/// behind light foreground text. `blue` matches the compiled-in brand.
fn named_color(name: &str) -> Option<u32> {
    Some(match name.to_ascii_lowercase().as_str() {
        "red" => 0xd0_5c_5c_ff,
        "green" => 0x5c_a0_5c_ff,
        "blue" => 0x56_79_9f_ff,
        "yellow" => 0xc9_a8_4c_ff,
        "orange" => 0xcf_7d_3c_ff,
        "magenta" | "pink" => 0xb0_5c_9f_ff,
        "cyan" | "teal" => 0x4c_9f_9f_ff,
        "purple" | "violet" => 0x8c_6c_c0_ff,
        "white" | "gray" | "grey" => 0x9a_9a_9a_ff,
        _ => return None,
    })
}

/// Parse an accent color into packed `0xRRGGBBAA`. Accepts a name from
/// `named_color`, or hex `#rgb` / `#rrggbb` / `#rrggbbaa` (the leading `#`
/// is optional). Missing alpha defaults to opaque.
fn parse_accent_color(s: &str) -> Result<u32> {
    let t = s.trim();
    if let Some(rgba) = named_color(t) {
        return Ok(rgba);
    }
    let hex = t.strip_prefix('#').unwrap_or(t);
    let bad = || anyhow!("invalid color '{s}' (expected a name or #rgb/#rrggbb/#rrggbbaa)");
    let bytes: [u8; 4] = match hex.len() {
        3 => {
            let mut rgb = [0u8; 3];
            for (i, c) in hex.chars().enumerate() {
                let d = c.to_digit(16).ok_or_else(bad)? as u8;
                rgb[i] = d * 0x11; // expand nibble: 0xF → 0xFF
            }
            [rgb[0], rgb[1], rgb[2], 0xff]
        }
        6 => {
            let v = u32::from_str_radix(hex, 16).map_err(|_| bad())?;
            let [_, r, g, b] = v.to_be_bytes();
            [r, g, b, 0xff]
        }
        8 => u32::from_str_radix(hex, 16).map_err(|_| bad())?.to_be_bytes(),
        _ => return Err(bad()),
    };
    Ok(u32::from_be_bytes(bytes))
}

/// Parse a prefix-key spec into its control byte. vmux's prefix is always a
/// control character; an optional `Ctrl`/`C-`/`^` modifier is accepted and
/// stripped. `space`/`@` map to Ctrl+Space (`0x00`); a bare letter maps to
/// its control code (e.g. `a` → Ctrl+A `0x01`).
fn parse_prefix_key(s: &str) -> Result<u8> {
    let lower = s.trim().to_ascii_lowercase();
    let key = lower
        .strip_prefix("ctrl-")
        .or_else(|| lower.strip_prefix("ctrl+"))
        .or_else(|| lower.strip_prefix("c-"))
        .or_else(|| lower.strip_prefix('^'))
        .unwrap_or(lower.as_str());
    let byte = match key {
        "space" | "spc" | "" | "@" => 0x00,
        "[" => 0x1b,
        "\\" => 0x1c,
        "]" => 0x1d,
        "^" => 0x1e,
        "_" => 0x1f,
        k if k.chars().count() == 1 && k.chars().next().unwrap().is_ascii_alphabetic() => {
            (k.chars().next().unwrap().to_ascii_uppercase() as u8) & 0x1f
        }
        _ => bail!("unsupported prefix key '{s}' (try e.g. C-a, ^b, or space)"),
    };
    Ok(byte)
}

fn main() -> Result<()> {
    use std::io::IsTerminal;
    let opts = parse_cli_args()?;
    if let Some(rgba) = opts.accent {
        CLI_ACCENT_RGBA.store(rgba, Ordering::Relaxed);
        CLI_ACCENT_SET.store(true, Ordering::Relaxed);
    }
    if let Some(prefix) = opts.prefix {
        PREFIX_BYTE.store(prefix, Ordering::Relaxed);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vmux must run with stdin/stdout connected to a terminal");
    }

    init_debug_log();
    install_winch_handler()?;

    let mut tty = TtyGuard::enable()?;
    drain_stale_stdin();

    let probe = probe_all(PROBE_TIMEOUT)?;
    if !probe.prt_ok {
        bail!(
            "PRT probe timed out — host terminal does not advertise the Portal Extension"
        );
    }
    let (cell_pw, cell_ph) = match &probe.vge {
        Some(p) => (p.cell_pw as f32, p.cell_ph as f32),
        None => (9.0, 20.0),
    };
    HOST_THEMED_STYLES.store(probe.host_themed_styles, Ordering::Relaxed);
    if let Some(rgba) = probe.accent_rgba {
        HOST_ACCENT_RGBA.store(u32::from_be_bytes(rgba), Ordering::Relaxed);
    }

    let (rows, cols) = get_host_winsize()?;
    tty.enter_alt_screen()?;

    let mut state =
        State::new(cols as u32, rows as u32, cell_pw, cell_ph)?;
    // A `vsd` session host answers the SES probe with its name; a
    // plain local `veter` host does not, leaving this `None`.
    state.session_name = probe.session_name;
    // Initial render — creates portal + chrome for p1 and sets focus.
    // Still on a blocking fd, so this small burst is sent in full.
    let env = state.relayout_and_render()?;
    if !env.is_empty() {
        write_all_stdout(&env)?;
    }

    // From here on, stdout is non-blocking and drained via `POLLOUT` in the
    // loop below, so a slow host can never stall input handling. Note this
    // also makes stdin non-blocking (fd 0/1 are dups of the same PTY slave
    // file description) — hence `read_stdin_nb`. Reverted before teardown.
    set_stdout_nonblocking(true)?;

    let mut prt_apc = PrtApcStream::with_marker(*PRT_MARKER_T2C);
    let mut vge_apc = VgeApcStream::with_marker(*VGE_MARKER_T2C);
    // Consumes `ses` host frames (a `prefix-D` detach Ok, or a late
    // probe response) so they never leak into the keystroke stream.
    let mut ses_apc = SesApcStream::with_marker(*SES_MARKER_H2C);
    let mut rd_buf = [0u8; 8192];

    while !state.quit {
        if WINCH_FLAG.swap(false, Ordering::SeqCst) || state.needs_resize_check {
            state.needs_resize_check = false;
            let (rows, cols) = get_host_winsize()?;
            if cols as u32 != state.host_w || rows as u32 != state.host_h {
                state.host_w = cols as u32;
                state.host_h = rows as u32;
                let env = state.relayout_and_render()?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
            }
        }

        // Build the poll set: stdin, then (when buffered output is waiting)
        // stdout for POLLOUT, then every pane's PTY master.
        let stdin_fd = std::io::stdin().as_raw_fd();
        let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let mut fds: Vec<PollFd<'_>> =
            vec![PollFd::new(stdin_borrowed, PollFlags::POLLIN)];

        // Watch stdout for writability only while bytes are queued, otherwise
        // POLLOUT would fire continuously and spin the loop.
        let pending = out_pending();
        let stdout_borrowed = unsafe { BorrowedFd::borrow_raw(std::io::stdout().as_raw_fd()) };
        let stdout_idx = if pending > 0 {
            fds.push(PollFd::new(stdout_borrowed, PollFlags::POLLOUT));
            Some(fds.len() - 1)
        } else {
            None
        };

        // Backpressure: once too much output is already queued for a slow
        // host, stop reading pane PTYs. Their kernel buffers fill, the inner
        // shells block on write, and memory stays bounded — while stdin and
        // the POLLOUT drain keep running so the UI stays responsive.
        let read_panes = pending < OUT_HIGH_WATER;
        // Snapshot the pane fd ordering so we can map back to ids after
        // poll returns. (HashMap iteration is unstable; we capture once.)
        let pane_ids: Vec<String> = state.panes.keys().cloned().collect();
        let pane_base = fds.len();
        let pane_borroweds: Vec<BorrowedFd<'_>> = pane_ids
            .iter()
            .map(|id| {
                let raw = state.panes[id].pty.raw_fd();
                unsafe { BorrowedFd::borrow_raw(raw) }
            })
            .collect();
        if read_panes {
            for bf in &pane_borroweds {
                fds.push(PollFd::new(*bf, PollFlags::POLLIN));
            }
        }

        let n = match poll(&mut fds, PollTimeout::from(50u16)) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("poll: {e}")),
        };

        // Drain whatever the host is now ready to accept.
        if let Some(idx) = stdout_idx {
            let revents = fds[idx].revents().unwrap_or(PollFlags::empty());
            if revents.contains(PollFlags::POLLOUT) {
                out_flush()?;
            }
        }

        if n == 0 {
            // Poll idle: nothing arrived for the full 50ms window, so any
            // ESC byte still buffered in the APC parsers' EscPending state
            // is unambiguously a lone keystroke (e.g. dismiss-modal). Push
            // it through so the input handler can act on it without
            // waiting for a follow-up byte that's never coming.
            let prt_flushed = prt_apc.flush_pending_esc();
            let mut after_vge = vge_apc.feed(&prt_flushed).passthrough;
            after_vge.extend(vge_apc.flush_pending_esc());
            let mut pending = ses_apc.feed(&after_vge).passthrough;
            pending.extend(ses_apc.flush_pending_esc());
            if !pending.is_empty() {
                process_user_input(&mut state, &pending)?;
            }
            continue;
        }

        // Stdin: host responses + user keystrokes.
        let stdin_revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if stdin_revents.contains(PollFlags::POLLIN) {
            match read_stdin_nb(&mut rd_buf)? {
                Some(0) => {
                    state.quit = true;
                    break;
                }
                Some(n) => {
                    dlog("stdin", &rd_buf[..n]);
                    trace_vmux_stdin(&rd_buf[..n]);
                    handle_stdin_chunk(
                        &mut state,
                        &mut prt_apc,
                        &mut vge_apc,
                        &mut ses_apc,
                        &rd_buf[..n],
                    )?;
                }
                // EAGAIN: POLLIN raced a drain; nothing to read this round.
                None => {}
            }
        }

        // Pane PTYs: shell output → WritePortal display path. Skipped entirely
        // while backpressured (their fds aren't in the poll set then).
        for (i, pid) in pane_ids.iter().enumerate() {
            if !read_panes {
                break;
            }
            let revents =
                fds[pane_base + i].revents().unwrap_or(PollFlags::empty());
            if revents.contains(PollFlags::POLLIN) {
                let raw = state.panes[pid].pty.raw_fd();
                let n = match nix::unistd::read(raw, &mut rd_buf) {
                    Ok(n) => n,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(e) => return Err(anyhow!("pty read: {e}")),
                };
                if n == 0 {
                    // Shell exited — close the pane (any tab).
                    let env = state.close_pane(pid)?;
                    if !env.is_empty() {
                        write_all_stdout(&env)?;
                    }
                    continue;
                }
                dlog(&format!("pty>{pid}"), &rd_buf[..n]);
                let env = build_prt_envelope(&[(
                    PrtCommand::WritePortal(WritePortalBody {
                        id: pid.clone(),
                        data: rd_buf[..n].to_vec(),
                    }),
                    0,
                )]);
                write_all_stdout(&env)?;
            } else if revents
                .intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
            {
                // PTY hung up — close the pane, same code-path as EOF.
                let env = state.close_pane(pid)?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
            }
        }
    }

    // Back to a blocking fd for teardown and drain anything still queued, so
    // the cleanup envelopes below (and the alt-screen exit in `RawTty::drop`)
    // are written in full rather than left half-sent.
    set_stdout_nonblocking(false)?;
    out_flush()?;

    // Hand focus back to the host BEFORE wiping its portal table —
    // otherwise the host would keep "focus on a portal" state with no
    // portals, which suppresses its own cursor. Then wait for the
    // matching responses so the kernel PTY buffer is empty when the
    // user's outer shell takes over (otherwise the encoded Ok envelopes
    // bleed in as input garbage).
    let prt_cleanup = [
        (
            PrtCommand::SetFocus {
                target: FocusTarget::Host,
            },
            0,
        ),
        (PrtCommand::ClearAll, 0),
    ];
    let vge_cleanup = [(VgeCommand::ClearAll, 0)];
    let _ = write_all_stdout(&build_prt_envelope(&prt_cleanup));
    let _ = write_all_stdout(&build_vge_envelope(&vge_cleanup));

    let _ = await_cleanup_responses(
        &mut prt_apc,
        &mut vge_apc,
        prt_cleanup.len(),
        vge_cleanup.len(),
        Duration::from_millis(500),
    );
    Ok(())
}

/// Wait until the host has acknowledged exactly `expected_prt` PRT
/// response frames and `expected_vge` VGE response frames, capped at
/// `timeout`. Events the host emits along the way (PortalEvicted,
/// trailing TitleChange, …) are read out of the PTY too — the goal is
/// that nothing tied to our cleanup commands lingers in the kernel
/// buffer when the outer shell takes over.
fn await_cleanup_responses(
    prt_apc: &mut PrtApcStream,
    vge_apc: &mut VgeApcStream,
    expected_prt: usize,
    expected_vge: usize,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut seen_prt = 0usize;
    let mut seen_vge = 0usize;
    let mut buf = [0u8; 4096];
    while seen_prt < expected_prt || seen_vge < expected_vge {
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        if !poll_stdin_for(deadline - now)? {
            return Ok(());
        }
        let n = match read_stdin(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(_) => return Ok(()),
        };
        let prt_out = prt_apc.feed(&buf[..n]);
        for payload in &prt_out.payloads {
            // Each envelope carries one or more frames; PRT response
            // codes are 0x01..=0x7F, events are 0x80..=0xFF (§4).
            let mut r = PrtReader::new(payload);
            let _ = r.u8();
            let _ = r.u32();
            while !r.at_end() {
                let Ok(ft) = r.u8() else { break };
                let _rid = r.u32().ok();
                let body_len = match r.u32() {
                    Ok(v) => v as usize,
                    Err(_) => break,
                };
                if r.take(body_len).is_err() {
                    break;
                }
                if ft < 0x80 {
                    seen_prt += 1;
                }
            }
        }
        let vge_out = vge_apc.feed(&prt_out.passthrough);
        for payload in &vge_out.payloads {
            let mut r = vge_protocol::codec::Reader::new(payload);
            let _ = r.u8();
            let _ = r.u32();
            while !r.at_end() {
                let Ok(_ft) = r.u8() else { break };
                let _rid = r.u32().ok();
                let body_len = match r.u32() {
                    Ok(v) => v as usize,
                    Err(_) => break,
                };
                if r.take(body_len).is_err() {
                    break;
                }
                seen_vge += 1;
            }
        }
    }
    Ok(())
}

/// Handle one chunk of bytes read from the host PTY (our stdin). Bytes
/// take three paths:
///   1. PRT envelopes → host responses + RawReply events.
///   2. VGE envelopes → swallow (we don't need responses for chrome).
///   3. Plain bytes   → user keystrokes; run them through the input
///      state machine.
fn handle_stdin_chunk(
    state: &mut State,
    prt_apc: &mut PrtApcStream,
    vge_apc: &mut VgeApcStream,
    ses_apc: &mut SesApcStream,
    bytes: &[u8],
) -> Result<()> {
    let prt_out = prt_apc.feed(bytes);

    // Pane ids in the active tab — activity events for these are
    // ignored (the user is already looking at them).
    let active_ids = state.active_pane_ids();
    // Set when a background pane reported activity, so the tab bar is
    // re-rendered once after the frame loop.
    let mut activity_dirty = false;

    // PRT host frames: scan for RawReply events (forward to the matching
    // pane's PTY) and Ok responses to our scroll commands (carry the
    // host-applied offset so we can show the real scroll position even
    // when the request exceeded the available history).
    // Most recent (applied_lines, history_depth) ack received per
    // pane in this chunk. Coalesced so a burst of scroll commands
    // results in a single chrome re-render per pane.
    let mut latest_acks: HashMap<String, (u32, u32)> = HashMap::new();
    // Pane ids whose effective title may have changed in this chunk.
    // Used after the loop to re-emit chrome and the tab bar in one go.
    let mut titles_dirty: HashSet<String> = HashSet::new();
    for payload in prt_out.payloads {
        let mut r = PrtReader::new(&payload);
        let _version = r.u8();
        let _payload_len = r.u32();
        while !r.at_end() {
            let Ok(ft) = r.u8() else { break };
            let rid = r.u32().unwrap_or(0);
            let body_len = match r.u32() {
                Ok(v) => v as usize,
                Err(_) => break,
            };
            let Ok(body) = r.take(body_len) else {
                break;
            };
            if ft == EVT_RAW_REPLY {
                let mut br = PrtReader::new(body);
                let id = br.string().unwrap_or("").to_string();
                let data = br.bytes().unwrap_or(&[]).to_vec();
                if !id.is_empty() {
                    if let Some(p) = state.panes.get(&id) {
                        dlog(&format!("rawreply>{id}"), &data);
                        let _ = p.pty.write_all(&data);
                    }
                }
            } else if ft == EVT_TITLE_CHANGE || ft == EVT_ICON_NAME_CHANGE {
                // §8.2: string id, string title. Empty title clears the
                // override and lets the next-precedence label show.
                let mut br = PrtReader::new(body);
                let id = br.string().unwrap_or("").to_string();
                let title = br.string().unwrap_or("").to_string();
                if !id.is_empty() {
                    if let Some(p) = state.panes.get_mut(&id) {
                        let slot = if ft == EVT_TITLE_CHANGE {
                            &mut p.osc_title
                        } else {
                            &mut p.osc_icon
                        };
                        let new_value = if title.is_empty() { None } else { Some(title) };
                        if *slot != new_value {
                            *slot = new_value;
                            titles_dirty.insert(id);
                        }
                    }
                }
            } else if ft == EVT_PORTAL_ACTIVITY {
                // §8: string id. The host already suppresses the
                // focused portal; vmux additionally ignores any pane
                // in the active tab and flags the rest so their tab
                // shows the activity marker.
                let mut br = PrtReader::new(body);
                let id = br.string().unwrap_or("").to_string();
                if !id.is_empty() && !active_ids.contains(&id) {
                    if let Some(p) = state.panes.get_mut(&id) {
                        if !p.activity {
                            p.activity = true;
                            activity_dirty = true;
                        }
                    }
                }
            } else if ft == EVT_MOUSE_MODE_CHANGE {
                // §8.9: id, protocol, encoding, focus_events. We only
                // need protocol — encoding/focus is handled per-event
                // when we re-encode forwarded mouse bytes.
                let mut br = PrtReader::new(body);
                let id = br.string().unwrap_or("").to_string();
                let protocol = br.u8().unwrap_or(0);
                if let Some(p) = state.panes.get_mut(&id) {
                    p.inner_mouse_protocol = protocol;
                }
            } else if ft == EVT_PORTAL_SCROLL_DELTA {
                // §8.11: string id, i32 delta. The host is asking us to
                // shift this pane's scrollback by `delta` lines (positive
                // = deeper into history). Used by veter's drag-select
                // autoscroll on a portal-target selection, so the offset
                // stays coherent with our `PaneScroll` instead of being
                // mutated behind our back. We reuse `wheel_scroll`'s
                // enter/apply/exit logic so a delta that lands at 0
                // cleanly drops scroll mode, mirroring a manual wheel.
                let mut br = PrtReader::new(body);
                let id = br.string().unwrap_or("").to_string();
                let delta = br.i32().unwrap_or(0);
                if !id.is_empty() && state.panes.contains_key(&id) {
                    let _ = wheel_scroll(state, &id, delta as i64);
                }
            } else if ft == EVT_PORTAL_SCROLL_SET {
                // §8.12: string id, u32 offset. The host is asking us to
                // jump this pane to an absolute scrollback offset (e.g.
                // a scrollback-search match). `apply_scroll_set` is the
                // absolute-value sibling of `wheel_scroll`: it enters
                // scroll mode if needed for a non-zero target, drops
                // scroll mode at zero, and otherwise sets the offset
                // through the normal `scroll_set` path so the chrome
                // thumb and `SetPortalScrollback` envelope to the host
                // stay coherent.
                let mut br = PrtReader::new(body);
                let id = br.string().unwrap_or("").to_string();
                let offset = br.u32().unwrap_or(0);
                if !id.is_empty() && state.panes.contains_key(&id) {
                    let _ = apply_scroll_set(state, &id, offset);
                }
            } else if ft == prt_protocol::frame::RSP_OK
                && body.len() >= 4
            {
                // §9.3 `SetPortalScrollback` ack: u32 applied_lines,
                // u32 history_depth (history_depth is optional per the
                // spec — older hosts may omit it). The frame doesn't
                // carry the pane id, so we route it via
                // `pending_scrolls` (rid → pane id) which we populated
                // when sending the request.
                if let Some(pane_id) = state.pending_scrolls.remove(&rid) {
                    let applied = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    let history = if body.len() >= 8 {
                        u32::from_le_bytes([body[4], body[5], body[6], body[7]])
                    } else {
                        0
                    };
                    latest_acks.insert(pane_id, (applied, history));
                }
            }
        }
    }
    // Flush title changes: re-emit each affected pane's chrome and the
    // tab bar (because the active pane's effective title can drive the
    // auto-tab-title). Cheap — small per-pane VGE elements. An activity
    // change needs only the tab bar re-rendered.
    if !titles_dirty.is_empty() || activity_dirty {
        let mut env = Vec::new();
        for id in &titles_dirty {
            env.extend(state.render_one_chrome(id));
        }
        env.extend(state.render_tabbar());
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }

    // Apply each pane's most recent scroll ack and re-render its chrome
    // if anything actually changed. A pane that has already exited
    // scroll mode (e.g. the user pressed `q` between the request and
    // the ack) silently drops the update.
    for (pane_id, (applied, history)) in &latest_acks {
        let changed = match state.panes.get_mut(pane_id).and_then(|p| p.scroll.as_mut()) {
            Some(s) => {
                let dirty = s.offset != *applied || s.history_depth != *history;
                s.offset = *applied;
                s.history_depth = *history;
                dirty
            }
            None => false,
        };
        if changed {
            let env = state.render_one_chrome(pane_id);
            if !env.is_empty() {
                write_all_stdout(&env)?;
            }
        }
    }

    // Strip VGE envelopes from the residual passthrough — what's left
    // is user keystrokes plus host-emitted mouse events (because we
    // enabled SGR mouse reporting on entry to alt screen).
    let vge_out = vge_apc.feed(&prt_out.passthrough);
    let _ = vge_out.payloads;

    // Strip SES host frames (detach Ok / late probe responses). vmux
    // acts on neither — `prefix-D` is fire-and-forget and the session
    // name is captured at startup — but they must not reach the
    // keystroke stream.
    let ses_out = ses_apc.feed(&vge_out.passthrough);
    let _ = ses_out.payloads;

    // Split mouse events out of the keystroke stream, dispatch each,
    // and forward only non-mouse bytes to the input state machine.
    let (regular, mouse_events) = extract_mouse_events(&ses_out.passthrough);
    for ev in mouse_events {
        handle_mouse_event(state, ev)?;
    }
    if !regular.is_empty() {
        process_user_input(state, &regular)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct MouseEvent {
    /// xterm-style button code: bits 0..1 = button (0=left, 1=middle,
    /// 2=right, 3=release), bit 2 = shift, bit 3 = meta, bit 4 = ctrl,
    /// bit 5 = motion, bit 6 = wheel (64=up, 65=down).
    button: u32,
    /// 1-indexed host cell coords as reported by SGR mouse encoding.
    col: u32,
    row: u32,
    /// `true` for press / motion events (`M`), `false` for release (`m`).
    press: bool,
}

/// Walk `bytes` and pull out SGR mouse sequences (`\e[<b;c;rM/m`).
/// Returns the bytes that were NOT part of any mouse sequence (suitable
/// for forwarding to the focused pane) plus the parsed events.
fn extract_mouse_events(bytes: &[u8]) -> (Vec<u8>, Vec<MouseEvent>) {
    let mut out_bytes = Vec::with_capacity(bytes.len());
    let mut events = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Try to recognise `\e[<…M/m` as a mouse event. Anything else
        // (including non-mouse CSI sequences) goes through verbatim.
        if bytes[i] == 0x1B
            && i + 2 < bytes.len()
            && bytes[i + 1] == b'['
            && bytes[i + 2] == b'<'
        {
            // Scan ahead for the terminating M/m, capping the lookahead
            // so a malformed sequence doesn't swallow the rest of the
            // chunk.
            let start = i + 3;
            let cap = (start + 32).min(bytes.len());
            if let Some(end) = (start..cap)
                .find(|&j| bytes[j] == b'M' || bytes[j] == b'm')
            {
                let press = bytes[end] == b'M';
                let body = &bytes[start..end];
                if let Some(ev) = parse_sgr_body(body, press) {
                    events.push(ev);
                    i = end + 1;
                    continue;
                }
            }
        }
        out_bytes.push(bytes[i]);
        i += 1;
    }
    (out_bytes, events)
}

fn parse_sgr_body(body: &[u8], press: bool) -> Option<MouseEvent> {
    let s = std::str::from_utf8(body).ok()?;
    let mut parts = s.splitn(3, ';');
    let button: u32 = parts.next()?.parse().ok()?;
    let col: u32 = parts.next()?.parse().ok()?;
    let row: u32 = parts.next()?.parse().ok()?;
    Some(MouseEvent {
        button,
        col,
        row,
        press,
    })
}

/// Hit-test a mouse event and dispatch:
///   - left click on row 0 → switch tabs (or step via a chevron);
///   - left click inside a pane → focus that pane;
///   - wheel + inner program doesn't want mouse → drive vmux's
///     scrollback for that pane (auto-enter the pane's scroll state,
///     auto-exit at offset 0);
///   - any other case → forward the event to the matching pane's PTY
///     in SGR encoding, with coords translated to portal-relative.
fn handle_mouse_event(state: &mut State, ev: MouseEvent) -> Result<()> {
    // SGR coords are 1-indexed; PaneRect and row 0 are 0-indexed.
    let host_col = ev.col.saturating_sub(1) as i32;
    let host_row = ev.row.saturating_sub(1) as i32;

    // A plain left-button press — no motion bit (0x20), no wheel bit
    // (0x40), modifier bits ignored — is what drives vmux focus. Drags
    // and releases must not retrigger it.
    let is_left_press = ev.press && (ev.button & 0x63) == 0;

    // An active divider drag captures every mouse event until the button
    // releases — including motion that strays onto the tab bar or off a
    // pane — so the boundary tracks the pointer smoothly.
    if state.resize_drag.is_some() {
        if ev.press {
            let env = state.update_resize_drag(host_col, host_row)?;
            if !env.is_empty() {
                write_all_stdout(&env)?;
            }
        } else {
            state.resize_drag = None;
        }
        return Ok(());
    }

    // Row 0 is the tab bar, never part of a pane rect: a left click
    // there switches tabs and the event goes no further. Gated to
    // `Mode::Normal` so a click can't reach through a modal.
    if host_row == 0 {
        if is_left_press && matches!(state.mode, Mode::Normal) {
            handle_tabbar_click(state, host_col)?;
        }
        return Ok(());
    }

    // A left-press on (or right next to) a split divider grabs it for a
    // drag-to-resize, taking precedence over focusing the pane underneath.
    // Skipped while zoomed — no separators are visible then. Gated to
    // `Mode::Normal` so a stray click can't resize through a modal.
    if is_left_press
        && matches!(state.mode, Mode::Normal)
        && state.tabs[state.active_tab].zoomed.is_none()
    {
        let bounds = state.full_bounds();
        let mut path = Vec::new();
        if let Some((path, dir, sbounds)) =
            separator_hit(state.active_layout(), bounds, host_col, host_row, &mut path)
        {
            state.resize_drag = Some(ResizeDrag { path, dir, bounds: sbounds });
            return Ok(());
        }
    }

    // Find the pane whose `last_rect` contains the event cell. Only
    // currently-visible panes are eligible — panes from inactive tabs
    // (and zoom-hidden siblings in the active tab) keep their
    // `last_rect` from when they were last visible, so a global scan
    // would land mouse events on hidden panes whose rects overlap the
    // active layout (and HashMap iteration order would make the
    // choice nondeterministic).
    let visible = state.visible_panes.clone();
    let mut target: Option<String> = None;
    for (id, pane) in &state.panes {
        if !visible.contains(id) {
            continue;
        }
        if let Some(rect) = pane.last_rect {
            if host_col >= rect.x
                && host_col < rect.x + rect.w as i32
                && host_row >= rect.y
                && host_row < rect.y + rect.h as i32
            {
                target = Some(id.clone());
                break;
            }
        }
    }
    let Some(pane_id) = target else {
        return Ok(());
    };

    // A left click anywhere in a pane focuses it — the pointer
    // equivalent of `prefix-o`. Done before the forward logic below so
    // that when the inner program also wants the click it both focuses
    // the pane and receives the event. Gated to `Mode::Normal` so a
    // stray click can't move focus out from under a modal.
    if is_left_press && matches!(state.mode, Mode::Normal) {
        let env = state.focus_pane(&pane_id)?;
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }

    let is_wheel = matches!(ev.button & 0xFF, 64 | 65);
    let inner_wants_mouse = state
        .panes
        .get(&pane_id)
        .map(|p| p.inner_mouse_protocol != 0)
        .unwrap_or(false);

    if is_wheel && !inner_wants_mouse {
        // Drive vmux's per-pane scrollback. Wheel-up enters scroll
        // mode; wheel-down at offset 0 is a no-op; reaching offset 0
        // exits scroll mode.
        if !ev.press {
            return Ok(()); // wheel only emits "press" events
        }
        let dir: i64 = if ev.button == 64 { 3 } else { -3 };
        wheel_scroll(state, &pane_id, dir)?;
        return Ok(());
    }

    if !inner_wants_mouse {
        // The inner program hasn't asked for mouse events, so the
        // wire-encoded escape would otherwise be echoed in a plain
        // shell. Drop it.
        return Ok(());
    }

    // Motion events (button bit 0x20) only matter to programs that asked
    // for button-event (3) or any-event (4) tracking. We keep 1002 on for
    // our own divider drags, so drop motion bound for a program in plain
    // X10 (1) / normal (2) mode rather than feed it events it never
    // requested.
    let is_motion = ev.button & 0x20 != 0;
    if is_motion {
        let proto = state
            .panes
            .get(&pane_id)
            .map(|p| p.inner_mouse_protocol)
            .unwrap_or(0);
        if proto < 3 {
            return Ok(());
        }
    }

    // Forward to the inner program. Translate host cells to
    // portal-relative cells (the inner expects coords inside its own
    // grid). If the click is on the chrome (border / title row),
    // ignore it rather than feed an out-of-range coordinate.
    let Some(rect) = state.panes.get(&pane_id).and_then(|p| p.last_rect) else {
        return Ok(());
    };
    let (rows, cols) = inner_grid_for(rect);
    let (origin_x, origin_y) = inner_origin_for(rect);
    let portal_col = host_col - origin_x;
    let portal_row = host_row - origin_y;
    if portal_col < 0
        || portal_col >= cols as i32
        || portal_row < 0
        || portal_row >= rows as i32
    {
        return Ok(());
    }
    let final_byte = if ev.press { b'M' } else { b'm' };
    let payload = format!(
        "\x1b[<{};{};{}{}",
        ev.button,
        portal_col + 1,
        portal_row + 1,
        final_byte as char,
    );
    if let Some(p) = state.panes.get(&pane_id) {
        let _ = p.pty.write_all(payload.as_bytes());
    }
    Ok(())
}

/// Resolve a left click on row 0 (the tab bar) and switch tabs to
/// match. The bar geometry is recomputed from the live tab list so the
/// hit-test maps to exactly the tabs the user sees; the chevrons step
/// to the neighbouring tab, mirroring `prefix-n` / `prefix-p`.
fn handle_tabbar_click(state: &mut State, host_col: i32) -> Result<()> {
    if host_col < 0 {
        return Ok(());
    }
    let labels = state.tabbar_labels();
    // The last render already settled `tab_scroll`; hit-test against a
    // copy so this probe leaves the carried offset untouched.
    let mut scroll = state.tab_scroll;
    let layout = compute_tabbar_layout(
        &labels,
        state.active_tab,
        &mut scroll,
        state.host_w,
        state.session_name.as_deref(),
    );
    let env = match tabbar_hit(&layout, state.host_w, host_col as f32) {
        Some(TabbarHit::Tab(idx)) => state.goto_tab(idx)?,
        Some(TabbarHit::PrevTab) => state.prev_tab()?,
        Some(TabbarHit::NextTab) => state.next_tab()?,
        None => return Ok(()),
    };
    if !env.is_empty() {
        write_all_stdout(&env)?;
    }
    Ok(())
}

/// Wheel-driven scroll for `pane_id`: enter or extend that pane's
/// scrollback navigation by `delta`. Each pane's scroll state is
/// independent, so wheeling pane B doesn't disturb pane A's scroll.
/// Used when the inner program hasn't enabled mouse reporting, so
/// wheel naturally drives vmux's scrollback for the pane under the
/// cursor.
/// Absolute-target sibling of `wheel_scroll`. Routes
/// `EVT_PORTAL_SCROLL_SET` (e.g. a host-driven scrollback search jump)
/// through the same enter/`scroll_set`/exit ladder so the chrome thumb,
/// `[scroll: N]` indicator, and host `SetPortalScrollback` envelope all
/// stay coherent. `offset == 0` drops scroll mode entirely.
fn apply_scroll_set(state: &mut State, pane_id: &str, offset: u32) -> Result<()> {
    let already_scrolling = state
        .panes
        .get(pane_id)
        .map(|p| p.scroll.is_some())
        .unwrap_or(false);

    if offset == 0 {
        if already_scrolling {
            let env = state.exit_scroll(pane_id)?;
            if !env.is_empty() {
                write_all_stdout(&env)?;
            }
        }
        return Ok(());
    }

    if !already_scrolling {
        let env = state.enter_scroll(pane_id)?;
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }
    let env = state.scroll_set(pane_id, offset)?;
    if !env.is_empty() {
        write_all_stdout(&env)?;
    }
    Ok(())
}

fn wheel_scroll(state: &mut State, pane_id: &str, delta: i64) -> Result<()> {
    let already_scrolling = state
        .panes
        .get(pane_id)
        .map(|p| p.scroll.is_some())
        .unwrap_or(false);

    // Wheeling down on a pane that isn't currently being scrolled
    // means "stay at live" — ignore.
    if !already_scrolling && delta < 0 {
        return Ok(());
    }

    if !already_scrolling {
        let env = state.enter_scroll(pane_id)?;
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }

    let env = state.scroll_delta(pane_id, delta)?;
    if !env.is_empty() {
        write_all_stdout(&env)?;
    }

    // If we just landed back at live, drop scroll for this pane so the
    // scrollbar / `[scroll: 0]` indicator clear.
    let at_live = state
        .panes
        .get(pane_id)
        .and_then(|p| p.scroll.as_ref())
        .map(|s| s.offset == 0)
        .unwrap_or(false);
    if at_live {
        let env = state.exit_scroll(pane_id)?;
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }
    Ok(())
}

/// Drive the input state machine for a chunk of user keystrokes.
fn process_user_input(state: &mut State, bytes: &[u8]) -> Result<()> {
    let mut idx = 0;
    while idx < bytes.len() {
        let b = bytes[idx];
        match &state.mode {
            Mode::Normal => {
                if b == prefix_byte() {
                    state.mode = Mode::Prefix;
                    idx += 1;
                    continue;
                }
                // If the focused pane is in scroll mode, route keys to
                // the scroll handler. Other panes (possibly also
                // scrolling) are unaffected — focus drives input.
                let focus_id = state.focus().to_string();
                let focus_scrolling = state
                    .panes
                    .get(&focus_id)
                    .map(|p| p.scroll.is_some())
                    .unwrap_or(false);
                if focus_scrolling {
                    let env = handle_scroll_byte(state, &focus_id, b)?;
                    if !env.is_empty() {
                        write_all_stdout(&env)?;
                    }
                    idx += 1;
                    continue;
                }
                // Forward the entire residual chunk to the focused pane
                // up to the next prefix byte. This keeps multi-byte
                // sequences (CSI, UTF-8) intact.
                let stop = bytes[idx..]
                    .iter()
                    .position(|c| *c == prefix_byte())
                    .map(|p| idx + p)
                    .unwrap_or(bytes.len());
                if let Some(pane) = state.panes.get(&focus_id) {
                    dlog(&format!("key>{focus_id}"), &bytes[idx..stop]);
                    trace_vmux_to_pane(&focus_id, &bytes[idx..stop]);
                    pane.pty.write_all(&bytes[idx..stop])?;
                }
                idx = stop;
            }
            Mode::Prefix => {
                // Arrow keys arrive as 3-byte sequences (`ESC [ X` in
                // normal cursor mode, `ESC O X` in DECCKM application
                // mode), so peek ahead and consume the whole sequence
                // when prefix-arrow is in flight. If the trailing
                // bytes haven't landed in this read yet, fall through
                // to the single-byte path — the lone ESC is silently
                // dropped, which is preferable to leaking partial
                // CSI bytes to the focused pane.
                if b == 0x1b
                    && idx + 2 < bytes.len()
                    && (bytes[idx + 1] == b'[' || bytes[idx + 1] == b'O')
                {
                    let env = handle_prefix_arrow(state, bytes[idx + 2])?;
                    state.mode = Mode::Normal;
                    if !env.is_empty() {
                        write_all_stdout(&env)?;
                    }
                    idx += 3;
                    continue;
                }
                let env = handle_prefix_command(state, b)?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += 1;
            }
            Mode::Rename { .. } => {
                let (consumed, env) = handle_rename_input(state, &bytes[idx..])?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += consumed.max(1);
            }
            Mode::Help { .. } => {
                let env = handle_help_byte(state, b)?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += 1;
            }
            Mode::ConfirmQuit => {
                let env = handle_confirm_byte(state, b)?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += 1;
            }
            Mode::Resize { .. } => {
                let env = handle_resize_byte(state, b)?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += 1;
            }
        }
    }
    Ok(())
}

/// Process one byte after the prefix key was pressed.
fn handle_prefix_command(state: &mut State, b: u8) -> Result<Vec<u8>> {
    state.mode = Mode::Normal;
    match b {
        // pane controls (lowercase)
        b'v' => state.split(SplitDir::Vertical),
        b'h' => state.split(SplitDir::Horizontal),
        b'x' => state.close_focused(),
        b'o' => state.cycle_focus(),
        b'z' => state.toggle_zoom(),
        b's' => state.enter_resize(),
        b'q' => {
            // Don't quit outright — pop a confirmation modal so an
            // accidental prefix-q is a recoverable keystroke.
            state.mode = Mode::ConfirmQuit;
            Ok(state.render_modal_overlay())
        }
        b'r' => {
            let pane_id = state.focus().to_string();
            // Pre-fill with the current effective title so backspacing
            // to empty cleanly drops the manual override.
            let buffer = state
                .panes
                .get(&pane_id)
                .map(|p| p.effective_title().to_string())
                .unwrap_or_default();
            state.mode = Mode::Rename {
                target: RenameTarget::Pane(pane_id),
                editor: LineEditor::new(buffer),
            };
            Ok(state.render_modal_overlay())
        }
        // tab controls
        b'c' => state.new_tab(),
        b'n' => state.next_tab(),
        b'p' => state.prev_tab(),
        b'R' => {
            let idx = state.active_tab;
            let buffer = state.tab_effective_title(idx);
            state.mode = Mode::Rename {
                target: RenameTarget::Tab(idx),
                editor: LineEditor::new(buffer),
            };
            Ok(state.render_modal_overlay())
        }
        b'1'..=b'9' => {
            let idx = (b - b'1') as usize;
            state.goto_tab(idx)
        }
        b'<' => state.move_tab_left(),
        b'>' => state.move_tab_right(),
        // detach the vsd session — fire-and-forget SES command.
        // A no-op outside a session (no host to act on it).
        b'D' => {
            if state.session_name.is_some() {
                Ok(build_ses_envelope(&SesCommand::Detach, 0))
            } else {
                Ok(Vec::new())
            }
        }
        // help
        b'?' => {
            state.mode = Mode::Help {
                offset: 0,
                csi_buf: Vec::new(),
            };
            Ok(state.render_modal_overlay())
        }
        // scroll mode for focused pane
        b'[' => {
            let pane_id = state.focus().to_string();
            state.enter_scroll(&pane_id)
        }
        _ if b == prefix_byte() => {
            // Double-tap: forward a literal prefix byte to the focused pane.
            let focus_id = state.focus().to_string();
            if let Some(pane) = state.panes.get(&focus_id) {
                pane.pty.write_all(&[prefix_byte()])?;
            }
            Ok(Vec::new())
        }
        _ => Ok(Vec::new()),
    }
}

/// Final-byte dispatch for `prefix + <arrow>`. Right (`C`) cycles to
/// the next tab, left (`D`) to the previous one. Up/down are reserved
/// for future use and silently consumed. Caller has already reset the
/// mode to `Mode::Normal` and consumed the whole CSI/SS3 sequence.
fn handle_prefix_arrow(state: &mut State, final_byte: u8) -> Result<Vec<u8>> {
    match final_byte {
        b'C' => state.next_tab(),
        b'D' => state.prev_tab(),
        _ => Ok(Vec::new()),
    }
}

/// Process keystrokes while the rename modal is up. Feeds the front of
/// `rest` to the [`LineEditor`] and acts on its outcome, returning the
/// number of input bytes consumed alongside the envelope to emit.
fn handle_rename_input(state: &mut State, rest: &[u8]) -> Result<(usize, Vec<u8>)> {
    let (consumed, outcome) = {
        let Mode::Rename { editor, .. } = &mut state.mode else {
            return Ok((1, Vec::new()));
        };
        editor.feed(rest)
    };
    match outcome {
        EditOutcome::Noop => Ok((consumed, Vec::new())),
        EditOutcome::Redraw => Ok((consumed, state.render_modal_overlay())),
        EditOutcome::Cancel => {
            state.mode = Mode::Normal;
            Ok((consumed, state.render_modal_overlay()))
        }
        EditOutcome::Commit => {
            let Mode::Rename { target, editor } = &mut state.mode else {
                return Ok((consumed, Vec::new()));
            };
            let new_title = std::mem::take(&mut editor.buffer);
            let target = match target {
                RenameTarget::Pane(id) => RenameTarget::Pane(id.clone()),
                RenameTarget::Tab(idx) => RenameTarget::Tab(*idx),
            };
            state.mode = Mode::Normal;
            let mut env = state.render_modal_overlay();
            match target {
                RenameTarget::Pane(pane_id) => {
                    if let Some(pane) = state.panes.get_mut(&pane_id) {
                        // Empty rename clears the override so the OSC
                        // title (or default `pN` label) takes over.
                        pane.manual_title = if new_title.is_empty() {
                            None
                        } else {
                            Some(new_title)
                        };
                    }
                    env.extend(state.render_one_chrome(&pane_id));
                    // Pane title is the auto-tab-title source, so the
                    // tab bar may need to re-render too.
                    env.extend(state.render_tabbar());
                }
                RenameTarget::Tab(idx) => {
                    if let Some(tab) = state.tabs.get_mut(idx) {
                        tab.manual_title = if new_title.is_empty() {
                            None
                        } else {
                            Some(new_title)
                        };
                    }
                    // Tab bar gets re-emitted in the next relayout; do
                    // an idempotent one now so the new title shows.
                    env.extend(state.relayout_and_render()?);
                }
            }
            Ok((consumed, env))
        }
    }
}

/// Process one byte while the quit-confirmation modal is up. `y`/`Y`
/// confirms the quit; `n`/`N`/Esc cancels; any other key is ignored so
/// a stray keystroke can neither quit nor dismiss the prompt.
fn handle_confirm_byte(state: &mut State, b: u8) -> Result<Vec<u8>> {
    if !matches!(state.mode, Mode::ConfirmQuit) {
        return Ok(Vec::new());
    }
    match b {
        b'y' | b'Y' => {
            state.quit = true;
            state.mode = Mode::Normal;
            Ok(state.render_modal_overlay())
        }
        b'n' | b'N' | 0x1B => {
            state.mode = Mode::Normal;
            Ok(state.render_modal_overlay())
        }
        _ => Ok(Vec::new()),
    }
}

/// Process one byte while interactive resize mode is up. `h`/`j`/`k`/`l`
/// and arrow keys nudge the focused pane's nearest divider; `q`/Esc/Enter
/// exit. Arrow keys arrive as CSI sequences, buffered (like scroll mode)
/// so the leading ESC isn't mistaken for an exit.
fn handle_resize_byte(state: &mut State, b: u8) -> Result<Vec<u8>> {
    enum Action {
        Nothing,
        Resize(ResizeDir),
        Exit,
    }

    let action = {
        let Mode::Resize { csi_buf } = &mut state.mode else {
            return Ok(Vec::new());
        };
        if csi_buf.is_empty() {
            match b {
                0x1B => {
                    csi_buf.push(b);
                    Action::Nothing
                }
                b'q' | b'\r' | b'\n' => Action::Exit,
                b'h' => Action::Resize(ResizeDir::Left),
                b'l' => Action::Resize(ResizeDir::Right),
                b'k' => Action::Resize(ResizeDir::Up),
                b'j' => Action::Resize(ResizeDir::Down),
                _ => Action::Nothing,
            }
        } else if csi_buf.len() == 1 {
            // After ESC: `[` (or SS3 `O`) continues an arrow sequence;
            // anything else means the ESC was a bare keypress → exit.
            if b == b'[' || b == b'O' {
                csi_buf.push(b);
                Action::Nothing
            } else {
                csi_buf.clear();
                Action::Exit
            }
        } else {
            csi_buf.push(b);
            if (0x40..=0x7E).contains(&b) {
                let act = match b {
                    b'D' => Action::Resize(ResizeDir::Left),
                    b'C' => Action::Resize(ResizeDir::Right),
                    b'A' => Action::Resize(ResizeDir::Up),
                    b'B' => Action::Resize(ResizeDir::Down),
                    _ => Action::Nothing,
                };
                csi_buf.clear();
                act
            } else if csi_buf.len() > 16 {
                csi_buf.clear();
                Action::Nothing
            } else {
                Action::Nothing
            }
        }
    };

    match action {
        Action::Nothing => Ok(Vec::new()),
        Action::Resize(dir) => state.resize_focus(dir),
        Action::Exit => state.exit_resize(),
    }
}

/// Process one byte while the help modal is up. Navigation keys scroll
/// the body; any unrecognised byte dismisses. CSI sequences for arrow
/// keys / PgUp / PgDn are buffered so the leading ESC doesn't dismiss
/// before we know what they are.
fn handle_help_byte(state: &mut State, b: u8) -> Result<Vec<u8>> {
    enum Action {
        Nothing,
        Delta(i64),
        Top,
        Bottom,
        Dismiss,
    }

    let max_offset = {
        let inner_h = help_lines().len() as f32;
        let box_h = (inner_h + 2.0).min(state.host_h.saturating_sub(2) as f32);
        help_body_window(box_h).1
    };

    let action = {
        let Mode::Help { csi_buf, .. } = &mut state.mode else {
            return Ok(Vec::new());
        };
        if csi_buf.is_empty() {
            match b {
                0x1B => {
                    csi_buf.push(b);
                    Action::Nothing
                }
                b'j' => Action::Delta(1),
                b'k' => Action::Delta(-1),
                b'd' | b' ' => Action::Delta(HELP_HALF_PAGE),
                b'u' => Action::Delta(-HELP_HALF_PAGE),
                b'g' => Action::Top,
                b'G' => Action::Bottom,
                _ => Action::Dismiss,
            }
        } else if csi_buf.len() == 1 {
            // After ESC: '[' (CSI) or 'O' (SS3, sent by terminals in
            // application cursor mode) continues an arrow / function
            // sequence; anything else means the ESC was a standalone
            // keystroke → dismiss.
            if b == b'[' || b == b'O' {
                csi_buf.push(b);
                Action::Nothing
            } else {
                csi_buf.clear();
                Action::Dismiss
            }
        } else {
            csi_buf.push(b);
            if (0x40..=0x7E).contains(&b) {
                let act = match csi_buf.as_slice() {
                    [0x1B, b'[', b'A'] | [0x1B, b'O', b'A'] => Action::Delta(-1),
                    [0x1B, b'[', b'B'] | [0x1B, b'O', b'B'] => Action::Delta(1),
                    [0x1B, b'[', b'5', b'~'] => Action::Delta(-HELP_HALF_PAGE * 2),
                    [0x1B, b'[', b'6', b'~'] => Action::Delta(HELP_HALF_PAGE * 2),
                    [0x1B, b'[', b'H'] | [0x1B, b'O', b'H'] => Action::Top,
                    [0x1B, b'[', b'F'] | [0x1B, b'O', b'F'] => Action::Bottom,
                    _ => Action::Dismiss,
                };
                csi_buf.clear();
                act
            } else {
                Action::Nothing
            }
        }
    };

    match action {
        Action::Nothing => Ok(Vec::new()),
        Action::Dismiss => {
            state.mode = Mode::Normal;
            Ok(state.render_modal_overlay())
        }
        other => {
            if let Mode::Help { offset, .. } = &mut state.mode {
                let new_off = match other {
                    Action::Delta(d) => (*offset as i64 + d)
                        .max(0)
                        .min(max_offset as i64) as u32,
                    Action::Top => 0,
                    Action::Bottom => max_offset,
                    _ => *offset,
                };
                if new_off != *offset {
                    *offset = new_off;
                    return Ok(state.render_help_scroll());
                }
            }
            Ok(Vec::new())
        }
    }
}

/// Process one byte while in scroll mode. Recognises:
///   - vim-style: j/k (line down/up), d/u (half-page down/up),
///     space/b (page down/up), g (top), G/q (exit)
///   - arrow keys (`\e[A`/`\e[B`) and PgUp/PgDn (`\e[5~`/`\e[6~`)
///   - bare ESC = exit
///
/// "Up" means scroll *back* into history (offset +); "Down" means scroll
/// toward live (offset −). Sends `SetPortalScrollback` to the host on
/// any change.
fn handle_scroll_byte(state: &mut State, pane_id: &str, b: u8) -> Result<Vec<u8>> {
    enum Action {
        Nothing,
        Delta(i64),
        SetTop,
        SetLive,
        Exit,
        ToPrefix,
    }

    // Read half-page first under a shared borrow.
    let half = match state.panes.get(pane_id).and_then(|p| p.scroll.as_ref()) {
        Some(s) => s.half_page as i64,
        None => return Ok(Vec::new()),
    };

    let action = {
        let Some(s) = state.panes.get_mut(pane_id).and_then(|p| p.scroll.as_mut()) else {
            return Ok(Vec::new());
        };
        let csi_buf = &mut s.csi_buf;
        if csi_buf.is_empty() {
            // Plain key.
            match b {
                0x1B => {
                    csi_buf.push(b);
                    Action::Nothing
                }
                // Prefix byte: keep this pane's scroll state alive but
                // yield to `Mode::Prefix` so prefix commands (tab
                // switches, splits, help, …) are reachable while
                // scrolling. Coming back to the focused pane resumes
                // scroll-key dispatch automatically.
                _ if b == prefix_byte() => Action::ToPrefix,
                b'q' | b'G' => Action::Exit,
                b'k' => Action::Delta(1),
                b'j' => Action::Delta(-1),
                b'u' => Action::Delta(half),
                b'd' => Action::Delta(-half),
                b'b' => Action::Delta(half * 2),
                b' ' => Action::Delta(-half * 2),
                b'g' => Action::SetTop,
                b'0' => Action::SetLive,
                _ => Action::Nothing,
            }
        } else if csi_buf.len() == 1 {
            // After ESC; expecting '[' to continue CSI, otherwise the
            // ESC was a lone keystroke and we exit.
            if b == b'[' {
                csi_buf.push(b);
                Action::Nothing
            } else {
                csi_buf.clear();
                Action::Exit
            }
        } else {
            // Inside CSI; accumulate until a final byte (0x40..=0x7E).
            csi_buf.push(b);
            if (0x40..=0x7E).contains(&b) {
                let act = match csi_buf.as_slice() {
                    [0x1B, b'[', b'A'] => Action::Delta(1),         // Up
                    [0x1B, b'[', b'B'] => Action::Delta(-1),        // Down
                    [0x1B, b'[', b'5', b'~'] => Action::Delta(half * 2), // PgUp
                    [0x1B, b'[', b'6', b'~'] => Action::Delta(-half * 2), // PgDn
                    [0x1B, b'[', b'H'] => Action::SetTop,          // Home
                    [0x1B, b'[', b'F'] => Action::SetLive,         // End
                    _ => Action::Nothing,
                };
                csi_buf.clear();
                act
            } else if csi_buf.len() > 16 {
                csi_buf.clear();
                Action::Nothing
            } else {
                Action::Nothing
            }
        }
    };

    match action {
        Action::Nothing => Ok(Vec::new()),
        Action::Delta(d) => state.scroll_delta(pane_id, d),
        Action::SetTop => state.scroll_set(pane_id, u32::MAX),
        Action::SetLive => state.scroll_set(pane_id, 0),
        Action::Exit => state.exit_scroll(pane_id),
        Action::ToPrefix => {
            // Keep the pane's scroll state intact; just switch the
            // global mode. Prefix commands run, and if focus returns
            // to this pane, scroll-key dispatch resumes.
            state.mode = Mode::Prefix;
            Ok(Vec::new())
        }
    }
}

/// Diagnostic trace of bytes vmux reads from stdin (what veter sent
/// downstream). Enable with `VMUX_DEBUG_INPUT=1`; output to
/// `/tmp/vmux-input.log`.
fn trace_vmux_stdin(bytes: &[u8]) {
    trace_bytes("/tmp/vmux-input.log", "stdin", bytes);
}

/// Diagnostic trace of bytes vmux writes to a focused pane's PTY
/// master. Same env var, separate log file
/// (`/tmp/vmux-output.log`).
fn trace_vmux_to_pane(pane_id: &str, bytes: &[u8]) {
    trace_bytes(
        "/tmp/vmux-output.log",
        &format!("pane:{pane_id}"),
        bytes,
    );
}

fn trace_bytes(path: &str, label: &str, bytes: &[u8]) {
    use std::io::Write;
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let enabled = *ENABLED.get_or_init(|| {
        std::env::var_os("VMUX_DEBUG_INPUT")
            .map(|v| v != "0" && !v.is_empty())
            == Some(true)
    });
    if !enabled {
        return;
    }
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut line = format!(
        "[{:>10}.{:03}] {:>14} {:3} bytes: ",
        ts.as_secs(),
        ts.subsec_millis(),
        label,
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
mod tests {
    use super::*;

    #[test]
    fn parse_prefix_key_variants() {
        // Ctrl+Space — the default — via several spellings.
        for s in ["space", "C-space", "ctrl+space", "@", "C-@", "^@"] {
            assert_eq!(parse_prefix_key(s).unwrap(), 0x00, "{s}");
        }
        // Letters map to their control code, modifier optional, case-insensitive.
        assert_eq!(parse_prefix_key("a").unwrap(), 0x01);
        assert_eq!(parse_prefix_key("C-a").unwrap(), 0x01);
        assert_eq!(parse_prefix_key("ctrl-A").unwrap(), 0x01);
        assert_eq!(parse_prefix_key("^b").unwrap(), 0x02);
        assert_eq!(parse_prefix_key("Z").unwrap(), 0x1a);
        // Non-letter control keys.
        assert_eq!(parse_prefix_key("]").unwrap(), 0x1d);
        // Unsupported keys are rejected.
        assert!(parse_prefix_key("f1").is_err());
        assert!(parse_prefix_key("ab").is_err());
    }

    #[test]
    fn parse_prefix_name_roundtrips() {
        PREFIX_BYTE.store(0x00, Ordering::Relaxed);
        assert_eq!(prefix_name(), "Ctrl+Space");
        PREFIX_BYTE.store(0x01, Ordering::Relaxed);
        assert_eq!(prefix_name(), "Ctrl+A");
        PREFIX_BYTE.store(0x00, Ordering::Relaxed); // restore default
    }

    #[test]
    fn parse_accent_color_variants() {
        assert_eq!(parse_accent_color("blue").unwrap(), 0x56_79_9f_ff);
        assert_eq!(parse_accent_color("#ff8800").unwrap(), 0xff_88_00_ff);
        assert_eq!(parse_accent_color("ff8800").unwrap(), 0xff_88_00_ff);
        assert_eq!(parse_accent_color("#f80").unwrap(), 0xff_88_00_ff);
        assert_eq!(parse_accent_color("#11223344").unwrap(), 0x11_22_33_44);
        assert!(parse_accent_color("nope").is_err());
        assert!(parse_accent_color("#12345").is_err());
        assert!(parse_accent_color("#zz0000").is_err());
    }

    /// Feed a whole keystroke string to a fresh editor seeded with
    /// `start`, returning the final (buffer, cursor) and last outcome.
    fn drive(start: &str, keys: &[u8]) -> (String, usize, &'static str) {
        let mut ed = LineEditor::new(start.to_string());
        let mut idx = 0;
        let mut last = "noop";
        while idx < keys.len() {
            let (consumed, outcome) = ed.feed(&keys[idx..]);
            last = match outcome {
                EditOutcome::Noop => "noop",
                EditOutcome::Redraw => "redraw",
                EditOutcome::Commit => "commit",
                EditOutcome::Cancel => "cancel",
            };
            idx += consumed.max(1);
        }
        (ed.buffer, ed.cursor, last)
    }

    #[test]
    fn line_editor_new_puts_cursor_at_end() {
        let ed = LineEditor::new("hello".to_string());
        assert_eq!(ed.cursor, 5);
        assert_eq!(ed.buffer, "hello");
    }

    #[test]
    fn line_editor_inserts_at_cursor() {
        // Ctrl+A (home) then type "X".
        let (buf, cur, _) = drive("bc", &[0x01, b'X']);
        assert_eq!(buf, "Xbc");
        assert_eq!(cur, 1);
    }

    #[test]
    fn line_editor_home_and_end() {
        let (_, cur, _) = drive("hello", &[0x01]); // Ctrl+A
        assert_eq!(cur, 0);
        let (_, cur, _) = drive("hello", &[0x01, 0x05]); // Ctrl+A then Ctrl+E
        assert_eq!(cur, 5);
    }

    #[test]
    fn line_editor_char_motion_and_forward_delete() {
        // Home, Ctrl+F twice, Ctrl+D removes the char under the cursor.
        let (buf, cur, _) = drive("abcd", &[0x01, 0x06, 0x06, 0x04]);
        assert_eq!(buf, "abd");
        assert_eq!(cur, 2);
    }

    #[test]
    fn line_editor_backspace() {
        let (buf, cur, _) = drive("abc", &[0x7F]);
        assert_eq!(buf, "ab");
        assert_eq!(cur, 2);
    }

    #[test]
    fn line_editor_kill_to_end_and_start() {
        // Home, Ctrl+F (cursor=1), Ctrl+K kills the tail.
        let (buf, _, _) = drive("abcd", &[0x01, 0x06, 0x0B]);
        assert_eq!(buf, "a");
        // Ctrl+E (end) then move left twice, Ctrl+U kills the head.
        let (buf, cur, _) = drive("abcd", &[0x05, 0x02, 0x02, 0x15]);
        assert_eq!(buf, "cd");
        assert_eq!(cur, 0);
    }

    #[test]
    fn line_editor_word_motion() {
        // Alt+B from end jumps to the start of the last word.
        let (_, cur, _) = drive("foo bar", &[0x1B, b'b']);
        assert_eq!(cur, 4);
        // Home, then Alt+F to the end of the first word.
        let (_, cur, _) = drive("foo bar", &[0x01, 0x1B, b'f']);
        assert_eq!(cur, 3);
    }

    #[test]
    fn line_editor_kill_word_back_and_forward() {
        // Ctrl+W at end deletes the last word (and its leading space).
        let (buf, cur, _) = drive("foo bar", &[0x17]);
        assert_eq!(buf, "foo ");
        assert_eq!(cur, 4);
        // Home, Alt+D deletes the first word forward.
        let (buf, cur, _) = drive("foo bar", &[0x01, 0x1B, b'd']);
        assert_eq!(buf, " bar");
        assert_eq!(cur, 0);
    }

    #[test]
    fn line_editor_arrow_keys_and_delete() {
        // CSI Left twice from end, then CSI Delete (ESC [ 3 ~).
        let keys = [0x1B, b'[', b'D', 0x1B, b'[', b'D', 0x1B, b'[', b'3', b'~'];
        let (buf, cur, _) = drive("abcd", &keys);
        assert_eq!(buf, "abd");
        assert_eq!(cur, 2);
    }

    #[test]
    fn line_editor_lone_esc_cancels_but_alt_does_not() {
        let (_, _, last) = drive("abc", &[0x1B]);
        assert_eq!(last, "cancel");
        // ESC immediately followed by 'b' is Alt+B, not a cancel.
        let (_, _, last) = drive("abc", &[0x1B, b'b']);
        assert_eq!(last, "redraw");
    }

    #[test]
    fn line_editor_enter_commits() {
        let (_, _, last) = drive("abc", &[b'\r']);
        assert_eq!(last, "commit");
    }

    #[test]
    fn line_editor_respects_length_cap() {
        let long = "x".repeat(RENAME_MAX_CHARS);
        let (buf, _, _) = drive(&long, &[b'y']);
        assert_eq!(buf.chars().count(), RENAME_MAX_CHARS);
    }

    #[test]
    fn elide_keeps_labels_within_cap() {
        assert_eq!(elide_tab_label("zsh", 10), "zsh");
        assert_eq!(elide_tab_label("exactly-10", 10), "exactly-10");
    }

    #[test]
    fn elide_truncates_labels_over_cap() {
        let out = elide_tab_label("a-very-long-tab-name", 6);
        assert_eq!(out.chars().count(), 6);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn label_cap_does_not_truncate_when_there_is_room() {
        // Three short tabs on a wide bar: the cap covers the longest
        // label, so nothing is elided.
        let labels = vec!["zsh".into(), "vim".into(), "htop".into()];
        let num_widths = vec![3.0, 3.0, 3.0];
        let cap = tabbar_label_cap(&labels, &num_widths, 200.0);
        assert!(cap >= 4, "cap {cap} must not clip a 4-char label");
    }

    #[test]
    fn label_cap_shrinks_to_reclaim_space() {
        // Eight 14-char tabs on an 80-cell bar: the cap drops below the
        // natural length but stays at or above the minimum.
        let labels: Vec<String> =
            (0..8).map(|_| "long-tab-title".to_string()).collect();
        let num_widths = vec![3.0; 8];
        let cap = tabbar_label_cap(&labels, &num_widths, 80.0);
        assert!(cap >= TABBAR_MIN_LABEL);
        assert!(cap < "long-tab-title".chars().count());
    }

    #[test]
    fn label_cap_bottoms_out_at_the_minimum() {
        // Far too many tabs to ever fit: the cap pins to the minimum
        // and the bar is left to scroll.
        let labels: Vec<String> = (0..50).map(|i| format!("tab-{i}")).collect();
        let num_widths = vec![3.0; 50];
        let cap = tabbar_label_cap(&labels, &num_widths, 80.0);
        assert_eq!(cap, TABBAR_MIN_LABEL);
    }

    #[test]
    fn last_visible_shows_every_tab_when_they_fit() {
        let widths = vec![10.0, 10.0, 10.0];
        assert_eq!(tabbar_last_visible(&widths, 100.0, 0), 2);
    }

    #[test]
    fn last_visible_clips_and_reserves_a_right_chevron() {
        // host 35, no left chevron: budget 35-2 (right chevron) = 33 →
        // tabs 0,1,2 fit (30 cells), tab 3 (40) does not.
        let widths = vec![10.0; 10];
        assert_eq!(tabbar_last_visible(&widths, 35.0, 0), 2);
    }

    #[test]
    fn last_visible_reserves_a_left_chevron_too() {
        // first=2 → budget 35-2 (left) -2 (right) = 31 → tabs 2,3,4 fit.
        let widths = vec![10.0; 10];
        assert_eq!(tabbar_last_visible(&widths, 35.0, 2), 4);
    }

    #[test]
    fn window_keeps_active_tab_visible_when_scrolling_right() {
        let widths = vec![10.0; 10];
        let mut scroll = 0;
        let (first, last) = tabbar_window(&widths, 9, 35.0, &mut scroll);
        assert!(
            first <= 9 && 9 <= last,
            "active tab 9 must land in window [{first}, {last}]"
        );
        assert!(first > 0, "the bar must have scrolled forward");
    }

    #[test]
    fn window_jumps_back_when_active_tab_is_left_of_view() {
        let widths = vec![10.0; 10];
        let mut scroll = 7;
        let (first, _) = tabbar_window(&widths, 0, 35.0, &mut scroll);
        assert_eq!(first, 0, "selecting tab 0 must reveal the bar's start");
    }

    #[test]
    fn window_collapses_left_after_tabs_close() {
        // The bar was scrolled to tab 7, then all but 3 tabs closed:
        // the stale offset must collapse back so no space is wasted.
        let widths = vec![10.0; 3];
        let mut scroll = 7;
        let window = tabbar_window(&widths, 2, 35.0, &mut scroll);
        assert_eq!(window, (0, 2));
    }

    #[test]
    fn build_tabbar_does_not_drop_the_active_tab() {
        // 30 tabs, far more than fit: the element must still be built
        // and the scroll offset advanced to reach the active tab.
        let labels: Vec<String> = (0..30).map(|i| format!("tab{i}")).collect();
        let mut scroll = 0;
        let body = build_tabbar_commands(&labels, 29, &mut scroll, 80, 8.0, 16.0, None, &[]);
        assert_eq!(body.id.as_str(), TABBAR_ELEMENT_ID);
        assert!(scroll > 0, "the bar must scroll to reveal the last tab");
        assert!(!body.commands.is_empty());
    }

    #[test]
    fn tabbar_hit_resolves_a_click_to_the_tab_under_it() {
        let labels: Vec<String> =
            vec!["one".into(), "two".into(), "three".into()];
        let mut scroll = 0;
        let layout = compute_tabbar_layout(&labels, 0, &mut scroll, 200, None);
        assert_eq!(layout.slots.len(), 3, "all three tabs fit a wide bar");
        // Every visible tab's midpoint must hit-test back to itself.
        for slot in &layout.slots {
            let mid = slot.x0 + (slot.num_w + slot.name_w) / 2.0;
            assert!(matches!(
                tabbar_hit(&layout, 200, mid),
                Some(TabbarHit::Tab(i)) if i == slot.index
            ));
        }
    }

    #[test]
    fn tabbar_hit_misses_past_the_last_tab() {
        let labels: Vec<String> = vec!["solo".into()];
        let mut scroll = 0;
        let layout = compute_tabbar_layout(&labels, 0, &mut scroll, 200, None);
        let last = &layout.slots[0];
        let past = last.x0 + last.num_w + last.name_w + 1.0;
        assert!(tabbar_hit(&layout, 200, past).is_none());
    }

    #[test]
    fn tabbar_hit_maps_chevrons_to_prev_next() {
        // 30 tabs on an 80-cell bar overflow; with a mid-list active
        // tab both chevrons show and own the bar's edges.
        let labels: Vec<String> = (0..30).map(|i| format!("tab{i}")).collect();
        let mut scroll = 0;
        let layout = compute_tabbar_layout(&labels, 15, &mut scroll, 80, None);
        assert!(layout.has_left && layout.has_right);
        assert!(matches!(
            tabbar_hit(&layout, 80, 0.5),
            Some(TabbarHit::PrevTab)
        ));
        assert!(matches!(
            tabbar_hit(&layout, 80, 79.5),
            Some(TabbarHit::NextTab)
        ));
    }

    #[test]
    fn tabbar_slots_stay_within_the_host_bar() {
        // No slot may extend past the host edge, or a click would fall
        // between a drawn tab and its hit-test extent.
        let labels: Vec<String> = (0..6).map(|i| format!("tab{i}")).collect();
        let mut scroll = 0;
        let layout = compute_tabbar_layout(&labels, 0, &mut scroll, 80, None);
        for slot in &layout.slots {
            assert!(slot.x0 >= 0.0);
            assert!(slot.x0 + slot.num_w + slot.name_w <= 80.0);
        }
    }

    #[test]
    fn build_tabbar_with_session_segment_still_packs_tabs() {
        // With a session segment eating the left edge the bar must
        // still build and keep the active tab visible.
        let labels: Vec<String> = (0..12).map(|i| format!("tab{i}")).collect();
        let mut scroll = 0;
        let body = build_tabbar_commands(
            &labels,
            11,
            &mut scroll,
            80,
            8.0,
            16.0,
            Some("session"),
            &[],
        );
        assert_eq!(body.id.as_str(), TABBAR_ELEMENT_ID);
        assert!(!body.commands.is_empty());
    }

    // ── layout resize ──────────────────────────────────────────────────

    /// `a | b` vertical split (a left, b right) at ratio 0.5 over a
    /// 100×40 region rooted at (0,0).
    fn vsplit_5050() -> (Layout, PaneRect) {
        let layout = Layout::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            a: Box::new(Layout::Leaf("a".into())),
            b: Box::new(Layout::Leaf("b".into())),
        };
        (layout, PaneRect { x: 0, y: 0, w: 100, h: 40 })
    }

    #[test]
    fn resize_moves_the_divider_in_the_arrow_direction_not_per_focus() {
        // The divider follows the arrow regardless of which pane is
        // focused: +cells (Right/Down) raises `a`'s share, −cells the
        // mirror — so a bottom/right pane shrinks on Down/Right and grows
        // on Up/Left, which is the intuitive "push the shared border".
        let (mut layout, bounds) = vsplit_5050();
        assert!(resize_split(&mut layout, "a", SplitDir::Vertical, 10, bounds));
        let Layout::Split { ratio, .. } = &layout else { unreachable!() };
        assert!((*ratio - 0.6).abs() < 1e-6, "focus a, +cells: ratio {ratio} should be 0.6");

        // Same +cells with the *other* pane focused moves the divider the
        // same way (ratio still rises) — sign is direction, not side.
        let (mut layout, bounds) = vsplit_5050();
        assert!(resize_split(&mut layout, "b", SplitDir::Vertical, 10, bounds));
        let Layout::Split { ratio, .. } = &layout else { unreachable!() };
        assert!((*ratio - 0.6).abs() < 1e-6, "focus b, +cells: ratio {ratio} should be 0.6");

        // −cells (Up/Left) grows the bottom/right focused pane.
        let (mut layout, bounds) = vsplit_5050();
        assert!(resize_split(&mut layout, "b", SplitDir::Vertical, -10, bounds));
        let Layout::Split { ratio, .. } = &layout else { unreachable!() };
        assert!((*ratio - 0.4).abs() < 1e-6, "focus b, −cells: ratio {ratio} should be 0.4");
    }

    #[test]
    fn resize_clamps_to_the_ratio_bounds() {
        let (mut layout, bounds) = vsplit_5050();
        // A huge shrink on the left pane pins ratio at the minimum.
        assert!(resize_split(&mut layout, "a", SplitDir::Vertical, -1000, bounds));
        let Layout::Split { ratio, .. } = &layout else { unreachable!() };
        assert!((*ratio - MIN_RATIO).abs() < 1e-6);
    }

    #[test]
    fn resize_ignores_the_wrong_orientation() {
        let (mut layout, bounds) = vsplit_5050();
        // A vertical-only split can't satisfy a height (Horizontal) resize.
        assert!(!resize_split(&mut layout, "a", SplitDir::Horizontal, 10, bounds));
        let Layout::Split { ratio, .. } = &layout else { unreachable!() };
        assert!((*ratio - 0.5).abs() < 1e-6, "ratio must be untouched");
    }

    #[test]
    fn resize_targets_the_innermost_matching_split() {
        // Outer vertical split; its right child is itself a vertical
        // split of c|d. Resizing focused `c` must move the *inner* divider.
        let mut layout = Layout::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            a: Box::new(Layout::Leaf("a".into())),
            b: Box::new(Layout::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                a: Box::new(Layout::Leaf("c".into())),
                b: Box::new(Layout::Leaf("d".into())),
            }),
        };
        let bounds = PaneRect { x: 0, y: 0, w: 100, h: 40 };
        assert!(resize_split(&mut layout, "c", SplitDir::Vertical, 5, bounds));
        let Layout::Split { ratio: outer, b, .. } = &layout else { unreachable!() };
        assert!((*outer - 0.5).abs() < 1e-6, "outer divider must not move");
        let Layout::Split { ratio: inner, .. } = b.as_ref() else { unreachable!() };
        // Inner region is the right half (width 50); +5 cells → +0.1.
        assert!((*inner - 0.6).abs() < 1e-6, "inner ratio {inner} should be 0.6");
    }

    #[test]
    fn separator_hit_grabs_the_divider_and_misses_elsewhere() {
        let (layout, bounds) = vsplit_5050();
        // Divider sits on the edge between cols 49 and 50; both straddling
        // cells grab.
        let mut p = Vec::new();
        let hit = separator_hit(&layout, bounds, 50, 20, &mut p);
        let (path, dir, sbounds) = hit.expect("col 50 must hit the divider");
        assert!(path.is_empty(), "root split → empty path");
        assert!(matches!(dir, SplitDir::Vertical));
        assert_eq!(sbounds.w, 100);
        // A cell deep inside the left pane is not a divider grab.
        let mut p2 = Vec::new();
        assert!(separator_hit(&layout, bounds, 10, 20, &mut p2).is_none());
    }

    #[test]
    fn ratio_at_path_mut_round_trips_with_separator_hit() {
        let (mut layout, bounds) = vsplit_5050();
        let mut p = Vec::new();
        let (path, _, _) = separator_hit(&layout, bounds, 50, 20, &mut p).unwrap();
        *layout.ratio_at_path_mut(&path).unwrap() = 0.25;
        let Layout::Split { ratio, .. } = &layout else { unreachable!() };
        assert!((*ratio - 0.25).abs() < 1e-6);
    }
}
