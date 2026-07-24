//! vfm — a file browser with picture previews for VGE-aware terminals.
//!
//! Navigation is ranger's: the directories you came through stay visible
//! as narrow columns on the left, `h` goes up, `l` goes in, and each
//! directory remembers where the cursor was. The current directory
//! itself is drawn the way a desktop file manager draws one — a grid of
//! tiles, each with a thumbnail for pictures and video and a file-type
//! icon for everything else.
//!
//! Everything on screen is VGE: panes, labels, icons and thumbnails
//! alike. The terminal's text layer always renders *below* VGE elements
//! (VGE §12), so drawing the filenames as `DrawText` is what makes a
//! selection bar behind them possible — and it lets the chrome share
//! vmux's accent and surfaces through `vge-ui`.
//!
//! Decoding and file operations both happen off the event loop: a small
//! pool of decoder threads fills thumbnails in ([`thumbs`]), and a
//! worker runs copies/moves/deletes ([`ops`]), so neither a directory of
//! 4000 JPEGs nor a multi-gigabyte copy stops the grid repainting.

mod entry;
mod icons;
mod layout;
mod ops;
mod preview;
mod render;
mod thumbs;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use vge_protocol::codec::Point;
use vge_protocol::command::{
    Color, Command as VgeCommand, CreateElementBody, DrawCmd, OriginAnchor, UpdateCommandsBody, UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::tty::{
    RawTty, drain_stale_stdin, install_sigwinch, poll_stdin_until, read_stdin, take_sigwinch,
    winsize,
};
use vge_render::{Encoding, choose_encoding, encode_payload, is_ssh_session, run_probe};
use vge_ui::edit::{EditOutcome, LineEditor, NAME_MAX_CHARS};
use vge_ui::input::{Button, Dir, Event, InputParser, Nav};
use vge_ui::modal::{ModalIds, ScrollModal, picker_element, prompt_element};
use vge_ui::picker::{FilterMode, Picker, PickerItem, PickerOutcome};
use vge_ui::theme;

use entry::{Entry, ListOpts, Sort, read_dir};
use layout::{DEFAULT_TILE_ZOOM, Layout, TILE_WIDTHS};
use ops::{Op, Runner};
use thumbs::{Thumbs, stamp_of};

const FRAME_DT: Duration = Duration::from_millis(16);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Long edge of a decoded thumbnail, in pixels. Generous enough for the
/// widest tile on a HiDPI cell grid without turning the cache into a
/// memory hog.
const THUMB_PX: u32 = 256;
/// Uploaded thumbnails kept alive at once. The host advertises 1024
/// images by default; this leaves headroom for whatever else is running
/// in the session.
const MAX_LIVE_THUMBS: usize = 192;
/// How long a status message stays up before the path returns.
const MESSAGE_TTL: Duration = Duration::from_secs(4);

const ID_BG: &str = "vfm.bg";
const ID_COLS: [&str; 2] = ["vfm.col0", "vfm.col1"];
const ID_GRID: &str = "vfm.grid";
const ID_STATUS: &str = "vfm.status";
const MODAL_IDS: ModalIds<'static> = ModalIds {
    root: "vfm.modal",
    body_fill: "vfm.modal.bg",
    body_lines: "vfm.modal.body",
    track: "vfm.modal.track",
    thumb: "vfm.modal.thumb",
};

const ORDER_BG: i32 = 0;
const ORDER_PANE: i32 = 10;
const ORDER_MODAL: i32 = 100;

// ─────────────────────────────────────────────────────────────────────
// Modes
// ─────────────────────────────────────────────────────────────────────

/// What a text prompt is collecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Rename,
    Mkdir,
    Filter,
    Goto,
}

impl PromptKind {
    fn title(self) -> &'static str {
        match self {
            PromptKind::Rename => "Rename",
            PromptKind::Mkdir => "New directory",
            PromptKind::Filter => "Filter",
            PromptKind::Goto => "Go to path",
        }
    }
}

/// Palette / keybinding actions. One enum so the palette and the keymap
/// cannot drift apart — a key binding runs the same [`App::run_cmd`] arm
/// the palette row does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Copy,
    Cut,
    Paste,
    Delete,
    Rename,
    Mkdir,
    Filter,
    ClearFilter,
    ToggleHidden,
    SortName,
    SortSize,
    SortMtime,
    SortExt,
    Reverse,
    Reload,
    ZoomIn,
    ZoomOut,
    Preview,
    MarkAll,
    MarkInvert,
    MarkClear,
    GoParent,
    GoHome,
    GoRoot,
    Goto,
    Help,
    Quit,
}

/// `(name, key hint, action)` for the palette, in display order.
const COMMANDS: &[(&str, &str, Cmd)] = &[
    ("copy", "y", Cmd::Copy),
    ("cut", "d", Cmd::Cut),
    ("paste", "p", Cmd::Paste),
    ("delete", "D", Cmd::Delete),
    ("rename", "r", Cmd::Rename),
    ("mkdir", "m", Cmd::Mkdir),
    ("filter", "/", Cmd::Filter),
    ("clear-filter", "Esc", Cmd::ClearFilter),
    ("toggle-hidden", ".", Cmd::ToggleHidden),
    ("sort-name", "", Cmd::SortName),
    ("sort-size", "", Cmd::SortSize),
    ("sort-mtime", "", Cmd::SortMtime),
    ("sort-ext", "", Cmd::SortExt),
    ("sort-reverse", "s", Cmd::Reverse),
    ("reload", "Ctrl+R", Cmd::Reload),
    ("zoom-in", "+", Cmd::ZoomIn),
    ("zoom-out", "-", Cmd::ZoomOut),
    ("preview", "Enter", Cmd::Preview),
    ("mark-all", "V", Cmd::MarkAll),
    ("mark-invert", "v", Cmd::MarkInvert),
    ("mark-clear", "U", Cmd::MarkClear),
    ("go-parent", "h", Cmd::GoParent),
    ("go-home", "~", Cmd::GoHome),
    ("go-root", "", Cmd::GoRoot),
    ("go-to", "", Cmd::Goto),
    ("help", "?", Cmd::Help),
    ("quit", "q", Cmd::Quit),
];

enum Mode {
    Normal,
    Prompt {
        kind: PromptKind,
        editor: LineEditor,
    },
    /// A y/n question over a pending mutation.
    Confirm {
        title: String,
        op: Op,
    },
    Palette(Picker<Cmd>),
    /// A scrollable body — a file preview or the keybinding help.
    Overlay {
        modal: ScrollModal,
        offset: u32,
    },
}

/// What a pending paste will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipMode {
    Copy,
    Cut,
}

