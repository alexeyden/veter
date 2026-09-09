//! Blocks → rows, laid out in **em units**.
//!
//! One em is a cell at zoom 1.0, so every coordinate here is a number
//! the renderer multiplies by the current zoom to get cells — which is
//! also what a VGE `font_scale` multiplies (§7.4). Laying out in em
//! rather than cells is what makes `+`/`-` a pure re-render at a
//! different multiplier for everything except the wrap width, and the
//! wrap width is the one thing that genuinely has to change: bigger
//! glyphs mean fewer of them across the same pane.
//!
//! Text widths come from `vge-ui`'s cell rule (`unicode-width`), which
//! is the grid's own approximation and not the host's real glyph
//! advances. Where being off by a fraction of a cell would show — a
//! right-aligned table column — the run carries a VGE `Align` and is
//! anchored to the edge it belongs to, so the host does the arithmetic
//! that matters.

use std::path::PathBuf;

use vge_protocol::command::Align as VgeAlign;
use vge_ui::measure::text_cells;

use crate::doc::{Alert, Align, Block, Doc, Emphasis, Inline, Item, Table};

// ─────────────────────────────────────────────────────────────────────
// Vertical rhythm, all in em
// ─────────────────────────────────────────────────────────────────────

/// Body line height. Above 1.0 so wrapped prose breathes; the grid
/// behind it is on 1.0 and the two are not meant to line up.
const LINE: f32 = 1.15;
const PARA_GAP: f32 = 0.75;
const HEAD_GAP_BEFORE: f32 = 0.95;
const HEAD_GAP_AFTER: f32 = 0.35;
/// Leading of a heading, as a multiple of its own font scale.
const HEAD_LEADING: f32 = 1.3;
const CODE_LINE: f32 = 1.1;
/// Vertical padding inside a code plate.
const CODE_PAD_Y: f32 = 0.4;
/// Horizontal padding inside a code plate.
const CODE_PAD_X: f32 = 1.0;
/// Indent of a code continuation line, when one source line wraps.
const CODE_HANG: f32 = 2.0;
const ITEM_GAP: f32 = 0.2;
const RULE_GAP: f32 = 0.75;
const TABLE_LINE: f32 = 1.25;
/// Padding either side of a table cell's text.
const CELL_PAD: f32 = 1.0;
/// Narrowest a squeezed table column gets before the text is elided.
const MIN_COL: f32 = 5.0;
/// Indent a block quote adds to its contents, past the bar.
const QUOTE_INDENT: f32 = 2.0;
/// Indent a footnote definition's body takes.
const FOOTNOTE_INDENT: f32 = 3.5;
/// Width reserved for an unordered list's bullet.
const BULLET_W: f32 = 2.0;
/// Placeholder height for a picture whose size we could not read.
const IMAGE_STUB_H: f32 = 3.0;

/// Font scale of each heading level, `h1` first.
const HEADING_SCALE: [f32; 6] = [1.95, 1.55, 1.3, 1.15, 1.0, 1.0];

pub fn heading_scale(level: u8) -> f32 {
    HEADING_SCALE[(level.clamp(1, 6) - 1) as usize]
}

/// Bullet glyph per nesting depth, cycling past the third level.
const BULLETS: [&str; 3] = ["•", "◦", "▪"];

// ─────────────────────────────────────────────────────────────────────
// Output
// ─────────────────────────────────────────────────────────────────────

/// What a run is *for*, which is what picks its colour. Kept apart from
/// [`Emphasis`], which picks its font bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
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

#[derive(Clone, Debug)]
pub struct Run {
    /// Em from the left edge of the content box. For a centered or
    /// right-aligned run this is the anchor the host aligns *to*, not
    /// the run's left edge.
    pub x: f32,
    pub text: String,
    pub emph: Emphasis,
    /// Multiplier on the document's own font scale.
    pub scale: f32,
    pub role: Role,
    pub align: VgeAlign,
    /// Index into [`Doc::links`], for click-to-open and the link picker.
    pub link: Option<usize>,
}

impl Run {
    fn new(x: f32, text: String, emph: Emphasis, scale: f32, role: Role) -> Self {
        Run {
            x,
            text,
            emph,
            scale,
            role,
            align: VgeAlign::Left,
            link: None,
        }
    }

    /// Width in em, by the cell rule.
    pub fn width(&self) -> f32 {
        text_cells(&self.text) * self.scale
    }
}

/// A quote bar to paint down the left of a row.
#[derive(Clone, Copy, Debug)]
pub struct QuoteBar {
    pub x: f32,
    pub alert: Option<Alert>,
}

/// A band painted behind a row, under its runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Deco {
    None,
    /// Recessed plate behind a code block or a picture placeholder.
    /// The flags round the corners at the two ends of the run.
    Plate {
        first: bool,
        last: bool,
    },
    TableHead,
    /// Every other table body row, so a wide table stays readable
    /// across.
    TableBand,
}

#[derive(Clone, Debug)]
pub struct ImageBox {
    pub path: Option<PathBuf>,
    pub alt: String,
    /// Drawn width in em. The height is the row's, since a picture is
    /// the only thing on the row it occupies.
    pub w: f32,
}

#[derive(Clone, Debug)]
pub enum Body {
    Runs(Vec<Run>),
    /// A thematic break, or the line under a table header.
    Rule {
        weight: f32,
    },
    Image(ImageBox),
}

#[derive(Clone, Debug)]
pub struct Row {
    /// Em from the top of the document.
    pub top: f32,
    pub height: f32,
    /// Left edge and width of this row's band, in em. Runs carry their
    /// own x, which is usually but not always inside it.
    pub x: f32,
    pub width: f32,
    pub quotes: Vec<QuoteBar>,
    pub deco: Deco,
    pub body: Body,
}

impl Row {
    pub fn runs(&self) -> &[Run] {
        match &self.body {
            Body::Runs(r) => r,
            _ => &[],
        }
    }
}

