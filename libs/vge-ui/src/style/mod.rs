//! Style engines: what the clients' chrome looks like, decided in one
//! place.
//!
//! A client describes *what* it is drawing — a tab bar, a pane's title,
//! a prompt — and the [`Engine`] in force decides how it looks and how
//! much room it takes ([`Metrics`]). The client keeps its layout and its
//! hit-testing; the engine only paints into the boxes it is handed and
//! publishes the numbers those boxes are built from. Two engines ship:
//! [`flat`], the plain look that takes every colour from the host
//! theme, and [`aero`], Frutiger Aero — gel, frosted glass, sky and
//! bubbles.
//!
//! The host picks the colours and the engine: it publishes both in the
//! PRT probe (`doc/portal-extension.md` §10), so every client in a
//! terminal — at any nesting depth, across `vsd` reattaches — draws
//! with the same one, and a client's `--style` overrides it. The
//! engine turns the host's colours into its materials (flat fills, or
//! Aero gels) and picks the text drawn over them; what it never takes
//! from the accent are the activity and warning colours and the icons,
//! which are its own.
//!
//! Parts are named for their meaning, not for how one engine happens to
//! render them, so a third engine is free to do anything with them.
//! Where two clients drew the same kind of thing differently, the part
//! says which use it serves rather than the engines merging them
//! silently.

pub mod aero;
pub mod flat;

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, CreateElementBody, DrawCmd};

use crate::modal::{ModalIds, ScrollModal};
use crate::paint::Px;
use crate::picker::PickerView;

/// One style engine.
pub trait Engine: Sync {
    /// The name the host and `--style` select it by.
    fn name(&self) -> &'static str;

    /// The geometry this engine needs — the clients build their layout
    /// from it.
    fn metrics(&self) -> &Metrics;

    // ── Multiplexer ────────────────────────────────────────────────