/// One ancestor listing, ready to draw.
struct Column {
    entries: Vec<Entry>,
    /// Index of the child that leads to where we are.
    selected: Option<usize>,
    error: Option<String>,
    path: PathBuf,
}

/// Which panes need their command list pushed this frame.
#[derive(Default, Clone, Copy)]
struct Dirty {
    columns: bool,
    grid: bool,
    status: bool,
    modal: bool,
}

impl Dirty {
    fn all() -> Self {
        Dirty {
            columns: true,
            grid: true,
            status: true,
            modal: true,
        }
    }
}

struct App {
    cwd: PathBuf,
    entries: Vec<Entry>,
    columns: Vec<Column>,
    error: Option<String>,
    cursor: usize,
    scroll_row: usize,
    marks: HashSet<PathBuf>,
    clipboard: Vec<PathBuf>,
    clip_mode: ClipMode,
    /// Where the cursor was in each directory we have visited.
    memory: HashMap<PathBuf, PathBuf>,
    opts: ListOpts,
    zoom: usize,

    cols: u32,
    rows: u32,
    cell_pw: f32,
    cell_ph: f32,
    layout: Layout,

    mode: Mode,
    message: Option<(String, bool, Instant)>,
    thumbs: Thumbs,
    runner: Runner,
    encoding: Encoding,

    /// Ids of the modal elements currently on screen.
    modal_live: Vec<String>,
    /// Set at startup and after a resize: the element tree is rebuilt.
    needs_rebuild: bool,
    dirty: Dirty,
    quit: bool,
}

