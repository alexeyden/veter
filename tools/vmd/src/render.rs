//! Rows → VGE draw commands.
//!
//! Layout works in em and knows nothing about the terminal; this is
//! where em becomes cells (one multiply by the zoom). What each part
//! looks like — and the colour each role's text takes — is the style
//! engine's ([`vge_ui::style`]); this module only decides where the
//! parts go. Nothing here holds state: the page is rebuilt from the
//! visible slice of rows on every scroll tick, which is bounded by the
//! pane height rather than the document length — a 10,000-line file
//! costs exactly what a one-screen one does.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, DrawCmd, FontStyle, Style};
use vge_ui::measure::{elide, text_cells};
use vge_ui::paint::Px;
use vge_ui::style::{self, DocChrome, DocFrame, DocRole, Engine, Figure};

use crate::layout::{Body, Deco, Layout, Role, Row, Run};

/// Thickness of a block quote's bar, in cells before zoom.
const BAR_W: f32 = 0.22;
/// How far a quote bar sits from the text it marks.
const BAR_GAP: f32 = 0.9;
/// Horizontal padding of the chip behind an inline `code` run.
const CHIP_PAD: f32 = 0.25;

/// Where a match sits: which row, which run in it, and the byte offset
/// the match starts at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MatchPos {
    pub row: usize,
    pub run: usize,
    pub start: usize,
    pub len: usize,
}

/// The search state the page paints: every occurrence gets a wash, the
/// one the reader is on gets a stronger one.
#[derive(Clone, Copy, Default)]
pub struct Search<'a> {
    pub query: &'a str,
    pub active: Option<MatchPos>,
}

/// Everything about the terminal and the scroll position the page draws
/// against. Cells throughout, except `zoom`, which converts em to them.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub cols: f32,
    pub rows: f32,
    pub cell_pw: f32,
    pub cell_ph: f32,
    /// Left edge of the reading column, in cells.
    pub content_x: f32,
    /// Top of the page viewport, in cells from the top of the screen.
    /// The page element's origin, so page-local y is screen y minus it.
    pub content_y: f32,
    pub content_h: f32,
    pub zoom: f32,
    /// Cells of document scrolled past the top of the viewport.
    pub scroll: f32,
}

impl View {
    /// Page-local y of a document position, in cells.
    fn y_of(&self, top_em: f32) -> f32 {
        top_em * self.zoom - self.scroll
    }

    fn x_of(&self, x_em: f32) -> f32 {
        self.content_x + x_em * self.zoom
    }

    fn px(&self) -> Px {
        Px {
            w: self.cell_pw,
            h: self.cell_ph,
        }
    }

    /// Where the page sits, as the engine draws the parts around it.
    pub fn frame(&self) -> DocFrame {
        DocFrame {
            cols: self.cols,
            rows: self.rows,
            content_x: self.content_x,
            content_y: self.content_y,
            content_h: self.content_h,
        }
    }
}

/// Whether a picture is on screen yet, so the page can draw a plate and
/// a caption while a decode is in flight.
pub enum Picture {
    /// Uploaded and drawable under this VGE image id.
    Ready(String),
    /// A worker has it.
    Pending,
    /// Nothing will come — unreadable file, or a remote URL.
    Missing,
}

/// What the page asks about each picture it is about to draw.
pub trait Pictures {
    fn picture(&self, image: &crate::layout::ImageBox) -> Picture;
}

// ─────────────────────────────────────────────────────────────────────
// Text
// ─────────────────────────────────────────────────────────────────────

fn doc_role(role: Role) -> DocRole {
    match role {
        Role::Body => DocRole::Body,
        Role::Heading(l) => DocRole::Heading(l),
        Role::Dim => DocRole::Dim,
        Role::Code => DocRole::Code,
        Role::Link => DocRole::Link,
        Role::Marker => DocRole::Marker,
        Role::Accent => DocRole::Accent,
        Role::Warn => DocRole::Warn,
    }
}

fn font_bits(run: &Run) -> FontStyle {
    let mut bits = 0u8;
    if run.emph.bold || matches!(run.role, Role::Heading(_)) {
        bits |= 0x01;
    }
    if run.emph.italic {
        bits |= 0x02;
    }
    if run.role == Role::Link {
        bits |= 0x04;
    }
    if run.emph.strike {
        bits |= 0x08;
    }
    FontStyle(bits)
}

// ─────────────────────────────────────────────────────────────────────
// The page
// ─────────────────────────────────────────────────────────────────────

