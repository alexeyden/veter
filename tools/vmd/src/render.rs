//! Rows → VGE draw commands.
//!
//! Layout works in em and knows nothing about the terminal; this is
//! where em becomes cells (one multiply by the zoom) and roles become
//! colours. Nothing here holds state: the page is rebuilt from the
//! visible slice of rows on every scroll tick, which is bounded by the
//! pane height rather than the document length — a 10,000-line file
//! costs exactly what a one-screen one does.
//!
//! The page is deliberately *not* given a ground of its own. The
//! terminal's background is the paper, so a themed veter, a light
//! scheme and a plain xterm all read correctly without vmd choosing a
//! colour for any of them.

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, DrawCmd, FontStyle, Style};
use vge_ui::measure::{elide, text_cells};
use vge_ui::shape::{chrome_corner_radii, rounded_rect_path, rounded_rect_path_corners};
use vge_ui::theme;

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
// Colours
// ─────────────────────────────────────────────────────────────────────

/// Body text on a terminal that publishes no theme.
const COLOR_TEXT: Color = Color {
    r: 0.86,
    g: 0.87,
    b: 0.90,
    a: 1.0,
};

fn text_color() -> Color {
    theme::host_fg().unwrap_or(COLOR_TEXT)
}

fn alpha(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

fn role_color(role: Role) -> Color {
    match role {
        Role::Body => text_color(),
        // A colour spine down the document: h1 takes the accent, the
        // levels under it step down through the text colours, so depth
        // reads even where the size difference is small.
        Role::Heading(1) => theme::accent_color(),
        Role::Heading(l) if l <= 4 => theme::title_text(),
        Role::Heading(_) => theme::dim_text(),
        Role::Dim | Role::Marker => theme::dim_text(),
        Role::Code => text_color(),
        Role::Link | Role::Accent => theme::accent_color(),
        Role::Warn => theme::warn_color().unwrap_or(theme::accent_color()),
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
    let mut cmds = Vec::new();
    for (i, row) in visible(layout, view) {
        let y = view.y_of(row.top);
        let h = row.height * view.zoom;
        decoration(&mut cmds, row, view, y, h);
        quote_bars(&mut cmds, row, view, y, h);
        match &row.body {
            Body::Rule { weight } => rule(&mut cmds, row, view, y, h, *weight),
            Body::Image(img) => picture(&mut cmds, img, row, view, y, h, pictures),
            Body::Runs(runs) => {
                for (j, run) in runs.iter().enumerate() {
                    if !search.query.is_empty() {
                        highlight(&mut cmds, run, view, y, h, &search, i, j);
                    }
                    if run.role == Role::Code && !matches!(row.deco, Deco::Plate { .. }) {
                        chip(&mut cmds, run, view, y, h);
                    }
                    cmds.push(text(run, view, y, h));
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

fn decoration(cmds: &mut Vec<DrawCmd>, row: &Row, view: &View, y: f32, h: f32) {
    let x0 = view.x_of(row.x);
    let x1 = view.x_of(row.x + row.width);
    match row.deco {
        Deco::None => {}
        Deco::Plate { first, last } => {
            let (rx, ry) = chrome_corner_radii(x1 - x0, h.max(1.0), view.cell_pw, view.cell_ph);
            cmds.push(DrawCmd::FillPath {
                fill: Style::Flat(theme::inset_bg()),
                segments: rounded_rect_path_corners(
                    x0,
                    y,
                    x1,
                    y + h,
                    rx,
                    ry,
                    first,
                    first,
                    last,
                    last,
                ),
            });
        }
        Deco::TableHead => cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(alpha(theme::accent_color(), 0.18)),
            rects: vec![Rect {
                x: x0,
                y,
                w: x1 - x0,
                h,
            }],
        }),
        Deco::TableBand => cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(alpha(text_color(), 0.05)),
            rects: vec![Rect {
                x: x0,
                y,
                w: x1 - x0,
                h,
            }],
        }),
    }
}

fn quote_bars(cmds: &mut Vec<DrawCmd>, row: &Row, view: &View, y: f32, h: f32) {
    for bar in &row.quotes {
        let color = match bar.alert {
            Some(a) if a.is_warning() => theme::warn_color().unwrap_or(theme::accent_color()),
            _ => theme::accent_color(),
        };
        cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(alpha(color, 0.75)),
            rects: vec![Rect {
                x: view.x_of(bar.x + BAR_GAP - BAR_W),
                y,
                w: BAR_W * view.zoom,
                h,
            }],
        });
    }
}

fn rule(cmds: &mut Vec<DrawCmd>, row: &Row, view: &View, y: f32, h: f32, weight: f32) {
    let w = (weight * view.zoom).max(0.02);
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(text_color(), 0.22)),
        rects: vec![Rect {
            x: view.x_of(row.x),
            y: y + (h - w) * 0.5,
            w: row.width * view.zoom,
            h: w,
        }],
    });
}