/// One heading, for the outline picker and the header breadcrumb.
#[derive(Clone, Debug)]
pub struct Anchor {
    pub level: u8,
    pub text: String,
    pub slug: String,
    pub top: f32,
}

pub struct Layout {
    pub rows: Vec<Row>,
    /// Total document height in em.
    pub height: f32,
    pub outline: Vec<Anchor>,
}

impl Layout {
    /// The last heading at or above `top` — what the header bar shows
    /// as "where you are".
    pub fn heading_at(&self, top: f32) -> Option<&Anchor> {
        self.outline.iter().rev().find(|a| a.top <= top + 0.5)
    }

    /// Top of the heading whose slug matches, for a `#anchor` link.
    pub fn anchor(&self, slug: &str) -> Option<f32> {
        self.outline.iter().find(|a| a.slug == slug).map(|a| a.top)
    }
}

/// What the layout needs to know about the terminal: how anisotropic
/// its cells are (an image's aspect ratio depends on it) and how much
/// vertical room a picture may take.
#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub cell_pw: f32,
    pub cell_ph: f32,
    /// Tallest a picture may be drawn, in em.
    pub max_image_h: f32,
}

/// Lay `doc` out into a content box `width` em wide.
pub fn lay_out(doc: &Doc, width: f32, metrics: Metrics, sizes: &dyn ImageSizes) -> Layout {
    let width = width.max(8.0);
    let mut ctx = Ctx {
        rows: Vec::new(),
        outline: Vec::new(),
        y: 0.0,
        quotes: Vec::new(),
        depth: 0,
        metrics,
        sizes,
    };
    ctx.blocks(&doc.blocks, 0.0, width);
    Layout {
        height: ctx.y,
        rows: ctx.rows,
        outline: ctx.outline,
    }
}

/// Pixel dimensions of an image, if they can be had cheaply. Layout has
/// to reserve a picture's height before anything is decoded, so this
/// reads headers only — the decode happens later, and only for the
/// pictures that come into view.
pub trait ImageSizes {
    fn size(&self, path: &std::path::Path) -> Option<(u32, u32)>;
}

// ─────────────────────────────────────────────────────────────────────
// The walk
// ─────────────────────────────────────────────────────────────────────

struct Ctx<'a> {
    rows: Vec<Row>,
    outline: Vec<Anchor>,
    y: f32,
    quotes: Vec<QuoteBar>,
    /// Nesting depth of the list being laid out, which picks the
    /// bullet glyph.
    depth: usize,
    metrics: Metrics,
    sizes: &'a dyn ImageSizes,
}