    /// The tab bar, across the top [`TabbarMetrics::rows`] rows, in
    /// the pieces a client updates one at a time.
    fn tab_bar(&self, px: Px, bar: &TabBar<'_>) -> TabBarParts;

    /// A pane's own chrome — its title and scroll thumb — in
    /// pane-relative cells.
    fn pane_chrome(&self, cmds: &mut Vec<DrawCmd>, px: Px, pane: &PaneChrome<'_>);

    /// The dividers between panes, one line each.
    fn separators(&self, cmds: &mut Vec<DrawCmd>, lines: Vec<(Point, Point)>);

    // ── Document viewer ───────────────────────────────────────────

    /// What sits under a document's page — wallpaper, a reading sheet —
    /// in screen cells. Its own element, redrawn only on a re-wrap;
    /// nothing at all when the engine leaves the terminal's background
    /// as the paper.
    fn doc_ground(&self, cmds: &mut Vec<DrawCmd>, px: Px, frame: &DocFrame);

    /// The header and status bars around the page, and its scrollbar.
    fn doc_chrome(&self, cmds: &mut Vec<DrawCmd>, px: Px, frame: &DocFrame, c: &DocChrome<'_>);

    /// The colour of a run of document text.
    fn doc_ink(&self, role: DocRole) -> Color;

    /// One row's band of a code block. A block is a column of rows;
    /// `first` and `last` mark its ends.
    #[allow(clippy::too_many_arguments)]
    fn code_plate(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x0: f32,
        x1: f32,
        y: f32,
        h: f32,
        first: bool,
        last: bool,
        zoom: f32,
    );

    /// A table row's band: the header, or every other body row.
    fn table_band(&self, cmds: &mut Vec<DrawCmd>, r: Rect, head: bool);

    /// A block quote's bar; `warning` for an alert that can go wrong.
    fn quote_bar(&self, cmds: &mut Vec<DrawCmd>, r: Rect, warning: bool);

    /// A thematic break across `r`, `weight` cells thick.
    fn rule(&self, cmds: &mut Vec<DrawCmd>, r: Rect, weight: f32);

    /// A picture in `r`, or the plate holding its place.
    fn figure(&self, cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, content: Figure<'_>, zoom: f32);

    /// The chip behind an inline code span, `x0..x1` on a row at `y`,
    /// `h` tall.
    #[allow(clippy::too_many_arguments)]
    fn code_chip(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x0: f32,
        x1: f32,
        y: f32,
        h: f32,
        zoom: f32,
    );

    /// Whatever marks a heading beyond its ink and size — a heading of
    /// `level` whose text spans `x..x + w` on a row at `y`, `h` tall.
    #[allow(clippy::too_many_arguments)]
    fn heading_mark(
        &self,
        cmds: &mut Vec<DrawCmd>,
        level: u8,
        x: f32,
        w: f32,
        y: f32,
        h: f32,
        zoom: f32,
    );

    /// A search match spanning `x..x + w` on a row at `y`, `h` tall;
    /// `current` for the one the reader is on.
    #[allow(clippy::too_many_arguments)]
    fn search_hit(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        w: f32,
        y: f32,
        h: f32,
        zoom: f32,
        current: bool,
    );

    // ── File browser ──────────────────────────────────────────────

    /// The window's ground, `w × h` cells, under every pane.
    fn browser_backdrop(&self, cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32);

    /// An ancestor column's panel.
    fn column_pane(&self, cmds: &mut Vec<DrawCmd>, px: Px, area: Rect);

    /// One row of an ancestor column at `y` — the entry leading to the
    /// directory below when `selected`. `label` is already fitted.
    #[allow(clippy::too_many_arguments)]
    fn column_row(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        y: f32,
        label: &str,
        is_dir: bool,
        selected: bool,
    );

    /// A line standing in for a listing: "empty", or an error when
    /// `warn`.
    #[allow(clippy::too_many_arguments)]
    fn browser_note(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        align: Align,
        text: &str,
        warn: bool,
    );

    /// The current directory's panel, under its tiles or rows.
    fn grid_pane(&self, cmds: &mut Vec<DrawCmd>, px: Px, area: Rect);

    /// The current directory's scrollbar: a `track_h` track down the
    /// right of `area`, a `thumb_h` thumb `frac` of the way along it.
    #[allow(clippy::too_many_arguments)]
    fn browser_scrollbar(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        track_h: f32,
        thumb_h: f32,
        frac: f32,
    );

    /// A tile's plate, under its picture; `img_h` is the picture
    /// area's height from the tile's top.
    fn tile_plate(&self, cmds: &mut Vec<DrawCmd>, px: Px, tile: Rect, img_h: f32, selected: bool);

    /// A marked tile's badge, over its picture.
    fn tile_mark(&self, cmds: &mut Vec<DrawCmd>, px: Px, tile: Rect);

    /// A tile's label, centred on `x`.
    #[allow(clippy::too_many_arguments)]
    fn tile_label(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        label: &str,
        is_dir: bool,
        selected: bool,
    );

    /// A detail-list row's plate; `odd` for every other row.
    fn list_row_plate(&self, cmds: &mut Vec<DrawCmd>, px: Px, row: Rect, odd: bool, selected: bool);

    /// A marked row's badge.
    fn list_row_mark(&self, cmds: &mut Vec<DrawCmd>, px: Px, row: Rect);

    /// One of a detail-list row's texts.
    #[allow(clippy::too_many_arguments)]
    fn list_text(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        align: Align,
        text: &str,
        what: ListInk,
        selected: bool,
    );

    /// A picture's thumbnail, fitted into `r` by the client.
    fn thumbnail(&self, cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, image_id: String);

    /// The icon for a file of `kind`, in `b`. `is_link` adds the
    /// shortcut mark; `broken`, for a dangling link, crosses it out.
    fn file_icon(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        kind: FileKind,
        b: IconBox,
        is_link: bool,
        broken: bool,
    );

    /// The status line: `left` (already fitted) and `right`.
    fn status_bar(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        left: &str,
        left_kind: StatusLeft,
        right: &str,
    );

    // ── Terminal ──────────────────────────────────────────────────

    /// A modal question over the whole terminal — the host's own close
    /// confirm — in panel-relative cells: the scrim over
    /// [`Dialog::screen`], the panel, the title and the buttons.
    fn dialog(&self, cmds: &mut Vec<DrawCmd>, px: Px, dialog: &Dialog<'_>);

    // ── Modals ────────────────────────────────────────────────────

    /// A one-line prompt or confirm box, centred on the host. `caret`
    /// is the insertion point as a char index into `line`; `None` is a
    /// confirm with no caret.
    #[allow(clippy::too_many_arguments)]
    fn prompt(
        &self,
        id: &str,
        host_w: u32,
        host_h: u32,
        title: &str,
        line: &str,
        px: Px,
        caret: Option<usize>,
        draw_order: i32,
    ) -> CreateElementBody;

    /// A filterable picker: title, input line, the matching rows.
    fn picker(
        &self,
        id: &str,
        host_w: u32,
        host_h: u32,
        picker: &dyn PickerView,
        px: Px,
        draw_order: i32,
    ) -> Vec<CreateElementBody>;

    /// A scrolling overlay. Has to keep [`ScrollModal::elements`]'s ids
    /// and origins, since [`ScrollModal::scroll_commands`] scrolls it.
    #[allow(clippy::too_many_arguments)]
    fn scroll_modal(
        &self,
        modal: &ScrollModal,
        ids: ModalIds<'_>,
        host_w: u32,
        host_h: u32,
        offset: u32,
        px: Px,
        draw_order: i32,
    ) -> Vec<CreateElementBody>;
}

/// Geometry an engine dictates.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub tabbar: TabbarMetrics,
    /// Rows a document's page keeps clear inside its header and status
    /// bars, at either end — where the page clip stops.
    pub page_inset: f32,
}

/// The tab bar's size and spacing: how many rows it takes off the top
/// of the host, and the cells it puts around and between tabs. Pane
/// bounds, mouse routing, the layout and its hit-test all read these,
/// so they agree with what is drawn.
#[derive(Clone, Copy, Debug)]
pub struct TabbarMetrics {
    /// Rows reserved for the bar; panes start below them.
    pub rows: u32,
    /// Cells left clear before the first tab and after the last.
    pub margin: f32,
    /// Cells between neighbouring tabs.
    pub gap: f32,
    /// Cells around a label inside its name rect, both sides together.
    pub name_pad: f32,
}

/// The tab bar, laid out by the client.
pub struct TabBar<'a> {
    /// The bar's width — the host's.
    pub width: f32,
    /// The session-name segment pinned at the left: its label and
    /// width.
    pub session: Option<(&'a str, f32)>,
    /// The visible tabs, left to right.
    pub tabs: Vec<Tab<'a>>,
    /// Where the `‹` goes, when tabs are clipped off the start.
    pub left_chevron: Option<f32>,
    /// Where the `›` goes, when tabs are clipped off the end.
    pub right_chevron: Option<f32>,
}

/// The tab bar as [`Engine::tab_bar`] paints it, split where a change
/// usually stops: switching tabs repaints two of them and leaves the
/// ground alone. Every part is in the bar's own coordinates.
#[derive(Debug, Default)]
pub struct TabBarParts {
    /// Everything that is not a tab: the bar's background and rule, the
    /// session segment, the overflow chevrons. Drawn under the tabs.
    pub ground: Vec<DrawCmd>,
    /// Each tab's own commands, parallel to [`TabBar::tabs`]. A tab may
    /// paint past its own rect — Aero's active tab casts a shadow on its
    /// neighbours — so a client draws the active one above the rest.
    pub tabs: Vec<Vec<DrawCmd>>,
}

/// One tab: a number rect `[x0, x0 + num_w)` and a name rect after it.
pub struct Tab<'a> {
    pub index: usize,
    pub x0: f32,
    pub num_w: f32,
    pub name_w: f32,
    pub label: &'a str,
    pub active: bool,
    /// Unseen output in a background tab.
    pub activity: bool,
}

/// A pane's chrome, in pane-relative cells.
pub struct PaneChrome<'a> {
    pub w: f32,
    pub h: f32,
    pub title: &'a str,
    pub focused: bool,
    /// False for the only pane in a tab: there is nothing to tell apart.
    pub show_title: bool,
    pub scroll: Option<PaneScroll>,
}

/// Where a pane is scrolled to: `offset` rows up a scrollback of
/// `history_depth` (0 is the live screen).
#[derive(Clone, Copy, Debug)]
pub struct PaneScroll {
    pub offset: u32,
    pub history_depth: u32,
}

/// Where a document's page sits on screen, in cells.
#[derive(Clone, Copy, Debug)]
pub struct DocFrame {
    pub cols: f32,
    pub rows: f32,
    /// Left edge of the reading column.
    pub content_x: f32,
    /// Top of the page viewport, and its height.
    pub content_y: f32,
    pub content_h: f32,
}

/// The strings and scroll state the bars around a document show.
pub struct DocChrome<'a> {
    pub title: &'a str,
    /// Section the top of the viewport is inside.
    pub section: Option<&'a str>,
    /// Left half of the status line — a message, or the key hints.
    pub status: &'a str,
    /// True when `status` is an error.
    pub status_warn: bool,
    /// Fraction of the document above the viewport, 0..=1.
    pub progress: f32,
    /// Fraction of the document the viewport shows, 0..=1.
    pub visible: f32,
}

/// What a run of document text is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DocRole {
    Body,
    Heading(u8),
    Dim,
    Code,
    Link,
    /// A list bullet or number, a footnote label.
    Marker,
    Accent,
    Warn,
}