impl App {
    fn new(cwd: PathBuf, opts: ListOpts, cols: u32, rows: u32, cell_pw: f32, cell_ph: f32) -> Self {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().saturating_sub(1).clamp(1, 4))
            .unwrap_or(2);
        let mut app = App {
            cwd,
            entries: Vec::new(),
            columns: Vec::new(),
            error: None,
            cursor: 0,
            scroll_row: 0,
            marks: HashSet::new(),
            clipboard: Vec::new(),
            clip_mode: ClipMode::Copy,
            memory: HashMap::new(),
            opts,
            zoom: DEFAULT_TILE_ZOOM,
            cols,
            rows,
            cell_pw,
            cell_ph,
            layout: Layout::compute(cols, rows, cell_pw, cell_ph, DEFAULT_TILE_ZOOM, 0),
            mode: Mode::Normal,
            message: None,
            thumbs: Thumbs::new(THUMB_PX, workers, MAX_LIVE_THUMBS),
            runner: Runner::new(),
            encoding: choose_encoding(0x01, is_ssh_session(), 82.0),
            modal_live: Vec::new(),
            needs_rebuild: true,
            dirty: Dirty::all(),
            quit: false,
        };
        app.reload();
        app
    }

    // ── listing ──────────────────────────────────────────────────────

    /// Re-read the current directory and its ancestors, keeping the
    /// cursor on the same entry where possible.
    fn reload(&mut self) {
        let previous = self.entries.get(self.cursor).map(|e| e.path.clone());
        match read_dir(&self.cwd, &self.opts) {
            Ok(v) => {
                self.entries = v;
                self.error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(e.to_string());
            }
        }
        let want = self
            .memory
            .get(&self.cwd)
            .cloned()
            .or(previous)
            .and_then(|p| self.entries.iter().position(|e| e.path == p));
        self.cursor = want.unwrap_or(0);
        self.marks.retain(|p| p.exists());
        self.rebuild_columns();
        self.relayout();
        self.clamp_cursor();
        self.dirty = Dirty::all();
    }

    /// Read the ancestor listings the left-hand columns show.
    fn rebuild_columns(&mut self) {
        self.columns.clear();
        let mut child = self.cwd.clone();
        let mut chain: Vec<(PathBuf, PathBuf)> = Vec::new(); // (dir, child on path)
        while let Some(parent) = child.parent().map(Path::to_path_buf) {
            if parent == child || chain.len() == ID_COLS.len() {
                break;
            }
            chain.push((parent.clone(), child.clone()));
            child = parent;
        }
        // Collected innermost-first; screen order is outermost-first.
        chain.reverse();
        for (dir, on_path) in chain {
            // A hidden directory still has to appear in its parent's
            // column, or the trail we came down would have a gap in it.
            let hidden_child = on_path
                .file_name()
                .map(|n| n.to_string_lossy().starts_with('.'))
                .unwrap_or(false);
            let opts = ListOpts {
                filter: String::new(),
                show_hidden: self.opts.show_hidden || hidden_child,
                ..self.opts.clone()
            };
            let (entries, error) = match read_dir(&dir, &opts) {
                Ok(v) => (v, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            };
            let selected = entries.iter().position(|e| e.path == on_path);
            self.columns.push(Column {
                entries,
                selected,
                error,
                path: dir,
            });
        }
    }

    fn relayout(&mut self) {
        self.layout = Layout::compute(
            self.cols,
            self.rows,
            self.cell_pw,
            self.cell_ph,
            self.zoom,
            self.columns.len(),
        );
        // The layout drops ancestor columns that would squeeze the grid;
        // keep the innermost ones, which are the ones worth showing.
        while self.columns.len() > self.layout.ancestors.len() {
            self.columns.remove(0);
        }
    }

    fn clamp_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.scroll_row = 0;
            return;
        }
        self.cursor = self.cursor.min(self.entries.len() - 1);
        self.scroll_row = self.layout.scroll_for(self.cursor, self.scroll_row);
    }

    fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// The entries an operation applies to: the marks if there are any,
    /// else the entry under the cursor.
    fn targets(&self) -> Vec<PathBuf> {
        if !self.marks.is_empty() {
            let mut v: Vec<PathBuf> = self.marks.iter().cloned().collect();
            v.sort();
            return v;
        }
        self.current().map(|e| e.path.clone()).into_iter().collect()
    }

    // ── navigation ───────────────────────────────────────────────────

    fn move_cursor(&mut self, delta: i64) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() as i64 - 1;
        self.cursor = (self.cursor as i64 + delta).clamp(0, last) as usize;
        self.remember();
        self.clamp_cursor();
        self.dirty.grid = true;
        self.dirty.status = true;
    }

    fn cursor_to(&mut self, index: usize) {
        if index < self.entries.len() {
            self.cursor = index;
            self.remember();
            self.clamp_cursor();
            self.dirty.grid = true;
            self.dirty.status = true;
        }
    }

    /// Note where the cursor is, so coming back here restores it.
    fn remember(&mut self) {
        if let Some(e) = self.entries.get(self.cursor) {
            self.memory.insert(self.cwd.clone(), e.path.clone());
        }
    }

    fn cd(&mut self, path: PathBuf) {
        if !path.is_dir() {
            self.warn(format!("not a directory: {}", path.display()));
            return;
        }
        self.remember();
        self.cwd = path;
        self.opts.filter.clear();
        self.scroll_row = 0;
        self.reload();
    }

    fn go_parent(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return;
        };
        if parent == self.cwd {
            return;
        }
        // Remember which child we came out of, so going back in lands
        // where we were.
        self.memory.insert(parent.clone(), self.cwd.clone());
        self.cwd = parent;
        self.opts.filter.clear();
        self.scroll_row = 0;
        self.reload();
    }

    /// `l` / Enter: descend into a directory, preview anything else.
    fn activate(&mut self) {
        let Some(e) = self.current() else { return };
        if e.is_dir() {
            let path = e.path.clone();
            self.cd(path);
        } else {
            self.open_preview();
        }
    }

    // ── modes ────────────────────────────────────────────────────────

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.dirty.modal = true;
    }

    fn prompt(&mut self, kind: PromptKind, initial: String) {
        let max = if kind == PromptKind::Goto {
            512
        } else {
            NAME_MAX_CHARS
        };
        self.set_mode(Mode::Prompt {
            kind,
            editor: LineEditor::with_max(initial, max),
        });
    }

    fn open_preview(&mut self) {
        let Some(e) = self.current().cloned() else {
            return;
        };
        let width = (self.cols as usize).saturating_sub(8).max(20);
        let lines = preview::lines(&e, width, &self.opts);
        self.set_mode(Mode::Overlay {
            modal: ScrollModal::new(lines),
            offset: 0,
        });
    }

    fn open_help(&mut self) {
        self.set_mode(Mode::Overlay {
            modal: ScrollModal::new(help_lines()),
            offset: 0,
        });
    }

    fn note(&mut self, text: String) {
        self.message = Some((text, false, Instant::now()));
        self.dirty.status = true;
    }

    fn warn(&mut self, text: String) {
        self.message = Some((text, true, Instant::now()));
        self.dirty.status = true;
    }

    // ── commands ─────────────────────────────────────────────────────

    fn run_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Quit => self.quit = true,
            Cmd::Help => self.open_help(),
            Cmd::Preview => self.open_preview(),
            Cmd::Reload => {
                self.reload();
                self.note("reloaded".into());
            }
            Cmd::GoParent => self.go_parent(),
            Cmd::GoHome => {
                if let Some(home) = std::env::var_os("HOME") {
                    self.cd(PathBuf::from(home));
                }
            }
            Cmd::GoRoot => self.cd(PathBuf::from("/")),
            Cmd::Goto => self.prompt(PromptKind::Goto, String::new()),
            Cmd::Filter => self.prompt(PromptKind::Filter, self.opts.filter.clone()),
            Cmd::ClearFilter => {
                if self.opts.filter.is_empty() {
                    self.marks.clear();
                    self.dirty = Dirty::all();
                } else {
                    self.opts.filter.clear();
                    self.reload();
                }
            }
            Cmd::ToggleHidden => {
                self.opts.show_hidden = !self.opts.show_hidden;
                self.reload();
            }
            Cmd::SortName => self.set_sort(Sort::Name),
            Cmd::SortSize => self.set_sort(Sort::Size),
            Cmd::SortMtime => self.set_sort(Sort::Mtime),
            Cmd::SortExt => self.set_sort(Sort::Ext),
            Cmd::Reverse => {
                self.opts.reverse = !self.opts.reverse;
                self.reload();
            }
            Cmd::ZoomIn => self.set_zoom(self.zoom + 1),
            Cmd::ZoomOut => self.set_zoom(self.zoom.saturating_sub(1)),
            Cmd::MarkAll => {
                for e in &self.entries {
                    self.marks.insert(e.path.clone());
                }
                self.dirty = Dirty::all();
            }
            Cmd::MarkInvert => {
                for e in &self.entries {
                    if !self.marks.remove(&e.path) {
                        self.marks.insert(e.path.clone());
                    }
                }
                self.dirty = Dirty::all();
            }
            Cmd::MarkClear => {
                self.marks.clear();
                self.dirty = Dirty::all();
            }
            Cmd::Copy | Cmd::Cut => {
                let targets = self.targets();
                if targets.is_empty() {
                    return;
                }
                let n = targets.len();
                self.clipboard = targets;
                self.clip_mode = if cmd == Cmd::Copy {
                    ClipMode::Copy
                } else {
                    ClipMode::Cut
                };
                self.marks.clear();
                self.note(format!(
                    "{} {n} item{}",
                    if cmd == Cmd::Copy { "copied" } else { "cut" },
                    if n == 1 { "" } else { "s" }
                ));
                self.dirty = Dirty::all();
            }
            Cmd::Paste => {
                if self.clipboard.is_empty() {
                    self.warn("clipboard is empty".into());
                    return;
                }
                let srcs = self.clipboard.clone();
                let dst = self.cwd.clone();
                let op = match self.clip_mode {
                    ClipMode::Copy => Op::Copy { srcs, dst },
                    ClipMode::Cut => Op::Move { srcs, dst },
                };
                if self.clip_mode == ClipMode::Cut {
                    self.clipboard.clear();
                }
                self.submit(op);
            }
            Cmd::Delete => {
                let targets = self.targets();
                if targets.is_empty() {
                    return;
                }
                let title = if targets.len() == 1 {
                    format!("Delete {}?", file_name(&targets[0]))
                } else {
                    format!("Delete {} items?", targets.len())
                };
                self.set_mode(Mode::Confirm {
                    title,
                    op: Op::Delete { paths: targets },
                });
            }
            Cmd::Rename => {
                let Some(e) = self.current() else { return };
                let name = e.name.clone();
                self.prompt(PromptKind::Rename, name);
            }
            Cmd::Mkdir => self.prompt(PromptKind::Mkdir, String::new()),
        }
    }

    fn set_sort(&mut self, sort: Sort) {
        self.opts.sort = sort;
        self.reload();
        self.note(format!("sorted by {}", sort.label()));
    }

    fn set_zoom(&mut self, zoom: usize) {
        let zoom = zoom.min(TILE_WIDTHS.len() - 1);
        if zoom == self.zoom {
            return;
        }
        self.zoom = zoom;
        self.relayout();
        self.clamp_cursor();
        self.dirty = Dirty::all();
    }

    /// Hand an operation to the worker, reporting refusals inline.
    fn submit(&mut self, op: Op) {
        match self.runner.submit(op) {
            Ok(_) => self.dirty.status = true,
            Err(e) => self.warn(e),
        }
    }

    // ── input ────────────────────────────────────────────────────────

    fn on_event(&mut self, ev: Event) {
        match &self.mode {
            Mode::Normal => self.on_normal(ev),
            Mode::Prompt { .. } => self.on_prompt(ev),
            Mode::Confirm { .. } => self.on_confirm(ev),
            Mode::Palette(_) => self.on_palette(ev),
            Mode::Overlay { .. } => self.on_overlay(ev),
        }
    }

    fn on_normal(&mut self, ev: Event) {
        let cols = self.layout.grid_cols as i64;
        let page = self.layout.page() as i64;
        match ev {
            Event::Key('q') | Event::Ctrl('c') => self.quit = true,
            Event::Key('?') => self.run_cmd(Cmd::Help),
            Event::Key(':') => {
                let items = COMMANDS
                    .iter()
                    .map(|(name, hint, cmd)| PickerItem::new(*name, *hint, *cmd))
                    .collect();
                self.set_mode(Mode::Palette(Picker::new(
                    "Command",
                    FilterMode::CommandLine,
                    items,
                )));
            }

            // Motion. Arrows step one entry (the grid reads left to
            // right); j/k and Up/Down move a whole row, as in any icon
            // view. h/l keep ranger's meaning.
            Event::Arrow(Dir::Right) => self.move_cursor(1),
            Event::Arrow(Dir::Left) => self.move_cursor(-1),
            Event::Key('j') | Event::Arrow(Dir::Down) => self.move_cursor(cols),
            Event::Key('k') | Event::Arrow(Dir::Up) => self.move_cursor(-cols),
            Event::Nav(Nav::PageDown) => self.move_cursor(page),
            Event::Nav(Nav::PageUp) => self.move_cursor(-page),
            Event::Nav(Nav::Home) | Event::Key('g') => self.cursor_to(0),
            Event::Nav(Nav::End) | Event::Key('G') => {
                self.cursor_to(self.entries.len().saturating_sub(1))
            }

            Event::Key('h') | Event::Backspace => self.go_parent(),
            Event::Key('l') | Event::Enter => self.activate(),
            Event::Key('~') => self.run_cmd(Cmd::GoHome),

            Event::Key(' ') => {
                if let Some(e) = self.current() {
                    let path = e.path.clone();
                    if !self.marks.remove(&path) {
                        self.marks.insert(path);
                    }
                    self.move_cursor(1);
                    self.dirty = Dirty::all();
                }
            }
            Event::Key('v') => self.run_cmd(Cmd::MarkInvert),
            Event::Key('V') => self.run_cmd(Cmd::MarkAll),
            Event::Key('U') => self.run_cmd(Cmd::MarkClear),
            Event::Key('y') => self.run_cmd(Cmd::Copy),
            Event::Key('d') => self.run_cmd(Cmd::Cut),
            Event::Key('p') => self.run_cmd(Cmd::Paste),
            Event::Key('D') => self.run_cmd(Cmd::Delete),
            Event::Key('r') => self.run_cmd(Cmd::Rename),
            Event::Key('m') => self.run_cmd(Cmd::Mkdir),
            Event::Key('/') => self.run_cmd(Cmd::Filter),
            Event::Key('.') => self.run_cmd(Cmd::ToggleHidden),
            Event::Key('s') => self.run_cmd(Cmd::Reverse),
            Event::Ctrl('r') => self.run_cmd(Cmd::Reload),
            Event::Key('+') | Event::Key('=') => self.run_cmd(Cmd::ZoomIn),
            Event::Key('-') => self.run_cmd(Cmd::ZoomOut),
            Event::Escape => self.run_cmd(Cmd::ClearFilter),

            Event::MouseDown { button, col, row } => self.on_click(button, col, row),
            Event::WheelDown { .. } => self.move_cursor(cols),
            Event::WheelUp { .. } => self.move_cursor(-cols),
            _ => {}
        }
    }

    fn on_click(&mut self, button: Button, col: u16, row: u16) {
        if button != Button::Left {
            return;
        }
        let (c, r) = (col as f32, row as f32);
        if let Some(index) = self
            .layout
            .tile_at(c, r, self.scroll_row, self.entries.len())
        {
            // Clicking the entry already under the cursor opens it — the
            // "second click" a desktop file manager has, without needing
            // double-click timing.
            if index == self.cursor {
                self.activate();
            } else {
                self.cursor_to(index);
            }
            return;
        }
        // A click in an ancestor column navigates to that directory.
        for (i, area) in self.layout.ancestors.iter().enumerate() {
            if area.contains(c, r) {
                let Some(column) = self.columns.get(i) else {
                    return;
                };
                let path = column.path.clone();
                self.cd(path);
                return;
            }
        }
    }

    fn on_prompt(&mut self, ev: Event) {
        let Mode::Prompt { kind, editor } = &mut self.mode else {
            return;
        };
        let kind = *kind;
        let outcome = editor.feed_event(ev);
        let text = editor.buffer.clone();
        match outcome {
            EditOutcome::Noop => {}
            EditOutcome::Redraw => {
                // A filter prompt applies as it is typed, like ranger's.
                if kind == PromptKind::Filter {
                    self.opts.filter = text;
                    self.reload();
                }
                self.dirty.modal = true;
            }
            EditOutcome::Cancel => {
                if kind == PromptKind::Filter {
                    self.opts.filter.clear();
                    self.reload();
                }
                self.set_mode(Mode::Normal);
            }
            EditOutcome::Commit => {
                self.set_mode(Mode::Normal);
                self.commit_prompt(kind, text.trim().to_string());
            }
        }
    }

    fn commit_prompt(&mut self, kind: PromptKind, text: String) {
        match kind {
            PromptKind::Filter => {
                self.opts.filter = text;
                self.reload();
            }
            PromptKind::Mkdir => {
                if text.is_empty() {
                    return;
                }
                let path = self.cwd.join(&text);
                self.submit(Op::Mkdir { path });
            }
            PromptKind::Rename => {
                let Some(e) = self.current() else { return };
                if text.is_empty() || text == e.name {
                    return;
                }
                if text.contains('/') {
                    self.warn("a name cannot contain '/'".into());
                    return;
                }
                let from = e.path.clone();
                self.submit(Op::Rename {
                    from,
                    to: self.cwd.join(&text),
                });
            }
            PromptKind::Goto => {
                if text.is_empty() {
                    return;
                }
                let path = expand_path(&text);
                self.cd(path);
            }
        }
    }

    fn on_confirm(&mut self, ev: Event) {
        match ev {
            Event::Key('y') | Event::Key('Y') | Event::Enter => {
                let Mode::Confirm { op, .. } = &self.mode else {
                    return;
                };
                let op = op.clone();
                self.set_mode(Mode::Normal);
                self.marks.clear();
                self.submit(op);
            }
            Event::Key('n') | Event::Key('N') | Event::Escape | Event::Ctrl('c') => {
                self.set_mode(Mode::Normal);
            }
            _ => {}
        }
    }

    fn on_palette(&mut self, ev: Event) {
        let Mode::Palette(picker) = &mut self.mode else {
            return;
        };
        match picker.feed_event(ev) {
            PickerOutcome::Noop => {}
            PickerOutcome::Redraw => self.dirty.modal = true,
            PickerOutcome::Cancel => self.set_mode(Mode::Normal),
            PickerOutcome::Commit => {
                let cmd = picker.current_item().map(|it| it.payload);
                self.set_mode(Mode::Normal);
                if let Some(cmd) = cmd {
                    self.run_cmd(cmd);
                }
            }
        }
    }

    fn on_overlay(&mut self, ev: Event) {
        let (cols, rows) = (self.cols, self.rows);
        let Mode::Overlay { modal, offset } = &mut self.mode else {
            return;
        };
        let max = modal.max_offset(cols, rows) as i64;
        const HALF_PAGE: i64 = 6;
        let delta = match ev {
            Event::Key('q') | Event::Escape | Event::Ctrl('c') | Event::Enter => {
                self.set_mode(Mode::Normal);
                return;
            }
            Event::Key('j') | Event::Arrow(Dir::Down) => 1,
            Event::Key('k') | Event::Arrow(Dir::Up) => -1,
            Event::Key('d') | Event::Nav(Nav::PageDown) => HALF_PAGE,
            Event::Key('u') | Event::Nav(Nav::PageUp) => -HALF_PAGE,
            Event::WheelDown { .. } => 3,
            Event::WheelUp { .. } => -3,
            Event::Key('g') | Event::Nav(Nav::Home) => -max,
            Event::Key('G') | Event::Nav(Nav::End) => max,
            _ => return,
        };
        let next = (*offset as i64 + delta).clamp(0, max) as u32;
        if next != *offset {
            *offset = next;
            self.dirty.modal = true;
        }
    }

    // ── background work ──────────────────────────────────────────────

    /// The slice of entries currently on screen.
    fn visible_range(&self) -> std::ops::Range<usize> {
        let first = (self.scroll_row * self.layout.grid_cols).min(self.entries.len());
        let last = (first + self.layout.page()).min(self.entries.len());
        first..last
    }

    /// Ask for thumbnails of everything visible, upload whatever the
    /// decoders finished, and drop what no longer fits in the budget.
    fn pump_thumbnails(&mut self, out: &mut impl Write) -> Result<()> {
        for e in &self.entries[self.visible_range()] {
            if e.thumbnailable() {
                let video = e.media() == entry::Media::Video;
                self.thumbs
                    .request(&e.path, stamp_of(e.size, e.mtime), video);
            }
        }

        let decoded = self.thumbs.drain();
        let mut cmds: Vec<(VgeCommand, u32)> = Vec::new();
        for d in decoded {
            let id = self.thumbs.next_image_id();
            let (encoding, payload) = encode_payload(d.rgba, d.w, d.h, self.encoding)?;
            cmds.push((
                VgeCommand::UploadImage(UploadImageBody {
                    id: id.clone(),
                    encoding,
                    width: d.w,
                    height: d.h,
                    total_bytes: payload.len() as u32,
                    chunk_offset: 0,
                    is_last: true,
                    data: payload,
                }),
                REQ_ID_NO_RESPONSE,
            ));
            self.thumbs.ready(d.path, d.stamp, id, d.w, d.h);
            self.dirty.grid = true;
        }

        let keep: Vec<PathBuf> = self.entries[self.visible_range()]
            .iter()
            .map(|e| e.path.clone())
            .collect();
        for id in self.thumbs.evict(&keep) {
            cmds.push((VgeCommand::DropImage { id }, REQ_ID_NO_RESPONSE));
        }
        if !cmds.is_empty() {
            out.write_all(&build_envelope(&cmds))?;
        }
        Ok(())
    }

    /// Report a finished file operation and refresh the listing.
    fn pump_ops(&mut self) {
        if let Some(outcome) = self.runner.poll() {
            if outcome.failed {
                self.warn(outcome.message);
            } else {
                self.note(outcome.message);
            }
            self.reload();
        }
    }

    fn expire_message(&mut self) {
        if let Some((_, _, at)) = &self.message {
            if at.elapsed() > MESSAGE_TTL {
                self.message = None;
                self.dirty.status = true;
            }
        }
    }

    fn resize(&mut self, cols: u32, rows: u32) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.rebuild_columns();
        self.relayout();
        self.clamp_cursor();
        self.needs_rebuild = true;
        self.dirty = Dirty::all();
    }

    // ── rendering ────────────────────────────────────────────────────

    fn column_commands(&self, i: usize) -> Vec<DrawCmd> {
        let (Some(area), Some(col)) = (self.layout.ancestors.get(i), self.columns.get(i)) else {
            return Vec::new();
        };
        render::column_commands(
            &render::ColumnView {
                area: *area,
                entries: &col.entries,
                selected: col.selected,
                error: col.error.as_deref(),
            },
            self.cell_pw,
            self.cell_ph,
        )
    }

    fn grid_commands(&self) -> Vec<DrawCmd> {
        render::grid_commands(&render::GridView {
            layout: &self.layout,
            entries: &self.entries,
            cursor: self.cursor,
            scroll_row: self.scroll_row,
            marks: &self.marks,
            thumbs: &self.thumbs,
            cell_pw: self.cell_pw,
            cell_ph: self.cell_ph,
            error: self.error.as_deref(),
        })
    }

    fn status_commands(&self) -> Vec<DrawCmd> {
        render::status_commands(
            &render::StatusView {
                area: self.layout.status,
                cwd: &self.cwd,
                index: self.cursor,
                total: self.entries.len(),
                marks: self.marks.len(),
                opts: &self.opts,
                clip: (!self.clipboard.is_empty()).then(|| {
                    (
                        self.clipboard.len(),
                        match self.clip_mode {
                            ClipMode::Copy => "copy",
                            ClipMode::Cut => "move",
                        },
                    )
                }),
                message: self
                    .message
                    .as_ref()
                    .map(|(m, failed, _)| (m.as_str(), *failed)),
                busy: self.runner.busy(),
            },
            self.cell_pw,
            self.cell_ph,
        )
    }

    /// The modal elements for the current mode.
    fn modal_elements(&self) -> Vec<CreateElementBody> {
        match &self.mode {
            Mode::Normal => Vec::new(),
            Mode::Prompt { kind, editor } => vec![prompt_element(
                MODAL_IDS.root,
                self.cols,
                self.rows,
                kind.title(),
                &editor.buffer,
                self.cell_pw,
                self.cell_ph,
                Some(editor.cursor),
                ORDER_MODAL,
            )],
            Mode::Confirm { title, .. } => vec![prompt_element(
                MODAL_IDS.root,
                self.cols,
                self.rows,
                title,
                "y = yes     n = cancel",
                self.cell_pw,
                self.cell_ph,
                None,
                ORDER_MODAL,
            )],
            Mode::Palette(picker) => picker_element(
                MODAL_IDS.root,
                self.cols,
                self.rows,
                picker,
                self.cell_pw,
                self.cell_ph,
                ORDER_MODAL,
            ),
            Mode::Overlay { modal, offset } => modal.elements(
                MODAL_IDS,
                self.cols,
                self.rows,
                *offset,
                self.cell_pw,
                self.cell_ph,
                ORDER_MODAL,
            ),
        }
    }

    /// Push whatever changed. Panes are created once and updated in
    /// place; modals are created and deleted as they come and go.
    fn render(&mut self, out: &mut impl Write) -> Result<()> {
        let mut cmds: Vec<(VgeCommand, u32)> = Vec::new();

        if self.needs_rebuild {
            self.needs_rebuild = false;
            self.modal_live.clear();
            // ClearAll drops elements, not uploaded images, so the
            // thumbnail cache survives a resize.
            cmds.push((VgeCommand::ClearAll, REQ_ID_NO_RESPONSE));
            cmds.push(create(ID_BG, render::backdrop(&self.layout), ORDER_BG));
            for (i, id) in ID_COLS.iter().enumerate() {
                cmds.push(create(id, self.column_commands(i), ORDER_PANE));
            }
            cmds.push(create(ID_GRID, self.grid_commands(), ORDER_PANE));
            cmds.push(create(ID_STATUS, self.status_commands(), ORDER_PANE));
            self.dirty = Dirty {
                modal: true,
                ..Default::default()
            };
        }

        if self.dirty.columns {
            for (i, id) in ID_COLS.iter().enumerate() {
                cmds.push(update(id, self.column_commands(i)));
            }
        }
        if self.dirty.grid {
            cmds.push(update(ID_GRID, self.grid_commands()));
        }
        if self.dirty.status {
            cmds.push(update(ID_STATUS, self.status_commands()));
        }
        if self.dirty.modal {
            for id in std::mem::take(&mut self.modal_live) {
                cmds.push((VgeCommand::DeleteElement { id }, REQ_ID_NO_RESPONSE));
            }
            for el in self.modal_elements() {
                self.modal_live.push(el.id.clone());
                cmds.push((VgeCommand::CreateElement(el), REQ_ID_NO_RESPONSE));
            }
        }
        self.dirty = Dirty::default();

        if !cmds.is_empty() {
            out.write_all(&build_envelope(&cmds))?;
            out.flush()?;
        }
        Ok(())
    }
}

