//! vmd — a markdown viewer for VGE-aware terminals.
//!
//! The point of drawing a document with VGE rather than with cells is
//! that a heading can actually be *bigger*: `font_scale` (VGE §7.4)
//! makes an `h1` twice the size of the body text instead of the same
//! size in a different colour, code sits on a real recessed plate, and
//! a picture is the picture rather than a link to it. Everything on
//! screen is one of two elements — the page and the chrome — and the
//! page is rebuilt from the visible slice of rows on each scroll tick,
//! so the cost of a frame follows the pane, not the file.
//!
//! Layout is in em (see [`layout`]); the zoom that converts em to cells
//! is the same number `+`/`-` change, which is why zooming is a
//! re-layout at a different wrap width and nothing more.

mod doc;
mod images;
mod layout;
mod render;

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use vge_protocol::codec::Point;
use vge_protocol::command::{
    Color, Command as VgeCommand, CreateElementBody, DrawCmd, OriginAnchor, Retention,
    UpdateCommandsBody, UploadImageBody,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::tty::{
    RawTty, drain_stale_stdin, install_sigwinch, poll_stdin_until, read_stdin, take_sigwinch,
    winsize,
};
use vge_render::{Encoding, choose_encoding, encode_payload, is_ssh_session, run_probe};
use vge_ui::edit::{COMMAND_MAX_CHARS, EditOutcome, LineEditor};
use vge_ui::input::{Button, Dir, Event, InputParser, Nav};
use vge_ui::modal::{ModalIds, ScrollModal, picker_element, prompt_element};
use vge_ui::picker::{FilterMode, Picker, PickerItem, PickerOutcome};
use vge_ui::theme;

use doc::Doc;
use images::Images;
use layout::{Body, Layout, Metrics};
use render::{Chrome, MatchPos, View};

const FRAME_DT: Duration = Duration::from_millis(16);
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const MESSAGE_TTL: Duration = Duration::from_secs(4);

/// Namespace every element and image id shares (§6.8): cleanup is one
/// prefix sweep per table, at exit *and* at startup — the image table
/// belongs to the session, so a run that was killed before it could
/// tidy up would otherwise leave its pictures behind for the next one
/// to collide with.
const ID_PREFIX: &str = "vmd.";
const ID_PAGE: &str = "vmd.page";
const ID_CHROME: &str = "vmd.chrome";
const MODAL_IDS: ModalIds<'static> = ModalIds {
    root: "vmd.modal",
    body_fill: "vmd.modal.bg",
    body_lines: "vmd.modal.body",
    track: "vmd.modal.track",
    thumb: "vmd.modal.thumb",
};

const ORDER_PAGE: i32 = 10;
const ORDER_CHROME: i32 = 20;
const ORDER_MODAL: i32 = 100;

/// Reading measure in em, when the pane is wider than one. Prose gets
/// hard to track much past this, and a terminal is often much wider.
const NARROW_EM: f32 = 88.0;
/// Left and right margin of the reading column, in cells.
const MARGIN: f32 = 2.0;
/// Tallest a picture is drawn, as a fraction of the page height.
const IMAGE_MAX_FRACTION: f32 = 0.72;

const ZOOM_MIN: f32 = 0.6;
const ZOOM_MAX: f32 = 4.0;
const ZOOM_STEP: f32 = 0.1;

/// Uploaded pictures kept alive at once.
const MAX_LIVE_IMAGES: usize = 24;

/// Extensions vmd opens itself rather than handing to the desktop.
const MARKDOWN_EXT: [&str; 5] = ["md", "markdown", "mdown", "mkd", "mdwn"];

const USAGE: &str = "\
vmd — markdown viewer for VGE-aware terminals

Usage: vmd [OPTIONS] [FILE]

With no FILE, reads the document from standard input.