impl Ctx<'_> {
    fn push(&mut self, height: f32, x: f32, width: f32, deco: Deco, body: Body) {
        self.rows.push(Row {
            top: self.y,
            height,
            x,
            width,
            quotes: self.quotes.clone(),
            deco,
            body,
        });
        self.y += height;
    }

    fn blocks(&mut self, blocks: &[Block], x: f32, width: f32) {
        for (i, block) in blocks.iter().enumerate() {
            if i > 0 {
                self.y += gap_before(block);
            }
            self.block(block, x, width);
            if i + 1 < blocks.len() {
                self.y += gap_after(block);
            }
        }
    }

    fn block(&mut self, block: &Block, x: f32, width: f32) {
        match block {
            Block::Heading {
                level,
                slug,
                inlines,
            } => self.heading(*level, slug, inlines, x, width),
            Block::Paragraph(inlines) => self.paragraph(inlines, x, width),
            Block::Code { lang, lines } => self.code(lang.as_deref(), lines, x, width),
            Block::Quote { alert, blocks } => self.quote(*alert, blocks, x, width),
            Block::List { start, items } => self.list(*start, items, x, width),
            Block::Table(t) => self.table(t, x, width),
            Block::Rule => {
                self.y += RULE_GAP;
                self.push(LINE, x, width, Deco::None, Body::Rule { weight: 0.08 });
                self.y += RULE_GAP;
            }
            Block::Image(img) => self.image(img, x, width),
            Block::Footnote { label, blocks } => self.footnote(label, blocks, x, width),
        }
    }

    fn heading(&mut self, level: u8, slug: &str, inlines: &[Inline], x: f32, width: f32) {
        let scale = heading_scale(level);
        let role = Role::Heading(level);
        self.outline.push(Anchor {
            level,
            text: crate::doc::plain_text(inlines),
            slug: slug.to_string(),
            top: self.y,
        });
        let lines = wrap(inlines, width, scale, role);
        let line_h = scale * HEAD_LEADING;
        for line in lines {
            self.push(line_h, x, width, Deco::None, Body::Runs(offset(line, x)));
        }
        // An h1/h2 gets a hairline under it, the way a printed page
        // separates its top-level sections.
        if level <= 2 {
            self.y += 0.15;
            self.push(
                0.35,
                x,
                width,
                Deco::None,
                Body::Rule {
                    weight: if level == 1 { 0.07 } else { 0.05 },
                },
            );
        }
    }

    fn paragraph(&mut self, inlines: &[Inline], x: f32, width: f32) {
        for line in wrap(inlines, width, 1.0, Role::Body) {
            self.push(LINE, x, width, Deco::None, Body::Runs(offset(line, x)));
        }
    }

    fn code(&mut self, lang: Option<&str>, lines: &[String], x: f32, width: f32) {
        let inner_w = (width - 2.0 * CODE_PAD_X).max(4.0);
        let text_x = x + CODE_PAD_X;
        let emph = Emphasis {
            code: true,
            ..Default::default()
        };

        // Wrap first, so the plate knows how many rows it spans before
        // any of them are pushed and the corner rounding lands on the
        // real ends of the run.
        let mut wrapped: Vec<Run> = Vec::new();
        for source in lines {
            for (i, piece) in split_to_width(source, inner_w, CODE_HANG)
                .into_iter()
                .enumerate()
            {
                let dx = if i == 0 { 0.0 } else { CODE_HANG };
                wrapped.push(Run::new(text_x + dx, piece, emph, 1.0, Role::Code));
            }
        }

        self.push(
            CODE_PAD_Y,
            x,
            width,
            Deco::Plate {
                first: true,
                last: false,
            },
            Body::Runs(Vec::new()),
        );
        for (i, run) in wrapped.into_iter().enumerate() {
            let mut runs = vec![run];
            // The language rides on the first row, right-aligned inside
            // the plate — a caption, not a line of the program.
            if i == 0
                && let Some(lang) = lang.filter(|l| !l.is_empty())
            {
                let mut tag = Run::new(
                    x + width - CODE_PAD_X,
                    lang.to_string(),
                    Emphasis::default(),
                    0.8,
                    Role::Dim,
                );
                tag.align = VgeAlign::Right;
                runs.push(tag);
            }
            self.push(
                CODE_LINE,
                x,
                width,
                Deco::Plate {
                    first: false,
                    last: false,
                },
                Body::Runs(runs),
            );
        }
        self.push(
            CODE_PAD_Y,
            x,
            width,
            Deco::Plate {
                first: false,
                last: true,
            },
            Body::Runs(Vec::new()),
        );
    }

    fn quote(&mut self, alert: Option<Alert>, blocks: &[Block], x: f32, width: f32) {
        self.quotes.push(QuoteBar { x, alert });
        let inner_x = x + QUOTE_INDENT;
        let inner_w = (width - QUOTE_INDENT).max(6.0);
        if let Some(alert) = alert {
            let role = if alert.is_warning() {
                Role::Warn
            } else {
                Role::Accent
            };
            let run = Run::new(
                inner_x,
                alert.label().to_string(),
                Emphasis {
                    bold: true,
                    ..Default::default()
                },
                0.9,
                role,
            );
            self.push(LINE, inner_x, inner_w, Deco::None, Body::Runs(vec![run]));
            self.y += 0.15;
        }
        self.blocks(blocks, inner_x, inner_w);
        self.quotes.pop();
    }

    fn list(&mut self, start: Option<u64>, items: &[Item], x: f32, width: f32) {
        // Every marker in one list shares a column, so `1.` and `10.`
        // don't stagger the text beside them.
        let marker_w = match start {
            Some(first) => {
                let last = first.saturating_add(items.len().saturating_sub(1) as u64);
                text_cells(&format!("{last}.")) + 1.0
            }
            None => BULLET_W,
        };
        let inner_x = x + marker_w;
        let inner_w = (width - marker_w).max(6.0);
        let depth = self.depth;
        self.depth += 1;

        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.y += ITEM_GAP;
            }
            let text = match (item.task, start) {
                (Some(done), _) => if done { "☑" } else { "☐" }.to_string(),
                (None, Some(first)) => format!("{}.", first.saturating_add(i as u64)),
                (None, None) => BULLETS[depth.min(BULLETS.len() - 1)].to_string(),
            };
            let role = match item.task {
                Some(true) => Role::Accent,
                Some(false) => Role::Dim,
                None => Role::Marker,
            };
            let marker = Run::new(x, text, Emphasis::default(), 1.0, role);
            self.with_marker(marker, x, marker_w, |ctx| {
                ctx.blocks(&item.blocks, inner_x, inner_w)
            });
        }
        self.depth = depth;
    }

    /// Lay out `body`, then put `marker` on the first row it produced —
    /// as a row of its own sharing that row's `top`, rather than by
    /// splicing a run into it. A list item can open with something that
    /// holds no runs at all (a picture, a nested table), and the bullet
    /// still belongs beside it.
    fn with_marker(&mut self, marker: Run, x: f32, width: f32, body: impl FnOnce(&mut Self)) {
        let start_row = self.rows.len();
        let before = self.y;
        body(self);
        match self.rows.get(start_row) {
            Some(first) => {
                let (top, height) = (first.top, first.height);
                let quotes = self.quotes.clone();
                self.rows.insert(
                    start_row,
                    Row {
                        top,
                        height,
                        x,
                        width,
                        quotes,
                        deco: Deco::None,
                        body: Body::Runs(vec![marker]),
                    },
                );
            }
            // An item with nothing in it still shows its bullet.
            None => {
                self.y = before;
                self.push(LINE, x, width, Deco::None, Body::Runs(vec![marker]));
            }
        }
    }

    fn footnote(&mut self, label: &str, blocks: &[Block], x: f32, width: f32) {
        let inner_x = x + FOOTNOTE_INDENT;
        let inner_w = (width - FOOTNOTE_INDENT).max(6.0);
        let marker = Run::new(
            x,
            format!("[{label}]"),
            Emphasis::default(),
            0.85,
            Role::Marker,
        );
        self.with_marker(marker, x, FOOTNOTE_INDENT, |ctx| {
            ctx.blocks(blocks, inner_x, inner_w)
        });
    }

    fn table(&mut self, t: &Table, x: f32, width: f32) {
        let cols = t
            .head
            .len()
            .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if cols == 0 {
            return;
        }
        let widths = column_widths(t, cols, width);

        if !t.head.is_empty() {
            self.table_row(&t.head, &widths, &t.align, x, width, true, Deco::TableHead);
            self.push(0.3, x, width, Deco::None, Body::Rule { weight: 0.06 });
        }
        for (i, row) in t.rows.iter().enumerate() {
            let deco = if i % 2 == 1 {
                Deco::TableBand
            } else {
                Deco::None
            };
            self.table_row(row, &widths, &t.align, x, width, false, deco);
        }
    }

    /// One table row, as one [`Row`] per *visual* line.
    ///
    /// A cell that doesn't fit its column wraps rather than being cut,
    /// so a row is as tall as its tallest cell and the shorter ones
    /// top-align beside it. The band (`deco`) repeats on every line;
    /// the fills are adjacent and identical, so a three-line row reads
    /// as one band rather than three stripes.
    #[allow(clippy::too_many_arguments)]
    fn table_row(
        &mut self,
        cells: &[Vec<Inline>],
        widths: &[f32],
        align: &[Align],
        x: f32,
        width: f32,
        header: bool,
        deco: Deco,
    ) {
        let mut columns: Vec<Vec<Vec<Run>>> = Vec::with_capacity(widths.len());
        let mut cx = x;
        for (i, w) in widths.iter().enumerate() {
            let inner = (w - 2.0 * CELL_PAD).max(1.0);
            let a = align.get(i).copied().unwrap_or(Align::Left);
            let mut lines = match cells.get(i) {
                Some(cell) if has_text(cell) => wrap(cell, inner, 1.0, Role::Body),
                _ => Vec::new(),
            };
            // A cell ending in a `<br>` leaves a trailing empty line,
            // which would make the whole row a line taller for nothing.
            while lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
            columns.push(
                lines
                    .into_iter()
                    .map(|mut line| {
                        if header {
                            for run in &mut line {
                                run.emph.bold = true;
                            }
                        }
                        place_line(line, cx, *w, a)
                    })
                    .collect(),
            );
            cx += w;
        }

        let height = columns.iter().map(|c| c.len()).max().unwrap_or(0).max(1);
        for line in 0..height {
            let mut runs = Vec::new();
            for column in &mut columns {
                if let Some(cell_line) = column.get_mut(line) {
                    runs.append(cell_line);
                }
            }
            self.push(TABLE_LINE, x, width, deco, Body::Runs(runs));
        }
    }

    fn image(&mut self, img: &crate::doc::ImageRef, x: f32, width: f32) {
        let size = img.path.as_deref().and_then(|p| self.sizes.size(p));
        let Some((pw, ph)) = size.filter(|(w, h)| *w > 0 && *h > 0) else {
            // Nothing to draw: a labelled placeholder keeps the
            // document's shape and says which picture is missing.
            let alt = format!("🖼  {}", img.alt);
            let run = Run::new(x + 1.0, alt, Emphasis::default(), 1.0, Role::Dim);
            self.push(
                IMAGE_STUB_H,
                x,
                width,
                Deco::Plate {
                    first: true,
                    last: true,
                },
                Body::Runs(vec![run]),
            );
            return;
        };

        // Natural size in em: pixels over the pixel size of one cell.
        let m = self.metrics;
        let mut w = pw as f32 / m.cell_pw.max(1.0);
        let mut h = ph as f32 / m.cell_ph.max(1.0);
        // Never upscale — a 16px icon should stay an icon.
        let fit = (width / w).min(1.0);
        w *= fit;
        h *= fit;
        if h > m.max_image_h {
            let k = m.max_image_h / h;
            w *= k;
            h *= k;
        }
        self.push(
            h,
            x,
            width,
            Deco::None,
            Body::Image(ImageBox {
                path: img.path.clone(),
                alt: img.alt.clone(),
                w,
            }),
        );
    }
}