/// Draw commands for the rows visible in `view`.
pub fn page(
    layout: &Layout,
    view: &View,
    pictures: &dyn Pictures,
    search: Search<'_>,
) -> Vec<DrawCmd> {
    let engine = style::engine();
    let mut cmds = Vec::new();
    for (i, row) in visible(layout, view) {
        let y = view.y_of(row.top);
        let h = row.height * view.zoom;
        decoration(&mut cmds, engine, row, view, y, h);
        quote_bars(&mut cmds, engine, row, view, y, h);
        match &row.body {
            Body::Rule { weight } => engine.rule(
                &mut cmds,
                Rect {
                    x: view.x_of(row.x),
                    y,
                    w: row.width * view.zoom,
                    h,
                },
                *weight * view.zoom,
            ),
            Body::Image(img) => picture(&mut cmds, engine, img, row, view, y, h, pictures),
            Body::Runs(runs) => {
                for (j, run) in runs.iter().enumerate() {
                    // Washes and marks hug left-aligned runs only; a
                    // right-aligned one (a table's number column) has no
                    // left edge to measure from.
                    if run.align == Align::Left {
                        if !search.query.is_empty() {
                            highlight(&mut cmds, engine, run, view, y, h, &search, i, j);
                        }
                        if run.role == Role::Code && !matches!(row.deco, Deco::Plate { .. }) {
                            let x0 = view.x_of(run.x) - CHIP_PAD * view.zoom;
                            let x1 = view.x_of(run.x + run.width()) + CHIP_PAD * view.zoom;
                            engine.code_chip(&mut cmds, view.px(), x0, x1, y, h, view.zoom);
                        }
                        if let Role::Heading(level) = run.role {
                            engine.heading_mark(
                                &mut cmds,
                                level,
                                view.x_of(run.x),
                                run.width() * view.zoom,
                                y,
                                h,
                                view.zoom,
                            );
                        }
                    }
                    cmds.push(text(engine, run, view, y, h));
                }
            }
        }
    }
    cmds
}

/// Rows overlapping the viewport, with their index in the layout.
///
/// Rows are in `top` order but not strictly increasing — a list marker
/// shares its item's first row — so this is a linear scan with an early
/// exit rather than a binary search over an ordering that does not
/// quite hold.
fn visible<'a>(layout: &'a Layout, view: &View) -> impl Iterator<Item = (usize, &'a Row)> {
    let top = view.scroll / view.zoom;
    let bottom = (view.scroll + view.content_h) / view.zoom;
    layout
        .rows
        .iter()
        .enumerate()
        .skip_while(move |(_, r)| r.top + r.height < top)
        .take_while(move |(_, r)| r.top <= bottom)
}

fn decoration(cmds: &mut Vec<DrawCmd>, engine: &dyn Engine, row: &Row, view: &View, y: f32, h: f32) {
    let x0 = view.x_of(row.x);
    let x1 = view.x_of(row.x + row.width);
    let band = Rect {
        x: x0,
        y,
        w: x1 - x0,
        h,
    };
    match row.deco {
        Deco::None => {}
        Deco::Plate { first, last } => {
            engine.code_plate(cmds, view.px(), x0, x1, y, h, first, last, view.zoom)
        }
        Deco::TableHead => engine.table_band(cmds, band, true),
        Deco::TableBand => engine.table_band(cmds, band, false),
    }
}