/// A picture's content: uploaded and drawable, or a label holding its
/// place in `font_scale`-sized text.
pub enum Figure<'a> {
    Image(String),
    Placeholder { label: &'a str },
}

/// What a file is, as its icon shows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    Dir,
    Image,
    Video,
    Audio,
    Text,
    Archive,
    Binary,
}

impl FileKind {
    pub const ALL: [FileKind; 7] = [
        FileKind::Dir,
        FileKind::Image,
        FileKind::Video,
        FileKind::Audio,
        FileKind::Text,
        FileKind::Archive,
        FileKind::Binary,
    ];
}

/// The box an icon is drawn into, in absolute cell coordinates.
#[derive(Debug, Clone, Copy)]
pub struct IconBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl IconBox {
    /// A box of `frac` of `area`'s smaller dimension, centered in it.
    /// `cell_pw/cell_ph` keep the result visually square.
    pub fn centered(area: (f32, f32, f32, f32), frac: f32, cell_pw: f32, cell_ph: f32) -> IconBox {
        let (ax, ay, aw, ah) = area;
        // Square in pixels: pick the side that fits both ways.
        let px = (aw * cell_pw).min(ah * cell_ph) * frac;
        let w = px / cell_pw.max(1.0);
        let h = px / cell_ph.max(1.0);
        IconBox {
            x: ax + (aw - w) * 0.5,
            y: ay + (ah - h) * 0.5,
            w,
            h,
        }
    }