/// Space above a block when it isn't the first in its container.
fn gap_before(block: &Block) -> f32 {
    match block {
        Block::Heading { .. } => HEAD_GAP_BEFORE,
        _ => 0.0,
    }
}

/// Space below a block when something follows it.
fn gap_after(block: &Block) -> f32 {
    match block {
        Block::Heading { .. } => HEAD_GAP_AFTER,
        Block::Rule => 0.0,
        _ => PARA_GAP,
    }
}

/// Shift a wrapped line, whose runs are laid out from zero, into the
/// content box at `x`.
fn offset(mut runs: Vec<Run>, x: f32) -> Vec<Run> {
    for run in &mut runs {
        run.x += x;
    }
    runs
}

// ─────────────────────────────────────────────────────────────────────
// Tables
// ─────────────────────────────────────────────────────────────────────

/// Column widths in em, by the same two-number rule CSS's automatic
/// table layout uses.
///
/// Each column has a *want* — the width at which nothing wraps — and a
/// *need*, the width of its longest single word, below which a break
/// would have to land mid-word. Given room for every want, that is what
/// each column gets. Otherwise every column gets its need and the slack
/// is shared out in proportion to what each still wanted on top of it,
/// so one prose column absorbs the squeeze instead of every column
/// giving up the same fraction.
fn column_widths(t: &Table, cols: usize, width: f32) -> Vec<f32> {
    let pad = 2.0 * CELL_PAD;
    let mut want = vec![0.0f32; cols];
    let mut need = vec![0.0f32; cols];
    for cells in std::iter::once(&t.head).chain(t.rows.iter()) {
        for (i, cell) in cells.iter().enumerate().take(cols) {
            let text = crate::doc::plain_text(cell);
            want[i] = want[i].max(text_cells(&text));
            need[i] = need[i].max(longest_word(&text));
        }
    }
    let want: Vec<f32> = want.iter().map(|w| (w + pad).max(MIN_COL)).collect();
    // One outsized token — a URL, a hash — must not be allowed to claim
    // the table on its own; past the cap it is char-split like any
    // other overlong word (see `Wrapper::token`).
    let cap = (width * 0.4).max(MIN_COL);
    let need: Vec<f32> = need
        .iter()
        .zip(&want)
        .map(|(n, w)| (n + pad).max(MIN_COL).min(cap).min(*w))
        .collect();

    let total_want: f32 = want.iter().sum();
    if total_want <= width {
        return want;
    }
    let total_need: f32 = need.iter().sum();
    if total_need >= width {
        // Not even the longest words fit side by side; share the page
        // out in proportion and let the wrapper split what it must.
        let k = width / total_need.max(f32::EPSILON);
        return need.iter().map(|n| n * k).collect();
    }
    let k = (width - total_need) / (total_want - total_need);
    need.iter()
        .zip(&want)
        .map(|(n, w)| n + (w - n) * k)
        .collect()
}

