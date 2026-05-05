//! vmux — terminal multiplexer over the Portal Extension (PRT) plus
//! Vector Graphics Extension (VGE) for chrome.
//!
//! Run inside a vterm session that advertises both extensions. Default
//! prefix key is **Ctrl+B**. After pressing it once:
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
    EVT_MOUSE_MODE_CHANGE, EVT_RAW_REPLY, MARKER_T2C as PRT_MARKER_T2C,
    RSP_PROBE as PRT_RSP_PROBE,
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
/// Inner grid of a portal placed at `rect`. One cell on each side is
/// eaten by the outline border. The title text overlays the bottom-right
/// portal cell rather than reserving a row of its own. Returns `(rows,
/// cols)`.
fn inner_grid_for(rect: PaneRect) -> (u32, u32) {
    let cols = rect.w.saturating_sub(2).max(1);
    let rows = rect.h.saturating_sub(2).max(1);
    (rows, cols)
}

/// Inner-portal origin within the host grid for `rect`.
fn inner_origin_for(rect: PaneRect) -> (i32, i32) {
    (rect.x + 1, rect.y + 1)
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

struct Pane {
    title: String,
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

/// Magic request id we tag every `SetPortalScrollback` with so the
/// matching `Ok` response (which carries the host-applied offset in its
/// 4-byte body) can be picked out of the response stream.
const SCROLL_REQUEST_ID: u32 = 0x5C_5C_5C_5C;

const COLOR_OUTLINE: Color = Color {
    r: 0.40,
    g: 0.45,
    b: 0.55,
    a: 1.0,
};
const COLOR_OUTLINE_FOCUS: Color = Color {
    r: 0.95,
    g: 0.75,
    b: 0.25,
    a: 1.0,
};
const COLOR_TABBAR_BG: Color = Color {
    r: 0.10,
    g: 0.12,
    b: 0.16,
    a: 1.0,
};
const COLOR_TAB_INACTIVE_TEXT: Color = Color {
    r: 0.55,
    g: 0.58,
    b: 0.65,
    a: 1.0,
};
const COLOR_TAB_ACTIVE_BG: Color = Color {
    r: 0.30,
    g: 0.25,
    b: 0.10,
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
const COLOR_MODAL_BG: Color = Color {
    r: 0.10,
    g: 0.12,
    b: 0.18,
    a: 0.96,
};
const COLOR_MODAL_OUTLINE: Color = Color {
    r: 0.95,
    g: 0.75,
    b: 0.25,
    a: 1.0,
};
const COLOR_MODAL_TEXT: Color = Color {
    r: 0.96,
    g: 0.96,
    b: 0.98,
    a: 1.0,
};

/// Build a closed rounded-rectangle path in cell-units, traversed CCW
/// (matches femtovg's tessellator preference — see brick_drawcmd in
/// tools/breakout).
fn rounded_rect_path(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    rx: f32,
    ry: f32,
) -> Vec<PathSegment> {
    let arc = |dst: Point| PathNode::ArcEllipseTo {
        large: false,
        sweep: false,
        rx,
        ry,
        rotation: 0.0,
        dst,
    };
    vec![PathSegment {
        start: Point { x: x0, y: y0 + ry },
        nodes: vec![
            PathNode::VerticalLineTo { y: y1 - ry },
            arc(Point { x: x0 + rx, y: y1 }),
            PathNode::HorizontalLineTo { x: x1 - rx },
            arc(Point { x: x1, y: y1 - ry }),
            PathNode::VerticalLineTo { y: y0 + ry },
            arc(Point { x: x1 - rx, y: y0 }),
            PathNode::HorizontalLineTo { x: x0 + rx },
            arc(Point { x: x0, y: y0 + ry }),
            PathNode::ClosePath,
        ],
    }]
}

/// VGE element ID used for a pane's chrome (outline + title strip).
fn chrome_element_id(pane_id: &str) -> String {
    format!("vmux-chrome-{pane_id}")
}

const MODAL_ELEMENT_ID: &str = "vmux-modal";
const TABBAR_ELEMENT_ID: &str = "vmux-tabbar";

/// Build the VGE element body that renders the host's top-row tab bar.
/// One label per tab, active tab highlighted with a bg fill + bold.
fn build_tabbar_commands(tabs: &[Tab], active: usize, host_w: u32) -> CreateElementBody {
    let mut cmds: Vec<DrawCmd> = Vec::new();

    // Background bar across the whole row.
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(COLOR_TABBAR_BG),
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w: host_w as f32,
            h: 1.0,
        }],
    });

    let mut x: f32 = 0.0;
    for (i, tab) in tabs.iter().enumerate() {
        let label = format!(" {}: {} ", i + 1, tab.title);
        let label_w = label.chars().count() as f32;
        if x + label_w > host_w as f32 {
            break;
        }
        let is_active = i == active;
        if is_active {
            cmds.push(DrawCmd::FillRectangles {
                fill: Style::Flat(COLOR_TAB_ACTIVE_BG),
                rects: vec![Rect {
                    x,
                    y: 0.0,
                    w: label_w,
                    h: 1.0,
                }],
            });
        }
        cmds.push(DrawCmd::DrawText {
            origin: Point { x, y: 0.0 },
            align: Align::Left,
            fill: Style::Flat(if is_active {
                COLOR_TAB_ACTIVE_TEXT
            } else {
                COLOR_TAB_INACTIVE_TEXT
            }),
            font_style: FontStyle(if is_active { 0x01 } else { 0x00 }),
            text: label,
        });
        x += label_w;
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
fn build_chrome_commands(
    rect: PaneRect,
    title: &str,
    focused: bool,
    cell_pw: f32,
    cell_ph: f32,
    scroll: Option<ScrollIndicator>,
) -> Vec<DrawCmd> {
    // Outline rounded rect spans the entire pane. The half-cell inset
    // keeps the stroke off the very edge so it doesn't get clipped by
    // neighbouring panes.
    let pw = rect.w as f32;
    let ph = rect.h as f32;

    let outline_x0 = 0.5;
    let outline_y0 = 0.5;
    let outline_x1 = pw - 0.5;
    let outline_y1 = ph - 0.5;

    // Compensate the corner radius so the corners look visually circular
    // on anisotropic cell grids (see tools/breakout for the trick).
    let r_cells: f32 = 0.45;
    let rx = r_cells.min((outline_x1 - outline_x0) * 0.4);
    let ry = (r_cells * cell_pw / cell_ph).min((outline_y1 - outline_y0) * 0.4);

    let outline_color = if focused {
        COLOR_OUTLINE_FOCUS
    } else {
        COLOR_OUTLINE
    };
    let mut cmds = vec![DrawCmd::DrawLinePath {
        stroke: Style::Flat(outline_color),
        line_width: 0.06,
        segments: rounded_rect_path(outline_x0, outline_y0, outline_x1, outline_y1, rx, ry),
    }];

    // Title text: the bare label, right-aligned in the bottom-right
    // portal cell with 1 cell of padding from the right border. Drawn at
    // higher draw_order than the portal so it overlays whatever the
    // shell put in that corner.
    let display_title = if title.is_empty() { " " } else { title };
    cmds.push(DrawCmd::DrawText {
        origin: Point {
            x: pw - 2.0,
            y: ph - 2.0,
        },
        align: Align::Right,
        fill: Style::Flat(COLOR_TITLE_TEXT),
        font_style: FontStyle(if focused { 0x01 } else { 0x00 }),
        text: display_title.to_string(),
    });

    // Scrollbar thumb on the right border, vterm-style. Drawn in cell
    // units across the full pane height; sized by visible_rows /
    // (visible_rows + history_depth) and positioned by offset.
    if let Some(s) = scroll {
        // Visible rows = portal interior height (matches inner_grid_for).
        let portal_rows = (ph - 2.0).max(1.0);
        let total = portal_rows + s.history_depth as f32;
        // Track spans almost the full pane height with a small inset
        // matching the outline. Sit a bit clear of the right border
        // (which is the outline stroke at x = pw-0.5) so the thumb
        // doesn't visually merge with it.
        let track_x = pw - 1.30;
        let track_w = 0.35;
        let track_y0 = 0.5;
        let track_y1 = ph - 0.5;
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
    prompt: &str,
    buffer: &str,
    cell_pw: f32,
    cell_ph: f32,
) -> CreateElementBody {
    // Center a fixed-size modal box on the host grid.
    let line = format!("{prompt}{buffer}_");
    let chars = line.chars().count() as f32;
    let inner_w = chars.max(20.0);
    let box_w = (inner_w + 4.0).min(host_w.saturating_sub(2) as f32);
    let box_h = 5.0_f32.min(host_h.saturating_sub(2) as f32);

    let origin_x = ((host_w as f32 - box_w) * 0.5).floor();
    let origin_y = ((host_h as f32 - box_h) * 0.5).floor();

    let rx = 0.6_f32.min(box_w * 0.4);
    let ry = (0.6_f32 * cell_pw / cell_ph).min(box_h * 0.4);

    let cmds = vec![
        DrawCmd::OutlineFillPath {
            fill: Style::Flat(COLOR_MODAL_BG),
            stroke: Style::Flat(COLOR_MODAL_OUTLINE),
            line_width: 0.1,
            segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
        },
        DrawCmd::DrawText {
            origin: Point {
                x: box_w * 0.5,
                y: 1.6,
            },
            align: Align::Center,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(0x01),
            text: "Rename pane".into(),
        },
        DrawCmd::DrawText {
            origin: Point {
                x: box_w * 0.5,
                y: 3.2,
            },
            align: Align::Center,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(0x00),
            text: line,
        },
        DrawCmd::DrawText {
            origin: Point {
                x: box_w * 0.5,
                y: 4.5,
            },
            align: Align::Center,
            fill: Style::Flat(COLOR_MODAL_TEXT),
            font_style: FontStyle(0x00),
            text: "Enter to confirm  ·  Esc to cancel".into(),
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
    "vmux keybindings  —  prefix is Ctrl+B",
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
    "  n        next tab",
    "  p        previous tab",
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
    "  ?        show this help",
    "  q        quit vmux",
    "  Ctrl+B   send a literal Ctrl+B",
    "",
    "press any key to dismiss",
];

fn build_help_modal_body(
    host_w: u32,
    host_h: u32,
    cell_pw: f32,
    cell_ph: f32,
) -> CreateElementBody {
    // Box sized to the longest line + comfortable padding, clamped to
    // the host so it doesn't overflow on very small terminals.
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

    let rx = 0.6_f32.min(box_w * 0.4);
    let ry = (0.6_f32 * cell_pw / cell_ph).min(box_h * 0.4);

    let mut cmds = vec![DrawCmd::OutlineFillPath {
        fill: Style::Flat(COLOR_MODAL_BG),
        stroke: Style::Flat(COLOR_MODAL_OUTLINE),
        line_width: 0.1,
        segments: rounded_rect_path(0.0, 0.0, box_w, box_h, rx, ry),
    }];

    // First line is the title (bold + centered); subsequent lines are
    // left-aligned with monospace alignment.
    for (i, line) in HELP_LINES.iter().enumerate() {
        let row_y = 1.0 + i as f32;
        if i == 0 {
            cmds.push(DrawCmd::DrawText {
                origin: Point {
                    x: box_w * 0.5,
                    y: row_y,
                },
                align: Align::Center,
                fill: Style::Flat(COLOR_MODAL_TEXT),
                font_style: FontStyle(0x01),
                text: (*line).to_string(),
            });
        } else {
            // Section headers (no leading space) get bold; indented
            // body lines stay normal.
            let bold = !line.is_empty() && !line.starts_with(' ');
            cmds.push(DrawCmd::DrawText {
                origin: Point { x: 3.0, y: row_y },
                align: Align::Left,
                fill: Style::Flat(COLOR_MODAL_TEXT),
                font_style: FontStyle(if bold { 0x01 } else { 0x00 }),
                text: (*line).to_string(),
            });
        }
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
    /// Prefix key (Ctrl+B) was pressed — next byte is interpreted as a
    /// vmux command.
    Prefix,
    /// Modal text editor for `prefix-r` (pane) or `prefix-R` (tab).
    /// Captures keystrokes until Enter (commit) or Esc (cancel).
    Rename {
        target: RenameTarget,
        buffer: String,
    },
    /// Help-modal display via `prefix-?`. Any keystroke dismisses it.
    Help,
    /// Scrollback navigation for one pane via `prefix-[`. Drives the
    /// portal's vt100 scrollback offset through PRT
    /// `SetPortalScrollback`. Active pane's chrome shows a status
    /// indicator instead of its title plus a vterm-style scrollbar.
    Scroll {
        pane_id: String,
        offset: u32,
        /// Latest history-depth reported by the host in the
        /// `SetPortalScrollback` ack (§9.3). Used to size the scrollbar
        /// thumb. 0 until the first ack arrives.
        history_depth: u32,
        /// Half the pane's portal-row count, cached on entry. Used for
        /// PgUp/PgDn / d / u "half-page" jumps.
        half_page: u32,
        /// Partial CSI sequence buffer (`\e`, `\e[`, `\e[5`, …). Lets
        /// us recognise multi-byte arrow / PgUp / PgDn sequences.
        csi_buf: Vec<u8>,
    },
}

const PREFIX_BYTE: u8 = 0x02; // Ctrl+B

// ─────────────────────────────────────────────────────────────────────────
// Multiplexer state
// ─────────────────────────────────────────────────────────────────────────

/// A vmux tab: a separate layout tree with its own focus and label.
/// Switching tabs toggles portal/chrome visibility — the portals
/// themselves are shared in `State.panes` and persist across tabs (their
/// inner shells keep running while the tab is hidden, per spec §6.5).
struct Tab {
    title: String,
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
}

impl State {
    fn new(host_w: u32, host_h: u32, cell_pw: f32, cell_ph: f32) -> Result<Self> {
        let id = "p1".to_string();
        let initial_tab = Tab {
            title: "1".to_string(),
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
        };
        let (rows, cols) = inner_grid_for(s.full_bounds());
        let pty = PanePty::spawn(rows as u16, cols as u16)?;
        s.panes.insert(
            id.clone(),
            Pane {
                title: id,
                pty,
                last_rect: None,
                // Pre-record the inner size so the first `relayout_and_render`
                // doesn't redundantly TIOCSWINSZ — that would fire SIGWINCH at
                // the freshly-started shell, prompting some shells (notably
                // bash with `checkwinsize`) to redraw their PS1 and leak an
                // empty line into the pane.
                last_inner: Some((cols, rows)),
                inner_mouse_protocol: 0,
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
                pty,
                last_rect: None,
                last_inner: Some((cols, rows)),
                inner_mouse_protocol: 0,
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
        // If we're scrolling the pane that's about to go away, drop
        // scroll state silently — there's no portal left to render the
        // indicator over and the host will tear it down anyway.
        if let Mode::Scroll { pane_id, .. } = &self.mode {
            if pane_id == target {
                self.mode = Mode::Normal;
            }
        }
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
                pty,
                last_rect: None,
                last_inner: Some((cols, rows)),
                inner_mouse_protocol: 0,
            },
        );
        self.relayout_and_render()
    }

    fn goto_tab(&mut self, index: usize) -> Result<Vec<u8>> {
        if index >= self.tabs.len() || index == self.active_tab {
            return Ok(Vec::new());
        }
        // Switching tabs implicitly exits scroll mode — the scrolled
        // pane is about to be hidden anyway.
        let mut out = Vec::new();
        if matches!(self.mode, Mode::Scroll { .. }) {
            out.extend(self.exit_scroll()?);
        }
        self.active_tab = index;
        out.extend(self.relayout_and_render()?);
        Ok(out)
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
                .map(|p| p.title.clone())
                .unwrap_or_default();
            let display_title = self.display_title_for(pane_id, &pane_title_raw);
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

        // Tab bar: re-create on every relayout so tab adds/removes,
        // active-tab changes, and renames all show up. Cheap — single
        // small VGE element.
        let tabbar_body = build_tabbar_commands(
            &self.tabs,
            self.active_tab,
            self.host_w,
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
        if let Some(body) = self.build_rename_modal_body() {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: MODAL_ELEMENT_ID.into(),
                },
                0,
            ));
            vge_cmds.push((VgeCommand::CreateElement(body), 0));
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
        if let Some(body) = self.build_rename_modal_body() {
            vge_cmds.push((
                VgeCommand::DeleteElement {
                    id: MODAL_ELEMENT_ID.into(),
                },
                0,
            ));
            vge_cmds.push((VgeCommand::CreateElement(body), 0));
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

    /// Build the modal CreateElement body for whichever overlay the
    /// current mode wants up — rename prompt or help — or `None`.
    fn build_rename_modal_body(&self) -> Option<CreateElementBody> {
        match &self.mode {
            Mode::Rename { target, buffer } => {
                let prompt = match target {
                    RenameTarget::Pane(id) => format!("rename pane {id}: "),
                    RenameTarget::Tab(idx) => format!("rename tab {}: ", idx + 1),
                };
                Some(build_modal_commands(
                    self.host_w,
                    self.host_h,
                    &prompt,
                    buffer,
                    self.cell_pw,
                    self.cell_ph,
                ))
            }
            Mode::Help => Some(build_help_modal_body(
                self.host_w,
                self.host_h,
                self.cell_pw,
                self.cell_ph,
            )),
            Mode::Normal | Mode::Prefix | Mode::Scroll { .. } => None,
        }
    }

    /// Title to display in `pane_id`'s chrome — usually the pane's own
    /// label, but a `[scroll: N]` indicator while that pane is being
    /// scrolled.
    fn display_title_for(&self, pane_id: &str, raw_title: &str) -> String {
        if let Mode::Scroll {
            pane_id: scrolled,
            offset,
            ..
        } = &self.mode
        {
            if scrolled == pane_id {
                return format!("[scroll: {offset}]");
            }
        }
        raw_title.to_string()
    }

    /// Scroll-indicator state to draw a thumb in `pane_id`'s chrome,
    /// or `None` if we're not currently scrolling that pane.
    fn scroll_indicator_for(&self, pane_id: &str) -> Option<ScrollIndicator> {
        if let Mode::Scroll {
            pane_id: scrolled,
            offset,
            history_depth,
            ..
        } = &self.mode
        {
            if scrolled == pane_id {
                return Some(ScrollIndicator {
                    offset: *offset,
                    history_depth: *history_depth,
                });
            }
        }
        None
    }

    /// Enter scroll mode for the focused pane. Caches the half-page
    /// step from the pane's portal row count and re-emits the chrome so
    /// the title turns into the scroll indicator.
    fn enter_scroll(&mut self) -> Result<Vec<u8>> {
        let pane_id = self.focus().to_string();
        let rows = self
            .panes
            .get(&pane_id)
            .and_then(|p| p.last_inner)
            .map(|(_cols, rows)| rows)
            .unwrap_or(10);
        let half_page = (rows / 2).max(1);
        self.mode = Mode::Scroll {
            pane_id: pane_id.clone(),
            offset: 0,
            history_depth: 0,
            half_page,
            csi_buf: Vec::new(),
        };
        // Probe the host once for the current history depth so the
        // scrollbar shows up right away (offset stays 0).
        let mut out = build_prt_envelope(&[(
            PrtCommand::SetPortalScrollback {
                id: pane_id.clone(),
                lines: 0,
            },
            SCROLL_REQUEST_ID,
        )]);
        out.extend(self.render_one_chrome(&pane_id));
        Ok(out)
    }

    /// Leave scroll mode: clear scrollback offset on the host, restore
    /// the pane's normal title, return to Normal mode.
    fn exit_scroll(&mut self) -> Result<Vec<u8>> {
        let Mode::Scroll { pane_id, .. } = &self.mode else {
            return Ok(Vec::new());
        };
        let pane_id = pane_id.clone();
        let mut out = Vec::new();
        if self.panes.contains_key(&pane_id) {
            out.extend(build_prt_envelope(&[(
                PrtCommand::SetPortalScrollback {
                    id: pane_id.clone(),
                    lines: 0,
                },
                SCROLL_REQUEST_ID,
            )]));
        }
        self.mode = Mode::Normal;
        out.extend(self.render_one_chrome(&pane_id));
        Ok(out)
    }

    /// Apply a delta to the current scroll offset (positive = scroll
    /// further back, negative = closer to live). Clamped to
    /// `[0, PORTAL_SCROLLBACK_LINES]`.
    fn scroll_delta(&mut self, delta: i64) -> Result<Vec<u8>> {
        let new_offset = match &self.mode {
            Mode::Scroll { offset, .. } => {
                let cur = *offset as i64;
                let next = (cur + delta).max(0);
                (next.min(PORTAL_SCROLLBACK_LINES as i64)) as u32
            }
            _ => return Ok(Vec::new()),
        };
        self.scroll_set(new_offset)
    }

    /// Jump to an absolute scroll offset.
    fn scroll_set(&mut self, mut offset: u32) -> Result<Vec<u8>> {
        if offset > PORTAL_SCROLLBACK_LINES {
            offset = PORTAL_SCROLLBACK_LINES;
        }
        let pane_id = match &mut self.mode {
            Mode::Scroll {
                pane_id,
                offset: cur,
                ..
            } => {
                if *cur == offset {
                    return Ok(Vec::new());
                }
                *cur = offset;
                pane_id.clone()
            }
            _ => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        if self.panes.contains_key(&pane_id) {
            out.extend(build_prt_envelope(&[(
                PrtCommand::SetPortalScrollback {
                    id: pane_id.clone(),
                    lines: offset,
                },
                SCROLL_REQUEST_ID,
            )]));
        }
        out.extend(self.render_one_chrome(&pane_id));
        Ok(out)
    }

    /// Re-emit only the chrome for one specific pane (e.g. after rename).
    fn render_one_chrome(&mut self, pane_id: &str) -> Vec<u8> {
        let (rect, title_raw) = match self.panes.get(pane_id) {
            Some(pane) => match pane.last_rect {
                Some(rect) => (rect, pane.title.clone()),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        let display_title = self.display_title_for(pane_id, &title_raw);
        let scroll_ind = self.scroll_indicator_for(pane_id);
        let cmds = build_chrome_commands(
            rect,
            &display_title,
            pane_id == self.focus(),
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
        // mouse reporting (DECSET 1000 + 1006) so vterm forwards every
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
    let mut latest_ack: Option<(u32, u32)> = None;
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
                && rid == SCROLL_REQUEST_ID
                && body.len() >= 4
            {
                // §9.3 ack: u32 applied_lines, u32 history_depth (the
                // second field is optional per the spec — older hosts
                // can omit it). We hold the most recent values and
                // apply once after the loop to avoid per-frame redraws.
                let applied = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let history = if body.len() >= 8 {
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]])
                } else {
                    0
                };
                latest_ack = Some((applied, history));
            }
        }
    }
    if let Some((applied, history)) = latest_ack {
        if let Mode::Scroll {
            pane_id,
            offset,
            history_depth,
            ..
        } = &mut state.mode
        {
            let changed = *offset != applied || *history_depth != history;
            *offset = applied;
            *history_depth = history;
            if changed {
                let pane_id = pane_id.clone();
                let env = state.render_one_chrome(&pane_id);
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
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
///     scrollback for that pane (auto-enter Mode::Scroll, auto-exit at
///     offset 0);
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

/// Wheel-driven scroll for `pane_id`: enter/exit `Mode::Scroll` as
/// needed and adjust the offset by `delta`. Used when the inner
/// program hasn't enabled mouse reporting, so wheel naturally drives
/// vmux's scrollback for the pane under the cursor.
fn wheel_scroll(state: &mut State, pane_id: &str, delta: i64) -> Result<()> {
    // If we're scrolling a different pane, exit that one first.
    let scrolling_other = matches!(
        &state.mode,
        Mode::Scroll { pane_id: cur, .. } if cur != pane_id
    );
    if scrolling_other {
        let env = state.exit_scroll()?;
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }

    // Wheeling down on a pane that isn't currently being scrolled
    // means "stay at live" — ignore.
    if !matches!(&state.mode, Mode::Scroll { .. }) && delta < 0 {
        return Ok(());
    }

    if !matches!(&state.mode, Mode::Scroll { .. }) {
        // Enter scroll mode for this pane. enter_scroll() uses the
        // current focus, so retarget focus briefly to the wheeled
        // pane, enter, then leave focus where it was — wheel-driven
        // scroll shouldn't steal keyboard focus.
        let saved_focus = state.focus().to_string();
        state.set_focus(pane_id.to_string());
        let env = state.enter_scroll()?;
        state.set_focus(saved_focus);
        if !env.is_empty() {
            write_all_stdout(&env)?;
        }
    }

    let env = state.scroll_delta(delta)?;
    if !env.is_empty() {
        write_all_stdout(&env)?;
    }

    // If we just landed back at live, drop scroll mode so the
    // scrollbar / `[scroll: 0]` indicator clear.
    if matches!(&state.mode, Mode::Scroll { offset: 0, .. }) {
        let env = state.exit_scroll()?;
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
                // Forward the entire residual chunk to the focused pane
                // up to the next prefix byte. This keeps multi-byte
                // sequences (CSI, UTF-8) intact.
                let stop = bytes[idx..]
                    .iter()
                    .position(|c| *c == PREFIX_BYTE)
                    .map(|p| idx + p)
                    .unwrap_or(bytes.len());
                let focus_id = state.focus().to_string();
                if let Some(pane) = state.panes.get(&focus_id) {
                    dlog(&format!("key>{focus_id}"), &bytes[idx..stop]);
                    pane.pty.write_all(&bytes[idx..stop])?;
                }
                idx = stop;
            }
            Mode::Prefix => {
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
            Mode::Help => {
                // Any keystroke dismisses the help modal. The byte is
                // consumed (not forwarded to the focused pane) so a
                // multi-byte sequence pressed by accident doesn't leak
                // through.
                state.mode = Mode::Normal;
                let env = state.render_modal_overlay();
                if !env.is_empty() {
                    write_all_stdout(&env)?;
                }
                idx += 1;
            }
            Mode::Scroll { .. } => {
                let env = handle_scroll_byte(state, b)?;
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
            let buffer = state
                .panes
                .get(&pane_id)
                .map(|p| p.title.clone())
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
            let buffer = state
                .tabs
                .get(idx)
                .map(|t| t.title.clone())
                .unwrap_or_default();
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
            state.mode = Mode::Help;
            Ok(state.render_modal_overlay())
        }
        // scroll mode
        b'[' => state.enter_scroll(),
        PREFIX_BYTE => {
            // Double-tap: forward a literal Ctrl+B to the focused pane.
            let focus_id = state.focus().to_string();
            if let Some(pane) = state.panes.get(&focus_id) {
                pane.pty.write_all(&[PREFIX_BYTE])?;
            }
            Ok(Vec::new())
        }
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
                        if !new_title.is_empty() {
                            pane.title = new_title;
                        }
                    }
                    env.extend(state.render_one_chrome(&pane_id));
                }
                RenameTarget::Tab(idx) => {
                    if let Some(tab) = state.tabs.get_mut(idx) {
                        if !new_title.is_empty() {
                            tab.title = new_title;
                        }
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

/// Process one byte while in scroll mode. Recognises:
///   - vim-style: j/k (line down/up), d/u (half-page down/up),
///     space/b (page down/up), g (top), G/q (exit)
///   - arrow keys (`\e[A`/`\e[B`) and PgUp/PgDn (`\e[5~`/`\e[6~`)
///   - bare ESC = exit
///
/// "Up" means scroll *back* into history (offset +); "Down" means scroll
/// toward live (offset −). Sends `SetPortalScrollback` to the host on
/// any change.
fn handle_scroll_byte(state: &mut State, b: u8) -> Result<Vec<u8>> {
    enum Action {
        Nothing,
        Delta(i64),
        SetTop,
        SetLive,
        Exit,
    }

    // Decide the action under a single mutable borrow scope so we can
    // call back into &mut self helpers afterwards.
    let half = match &state.mode {
        Mode::Scroll { half_page, .. } => *half_page as i64,
        _ => return Ok(Vec::new()),
    };

    let action = {
        let Mode::Scroll { csi_buf, .. } = &mut state.mode else {
            return Ok(Vec::new());
        };
        if csi_buf.is_empty() {
            // Plain key.
            match b {
                0x1B => {
                    csi_buf.push(b);
                    Action::Nothing
                }
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
        Action::Delta(d) => state.scroll_delta(d),
        Action::SetTop => state.scroll_set(u32::MAX),
        Action::SetLive => state.scroll_set(0),
        Action::Exit => state.exit_scroll(),
    }
}