    /// Map unit-box coordinates (0..1) into the box.
    pub(crate) fn p(&self, u: f32, v: f32) -> Point {
        Point {
            x: self.x + u * self.w,
            y: self.y + v * self.h,
        }
    }

    /// Map a unit-box rectangle into the box.
    pub(crate) fn r(&self, u: f32, v: f32, uw: f32, vh: f32) -> Rect {
        Rect {
            x: self.x + u * self.w,
            y: self.y + v * self.h,
            w: uw * self.w,
            h: vh * self.h,
        }
    }
}

/// Which of a detail-list row's texts is being drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListInk {
    /// A size or a date.
    Meta,
    Name {
        is_dir: bool,
    },
}

/// The colours a terminal picked for its own chrome, from its theme.
/// The host picks them and the engine makes materials of them: flat
/// paints them as they are, Aero keeps its own glass and takes the
/// accent and the warning colour as gels.
#[derive(Clone, Copy, Debug)]
pub struct HostInks {
    /// A panel's fill.
    pub surface: Color,
    /// Text over [`Self::surface`].
    pub text: Color,
    pub accent: Color,
    /// Text over [`Self::accent`].
    pub on_accent: Color,
    /// The warm trio for the answer that gets you nothing: fill,
    /// outline, text.
    pub warn_fill: Color,
    pub warn_border: Color,
    pub warn_text: Color,
}

