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
//!   q  quit vmux
//!
//! When the last pane closes, vmux exits.
//!
//! Each pane is backed by a host PRT portal that receives the inner
//! shell's output. The pane's outline (rounded rect) and title strip are
//! drawn with VGE elements. Keystrokes go to the focused pane's PTY
//! master directly — no input crosses the PRT wire (§9.1).

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
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
    EVT_ICON_NAME_CHANGE, EVT_MOUSE_MODE_CHANGE, EVT_RAW_REPLY, EVT_TITLE_CHANGE,
    MARKER_T2C as PRT_MARKER_T2C, RSP_PROBE as PRT_RSP_PROBE,
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
        match dir {
            SplitDir::Vertical => {
                let w_a = ((bounds.w as f32 * ratio).round() as u32).max(1);
                let w_a = w_a.min(bounds.w.saturating_sub(1));
                let w_b = bounds.w - w_a;
                let rect_a = PaneRect {
                    x: bounds.x,
                    y: bounds.y,
                    w: w_a,
                    h: bounds.h,
                };
                let rect_b = PaneRect {
                    x: bounds.x + w_a as i32,
                    y: bounds.y,
                    w: w_b,
                    h: bounds.h,
                };
                let y0 = bounds.y as f32;
                let y1 = bounds.y as f32 + bounds.h as f32;
                if y1 > y0 {
                    out.push(Separator::Vertical {
                        x: bounds.x as f32 + w_a as f32,
                        y0,
                        y1,
                    });
                }
                collect_separators(a, rect_a, out);
                collect_separators(b, rect_b, out);
            }
            SplitDir::Horizontal => {
                let h_a = ((bounds.h as f32 * ratio).round() as u32).max(1);
                let h_a = h_a.min(bounds.h.saturating_sub(1));
                let h_b = bounds.h - h_a;
                let rect_a = PaneRect {
                    x: bounds.x,
                    y: bounds.y,
                    w: bounds.w,
                    h: h_a,
                };
                let rect_b = PaneRect {
                    x: bounds.x,
                    y: bounds.y + h_a as i32,
                    w: bounds.w,
                    h: h_b,
                };
                let x0 = bounds.x as f32;
                let x1 = bounds.x as f32 + bounds.w as f32;
                if x1 > x0 {
                    out.push(Separator::Horizontal {
                        y: bounds.y as f32 + h_a as f32,
                        x0,
                        x1,
                    });
                }
                collect_separators(a, rect_a, out);
                collect_separators(b, rect_b, out);
            }
        }
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
        let lines: Vec<(Point, Point)> = seps
            .into_iter()
            .map(|s| match s {
                Separator::Vertical { x, y0, y1 } => {
                    (Point { x, y: y0 }, Point { x, y: y1 })
                }
                Separator::Horizontal { y, x0, x1 } => {
                    (Point { x: x0, y }, Point { x: x1, y })
                }
            })
            .collect();
        cmds.push(DrawCmd::DrawLines {
            stroke: Style::Flat(COLOR_BRAND),
            line_width: 0.06,
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

fn layout_rects(node: &Layout, bounds: PaneRect, out: &mut HashMap<String, PaneRect>) {
    match node {
        Layout::Leaf(id) => {
            out.insert(id.clone(), bounds);
        }
        Layout::Split { dir, ratio, a, b } => match dir {
            SplitDir::Vertical => {
                let w_a = ((bounds.w as f32 * ratio).round() as u32).max(1);
                let w_a = w_a.min(bounds.w.saturating_sub(1));
                let w_b = bounds.w - w_a;
                let rect_a = PaneRect {
                    x: bounds.x,
                    y: bounds.y,
                    w: w_a,
                    h: bounds.h,
                };
                let rect_b = PaneRect {
                    x: bounds.x + w_a as i32,
                    y: bounds.y,
                    w: w_b,
                    h: bounds.h,
                };
                layout_rects(a, rect_a, out);
                layout_rects(b, rect_b, out);
            }
            SplitDir::Horizontal => {
                let h_a = ((bounds.h as f32 * ratio).round() as u32).max(1);
                let h_a = h_a.min(bounds.h.saturating_sub(1));
                let h_b = bounds.h - h_a;
                let rect_a = PaneRect {
                    x: bounds.x,
                    y: bounds.y,
                    w: bounds.w,
                    h: h_a,
                };
                let rect_b = PaneRect {
                    x: bounds.x,
                    y: bounds.y + h_a as i32,
                    w: bounds.w,
                    h: h_b,
                };
                layout_rects(a, rect_a, out);
                layout_rects(b, rect_b, out);
            }
        },
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

/// Brand color (#3e3b73) shared by separators, the title thumb, the
/// active-tab gradient, and the modal outline. Muted purple — calm
/// enough to recede into chrome but distinctive against the terminal
/// background.
const COLOR_BRAND: Color = Color {
    r: 0x3e as f32 / 255.0,
    g: 0x3b as f32 / 255.0,
    b: 0x73 as f32 / 255.0,
    a: 1.0,
};
/// Same hue as COLOR_BRAND but semi-transparent — used to fill the thumb
/// behind a pane's title text.
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
/// Modal outline uses COLOR_BRAND so the dialog edge picks up the same
/// accent the rest of the chrome uses.
const COLOR_MODAL_OUTLINE: Color = COLOR_BRAND;
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
/// Single VGE element holding all between-pane separator strokes for the
/// active tab. Recreated on every relayout — the layout tree determines
/// which split boundaries to draw.
const SEPARATORS_ELEMENT_ID: &str = "vmux-separators";

/// Build the VGE element body that renders the host's top-row tab bar.
/// Each tab is split into two adjacent sub-rects:
///   - **number rect** (always present, dim modal-background fill, top-
///     left corner rounded) showing the 1-based tab index. Color is
///     fixed regardless of selection so the index reads as a stable
///     anchor.
///   - **name rect** (filled with brand color only when active, top-
///     right corner rounded) showing the tab title.
///
/// The rule along row 1 separates the bar from the pane area below;
/// active tab fills sit flush on top of it.
fn build_tabbar_commands(
    labels: &[String],
    active: usize,
    host_w: u32,
    cell_pw: f32,
    cell_ph: f32,
) -> CreateElementBody {
    // No bar background — row 0 of the host vt100 is left untouched
    // (vmux never writes there) so the terminal's default cell color
    // shows through, matching whatever theme the user runs veter in.
    let mut cmds: Vec<DrawCmd> = Vec::new();

    // Solid rule along the bottom edge of the tab row, separating the
    // bar from the pane area at row 1. Drawn before tab fills so an
    // active tab sits flush on top of it.
    cmds.push(DrawCmd::DrawLines {
        stroke: Style::Flat(COLOR_BRAND),
        line_width: 0.06,
        lines: vec![(
            Point { x: 0.0, y: 1.0 },
            Point { x: host_w as f32, y: 1.0 },
        )],
    });

    let mut x: f32 = 0.0;
    for (i, raw) in labels.iter().enumerate() {
        let num_text = format!(" {} ", i + 1);
        let name_text = format!(" {} ", raw);
        let num_w = num_text.chars().count() as f32;
        let name_w = name_text.chars().count() as f32;
        let total_w = num_w + name_w;
        if x + total_w > host_w as f32 {
            break;
        }
        let is_active = i == active;

        // Number sub-rect: always filled with the dim modal background,
        // top-left corner rounded.
        let (num_rx, num_ry) = chrome_corner_radii(num_w, 1.0, cell_pw, cell_ph);
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(COLOR_MODAL_BG),
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
        cmds.push(DrawCmd::DrawText {
            origin: Point { x, y: 0.0 },
            align: Align::Left,
            // Number text is light regardless of selection so it stays
            // legible on the fixed dim background.
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
                fill: Style::Flat(COLOR_BRAND),
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
        x += total_w;
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
        cmds.push(DrawCmd::FillPath {
            fill: Style::Flat(COLOR_TITLE_THUMB),
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
    prompt: &str,
    buffer: &str,
    cell_pw: f32,
    cell_ph: f32,
) -> CreateElementBody {
    // Center a fixed-size modal box on the host grid. The 4-cell box
    // is (title strip)(body top pad)(buffer)(body bottom pad) — title
    // row is filled with brand color, the rest with the modal bg, and
    // the whole box gets a rounded-edge brand outline that matches the
    // tab and pill styling.
    let line = format!("{prompt}{buffer}_");
    let chars = line.chars().count() as f32;
    let inner_w = chars.max(20.0);
    let box_w = (inner_w + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = 4.0_f32.min(host_h.saturating_sub(2) as f32);

    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let (rx, ry) = chrome_corner_radii(box_w, box_h, cell_pw, cell_ph);
    let cmds = vec![
        // Body fill — full rounded rect; the title strip is drawn over
        // its top region.
        DrawCmd::FillPath {
            fill: Style::Flat(COLOR_MODAL_BG),
            segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
        },
        // Title strip — rounded only on the top corners so the seam
        // with the body fill below is straight.
        DrawCmd::FillPath {
            fill: Style::Flat(COLOR_BRAND),
            segments: rounded_rect_path_corners(
                0.0, 0.0, box_w, 1.0, rx, ry, true, true, false, false,
            ),
        },
        DrawCmd::DrawLinePath {
            stroke: Style::Flat(COLOR_MODAL_OUTLINE),
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
            text: line,
        },
    ];

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
/// `prefix-?`, dismissed by any keystroke.
const HELP_LINES: &[&str] = &[
    "vmux keybindings  —  prefix is Ctrl+Space",
    "",
    "Pane",
    "  v        split focused pane vertically",
    "  h        split focused pane horizontally",
    "  o        cycle focus to next pane",
    "  x        close focused pane",
    "  r        rename focused pane",
    "",
    "Tab",
    "  c        new tab",
    "  n / →    next tab",
    "  p / ←    previous tab",
    "  1..9     jump to tab N",
    "  R        rename current tab",
    "",
    "Scroll  (prefix-[ enters; q/Esc/G exits)",
    "  k / Up        scroll up one line",
    "  j / Down      scroll down one line",
    "  u / d         half page up / down",
    "  PgUp / PgDn   full page up / down",
    "  g / Home      jump to top of scrollback",
    "  0 / End       jump back to live",
    "",
    "Misc",
    "  ?           show this help",
    "  q           quit vmux",
    "  Ctrl+Space  send a literal Ctrl+Space",
    "",
    "j/k or ↑/↓ scroll · any other key dismisses",
];

/// Number of body lines a half-page jump moves through.
const HELP_HALF_PAGE: i64 = 6;

/// Returns (visible body rows, max scroll offset in body lines) given a
/// modal box height. Body lines start at row 2 (after the title strip
/// and a one-row gap) and stop at row `box_h - 1`, leaving a one-row
/// bottom pad. When `box_h` is too small for the gap/pad, body content
/// fills whatever is left.
fn help_body_window(box_h: f32) -> (usize, u32) {
    let body_rows = (box_h as i32 - 3).max(1) as usize;
    let body_lines = HELP_LINES.len().saturating_sub(1);
    let max_offset = body_lines.saturating_sub(body_rows) as u32;
    (body_rows, max_offset)
}

/// Y-coordinate of the scrollbar thumb element's origin (parent-local)
/// for a given scroll `offset`. Centralised so initial creation and
/// `UpdateOrigin`-driven scroll updates compute identical values.
fn help_thumb_origin_y(box_h: f32, offset: u32) -> f32 {
    let (body_rows, max_offset) = help_body_window(box_h);
    let body_lines = HELP_LINES.len().saturating_sub(1);
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
    let max_line = HELP_LINES
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(20) as f32;
    let inner_w = max_line.max(30.0);
    let box_w = (inner_w + 6.0).min(host_w.saturating_sub(2) as f32);
    let inner_h = HELP_LINES.len() as f32;
    let box_h = (inner_h + 2.0).min(host_h.saturating_sub(2) as f32);

    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let (body_rows, max_offset) = help_body_window(box_h);
    let offset = offset.min(max_offset);
    let body_lines = HELP_LINES.len().saturating_sub(1);
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
                fill: Style::Flat(COLOR_BRAND),
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
                text: HELP_LINES[0].to_string(),
            },
            DrawCmd::DrawLinePath {
                stroke: Style::Flat(COLOR_MODAL_OUTLINE),
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
            fill: Style::Flat(COLOR_MODAL_BG),
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
    for i in 1..HELP_LINES.len() {
        let line = HELP_LINES[i];
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
                fill: Style::Flat(COLOR_BRAND),
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

#[derive(Debug)]
enum Mode {
    Normal,
    /// Prefix key (Ctrl+Space) was pressed — next byte is interpreted as a
    /// vmux command.
    Prefix,
    /// Modal text editor for `prefix-r` (pane) or `prefix-R` (tab).
    /// Captures keystrokes until Enter (commit) or Esc (cancel).
    Rename {
        target: RenameTarget,
        buffer: String,
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
}

const PREFIX_BYTE: u8 = 0x00; // Ctrl+Space

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
}

impl State {
    fn new(host_w: u32, host_h: u32, cell_pw: f32, cell_ph: f32) -> Result<Self> {
        let id = "p1".to_string();
        let initial_tab = Tab {
            title: "1".to_string(),
            manual_title: None,
            layout: Layout::Leaf(id.clone()),
            focus: id.clone(),
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
            separators_created: false,
            pending_scrolls: HashMap::new(),
            next_scroll_req_id: SCROLL_REQUEST_ID_BASE,
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
        self.set_focus(next);
        // Re-emit chrome (focused pane changes color) and SetFocus.
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

    /// Recompute the active tab's pane rects, dispatching the diffs as
    /// PRT (CreatePortal/UpdateSize/UpdateOrigin/UpdateVisibility) and
    /// VGE (chrome + tab bar) envelopes. Panes in non-active tabs are
    /// hidden. Idempotent.
    fn relayout_and_render(&mut self) -> Result<Vec<u8>> {
        let mut rects = HashMap::new();
        layout_rects(self.active_layout(), self.full_bounds(), &mut rects);

        let mut prt_cmds: Vec<(PrtCommand, u32)> = Vec::new();
        let mut vge_cmds: Vec<(VgeCommand, u32)> = Vec::new();
        let focus = self.focus().to_string();
        let active_set = self.active_pane_ids();

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
        // need a stroke.
        let sep_body = build_separators_body(self.active_layout(), self.full_bounds());
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
        let tabbar_labels: Vec<String> = (0..self.tabs.len())
            .map(|i| self.tab_effective_title(i))
            .collect();
        let tabbar_body = build_tabbar_commands(
            &tabbar_labels,
            self.active_tab,
            self.host_w,
            self.cell_pw,
            self.cell_ph,
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
        let inner_h = HELP_LINES.len() as f32;
        let box_h = (inner_h + 2.0).min(self.host_h.saturating_sub(2) as f32);
        let (body_rows, max_offset) = help_body_window(box_h);
        let body_lines = HELP_LINES.len().saturating_sub(1);
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
            Mode::Rename { target, buffer } => {
                let title = match target {
                    RenameTarget::Pane(_) => "Rename pane",
                    RenameTarget::Tab(_) => "Rename tab",
                };
                vec![build_modal_commands(
                    self.host_w,
                    self.host_h,
                    title,
                    "",
                    buffer,
                    self.cell_pw,
                    self.cell_ph,
                )]
            }
            Mode::Help { offset, .. } => build_help_modal_elements(
                self.host_w,
                self.host_h,
                *offset,
                self.cell_pw,
                self.cell_ph,
            ),
            Mode::Normal | Mode::Prefix => Vec::new(),
        }
    }

    /// Title to display in `pane_id`'s chrome — usually the pane's own
    /// label, but a `[scroll: N]` indicator while that pane is being
    /// scrolled.
    fn display_title_for(&self, pane_id: &str, raw_title: &str) -> String {
        if let Some(s) = self.panes.get(pane_id).and_then(|p| p.scroll.as_ref()) {
            return format!("[scroll: {}]", s.offset);
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

    /// Re-emit just the tab bar element. Used when an OSC title or a
    /// rename changes the active pane's effective title without
    /// otherwise affecting layout.
    fn render_tabbar(&mut self) -> Vec<u8> {
        let labels: Vec<String> = (0..self.tabs.len())
            .map(|i| self.tab_effective_title(i))
            .collect();
        let body = build_tabbar_commands(
            &labels,
            self.active_tab,
            self.host_w,
            self.cell_pw,
            self.cell_ph,
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
        // mouse reporting (DECSET 1000 + 1006) so veter forwards every
        // wheel/click/drag to us; we hit-test against pane bounds in
        // `handle_mouse_event` and either drive scrollback or
        // re-encode and forward to the inner program's PTY.
        write_all_stdout(
            b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?1000h\x1b[?1006h",
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
                b"\x1b[?1006l\x1b[?1000l\x1b[?1049l\x1b[?25h",
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

fn write_all_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).context("stdout write")?;
    stdout.flush().context("stdout flush")?;
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
fn probe_prt(timeout: Duration) -> Result<bool> {
    let env = build_prt_envelope(&[(PrtCommand::Probe, 0)]);
    write_all_stdout(&env)?;
    let mut apc = PrtApcStream::with_marker(*PRT_MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        if !poll_stdin_for(deadline - now)? {
            return Ok(false);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        let out = apc.feed(&buf[..n]);
        for payload in out.payloads {
            let mut r = PrtReader::new(&payload);
            let _ = r.u8();
            let _ = r.u32();
            if let Ok(ft) = r.u8() {
                if ft == PRT_RSP_PROBE {
                    return Ok(true);
                }
            }
        }
    }
}

struct VgeProbeData {
    cell_pw: u16,
    cell_ph: u16,
}

fn probe_vge(timeout: Duration) -> Result<Option<VgeProbeData>> {
    let env = build_vge_envelope(&[(VgeCommand::Probe, 0)]);
    write_all_stdout(&env)?;
    let mut apc = VgeApcStream::with_marker(*VGE_MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        if !poll_stdin_for(deadline - now)? {
            return Ok(None);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let out = apc.feed(&buf[..n]);
        for payload in out.payloads {
            let mut r = vge_protocol::codec::Reader::new(&payload);
            let _ = r.u8();
            let _ = r.u32();
            let Ok(ft) = r.u8() else { continue };
            if ft != VGE_RSP_PROBE {
                continue;
            }
            let _ = r.u32();
            let _ = r.u32();
            let _proto = r.u16().ok();
            let cw = r.u16().unwrap_or(9);
            let ch = r.u16().unwrap_or(20);
            return Ok(Some(VgeProbeData {
                cell_pw: cw,
                cell_ph: ch,
            }));
        }
    }
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

fn main() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vmux must run with stdin/stdout connected to a terminal");
    }

    init_debug_log();
    install_winch_handler()?;

    let mut tty = TtyGuard::enable()?;
    drain_stale_stdin();

    if !probe_prt(PROBE_TIMEOUT)? {
        bail!(
            "PRT probe timed out — host terminal does not advertise the Portal Extension"
        );
    }
    let vge_probe = probe_vge(PROBE_TIMEOUT)?;
    let (cell_pw, cell_ph) = match vge_probe {
        Some(p) => (p.cell_pw as f32, p.cell_ph as f32),
        None => (9.0, 20.0),
    };

    let (rows, cols) = get_host_winsize()?;
    tty.enter_alt_screen()?;

    let mut state =
        State::new(cols as u32, rows as u32, cell_pw, cell_ph)?;
    // Initial render — creates portal + chrome for p1 and sets focus.
    let env = state.relayout_and_render()?;
    if !env.is_empty() {
        write_all_stdout(&env)?;
    }

    let mut prt_apc = PrtApcStream::with_marker(*PRT_MARKER_T2C);
    let mut vge_apc = VgeApcStream::with_marker(*VGE_MARKER_T2C);
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

        // Build the poll set: stdin + every pane's PTY master.
        let stdin_fd = std::io::stdin().as_raw_fd();
        let stdin_borrowed = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        // Snapshot the pane fd ordering so we can map back to ids after
        // poll returns. (HashMap iteration is unstable; we capture once.)
        let pane_ids: Vec<String> = state.panes.keys().cloned().collect();
        let mut fds: Vec<PollFd<'_>> =
            vec![PollFd::new(stdin_borrowed, PollFlags::POLLIN)];
        let pane_borroweds: Vec<BorrowedFd<'_>> = pane_ids
            .iter()
            .map(|id| {
                let raw = state.panes[id].pty.raw_fd();
                unsafe { BorrowedFd::borrow_raw(raw) }
            })
            .collect();
        for bf in &pane_borroweds {
            fds.push(PollFd::new(*bf, PollFlags::POLLIN));
        }

        let n = match poll(&mut fds, PollTimeout::from(50u16)) {
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("poll: {e}")),
        };
        if n == 0 {
            // Poll idle: nothing arrived for the full 50ms window, so any
            // ESC byte still buffered in the APC parsers' EscPending state
            // is unambiguously a lone keystroke (e.g. dismiss-modal). Push
            // it through so the input handler can act on it without
            // waiting for a follow-up byte that's never coming.
            let prt_flushed = prt_apc.flush_pending_esc();
            let mut pending = vge_apc.feed(&prt_flushed).passthrough;
            pending.extend(vge_apc.flush_pending_esc());
            if !pending.is_empty() {
                process_user_input(&mut state, &pending)?;
            }
            continue;
        }

        // Stdin: host responses + user keystrokes.
        let stdin_revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if stdin_revents.contains(PollFlags::POLLIN) {
            let n = read_stdin(&mut rd_buf)?;
            if n == 0 {
                state.quit = true;
                break;
            }
            dlog("stdin", &rd_buf[..n]);
            handle_stdin_chunk(
                &mut state,
                &mut prt_apc,
                &mut vge_apc,
                &rd_buf[..n],
            )?;
        }

        // Pane PTYs: shell output → WritePortal display path.
        for (i, pid) in pane_ids.iter().enumerate() {
            let revents =
                fds[i + 1].revents().unwrap_or(PollFlags::empty());
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
    bytes: &[u8],
) -> Result<()> {
    let prt_out = prt_apc.feed(bytes);

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
    // auto-tab-title). Cheap — small per-pane VGE elements.
    if !titles_dirty.is_empty() {
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

    // Split mouse events out of the keystroke stream, dispatch each,
    // and forward only non-mouse bytes to the input state machine.
    let (regular, mouse_events) = extract_mouse_events(&vge_out.passthrough);
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

/// Hit-test a mouse event against pane bounds and dispatch:
///   - wheel + inner program doesn't want mouse → drive vmux's
///     scrollback for that pane (auto-enter the pane's scroll state,
///     auto-exit at offset 0);
///   - any other case → forward the event to the matching pane's PTY
///     in SGR encoding, with coords translated to portal-relative.
fn handle_mouse_event(state: &mut State, ev: MouseEvent) -> Result<()> {
    // Find the pane whose `last_rect` contains the event cell. Only
    // panes in the active tab are eligible — panes from inactive tabs
    // keep their `last_rect` from when they were last visible, so a
    // global scan over `state.panes` would land mouse events on hidden
    // panes whose rects overlap the active layout (and HashMap
    // iteration order would make the choice nondeterministic). SGR
    // coords are 1-indexed; PaneRect is 0-indexed.
    let host_col = ev.col.saturating_sub(1) as i32;
    let host_row = ev.row.saturating_sub(1) as i32;
    let active = state.active_pane_ids();
    let mut target: Option<String> = None;
    for (id, pane) in &state.panes {
        if !active.contains(id) {
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

/// Wheel-driven scroll for `pane_id`: enter or extend that pane's
/// scrollback navigation by `delta`. Each pane's scroll state is
/// independent, so wheeling pane B doesn't disturb pane A's scroll.
/// Used when the inner program hasn't enabled mouse reporting, so
/// wheel naturally drives vmux's scrollback for the pane under the
/// cursor.
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
                if b == PREFIX_BYTE {
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
                    .position(|c| *c == PREFIX_BYTE)
                    .map(|p| idx + p)
                    .unwrap_or(bytes.len());
                if let Some(pane) = state.panes.get(&focus_id) {
                    dlog(&format!("key>{focus_id}"), &bytes[idx..stop]);
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
                let env = handle_rename_byte(state, b)?;
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += 1;
            }
            Mode::Help { .. } => {
                let env = handle_help_byte(state, b)?;
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
        b'q' => {
            state.quit = true;
            Ok(Vec::new())
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
                buffer,
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
                buffer,
            };
            Ok(state.render_modal_overlay())
        }
        b'1'..=b'9' => {
            let idx = (b - b'1') as usize;
            state.goto_tab(idx)
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
        PREFIX_BYTE => {
            // Double-tap: forward a literal Ctrl+Space to the focused pane.
            let focus_id = state.focus().to_string();
            if let Some(pane) = state.panes.get(&focus_id) {
                pane.pty.write_all(&[PREFIX_BYTE])?;
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

/// Process one byte while the rename modal is up.
fn handle_rename_byte(state: &mut State, b: u8) -> Result<Vec<u8>> {
    let Mode::Rename { target, buffer } = &mut state.mode else {
        return Ok(Vec::new());
    };
    match b {
        // Enter — commit.
        b'\r' | b'\n' => {
            let new_title = std::mem::take(buffer);
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
            Ok(env)
        }
        // Escape — cancel.
        0x1B => {
            state.mode = Mode::Normal;
            Ok(state.render_modal_overlay())
        }
        // Backspace / DEL.
        0x7F | 0x08 => {
            buffer.pop();
            Ok(state.render_modal_overlay())
        }
        // Printable ASCII. Stay conservative for MVP — non-ASCII is
        // forwarded through but we don't try to decode UTF-8 boundaries.
        0x20..=0x7E => {
            if buffer.chars().count() < 32 {
                buffer.push(b as char);
            }
            Ok(state.render_modal_overlay())
        }
        _ => Ok(Vec::new()),
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
        let inner_h = HELP_LINES.len() as f32;
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
                PREFIX_BYTE => Action::ToPrefix,
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