#[allow(clippy::too_many_arguments)]
fn picture(
    cmds: &mut Vec<DrawCmd>,
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
    match pictures.picture(img) {
        Picture::Ready(id) => cmds.push(DrawCmd::DrawImage {
            target_rect: rect,
            image_id: id,
            source_rect: None,
        }),
        // Decoding, or gone: hold the space with the plate the picture
        // will fill, so nothing under it jumps when it lands.
        state => {
            let (rx, ry) = chrome_corner_radii(rect.w, rect.h, view.cell_pw, view.cell_ph);
            cmds.push(DrawCmd::FillPath {
                fill: Style::Flat(theme::inset_bg()),
                segments: rounded_rect_path(
                    rect.x,
                    rect.y,
                    rect.x + rect.w,
                    rect.y + rect.h,
                    rx,
                    ry,
                ),
            });
            let label = match state {
                Picture::Pending => img.alt.clone(),
                _ => format!("{}  (unreadable)", img.alt),
            };
            let budget = (rect.w / view.zoom).max(1.0) as usize;
            cmds.push(DrawCmd::DrawText {
                origin: Point {
                    x: rect.x + rect.w * 0.5,
                    y: rect.y + (rect.h - view.zoom) * 0.5,
                },
                align: Align::Center,
                fill: Style::Flat(theme::dim_text()),
                font_style: FontStyle(0),
                font_scale: view.zoom,
                text: elide(&label, budget),
            });
        }
    }
}

/// The plate behind an inline `code` span, which has no code block to
/// sit on.
fn chip(cmds: &mut Vec<DrawCmd>, run: &Run, view: &View, y: f32, h: f32) {
    if run.align != Align::Left {
        return;
    }
    let x0 = view.x_of(run.x) - CHIP_PAD * view.zoom;
    let x1 = view.x_of(run.x + run.width()) + CHIP_PAD * view.zoom;
    let inset = 0.06 * view.zoom;
    let (rx, ry) = chrome_corner_radii(x1 - x0, h, view.cell_pw, view.cell_ph);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(alpha(theme::inset_bg(), 0.85)),
        segments: rounded_rect_path(x0, y + inset, x1, y + h - inset, rx, ry),
    });
}