fn create(id: &str, commands: Vec<DrawCmd>, draw_order: i32) -> (VgeCommand, u32) {
    (
        VgeCommand::CreateElement(CreateElementBody {
            id: id.to_string(),
            commands,
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order,
            parent: None,
            size: None,
            transform: None,
            anchor: OriginAnchor::Viewport,
        }),
        REQ_ID_NO_RESPONSE,
    )
}

fn update(id: &str, commands: Vec<DrawCmd>) -> (VgeCommand, u32) {
    (
        VgeCommand::UpdateCommands(UpdateCommandsBody {
            id: id.to_string(),
            commands,
        }),
        REQ_ID_NO_RESPONSE,
    )
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Expand `~` and make a relative path absolute against the process cwd.
fn expand_path(text: &str) -> PathBuf {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('~') {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            let rest = rest.trim_start_matches('/');
            if !rest.is_empty() {
                p.push(rest);
            }
            return p;
        }
    }
    let p = PathBuf::from(text);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

fn help_lines() -> Vec<String> {
    [
        "vfm keybindings",
        "",
        "Navigate",
        "  h / Backspace   go to the parent directory",
        "  l / Enter       enter a directory, or preview a file",
        "  ← / →           previous / next entry",
        "  j / k, ↑ / ↓    down / up one row of tiles",
        "  PgUp / PgDn     one screen of tiles",
        "  g / G           first / last entry",
        "  ~               go to your home directory",
        "  click           select; click again to open",
        "",
        "Select",
        "  Space           mark the entry and step on",
        "  v / V / U       invert marks / mark all / clear marks",
        "",
        "Act on the marks (or on the entry under the cursor)",
        "  y               copy",
        "  d               cut",
        "  p               paste into this directory",
        "  D               delete (asks first)",
        "  r               rename",
        "  m               make a directory",
        "",
        "View",
        "  .               show / hide dotfiles",
        "  s               reverse the sort order",
        "  / and Esc       filter by name / clear the filter",
        "  + / -           bigger / smaller tiles",
        "  Ctrl+R          re-read the directory",
        "",
        "Other",
        "  :               command palette (type to filter, Enter runs)",
        "  ?               this help",
        "  q               quit",
        "",
        "j/k or ↑/↓ scroll · q or Esc closes",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Startup
// ─────────────────────────────────────────────────────────────────────

const USAGE: &str = "\
vfm — file browser with picture previews for VGE-aware terminals

usage: vfm [PATH] [options]

options:
  -a, --hidden          show dotfiles from the start
  -A, --accent COLOR    chrome accent (name or #rrggbb); overrides the
                        terminal's themed accent
  -h, --help            show this help
";

struct Args {
    path: PathBuf,
    hidden: bool,
    accent: Option<u32>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut path: Option<PathBuf> = None;
    let mut hidden = false;
    let mut accent = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-a" | "--hidden" => hidden = true,
            "-A" | "--accent" => {
                let v = it.next().ok_or_else(|| anyhow!("--accent needs a value"))?;
                accent = Some(theme::parse_accent_color(&v).map_err(|e| anyhow!(e))?);
            }
            s if s.starts_with("--accent=") => {
                accent = Some(
                    theme::parse_accent_color(&s["--accent=".len()..]).map_err(|e| anyhow!(e))?,
                );
            }
            s if s.starts_with('-') => bail!("unknown option: {s}"),
            s if path.is_none() => path = Some(PathBuf::from(s)),
            s => bail!("unexpected extra argument: {s}"),
        }
    }
    let path = match path {
        Some(p) => p.canonicalize().unwrap_or(p),
        None => std::env::current_dir()?,
    };
    Ok(Some(Args {
        path,
        hidden,
        accent,
    }))
}

/// Restores the terminal on the way out, however we leave.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        // Drop every element we created, then put the screen back.
        let _ = out.write_all(&build_envelope(&[(
            VgeCommand::ClearAll,
            REQ_ID_NO_RESPONSE,
        )]));
        let _ = out.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
}

/// Query the terminal's themed accent with a PRT probe (portal-extension
/// §2.1 / §10), so vfm's chrome tracks veter's theme and, inside a vmux
/// pane, that pane's nesting depth. Best-effort: a terminal that does not
/// speak PRT simply leaves vfm on its built-in accent.
fn probe_host_accent(timeout: Duration) -> Option<Color> {
    use prt_protocol::frame::MARKER_T2C;

    let env = prt_protocol::encode::build_envelope(&[(prt_protocol::command::Command::Probe, 0)]);
    {
        let mut out = std::io::stdout();
        out.write_all(&env).ok()?;
        out.flush().ok()?;
    }

    let mut apc = prt_protocol::ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        if !poll_stdin_until(deadline).ok()? {
            return None;
        }
        let n = read_stdin(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        if let Some(payload) = apc.feed(&buf[..n]).payloads.into_iter().next() {
            return parse_prt_accent(&payload);
        }
    }
}

/// Pull the themed accent out of a PRT ProbeResponse payload. The accent
/// RGBA8 follows the `vge_features` byte (§10) only when the host sets
/// `FEAT_VGE_HOST_THEMED_STYLES`; a short body (older host) reads the
/// trailing fields as absent and yields `None`.
fn parse_prt_accent(payload: &[u8]) -> Option<Color> {
    use prt_protocol::frame::{FEAT_VGE_HOST_THEMED_STYLES, RSP_PROBE};

    let mut r = prt_protocol::Reader::new(payload);
    let _ = r.u8(); // payload protocol_version
    let _ = r.u32(); // payload_length
    if r.u8().ok()? != RSP_PROBE {
        return None;
    }
    let _ = r.u32(); // request_id
    let _ = r.u32(); // body_length
    let _ = r.u16(); // protocol_version
    let _ = r.u32(); // max_portals
    let _ = r.u32(); // max_portal_cells_w
    let _ = r.u32(); // max_portal_cells_h
    let _ = r.u32(); // max_scrollback_lines
    let _ = r.u32(); // max_write_bytes
    let _ = r.u8(); // features
    let _ = r.u8(); // max_nesting_depth
    let vge_features = r.u8().unwrap_or(0); // §10 trailing byte
    if vge_features & FEAT_VGE_HOST_THEMED_STYLES == 0 {
        return None;
    }
    let (red, green, blue, alpha) = (r.u8().ok()?, r.u8().ok()?, r.u8().ok()?, r.u8().ok()?);
    Some(Color {
        r: red as f32 / 255.0,
        g: green as f32 / 255.0,
        b: blue as f32 / 255.0,
        a: alpha as f32 / 255.0,
    })
}

fn term_size() -> (u32, u32) {
    match winsize() {
        Some(ws) if ws.ws_col > 0 && ws.ws_row > 0 => (ws.ws_col as u32, ws.ws_row as u32),
        _ => (80, 24),
    }
}

fn main() -> Result<()> {
    use std::io::IsTerminal;

    let Some(args) = parse_args()? else {
        return Ok(());
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vfm must run with stdin and stdout connected to a terminal");
    }
    if !args.path.is_dir() {
        bail!("not a directory: {}", args.path.display());
    }

    let _raw = RawTty::enable()?;
    let winch = install_sigwinch();
    let mut out = std::io::stdout();
    // Alt screen, hidden cursor, cleared grid, then button-event mouse
    // tracking (?1002) in SGR encoding (?1006) — the pair vplay and
    // vdraw use.
    out.write_all(b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h")?;
    out.flush()?;
    let _term = TermGuard;

    drain_stale_stdin();
    let probe = run_probe(PROBE_TIMEOUT)?.ok_or_else(|| {
        anyhow!("VGE probe timed out — this terminal does not appear to support VGE")
    })?;
    if let Some(rgba) = args.accent {
        theme::set_cli_accent(theme::unpack(rgba));
    } else if let Some(accent) = probe_host_accent(PROBE_TIMEOUT) {
        theme::set_host_accent(accent);
    }

    let (cols, rows) = term_size();
    let opts = ListOpts {
        show_hidden: args.hidden,
        ..ListOpts::default()
    };
    let mut app = App::new(
        args.path,
        opts,
        cols,
        rows,
        probe.cell_pixel_width.max(1) as f32,
        probe.cell_pixel_height.max(1) as f32,
    );
    app.encoding = choose_encoding(probe.supported_image_encodings, is_ssh_session(), 82.0);

    let mut parser = InputParser::new();
    let mut buf = [0u8; 4096];
    while !app.quit {
        let deadline = Instant::now() + FRAME_DT;
        let mut events = Vec::new();
        while poll_stdin_until(deadline)? {
            let n = read_stdin(&mut buf)?;
            if n == 0 {
                app.quit = true;
                break;
            }
            events.extend(parser.feed(&buf[..n]));
        }
        events.extend(parser.flush());
        for ev in events {
            app.on_event(ev);
            if app.quit {
                break;
            }
        }

        if take_sigwinch(winch) {
            let (cols, rows) = term_size();
            app.resize(cols, rows);
        }
        app.pump_ops();
        app.expire_message();
        app.pump_thumbnails(&mut out)?;
        app.render(&mut out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfm-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn app_in(dir: &Path) -> App {
        App::new(dir.to_path_buf(), ListOpts::default(), 120, 40, 9.0, 20.0)
    }

    #[test]
    fn every_palette_command_has_a_unique_name() {
        let mut names: Vec<&str> = COMMANDS.iter().map(|(n, _, _)| *n).collect();
        names.sort_unstable();
        let mut dedup = names.clone();
        dedup.dedup();
        assert_eq!(names, dedup);
    }

    #[test]
    fn navigation_remembers_the_cursor_per_directory() {
        let d = tmpdir("memory");
        std::fs::create_dir(d.join("a")).unwrap();
        std::fs::create_dir(d.join("b")).unwrap();
        std::fs::write(d.join("b/inner.txt"), b"x").unwrap();

        let mut app = app_in(&d);
        assert_eq!(app.entries.len(), 2);
        app.move_cursor(1);
        assert_eq!(app.current().unwrap().name, "b");
        app.activate(); // into b/
        assert_eq!(app.cwd, d.join("b"));
        app.go_parent();
        assert_eq!(app.cwd, d);
        assert_eq!(app.current().unwrap().name, "b", "cursor restored");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn going_up_selects_the_directory_we_came_from() {
        let d = tmpdir("upsel");
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        let mut app = app_in(&d.join("sub/deeper"));
        app.go_parent();
        assert_eq!(app.cwd, d.join("sub"));
        assert_eq!(app.current().unwrap().name, "deeper");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn ancestor_columns_track_the_path() {
        let d = tmpdir("cols");
        std::fs::create_dir_all(d.join("one/two")).unwrap();
        let app = app_in(&d.join("one/two"));
        assert_eq!(app.columns.len(), 2);
        // The innermost column is the immediate parent, and it points at
        // the directory we are in.
        let inner = app.columns.last().unwrap();
        assert_eq!(inner.path, d.join("one"));
        assert_eq!(inner.entries[inner.selected.unwrap()].name, "two");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn marks_drive_operations_and_fall_back_to_the_cursor() {
        let d = tmpdir("marks");
        std::fs::write(d.join("a.txt"), b"a").unwrap();
        std::fs::write(d.join("b.txt"), b"b").unwrap();
        let mut app = app_in(&d);

        assert_eq!(app.targets().len(), 1, "just the cursor");
        app.on_event(Event::Key(' ')); // mark a.txt, step to b.txt
        assert_eq!(app.marks.len(), 1);
        app.on_event(Event::Key(' ')); // mark b.txt
        assert_eq!(app.targets().len(), 2);
        app.run_cmd(Cmd::MarkClear);
        assert_eq!(app.targets().len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn delete_asks_before_doing_anything() {
        let d = tmpdir("confirm");
        std::fs::write(d.join("a.txt"), b"a").unwrap();
        let mut app = app_in(&d);
        app.run_cmd(Cmd::Delete);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        app.on_event(Event::Key('n'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(d.join("a.txt").exists(), "cancelling touches nothing");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_filter_prompt_narrows_the_listing_as_it_is_typed() {
        let d = tmpdir("filter");
        std::fs::write(d.join("photo.png"), b"a").unwrap();
        std::fs::write(d.join("notes.txt"), b"b").unwrap();
        let mut app = app_in(&d);
        app.run_cmd(Cmd::Filter);
        for c in "photo".chars() {
            app.on_event(Event::Key(c));
        }
        assert_eq!(app.entries.len(), 1);
        app.on_event(Event::Escape);
        assert_eq!(app.entries.len(), 2, "cancelling restores the listing");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hidden_files_toggle() {
        let d = tmpdir("hidden");
        std::fs::write(d.join(".rc"), b"a").unwrap();
        std::fs::write(d.join("visible"), b"b").unwrap();
        let mut app = app_in(&d);
        assert_eq!(app.entries.len(), 1);
        app.run_cmd(Cmd::ToggleHidden);
        assert_eq!(app.entries.len(), 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_cursor_never_leaves_the_listing() {
        let d = tmpdir("clamp");
        for i in 0..3 {
            std::fs::write(d.join(format!("f{i}")), b"x").unwrap();
        }
        let mut app = app_in(&d);
        app.move_cursor(-10);
        assert_eq!(app.cursor, 0);
        app.move_cursor(1000);
        assert_eq!(app.cursor, 2);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_empty_directory_has_no_targets_and_does_not_panic() {
        let d = tmpdir("empty");
        let mut app = app_in(&d);
        assert!(app.entries.is_empty());
        assert!(app.targets().is_empty());
        app.move_cursor(1);
        app.activate();
        app.run_cmd(Cmd::Delete);
        assert!(matches!(app.mode, Mode::Normal), "nothing to confirm");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn preview_opens_for_a_file_and_scrolls() {
        let d = tmpdir("preview");
        std::fs::write(d.join("a.txt"), "line\n".repeat(200)).unwrap();
        let mut app = app_in(&d);
        app.on_event(Event::Enter);
        assert!(matches!(app.mode, Mode::Overlay { .. }));
        app.on_event(Event::Key('j'));
        let Mode::Overlay { offset, .. } = &app.mode else {
            panic!("still an overlay");
        };
        assert_eq!(*offset, 1);
        app.on_event(Event::Key('q'));
        assert!(matches!(app.mode, Mode::Normal));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_palette_runs_the_command_it_committed() {
        let d = tmpdir("palette");
        std::fs::write(d.join(".rc"), b"x").unwrap();
        let mut app = app_in(&d);
        app.on_event(Event::Key(':'));
        assert!(matches!(app.mode, Mode::Palette(_)));
        for c in "toggle-hidden".chars() {
            app.on_event(Event::Key(c));
        }
        app.on_event(Event::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.opts.show_hidden);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_click_selects_and_a_second_click_opens() {
        let d = tmpdir("click");
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("z.txt"), b"x").unwrap();
        let mut app = app_in(&d);
        // Tile 1 is z.txt — directories sort first.
        let (x, y) = app.layout.tile_origin(1, 0);
        let click = Event::MouseDown {
            button: Button::Left,
            col: x as u16,
            row: y as u16,
        };
        app.on_event(click);
        assert_eq!(app.cursor, 1);
        app.on_event(click);
        assert!(matches!(app.mode, Mode::Overlay { .. }), "opened a preview");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn zoom_stays_within_the_available_tile_sizes() {
        let d = tmpdir("zoom");
        let mut app = app_in(&d);
        for _ in 0..10 {
            app.run_cmd(Cmd::ZoomIn);
        }
        assert_eq!(app.zoom, TILE_WIDTHS.len() - 1);
        for _ in 0..10 {
            app.run_cmd(Cmd::ZoomOut);
        }
        assert_eq!(app.zoom, 0);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn rename_rejects_a_name_with_a_slash() {
        let d = tmpdir("rename");
        std::fs::write(d.join("a.txt"), b"x").unwrap();
        let mut app = app_in(&d);
        app.commit_prompt(PromptKind::Rename, "sub/a.txt".into());
        assert!(app.message.as_ref().unwrap().1, "reported as a failure");
        assert!(d.join("a.txt").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_resize_rebuilds_the_element_tree() {
        let d = tmpdir("resize");
        let mut app = app_in(&d);
        app.needs_rebuild = false;
        app.resize(80, 24);
        assert!(app.needs_rebuild);
        assert_eq!(app.layout.cols, 80);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
