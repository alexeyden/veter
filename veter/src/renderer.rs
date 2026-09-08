use std::collections::HashMap;

use femtovg::{
    Atlas, Canvas, Color, DrawCommand, GlyphDrawCommands, ImageFlags, ImageSource, Paint, Path,
    Quad, Renderer, Solidity,
};

use crate::prt;
use crate::theme::{Rgba, Theme};
use crate::vge;
use imgref::{Img, ImgRef};
use parley::{
    layout::{Alignment, Layout, PositionedLayoutItem},
    style::{FontFamily, FontStack, GenericFamily, StyleProperty},
    AlignmentOptions, FontContext, LayoutContext,
};
use rgb::RGBA8;
use std::borrow::Cow;
use swash::{
    scale::{image::Content, Render, ScaleContext, Source, StrikeWith},
    zeno::Format,
    FontRef, StringId,
};

const TEXTURE_SIZE: usize = 512;

/// Transparent padding reserved to the right of and below every glyph
/// in the atlas. Glyph quads map 1:1 to texels and sample NEAREST, so a
/// quad edge landing exactly on a half-pixel puts the interpolated
/// texel coordinate exactly on a texel boundary, where float error can
/// floor it one texel outside the glyph's own box. Without padding that
/// texel is the *adjacent glyph*, which then bleeds in as a 1px sliver
/// of an unrelated letter. The gutter makes the out-of-range texel
/// transparent instead. The atlas is created fully transparent and
/// `update_image` only ever writes the glyph's own w*h box, so the
/// reserved gutter stays transparent for the atlas's lifetime.
///
/// A single trailing gutter covers the leading edge too: the column
/// left of a glyph is the previous glyph's gutter, and glyphs at
/// `atlas_x == 0` clamp to their own first column (the atlas is not
/// REPEAT-wrapped).
const GLYPH_GUTTER: usize = 1;

/// Selection range expressed in visible-row coords (i.e. as the
/// renderer sees them after the user's current scrollback offset is
/// applied). `start_row` may be negative when the selection extends
/// above the viewport (anchor in scrollback that's now off-screen);
/// `end_row` may exceed `rows` for the same reason at the bottom.
/// Half-open: `[start, end)` in lexicographic (row, col) order.
/// `block_cols`, when `Some`, additionally clips each visible row to
/// the pane's column band so a smart pane select can't bleed across
/// borders.
#[derive(Copy, Clone, Debug)]
pub struct SelectionRange {
    pub start_row: i32,
    pub start_col: u16,
    pub end_row: i32,
    pub end_col: u16,
    pub block_cols: Option<(u16, u16)>,
}

impl SelectionRange {
    fn contains(&self, row: u16, col: u16) -> bool {
        let pos = (row as i32, col);
        let start = (self.start_row, self.start_col);
        let end = (self.end_row, self.end_col);
        if pos < start || pos >= end {
            return false;
        }
        if let Some((left, right)) = self.block_cols
            && (col < left || col > right)
        {
            return false;
        }
        true
    }
}

/// One row's worth of search-match highlight, in the target screen's
/// currently-visible row coords. Multiple spans on the same row are
/// allowed; the renderer paints each as a background fill. The
/// `is_current` flag marks the active match (the one the viewport
/// is scrolled to) so the renderer can give it a stronger color.
#[derive(Copy, Clone, Debug)]
pub struct HighlightSpan {
    pub row: u16,
    pub col_start: u16,
    /// Exclusive end column.
    pub col_end: u16,
    pub is_current: bool,
}

/// Project all matches in `matches` into [`HighlightSpan`]s for the
/// currently-visible viewport of a parser at `top_of_live_screen` /
/// `scrollback`. `current` is the index of the active match; the
/// resulting spans for that match (if visible) have `is_current = true`.
/// Off-screen matches contribute no spans.
///
/// A match that crosses a soft wrap (hint mode detects those; a typed
/// query never produces one) yields one span per row it covers, each
/// clipped to the viewport — so a URL broken by the right margin
/// highlights as the one run of text it visually is.
pub fn search_highlights_for_viewport(
    matches: &[crate::search::MatchSpan],
    current: usize,
    top_of_live_screen: i64,
    scrollback: usize,
    rows: u16,
    cols: u16,
) -> Vec<HighlightSpan> {
    let viewport_top = top_of_live_screen - scrollback as i64;
    let mut out = Vec::new();
    for (i, m) in matches.iter().enumerate() {
        for line in m.line..=m.end_line.max(m.line) {
            let row_i = line - viewport_top;
            if row_i < 0 || row_i >= rows as i64 {
                continue;
            }
            let col_start = if line == m.line { m.col_start } else { 0 };
            let col_end = if line == m.end_line { m.col_end } else { cols };
            if col_end <= col_start {
                continue;
            }
            out.push(HighlightSpan {
                row: row_i as u16,
                col_start,
                col_end,
                is_current: i == current,
            });
        }
    }
    out
}

/// Resolve an absolute-line selection (anchor + head in some vt100's
/// scrollback line coords) into a half-open `SelectionRange` in that
/// vt100's currently-visible row coords. Used by both the host call
/// site and per-portal render to avoid duplicating the math.
/// Returns `None` for empty or fully off-screen selections. When
/// `block_cols` is `Some`, the lex range is the same as without
/// (since the head's column is already clamped to the pane at drag
/// time), but `contains` will additionally clip each row to that band.
#[allow(clippy::too_many_arguments)]
pub fn selection_range_from_abs(
    anchor_line: i64,
    anchor_col: u16,
    head_line: i64,
    head_col: u16,
    block_cols: Option<(u16, u16)>,
    top_of_live_screen: i64,
    scrollback: usize,
    rows: u16,
    cols: u16,
) -> Option<SelectionRange> {
    if (anchor_line, anchor_col) == (head_line, head_col) {
        return None;
    }
    let ((s_line, s_col), (e_line, e_col)) =
        if (anchor_line, anchor_col) <= (head_line, head_col) {
            ((anchor_line, anchor_col), (head_line, head_col))
        } else {
            ((head_line, head_col), (anchor_line, anchor_col))
        };
    let viewport_top = top_of_live_screen - scrollback as i64;
    let s_row = (s_line - viewport_top) as i32;
    let mut e_row = (e_line - viewport_top) as i32;
    let mut e_col_open = e_col.saturating_add(1);
    if e_col_open > cols {
        e_row += 1;
        e_col_open = 0;
    }
    if e_row < 0 || s_row >= rows as i32 {
        return None;
    }
    Some(SelectionRange {
        start_row: s_row,
        start_col: s_col,
        end_row: e_row,
        end_col: e_col_open,
        block_cols,
    })
}

/// Terminal default background, in the built-in theme. The window's
/// ground before a renderer exists, and the "selected" foreground for
/// VGE text (`draw_vge_text_selected`), which reverse-videos against
/// the text's own colour the way a selected cell does.
///
/// A themed veter reads [`Palette::bg`] instead; this is the value a
/// `[theme]`-free config resolves to, kept as a `const` for the paths
/// that have no palette to ask.
pub const DEFAULT_BG: Color = Color {
    r: 30.0 / 255.0,
    g: 30.0 / 255.0,
    b: 30.0 / 255.0,
    a: 1.0,
};

/// The colour table a program can rewrite from inside the terminal:
/// the 256 indexed entries (`OSC 4`, terminfo `initc`) plus the three
/// "dynamic" colours (`OSC 10 / 11 / 12`, terminfo `Cs`). Every entry
/// is an override over the built-in value, so `OSC 104` / `OSC 110`
/// and friends reset by clearing rather than by remembering a copy of
/// the defaults.
/// The theme colours a [`Palette`] resolves against, flattened out of
/// [`Theme`] so the whole palette stays plain `Copy` data — the render
/// pass clones it once per frame, and a `Theme` in there would put a
/// heap allocation on that path.
#[derive(Clone, Copy)]
struct PaletteBase {
    ansi: [Color; 16],
    fg: Color,
    bg: Color,
    cursor: Option<Color>,
    selection_bg: Option<Color>,
    selection_fg: Option<Color>,
}