/// Wash behind every occurrence of the query inside `run`, brighter for
/// the one the reader is on.
#[allow(clippy::too_many_arguments)]
fn highlight(
    cmds: &mut Vec<DrawCmd>,
    run: &Run,
    view: &View,
    y: f32,
    h: f32,
    search: &Search<'_>,
    row: usize,
    index: usize,
) {
    if run.align != Align::Left {
        return;
    }
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
        cmds.push(DrawCmd::FillRectangles {
            fill: Style::Flat(alpha(
                theme::accent_color(),
                if active { 0.65 } else { 0.28 },
            )),
            rects: vec![Rect {
                x: view.x_of(run.x + before),
                y: y + 0.05 * view.zoom,
                w: width * view.zoom,
                h: h - 0.1 * view.zoom,
            }],
        });
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

fn text(run: &Run, view: &View, y: f32, h: f32) -> DrawCmd {
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
        fill: Style::Flat(role_color(run.role)),
        font_style: font_bits(run),
        font_scale: scale.clamp(0.05, 63.0),
        text: run.text.clone(),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Chrome
// ─────────────────────────────────────────────────────────────────────

pub struct Chrome<'a> {
    pub title: &'a str,
    /// Section the top of the viewport is inside.
    pub section: Option<&'a str>,
    /// Left half of the status line — a message, or the key hints.
    pub status: &'a str,
    /// True when `status` is an error and should take the warm tone.
    pub status_warn: bool,
    /// Fraction of the document above the viewport, 0..=1.
    pub progress: f32,
    /// Fraction of the document the viewport shows, 0..=1.
    pub visible: f32,
}

/// The header bar, status line and scrollbar — everything outside the
/// page's clip.
pub fn chrome(c: &Chrome<'_>, view: &View) -> Vec<DrawCmd> {
    let mut cmds = Vec::new();
    let surface = theme::modal_bg();
    let last_row = (view.rows - 1.0).max(0.0);

    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(surface),
        rects: vec![
            Rect {
                x: 0.0,
                y: 0.0,
                w: view.cols,
                h: 1.0,
            },
            Rect {
                x: 0.0,
                y: last_row,
                w: view.cols,
                h: 1.0,
            },
        ],
    });
    // A hairline under the header, in the accent, so the bar reads as
    // chrome rather than as the first line of the document.
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(theme::accent_color(), 0.55)),
        rects: vec![Rect {
            x: 0.0,
            y: 1.0 - 0.06,
            w: view.cols,
            h: 0.06,
        }],
    });

    let half = (view.cols as usize).saturating_sub(4) / 2;
    cmds.push(bar_text(
        1.0,
        0.0,
        Align::Left,
        theme::title_text(),
        0x01,
        elide(c.title, half.max(8)),
    ));
    if let Some(section) = c.section.filter(|s| !s.is_empty()) {
        cmds.push(bar_text(
            view.cols - 1.0,
            0.0,
            Align::Right,
            theme::dim_text(),
            0,
            elide(section, half.max(8)),
        ));
    }

    let status_color = if c.status_warn {
        theme::warn_color().unwrap_or(theme::accent_color())
    } else {
        theme::dim_text()
    };
    cmds.push(bar_text(
        1.0,
        last_row,
        Align::Left,
        status_color,
        0,
        elide(c.status, (view.cols as usize).saturating_sub(8)),
    ));
    cmds.push(bar_text(
        view.cols - 1.0,
        last_row,
        Align::Right,
        theme::dim_text(),
        0,
        format!("{:>3}%", (c.progress * 100.0).round() as i32),
    ));

    scrollbar(&mut cmds, c, view);
    cmds
}

fn bar_text(x: f32, y: f32, align: Align, fill: Color, bits: u8, text: String) -> DrawCmd {
    DrawCmd::DrawText {
        origin: Point { x, y },
        align,
        fill: Style::Flat(fill),
        font_style: FontStyle(bits),
        font_scale: 1.0,
        text,
    }
}

fn scrollbar(cmds: &mut Vec<DrawCmd>, c: &Chrome<'_>, view: &View) {
    if c.visible >= 1.0 {
        return;
    }
    let track_y = view.content_y;
    let track_h = view.content_h;
    if track_h <= 0.0 {
        return;
    }
    let x = view.cols - 1.0;
    cmds.push(DrawCmd::FillRectangles {
        fill: Style::Flat(alpha(text_color(), 0.08)),
        rects: vec![Rect {
            x,
            y: track_y,
            w: 1.0,
            h: track_h,
        }],
    });
    let thumb_h = (track_h * c.visible).max(1.0);
    let thumb_y = track_y + (track_h - thumb_h) * c.progress.clamp(0.0, 1.0);
    let (rx, ry) = chrome_corner_radii(1.0, thumb_h, view.cell_pw, view.cell_ph);
    cmds.push(DrawCmd::FillPath {
        fill: Style::Flat(theme::scrollbar()),
        segments: rounded_rect_path(x + 0.25, thumb_y, x + 0.75, thumb_y + thumb_h, rx, ry),
    });
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