/// Width of the widest whitespace-separated token in `text`.
fn longest_word(text: &str) -> f32 {
    text.split_whitespace()
        .map(text_cells)
        .fold(0.0f32, f32::max)
}

/// True if a cell holds anything but whitespace.
fn has_text(cell: &[Inline]) -> bool {
    cell.iter()
        .any(|i| matches!(i, Inline::Run(r) if !r.text.trim().is_empty()))
}

/// Position one wrapped line inside the column at `cx`, `w` wide.
///
/// A line that came out as a single run keeps a real VGE `Align`
/// anchored to the column edge, so the host's own glyph advances place
/// it rather than this crate's cell estimate — which is the common
/// case, since same-styled tokens merge as they wrap. A line of several
/// runs cannot be anchored by one `Align`, so it is offset by the
/// measured line width instead.
fn place_line(mut line: Vec<Run>, cx: f32, w: f32, align: Align) -> Vec<Run> {
    if line.is_empty() {
        return line;
    }
    let line_w = line.iter().map(|r| r.x + r.width()).fold(0.0f32, f32::max);
    let shift = |line: &mut Vec<Run>, dx: f32| {
        for run in line {
            run.x += dx;
        }
    };
    match align {
        Align::Left => shift(&mut line, cx + CELL_PAD),
        Align::Center => match line.as_mut_slice() {
            [only] => {
                only.x = cx + w * 0.5;
                only.align = VgeAlign::Center;
            }
            _ => shift(&mut line, cx + (w - line_w) * 0.5),
        },
        Align::Right => match line.as_mut_slice() {
            [only] => {
                only.x = cx + w - CELL_PAD;
                only.align = VgeAlign::Right;
            }
            _ => shift(&mut line, cx + w - CELL_PAD - line_w),
        },
    }
    line
}

// ─────────────────────────────────────────────────────────────────────
// Wrapping
// ─────────────────────────────────────────────────────────────────────

/// Greedy word wrap over styled inlines, at `width` em with every run
/// drawn at `scale`.
///
/// Returns lines whose runs are positioned from x = 0. Adjacent tokens
/// that share a style are merged into one run, so a wrapped paragraph
/// costs about one `DrawText` per line rather than one per word.
fn wrap(inlines: &[Inline], width: f32, scale: f32, base: Role) -> Vec<Vec<Run>> {
    let mut w = Wrapper {
        lines: Vec::new(),
        cur: Vec::new(),
        x: 0.0,
        width: width.max(1.0),
        scale,
        space: false,
    };
    for inline in inlines {
        match inline {
            Inline::Break => w.hard_break(),
            Inline::Run(run) => {
                let role = if run.emph.code {
                    Role::Code
                } else if run.link.is_some() {
                    Role::Link
                } else {
                    base
                };
                w.text(&run.text, run.emph, role, run.link);
            }
        }
    }
    w.finish()
}

struct Wrapper {
    lines: Vec<Vec<Run>>,
    cur: Vec<Run>,
    x: f32,
    width: f32,
    scale: f32,
    /// Whitespace seen since the last token, owed to the next one.
    space: bool,
}

impl Wrapper {
    fn newline(&mut self) {
        self.lines.push(std::mem::take(&mut self.cur));
        self.x = 0.0;
        self.space = false;
    }

    fn hard_break(&mut self) {
        self.newline();
    }

    fn text(&mut self, text: &str, emph: Emphasis, role: Role, link: Option<usize>) {
        let mut token = String::new();
        for c in text.chars() {
            if c.is_whitespace() {
                if !token.is_empty() {
                    self.token(std::mem::take(&mut token), emph, role, link);
                }
                self.space = true;
            } else {
                token.push(c);
            }
        }
        if !token.is_empty() {
            self.token(token, emph, role, link);
        }
    }

    fn token(&mut self, token: String, emph: Emphasis, role: Role, link: Option<usize>) {
        let w = text_cells(&token) * self.scale;
        // A token that cannot fit a line of its own — a long URL, or a
        // run of CJK, which has no spaces to break at — is cut by
        // character instead of overflowing the page.
        if w > self.width {
            for piece in split_to_width(&token, self.width / self.scale, 0.0) {
                if self.x > 0.0 {
                    self.newline();
                }
                self.emit(piece, emph, role, link, false);
            }
            return;
        }
        let space = if self.x > 0.0 && self.space {
            self.scale
        } else {
            0.0
        };
        if self.x > 0.0 && self.x + space + w > self.width + 1e-3 {
            self.newline();
            self.emit(token, emph, role, link, false);
        } else {
            self.emit(token, emph, role, link, space > 0.0);
        }
    }

    /// Place a token at the caret, merging into the previous run when
    /// nothing about its styling changed.
    fn emit(
        &mut self,
        token: String,
        emph: Emphasis,
        role: Role,
        link: Option<usize>,
        space: bool,
    ) {
        let w = text_cells(&token) * self.scale;
        let advance = if space { self.scale } else { 0.0 };
        let x = self.x + advance;
        let merged = match self.cur.last_mut() {
            Some(last) if last.emph == emph && last.role == role && last.link == link => {
                if space {
                    last.text.push(' ');
                }
                last.text.push_str(&token);
                true
            }
            _ => false,
        };
        if !merged {
            let mut run = Run::new(x, token, emph, self.scale, role);
            run.link = link;
            self.cur.push(run);
        }
        self.x = x + w;
        self.space = false;
    }

    fn finish(mut self) -> Vec<Vec<Run>> {
        if !self.cur.is_empty() || self.lines.is_empty() {
            self.lines.push(std::mem::take(&mut self.cur));
        }
        self.lines
    }
}