impl PaletteBase {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            ansi: theme.ansi.map(Rgba::to_femto),
            fg: theme.foreground.to_femto(),
            bg: theme.background.to_femto(),
            cursor: theme.cursor.map(Rgba::to_femto),
            selection_bg: theme.selection_bg.map(Rgba::to_femto),
            selection_fg: theme.selection_fg.map(Rgba::to_femto),
        }
    }

    fn indexed(&self, i: u8) -> Color {
        match i {
            0..=15 => self.ansi[i as usize],
            _ => crate::theme::indexed_extended(i).to_femto(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    /// The user's theme, as the base every override below sits on and
    /// what a reset falls back to.
    base: PaletteBase,
    indexed: [Option<Color>; 256],
    default_fg: Option<Color>,
    default_bg: Option<Color>,
    cursor: Option<Color>,
}

impl Default for Palette {
    fn default() -> Self {
        Self::with_theme(&Theme::default())
    }
}

impl Palette {
    /// A palette over `theme`. The theme is the *base*: an `OSC 4 / 10
    /// / 11 / 12` from inside the terminal overrides an entry, and
    /// `OSC 104 / 110 / 111 / 112` drops the override — putting the
    /// theme's colour back, not a hardcoded one.
    #[must_use]
    pub fn with_theme(theme: &Theme) -> Self {
        Self {
            base: PaletteBase::from_theme(theme),
            indexed: [None; 256],
            default_fg: None,
            default_bg: None,
            cursor: None,
        }
    }

    #[must_use]
    pub fn indexed(&self, i: u8) -> Color {
        self.indexed[i as usize].unwrap_or_else(|| self.base.indexed(i))
    }

    #[must_use]
    pub fn fg(&self) -> Color {
        self.default_fg.unwrap_or(self.base.fg)
    }

    #[must_use]
    pub fn bg(&self) -> Color {
        self.default_bg.unwrap_or(self.base.bg)
    }

    /// The theme's selection colours, if it sets any: `(background,
    /// foreground)`, either of which may be `None` to leave that half
    /// of the cell alone. `None` for the whole thing means the built-in
    /// reverse-video selection.
    #[must_use]
    pub fn selection(&self) -> Option<(Option<Color>, Option<Color>)> {
        match (self.base.selection_bg, self.base.selection_fg) {
            (None, None) => None,
            pair => Some(pair),
        }
    }

    /// The cursor colour, from `OSC 12` or from the theme, or `None`
    /// to keep the built-in look (the cell drawn in reverse video).
    #[must_use]
    pub fn cursor(&self) -> Option<Color> {
        self.cursor.or(self.base.cursor)
    }

    pub fn set_indexed(&mut self, i: u8, rgb: (u8, u8, u8)) {
        self.indexed[i as usize] = Some(Color::rgb(rgb.0, rgb.1, rgb.2));
    }

    /// `OSC 104` — one entry, or the whole table when `i` is `None`.
    pub fn reset_indexed(&mut self, i: Option<u8>) {
        match i {
            Some(i) => self.indexed[i as usize] = None,
            None => self.indexed = [None; 256],
        }
    }

    pub fn set_dynamic(&mut self, which: vt100::DynamicColor, rgb: (u8, u8, u8)) {
        let c = Some(Color::rgb(rgb.0, rgb.1, rgb.2));
        match which {
            vt100::DynamicColor::Foreground => self.default_fg = c,
            vt100::DynamicColor::Background => self.default_bg = c,
            vt100::DynamicColor::Cursor => self.cursor = c,
        }
    }

    pub fn reset_dynamic(&mut self, which: vt100::DynamicColor) {
        match which {
            vt100::DynamicColor::Foreground => self.default_fg = None,
            vt100::DynamicColor::Background => self.default_bg = None,
            vt100::DynamicColor::Cursor => self.cursor = None,
        }
    }

    /// What to answer an `OSC 10 / 11 / 12 ; ?` query with. The cursor
    /// has no colour of its own until one is set, and reports the
    /// foreground it would otherwise be drawn against.
    #[must_use]
    pub fn dynamic_rgb(&self, which: vt100::DynamicColor) -> (u8, u8, u8) {
        rgb8(match which {
            vt100::DynamicColor::Foreground => self.fg(),
            vt100::DynamicColor::Background => self.bg(),
            vt100::DynamicColor::Cursor => self.cursor().unwrap_or_else(|| self.fg()),
        })
    }

    #[must_use]
    pub fn indexed_rgb(&self, i: u8) -> (u8, u8, u8) {
        rgb8(self.indexed(i))
    }
}

/// A femtovg colour as the eight-bit triple the OSC reports carry.
fn rgb8(c: Color) -> (u8, u8, u8) {
    (
        (c.r * 255.0 + 0.5) as u8,
        (c.g * 255.0 + 0.5) as u8,
        (c.b * 255.0 + 0.5) as u8,
    )
}

/// One cell's worth of a straight underline: solid, doubled, dotted
/// or dashed. Dots and dashes are sized off the rule's own thickness so
/// they keep their proportions at any font size.
fn straight_underline(
    path: &mut Path,
    style: vt100::UnderlineStyle,
    x: f32,
    y: f32,
    width: f32,
    thickness: f32,
) {
    match style {
        vt100::UnderlineStyle::Single => path.rect(x, y, width, thickness),
        vt100::UnderlineStyle::Double => {
            path.rect(x, y, width, thickness);
            path.rect(x, y + thickness * 2.0, width, thickness);
        }
        vt100::UnderlineStyle::Dotted => {
            dashes(path, x, y, width, thickness, thickness, thickness * 2.0);
        }
        vt100::UnderlineStyle::Dashed => {
            dashes(
                path,
                x,
                y,
                width,
                thickness,
                thickness * 3.0,
                thickness * 3.0,
            );
        }
        // Curly is stroked, not filled — see `curly_underline`.
        vt100::UnderlineStyle::Curly => path.rect(x, y, width, thickness),
    }
}

/// Lay `on`-long marks separated by `off` across `width`, phased on the
/// absolute x so the pattern lines up across cell boundaries instead of
/// restarting in each one.
fn dashes(
    path: &mut Path,
    x: f32,
    y: f32,
    width: f32,
    thickness: f32,
    on: f32,
    off: f32,
) {
    let period = on + off;
    let start = (x / period).floor() * period;
    let mut mark = start;
    while mark < x + width {
        let x0 = mark.max(x);
        let x1 = (mark + on).min(x + width);
        if x1 > x0 {
            path.rect(x0, y, x1 - x0, thickness);
        }
        mark += period;
    }
}

/// One cell of the squiggle nvim draws under a diagnostic. Sampled
/// rather than drawn as two arcs so the wave keeps its phase across
/// cells — the sample positions come off the absolute x.
fn curly_underline(
    path: &mut Path,
    x: f32,
    y: f32,
    width: f32,
    thickness: f32,
) {
    let period = (thickness * 6.0).max(4.0);
    let amplitude = thickness;
    let step = (period / 6.0).max(1.0);
    let mut px = x;
    let sample = |px: f32| {
        y + amplitude
            * (std::f32::consts::TAU * (px / period)).sin()
    };
    path.move_to(px, sample(px));
    while px < x + width {
        px = (px + step).min(x + width);
        path.line_to(px, sample(px));
    }
}

fn resolve_cell_colors(
    cell: &vt100::Cell,
    is_cursor: bool,
    is_selected: bool,
    palette: &Palette,
    reverse_video: bool,
) -> (Color, Color) {
    let mut fg = match cell.fgcolor() {
        vt100::Color::Default => palette.fg(),
        vt100::Color::Idx(i) => {
            let i = if cell.bold() && i < 8 { i + 8 } else { i };
            palette.indexed(i)
        }
        vt100::Color::Rgb(r, g, b) => Color::rgb(r, g, b),
    };

    let mut bg = match cell.bgcolor() {
        vt100::Color::Default => palette.bg(),
        vt100::Color::Idx(i) => palette.indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::rgb(r, g, b),
    };

    // A themed selection paints its own colours; a theme that names
    // none keeps the built-in look, where selecting a cell is one more
    // inversion of it. Setting only `selection_bg` fills behind the
    // cell and leaves the character its own colour, which is what the
    // schemes that publish a single selection colour mean.
    let themed_selection = is_selected.then(|| palette.selection()).flatten();

    // DECSCNM (`?5`) reverses the whole screen, cell colours included —
    // it is what terminfo's `flash` blinks the window with. It stacks
    // with the per-cell inversions rather than overriding them.
    let invert = cell.inverse() ^ reverse_video ^ (is_selected && themed_selection.is_none());
    if invert {
        std::mem::swap(&mut fg, &mut bg);
    }

    if let Some((sel_bg, sel_fg)) = themed_selection {
        if let Some(c) = sel_bg {
            bg = c;
        }
        if let Some(c) = sel_fg {
            fg = c;
        }
    }

    // The block cursor. Without an `OSC 12` colour it is the cell in
    // reverse video, which is what veter has always drawn; with one it
    // paints that colour behind the character and the character in the
    // background it was sitting on, the way xterm does.
    if is_cursor {
        match palette.cursor() {
            Some(cursor) => {
                fg = bg;
                bg = cursor;
            }
            None => std::mem::swap(&mut fg, &mut bg),
        }
    }

    // SGR 8, terminfo `invis`: the glyph is drawn in the background it
    // sits on. Painting it rather than skipping it keeps the cell's
    // width and any selection highlight behind it intact.
    if cell.conceal() {
        fg = bg;
    }

    (fg, bg)
}

fn color_key(c: Color) -> u32 {
    let r = (c.r * 255.0 + 0.5) as u32;
    let g = (c.g * 255.0 + 0.5) as u32;
    let b = (c.b * 255.0 + 0.5) as u32;
    let a = (c.a * 255.0 + 0.5) as u32;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// Draw a Powerline separator (U+E0B0..U+E0B7) as a cell-sized
/// primitive rather than a font glyph, for the same reason as the box
/// and block elements: these are tiling shapes whose whole job is to
/// butt seamlessly against the neighbouring cell's background, and a
/// glyph only does that if the font it came from happens to share the
/// primary font's cell box.
///
/// It rarely does. The separators live in the Private Use Area, so they
/// arrive from whatever fallback face `resolve_fallback` lands on, and
/// that face is drawn at the primary's pixel size, not its cell: at
/// 32px, Noto Sans Mono spans 43.6px ascent-to-descent while Symbols
/// Nerd Font Mono spans 32.0 — a separator 27% short of the cell, with
/// the notch that leaves. (Konsole gets away with a font glyph because
/// its default Liberation Mono spans 36.2px, close enough to the patched
/// fonts' 37.2 to hide the seam.) Drawn here they fit exactly, whatever
/// is installed — including nothing.
///
/// Returns `true` if `ch` was a separator and the cell was filled.
fn try_draw_powerline<T: Renderer>(
    canvas: &mut Canvas<T>,
    ch: char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fg: Color,
) -> bool {
    let code = ch as u32;
    if !(0xE0B0..=0xE0B7).contains(&code) {
        return false;
    }
    // The thin variants are strokes of the same outline. Scale the pen
    // with the cell so it stays visible at small sizes and doesn't turn
    // into a slab at large ones.
    // `max` before `clamp`: at a tiny cell the upper bound can fall
    // under the lower one, and `clamp` panics on an inverted range.
    let pen = (w * 0.14).clamp(1.0, (h * 0.12).max(1.0));
    let (right, thin) = match code {
        0xE0B0 => (true, false),
        0xE0B1 => (true, true),
        0xE0B2 => (false, false),
        0xE0B3 => (false, true),
        0xE0B4 => (true, false),
        0xE0B5 => (true, true),
        0xE0B6 => (false, false),
        _ => (false, true),
    };
    let round = code >= 0xE0B4;

    // Inset a stroked outline by half the pen, or it is clipped in half
    // by the cell edge.
    let (x0, x1) = if thin {
        let i = pen * 0.5;
        if right { (x + i, x + w - i) } else { (x + w - i, x + i) }
    } else if right {
        (x, x + w)
    } else {
        (x + w, x)
    };
    let (y0, y1) = if thin {
        (y + pen * 0.5, y + h - pen * 0.5)
    } else {
        (y, y + h)
    };

    let mut p = Path::new();
    p.move_to(x0, y0);
    if round {
        // A semicircle bulging toward `x1`. The 4/3 control offset is
        // the standard cubic approximation of a half-ellipse.
        let bulge = (x1 - x0) * 4.0 / 3.0;
        p.bezier_to(x0 + bulge, y0, x0 + bulge, y1, x0, y1);
    } else {
        p.line_to(x1, (y0 + y1) * 0.5);
        p.line_to(x0, y1);
    }
    if thin {
        let mut paint = Paint::color(fg);
        paint.set_line_width(pen);
        canvas.stroke_path(&p, &paint);
    } else {
        p.close();
        canvas.fill_path(&p, &Paint::color(fg));
    }
    true
}

/// Draw a Unicode block element (U+2580..U+259F) directly with cell-sized
/// rectangles instead of using the font glyph. Most monospace fonts ship
/// block glyphs that fall short of the cell box (especially the cell
/// height when leading is non-zero), which leaves visible gaps when these
/// characters are tiled — see e.g. ASCII art that uses U+2588 FULL BLOCK.
/// Konsole, kitty, alacritty, wezterm all do the same thing.
///
/// Returns `true` if `ch` was a block element and the cell was filled.
fn try_draw_block_element<T: Renderer>(
    canvas: &mut Canvas<T>,
    ch: char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fg: Color,
) -> bool {
    let code = ch as u32;
    if !(0x2580..=0x259F).contains(&code) {
        return false;
    }

    let fill = |canvas: &mut Canvas<T>, rx: f32, ry: f32, rw: f32, rh: f32, color: Color| {
        let mut p = Path::new();
        p.rect(rx, ry, rw, rh);
        canvas.fill_path(&p, &Paint::color(color));
    };
    let shaded = |alpha: u8| Color::rgba((fg.r * 255.0) as u8, (fg.g * 255.0) as u8, (fg.b * 255.0) as u8, alpha);

    let cx = x + w * 0.5;
    let cy = y + h * 0.5;
    let half_w = w * 0.5;
    let half_h = h * 0.5;

    match code {
        // U+2580 UPPER HALF BLOCK
        0x2580 => fill(canvas, x, y, w, half_h, fg),
        // U+2581..U+2587 LOWER N/8 BLOCK (1/8 .. 7/8 from bottom)
        0x2581..=0x2587 => {
            let n = (code - 0x2580) as f32; // 1..=7
            let bh = h * n / 8.0;
            fill(canvas, x, y + h - bh, w, bh, fg);
        }
        // U+2588 FULL BLOCK
        0x2588 => fill(canvas, x, y, w, h, fg),
        // U+2589..U+258F LEFT N/8 BLOCK (7/8 .. 1/8 from left)
        0x2589..=0x258F => {
            let n = (0x2590 - code) as f32; // 7..=1
            fill(canvas, x, y, w * n / 8.0, h, fg);
        }
        // U+2590 RIGHT HALF BLOCK
        0x2590 => fill(canvas, cx, y, half_w, h, fg),
        // U+2591 LIGHT SHADE
        0x2591 => fill(canvas, x, y, w, h, shaded(64)),
        // U+2592 MEDIUM SHADE
        0x2592 => fill(canvas, x, y, w, h, shaded(128)),
        // U+2593 DARK SHADE
        0x2593 => fill(canvas, x, y, w, h, shaded(192)),
        // U+2594 UPPER ONE EIGHTH BLOCK
        0x2594 => fill(canvas, x, y, w, h / 8.0, fg),
        // U+2595 RIGHT ONE EIGHTH BLOCK
        0x2595 => fill(canvas, x + w * 7.0 / 8.0, y, w / 8.0, h, fg),
        // U+2596..U+259F QUADRANT BLOCKS
        0x2596..=0x259F => {
            // Bitfield: bit0=UL, bit1=UR, bit2=LL, bit3=LR.
            let mask: u8 = match code {
                0x2596 => 0b0100, // ▖ LL
                0x2597 => 0b1000, // ▗ LR
                0x2598 => 0b0001, // ▘ UL
                0x2599 => 0b1101, // ▙ UL+LL+LR
                0x259A => 0b1001, // ▚ UL+LR
                0x259B => 0b0111, // ▛ UL+UR+LL
                0x259C => 0b1011, // ▜ UL+UR+LR
                0x259D => 0b0010, // ▝ UR
                0x259E => 0b0110, // ▞ UR+LL
                0x259F => 0b1110, // ▟ UR+LL+LR
                _ => unreachable!(),
            };
            if mask & 0b0001 != 0 {
                fill(canvas, x, y, half_w, half_h, fg);
            }
            if mask & 0b0010 != 0 {
                fill(canvas, cx, y, half_w, half_h, fg);
            }
            if mask & 0b0100 != 0 {
                fill(canvas, x, cy, half_w, half_h, fg);
            }
            if mask & 0b1000 != 0 {
                fill(canvas, cx, cy, half_w, half_h, fg);
            }
        }
        _ => return false,
    }
    true
}

// Box-drawing range (U+2500..U+257F): same rationale as the block
// elements above. Box-drawing glyphs in most fonts don't tile cleanly
// (visible gaps at cell joints, weight inconsistencies between Light
// and Heavy variants), so terminals draw these as primitives.
//
// Each cell is modelled as four directional stubs (N/E/S/W) at one of
// {None, Light, Heavy, Double}, plus a small set of specials for
// dashes (12 chars), arcs (4), and diagonals (3). 109 chars are pure
// stub combinations; 19 are specials.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Stub {
    None,
    Light,
    Heavy,
    Double,
}

#[derive(Copy, Clone)]
enum BoxSpecial {
    None,
    /// Horizontal dashed rule across full cell width. (heavy, count).
    DashH(bool, u8),
    /// Vertical dashed rule across full cell height. (heavy, count).
    DashV(bool, u8),
    /// Light arc joining two adjacent edges. Booleans pick which two:
    /// (right, down) — true means the arc reaches that edge.
    /// ╭=(t,t) ╮=(f,t) ╯=(f,f) ╰=(t,f).
    Arc(bool, bool),
    /// Diagonals. (nw_se, ne_sw): ╲=(t,f) ╱=(f,t) ╳=(t,t).
    Diag(bool, bool),
}

#[derive(Copy, Clone)]
struct BoxDef {
    n: Stub,
    e: Stub,
    s: Stub,
    w: Stub,
    special: BoxSpecial,
}

const fn b(n: Stub, e: Stub, s: Stub, w: Stub) -> BoxDef {
    BoxDef { n, e, s, w, special: BoxSpecial::None }
}
const fn bs(special: BoxSpecial) -> BoxDef {
    BoxDef { n: Stub::None, e: Stub::None, s: Stub::None, w: Stub::None, special }
}

// Indexed by (codepoint - 0x2500). Comments give the codepoint and
// glyph; stubs are listed N/E/S/W.
#[rustfmt::skip]
static BOX_DRAWING: [BoxDef; 128] = {
    use Stub::{Double as D, Heavy as H, Light as L, None as O};
    use BoxSpecial::{Arc, DashH, DashV, Diag};
    [
        // 2500 ─  2501 ━  2502 │  2503 ┃
        b(O, L, O, L), b(O, H, O, H), b(L, O, L, O), b(H, O, H, O),
        // 2504 ┄ 2505 ┅  2506 ┆  2507 ┇  (triple dash)
        bs(DashH(false, 3)), bs(DashH(true, 3)), bs(DashV(false, 3)), bs(DashV(true, 3)),
        // 2508 ┈ 2509 ┉  250A ┊  250B ┋  (quad dash)
        bs(DashH(false, 4)), bs(DashH(true, 4)), bs(DashV(false, 4)), bs(DashV(true, 4)),
        // 250C ┌  250D ┍  250E ┎  250F ┏
        b(O, L, L, O), b(O, H, L, O), b(O, L, H, O), b(O, H, H, O),
        // 2510 ┐  2511 ┑  2512 ┒  2513 ┓
        b(O, O, L, L), b(O, O, L, H), b(O, O, H, L), b(O, O, H, H),
        // 2514 └  2515 ┕  2516 ┖  2517 ┗
        b(L, L, O, O), b(L, H, O, O), b(H, L, O, O), b(H, H, O, O),
        // 2518 ┘  2519 ┙  251A ┚  251B ┛
        b(L, O, O, L), b(L, O, O, H), b(H, O, O, L), b(H, O, O, H),
        // 251C ├  251D ┝  251E ┞  251F ┟
        b(L, L, L, O), b(L, H, L, O), b(H, L, L, O), b(L, L, H, O),
        // 2520 ┠  2521 ┡  2522 ┢  2523 ┣
        b(H, L, H, O), b(H, H, L, O), b(L, H, H, O), b(H, H, H, O),
        // 2524 ┤  2525 ┥  2526 ┦  2527 ┧
        b(L, O, L, L), b(L, O, L, H), b(H, O, L, L), b(L, O, H, L),
        // 2528 ┨  2529 ┩  252A ┪  252B ┫
        b(H, O, H, L), b(H, O, L, H), b(L, O, H, H), b(H, O, H, H),
        // 252C ┬  252D ┭  252E ┮  252F ┯
        b(O, L, L, L), b(O, L, L, H), b(O, H, L, L), b(O, H, L, H),
        // 2530 ┰  2531 ┱  2532 ┲  2533 ┳
        b(O, L, H, L), b(O, L, H, H), b(O, H, H, L), b(O, H, H, H),
        // 2534 ┴  2535 ┵  2536 ┶  2537 ┷
        b(L, L, O, L), b(L, L, O, H), b(L, H, O, L), b(L, H, O, H),
        // 2538 ┸  2539 ┹  253A ┺  253B ┻
        b(H, L, O, L), b(H, L, O, H), b(H, H, O, L), b(H, H, O, H),
        // 253C ┼  253D ┽  253E ┾  253F ┿
        b(L, L, L, L), b(L, L, L, H), b(L, H, L, L), b(L, H, L, H),
        // 2540 ╀  2541 ╁  2542 ╂  2543 ╃
        b(H, L, L, L), b(L, L, H, L), b(H, L, H, L), b(H, L, L, H),
        // 2544 ╄  2545 ╅  2546 ╆  2547 ╇
        b(H, H, L, L), b(L, L, H, H), b(L, H, H, L), b(H, H, L, H),
        // 2548 ╈  2549 ╉  254A ╊  254B ╋
        b(L, H, H, H), b(H, L, H, H), b(H, H, H, L), b(H, H, H, H),
        // 254C ╌  254D ╍  254E ╎  254F ╏  (double dash)
        bs(DashH(false, 2)), bs(DashH(true, 2)), bs(DashV(false, 2)), bs(DashV(true, 2)),
        // 2550 ═  2551 ║  2552 ╒  2553 ╓
        b(O, D, O, D), b(D, O, D, O), b(O, D, L, O), b(O, L, D, O),
        // 2554 ╔  2555 ╕  2556 ╖  2557 ╗
        b(O, D, D, O), b(O, O, L, D), b(O, O, D, L), b(O, O, D, D),
        // 2558 ╘  2559 ╙  255A ╚  255B ╛
        b(L, D, O, O), b(D, L, O, O), b(D, D, O, O), b(L, O, O, D),
        // 255C ╜  255D ╝  255E ╞  255F ╟
        b(D, O, O, L), b(D, O, O, D), b(L, D, L, O), b(D, L, D, O),
        // 2560 ╠  2561 ╡  2562 ╢  2563 ╣
        b(D, D, D, O), b(L, O, L, D), b(D, O, D, L), b(D, O, D, D),
        // 2564 ╤  2565 ╥  2566 ╦  2567 ╧
        b(O, L, D, L), b(O, D, L, D), b(O, D, D, D), b(D, L, O, L),
        // 2568 ╨  2569 ╩  256A ╪  256B ╫
        b(L, D, O, D), b(D, D, O, D), b(L, D, L, D), b(D, L, D, L),
        // 256C ╬  256D ╭  256E ╮  256F ╯
        b(D, D, D, D), bs(Arc(true, true)), bs(Arc(false, true)), bs(Arc(false, false)),
        // 2570 ╰  2571 ╱  2572 ╲  2573 ╳
        bs(Arc(true, false)), bs(Diag(false, true)), bs(Diag(true, false)), bs(Diag(true, true)),
        // 2574 ╴  2575 ╵  2576 ╶  2577 ╷
        b(O, O, O, L), b(L, O, O, O), b(O, L, O, O), b(O, O, L, O),
        // 2578 ╸  2579 ╹  257A ╺  257B ╻
        b(O, O, O, H), b(H, O, O, O), b(O, H, O, O), b(O, O, H, O),
        // 257C ╼  257D ╽  257E ╾  257F ╿
        b(O, H, O, L), b(L, O, H, O), b(O, L, O, H), b(H, O, L, O),
    ]
};

/// Draw a Unicode box-drawing element (U+2500..U+257F) directly with
/// stub-based primitives. Returns `true` if `ch` was recognised and
/// the cell was filled.
fn try_draw_box_drawing<T: Renderer>(
    canvas: &mut Canvas<T>,
    ch: char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fg: Color,
) -> bool {
    let code = ch as u32;
    if !(0x2500..=0x257F).contains(&code) {
        return false;
    }
    let def = BOX_DRAWING[(code - 0x2500) as usize];

    // Light = 1 unit, Heavy ≈ 2 units. Double = two light rules with a
    // light-sized gap (3 light units span). Tuned so a 24px cell gives
    // light=2, heavy=4 — visually consistent with what fonts ship.
    let light = (h / 14.0).round().max(1.0);
    let heavy = (light * 2.0).max(2.0);

    let cx = x + w * 0.5;
    let cy = y + h * 0.5;

    let fill = |canvas: &mut Canvas<T>, rx: f32, ry: f32, rw: f32, rh: f32| {
        if rw <= 0.0 || rh <= 0.0 {
            return;
        }
        let mut p = Path::new();
        p.rect(rx, ry, rw, rh);
        canvas.fill_path(&p, &Paint::color(fg));
    };

    // Specials short-circuit before stub rendering — they don't combine
    // with stubs in the box-drawing range.
    match def.special {
        BoxSpecial::None => {}
        BoxSpecial::DashH(heavy_w, n) => {
            let thick = if heavy_w { heavy } else { light };
            let gap = w / (3.0 * n as f32 - 1.0);
            let dash = gap * 2.0;
            for i in 0..n as usize {
                fill(canvas, x + i as f32 * (dash + gap), cy - thick * 0.5, dash, thick);
            }
            return true;
        }
        BoxSpecial::DashV(heavy_w, n) => {
            let thick = if heavy_w { heavy } else { light };
            let gap = h / (3.0 * n as f32 - 1.0);
            let dash = gap * 2.0;
            for i in 0..n as usize {
                fill(canvas, cx - thick * 0.5, y + i as f32 * (dash + gap), thick, dash);
            }
            return true;
        }
        BoxSpecial::Arc(right, down) => {
            // True quarter-circle of radius `r = min(w/2, h/2)`, with
            // straight bridge segments from each cell-edge midpoint to
            // the arc tangent point. A quadratic with control at the
            // cell center stretches with the cell's aspect ratio, so
            // tall cells produce a vertically-elongated curve; this
            // formulation keeps the curvature symmetric and snaps the
            // straight legs onto adjacent cells' lines at cx / cy.
            let r = (w * 0.5).min(h * 0.5);
            let sign_r = if right { 1.0 } else { -1.0 };
            let sign_d = if down { 1.0 } else { -1.0 };
            let center_x = cx + sign_r * r;
            let center_y = cy + sign_d * r;
            // Arc endpoints (in screen-y-down polar): the v-side end is
            // due-east or due-west of the center; the h-side end is
            // due-south or due-north. Sweep direction follows the
            // sign(right) × sign(down) parity.
            let pi = std::f32::consts::PI;
            let theta_v = if right { pi } else { 0.0 };
            let theta_h = if down { 1.5 * pi } else { 0.5 * pi };
            let solidity = if right == down {
                Solidity::Hole
            } else {
                Solidity::Solid
            };
            let v_edge_y = if down { y + h } else { y };
            let h_edge_x = if right { x + w } else { x };
            let mut p = Path::new();
            p.move_to(cx, v_edge_y);
            p.line_to(cx, center_y);
            p.arc(center_x, center_y, r, theta_v, theta_h, solidity);
            p.line_to(h_edge_x, cy);
            canvas.stroke_path(&p, &Paint::color(fg).with_line_width(light));
            return true;
        }
        BoxSpecial::Diag(nw_se, ne_sw) => {
            let mut p = Path::new();
            if nw_se {
                p.move_to(x, y);
                p.line_to(x + w, y + h);
            }
            if ne_sw {
                p.move_to(x + w, y);
                p.line_to(x, y + h);
            }
            canvas.stroke_path(&p, &Paint::color(fg).with_line_width(light));
            return true;
        }
    }

    // Pure-double corners (exactly two stubs, both Double, perpendicular).
    // Naïve stub-by-stub leaves the outer-corner pixel empty because each
    // double rule stops at center; extend the relevant rule past center
    // to close it.
    let (n_d, e_d, s_d, w_d) = (
        def.n == Stub::Double,
        def.e == Stub::Double,
        def.s == Stub::Double,
        def.w == Stub::Double,
    );
    let (n_any, e_any, s_any, w_any) = (
        def.n != Stub::None,
        def.e != Stub::None,
        def.s != Stub::None,
        def.w != Stub::None,
    );
    let dr = e_d && s_d && !n_any && !w_any; // ╔
    let dl = w_d && s_d && !n_any && !e_any; // ╗
    let ur = n_d && e_d && !s_any && !w_any; // ╚
    let ul = n_d && w_d && !s_any && !e_any; // ╝

    // East stub.
    match def.e {
        Stub::None => {}
        Stub::Light => fill(canvas, cx, cy - light * 0.5, x + w - cx, light),
        Stub::Heavy => fill(canvas, cx, cy - heavy * 0.5, x + w - cx, heavy),
        Stub::Double => {
            let top_x = if dr { cx - 1.5 * light } else { cx };
            let bot_x = if ur { cx - 1.5 * light } else { cx };
            fill(canvas, top_x, cy - 1.5 * light, x + w - top_x, light);
            fill(canvas, bot_x, cy + 0.5 * light, x + w - bot_x, light);
        }
    }
    // West stub.
    match def.w {
        Stub::None => {}
        Stub::Light => fill(canvas, x, cy - light * 0.5, cx - x, light),
        Stub::Heavy => fill(canvas, x, cy - heavy * 0.5, cx - x, heavy),
        Stub::Double => {
            let top_w = if dl { cx + 1.5 * light - x } else { cx - x };
            let bot_w = if ul { cx + 1.5 * light - x } else { cx - x };
            fill(canvas, x, cy - 1.5 * light, top_w, light);
            fill(canvas, x, cy + 0.5 * light, bot_w, light);
        }
    }
    // North stub.
    match def.n {
        Stub::None => {}
        Stub::Light => fill(canvas, cx - light * 0.5, y, light, cy - y),
        Stub::Heavy => fill(canvas, cx - heavy * 0.5, y, heavy, cy - y),
        Stub::Double => {
            let left_h = if ul { cy + 1.5 * light - y } else { cy - y };
            let right_h = if ur { cy + 1.5 * light - y } else { cy - y };
            fill(canvas, cx - 1.5 * light, y, light, left_h);
            fill(canvas, cx + 0.5 * light, y, light, right_h);
        }
    }
    // South stub.
    match def.s {
        Stub::None => {}
        Stub::Light => fill(canvas, cx - light * 0.5, cy, light, y + h - cy),
        Stub::Heavy => fill(canvas, cx - heavy * 0.5, cy, heavy, y + h - cy),
        Stub::Double => {
            let left_y = if dl { cy - 1.5 * light } else { cy };
            let right_y = if dr { cy - 1.5 * light } else { cy };
            fill(canvas, cx - 1.5 * light, left_y, light, y + h - left_y);
            fill(canvas, cx + 0.5 * light, right_y, light, y + h - right_y);
        }
    }

    true
}

fn key_to_color(key: u32) -> Color {
    Color::rgba(
        ((key >> 16) & 0xFF) as u8,
        ((key >> 8) & 0xFF) as u8,
        (key & 0xFF) as u8,
        ((key >> 24) & 0xFF) as u8,
    )
}

// --- Glyph cache ---

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
struct GlyphCacheKey {
    glyph_id: u16,
    font_id: u16, // 0 = primary, 1+ = fallback index + 1
    font_size_tenths: u32,
}

#[derive(Copy, Clone, Debug)]
struct RenderedGlyph {
    texture_index: usize,
    width: u32,
    height: u32,
    offset_x: i32,
    offset_y: i32,
    atlas_x: u32,
    atlas_y: u32,
    color_glyph: bool,
}

struct FontTexture {
    atlas: Atlas,
    image_id: femtovg::ImageId,
}

struct GlyphCache {
    entries: HashMap<GlyphCacheKey, Option<RenderedGlyph>>,
    textures: Vec<FontTexture>,
}

impl GlyphCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            textures: Vec::new(),
        }
    }

    fn get_or_render<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        scale_cx: &mut ScaleContext,
        font_ref: FontRef<'_>,
        glyph_id: u16,
        font_size: f32,
        font_id: u16,
    ) -> Option<RenderedGlyph> {
        let key = GlyphCacheKey {
            glyph_id,
            font_id,
            font_size_tenths: (font_size * 10.0) as u32,
        };

        if let Some(cached) = self.entries.get(&key) {
            return *cached;
        }

        let mut scaler = scale_cx.builder(font_ref).size(font_size).hint(true).build();
        let result = self.render_glyph(canvas, &mut scaler, glyph_id);
        self.entries.insert(key, result);
        result
    }

    fn render_glyph<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        scaler: &mut swash::scale::Scaler<'_>,
        glyph_id: u16,
    ) -> Option<RenderedGlyph> {
        let image = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ])
        .format(Format::Alpha)
        .render(scaler, glyph_id)?;

        let w = image.placement.width as usize;
        let h = image.placement.height as usize;
        if w == 0 || h == 0 {
            return None;
        }

        let mut pixels = Vec::with_capacity(w * h);
        match image.content {
            Content::Mask => {
                for &alpha in &image.data {
                    pixels.push(RGBA8::new(alpha, 0, 0, 0));
                }
            }
            Content::Color => {
                for chunk in image.data.chunks_exact(4) {
                    pixels.push(RGBA8::new(chunk[0], chunk[1], chunk[2], chunk[3]));
                }
            }
            Content::SubpixelMask => unreachable!(),
        }

        // Find atlas space. Reserve the glyph box plus its gutter; the
        // returned (ax, ay) still addresses the glyph's own top-left,
        // and `RenderedGlyph.width/height` stay the glyph's real size.
        let mut found = None;
        for (idx, tex) in self.textures.iter_mut().enumerate() {
            if let Some((ax, ay)) = tex.atlas.add_rect(w + GLYPH_GUTTER, h + GLYPH_GUTTER) {
                found = Some((idx, ax, ay));
                break;
            }
        }

        let (tex_idx, ax, ay) = found.unwrap_or_else(|| {
            let mut atlas = Atlas::new(TEXTURE_SIZE, TEXTURE_SIZE);
            let image_id = canvas
                .create_image(
                    Img::new(
                        vec![RGBA8::new(0, 0, 0, 0); TEXTURE_SIZE * TEXTURE_SIZE],
                        TEXTURE_SIZE,
                        TEXTURE_SIZE,
                    )
                    .as_ref(),
                    ImageFlags::NEAREST,
                )
                .unwrap();
            let (ax, ay) = atlas.add_rect(w + GLYPH_GUTTER, h + GLYPH_GUTTER).unwrap();
            let idx = self.textures.len();
            self.textures.push(FontTexture { atlas, image_id });
            (idx, ax, ay)
        });

        canvas
            .update_image::<ImageSource>(
                self.textures[tex_idx].image_id,
                ImgRef::new(&pixels, w, h).into(),
                ax,
                ay,
            )
            .unwrap();

        Some(RenderedGlyph {
            texture_index: tex_idx,
            width: image.placement.width,
            height: image.placement.height,
            offset_x: image.placement.left,
            offset_y: image.placement.top,
            atlas_x: ax as u32,
            atlas_y: ay as u32,
            color_glyph: matches!(image.content, Content::Color),
        })
    }
}