/// What a [`DialogButton`] does, which is what it looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// The safe answer, the one Enter and Esc give.
    Default,
    /// The destructive one, which has to be aimed at.
    Danger,
}

#[derive(Clone, Copy, Debug)]
pub struct DialogButton<'a> {
    /// In panel cells.
    pub rect: Rect,
    pub label: &'a str,
    pub kind: ButtonKind,
    /// Under the pointer.
    pub hot: bool,
}

/// A modal question, laid out by the host: everything in cells,
/// relative to the panel's top-left corner.
#[derive(Clone, Copy, Debug)]
pub struct Dialog<'a> {
    pub w: f32,
    pub h: f32,
    /// The whole terminal, for the scrim — so its origin is negative.
    pub screen: Rect,
    pub title: &'a str,
    /// Top of the title's row, centred across the panel.
    pub title_y: f32,
    pub buttons: &'a [DialogButton<'a>],
    pub inks: &'a HostInks,
}

/// What the left half of a status line is showing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusLeft {
    /// An operation in flight.
    Busy,
    /// A message reporting a failure.
    Failed,
    Message,
    /// The current path.
    Path,
}

// ── Selection ──────────────────────────────────────────────────────────

const FLAT: u8 = 0;
const AERO: u8 = 1;

static STYLE: AtomicU8 = AtomicU8::new(FLAT);
/// Set once a client's own `--style` has picked the engine, which the
/// host's choice then no longer overrides.
static CHOSEN: AtomicBool = AtomicBool::new(false);

/// Pick the engine by name — a client's `--style`, which outranks the
/// one the host names.
pub fn select(name: &str) -> Result<(), String> {
    STYLE.store(parse(name)?, Ordering::Relaxed);
    CHOSEN.store(true, Ordering::Relaxed);
    Ok(())
}

/// Take the engine the host names (PRT §10: the probe's theme tail, or
/// a `HostThemeChanged`), unless `--style` already picked one. A name
/// this crate does not know, and an empty one, is flat. True when the
/// engine in force changed — a client then has to lay out again, since
/// the engines' [`Metrics`] differ.
pub fn adopt(name: &str) -> bool {
    if CHOSEN.load(Ordering::Relaxed) {
        return false;
    }
    let next = parse(name).unwrap_or(FLAT);
    STYLE.swap(next, Ordering::Relaxed) != next
}

fn parse(name: &str) -> Result<u8, String> {
    match name {
        "aero" => Ok(AERO),
        "flat" => Ok(FLAT),
        other => Err(format!("unknown style: {other} (flat, aero)")),
    }
}

/// The canonical name of the engine called `name`, for a host that
/// validates a configured style before publishing it.
pub fn canonical(name: &str) -> Result<&'static str, String> {
    parse(name).map(|s| if s == AERO { "aero" } else { "flat" })
}

/// The engine in force: flat until the host or `--style` names another.
pub fn engine() -> &'static dyn Engine {
    match STYLE.load(Ordering::Relaxed) {
        AERO => &aero::Aero,
        _ => &flat::Flat,
    }
}

// ── Settings ───────────────────────────────────────────────────────────