/// Split `text` into pieces no wider than `width` cells, where every
/// piece after the first is indented by `hang` (and so gets that much
/// less room). Never returns an empty vector.
fn split_to_width(text: &str, width: f32, hang: f32) -> Vec<String> {
    let first = width.max(1.0);
    if text_cells(text) <= first {
        return vec![text.to_string()];
    }
    let rest = (width - hang).max(1.0);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut used = 0.0f32;
    let mut budget = first;
    for c in text.chars() {
        let cw = text_cells(&c.to_string());
        if used + cw > budget && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            used = 0.0;
            budget = rest;
        }
        cur.push(c);
        used += cw;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Run as DocRun, parse};
    use std::path::Path;

    struct NoImages;
    impl ImageSizes for NoImages {
        fn size(&self, _: &std::path::Path) -> Option<(u32, u32)> {
            None
        }
    }

    struct FixedImages(u32, u32);
    impl ImageSizes for FixedImages {
        fn size(&self, _: &std::path::Path) -> Option<(u32, u32)> {
            Some((self.0, self.1))
        }
    }

    const M: Metrics = Metrics {
        cell_pw: 10.0,
        cell_ph: 20.0,
        max_image_h: 20.0,
    };

    fn lay(text: &str, width: f32) -> Layout {
        lay_out(&parse(text, Path::new(".")), width, M, &NoImages)
    }

    fn line_text(row: &Row) -> String {
        row.runs()
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("|")
    }

    fn runs(text: &str) -> Vec<Inline> {
        vec![Inline::Run(DocRun {
            text: text.to_string(),
            emph: Emphasis::default(),
            link: None,
        })]
    }

    #[test]
    fn wrapping_respects_the_width_and_merges_runs() {
        let lines = wrap(&runs("one two three four five"), 10.0, 1.0, Role::Body);
        assert!(lines.len() > 1);
        for line in &lines {
            // One run per line, because every token shares a style.
            assert_eq!(line.len(), 1, "{line:?}");
            let run = &line[0];
            assert!(run.width() <= 10.0 + 1e-3, "{run:?}");
        }
    }

    #[test]
    fn a_token_wider_than_the_line_is_cut_rather_than_overflowing() {
        let lines = wrap(&runs("aaaaaaaaaaaaaaaaaaaaaaaa"), 8.0, 1.0, Role::Body);
        assert!(lines.len() >= 3, "{lines:?}");
        for line in &lines {
            assert!(line[0].width() <= 8.0 + 1e-3, "{line:?}");
        }
    }

    #[test]
    fn a_bigger_scale_fits_fewer_words_on_a_line() {
        let small = wrap(&runs("one two three four five six"), 20.0, 1.0, Role::Body);
        let big = wrap(&runs("one two three four five six"), 20.0, 2.0, Role::Body);
        assert!(big.len() > small.len(), "{} vs {}", big.len(), small.len());
        for line in &big {
            assert!(line[0].width() <= 20.0 + 1e-3, "{line:?}");
        }
    }

    #[test]
    fn a_hard_break_starts_a_line_even_mid_paragraph() {
        let mut inlines = runs("one");
        inlines.push(Inline::Break);
        inlines.extend(runs("two"));
        let lines = wrap(&inlines, 40.0, 1.0, Role::Body);
        assert_eq!(lines.len(), 2, "{lines:?}");
    }

    #[test]
    fn headings_scale_up_and_land_in_the_outline() {
        let l = lay("# Big\n\ntext\n\n## Small\n", 40.0);
        assert_eq!(l.outline.len(), 2);
        assert_eq!(l.outline[0].text, "Big");
        assert_eq!(l.outline[0].slug, "big");
        assert!(l.outline[0].top < l.outline[1].top);
        let h1 = l.rows.iter().find(|r| !r.runs().is_empty()).unwrap();
        assert_eq!(h1.runs()[0].scale, heading_scale(1));
    }

    #[test]
    fn heading_at_reports_the_section_a_position_is_inside() {
        let l = lay("# One\n\nbody\n\n# Two\n\nbody\n", 40.0);
        let second = l.outline[1].top;
        assert_eq!(l.heading_at(0.0).map(|a| a.text.as_str()), Some("One"));
        assert_eq!(
            l.heading_at(second + 1.0).map(|a| a.text.as_str()),
            Some("Two")
        );
        assert_eq!(l.anchor("two"), Some(second));
        assert_eq!(l.anchor("nope"), None);
    }

    #[test]
    fn a_list_marker_shares_the_top_of_the_row_it_labels() {
        let l = lay("- alpha\n- beta\n", 40.0);
        let markers: Vec<usize> = l
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.runs().first().is_some_and(|run| run.role == Role::Marker))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(markers.len(), 2, "{:?}", l.rows);
        for i in markers {
            let marker = &l.rows[i];
            let labelled = &l.rows[i + 1];
            assert_eq!(line_text(marker), "•");
            // Same line, and the bullet sits left of the text.
            assert!((marker.top - labelled.top).abs() < 1e-3);
            assert!(marker.runs()[0].x < labelled.runs()[0].x);
        }
        assert_eq!(line_text(&l.rows[1]), "alpha");
        assert_eq!(line_text(&l.rows[3]), "beta");
    }

    #[test]
    fn ordered_markers_share_a_column_wide_enough_for_the_last_one() {
        let items: String = (1..=10).map(|i| format!("{i}. item\n")).collect();
        let l = lay(&items, 40.0);
        let (markers, text): (Vec<&Row>, Vec<&Row>) = l
            .rows
            .iter()
            .partition(|r| r.runs().first().is_some_and(|run| run.role == Role::Marker));
        assert_eq!(markers.len(), 10);
        assert_eq!(text.len(), 10);
        // Both columns are flush: `1.` and `10.` do not stagger the
        // text beside them.
        let xs: Vec<f32> = text.iter().map(|r| r.runs()[0].x).collect();
        assert!(xs.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-3), "{xs:?}");
        assert_eq!(markers[9].runs()[0].text, "10.");
        assert!(markers[0].runs()[0].x < xs[0]);
    }

    #[test]
    fn a_nested_list_indents_past_its_parent() {
        let l = lay("- outer\n  - inner\n", 40.0);
        let markers: Vec<&Run> = l
            .rows
            .iter()
            .filter_map(|r| r.runs().first())
            .filter(|r| r.role == Role::Marker)
            .collect();
        assert_eq!(markers.len(), 2, "{:?}", l.rows);
        assert!(markers[1].x > markers[0].x, "{markers:?}");
        // Depth picks a different bullet glyph.
        assert_ne!(markers[0].text, markers[1].text);
    }

    #[test]
    fn a_code_block_is_bracketed_by_its_plate() {
        let l = lay("```rust\nfn main() {}\n```\n", 40.0);
        let plate: Vec<&Row> = l
            .rows
            .iter()
            .filter(|r| matches!(r.deco, Deco::Plate { .. }))
            .collect();
        assert!(plate.len() >= 3, "{:?}", l.rows);
        assert_eq!(
            plate[0].deco,
            Deco::Plate {
                first: true,
                last: false
            }
        );
        assert_eq!(
            plate[plate.len() - 1].deco,
            Deco::Plate {
                first: false,
                last: true
            }
        );
        // The language caption is right-anchored inside the plate.
        let tag = plate
            .iter()
            .flat_map(|r| r.runs())
            .find(|r| r.text == "rust")
            .expect("language caption");
        assert_eq!(tag.align, VgeAlign::Right);
    }

    #[test]
    fn a_long_code_line_wraps_with_a_hanging_indent() {
        let long = "x".repeat(200);
        let l = lay(&format!("```\n{long}\n```\n"), 40.0);
        let code: Vec<&Run> = l
            .rows
            .iter()
            .flat_map(|r| r.runs())
            .filter(|r| r.role == Role::Code)
            .collect();
        assert!(code.len() > 1, "{}", code.len());
        assert!(code[1].x > code[0].x, "continuation should hang");
        assert!(code.iter().all(|r| r.width() <= 40.0));
    }

    #[test]
    fn quotes_carry_a_bar_and_indent_their_contents() {
        let l = lay("> quoted\n", 40.0);
        let row = l.rows.iter().find(|r| !r.runs().is_empty()).unwrap();
        assert_eq!(row.quotes.len(), 1);
        assert!(row.runs()[0].x > row.quotes[0].x);
    }

    #[test]
    fn an_alert_labels_its_quote_and_colours_the_bar() {
        let l = lay("> [!WARNING]\n> careful\n", 40.0);
        let label = l
            .rows
            .iter()
            .flat_map(|r| r.runs())
            .find(|r| r.text == "WARNING")
            .expect("alert label");
        assert_eq!(label.role, Role::Warn);
        assert_eq!(
            l.rows.iter().find(|r| !r.quotes.is_empty()).unwrap().quotes[0].alert,
            Some(Alert::Warning)
        );
    }

    #[test]
    fn table_columns_fit_the_page_and_keep_their_alignment() {
        let md = "| left | right |\n|:--|--:|\n| a | b |\n";
        let l = lay(md, 40.0);
        let head = l
            .rows
            .iter()
            .find(|r| r.deco == Deco::TableHead)
            .expect("header row");
        assert_eq!(head.runs()[0].align, VgeAlign::Left);
        assert_eq!(head.runs()[1].align, VgeAlign::Right);
        // Body rows alternate a band so a wide table stays readable.
        assert!(l.rows.iter().any(|r| r.deco == Deco::TableBand) || l.rows.len() < 6);
    }

    /// Text of every row carrying `deco` that has anything on it, in
    /// order — one string per visual line of a table row. Run-less
    /// rows are skipped so the rule under a table header doesn't count
    /// as a line of the body.
    fn banded_lines(l: &Layout, deco: Deco) -> Vec<String> {
        l.rows
            .iter()
            .filter(|r| r.deco == deco && !r.runs().is_empty())
            .map(|r| {
                r.runs()
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    #[test]
    fn a_long_cell_wraps_across_lines_instead_of_being_cut() {
        let md =
            "| tag | note |\n|---|---|\n| a | one two three four five six seven eight nine ten |\n";
        let l = lay(md, 30.0);
        let body = banded_lines(&l, Deco::None);
        // Several lines, and between them the whole sentence — nothing
        // was dropped and no ellipsis was introduced.
        assert!(body.len() > 1, "{body:?}");
        let joined = body.join(" ");
        assert!(
            !joined.contains('…'),
            "a wrapped cell must not be elided: {joined:?}"
        );
        for word in ["one", "five", "ten"] {
            assert!(joined.contains(word), "{word:?} missing from {joined:?}");
        }
    }

    #[test]
    fn a_wrapped_row_is_as_tall_as_its_tallest_cell_and_the_rest_top_align() {
        let md = "| tag | note |\n|---|---|\n| a | one two three four five six seven eight |\n";
        let l = lay(md, 26.0);
        let body: Vec<&Row> = l
            .rows
            .iter()
            .filter(|r| r.deco == Deco::None && !r.runs().is_empty())
            .collect();
        assert!(body.len() > 1, "the long cell should wrap");
        // The short cell is drawn once, on the row's first line.
        assert!(body[0].runs().iter().any(|r| r.text == "a"));
        for line in &body[1..] {
            assert!(
                !line.runs().iter().any(|r| r.text == "a"),
                "the short cell must not repeat down the row"
            );
        }
        // Every line of the row carries the same band, so it reads as
        // one row rather than several.
        assert!(body.iter().all(|r| r.deco == Deco::None));
    }

    #[test]
    fn a_squeeze_falls_on_the_prose_column_not_the_narrow_one() {
        let md = "| id | prose |\n|---|---|\n| 7 | a long stretch of words that has to give way somewhere |\n";
        let l = lay(md, 30.0);
        let head = l.rows.iter().find(|r| r.deco == Deco::TableHead).unwrap();
        let (id, prose) = (&head.runs()[0], &head.runs()[1]);
        // The `id` column keeps roughly what it asked for; the prose
        // column absorbs the squeeze. Column starts are what the run
        // x values report.
        assert!(
            prose.x - id.x < 10.0,
            "id column took {} em",
            prose.x - id.x
        );
    }

    #[test]
    fn a_column_is_never_squeezed_below_its_longest_word_while_there_is_room() {
        let md = "| a | b |\n|---|---|\n| indivisible | short words here that can wrap freely |\n";
        let l = lay(md, 40.0);
        let cell = l
            .rows
            .iter()
            .flat_map(|r| r.runs())
            .find(|r| r.text == "indivisible")
            .expect("the long word should survive whole");
        assert!(cell.width() <= 40.0);
    }

    #[test]
    fn emphasis_and_links_inside_a_cell_survive_the_wrap() {
        let md = "| what | which |\n|---|---|\n| **bold** and `code` | [a link](https://example.com) |\n";
        let l = lay(md, 60.0);
        let runs: Vec<&Run> = l.rows.iter().flat_map(|r| r.runs()).collect();
        assert!(
            runs.iter().any(|r| r.text == "bold" && r.emph.bold),
            "{runs:?}"
        );
        assert!(
            runs.iter()
                .any(|r| r.text == "code" && r.role == Role::Code),
            "{runs:?}"
        );
        let link = runs
            .iter()
            .find(|r| r.role == Role::Link)
            .expect("the cell's link");
        assert_eq!(link.link, Some(0));
        assert_eq!(link.text, "a link");
    }

    #[test]
    fn a_right_aligned_cell_that_wraps_stays_inside_its_column() {
        let md = "| a | b |\n|---|--:|\n| x | several words that will not fit on one line |\n";
        let l = lay(md, 26.0);
        for row in l.rows.iter().filter(|r| !r.runs().is_empty()) {
            for run in row.runs() {
                let right = match run.align {
                    VgeAlign::Right => run.x,
                    VgeAlign::Center => run.x + run.width() * 0.5,
                    VgeAlign::Left => run.x + run.width(),
                };
                assert!(right <= 26.0 + 1e-3, "{run:?} runs past the page");
                assert!(run.x >= -1e-3, "{run:?} starts left of the page");
            }
        }
    }

    #[test]
    fn a_squeezed_table_still_fits_the_page() {
        let wide = "| ".to_string()
            + &(0..6)
                .map(|i| format!("column-header-{i} "))
                .collect::<Vec<_>>()
                .join("| ")
            + "|\n|"
            + &"---|".repeat(6)
            + "\n";
        let l = lay(&wide, 40.0);
        let head = l.rows.iter().find(|r| r.deco == Deco::TableHead).unwrap();
        let right = head
            .runs()
            .iter()
            .map(|r| r.x + r.width())
            .fold(0.0f32, f32::max);
        assert!(right <= 40.0 + 1e-3, "{right}");
    }

    #[test]
    fn an_unreadable_picture_becomes_a_labelled_placeholder() {
        let l = lay("![a diagram](missing.png)\n", 40.0);
        let stub = l
            .rows
            .iter()
            .find(|r| r.runs().iter().any(|run| run.text.contains("a diagram")))
            .expect("placeholder row");
        assert!(stub.height >= IMAGE_STUB_H - 1e-3);
    }

    /// The one `Body::Image` a single-picture document lays out, with
    /// the row height that goes with it.
    fn only_picture(l: &Layout) -> (ImageBox, f32) {
        l.rows
            .iter()
            .find_map(|r| match &r.body {
                Body::Image(i) => Some((i.clone(), r.height)),
                _ => None,
            })
            .expect("an image row")
    }

    #[test]
    fn a_picture_keeps_its_aspect_ratio_under_the_cell_aspect() {
        // 200×200 px on 10×20 px cells is 20 em wide and 10 em tall —
        // square on screen, not square in cells.
        let doc = parse("![x](Cargo.toml)", Path::new("."));
        let (img, h) = only_picture(&lay_out(&doc, 40.0, M, &FixedImages(200, 200)));
        assert!((img.w - 20.0).abs() < 1e-3, "{}", img.w);
        assert!((h - 10.0).abs() < 1e-3, "{h}");
    }

    #[test]
    fn a_wide_picture_is_scaled_down_to_the_page_but_never_up() {
        let doc = parse("![x](Cargo.toml)", Path::new("."));
        let (wide, _) = only_picture(&lay_out(&doc, 10.0, M, &FixedImages(400, 200)));
        assert!((wide.w - 10.0).abs() < 1e-3, "{}", wide.w);
        // 50 px across 10 px cells is 5 em, and a small picture stays
        // small rather than being blown up to the measure.
        let (small, _) = only_picture(&lay_out(&doc, 40.0, M, &FixedImages(50, 40)));
        assert!((small.w - 5.0).abs() < 1e-3, "{}", small.w);
    }

    #[test]
    fn a_tall_picture_is_capped_by_max_image_h() {
        let doc = parse("![x](Cargo.toml)", Path::new("."));
        let (img, h) = only_picture(&lay_out(&doc, 40.0, M, &FixedImages(100, 4000)));
        assert!(h <= M.max_image_h + 1e-3, "{h}");
        // Capping the height narrows it too — the ratio is preserved.
        assert!(img.w < 10.0, "{}", img.w);
    }

    #[test]
    fn rows_are_ordered_and_the_height_is_their_extent() {
        let l = lay("# Title\n\nbody text\n\n- a\n- b\n\n```\ncode\n```\n", 40.0);
        let mut prev = -1.0f32;
        for row in &l.rows {
            assert!(row.top >= prev - 1e-3, "{:?}", row);
            prev = row.top;
        }
        let last = l.rows.last().unwrap();
        assert!(l.height >= last.top + last.height - 1e-3);
    }

    #[test]
    fn a_narrow_page_does_not_produce_negative_widths() {
        let l = lay("> - deeply\n>   - nested\n>     - list\n", 4.0);
        for row in &l.rows {
            assert!(row.width > 0.0, "{row:?}");
        }
    }
}