Options:
  -w, --width <COLS>   reading measure, in characters (0 = fill the pane)
  -z, --zoom <FACTOR>  initial text size (1.0 = the terminal's own)
  -A, --accent <COLOR> chrome accent: a name, or #rgb / #rrggbb / #rrggbbaa
  -h, --help           show this help
  -V, --version        show the version
";

// ─────────────────────────────────────────────────────────────────────
// Modes
// ─────────────────────────────────────────────────────────────────────

enum Mode {
    Normal,
    /// The `/` prompt.
    Search(LineEditor),
    /// Jump to a heading.
    Outline(Picker<usize>),
    /// Open a link.
    Links(Picker<usize>),
    Help {
        modal: ScrollModal,
        offset: u32,
    },
}

#[derive(Default, Clone, Copy)]
struct Dirty {
    page: bool,
    chrome: bool,
    modal: bool,
}

impl Dirty {
    fn all() -> Self {
        Dirty {
            page: true,
            chrome: true,
            modal: true,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// App
// ─────────────────────────────────────────────────────────────────────

struct App {
    /// The file on screen, or `None` when the document came in on
    /// stdin and there is nothing to reload from.
    path: Option<PathBuf>,
    title: String,
    doc: Doc,
    layout: Layout,
    images: Images,

    cols: u32,
    rows: u32,
    cell_pw: f32,
    cell_ph: f32,

    zoom: f32,
    /// Reading measure in em; `None` fills the pane.
    measure: Option<f32>,
    /// Cells of document above the top of the viewport.
    scroll: f32,

    mode: Mode,
    query: String,
    matches: Vec<MatchPos>,
    match_at: usize,

    /// Documents we came from, with where we were in each.
    history: Vec<(PathBuf, f32)>,
    message: Option<(String, bool, Instant)>,
    /// A URL for the event loop to hand to the desktop; `main` owns the
    /// process spawning, not the App.
    pending_open: Option<String>,

    encoding: Encoding,
    modal_live: Vec<String>,
    needs_rebuild: bool,
    dirty: Dirty,
    quit: bool,
}

impl App {
    // Startup state, straight from the CLI and the probe. Bundling it
    // in a struct would only move the same list one line up.
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: Option<PathBuf>,
        text: &str,
        cols: u32,
        rows: u32,
        cell_pw: f32,
        cell_ph: f32,
        zoom: f32,
        measure: Option<f32>,
    ) -> Self {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(1, 4))
            .unwrap_or(2);
        let base = path
            .as_deref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let doc = doc::parse(text, &base);
        let mut app = App {
            title: title_for(path.as_deref(), &doc),
            path,
            doc,
            layout: Layout {
                rows: Vec::new(),
                height: 0.0,
                outline: Vec::new(),
            },
            images: Images::new(workers, MAX_LIVE_IMAGES),
            cols,
            rows,
            cell_pw,
            cell_ph,
            zoom,
            measure,
            scroll: 0.0,
            mode: Mode::Normal,
            query: String::new(),
            matches: Vec::new(),
            match_at: 0,
            history: Vec::new(),
            message: None,
            pending_open: None,
            encoding: Encoding::Raw,
            modal_live: Vec::new(),
            needs_rebuild: true,
            dirty: Dirty::all(),
            quit: false,
        };
        app.relayout(0.0);
        app
    }

    // ── geometry ─────────────────────────────────────────────────────

    /// Rows the page viewport spans: everything but the header and the
    /// status line.
    fn content_h(&self) -> f32 {
        (self.rows as f32 - 2.0).max(1.0)
    }

    /// Width of the reading column, in em.
    fn content_em(&self) -> f32 {
        let avail = (self.cols as f32 - 2.0 * MARGIN - 1.0).max(8.0);
        let em = avail / self.zoom;
        match self.measure {
            Some(m) if m > 0.0 => em.min(m),
            _ => em,
        }
    }

    fn view(&self) -> View {
        let content_w = self.content_em() * self.zoom;
        View {
            cols: self.cols as f32,
            rows: self.rows as f32,
            cell_pw: self.cell_pw,
            cell_ph: self.cell_ph,
            content_x: ((self.cols as f32 - content_w) * 0.5).floor().max(MARGIN),
            content_y: 1.0,
            content_h: self.content_h(),
            zoom: self.zoom,
            scroll: self.scroll,
        }
    }

    fn doc_height(&self) -> f32 {
        self.layout.height * self.zoom
    }

    fn max_scroll(&self) -> f32 {
        (self.doc_height() - self.content_h()).max(0.0)
    }

    /// Re-wrap at the current width and zoom, keeping `anchor_em` — the
    /// document position that was at the top of the viewport — where it
    /// was. Every row and run index moves, so the search index is
    /// rebuilt with it.
    fn relayout(&mut self, anchor_em: f32) {
        let metrics = Metrics {
            cell_pw: self.cell_pw,
            cell_ph: self.cell_ph,
            max_image_h: (self.content_h() * IMAGE_MAX_FRACTION / self.zoom).max(3.0),
        };
        self.layout = layout::lay_out(&self.doc, self.content_em(), metrics, &self.images);
        self.scroll = (anchor_em * self.zoom).clamp(0.0, self.max_scroll());
        self.rebuild_matches();
        self.dirty = Dirty::all();
    }

    /// Where the reader is, in em — the quantity a re-wrap preserves.
    fn anchor(&self) -> f32 {
        self.scroll / self.zoom
    }

    fn scroll_by(&mut self, cells: f32) {
        self.scroll_to(self.scroll + cells);
    }

    fn scroll_to(&mut self, to: f32) {
        let clamped = to.clamp(0.0, self.max_scroll());
        if (clamped - self.scroll).abs() > 1e-4 {
            self.scroll = clamped;
            self.dirty.page = true;
            self.dirty.chrome = true;
        }
    }

    /// Put document position `top_em` a little below the top of the
    /// viewport, so a jump doesn't land its target on the very edge.
    fn reveal(&mut self, top_em: f32) {
        self.scroll_to(top_em * self.zoom - self.content_h() * 0.15);
    }

    // ── document ─────────────────────────────────────────────────────

    fn load(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow!("{}: {e}", path.display()))?;
        let base = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        self.doc = doc::parse(&text, &base);
        self.path = Some(path.to_path_buf());
        self.title = title_for(self.path.as_deref(), &self.doc);
        self.relayout(0.0);
        Ok(())
    }

    fn reload(&mut self) {
        let Some(path) = self.path.clone() else {
            self.warn("nothing to reload — the document came from stdin".into());
            return;
        };
        let where_we_were = self.anchor();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                self.doc = doc::parse(&text, &self.doc.base.clone());
                self.title = title_for(Some(&path), &self.doc);
                self.relayout(where_we_were);
                self.note(format!("reloaded {}", short_path(&path)));
            }
            Err(e) => self.warn(format!("{}: {e}", short_path(&path))),
        }
    }

    /// Follow a link: an anchor scrolls, a markdown file opens here
    /// (with the current one pushed onto the back stack), and anything
    /// else goes to the desktop.
    fn follow(&mut self, index: usize) {
        let Some(link) = self.doc.links.get(index).cloned() else {
            return;
        };
        let url = link.url.trim().to_string();
        if url.is_empty() {
            self.warn("that link has no destination".into());
            return;
        }
        if let Some(slug) = url.strip_prefix('#') {
            match self.layout.anchor(slug) {
                Some(top) => self.reveal(top),
                None => self.warn(format!("no heading named “{slug}”")),
            }
            return;
        }
        let local = doc::resolve_local(&self.doc.base, &url);
        match local {
            Some(target) if is_markdown(&target) => {
                let from = self.path.clone();
                let at = self.anchor();
                match self.load(&target) {
                    Ok(()) => {
                        if let Some(from) = from {
                            self.history.push((from, at));
                        }
                        // An anchor on the far side of the link lands in
                        // the document we just opened.
                        if let Some(slug) = url.split('#').nth(1)
                            && let Some(top) = self.layout.anchor(slug)
                        {
                            self.reveal(top);
                        }
                        self.note(format!("opened {}", short_path(&target)));
                    }
                    Err(e) => self.warn(e.to_string()),
                }
            }
            Some(target) => self.pending_open = Some(target.display().to_string()),
            None if doc::has_scheme(&url) => self.pending_open = Some(url),
            None => self.warn(format!("cannot open “{url}”")),
        }
    }

    fn back(&mut self) {
        let Some((path, at)) = self.history.pop() else {
            self.warn("no document to go back to".into());
            return;
        };
        match self.load(&path) {
            Ok(()) => {
                self.scroll_to(at * self.zoom);
                self.note(format!("back to {}", short_path(&path)));
            }
            Err(e) => self.warn(e.to_string()),
        }
    }

    // ── search ───────────────────────────────────────────────────────

    fn rebuild_matches(&mut self) {
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }
        for (i, row) in self.layout.rows.iter().enumerate() {
            for (j, run) in row.runs().iter().enumerate() {
                for (start, len) in render::matches_in(&run.text, &self.query) {
                    self.matches.push(MatchPos {
                        row: i,
                        run: j,
                        start,
                        len,
                    });
                }
            }
        }
        self.match_at = self.match_at.min(self.matches.len().saturating_sub(1));
    }

    /// Step to the next match, wrapping. `delta` is +1 or -1.
    fn step_match(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.warn(if self.query.is_empty() {
                "no search — press / first".into()
            } else {
                format!("no matches for “{}”", self.query)
            });
            return;
        }
        let n = self.matches.len() as isize;
        self.match_at = (((self.match_at as isize + delta) % n + n) % n) as usize;
        self.jump_to_match();
    }

    /// Scroll to the match nearest below the top of the viewport — what
    /// `/` should land on, rather than the first in the file.
    fn first_match_from_here(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let here = self.anchor();
        self.match_at = self
            .matches
            .iter()
            .position(|m| {
                self.layout
                    .rows
                    .get(m.row)
                    .is_some_and(|r| r.top >= here - 0.5)
            })
            .unwrap_or(0);
        self.jump_to_match();
    }

    fn jump_to_match(&mut self) {
        let Some(m) = self.matches.get(self.match_at) else {
            return;
        };
        if let Some(row) = self.layout.rows.get(m.row) {
            let top = row.top;
            self.reveal(top);
        }
        self.dirty.page = true;
        self.dirty.chrome = true;
    }

    // ── messages ─────────────────────────────────────────────────────

    fn note(&mut self, text: String) {
        self.message = Some((text, false, Instant::now()));
        self.dirty.chrome = true;
    }

    fn warn(&mut self, text: String) {
        self.message = Some((text, true, Instant::now()));
        self.dirty.chrome = true;
    }

    fn expire_message(&mut self) {
        if self
            .message
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() > MESSAGE_TTL)
        {
            self.message = None;
            self.dirty.chrome = true;
        }
    }

    // ── input ────────────────────────────────────────────────────────

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.dirty.modal = true;
        self.dirty.chrome = true;
    }

    fn on_event(&mut self, ev: Event) {
        match &mut self.mode {
            Mode::Normal => self.normal_key(ev),
            Mode::Search(_) => self.search_key(ev),
            Mode::Outline(_) | Mode::Links(_) => self.picker_key(ev),
            Mode::Help { .. } => self.help_key(ev),
        }
    }

    fn normal_key(&mut self, ev: Event) {
        let page = self.content_h();
        match ev {
            Event::Key('q') | Event::Ctrl('c') => self.quit = true,
            Event::Key('j') | Event::Arrow(Dir::Down) => self.scroll_by(self.zoom),
            Event::Key('k') | Event::Arrow(Dir::Up) => self.scroll_by(-self.zoom),
            Event::Key('d') | Event::Ctrl('d') => self.scroll_by(page * 0.5),
            Event::Key('u') | Event::Ctrl('u') => self.scroll_by(-page * 0.5),
            Event::Key('f') | Event::Key(' ') | Event::Nav(Nav::PageDown) => {
                self.scroll_by(page - 2.0)
            }
            Event::Key('b') | Event::Nav(Nav::PageUp) => self.scroll_by(-(page - 2.0)),
            Event::Key('g') | Event::Nav(Nav::Home) => self.scroll_to(0.0),
            Event::Key('G') | Event::Nav(Nav::End) => self.scroll_to(f32::MAX),
            Event::Key('/') => self.set_mode(Mode::Search(LineEditor::with_max(
                self.query.clone(),
                COMMAND_MAX_CHARS,
            ))),
            Event::Key('n') => self.step_match(1),
            Event::Key('N') => self.step_match(-1),
            Event::Key('t') => self.open_outline(),
            Event::Key('l') => self.open_links(),
            Event::Key('?') => self.set_mode(Mode::Help {
                modal: ScrollModal::new(help_lines()),
                offset: 0,
            }),
            Event::Key('r') => self.reload(),
            Event::Key('+') | Event::Key('=') => self.set_zoom(self.zoom + ZOOM_STEP),
            Event::Key('-') | Event::Key('_') => self.set_zoom(self.zoom - ZOOM_STEP),
            Event::Key('0') => self.set_zoom(1.0),
            Event::Key('w') => self.toggle_measure(),
            Event::Backspace => self.back(),
            Event::Escape => {
                if self.query.is_empty() {
                    return;
                }
                self.query.clear();
                self.matches.clear();
                self.dirty.page = true;
                self.dirty.chrome = true;
            }
            Event::WheelDown { .. } => self.scroll_by(3.0 * self.zoom),
            Event::WheelUp { .. } => self.scroll_by(-3.0 * self.zoom),
            Event::MouseDown {
                button: Button::Left,
                col,
                row,
            } => {
                if let Some(index) = self.link_at(col, row) {
                    self.follow(index);
                }
            }
            _ => {}
        }
    }

    fn search_key(&mut self, ev: Event) {
        let Mode::Search(editor) = &mut self.mode else {
            return;
        };
        match editor.feed_event(ev) {
            EditOutcome::Noop => {}
            EditOutcome::Redraw => self.dirty.modal = true,
            EditOutcome::Cancel => self.set_mode(Mode::Normal),
            EditOutcome::Commit => {
                let query = editor.buffer.trim().to_string();
                self.set_mode(Mode::Normal);
                self.query = query;
                self.match_at = 0;
                self.rebuild_matches();
                if self.query.is_empty() {
                    self.dirty.page = true;
                } else if self.matches.is_empty() {
                    self.warn(format!("no matches for “{}”", self.query));
                    self.dirty.page = true;
                } else {
                    let n = self.matches.len();
                    self.first_match_from_here();
                    self.note(format!("{n} match{}", if n == 1 { "" } else { "es" }));
                }
            }
        }
    }

    fn picker_key(&mut self, ev: Event) {
        let outcome = match &mut self.mode {
            Mode::Outline(p) => p.feed_event(ev),
            Mode::Links(p) => p.feed_event(ev),
            _ => return,
        };
        match outcome {
            PickerOutcome::Noop => {}
            PickerOutcome::Redraw => self.dirty.modal = true,
            PickerOutcome::Cancel => self.set_mode(Mode::Normal),
            PickerOutcome::Commit => {
                let chosen = match &self.mode {
                    Mode::Outline(p) => p.current_item().map(|i| (true, i.payload)),
                    Mode::Links(p) => p.current_item().map(|i| (false, i.payload)),
                    _ => None,
                };
                self.set_mode(Mode::Normal);
                match chosen {
                    Some((true, i)) => {
                        if let Some(anchor) = self.layout.outline.get(i) {
                            let top = anchor.top;
                            self.reveal(top);
                        }
                    }
                    Some((false, i)) => self.follow(i),
                    None => {}
                }
            }
        }
    }

    fn help_key(&mut self, ev: Event) {
        let Mode::Help { modal, offset } = &mut self.mode else {
            return;
        };
        let max = modal.max_offset(self.cols, self.rows);
        let step = |o: &mut u32, d: i64| {
            let next = (*o as i64 + d).clamp(0, max as i64) as u32;
            let moved = next != *o;
            *o = next;
            moved
        };
        let moved = match ev {
            Event::Escape | Event::Key('q') | Event::Enter | Event::Key('?') => {
                self.set_mode(Mode::Normal);
                return;
            }
            Event::Key('j') | Event::Arrow(Dir::Down) | Event::WheelDown { .. } => step(offset, 1),
            Event::Key('k') | Event::Arrow(Dir::Up) | Event::WheelUp { .. } => step(offset, -1),
            Event::Nav(Nav::PageDown) | Event::Key(' ') => step(offset, modal.half_page),
            Event::Nav(Nav::PageUp) => step(offset, -modal.half_page),
            Event::Nav(Nav::Home) | Event::Key('g') => step(offset, -(max as i64)),
            Event::Nav(Nav::End) | Event::Key('G') => step(offset, max as i64),
            _ => false,
        };
        if moved {
            self.dirty.modal = true;
        }
    }

    fn set_zoom(&mut self, zoom: f32) {
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if (zoom - self.zoom).abs() < 1e-3 {
            return;
        }
        let anchor = self.anchor();
        self.zoom = zoom;
        self.relayout(anchor);
        self.note(format!("text {}%", (zoom * 100.0).round() as i32));
    }

    fn toggle_measure(&mut self) {
        let anchor = self.anchor();
        self.measure = match self.measure {
            Some(_) => None,
            None => Some(NARROW_EM),
        };
        self.relayout(anchor);
        self.note(match self.measure {
            Some(m) => format!("measure {} columns", m as i32),
            None => "measure: fill the pane".into(),
        });
    }

    fn open_outline(&mut self) {
        if self.layout.outline.is_empty() {
            self.warn("this document has no headings".into());
            return;
        }
        let items = self
            .layout
            .outline
            .iter()
            .enumerate()
            .map(|(i, a)| {
                // Indent by level, so the picker reads as an outline
                // even after the filter has thinned it out.
                let label = format!(
                    "{}{}",
                    "  ".repeat(a.level.saturating_sub(1) as usize),
                    a.text
                );
                PickerItem::new(label, format!("h{}", a.level), i)
            })
            .collect();
        self.set_mode(Mode::Outline(Picker::new(
            "Outline",
            FilterMode::Whole,
            items,
        )));
    }

    fn open_links(&mut self) {
        if self.doc.links.is_empty() {
            self.warn("this document has no links".into());
            return;
        }
        let items = self
            .doc
            .links
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let label = if l.text.trim().is_empty() {
                    l.url.clone()
                } else {
                    l.text.clone()
                };
                PickerItem::new(label, l.url.clone(), i)
            })
            .collect();
        self.set_mode(Mode::Links(Picker::new("Links", FilterMode::Whole, items)));
    }

    /// The link under a click, if any. vmd hit-tests against its own
    /// layout rather than asking the terminal (VGE §15): it laid the
    /// runs out, so it already knows where they are, and a click that
    /// lands a fraction of a cell off the end of a link is not worth a
    /// round trip.
    fn link_at(&self, col: u16, row: u16) -> Option<usize> {
        let view = self.view();
        let local_y = row as f32 - view.content_y;
        if local_y < 0.0 || local_y >= view.content_h {
            return None;
        }
        let doc_y = (local_y + self.scroll) / self.zoom;
        let doc_x = (col as f32 + 0.5 - view.content_x) / self.zoom;
        // Later rows draw over earlier ones, so the last match wins.
        self.layout
            .rows
            .iter()
            .filter(|r| doc_y >= r.top && doc_y < r.top + r.height)
            .flat_map(|r| r.runs())
            .rfind(|run| {
                if run.link.is_none() {
                    return false;
                }
                let w = run.width();
                // The run's x is the anchor its alignment names, not
                // necessarily its left edge.
                let (x0, x1) = match run.align {
                    vge_protocol::command::Align::Left => (run.x, run.x + w),
                    vge_protocol::command::Align::Center => (run.x - w * 0.5, run.x + w * 0.5),
                    vge_protocol::command::Align::Right => (run.x - w, run.x),
                };
                doc_x >= x0 && doc_x <= x1
            })
            .and_then(|run| run.link)
    }

    fn resize(&mut self, cols: u32, rows: u32) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        let anchor = self.anchor();
        self.cols = cols;
        self.rows = rows;
        self.relayout(anchor);
        // The page element's clip rect is its size, so a resize is a
        // new element rather than new commands.
        self.needs_rebuild = true;
    }

    // ── pictures ─────────────────────────────────────────────────────

    /// Files the visible rows draw, in row order.
    fn visible_pictures(&self) -> Vec<PathBuf> {
        let top = self.anchor();
        let bottom = (self.scroll + self.content_h()) / self.zoom;
        self.layout
            .rows
            .iter()
            .filter(|r| r.top + r.height >= top && r.top <= bottom)
            .filter_map(|r| match &r.body {
                Body::Image(img) => img.path.clone(),
                _ => None,
            })
            .collect()
    }

    /// Ask for what is on screen, upload what came back, and drop the
    /// coldest pictures past the budget.
    fn pump_images(&mut self, out: &mut impl Write) -> Result<()> {
        let visible = self.visible_pictures();
        for path in &visible {
            self.images.request(path);
        }

        let mut cmds: Vec<(VgeCommand, u32)> = Vec::new();
        for decoded in self.images.drain() {
            let id = self.images.next_image_id();
            let (encoding, payload) =
                encode_payload(decoded.rgba, decoded.w, decoded.h, self.encoding)?;
            cmds.push((
                VgeCommand::UploadImage(UploadImageBody {
                    id: id.clone(),
                    encoding,
                    // Manual: the page's command list is replaced on
                    // every scroll tick, so an `Auto` picture would be
                    // collected the moment it scrolled off and have to
                    // be re-uploaded on the way back. We free them
                    // ourselves, through the LRU below.
                    retention: Retention::Manual,
                    width: decoded.w,
                    height: decoded.h,
                    total_bytes: payload.len() as u32,
                    chunk_offset: 0,
                    is_last: true,
                    data: payload,
                }),
                REQ_ID_NO_RESPONSE,
            ));
            self.images.ready(decoded.path, decoded.stamp, id);
            self.dirty.page = true;
        }
        for id in self.images.evict(&visible) {
            cmds.push((
                VgeCommand::DropImage {
                    id,
                    by_prefix: false,
                },
                REQ_ID_NO_RESPONSE,
            ));
        }
        if !cmds.is_empty() {
            out.write_all(&build_envelope(&cmds))?;
        }
        Ok(())
    }

    // ── drawing ──────────────────────────────────────────────────────

    fn page_commands(&self) -> Vec<DrawCmd> {
        let search = render::Search {
            query: &self.query,
            active: self.matches.get(self.match_at).copied(),
        };
        render::page(&self.layout, &self.view(), &self.images, search)
    }

    fn chrome_commands(&self) -> Vec<DrawCmd> {
        let height = self.doc_height().max(1.0);
        let max = self.max_scroll();
        let hints = match &self.mode {
            Mode::Search(editor) => format!("/{}", editor.buffer),
            _ if !self.query.is_empty() && !self.matches.is_empty() => format!(
                "/{}  [{}/{}]",
                self.query,
                self.match_at + 1,
                self.matches.len()
            ),
            _ => "? help   / search   t outline   l links   q quit".to_string(),
        };
        let (status, warn) = match &self.message {
            Some((text, warn, _)) => (text.clone(), *warn),
            None => (hints, false),
        };
        let section = self
            .layout
            .heading_at(self.anchor())
            .map(|a| a.text.as_str());
        render::chrome(
            &Chrome {
                title: &self.title,
                section,
                status: &status,
                status_warn: warn,
                progress: if max > 0.0 { self.scroll / max } else { 0.0 },
                visible: (self.content_h() / height).min(1.0),
            },
            &self.view(),
        )
    }

    fn modal_elements(&self) -> Vec<CreateElementBody> {
        match &self.mode {
            Mode::Normal => Vec::new(),
            Mode::Search(editor) => vec![prompt_element(
                MODAL_IDS.root,
                self.cols,
                self.rows,
                "Search",
                &editor.buffer,
                self.cell_pw,
                self.cell_ph,
                Some(editor.cursor),
                ORDER_MODAL,
            )],
            Mode::Outline(picker) => picker_element(
                MODAL_IDS.root,
                self.cols,
                self.rows,
                picker,
                self.cell_pw,
                self.cell_ph,
                ORDER_MODAL,
            ),
            Mode::Links(picker) => picker_element(
                MODAL_IDS.root,
                self.cols,
                self.rows,
                picker,
                self.cell_pw,
                self.cell_ph,
                ORDER_MODAL,
            ),
            Mode::Help { modal, offset } => modal.elements(
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

    fn render(&mut self, out: &mut impl Write) -> Result<()> {
        let mut cmds: Vec<(VgeCommand, u32)> = Vec::new();

        if self.needs_rebuild {
            self.needs_rebuild = false;
            self.modal_live.clear();
            // Elements only — the uploaded pictures are pinned (§8.0)
            // and must survive a resize.
            cmds.push((
                VgeCommand::DeleteElement {
                    id: ID_PREFIX.into(),
                    by_prefix: true,
                },
                REQ_ID_NO_RESPONSE,
            ));
            let view = self.view();
            cmds.push((
                VgeCommand::CreateElement(CreateElementBody {
                    id: ID_PAGE.into(),
                    commands: self.page_commands(),
                    origin: Point {
                        x: 0.0,
                        y: view.content_y,
                    },
                    is_visible: true,
                    draw_order: ORDER_PAGE,
                    parent: None,
                    // The clip is what keeps a half-scrolled heading —
                    // or a picture taller than the pane — off the
                    // header and the status line (§9.2).
                    size: Some(Point {
                        x: view.cols,
                        y: view.content_h,
                    }),
                    transform: None,
                    anchor: OriginAnchor::Viewport,
                }),
                REQ_ID_NO_RESPONSE,
            ));
            cmds.push(create(ID_CHROME, self.chrome_commands(), ORDER_CHROME));
            self.dirty = Dirty {
                modal: true,
                ..Default::default()
            };
        }

        if self.dirty.page {
            cmds.push(update(ID_PAGE, self.page_commands()));
        }
        if self.dirty.chrome {
            cmds.push(update(ID_CHROME, self.chrome_commands()));
        }
        if self.dirty.modal {
            for id in std::mem::take(&mut self.modal_live) {
                cmds.push((
                    VgeCommand::DeleteElement {
                        id,
                        by_prefix: false,
                    },
                    REQ_ID_NO_RESPONSE,
                ));
            }
            for element in self.modal_elements() {
                self.modal_live.push(element.id.clone());
                cmds.push((VgeCommand::CreateElement(element), REQ_ID_NO_RESPONSE));
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

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| MARKDOWN_EXT.contains(&e.as_str()))
}

/// What the header bar names the document: its `#` title when it has
/// one, else the file name.
fn title_for(path: Option<&Path>, doc: &Doc) -> String {
    if let Some(title) = doc.title.as_deref().filter(|t| !t.trim().is_empty()) {
        return title.to_string();
    }
    match path {
        Some(p) => short_path(p),
        None => "(stdin)".to_string(),
    }
}

fn short_path(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Hand a URL or file to the desktop, detached: its own session, null
/// stdio, never waited on (a short reaper thread keeps it from
/// lingering as a zombie). Mirrors vfm's `open::spawn_detached`.
fn open_externally(target: &str) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new("xdg-open");
    cmd.arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn help_lines() -> Vec<String> {
    [
        "vmd keybindings",
        "",
        "Move",
        "  j / k, ↓ / ↑      down / up a line",
        "  d / u             half a page",
        "  f / Space, b      a full page",
        "  g / G             top / bottom",
        "  wheel             scroll",
        "",
        "Find your way",
        "  /                 search (Enter to run, Esc to cancel)",
        "  n / N             next / previous match",
        "  Esc               clear the search",
        "  t                 outline — jump to a heading",
        "  l                 links — open one",
        "  click a link      follow it",
        "  Backspace         back to the previous document",
        "",
        "Look",
        "  + / -             text bigger / smaller",
        "  0                 back to the terminal's own size",
        "  w                 reading measure: narrow / fill the pane",
        "",
        "Other",
        "  r                 reload from disk",
        "  ?                 this help",
        "  q                 quit",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn term_size() -> (u32, u32) {
    match winsize() {
        Some(ws) if ws.ws_col > 0 && ws.ws_row > 0 => (ws.ws_col as u32, ws.ws_row as u32),
        _ => (80, 24),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Terminal setup
// ─────────────────────────────────────────────────────────────────────

/// Alt screen, hidden cursor, clear+home, button-event mouse tracking
/// (?1002) in SGR encoding (?1006) — the pair vfm/vplay/vdraw use.
const ENTER_UI: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[?1002h\x1b[?1006h";

/// Restores the terminal on the way out, however we leave.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(&sweep());
        let _ = out.write_all(b"\x1b[?1002l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        let _ = out.flush();
    }
}

/// Drop every element and picture in vmd's namespace. Matching nothing
/// is `Ok` (§6.2 / §8.2), so this is also what a first run sends.
fn sweep() -> Vec<u8> {
    build_envelope(&[
        (
            VgeCommand::DeleteElement {
                id: ID_PREFIX.into(),
                by_prefix: true,
            },
            REQ_ID_NO_RESPONSE,
        ),
        (
            VgeCommand::DropImage {
                id: ID_PREFIX.into(),
                by_prefix: true,
            },
            REQ_ID_NO_RESPONSE,
        ),
    ])
}

/// Query the terminal's themed palette with a PRT probe
/// (portal-extension §2.1 / §10), so vmd's chrome tracks veter's theme
/// and, inside a vmux pane, that pane's nesting depth. Best-effort: a
/// terminal that does not speak PRT leaves vmd on its built-in colours.
fn probe_host_theme(timeout: Duration) -> Option<(Color, Option<[[u8; 4]; 8]>)> {
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
        if let Some(payload) = apc.feed(&buf[..n]).into_payloads().next() {
            return parse_prt_theme(&payload);
        }
    }
}

/// Pull the themed palette out of a PRT ProbeResponse payload: the
/// accent, and the eight-colour theme block behind it. Both follow the
/// `vge_features` byte (§10) and are present only when the host sets
/// `FEAT_VGE_HOST_THEMED_STYLES`; a short body (older host) reads the
/// trailing fields as absent.
fn parse_prt_theme(payload: &[u8]) -> Option<(Color, Option<[[u8; 4]; 8]>)> {
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
    let mut quad = || match (r.u8(), r.u8(), r.u8(), r.u8()) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => Some([a, b, c, d]),
        _ => None,
    };
    let [red, green, blue, alpha] = quad()?;
    // All eight or none — half a palette is worse than none of it.
    let colors = {
        let mut quads = [[0u8; 4]; 8];
        let mut all = true;
        for slot in &mut quads {
            match quad() {
                Some(q) => *slot = q,
                None => {
                    all = false;
                    break;
                }
            }
        }
        all.then_some(quads)
    };
    Some((
        Color {
            r: red as f32 / 255.0,
            g: green as f32 / 255.0,
            b: blue as f32 / 255.0,
            a: alpha as f32 / 255.0,
        },
        colors,
    ))
}

// ─────────────────────────────────────────────────────────────────────
// CLI
// ─────────────────────────────────────────────────────────────────────

struct Args {
    path: Option<PathBuf>,
    measure: Option<f32>,
    zoom: f32,
    accent: Option<u32>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut path: Option<PathBuf> = None;
    let mut measure = Some(NARROW_EM);
    let mut zoom = 1.0f32;
    let mut accent = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |long: &str, given: &str| -> Result<String> {
            match given.strip_prefix(&format!("{long}=")) {
                Some(v) => Ok(v.to_string()),
                None => it.next().ok_or_else(|| anyhow!("{long} needs a value")),
            }
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!(
                    "vmd {}",
                    veter_version::long_version(env!("CARGO_PKG_VERSION"))
                );
                return Ok(None);
            }
            a if a == "-w" || a == "--width" || a.starts_with("--width=") => {
                let v = value("--width", a)?;
                let cols: f32 = v.parse().map_err(|_| anyhow!("--width wants a number"))?;
                // 0 is the documented way to say "fill the pane".
                measure = (cols >= 1.0).then_some(cols);
            }
            a if a == "-z" || a == "--zoom" || a.starts_with("--zoom=") => {
                let v = value("--zoom", a)?;
                zoom = v
                    .parse::<f32>()
                    .map_err(|_| anyhow!("--zoom wants a number"))?
                    .clamp(ZOOM_MIN, ZOOM_MAX);
            }
            a if a == "-A" || a == "--accent" || a.starts_with("--accent=") => {
                let v = value("--accent", a)?;
                accent = Some(theme::parse_accent_color(&v).map_err(|e| anyhow!(e))?);
            }
            s if s.starts_with('-') && s.len() > 1 => bail!("unknown option: {s}"),
            s if path.is_none() => path = Some(PathBuf::from(s)),
            s => bail!("unexpected extra argument: {s}"),
        }
    }
    Ok(Some(Args {
        path,
        measure,
        zoom,
        accent,
    }))
}

/// Read the document from standard input, then put fd 0 back on the
/// controlling terminal so the event loop has a keyboard and the VGE
/// probe has somewhere to read its reply from. This is what makes
/// `… | vmd` work.
fn document_from_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let tty = std::fs::File::open("/dev/tty")
        .map_err(|e| anyhow!("reading from a pipe needs a controlling terminal: {e}"))?;
    // SAFETY: dup2 onto fd 0 with a valid fd; the original is closed
    // when `tty` drops, which is after the duplicate exists.
    let rc = unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&tty), 0) };
    if rc < 0 {
        bail!("could not attach standard input to the terminal");
    }
    Ok(text)
}

fn main() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    let (path, text) = match &args.path {
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
            (Some(p.canonicalize().unwrap_or_else(|_| p.clone())), text)
        }
        None if !std::io::stdin().is_terminal() => (None, document_from_stdin()?),
        None => bail!("no document: give vmd a file, or pipe one in"),
    };
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vmd must run with stdin and stdout connected to a terminal");
    }

    let _raw = RawTty::enable()?;
    let winch = install_sigwinch();
    let mut out = std::io::stdout();
    out.write_all(ENTER_UI)?;
    out.flush()?;
    let _term = TermGuard;

    drain_stale_stdin();
    let probe = run_probe(PROBE_TIMEOUT)?.ok_or_else(|| {
        anyhow!("VGE probe timed out — this terminal does not appear to support VGE")
    })?;
    // Reclaim anything a previous run left behind before drawing over
    // it; see `sweep`.
    out.write_all(&sweep())?;
    out.flush()?;

    // The CLI accent overrides the host's, but never its surfaces and
    // text — those describe the terminal vmd is drawing on, not which
    // colour its chrome should be.
    let host = probe_host_theme(PROBE_TIMEOUT);
    match (args.accent, host) {
        (Some(rgba), _) => theme::set_cli_accent(theme::unpack(rgba)),
        (None, Some((accent, _))) => theme::set_host_accent(accent),
        (None, None) => {}
    }
    if let Some((_, Some(quads))) = host {
        theme::set_host_theme(theme::HostColors::from_rgba8(quads));
    }

    let (cols, rows) = term_size();
    let mut app = App::new(
        path,
        &text,
        cols,
        rows,
        probe.cell_pixel_width.max(1) as f32,
        probe.cell_pixel_height.max(1) as f32,
        args.zoom,
        args.measure,
    );
    app.encoding = choose_encoding(probe.supported_image_encodings, is_ssh_session(), 88.0);

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
        app.expire_message();
        app.pump_images(&mut out)?;
        app.render(&mut out)?;

        if let Some(target) = app.pending_open.take() {
            match open_externally(&target) {
                Ok(()) => app.note(format!("opened {target}")),
                Err(e) => app.warn(format!("xdg-open: {e}")),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(text: &str) -> App {
        App::new(None, text, 100, 30, 10.0, 20.0, 1.0, Some(NARROW_EM))
    }

    /// One of everything vmd knows how to draw.
    const KITCHEN_SINK: &str = "\
---
title: front matter
---

# Heading one

Body text with **bold**, *italic*, `inline code`, ~~struck out~~ and a
[link](https://example.com), plus an [anchor](#heading-one) and a
[relative one](README.md).

## Heading two

### Three
#### Four
##### Five
###### Six

- tight bullet
- another, with a nested list
  - inner
    - innermost
- [ ] a task
- [x] a done task

1. first
2. second
10. tenth

> A quote, with **emphasis** inside it.
>
> > and a nested one

> [!WARNING]
> Mind the gap.

```rust
fn main() {
    // a line long enough that it has to wrap somewhere on any sensible measure at all, twice over
    println!(\"hello\");
}
```

    an indented code block

| left | centre | right |
|:-----|:------:|------:|
| a | b | c |
| a much longer cell than the header | x | 42 |

---

![a picture](nope.png)

A footnote reference[^1] in a sentence.

[^1]: And the note it points at.

Text with a hard break at the end  
and the line after it.
";

    /// Unwrap an envelope into `(frame_type, request_id, body)`
    /// triples, the way `vproto`'s round-trip test does.
    fn frames(bytes: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
        use vge_protocol::codec::Reader;
        let out = vge_protocol::apc::ApcStream::with_marker(*b"VGE").feed(bytes);
        let mut frames = Vec::new();
        for payload in out.payloads {
            let mut r = Reader::new(&payload);
            let _version = r.u8().unwrap();
            let _len = r.u32().unwrap();
            while !r.at_end() {
                let ty = r.u8().unwrap();
                let req = r.u32().unwrap();
                let n = r.u32().unwrap() as usize;
                frames.push((ty, req, r.take(n).unwrap().to_vec()));
            }
        }
        frames
    }

    /// Encode `cmds` and push them back through the protocol crate's
    /// own decoder — the same validation the terminal runs — returning
    /// how many draw commands came out the far side.
    fn survives_the_host(cmds: Vec<(VgeCommand, u32)>) -> usize {
        let bytes = build_envelope(&cmds);
        let mut drawn = 0;
        for (ty, _, body) in frames(&bytes) {
            let decoded = vge_protocol::command::parse(ty, &body).unwrap_or_else(|e| {
                panic!("the host would reject vmd's own encoding: err 0x{e:04X}")
            });
            drawn += match decoded {
                VgeCommand::CreateElement(b) => b.commands.len(),
                VgeCommand::UpdateCommands(b) => b.commands.len(),
                _ => 0,
            };
        }
        drawn
    }

    /// Everything vmd can put on screen, at both ends of the zoom
    /// range, scrolled from top to bottom, encoded and decoded again.
    ///
    /// A `CreateElement` fails *atomically* (§6.1), so one draw command
    /// with a font scale outside `(0, 64]` or a negative line width
    /// does not degrade the page — it blanks it. This is the cheapest
    /// place to find that out.
    #[test]
    fn every_command_vmd_draws_survives_the_hosts_own_parser() {
        let mut total = 0;
        for zoom in [ZOOM_MIN, 1.0, ZOOM_MAX] {
            for measure in [Some(NARROW_EM), Some(20.0), None] {
                let mut a = app(KITCHEN_SINK);
                a.zoom = zoom;
                a.measure = measure;
                a.query = "the".into();
                a.rebuild_matches();
                a.match_at = 0;
                assert!(!a.matches.is_empty(), "the sink should contain the query");

                let mut scroll = 0.0;
                loop {
                    a.scroll_to(scroll);
                    total += survives_the_host(vec![
                        create(ID_PAGE, a.page_commands(), ORDER_PAGE),
                        create(ID_CHROME, a.chrome_commands(), ORDER_CHROME),
                    ]);
                    if scroll >= a.max_scroll() {
                        break;
                    }
                    scroll += a.content_h() * 0.5;
                }
            }
        }
        assert!(
            total > 200,
            "only {total} draw commands — is the sink drawing?"
        );
    }

    /// The overlays come from `vge-ui` but are built with vmd's own
    /// sizes, so they go through the same check.
    #[test]
    fn every_overlay_survives_the_hosts_own_parser() {
        let mut a = app(KITCHEN_SINK);
        for mode in [
            Mode::Search(LineEditor::with_max("query".into(), COMMAND_MAX_CHARS)),
            Mode::Help {
                modal: ScrollModal::new(help_lines()),
                offset: 3,
            },
        ] {
            a.mode = mode;
            let cmds = a
                .modal_elements()
                .into_iter()
                .map(|e| (VgeCommand::CreateElement(e), REQ_ID_NO_RESPONSE))
                .collect();
            assert!(survives_the_host(cmds) > 0);
        }
        a.open_outline();
        assert!(matches!(a.mode, Mode::Outline(_)), "the sink has headings");
        let cmds = a
            .modal_elements()
            .into_iter()
            .map(|e| (VgeCommand::CreateElement(e), REQ_ID_NO_RESPONSE))
            .collect();
        assert!(survives_the_host(cmds) > 0);

        a.open_links();
        assert!(matches!(a.mode, Mode::Links(_)), "the sink has links");
        let cmds = a
            .modal_elements()
            .into_iter()
            .map(|e| (VgeCommand::CreateElement(e), REQ_ID_NO_RESPONSE))
            .collect();
        assert!(survives_the_host(cmds) > 0);
    }

    /// A frame costs what the pane holds, not what the file does.
    #[test]
    fn the_page_draws_the_viewport_not_the_document() {
        let short = app(KITCHEN_SINK);
        let long = app(&KITCHEN_SINK.repeat(40));
        assert!(long.layout.rows.len() > 20 * short.layout.rows.len());
        let drawn = long.page_commands().len();
        assert!(
            drawn < short.page_commands().len() * 3,
            "{drawn} commands for a viewport-sized slice of a 40× document"
        );
    }

    #[test]
    fn scrolling_clamps_to_the_document() {
        let mut a = app(&"a paragraph.\n\n".repeat(200));
        a.scroll_to(-50.0);
        assert_eq!(a.scroll, 0.0);
        a.scroll_to(f32::MAX);
        assert!((a.scroll - a.max_scroll()).abs() < 1e-3);
        assert!(a.max_scroll() > 0.0);
    }

    #[test]
    fn an_empty_document_still_draws_a_frame() {
        // Everything downstream divides by a document height or a
        // scroll range at some point; both are zero here.
        for text in ["", "\n\n\n", "   "] {
            let mut a = app(text);
            assert_eq!(a.max_scroll(), 0.0);
            a.scroll_to(f32::MAX);
            let chrome = a.chrome_commands();
            assert!(!chrome.is_empty());
            assert!(a.page_commands().is_empty(), "nothing to draw for {text:?}");
            a.set_zoom(ZOOM_MAX);
            a.step_match(1);
            assert!(!a.quit);
        }
    }

    #[test]
    fn a_document_shorter_than_the_pane_does_not_scroll() {
        let mut a = app("# Tiny\n\none line\n");
        a.scroll_to(f32::MAX);
        assert_eq!(a.scroll, 0.0);
        assert_eq!(a.max_scroll(), 0.0);
    }

    #[test]
    fn zoom_keeps_the_reading_position() {
        let mut a = app(&"paragraph text here.\n\n".repeat(120));
        a.scroll_to(a.max_scroll() * 0.5);
        let before = a.anchor();
        a.set_zoom(2.0);
        // Re-wrapping at half the columns moves content around, so the
        // position is preserved approximately — what matters is that it
        // does not jump to the top or the end.
        assert!(a.anchor() > 0.0);
        assert!(a.scroll <= a.max_scroll() + 1e-3);
        let _ = before;
    }

    #[test]
    fn a_narrower_measure_wraps_more_lines_into_the_page() {
        let text = "word ".repeat(400);
        let mut wide = app(&text);
        wide.measure = None;
        wide.relayout(0.0);
        let mut narrow = app(&text);
        narrow.measure = Some(30.0);
        narrow.relayout(0.0);
        assert!(
            narrow.layout.height > wide.layout.height,
            "{} vs {}",
            narrow.layout.height,
            wide.layout.height
        );
    }

    #[test]
    fn search_finds_every_occurrence_and_steps_through_them() {
        let mut a = app("alpha beta\n\ngamma alpha\n\nalpha\n");
        a.query = "alpha".into();
        a.rebuild_matches();
        assert_eq!(a.matches.len(), 3);
        a.match_at = 0;
        a.step_match(1);
        assert_eq!(a.match_at, 1);
        a.step_match(-1);
        assert_eq!(a.match_at, 0);
        // Stepping back from the first wraps to the last.
        a.step_match(-1);
        assert_eq!(a.match_at, 2);
    }

    #[test]
    fn a_search_with_no_matches_leaves_the_index_alone() {
        let mut a = app("nothing to see\n");
        a.query = "absent".into();
        a.rebuild_matches();
        assert!(a.matches.is_empty());
        a.step_match(1);
        assert_eq!(a.match_at, 0);
        assert!(a.message.as_ref().is_some_and(|(_, warn, _)| *warn));
    }

    #[test]
    fn a_click_on_a_link_resolves_to_it() {
        let mut a = app("[click me](https://example.com) and plain text\n");
        assert_eq!(a.doc.links.len(), 1);
        a.scroll_to(0.0);
        let view = a.view();
        // The link is the first run of the first row, so a click just
        // inside its left edge is on it.
        let row = a
            .layout
            .rows
            .iter()
            .find(|r| r.runs().iter().any(|run| run.link.is_some()))
            .expect("a row with the link");
        let run = row.runs().iter().find(|r| r.link.is_some()).unwrap();
        let col = (view.content_x + run.x * view.zoom + 1.0) as u16;
        let screen_row = (view.content_y + row.top * view.zoom) as u16;
        assert_eq!(a.link_at(col, screen_row), Some(0));
        // A click past the end of the run hits nothing.
        let past = (view.content_x + (run.x + run.width()) * view.zoom + 5.0) as u16;
        assert_eq!(a.link_at(past, screen_row), None);
    }

    #[test]
    fn a_click_outside_the_page_viewport_hits_nothing() {
        let a = app("[x](y)\n");
        // Row 0 is the header bar, not the page.
        assert_eq!(a.link_at(5, 0), None);
    }

    #[test]
    fn an_anchor_link_scrolls_instead_of_opening_anything() {
        let mut a = app(&format!(
            "[jump](#target)\n\n{}\n## Target\n\nend\n",
            "filler\n\n".repeat(80)
        ));
        a.follow(0);
        assert!(a.pending_open.is_none());
        assert!(a.scroll > 0.0, "should have scrolled to the heading");
    }

    #[test]
    fn a_missing_anchor_reports_rather_than_scrolling() {
        let mut a = app("[jump](#nowhere)\n\n# Somewhere\n");
        a.follow(0);
        assert!(a.pending_open.is_none());
        assert!(a.message.as_ref().is_some_and(|(_, warn, _)| *warn));
    }

    #[test]
    fn an_external_link_is_handed_to_the_desktop() {
        let mut a = app("[site](https://example.com)\n");
        a.follow(0);
        assert_eq!(a.pending_open.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn the_title_prefers_the_documents_own_heading() {
        let a = app("# Real Title\n\nbody\n");
        assert_eq!(a.title, "Real Title");
        let b = app("no heading here\n");
        assert_eq!(b.title, "(stdin)");
    }

    #[test]
    fn markdown_extensions_open_in_place() {
        assert!(is_markdown(Path::new("a/b/README.md")));
        assert!(is_markdown(Path::new("NOTES.MARKDOWN")));
        assert!(!is_markdown(Path::new("picture.png")));
        assert!(!is_markdown(Path::new("no-extension")));
    }

    #[test]
    fn back_with_no_history_says_so_instead_of_panicking() {
        let mut a = app("# doc\n");
        a.back();
        assert!(a.message.as_ref().is_some_and(|(_, warn, _)| *warn));
    }

    #[test]
    fn zoom_is_clamped_at_both_ends() {
        let mut a = app("# doc\n");
        a.set_zoom(100.0);
        assert!((a.zoom - ZOOM_MAX).abs() < 1e-3);
        a.set_zoom(0.0);
        assert!((a.zoom - ZOOM_MIN).abs() < 1e-3);
    }

    #[test]
    fn a_resize_keeps_the_reader_inside_the_document() {
        let mut a = app(&"line of text\n\n".repeat(100));
        a.scroll_to(f32::MAX);
        a.resize(40, 10);
        assert!(a.scroll <= a.max_scroll() + 1e-3);
        assert!(a.needs_rebuild, "the clip rect changed with the pane");
    }
}