fn quote_bars(cmds: &mut Vec<DrawCmd>, engine: &dyn Engine, row: &Row, view: &View, y: f32, h: f32) {
    for bar in &row.quotes {
        let warning = bar.alert.is_some_and(|a| a.is_warning());
        engine.quote_bar(
            cmds,
            Rect {
                x: view.x_of(bar.x + BAR_GAP - BAR_W),
                y,
                w: BAR_W * view.zoom,
                h,
            },
            warning,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn picture(
    cmds: &mut Vec<DrawCmd>,
    engine: &dyn Engine,
    img: &crate::layout::ImageBox,
    row: &Row,
    view: &View,
    y: f32,
    h: f32,
    pictures: &dyn Pictures,
) {
    // The row's own left edge, not the reading column's: a picture
    // inside a list item or a quote is indented with the text it
    // belongs to, and layout already fitted its width to that box.
    let rect = Rect {
        x: view.x_of(row.x),
        y,
        w: img.w * view.zoom,
        h,
    };
    let label;
    let content = match pictures.picture(img) {
        Picture::Ready(id) => Figure::Image(id),
        state => {
            let alt = match state {
                Picture::Pending => img.alt.clone(),
                _ => format!("{}  (unreadable)", img.alt),
            };
            let budget = (rect.w / view.zoom).max(1.0) as usize;
            label = elide(&alt, budget);
            Figure::Placeholder { label: &label }
        }
    };
    engine.figure(cmds, view.px(), rect, content, view.zoom);
}

/// A search mark on every occurrence of the query inside `run`, the
/// one the reader is on marked as current.
#[allow(clippy::too_many_arguments)]
fn highlight(
    cmds: &mut Vec<DrawCmd>,
    engine: &dyn Engine,
    run: &Run,
    view: &View,
    y: f32,
    h: f32,
    search: &Search<'_>,
    row: usize,
    index: usize,
) {
    for (start, len) in matches_in(&run.text, search.query) {
        let before = text_cells(&run.text[..start]) * run.scale;
        let width = text_cells(&run.text[start..start + len]) * run.scale;
        let active = search.active
            == Some(MatchPos {
                row,
                run: index,
                start,
                len,
            });
        engine.search_hit(
            cmds,
            view.px(),
            view.x_of(run.x + before),
            width * view.zoom,
            y,
            h,
            view.zoom,
            active,
        );
    }
}

/// Byte offsets of every case-insensitive occurrence of `needle` in
/// `haystack`.
///
/// ASCII-lowercased rather than `to_lowercase`d, because a Unicode
/// case fold can change a string's byte length and the offsets have to
/// index the original.
pub fn matches_in(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let hay = haystack.to_ascii_lowercase();
    let pat = needle.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = hay[from..].find(&pat) {
        let at = from + i;
        out.push((at, pat.len()));
        from = at + pat.len().max(1);
        if from > hay.len() {
            break;
        }
    }
    out
}

fn text(engine: &dyn Engine, run: &Run, view: &View, y: f32, h: f32) -> DrawCmd {
    let scale = run.scale * view.zoom;
    DrawCmd::DrawText {
        // Centre the glyph box in the row: the leading is a property of
        // the row, not of the run, and a small run on a tall row should
        // sit on the same optical line as a big one.
        origin: Point {
            x: view.x_of(run.x),
            y: y + (h - scale) * 0.5,
        },
        align: run.align,
        fill: Style::Flat(engine.doc_ink(doc_role(run.role))),
        font_style: font_bits(run),
        font_scale: scale.clamp(0.05, 63.0),
        text: run.text.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Chrome
// ─────────────────────────────────────────────────────────────────────

pub type Chrome<'a> = DocChrome<'a>;

/// The header bar, status line and scrollbar — everything outside the
/// page's clip.
pub fn chrome(c: &Chrome<'_>, view: &View) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();
    style::engine().doc_chrome(&mut cmds, view.px(), &view.frame(), c);
    cmds
}

/// What sits under the page, in screen cells — its own element.
pub fn ground(view: &View) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();
    style::engine().doc_ground(&mut cmds, view.px(), &view.frame());
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ImageSizes, Metrics, lay_out};
    use std::path::Path;

    struct FakeImages(u32, u32);
    impl ImageSizes for FakeImages {
        fn size(&self, _: &Path) -> Option<(u32, u32)> {
            Some((self.0, self.1))
        }
    }
    impl Pictures for FakeImages {
        fn picture(&self, _: &crate::layout::ImageBox) -> Picture {
            Picture::Ready("img".into())
        }
    }

    /// A picture inside a list item is indented with the text it
    /// belongs to, not flush with the reading column.
    #[test]
    fn an_indented_picture_is_drawn_at_its_own_left_edge() {
        let sizes = FakeImages(100, 100);
        let metrics = Metrics {
            cell_pw: 10.0,
            cell_ph: 20.0,
            max_image_h: 20.0,
        };
        let view = View {
            cols: 80.0,
            rows: 24.0,
            cell_pw: 10.0,
            cell_ph: 20.0,
            content_x: 4.0,
            content_y: 1.0,
            content_h: 22.0,
            zoom: 1.0,
            scroll: 0.0,
        };
        let x_of = |md: &str| {
            let doc = crate::doc::parse(md, Path::new("."));
            let layout = lay_out(&doc, 40.0, metrics, &sizes);
            page(&layout, &view, &sizes, Search::default())
                .into_iter()
                .find_map(|c| match c {
                    DrawCmd::DrawImage { target_rect, .. } => Some(target_rect.x),
                    _ => None,
                })
                .expect("an image was drawn")
        };
        let flush = x_of("![x](Cargo.toml)");
        let nested = x_of("- item\n\n  ![x](Cargo.toml)\n");
        assert!((flush - view.content_x).abs() < 1e-3, "{flush}");
        assert!(nested > flush, "{nested} should be indented past {flush}");
    }

    #[test]
    fn matches_are_case_insensitive_and_index_the_original() {
        let hay = "Foo foo FOO";
        let found = matches_in(hay, "foo");
        assert_eq!(found.len(), 3);
        for (at, len) in found {
            assert_eq!(hay[at..at + len].to_ascii_lowercase(), "foo");
        }
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(matches_in("anything", "").is_empty());
    }

    #[test]
    fn overlapping_matches_advance_past_themselves() {
        // "aa" in "aaaa" is found twice, not four times — and the loop
        // terminates, which is the part worth pinning.
        assert_eq!(matches_in("aaaa", "aa").len(), 2);
    }
}