// --- Font fallback ---

struct FallbackFont {
    data: Vec<u8>,
    index: usize,
    /// Rasterisation scales that fit this face to the primary's cell,
    /// kept apart so a double-width character can be fitted to the two
    /// cells it occupies (see `cell_fit`). Both are 1.0 for a face
    /// already proportioned like the primary, and well under it for a
    /// symbol font whose glyphs are a full em wide.
    width_fit: f32,
    height_fit: f32,
}

/// The primary font's cell, which fallback faces are fitted to.
#[derive(Debug, Clone, Copy)]
struct CellMetrics {
    width: f32,
    height: f32,
    /// Pixel size the primary is rasterised at.
    size: f32,
}

/// How much to shrink (or grow) a fallback face so one of its glyphs
/// lands inside one cell instead of sprawling across its neighbours.
///
/// A fallback is rasterised at the primary's pixel size, which says
/// nothing about how big its glyphs come out: at 32px Symbols Nerd Font
/// Mono advances 32.0px per glyph — "Mono" there means one advance for
/// every glyph, an em wide, not one that matches a terminal — against a
/// 19.2px cell. Its icons overhang the next two columns. The patched
/// full fonts are already close (CaskaydiaCove advances 18.75px), so
/// they come out near 1.0 and are left alone.
///
/// Fitting is uniform, so icons keep their shape. Cell-filling glyphs
/// need the opposite treatment — exact, non-uniform, cell geometry —
/// which is why the separators are drawn as primitives instead.
fn cell_fit(data: &[u8], index: usize, cell: CellMetrics) -> (f32, f32) {
    let Some(font_ref) = FontRef::from_index(data, index) else {
        return (1.0, 1.0);
    };
    let m = font_ref.metrics(&[]).scale(cell.size);
    let line = m.ascent + m.descent;
    let width = if m.average_width > 0.0 {
        (cell.width / m.average_width).clamp(0.1, 4.0)
    } else {
        1.0
    };
    let height = if line > 0.0 {
        (cell.height / line).clamp(0.1, 4.0)
    } else {
        1.0
    };
    (width, height)
}

/// Which faces the grid is drawn with. Mirrors the binary's `[font]`
/// config section, which owns the defaults — `renderer` is in the
/// library half and cannot see `config`.
#[derive(Debug, Clone, Default)]
pub struct FontSpec {
    /// Primary family, resolved by Fontconfig. Empty means `monospace`.
    pub family: String,
    /// Families tried in order for a character the primary lacks.
    pub fallback: Vec<String>,
}

/// Resolved glyph: which font and glyph ID to use for a character.
#[derive(Copy, Clone)]
struct ResolvedGlyph {
    glyph_id: u16,
    font_id: u16, // 0 = primary, 1+ = fallback index + 1
}

/// Resolve a character to a fallback font. Uses Parley for font discovery.
/// Kept as a free function so the caller can pass disjoint struct fields.
/// A Private Use Area codepoint (BMP block plus the two supplementary
/// planes). Coverage of one of these is not evidence of intent: the
/// range has no agreed meaning, so a font may map it to anything of its
/// own — Adwaita Sans, the GNOME UI font, spends 745 PUA codepoints on
/// stylistic alternates, and lands `divide.case` on U+E0A0 where a
/// powerline branch belongs. Hence the arbitration below.
fn is_private_use(ch: char) -> bool {
    matches!(
        ch as u32,
        0xE000..=0xF8FF | 0xF_0000..=0xF_FFFD | 0x10_0000..=0x10_FFFD
    )
}