/// The user's settings for the engines, configured in veter's
/// `[style.<engine>]` and published beside the engine's name as
/// `<engine>.<key>` options (PRT §10). Every engine's travel whichever
/// one is in force, so a client whose `--style` picks another engine
/// still draws that one the user's way. None of them changes
/// [`Metrics`]: a change is a repaint, never a new layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// `flat.rounding` — the corner radius of flat chrome, as a multiple
    /// of the stock one: `0` is square, `2` twice as round.
    pub rounding: f32,
    /// `aero.bubbles` — whether Aero scatters bubbles over its skies,
    /// bars and frames.
    pub bubbles: bool,
    /// `aero.glass` — how milky Aero's frosted panes and reading sheet
    /// are, as a multiple of the stock opacity: below `1` more of the
    /// wallpaper shows through, above it they turn solid.
    pub glass: f32,
}

impl Settings {
    pub const DEFAULT: Self = Self {
        rounding: 1.0,
        bubbles: true,
        glass: 1.0,
    };
    /// How far `rounding` reaches; past it every corner is already as
    /// round as the shape allows.
    pub const MAX_ROUNDING: f32 = 4.0;
    pub const MAX_GLASS: f32 = 2.0;

    /// The settings `options` name, the stock value for any it does not
    /// or names unreadably. Values are clamped into range.
    pub fn from_options(options: &[(String, String)]) -> Self {
        let mut s = Self::DEFAULT;
        for (key, value) in options {
            let value = value.trim();
            match key.as_str() {
                "flat.rounding" => {
                    if let Ok(v) = value.parse::<f32>() {
                        s.rounding = v;
                    }
                }
                "aero.bubbles" => {
                    if let Ok(v) = value.parse() {
                        s.bubbles = v;
                    }
                }
                "aero.glass" => {
                    if let Ok(v) = value.parse::<f32>() {
                        s.glass = v;
                    }
                }
                _ => {}
            }
        }
        s.clamped()
    }

