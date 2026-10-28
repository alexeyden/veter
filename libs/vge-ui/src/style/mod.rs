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

    /// The tab bar, across the top [`TabbarMetrics::rows`] rows.
    fn tab_bar(&self, cmds: &mut Vec<DrawCmd>, px: Px, bar: &TabBar<'_>);

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn style_names_parse() {
        assert_eq!(parse("aero").unwrap(), AERO);
        assert_eq!(parse("flat").unwrap(), FLAT);
        assert!(parse("vista").is_err());
        assert_eq!(canonical("aero"), Ok("aero"));
    }
}