/// Ask Fontconfig which font actually owns `ch` — an ordered charset
/// match across everything installed, which is how Konsole and every
/// other Qt/GTK terminal resolves a missing glyph. `None` if `fc-match`
/// isn't there to ask.
fn fontconfig_family_for(ch: char) -> Option<String> {
    let out = std::process::Command::new("fc-match")
        .arg("-f")
        .arg("%{family[0]}")
        .arg(format!(":charset={:x}", ch as u32))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Intern `data` and return its `font_id` (1-based; 0 is the primary).
/// Identity is the font bytes themselves. Anything cheaper is a trap:
/// a family name collides across weights, and Parley's blob address
/// can be recycled once the blob is dropped, which would silently point
/// a cached glyph id at a different font's tables.
fn register_fallback(
    fallback_fonts: &mut Vec<FallbackFont>,
    data: &[u8],
    index: usize,
    cell: CellMetrics,
) -> u16 {
    let idx = fallback_fonts
        .iter()
        .position(|fb| fb.index == index && fb.data == data)
        .unwrap_or_else(|| {
            let (width_fit, height_fit) = cell_fit(data, index, cell);
            fallback_fonts.push(FallbackFont {
                data: data.to_vec(),
                index,
                width_fit,
                height_fit,
            });
            fallback_fonts.len() - 1
        });
    (idx + 1) as u16
}

/// Resolve `ch` against a named family, if it is installed and maps it.
fn glyph_in_family(
    font_cx: &mut FontContext,
    fallback_fonts: &mut Vec<FallbackFont>,
    name: &str,
    ch: char,
    cell: CellMetrics,
) -> Option<ResolvedGlyph> {
    let family = font_cx.collection.family_by_name(name)?;
    let info = family.default_font()?;
    let index = info.index() as usize;
    let blob = info.load(Some(&mut font_cx.source_cache))?;
    let data = blob.as_ref();
    let glyph_id = FontRef::from_index(data, index)?.charmap().map(ch);
    if glyph_id == 0 {
        return None;
    }
    Some(ResolvedGlyph {
        glyph_id,
        font_id: register_fallback(fallback_fonts, data, index, cell),
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_fallback(
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<Color>,
    fallback_fonts: &mut Vec<FallbackFont>,
    char_font_map: &mut HashMap<char, Option<ResolvedGlyph>>,
    fallback_families: &[String],
    pua_families: &mut Vec<String>,
    ch: char,
    cell: CellMetrics,
) -> Option<ResolvedGlyph> {
    if let Some(&cached) = char_font_map.get(&ch) {
        return cached;
    }

    // Configured families first, in order. Absent families just miss.
    for name in fallback_families {
        if let Some(g) = glyph_in_family(font_cx, fallback_fonts, name, ch, cell) {
            char_font_map.insert(ch, Some(g));
            return Some(g);
        }
    }

    // For the PUA, let Fontconfig arbitrate rather than accepting the
    // first family that claims the codepoint. One query serves a whole
    // icon set: the family it names is remembered, so the rest of a
    // status line resolves without spawning anything.
    //
    // Kept apart from the configured list on purpose — a family found
    // for one icon must not become the fallback for unrelated
    // characters, or which font draws U+25B6 would depend on whether an
    // icon happened to be rendered earlier in the session.
    if is_private_use(ch) {
        for i in 0..pua_families.len() {
            let name = pua_families[i].clone();
            if let Some(g) = glyph_in_family(font_cx, fallback_fonts, &name, ch, cell) {
                char_font_map.insert(ch, Some(g));
                return Some(g);
            }
        }
        if let Some(name) = fontconfig_family_for(ch)
            && let Some(g) = glyph_in_family(font_cx, fallback_fonts, &name, ch, cell)
        {
            pua_families.push(name);
            char_font_map.insert(ch, Some(g));
            return Some(g);
        }
    }

    // Generic lookup, and what everything above is refining. A PUA
    // character reaches here only when Fontconfig had no answer (or is
    // absent) — better the old guess than a blank cell, since a system
    // font may legitimately be the one carrying those icons.
    let s = String::from(ch);
    let mut builder = layout_cx.ranged_builder(font_cx, &s, 1.0, false);
    builder.push_default(StyleProperty::Brush(Color::white()));
    builder.push_default(FontStack::from("system-ui"));
    builder.push_default(StyleProperty::FontSize(cell.size));
    let mut layout: Layout<Color> = builder.build(&s);
    layout.break_all_lines(None);
    layout.align(None, Alignment::Start, AlignmentOptions::default());

    for line in layout.lines() {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let font = run.font();
                let data_ref = font.data.as_ref();
                let index = font.index as usize;

                let font_ref = FontRef::from_index(data_ref, index).unwrap();
                let glyph_id = font_ref.charmap().map(ch);

                if glyph_id != 0 {
                    let resolved = ResolvedGlyph {
                        glyph_id,
                        font_id: register_fallback(fallback_fonts, data_ref, index, cell),
                    };
                    char_font_map.insert(ch, Some(resolved));
                    return Some(resolved);
                }
            }
        }
    }

    char_font_map.insert(ch, None);
    None
}

// --- Glyph-batch helpers (used by both DrawText render paths) ---

/// Horizontal extent of a drawn VGE text run, in device pixels.
/// `start_x` is the run's left edge after `align` has been applied to
/// its anchor, so it is not generally the `x_px` that was passed in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextExtent {
    pub start_x: f32,
    pub total_width: f32,
}

/// What a drawn VGE run leaves behind: where it landed, and where each
/// character boundary falls inside it. The stops come from the layout
/// the draw itself used, so the pick index can answer "which character
/// is under this point" (§15) without a second measurement that could
/// drift out of step with what was painted.
pub struct TextRun {
    pub extent: TextExtent,
    /// `(byte offset, x offset from `extent.start_x`)`, ascending in
    /// both, terminated by a `(text.len(), total_width)` sentinel.
    pub stops: Vec<(usize, f32)>,
}

/// One glyph of a plain (unstyled) run. `x` is the offset from the
/// run's `start_x`.
struct PlainGlyph {
    ch: char,
    glyph_id: u16,
    font_id: u16,
    x: f32,
}

/// One glyph of a Parley-shaped run. `x`/`y` are offsets from the run's
/// `start_x` and baseline respectively.
struct StyledGlyph {
    x: f32,
    y: f32,
    glyph_id: u16,
    font_id: u16,
}

enum LayoutGlyphs {
    Plain(Vec<PlainGlyph>),
    Styled(Vec<StyledGlyph>),
}

/// A shaped VGE text run: everything needed to draw it, plus the map
/// from horizontal position back to a byte offset in the source string.
///
/// Produced by [`TerminalRenderer::layout_vge_text`] and consumed both
/// by drawing and by pointer hit-testing, so a click can never resolve
/// to a different character than the one painted under it.
pub struct TextLayout {
    /// Left edge of the run in device pixels, after alignment.
    pub start_x: f32,
    /// Total advance width in device pixels.
    pub total_width: f32,
    /// Rasterisation size (`font_size · scale`).
    font_px: f32,
    /// Character boundaries as `(byte offset, x offset from
    /// `start_x`)`, ascending in both, terminated by a
    /// `(text.len(), total_width)` sentinel.
    stops: Vec<(usize, f32)>,
    glyphs: LayoutGlyphs,
}

impl TextLayout {
    /// The run's character boundaries as `(byte offset, x offset from
    /// `start_x`)`. Handed to the pick index so a hit test reads the
    /// same measurements the draw used (§15).
    pub fn stops(&self) -> &[(usize, f32)] {
        &self.stops
    }

    pub fn extent(&self) -> TextExtent {
        TextExtent {
            start_x: self.start_x,
            total_width: self.total_width,
        }
    }

    /// The character boundary nearest a device-pixel x — a caret
    /// position, so clicking the left half of a glyph lands before it
    /// and the right half after it. Clamped to the run.
    pub fn byte_offset_at(&self, x_px: f32) -> usize {
        let rel = x_px - self.start_x;
        let mut best = self.stops.first().map_or(0, |(off, _)| *off);
        let mut best_dist = f32::INFINITY;
        for (off, x) in &self.stops {
            let dist = (x - rel).abs();
            if dist < best_dist {
                best_dist = dist;
                best = *off;
            } else if dist > best_dist {
                // `stops` ascends in x, so distance is unimodal: once it
                // starts growing there is nothing better ahead. Equal
                // distances are *not* a stopping point — a character
                // with no glyph contributes zero advance, so several
                // boundaries can share an x.
                break;
            }
        }
        best
    }

    /// The byte range of the character *under* a device-pixel x, or
    /// `None` when the position falls outside the run. Unlike
    /// [`Self::byte_offset_at`] this never rounds to a neighbour, which
    /// is what "select the thing I clicked on" wants.
    pub fn char_range_at(&self, x_px: f32) -> Option<(usize, usize)> {
        let rel = x_px - self.start_x;
        if rel < 0.0 || rel > self.total_width {
            return None;
        }
        self.stops
            .windows(2)
            .find(|w| rel >= w[0].1 && rel < w[1].1)
            .map(|w| (w[0].0, w[1].0))
    }

    /// The x offset in device pixels of a byte offset, clamped to the
    /// run. Used to place a selection highlight over the run.
    pub fn x_of_byte(&self, byte_off: usize) -> f32 {
        for (off, x) in &self.stops {
            if *off >= byte_off {
                return self.start_x + x;
            }
        }
        self.start_x + self.total_width
    }
}

fn align_offset(anchor_x: f32, total_width: f32, align: vge::command::Align) -> f32 {
    match align {
        vge::command::Align::Left => anchor_x,
        vge::command::Align::Center => anchor_x - total_width * 0.5,
        vge::command::Align::Right => anchor_x - total_width,
    }
}

/// Build the textured quad for one rasterised glyph and append it to
/// the appropriate batch (color vs alpha) keyed by atlas texture.
fn push_glyph_quad(
    alpha_batches: &mut HashMap<usize, Vec<Quad>>,
    color_batches: &mut HashMap<usize, Vec<Quad>>,
    rendered: RenderedGlyph,
    pen_x: f32,
    pen_y: f32,
) {
    let it = 1.0 / TEXTURE_SIZE as f32;
    let mut q = Quad::default();
    // Snap to whole pixels on both axes. VGE coordinates are f32 and
    // sub-cell (`vector-graphics-extension.md` §5.1), and `align_offset`
    // can halve an odd width, so a quad edge can otherwise land
    // mid-pixel; the glyphs are hinted at integer positions (`hint(true)`
    // above) so a fractional quad buys no extra fidelity, it only blurs
    // the raster and risks the half-pixel texel-boundary case the
    // GLYPH_GUTTER guards. Both axes matter: y carries the font's
    // fractional ascent, so it is only incidentally safe per-font.
    q.x0 = (pen_x + rendered.offset_x as f32).round();
    q.y0 = (pen_y - rendered.offset_y as f32).round();
    q.x1 = q.x0 + rendered.width as f32;
    q.y1 = q.y0 + rendered.height as f32;
    q.s0 = rendered.atlas_x as f32 * it;
    q.t0 = rendered.atlas_y as f32 * it;
    q.s1 = (rendered.atlas_x + rendered.width) as f32 * it;
    q.t1 = (rendered.atlas_y + rendered.height) as f32 * it;
    if rendered.color_glyph {
        color_batches
            .entry(rendered.texture_index)
            .or_default()
            .push(q);
    } else {
        alpha_batches
            .entry(rendered.texture_index)
            .or_default()
            .push(q);
    }
}

/// Drain alpha + color glyph batches to the canvas with one
/// `draw_glyph_commands` call per group.
fn emit_glyph_batches<T: Renderer>(
    canvas: &mut Canvas<T>,
    glyph_cache: &GlyphCache,
    alpha_batches: HashMap<usize, Vec<Quad>>,
    color_batches: HashMap<usize, Vec<Quad>>,
    color: Color,
) {
    if !alpha_batches.is_empty() {
        let cmds: Vec<DrawCommand> = alpha_batches
            .into_iter()
            .map(|(tex_idx, quads)| DrawCommand {
                image_id: glyph_cache.textures[tex_idx].image_id,
                quads,
            })
            .collect();
        canvas.draw_glyph_commands(
            GlyphDrawCommands {
                alpha_glyphs: cmds,
                color_glyphs: vec![],
            },
            &Paint::color(color),
        );
    }
    if !color_batches.is_empty() {
        let cmds: Vec<DrawCommand> = color_batches
            .into_iter()
            .map(|(tex_idx, quads)| DrawCommand {
                image_id: glyph_cache.textures[tex_idx].image_id,
                quads,
            })
            .collect();
        canvas.draw_glyph_commands(
            GlyphDrawCommands {
                alpha_glyphs: vec![],
                color_glyphs: cmds,
            },
            &Paint::color(Color::white()),
        );
    }
}

// --- Terminal renderer ---

pub struct TerminalRenderer {
    // Primary font
    font_data: Vec<u8>,
    font_index: usize,
    /// Family name as advertised in the primary font's `name` table.
    /// Used as the FontStack base for VGE styled text so that
    /// bold/italic resolve from the same family the unstyled cell
    /// renderer uses.
    font_family: String,

    // Font fallback (separate fields for disjoint borrowing)
    font_cx: FontContext,
    layout_cx: LayoutContext<Color>,
    fallback_fonts: Vec<FallbackFont>,
    /// Families consulted for a character the primary font lacks:
    /// the configured `[font] fallback` list.
    fallback_families: Vec<String>,
    /// Families Fontconfig named for a PUA codepoint earlier in this
    /// session, so one icon set costs one query. PUA lookups only.
    pua_families: Vec<String>,
    char_font_map: HashMap<char, Option<ResolvedGlyph>>,

    // Rendering
    font_size: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    ascent: f32,
    scale_cx: ScaleContext,
    glyph_cache: GlyphCache,

    // VGE image bookkeeping. The host engines store `GpuImageId`
    // (opaque, renderer-defined); the renderer maintains the
    // mapping from those to its own GPU texture handles so the
    // engine state stays GUI-free.
    gpu_image_handles: HashMap<crate::vge::GpuImageId, femtovg::ImageId>,
    next_gpu_image_id: u64,

    /// Pointer hit-test index for VGE content, rebuilt by every render
    /// pass (see `vge::pick`). Public because the event loop reads it
    /// on the next pointer event.
    pub pick: crate::vge::pick::PickList,

    // Search-chrome colors. Configurable via the user's config
    // (`[search]`); default to the values that were hardcoded here
    // before the config existed. Set via `set_search_colors`.
    search_accent: Color,
    search_bar_text: Color,
    search_current_match: Color,
    search_match: Color,
    /// Outline colour for a selected VGE image. Defaults to the
    /// built-in accent slot 0; `set_selection_accent` overrides it from
    /// `[accent]`.
    selection_accent: Color,

    /// The user's theme. Backs `palette` (the grid) and the chrome
    /// accessors below (the search panel and the close prompt), so
    /// every colour veter paints of its own comes from one place. The
    /// built-in default until `set_theme`.
    theme: Theme,

    /// The colour table, as OSC 4 / 10 / 11 / 12 have left it.
    pub palette: Palette,

    /// The on half of the blink cycle, driven by the App's clock. Both
    /// `SGR 5` text and a blinking cursor read it, so everything on
    /// screen blinks in step.
    pub blink_phase: bool,
    /// Set by a render pass that painted something blinking, so the App
    /// knows to schedule the next frame. Cleared by
    /// [`take_blink_seen`](Self::take_blink_seen).
    blink_seen: bool,

    /// Bold / italic / bold-italic faces of the primary family,
    /// resolved on first use and indexed by `bold | italic << 1`. The
    /// outer `Option` is "not looked up yet"; the inner one is `None`
    /// when the family has no distinct face and the primary is used
    /// as-is.
    styled_faces: [Option<Option<u16>>; 4],
}

impl TerminalRenderer {
    pub fn new<T: Renderer>(
        _canvas: &mut Canvas<T>,
        font_size: f32,
        font: FontSpec,
    ) -> Self {
        let mut font_cx = FontContext::new();
        let mut layout_cx = LayoutContext::new();

        let sample = "X";
        let mut builder = layout_cx.ranged_builder(&mut font_cx, sample, 1.0, false);
        let requested = if font.family.trim().is_empty() {
            "monospace"
        } else {
            font.family.trim()
        };
        builder.push_default(FontStack::from(requested));
        builder.push_default(StyleProperty::FontSize(font_size));
        let mut layout: Layout<Color> = builder.build(sample);
        layout.break_all_lines(None);
        layout.align(None, Alignment::Start, AlignmentOptions::default());

        let mut font_data = Vec::new();
        let mut font_index = 0usize;
        let mut font_family = String::new();
        let mut cell_width = (font_size * 0.6).ceil();
        let mut cell_height = (font_size * 1.2).ceil();
        let mut ascent = font_size;

        if let Some(glyph_run) = layout.lines().next().and_then(|line| {
            line.items().find_map(|item| match item {
                PositionedLayoutItem::GlyphRun(g) => Some(g),
                _ => None,
            })
        }) {
            let run = glyph_run.run();
            let font = run.font();
            font_data = font.data.as_ref().to_vec();
            font_index = font.index as usize;

            let font_ref = FontRef::from_index(&font_data, font_index).unwrap();
            let metrics = font_ref.metrics(&[]).scale(font_size);
            ascent = metrics.ascent;
            // Match Konsole / kitty / alacritty: cell height excludes
            // font-supplied leading. Including leading widens line
            // spacing visibly versus what users expect from a terminal.
            cell_height = (metrics.ascent + metrics.descent).ceil();

            let glyph_metrics = font_ref.glyph_metrics(&[]).scale(font_size);
            let charmap = font_ref.charmap();
            let m_glyph = charmap.map('M');
            cell_width = glyph_metrics.advance_width(m_glyph).ceil();

            if let Some(name) = font_ref
                .localized_strings()
                .find_by_id(StringId::Family, None)
            {
                font_family = name.to_string();
            }
        }

        eprintln!(
            "Font: requested={:?} family={:?} cell={}x{}, ascent={}, size={}",
            requested, font_family, cell_width, cell_height, ascent, font_size
        );

        Self {
            font_data,
            font_index,
            font_family,
            font_cx,
            layout_cx,
            fallback_fonts: Vec::new(),
            fallback_families: font.fallback,
            pua_families: Vec::new(),
            char_font_map: HashMap::new(),
            font_size,
            cell_width,
            cell_height,
            ascent,
            scale_cx: ScaleContext::new(),
            glyph_cache: GlyphCache::new(),
            gpu_image_handles: HashMap::new(),
            next_gpu_image_id: 0,
            pick: crate::vge::pick::PickList::new(),
            search_accent: Color::rgb(0x56, 0x79, 0x9f),
            search_bar_text: Color::rgb(230, 230, 230),
            search_current_match: Color::rgb(220, 160, 0),
            search_match: Color::rgb(80, 80, 30),
            selection_accent: Color::rgb(0x56, 0x79, 0x9f),
            theme: Theme::default(),
            palette: Palette::default(),
            blink_phase: true,
            blink_seen: false,
            styled_faces: [None; 4],
        }
    }

    /// Override the VGE selection outline colour from user config.
    /// Whether the frame just drawn contained anything that blinks, so
    /// the caller knows to come back for the other half of the cycle.
    pub fn take_blink_seen(&mut self) -> bool {
        std::mem::take(&mut self.blink_seen)
    }

    /// The faces to draw bold / italic grid cells with, indexed by
    /// `bold | italic << 1` and resolved out of the primary font's own
    /// family so they share its metrics.
    ///
    /// `None` in a slot means the family offers no distinct face there
    /// and the primary should be used — which is what every cell used
    /// to get, bold being nothing but a brighter colour and italic
    /// nothing at all. Resolved once, on the first frame that asks.
    fn grid_styled_faces(&mut self) -> [Option<u16>; 4] {
        if self.styled_faces[0].is_none() {
            for slot in 0..4 {
                let resolved =
                    self.resolve_styled_face(slot & 1 != 0, slot & 2 != 0);
                self.styled_faces[slot] = Some(resolved);
            }
        }
        // Slot 0 is the unstyled face, which is the primary by
        // definition, so it is always `Some(None)` once resolved.
        std::array::from_fn(|i| self.styled_faces[i].flatten())
    }

    fn resolve_styled_face(&mut self, bold: bool, italic: bool) -> Option<u16> {
        use parley::style::{FontStyle as PStyle, FontWeight};

        let cell = self.cell_metrics();
        let mut builder =
            self.layout_cx
                .ranged_builder(&mut self.font_cx, "m", 1.0, false);
        builder.push_default(StyleProperty::Brush(Color::white()));
        let stack: FontStack<'_> = if self.font_family.is_empty() {
            FontStack::from(GenericFamily::Monospace)
        } else {
            FontStack::List(Cow::Owned(vec![
                FontFamily::Named(Cow::Borrowed(self.font_family.as_str())),
                FontFamily::Generic(GenericFamily::Monospace),
            ]))
        };
        builder.push_default(stack);
        builder.push_default(StyleProperty::FontSize(self.font_size));
        builder.push_default(StyleProperty::FontWeight(if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        }));
        builder.push_default(StyleProperty::FontStyle(if italic {
            PStyle::Italic
        } else {
            PStyle::Normal
        }));
        let mut layout: Layout<Color> = builder.build("m");
        layout.break_all_lines(None);

        let mut face: Option<(Vec<u8>, usize)> = None;
        'outer: for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(run_layout) = item {
                    let font = run_layout.run().font();
                    face = Some((
                        font.data.as_ref().to_vec(),
                        font.index as usize,
                    ));
                    break 'outer;
                }
            }
        }
        let (data, index) = face?;
        // A family with no bold or no italic of its own resolves right
        // back to the regular face. Going through the fallback path for
        // that would cost a second glyph cache for identical output.
        if index == self.font_index && data == self.font_data {
            return None;
        }
        Some(register_fallback(
            &mut self.fallback_fonts,
            &data,
            index,
            cell,
        ))
    }

    /// Adopt the user's theme: it becomes the grid palette's base (see
    /// [`Palette::with_theme`]) and the source of every chrome colour
    /// below. Called once at startup, before `set_search_colors` and
    /// `set_selection_accent` layer the `[search]` / `[accent]`
    /// sections over the values it implies.
    pub fn set_theme(&mut self, theme: Theme) {
        self.palette = Palette::with_theme(&theme);
        self.search_accent = theme.search_accent().to_femto();
        self.search_bar_text = theme.search_text().to_femto();
        self.search_current_match = theme.search_current_match().to_femto();
        self.search_match = theme.search_match().to_femto();
        self.selection_accent = theme.accent_primary().to_femto();
        self.theme = theme;
    }

    /// Background of an overlay panel — the search panel, the close
    /// prompt.
    pub fn panel_bg(&self) -> Color {
        self.theme.surface().to_femto()
    }

    /// A recessed surface inside a panel: the query field, and a chip
    /// that is switched off.
    pub fn panel_inset_bg(&self) -> Color {
        self.theme.surface_inset().to_femto()
    }

    /// Primary text on [`Self::panel_bg`].
    pub fn panel_text(&self) -> Color {
        self.theme.text().to_femto()
    }

    /// Text on an accent-filled chip or button.
    pub fn on_accent_text(&self) -> Color {
        self.theme.text_on_accent().to_femto()
    }

    /// The warm trio — fill, outline, text — for the answer that gets
    /// you nothing: the prompt's Quit button, the search panel's
    /// `no matches`.
    pub fn warn_colors(&self) -> (Color, Color, Color) {
        (
            self.theme.warn_fill().to_femto(),
            self.theme.warn_border().to_femto(),
            self.theme.warn_text().to_femto(),
        )
    }

    pub fn set_selection_accent(&mut self, accent: Color) {
        self.selection_accent = accent;
    }

    /// Outline tint for a selected VGE image (`vge::render`).
    pub fn selection_accent(&self) -> Color {
        self.selection_accent
    }

    /// Override the search-chrome colors from user config. Called once
    /// after construction; the defaults above stand in until then.
    pub fn set_search_colors(
        &mut self,
        accent: Color,
        bar_text: Color,
        current_match: Color,
        other_match: Color,
    ) {
        self.search_accent = accent;
        self.search_bar_text = bar_text;
        self.search_current_match = current_match;
        self.search_match = other_match;
    }

    /// Accent tint for the search panel's chrome — its border, the caret
    /// and the chip fills (`draw_search_bar`). Defaults to the first
    /// `[accent]` slot, so a palette change restyles the panel;
    /// `[search] accent` overrides it.
    pub fn search_accent(&self) -> Color {
        self.search_accent
    }

    /// Search-bar text color (for `draw_search_bar`).
    pub fn search_bar_text(&self) -> Color {
        self.search_bar_text
    }

    /// Colour of the active match in the grid. The search panel's match
    /// counter is drawn in it too, so the count and the highlight it
    /// points at share a colour.
    pub fn search_current_match(&self) -> Color {
        self.search_current_match
    }

    /// Allocate a fresh `GpuImageId` and record the renderer-side
    /// femtovg handle it maps to.
    pub fn register_gpu_image(
        &mut self,
        femto_id: femtovg::ImageId,
    ) -> crate::vge::GpuImageId {
        let gpu = crate::vge::GpuImageId(self.next_gpu_image_id);
        self.next_gpu_image_id += 1;
        self.gpu_image_handles.insert(gpu, femto_id);
        gpu
    }

    /// Look up the femtovg handle for a `GpuImageId`, if registered.
    pub fn lookup_gpu_image(
        &self,
        gpu: crate::vge::GpuImageId,
    ) -> Option<femtovg::ImageId> {
        self.gpu_image_handles.get(&gpu).copied()
    }

    /// Release a renderer-side image. The host engine drains its
    /// `pending_image_deletes` queue and asks the renderer to free
    /// each entry; the renderer translates back to its private
    /// femtovg handle and calls `delete_image`.
    pub fn release_gpu_image<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        gpu: crate::vge::GpuImageId,
    ) {
        if let Some(femto_id) = self.gpu_image_handles.remove(&gpu) {
            canvas.delete_image(femto_id);
        }
    }

    pub fn terminal_size(&self, width: u32, height: u32) -> (u16, u16) {
        let cols = (width as f32 / self.cell_width).floor() as u16;
        let rows = (height as f32 / self.cell_height).floor() as u16;
        (cols.max(1), rows.max(1))
    }

    pub fn ascent(&self) -> f32 {
        self.ascent
    }

    /// Resolve a single character to (glyph_id, font_id), using the primary
    /// font when possible and falling back to Parley-discovered fonts.
    fn resolve_glyph(&mut self, ch: char) -> Option<(u16, u16)> {
        let primary_ref = FontRef::from_index(&self.font_data, self.font_index).unwrap();
        let gid = primary_ref.charmap().map(ch);
        if gid != 0 {
            return Some((gid, 0));
        }
        let cell = self.cell_metrics();
        let resolved = resolve_fallback(
            &mut self.font_cx,
            &mut self.layout_cx,
            &mut self.fallback_fonts,
            &mut self.char_font_map,
            &self.fallback_families,
            &mut self.pua_families,
            ch,
            cell,
        )?;
        Some((resolved.glyph_id, resolved.font_id))
    }

    /// The primary's cell, as fallback faces are fitted to it.
    fn cell_metrics(&self) -> CellMetrics {
        CellMetrics {
            width: self.cell_width,
            height: self.cell_height,
            size: self.font_size,
        }
    }

    /// Rasterisation size for `font_id` in the grid: a fallback face is
    /// shrunk to fit the cell, the primary is already the cell.
    /// `span` is the character's column count, so a double-width glyph
    /// is fitted to the two cells it actually occupies.
    fn grid_raster_size(&self, font_id: u16, span: f32) -> f32 {
        if font_id == 0 {
            return self.font_size;
        }
        let fb = &self.fallback_fonts[(font_id - 1) as usize];
        self.font_size * (fb.width_fit * span).min(fb.height_fit)
    }

    fn font_ref_for(&self, font_id: u16) -> FontRef<'_> {
        if font_id == 0 {
            FontRef::from_index(&self.font_data, self.font_index).unwrap()
        } else {
            let fb = &self.fallback_fonts[(font_id - 1) as usize];
            FontRef::from_index(&fb.data, fb.index).unwrap()
        }
    }

    /// Draw arbitrary text at a pixel-baseline coordinate, with alignment.
    /// Used by VGE DrawText (§7.4). Bold and italic both route through
    /// a Parley layout pass so the system's actual styled font face
    /// gets resolved; plain text uses the cell renderer's faster
    /// per-char path. Underline and strikethrough are applied as
    /// horizontal rules over the rendered glyphs.
    ///
    /// Returns the run's horizontal extent so the caller can record a
    /// hit-test box for it (`vge::pick`).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_vge_text<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        x_px: f32,
        y_px: f32,
        text: &str,
        color: Color,
        align: vge::command::Align,
        font_style: vge::command::FontStyle,
        scale: f32,
    ) -> TextExtent {
        self.draw_vge_text_selected(
            canvas, x_px, y_px, text, color, align, font_style, scale, None,
        )
        .extent
    }

    /// [`Self::draw_vge_text`] with a byte range drawn selected.
    ///
    /// The selected span gets the grid's reverse-video treatment rather
    /// than a translucent wash: the run paints normally, a bar in the
    /// text's own colour covers the selected span, and the run is
    /// redrawn in the terminal background colour scissored to that bar.
    /// Both passes come off the same [`TextLayout`], so the highlight
    /// cannot land half a glyph away from the text it marks.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_vge_text_selected<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        x_px: f32,
        y_px: f32,
        text: &str,
        color: Color,
        align: vge::command::Align,
        font_style: vge::command::FontStyle,
        scale: f32,
        selected: Option<(usize, usize)>,
    ) -> TextRun {
        if text.is_empty() {
            return TextRun {
                extent: TextExtent {
                    start_x: x_px,
                    total_width: 0.0,
                },
                stops: vec![(0, 0.0)],
            };
        }

        let layout = self.layout_vge_text(text, x_px, align, font_style, scale);
        let extent = layout.extent();
        // The colour a selected span's glyphs are redrawn in: the
        // terminal's own background, so VGE text reverse-videos against
        // the same ground a selected grid cell does — theme included.
        let ground = self.palette.bg();
        self.draw_run(canvas, &layout, y_px, scale, font_style, color);

        if let Some((start, end)) = selected
            && end > start
        {
            let x0 = layout.x_of_byte(start);
            let x1 = layout.x_of_byte(end);
            if x1 > x0 {
                let top = y_px - self.ascent * scale;
                let height = self.cell_height * scale;
                let mut bar = Path::new();
                bar.rect(x0, top, x1 - x0, height);
                canvas.fill_path(&bar, &Paint::color(color));

                canvas.save();
                canvas.intersect_scissor(x0, top, x1 - x0, height);
                self.draw_run(canvas, &layout, y_px, scale, font_style, ground);
                canvas.restore();
            }
        }

        TextRun {
            extent,
            stops: layout.stops,
        }
    }

    /// Glyphs plus any underline / strikethrough rules, in one colour.
    fn draw_run<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        layout: &TextLayout,
        y_px: f32,
        scale: f32,
        font_style: vge::command::FontStyle,
        color: Color,
    ) {
        self.draw_text_layout(canvas, layout, y_px, color);

        if font_style.underline() || font_style.strikethrough() {
            let mut path = Path::new();
            let thickness = (layout.font_px / 16.0).max(1.0);
            if font_style.underline() {
                let uy = y_px + (self.cell_height - self.ascent) * 0.5 * scale;
                path.rect(layout.start_x, uy, layout.total_width, thickness);
            }
            if font_style.strikethrough() {
                let sy = y_px - self.ascent * 0.35 * scale;
                path.rect(layout.start_x, sy, layout.total_width, thickness);
            }
            canvas.fill_path(&path, &Paint::color(color));
        }
    }

    /// Shape one VGE text run: resolve its glyphs, measure it, and
    /// record where each character boundary falls.
    ///
    /// Both the draw path and pointer hit-testing go through this, so
    /// the character a click resolves to is by construction the one
    /// that was painted there — there is no second measurement to drift
    /// out of step. `x_px` is the run's anchor in device pixels and
    /// `align` decides which edge of the run it pins (§7.4).
    pub fn layout_vge_text(
        &mut self,
        text: &str,
        x_px: f32,
        align: vge::command::Align,
        font_style: vge::command::FontStyle,
        scale: f32,
    ) -> TextLayout {
        // `scale` is the element's composed on-screen scale (VGE §9.11).
        // We shape and rasterise at `font_size · scale` so a zoomed-in
        // text element is drawn crisp at its final pixel size rather
        // than magnified from a cell-size atlas.
        let font_px = self.font_size * scale;
        if font_style.bold() || font_style.italic() {
            self.layout_text_styled(text, x_px, align, font_style, font_px)
        } else {
            self.layout_text_plain(text, x_px, align, font_px)
        }
    }

    /// Per-char shaping for plain (no bold/italic) text. Reuses the
    /// cell renderer's primary font + fallback chain.
    fn layout_text_plain(
        &mut self,
        text: &str,
        x_px: f32,
        align: vge::command::Align,
        font_px: f32,
    ) -> TextLayout {
        let mut glyphs: Vec<PlainGlyph> = Vec::with_capacity(text.len());
        let mut stops: Vec<(usize, f32)> = Vec::with_capacity(text.len() + 1);
        let mut w = 0.0f32;
        for (byte_off, ch) in text.char_indices() {
            // Every character gets a stop, including ones with no glyph:
            // they still occupy bytes, and a boundary list with holes in
            // it would let a click land on an offset that can't be sliced.
            stops.push((byte_off, w));
            let Some((glyph_id, font_id)) = self.resolve_glyph(ch) else {
                continue;
            };
            let advance = self
                .font_ref_for(font_id)
                .glyph_metrics(&[])
                .scale(font_px)
                .advance_width(glyph_id);
            glyphs.push(PlainGlyph {
                ch,
                glyph_id,
                font_id,
                x: w,
            });
            w += advance;
        }
        stops.push((text.len(), w));

        TextLayout {
            start_x: align_offset(x_px, w, align),
            total_width: w,
            font_px,
            stops,
            glyphs: LayoutGlyphs::Plain(glyphs),
        }
    }

    /// Bold/italic-capable shaping via Parley. Asks Parley to resolve a
    /// font face matching the requested weight and slant, walks the
    /// resulting runs, and registers each face as a fallback font so
    /// the glyph cache can key on it. Different faces (regular vs bold
    /// vs italic) end up under distinct `font_id`s and so cache
    /// independently.
    fn layout_text_styled(
        &mut self,
        text: &str,
        x_px: f32,
        align: vge::command::Align,
        font_style: vge::command::FontStyle,
        font_px: f32,
    ) -> TextLayout {
        use parley::style::{FontStyle as PStyle, FontWeight};

        let weight = if font_style.bold() {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        let pstyle = if font_style.italic() {
            PStyle::Italic
        } else {
            PStyle::Normal
        };

        let mut builder = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, text, 1.0, false);
        builder.push_default(StyleProperty::Brush(Color::white()));
        let stack: FontStack<'_> = if self.font_family.is_empty() {
            FontStack::from(GenericFamily::Monospace)
        } else {
            FontStack::List(Cow::Owned(vec![
                FontFamily::Named(Cow::Borrowed(self.font_family.as_str())),
                FontFamily::Generic(GenericFamily::Monospace),
            ]))
        };
        builder.push_default(stack);
        builder.push_default(StyleProperty::FontSize(font_px));
        builder.push_default(StyleProperty::FontWeight(weight));
        builder.push_default(StyleProperty::FontStyle(pstyle));
        let mut layout: Layout<Color> = builder.build(text);
        layout.break_all_lines(None);
        layout.align(None, Alignment::Start, AlignmentOptions::default());

        let total_width = layout.width();
        let cell = self.cell_metrics();

        // Walk runs, registering fonts and collecting per-glyph info.
        // Cluster boundaries come off the same runs, so the stop list
        // and the glyphs agree about where each character sits.
        let mut glyphs: Vec<StyledGlyph> = Vec::new();
        let mut stops: Vec<(usize, f32)> = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(run_layout) = item {
                    let run = run_layout.run();
                    let font = run.font();
                    let data_ref = font.data.as_ref();
                    let font_index = font.index as usize;
                    let font_id =
                        register_fallback(
                            &mut self.fallback_fonts,
                            data_ref,
                            font_index,
                            cell,
                        );

                    let mut cluster_x = run_layout.offset();
                    for cluster in run.clusters() {
                        stops.push((cluster.text_range().start, cluster_x));
                        cluster_x += cluster.advance();
                    }

                    // Parley's `glyphs()` returns un-positioned glyphs
                    // — each `glyph.x` is a per-glyph offset (kerning
                    // / cluster nudge), `glyph.advance` is the step to
                    // the next glyph, and `glyph.y` is the offset from
                    // the run's baseline. We accumulate the pen
                    // ourselves so the position we hand to the renderer
                    // is in baseline coordinates (matches how the
                    // per-char plain path computes positions).
                    let mut pen_x = run_layout.offset();
                    for glyph in run_layout.glyphs() {
                        glyphs.push(StyledGlyph {
                            x: pen_x + glyph.x,
                            y: glyph.y,
                            glyph_id: glyph.id as u16,
                            font_id,
                        });
                        pen_x += glyph.advance;
                    }
                }
            }
        }
        // Runs arrive in visual order, which is not byte order for
        // bidi text. Sorting keeps the list bisectable; a genuinely
        // RTL run still maps a click to the wrong end, which is the
        // same approximation §7.4's single-line model already makes.
        stops.sort_by_key(|(off, _)| *off);
        stops.push((text.len(), total_width));

        TextLayout {
            start_x: align_offset(x_px, total_width, align),
            total_width,
            font_px,
            stops,
            glyphs: LayoutGlyphs::Styled(glyphs),
        }
    }

    /// Rasterise an already-shaped run at a pixel baseline.
    fn draw_text_layout<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        layout: &TextLayout,
        y_px: f32,
        color: Color,
    ) {
        let start_x = layout.start_x;
        let font_px = layout.font_px;
        let mut alpha_batches: HashMap<usize, Vec<Quad>> = HashMap::new();
        let mut color_batches: HashMap<usize, Vec<Quad>> = HashMap::new();

        match &layout.glyphs {
            LayoutGlyphs::Plain(glyphs) => {
                for g in glyphs {
                    if g.ch == ' ' {
                        continue;
                    }
                    let rendered = if g.font_id == 0 {
                        let fr = FontRef::from_index(&self.font_data, self.font_index).unwrap();
                        self.glyph_cache.get_or_render(
                            canvas,
                            &mut self.scale_cx,
                            fr,
                            g.glyph_id,
                            font_px,
                            0,
                        )
                    } else {
                        let fb = &self.fallback_fonts[(g.font_id - 1) as usize];
                        let fr = FontRef::from_index(&fb.data, fb.index).unwrap();
                        self.glyph_cache.get_or_render(
                            canvas,
                            &mut self.scale_cx,
                            fr,
                            g.glyph_id,
                            font_px,
                            g.font_id,
                        )
                    };
                    let Some(rendered) = rendered else { continue };
                    push_glyph_quad(
                        &mut alpha_batches,
                        &mut color_batches,
                        rendered,
                        start_x + g.x,
                        y_px,
                    );
                }
            }
            LayoutGlyphs::Styled(glyphs) => {
                for g in glyphs {
                    let fb = &self.fallback_fonts[(g.font_id - 1) as usize];
                    let fr = FontRef::from_index(&fb.data, fb.index).unwrap();
                    let rendered = self.glyph_cache.get_or_render(
                        canvas,
                        &mut self.scale_cx,
                        fr,
                        g.glyph_id,
                        font_px,
                        g.font_id,
                    );
                    let Some(rendered) = rendered else { continue };
                    push_glyph_quad(
                        &mut alpha_batches,
                        &mut color_batches,
                        rendered,
                        start_x + g.x,
                        y_px + g.y,
                    );
                }
            }
        }

        emit_glyph_batches(canvas, &self.glyph_cache, alpha_batches, color_batches, color);
    }

    /// Draw the cells of `screen` into the canvas at the given pixel
    /// origin. `focused_cursor` names the cell that should render with
    /// inverted foreground/background (the focused cursor look); if
    /// `None`, no cell is inverted.
    ///
    /// The host render path passes `Some(host_cursor_pos)` when the
    /// cursor is visible and the user isn't scrolled back; portal
    /// rendering passes `None` because portal cursors are drawn
    /// separately by `prt::render` (so the unfocused-style policy
    /// from §9.2 can apply).
    /// `scroll_offset` is the scrollback offset to read `screen` at, in
    /// rows above the live region. For the host grid that is the grid's
    /// own offset; for a PRT portal it is the *view's* offset, since two
    /// forked views share one buffer and scroll independently — the
    /// buffer itself stays live and never moves.
    pub fn draw_screen_at<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        screen: &vt100::Screen,
        scroll_offset: usize,
        ox_px: f32,
        oy_px: f32,
        focused_cursor: Option<(u16, u16)>,
        selection: Option<&SelectionRange>,
        search_highlights: Option<&[HighlightSpan]>,
    ) {
        let (rows, cols) = screen.size();
        let default_bg = self.palette.bg();
        // DECSCNM (`?5`) — terminfo's `flash` is a tenth of a second of
        // this.
        let reverse_video = screen.reverse_video();
        // DECSCUSR (`CSI Ps SP q`). The shape belongs to the screen
        // being drawn, so a portal running vim gets a bar while the
        // host keeps its block, with no extra plumbing.
        let cursor_shape = screen.cursor_shape();
        let cursor_lit = !screen.cursor_blink() || self.blink_phase;
        if focused_cursor.is_some() && screen.cursor_blink() {
            self.blink_seen = true;
        }
        // Only a block cursor is drawn by inverting its cell; a bar or
        // an underline is a rule painted over one, so the cell beneath
        // keeps its own colours.
        let block_cursor =
            matches!(cursor_shape, vt100::CursorShape::Block) && cursor_lit;
        let cursor_cell =
            |row: u16, col: u16| block_cursor && focused_cursor == Some((row, col));
        // Resolving a styled face borrows the whole renderer, so it has
        // to happen before the primary `FontRef` below pins `font_data`
        // for the rest of the pass. All four go at once on the first
        // frame — the alternative, resolving on demand, would need a
        // scan of the grid every frame to find out which are wanted.
        let styled_faces = self.grid_styled_faces();
        let palette = self.palette.clone();
        let selected = |r, c| selection.map(|s| s.contains(r, c)).unwrap_or(false);
        // Per-cell search-highlight color, or None if cell is unhit.
        // The current match takes precedence over other matches on the
        // same cell so n/N reliably colors the active hit even if it
        // happens to overlap another (e.g. very short query).
        let current_match = self.search_current_match;
        let other_match = self.search_match;
        let highlight_color = |r: u16, c: u16| -> Option<Color> {
            let spans = search_highlights?;
            let mut color: Option<Color> = None;
            for span in spans {
                if span.row == r && c >= span.col_start && c < span.col_end {
                    if span.is_current {
                        return Some(current_match);
                    }
                    color = Some(other_match);
                }
            }
            color
        };

        // Cell backgrounds.
        for row in 0..rows {
            for col in 0..cols {
                let cell = match screen.cell_at(scroll_offset, row, col) {
                    Some(c) => c,
                    None => continue,
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let is_cursor = cursor_cell(row, col);
                let sel = selected(row, col);
                let hl = highlight_color(row, col);
                let (_, base_bg) =
                    resolve_cell_colors(cell, is_cursor, sel, &palette, reverse_video);
                // Search highlight overrides everything except cursor/selection.
                // Selection wins over a non-current match so the user's
                // explicit selection stays visible.
                let bg = if let Some(hc) = hl
                    && !sel
                    && !is_cursor
                {
                    hc
                } else {
                    base_bg
                };
                let w = if cell.is_wide() { 2.0 } else { 1.0 };
                // Selected / highlighted cells need a bg fill even when
                // the underlying cell uses the default bg, so the
                // overlay is visible.
                if bg != default_bg || sel || hl.is_some() {
                    let x = ox_px + col as f32 * self.cell_width;
                    let y = oy_px + row as f32 * self.cell_height;
                    let mut path = Path::new();
                    path.rect(x, y, self.cell_width * w, self.cell_height);
                    canvas.fill_path(&path, &Paint::color(bg));
                }
            }
        }

        // Glyphs.
        let primary_ref = FontRef::from_index(&self.font_data, self.font_index).unwrap();
        let primary_charmap = primary_ref.charmap();
        let mut alpha_batches: HashMap<u32, HashMap<usize, Vec<Quad>>> = HashMap::new();
        let mut color_batches: HashMap<usize, Vec<Quad>> = HashMap::new();

        let cell_metrics = self.cell_metrics();
        for row in 0..rows {
            for col in 0..cols {
                let cell = match screen.cell_at(scroll_offset, row, col) {
                    Some(c) => c,
                    None => continue,
                };
                if cell.is_wide_continuation() || !cell.has_contents() {
                    continue;
                }
                let ch = match cell.contents().chars().next() {
                    Some(c) if c > ' ' => c,
                    _ => continue,
                };

                // Box-drawing (U+2500..U+257F) and block elements
                // (U+2580..U+259F) tile seamlessly only when drawn as
                // primitives; the font glyphs leave gaps because the
                // cell box includes leading and weights are inconsistent.
                // Short-circuit before the font lookup.
                let is_cursor = cursor_cell(row, col);
                let (fg, _) = resolve_cell_colors(
                    cell,
                    is_cursor,
                    selected(row, col),
                    &palette,
                    reverse_video,
                );
                // SGR 5, terminfo `blink`: skip the glyph on the off
                // half of the cycle. The cell's background, decorations
                // and any selection stay put — only the text winks.
                if cell.blink() {
                    self.blink_seen = true;
                    if !self.blink_phase {
                        continue;
                    }
                }
                let cx = ox_px + col as f32 * self.cell_width;
                let cy = oy_px + row as f32 * self.cell_height;
                let code = ch as u32;
                let drawn = if (0x2500..=0x257F).contains(&code) {
                    try_draw_box_drawing(
                        canvas, ch, cx, cy, self.cell_width, self.cell_height, fg,
                    )
                } else if (0x2580..=0x259F).contains(&code) {
                    try_draw_block_element(
                        canvas, ch, cx, cy, self.cell_width, self.cell_height, fg,
                    )
                } else if (0xE0B0..=0xE0B7).contains(&code) {
                    try_draw_powerline(
                        canvas, ch, cx, cy, self.cell_width, self.cell_height, fg,
                    )
                } else {
                    false
                };
                if drawn {
                    continue;
                }

                // Bold and italic draw from the family's own faces.
                // Until these existed, bold was nothing but a brighter
                // colour and italic was nothing at all.
                let styled = if cell.bold() || cell.italic() {
                    styled_faces[usize::from(cell.bold())
                        | (usize::from(cell.italic()) << 1)]
                } else {
                    None
                };
                let styled_glyph = styled.and_then(|fid| {
                    let fb = &self.fallback_fonts[(fid - 1) as usize];
                    let fr = FontRef::from_index(&fb.data, fb.index)?;
                    let gid = fr.charmap().map(ch);
                    (gid != 0).then_some((gid, fid))
                });

                let (glyph_id, font_id) = if let Some(sg) = styled_glyph {
                    sg
                } else {
                    let gid = primary_charmap.map(ch);
                    if gid != 0 {
                        (gid, 0u16)
                    } else {
                        match resolve_fallback(
                            &mut self.font_cx,
                            &mut self.layout_cx,
                            &mut self.fallback_fonts,
                            &mut self.char_font_map,
                            &self.fallback_families,
                            &mut self.pua_families,
                            ch,
                            cell_metrics,
                        ) {
                            Some(rg) => (rg.glyph_id, rg.font_id),
                            None => continue,
                        }
                    }
                };

                let x = cx;
                let y = cy + self.ascent;

                // A fallback face is rasterised small enough to stay
                // inside the cells this character occupies; the primary
                // defines them, so it is drawn as-is.
                let span = if cell.is_wide() { 2.0 } else { 1.0 };
                let raster_size = self.grid_raster_size(font_id, span);
                let rendered = if font_id == 0 {
                    let fr = FontRef::from_index(&self.font_data, self.font_index).unwrap();
                    self.glyph_cache.get_or_render(
                        canvas,
                        &mut self.scale_cx,
                        fr,
                        glyph_id,
                        raster_size,
                        0,
                    )
                } else {
                    let fb = &self.fallback_fonts[(font_id - 1) as usize];
                    let fr = FontRef::from_index(&fb.data, fb.index).unwrap();
                    self.glyph_cache.get_or_render(
                        canvas,
                        &mut self.scale_cx,
                        fr,
                        glyph_id,
                        raster_size,
                        font_id,
                    )
                };
                let rendered = match rendered {
                    Some(r) => r,
                    None => continue,
                };

                let it = 1.0 / TEXTURE_SIZE as f32;
                let mut q = Quad::default();
                q.x0 = x + rendered.offset_x as f32;
                q.y0 = y - rendered.offset_y as f32;
                q.x1 = q.x0 + rendered.width as f32;
                q.y1 = q.y0 + rendered.height as f32;
                q.s0 = rendered.atlas_x as f32 * it;
                q.t0 = rendered.atlas_y as f32 * it;
                q.s1 = (rendered.atlas_x + rendered.width) as f32 * it;
                q.t1 = (rendered.atlas_y + rendered.height) as f32 * it;

                if rendered.color_glyph {
                    color_batches
                        .entry(rendered.texture_index)
                        .or_default()
                        .push(q);
                } else {
                    alpha_batches
                        .entry(color_key(fg))
                        .or_default()
                        .entry(rendered.texture_index)
                        .or_default()
                        .push(q);
                }
            }
        }

        for (ck, tex_quads) in alpha_batches {
            let color = key_to_color(ck);
            let cmds: Vec<DrawCommand> = tex_quads
                .into_iter()
                .map(|(tex_idx, quads)| DrawCommand {
                    image_id: self.glyph_cache.textures[tex_idx].image_id,
                    quads,
                })
                .collect();
            canvas.draw_glyph_commands(
                GlyphDrawCommands {
                    alpha_glyphs: cmds,
                    color_glyphs: vec![],
                },
                &Paint::color(color),
            );
        }

        if !color_batches.is_empty() {
            let cmds: Vec<DrawCommand> = color_batches
                .into_iter()
                .map(|(tex_idx, quads)| DrawCommand {
                    image_id: self.glyph_cache.textures[tex_idx].image_id,
                    quads,
                })
                .collect();
            canvas.draw_glyph_commands(
                GlyphDrawCommands {
                    alpha_glyphs: vec![],
                    color_glyphs: cmds,
                },
                &Paint::color(Color::white()),
            );
        }

        self.draw_cell_decorations(
            canvas,
            screen,
            scroll_offset,
            ox_px,
            oy_px,
            &palette,
            reverse_video,
            &|r, c| selected(r, c),
        );

        // A bar or underline cursor is a rule of its own rather than an
        // inversion, painted last so it sits over the glyph.
        if let Some((crow, ccol)) = focused_cursor
            && cursor_lit
            && !block_cursor
        {
            let thickness = (self.cell_height / 12.0).max(2.0);
            let x = ox_px + f32::from(ccol) * self.cell_width;
            let y = oy_px + f32::from(crow) * self.cell_height;
            let mut path = Path::new();
            match cursor_shape {
                vt100::CursorShape::Bar => {
                    path.rect(x, y, thickness, self.cell_height);
                }
                vt100::CursorShape::Underline => {
                    path.rect(
                        x,
                        y + self.cell_height - thickness,
                        self.cell_width,
                        thickness,
                    );
                }
                vt100::CursorShape::Block => unreachable!("handled above"),
            }
            let color = palette.cursor().unwrap_or_else(|| palette.fg());
            canvas.fill_path(&path, &Paint::color(color));
        }
    }

    /// Underline (all five shapes), strikethrough and overline for the
    /// cells of one screen.
    ///
    /// A separate pass, after the glyphs, because the glyph path
    /// batches by texture and colour and has nowhere to hang a rule.
    /// Rules accumulate into one path per colour so a run of underlined
    /// text costs one fill, not one per cell — and so adjacent cells
    /// meet without a seam.
    #[allow(clippy::too_many_arguments)]
    fn draw_cell_decorations<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        screen: &vt100::Screen,
        scroll_offset: usize,
        ox_px: f32,
        oy_px: f32,
        palette: &Palette,
        reverse_video: bool,
        selected: &dyn Fn(u16, u16) -> bool,
    ) {
        let (rows, cols) = screen.size();
        let thickness = (self.font_size / 16.0).max(1.0);
        let underline_y = self.ascent + (self.cell_height - self.ascent) * 0.5;
        let strike_y = self.ascent - self.ascent * 0.35;

        // Straight rules by colour, and curly ones separately: a
        // squiggle has to be stroked, not filled.
        let mut fills: HashMap<u32, Path> = HashMap::new();
        let mut curls: HashMap<u32, Path> = HashMap::new();

        for row in 0..rows {
            for col in 0..cols {
                let Some(cell) = screen.cell_at(scroll_offset, row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let style = cell.underline_style();
                if style.is_none() && !cell.strikethrough() && !cell.overline() {
                    continue;
                }
                // Blinking text takes its rules with it.
                if cell.blink() && !self.blink_phase {
                    continue;
                }
                let (fg, _) = resolve_cell_colors(
                    cell,
                    false,
                    selected(row, col),
                    palette,
                    reverse_video,
                );
                let x = ox_px + f32::from(col) * self.cell_width;
                let y = oy_px + f32::from(row) * self.cell_height;
                let w = self.cell_width * if cell.is_wide() { 2.0 } else { 1.0 };
                let key = color_key(fg);

                if let Some(style) = style {
                    match style {
                        vt100::UnderlineStyle::Curly => {
                            let path = curls.entry(key).or_default();
                            curly_underline(
                                path,
                                x,
                                y + underline_y,
                                w,
                                thickness,
                            );
                        }
                        other => {
                            let path = fills.entry(key).or_default();
                            straight_underline(
                                path,
                                other,
                                x,
                                y + underline_y,
                                w,
                                thickness,
                            );
                        }
                    }
                }
                if cell.strikethrough() {
                    fills.entry(key).or_default().rect(
                        x,
                        y + strike_y,
                        w,
                        thickness,
                    );
                }
                if cell.overline() {
                    fills.entry(key).or_default().rect(x, y, w, thickness);
                }
            }
        }

        for (key, path) in &fills {
            canvas.fill_path(path, &Paint::color(key_to_color(*key)));
        }
        for (key, path) in &curls {
            let mut paint = Paint::color(key_to_color(*key));
            paint.set_line_width(thickness);
            canvas.stroke_path(path, &paint);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        screen: &vt100::Screen,
        max_scrollback: usize,
        vge_state: &vge::VgeState,
        top_of_live_screen: i64,
        prt_state: &prt::PrtState,
        selection: Option<&SelectionRange>,
        portal_selection: Option<&prt::render::PortalSelectionCtx>,
        search_overlay: Option<&prt::render::PortalSearchCtx>,
        vge_selection: Option<&crate::vge::pick::VgeSelection>,
    ) {
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let show_cursor = !screen.hide_cursor() && screen.scrollback() == 0;
        // The VGE hit-test index is a by-product of this pass; last
        // frame's entries describe a screen that no longer exists.
        self.pick.clear();
        // §9.1 — the host's text-grid cursor renders only when host
        // focus is on the host itself; if focus has been routed into a
        // portal, the host cursor is suppressed and the focused-leaf
        // portal renders the focused look instead.
        let host_has_focus = matches!(prt_state.focus, prt::FocusKind::Host);

        // Host text grid.
        let focused_cursor = if show_cursor && host_has_focus {
            Some((cursor_row, cursor_col))
        } else {
            None
        };
        // Search highlights paint on this scope only when the overlay
        // targets the host (remaining_path empty).
        let host_highlights: Option<Vec<HighlightSpan>> =
            search_overlay.and_then(|o| {
                if !o.remaining_path.is_empty() {
                    return None;
                }
                Some(search_highlights_for_viewport(
                    o.matches,
                    o.current,
                    top_of_live_screen,
                    screen.scrollback(),
                    rows,
                    cols,
                ))
            });
        self.draw_screen_at(
            canvas,
            screen,
            // The host grid is not a view — it owns its own offset.
            screen.scrollback(),
            0.0,
            0.0,
            focused_cursor,
            selection,
            host_highlights.as_deref(),
        );

        // Unified §10 layer walk: top-level VGE elements + host portals
        // sorted by (draw_order, creation_seq), each rendered in turn.
        // Per-portal sub-portals recurse from inside.
        prt::render::render_layers(
            canvas,
            self,
            vge_state,
            prt_state,
            top_of_live_screen,
            rows,
            cols,
            screen.scrollback(),
            portal_selection,
            search_overlay,
            vge_selection,
        );

        // Draw scrollbar when scrolled back
        let scrollback = screen.scrollback();
        if scrollback > 0 && max_scrollback > 0 {
            let track_height = rows as f32 * self.cell_height;
            let total_lines = (max_scrollback + rows as usize) as f32;
            let thumb_ratio = (rows as f32 / total_lines).clamp(0.05, 1.0);
            let thumb_height = (thumb_ratio * track_height).max(16.0);
            let available = track_height - thumb_height;
            let thumb_y =
                ((max_scrollback - scrollback) as f32 / max_scrollback as f32) * available;

            let bar_width = 6.0;
            let bar_x = cols as f32 * self.cell_width - bar_width - 2.0;

            let mut path = Path::new();
            path.rounded_rect(bar_x, thumb_y, bar_width, thumb_height, 3.0);
            canvas.fill_path(&path, &Paint::color(Color::rgba(255, 255, 255, 90)));
        }
    }
}

#[cfg(test)]
mod palette_tests {
    use super::*;

    fn cell_after(bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(4, 20, 0);
        p.process(bytes);
        p
    }

    fn base(i: u8) -> (u8, u8, u8) {
        let c = Theme::default().indexed(i);
        (c.r, c.g, c.b)
    }

    #[test]
    fn osc_4_overrides_one_entry_and_104_puts_it_back() {
        let mut pal = Palette::default();
        assert_eq!(pal.indexed_rgb(1), base(1));
        pal.set_indexed(1, (1, 2, 3));
        assert_eq!(pal.indexed_rgb(1), (1, 2, 3));
        pal.reset_indexed(Some(1));
        assert_eq!(pal.indexed_rgb(1), base(1));

        pal.set_indexed(1, (1, 2, 3));
        pal.set_indexed(2, (4, 5, 6));
        pal.reset_indexed(None);
        assert_eq!(pal.indexed_rgb(2), base(2));
    }

    /// The theme is the *base*, not another override: `OSC 104` puts
    /// back what the user configured, not what veter shipped with. A
    /// themed terminal whose reset dropped to Tango would be a
    /// different terminal after any program that resets the palette on
    /// exit.
    #[test]
    fn a_reset_falls_back_to_the_theme_not_to_the_built_in_palette() {
        let nord = crate::theme::builtin("nord").unwrap();
        let mut pal = Palette::with_theme(&nord);
        assert_eq!(pal.indexed_rgb(1), (0xbf, 0x61, 0x6a));
        pal.set_indexed(1, (1, 2, 3));
        pal.reset_indexed(Some(1));
        assert_eq!(pal.indexed_rgb(1), (0xbf, 0x61, 0x6a));

        // The same for the dynamic trio. `OSC 11 ; ?` answering the
        // theme background is what tells vim which colourscheme half to
        // pick, so it must survive an `OSC 111`.
        assert_eq!(
            pal.dynamic_rgb(vt100::DynamicColor::Background),
            (0x2e, 0x34, 0x40)
        );
        pal.set_dynamic(vt100::DynamicColor::Background, (9, 9, 9));
        assert_eq!(pal.dynamic_rgb(vt100::DynamicColor::Background), (9, 9, 9));
        pal.reset_dynamic(vt100::DynamicColor::Background);
        assert_eq!(
            pal.dynamic_rgb(vt100::DynamicColor::Background),
            (0x2e, 0x34, 0x40)
        );
        // nord names a cursor colour, so the cursor reports it rather
        // than the foreground it would otherwise be drawn against.
        assert_eq!(
            pal.dynamic_rgb(vt100::DynamicColor::Cursor),
            (0xd8, 0xde, 0xe9)
        );
    }

    /// A theme that names a selection colour paints it; one that does
    /// not keeps the reverse-video selection veter has always drawn.
    /// Both stack with `SGR 7` the same way — selecting inverted text
    /// must not cancel the inversion out.
    #[test]
    fn a_themed_selection_paints_instead_of_inverting() {
        let plain = cell_after(b"x");
        let cell = plain.screen().cell(0, 0).unwrap();

        let pal = Palette::default();
        let (fg, bg) = resolve_cell_colors(cell, false, true, &pal, false);
        assert_eq!((rgb8(fg), rgb8(bg)), (rgb8(pal.bg()), rgb8(pal.fg())));

        let nord = crate::theme::builtin("nord").unwrap();
        let pal = Palette::with_theme(&nord);
        let (fg, bg) = resolve_cell_colors(cell, false, true, &pal, false);
        // Only `selection_bg` is set, so the glyph keeps its own colour.
        assert_eq!(rgb8(bg), (0x43, 0x4c, 0x5e));
        assert_eq!(rgb8(fg), rgb8(pal.fg()));

        // Under `SGR 7` the cell is already inverted; a themed
        // selection replaces the background of whatever is there rather
        // than un-inverting it.
        let inverted = cell_after(b"\x1b[7mx");
        let cell = inverted.screen().cell(0, 0).unwrap();
        let (fg, bg) = resolve_cell_colors(cell, false, true, &pal, false);
        assert_eq!(rgb8(bg), (0x43, 0x4c, 0x5e));
        assert_eq!(rgb8(fg), rgb8(pal.bg()));
    }

    /// An `OSC 11 ; ?` before anything sets it must report the real
    /// default background, not black — that answer decides whether vim
    /// picks a light or a dark colourscheme.
    #[test]
    fn the_dynamic_colours_report_the_defaults_until_they_are_set() {
        let theme = Theme::default();
        let (fg, bg) = (
            (theme.foreground.r, theme.foreground.g, theme.foreground.b),
            (theme.background.r, theme.background.g, theme.background.b),
        );
        // `DEFAULT_BG` is the pre-renderer stand-in for the same value
        // and must not drift from it.
        assert_eq!(rgb8(DEFAULT_BG), bg);

        let mut pal = Palette::default();
        assert_eq!(pal.dynamic_rgb(vt100::DynamicColor::Background), bg);
        assert_eq!(pal.dynamic_rgb(vt100::DynamicColor::Foreground), fg);
        // With no cursor colour of its own, the cursor reports the
        // foreground it is drawn against.
        assert_eq!(pal.dynamic_rgb(vt100::DynamicColor::Cursor), fg);
        pal.set_dynamic(vt100::DynamicColor::Cursor, (9, 9, 9));
        assert_eq!(pal.dynamic_rgb(vt100::DynamicColor::Cursor), (9, 9, 9));
        pal.reset_dynamic(vt100::DynamicColor::Cursor);
        assert_eq!(pal.dynamic_rgb(vt100::DynamicColor::Cursor), fg);
    }

    #[test]
    fn a_palette_override_reaches_the_cell_colours() {
        let p = cell_after(b"\x1b[31mx");
        let cell = p.screen().cell(0, 0).unwrap();
        let mut pal = Palette::default();
        pal.set_indexed(1, (10, 20, 30));
        let (fg, _) = resolve_cell_colors(cell, false, false, &pal, false);
        assert_eq!(rgb8(fg), (10, 20, 30));
    }

    /// DECSCNM reverses the whole screen, and stacks with the per-cell
    /// inversion rather than overriding it.
    #[test]
    fn decscnm_reverses_the_screen_and_stacks_with_sgr_7() {
        let p = cell_after(b"x\x1b[7my");
        let plain = p.screen().cell(0, 0).unwrap();
        let inverse = p.screen().cell(0, 1).unwrap();
        let pal = Palette::default();

        let (fg, bg) = resolve_cell_colors(plain, false, false, &pal, true);
        assert_eq!((rgb8(fg), rgb8(bg)), (rgb8(pal.bg()), rgb8(pal.fg())));

        // Reversed twice is not reversed at all.
        let (fg, bg) = resolve_cell_colors(inverse, false, false, &pal, true);
        assert_eq!((rgb8(fg), rgb8(bg)), (rgb8(pal.fg()), rgb8(pal.bg())));
    }

    /// SGR 8 draws the glyph in the background it sits on, so the text
    /// is invisible but the cell keeps its shape.
    #[test]
    fn conceal_paints_the_glyph_in_its_own_background() {
        let p = cell_after(b"\x1b[8;44mx");
        let cell = p.screen().cell(0, 0).unwrap();
        let pal = Palette::default();
        let (fg, bg) = resolve_cell_colors(cell, false, false, &pal, false);
        assert_eq!(rgb8(fg), rgb8(bg));
    }

    /// Without `OSC 12` the block cursor is the cell in reverse video,
    /// which is what veter has always drawn; with one it paints that
    /// colour behind the character.
    #[test]
    fn the_block_cursor_takes_an_osc_12_colour() {
        let p = cell_after(b"x");
        let cell = p.screen().cell(0, 0).unwrap();

        let pal = Palette::default();
        let (fg, bg) = resolve_cell_colors(cell, true, false, &pal, false);
        assert_eq!((rgb8(fg), rgb8(bg)), (rgb8(pal.bg()), rgb8(pal.fg())));

        let mut pal = Palette::default();
        pal.set_dynamic(vt100::DynamicColor::Cursor, (200, 0, 0));
        let (fg, bg) = resolve_cell_colors(cell, true, false, &pal, false);
        assert_eq!(rgb8(bg), (200, 0, 0));
        assert_eq!(rgb8(fg), rgb8(pal.bg()), "text takes the cell's background");
    }
}

#[cfg(test)]
mod decoration_tests {
    use super::*;

    /// Dashes are phased on the absolute x, so a run of underlined
    /// cells reads as one dashed rule instead of restarting in each
    /// cell.
    #[test]
    fn dashes_keep_their_phase_across_a_cell_boundary() {
        fn marks(x: f32, width: f32) -> Vec<(u32, u32)> {
            let mut path = Path::new();
            dashes(&mut path, x, 0.0, width, 1.0, 2.0, 2.0);
            path.verbs()
                .filter_map(|v| match v {
                    femtovg::Verb::MoveTo(px, _) => Some(px),
                    _ => None,
                })
                .map(|px| (px as u32, 0))
                .collect()
        }
        // Two adjacent 8px cells produce the same marks as one 16px run.
        let split: Vec<_> =
            marks(0.0, 8.0).into_iter().chain(marks(8.0, 8.0)).collect();
        assert_eq!(split, marks(0.0, 16.0));
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;
    use crate::search::MatchSpan;

    /// Viewport of `rows` rows showing lines `0..rows`, i.e. live screen
    /// with no scrollback.
    fn project(matches: &[MatchSpan], rows: u16, cols: u16) -> Vec<(u16, u16, u16)> {
        search_highlights_for_viewport(matches, 0, 0, 0, rows, cols)
            .into_iter()
            .map(|h| (h.row, h.col_start, h.col_end))
            .collect()
    }

    #[test]
    fn single_row_span_projects_to_one_highlight() {
        let spans = project(&[MatchSpan::row(2, 3, 8)], 10, 40);
        assert_eq!(spans, vec![(2, 3, 8)]);
    }

    /// A hint that crossed a soft wrap paints as one run: the tail of its
    /// first row, all of the middle, the head of its last.
    #[test]
    fn multi_row_span_covers_every_row() {
        let span = MatchSpan {
            line: 1,
            col_start: 30,
            end_line: 3,
            col_end: 12,
        };
        assert_eq!(project(&[span], 10, 40), vec![(1, 30, 40), (2, 0, 40), (3, 0, 12)]);
    }

    /// Scrolled so only the span's tail is on screen: the off-screen rows
    /// contribute nothing rather than clamping onto row 0.
    #[test]
    fn rows_outside_the_viewport_are_dropped() {
        let span = MatchSpan {
            line: -2,
            col_start: 30,
            end_line: 1,
            col_end: 5,
        };
        assert_eq!(project(&[span], 10, 40), vec![(0, 0, 40), (1, 0, 5)]);
    }

    /// A wrapped span whose last row ends at column 0 has nothing to
    /// paint there — an empty span would draw as a zero-width artefact.
    #[test]
    fn empty_trailing_row_is_dropped() {
        let span = MatchSpan {
            line: 0,
            col_start: 10,
            end_line: 1,
            col_end: 0,
        };
        assert_eq!(project(&[span], 10, 40), vec![(0, 10, 40)]);
    }

    #[test]
    fn current_match_is_flagged_on_all_its_rows() {
        let spans = search_highlights_for_viewport(
            &[
                MatchSpan::row(0, 0, 4),
                MatchSpan {
                    line: 1,
                    col_start: 0,
                    end_line: 2,
                    col_end: 3,
                },
            ],
            1,
            0,
            0,
            10,
            40,
        );
        let flags: Vec<bool> = spans.iter().map(|s| s.is_current).collect();
        assert_eq!(flags, vec![false, true, true]);
    }
}

#[cfg(test)]
mod text_layout_tests {
    use super::*;
    use femtovg::renderer::Void;
    use vge::command::{Align, FontStyle};

    fn harness() -> (Canvas<Void>, TerminalRenderer) {
        let mut canvas = Canvas::new(Void).unwrap();
        canvas.set_size(800, 600, 1.0);
        let tr = TerminalRenderer::new(&mut canvas, 14.0, FontSpec::default());
        (canvas, tr)
    }

    fn layout(tr: &mut TerminalRenderer, text: &str, x: f32, align: Align) -> TextLayout {
        tr.layout_vge_text(text, x, align, FontStyle(0), 1.0)
    }

    #[test]
    fn stops_cover_every_boundary_and_ascend() {
        let (_canvas, mut tr) = harness();
        let text = "héllo wörld";
        let l = layout(&mut tr, text, 0.0, Align::Left);

        assert_eq!(l.stops.len(), text.chars().count() + 1);
        assert_eq!(l.stops.first().unwrap().0, 0);
        assert_eq!(l.stops.last().unwrap().0, text.len());
        for w in l.stops.windows(2) {
            assert!(w[0].0 < w[1].0, "byte offsets must ascend");
            assert!(w[1].1 >= w[0].1, "x must not go backwards");
            // Every offset is a real char boundary, so slicing there
            // can never panic.
            assert!(text.is_char_boundary(w[0].0));
        }
        assert!(l.total_width > 0.0);
    }

    #[test]
    fn alignment_pins_the_requested_edge() {
        let (_canvas, mut tr) = harness();
        let left = layout(&mut tr, "abcd", 100.0, Align::Left);
        let centre = layout(&mut tr, "abcd", 100.0, Align::Center);
        let right = layout(&mut tr, "abcd", 100.0, Align::Right);
        let w = left.total_width;

        assert!((left.start_x - 100.0).abs() < 1e-3);
        assert!((centre.start_x - (100.0 - w / 2.0)).abs() < 1e-3);
        assert!((right.start_x - (100.0 - w)).abs() < 1e-3);
    }

    #[test]
    fn byte_offset_at_is_a_caret_position() {
        let (_canvas, mut tr) = harness();
        let l = layout(&mut tr, "abcd", 50.0, Align::Left);
        let cw = l.total_width / 4.0;

        // Clamped at both ends.
        assert_eq!(l.byte_offset_at(-1000.0), 0);
        assert_eq!(l.byte_offset_at(50.0 + l.total_width + 1000.0), 4);
        // Left third of the first glyph rounds before it, right third
        // after it.
        assert_eq!(l.byte_offset_at(50.0 + cw * 0.2), 0);
        assert_eq!(l.byte_offset_at(50.0 + cw * 0.8), 1);
        assert_eq!(l.byte_offset_at(50.0 + cw * 2.1), 2);
    }

    #[test]
    fn char_range_at_never_rounds_to_a_neighbour() {
        let (_canvas, mut tr) = harness();
        let text = "aé z";
        let l = layout(&mut tr, text, 0.0, Align::Left);
        let cw = l.total_width / 4.0;

        assert_eq!(l.char_range_at(cw * 0.5), Some((0, 1)));
        // 'é' is two bytes; the range has to reflect that or slicing
        // the copied text would split a code point.
        assert_eq!(l.char_range_at(cw * 1.5), Some((1, 3)));
        assert_eq!(&text[1..3], "é");
        assert_eq!(l.char_range_at(-1.0), None);
        assert_eq!(l.char_range_at(l.total_width + 1.0), None);
    }

    #[test]
    fn x_of_byte_round_trips_the_run_edges() {
        let (_canvas, mut tr) = harness();
        let l = layout(&mut tr, "hello", 30.0, Align::Left);

        assert!((l.x_of_byte(0) - l.start_x).abs() < 1e-3);
        assert!((l.x_of_byte(5) - (l.start_x + l.total_width)).abs() < 1e-3);
        // Monotonic in between.
        for b in 0..5 {
            assert!(l.x_of_byte(b) <= l.x_of_byte(b + 1));
        }
        // Past the end clamps rather than panicking.
        assert!((l.x_of_byte(99) - (l.start_x + l.total_width)).abs() < 1e-3);
    }

    /// Bold and italic take the Parley path, where the stop list comes
    /// from shaped clusters rather than per-char advances. It has to
    /// come out with the same shape as the plain one.
    #[test]
    fn styled_runs_produce_the_same_stop_shape() {
        let (_canvas, mut tr) = harness();
        let text = "Bold text";
        for bits in [0x01u8, 0x02, 0x03] {
            let l = tr.layout_vge_text(text, 0.0, Align::Left, FontStyle(bits), 1.0);
            assert!(l.total_width > 0.0, "style {bits:#x} measured to nothing");
            assert_eq!(l.stops.first().unwrap().0, 0);
            assert_eq!(l.stops.last().unwrap().0, text.len());
            for w in l.stops.windows(2) {
                assert!(w[0].0 <= w[1].0, "style {bits:#x}: offsets must ascend");
                assert!(text.is_char_boundary(w[0].0));
            }
            assert_eq!(l.byte_offset_at(-1000.0), 0);
            assert_eq!(l.byte_offset_at(l.total_width + 1000.0), text.len());
        }
    }

    /// Mapping a click to a character re-lays-out the run as `Left` at
    /// the left edge recorded in the pick index, whatever alignment it
    /// was originally drawn with. That shortcut is only sound if the
    /// two layouts are identical — this pins it.
    #[test]
    fn realigning_at_start_x_reproduces_the_layout() {
        let (_canvas, mut tr) = harness();
        let text = "some label";
        for align in [Align::Left, Align::Center, Align::Right] {
            let drawn = tr.layout_vge_text(text, 137.0, align, FontStyle(0), 1.0);
            let relaid =
                tr.layout_vge_text(text, drawn.start_x, Align::Left, FontStyle(0), 1.0);

            assert!((relaid.start_x - drawn.start_x).abs() < 1e-4, "{align:?}");
            assert!(
                (relaid.total_width - drawn.total_width).abs() < 1e-4,
                "{align:?}"
            );
            assert_eq!(relaid.stops, drawn.stops, "{align:?}");
            // And so every position maps to the same character.
            for i in 0..=20 {
                let x = drawn.start_x + drawn.total_width * (i as f32 / 20.0);
                assert_eq!(relaid.byte_offset_at(x), drawn.byte_offset_at(x), "{align:?}");
            }
        }
    }

    #[test]
    fn empty_run_has_no_width() {
        let (_canvas, mut tr) = harness();
        let l = layout(&mut tr, "", 12.0, Align::Left);
        assert_eq!(l.total_width, 0.0);
        assert_eq!(l.byte_offset_at(1000.0), 0);
    }
}

#[cfg(test)]
mod font_fallback_tests {
    use super::*;

    #[test]
    fn private_use_blocks_are_recognised() {
        // BMP block and both supplementary planes.
        assert!(is_private_use('\u{e000}'));
        assert!(is_private_use('\u{e0a0}')); // powerline branch
        assert!(is_private_use('\u{f8ff}'));
        assert!(is_private_use('\u{f0000}'));
        assert!(is_private_use('\u{100000}'));
        // Neighbours outside it, and ordinary text.
        assert!(!is_private_use('\u{d7ff}')); // last codepoint before the surrogates
        assert!(!is_private_use('\u{f900}'));
        assert!(!is_private_use('\u{25b6}'));
        assert!(!is_private_use('A'));
    }

    /// Font identity is the bytes. Keying on the blob address instead
    /// would let a recycled allocation alias two different fonts, and a
    /// cached glyph id would then index the wrong tables.
    #[test]
    fn register_fallback_interns_by_content() {
        let mut fonts = Vec::new();
        let a = vec![1u8, 2, 3];
        let b = vec![4u8, 5, 6];

        let cell = CellMetrics { width: 9.0, height: 20.0, size: 16.0 };
        assert_eq!(register_fallback(&mut fonts, &a, 0, cell), 1);
        assert_eq!(
            register_fallback(&mut fonts, &a, 0, cell),
            1,
            "same bytes reuse the id"
        );
        assert_eq!(
            register_fallback(&mut fonts, &b, 0, cell),
            2,
            "different bytes, new id"
        );
        assert_eq!(
            register_fallback(&mut fonts, &a, 1, cell),
            3,
            "same bytes at another face index is another font"
        );
        assert_eq!(fonts.len(), 3);
    }

    /// Separators are primitives, so they never reach font selection —
    /// that is what makes them tile against the neighbouring cell's
    /// background whatever (if anything) is installed.
    #[test]
    fn powerline_separators_are_drawn_as_primitives() {
        let mut canvas = Canvas::new(femtovg::renderer::Void).unwrap();
        canvas.set_size(200, 100, 1.0);
        let fg = Color::white();
        for code in 0xE0B0..=0xE0B7u32 {
            let ch = char::from_u32(code).unwrap();
            assert!(
                try_draw_powerline(&mut canvas, ch, 0.0, 0.0, 9.0, 20.0, fg),
                "U+{code:04X} must be drawn as a primitive"
            );
        }
        // Its neighbours are ordinary glyphs: U+E0AF ends the block
        // below, U+E0B8 starts the slanted set, and U+E0A0 is the branch
        // icon, which is not a tiling shape.
        // A one-pixel cell must not panic the pen calculation.
        assert!(try_draw_powerline(&mut canvas, '\u{e0b1}', 0.0, 0.0, 1.0, 1.0, fg));

        for code in [0xE0AFu32, 0xE0B8, 0xE0A0] {
            let ch = char::from_u32(code).unwrap();
            assert!(
                !try_draw_powerline(&mut canvas, ch, 0.0, 0.0, 9.0, 20.0, fg),
                "U+{code:04X} must fall through to the font"
            );
        }
    }

    /// A fallback face is rasterised at the primary's pixel size, which
    /// says nothing about how wide its glyphs come out.
    #[test]
    fn cell_fit_scales_a_face_into_the_cell() {
        let mut cx = FontContext::new();
        let cell = CellMetrics { width: 19.2, height: 43.6, size: 32.0 };
        let Some(fam) = cx.collection.family_by_name("Symbols Nerd Font Mono") else {
            eprintln!("skipped: Symbols Nerd Font Mono not installed");
            return;
        };
        let info = fam.default_font().unwrap();
        let index = info.index() as usize;
        let blob = info.load(Some(&mut cx.source_cache)).unwrap();
        let (width_fit, height_fit) = cell_fit(blob.as_ref(), index, cell);

        // It advances a full em (32px at this size) into a 19.2px cell,
        // so its glyphs have to come down to ~0.6 to stay in one column.
        assert!(
            (0.5..0.7).contains(&width_fit),
            "width_fit was {width_fit}, expected the em to be scaled into the cell"
        );
        // Vertically it is short of the cell, so it would be grown —
        // which is why the narrower of the two constraints wins.
        assert!(height_fit > 1.0, "height_fit was {height_fit}");
    }

    /// The regression this whole chain exists for: a powerline glyph
    /// must come from a font that actually draws powerline glyphs, not
    /// from whichever family happens to map the codepoint. Skipped
    /// where the symbol font isn't installed, since the answer then
    /// legitimately depends on what is.
    #[test]
    fn pua_resolves_through_the_configured_symbol_font() {
        const SYMBOLS: &str = "Symbols Nerd Font Mono";
        let mut font_cx = FontContext::new();
        let mut layout_cx: LayoutContext<Color> = LayoutContext::new();
        let mut fonts: Vec<FallbackFont> = Vec::new();
        let mut map: HashMap<char, Option<ResolvedGlyph>> = HashMap::new();
        let mut pua: Vec<String> = Vec::new();

        let cell = CellMetrics { width: 19.2, height: 43.6, size: 32.0 };
        if glyph_in_family(&mut font_cx, &mut fonts, SYMBOLS, '\u{e0a0}', cell).is_none() {
            eprintln!("skipped: {SYMBOLS} not installed");
            return;
        }
        fonts.clear();

        let configured = vec![SYMBOLS.to_string()];
        let resolved = resolve_fallback(
            &mut font_cx,
            &mut layout_cx,
            &mut fonts,
            &mut map,
            &configured,
            &mut pua,
            '\u{e0a0}',
            cell,
        )
        .expect("powerline branch must resolve");

        let fb = &fonts[(resolved.font_id - 1) as usize];
        let family = FontRef::from_index(&fb.data, fb.index)
            .and_then(|f| {
                f.localized_strings()
                    .find_by_id(StringId::Family, None)
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        assert_eq!(family, SYMBOLS);
        // The configured list answered, so nothing was spawned.
        assert!(pua.is_empty());
    }
}