    /// What a host publishes for these settings.
    pub fn to_options(&self) -> Vec<(String, String)> {
        let s = self.clamped();
        [
            ("flat.rounding", s.rounding.to_string()),
            ("aero.bubbles", s.bubbles.to_string()),
            ("aero.glass", s.glass.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Into range; a NaN is the stock value.
    pub fn clamped(self) -> Self {
        let clamp = |v: f32, max: f32, stock: f32| {
            if v.is_nan() { stock } else { v.clamp(0.0, max) }
        };
        Self {
            rounding: clamp(self.rounding, Self::MAX_ROUNDING, 1.0),
            bubbles: self.bubbles,
            glass: clamp(self.glass, Self::MAX_GLASS, 1.0),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

static SETTINGS: RwLock<Settings> = RwLock::new(Settings::DEFAULT);

/// The settings in force.
pub fn settings() -> Settings {
    SETTINGS.read().map_or(Settings::DEFAULT, |s| *s)
}

/// Adopt the settings a host published. True when they changed, which
/// is a repaint.
pub fn adopt_settings(options: &[(String, String)]) -> bool {
    let next = Settings::from_options(options);
    SETTINGS
        .write()
        .is_ok_and(|mut s| std::mem::replace(&mut *s, next) != next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both engines paint the dialog veter lays out: the title and
    /// every button's label, each inside the panel, since veter's own
    /// hit-test reads the same rects.
    #[test]
    fn every_engine_draws_the_dialog_it_is_handed() {
        let ink = |r| Color {
            r,
            g: 0.5,
            b: 0.5,
            a: 1.0,
        };
        let inks = HostInks {
            surface: ink(0.1),
            text: ink(0.9),
            accent: ink(0.3),
            on_accent: ink(0.0),
            warn_fill: ink(0.6),
            warn_border: ink(0.7),
            warn_text: ink(0.8),
        };
        let button = |x, label, kind| DialogButton {
            rect: Rect {
                x,
                y: 3.0,
                w: 12.0,
                h: 2.0,
            },
            label,
            kind,
            hot: false,
        };
        let buttons = [
            button(2.0, "Cancel", ButtonKind::Default),
            button(16.0, "Quit", ButtonKind::Danger),
        ];
        let dialog = Dialog {
            w: 30.0,
            h: 7.0,
            screen: Rect {
                x: -20.0,
                y: -10.0,
                w: 80.0,
                h: 30.0,
            },
            title: "Close?",
            title_y: 1.0,
            buttons: &buttons,
            inks: &inks,
        };
        let px = Px { w: 8.0, h: 16.0 };
        for engine in [&flat::Flat as &dyn Engine, &aero::Aero] {
            let mut cmds = Vec::new();
            engine.dialog(&mut cmds, px, &dialog);
            let texts: Vec<(&str, Point)> = cmds
                .iter()
                .filter_map(|c| match c {
                    DrawCmd::DrawText { text, origin, .. } => Some((text.as_str(), *origin)),
                    _ => None,
                })
                .collect();
            for want in ["Close?", "Cancel", "Quit"] {
                let (_, at) = texts
                    .iter()
                    .find(|(t, _)| *t == want)
                    .unwrap_or_else(|| panic!("{}: no {want}", engine.name()));
                assert!(
                    (0.0..dialog.w).contains(&at.x) && (0.0..dialog.h).contains(&at.y),
                    "{}: {want} at {at:?}",
                    engine.name()
                );
            }
        }
    }

    #[test]
    fn icon_box_is_square_in_pixels_and_centered() {
        let b = IconBox::centered((10.0, 4.0, 12.0, 5.0), 0.8, 9.0, 20.0);
        assert!((b.w * 9.0 - b.h * 20.0).abs() < 1e-3, "square in pixels");
        // Centered within the area on both axes.
        assert!(((b.x - 10.0) - (12.0 - b.w - (b.x - 10.0))).abs() < 1e-3);
        assert!(((b.y - 4.0) - (5.0 - b.h - (b.y - 4.0))).abs() < 1e-3);
        // And it fits.
        assert!(b.w <= 12.0 && b.h <= 5.0);
    }

    /// The host's choice is taken whenever it changes the engine, and
    /// an unknown name is flat rather than a refusal to draw.
    #[test]
    fn the_host_names_the_engine() {
        adopt("flat");
        assert!(adopt("aero"));
        assert_eq!(engine().name(), "aero");
        assert!(!adopt("aero"), "no change, nothing to lay out again");
        assert!(adopt("vista"));
        assert_eq!(engine().name(), "flat");
        assert!(!adopt(""));
    }

    /// The settings survive the trip through the host's options, and
    /// what cannot be read is the stock value rather than a refusal.
    #[test]
    fn settings_round_trip_through_options() {
        let s = Settings {
            rounding: 0.0,
            bubbles: false,
            glass: 0.6,
        };
        assert_eq!(Settings::from_options(&s.to_options()), s);
        assert_eq!(Settings::from_options(&[]), Settings::DEFAULT);
        let odd = |k: &str, v: &str| Settings::from_options(&[(k.into(), v.into())]);
        assert_eq!(odd("aero.bubbles", "maybe"), Settings::DEFAULT);
        assert_eq!(odd("aero.sparkle", "1"), Settings::DEFAULT);
        assert_eq!(odd("flat.rounding", "NaN"), Settings::DEFAULT);
        assert_eq!(odd("flat.rounding", "-3").rounding, 0.0);
        assert_eq!(odd("aero.glass", "9").glass, Settings::MAX_GLASS);
    }

    #[test]
    fn style_names_parse() {
        assert_eq!(parse("aero").unwrap(), AERO);
        assert_eq!(parse("flat").unwrap(), FLAT);
        assert!(parse("vista").is_err());
        assert_eq!(canonical("aero"), Ok("aero"));
    }
}
