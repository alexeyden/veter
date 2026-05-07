use std::collections::HashMap;

use femtovg::{
    Atlas, Canvas, Color, DrawCommand, GlyphDrawCommands, ImageFlags, ImageSource, Paint, Path,
    Quad, Renderer,
};

use crate::prt;
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

/// Selection range expressed in visible-row coords (i.e. as the
/// renderer sees them after the user's current scrollback offset is
/// applied). `start_row` may be negative when the selection extends
/// above the viewport (anchor in scrollback that's now off-screen);
/// `end_row` may exceed `rows` for the same reason at the bottom.
/// Half-open: `[start, end)` in lexicographic (row, col) order.
#[derive(Copy, Clone, Debug)]
pub struct SelectionRange {
    pub start_row: i32,
    pub start_col: u16,
    pub end_row: i32,
    pub end_col: u16,
}

impl SelectionRange {
    fn contains(&self, row: u16, col: u16) -> bool {
        let pos = (row as i32, col);
        let start = (self.start_row, self.start_col);
        let end = (self.end_row, self.end_col);
        pos >= start && pos < end
    }
}

/// Resolve an absolute-line selection (anchor + head in some vt100's
/// scrollback line coords) into a half-open `SelectionRange` in that
/// vt100's currently-visible row coords. Used by both the host call
/// site and per-portal render to avoid duplicating the math.
/// Returns `None` for empty or fully off-screen selections.
#[allow(clippy::too_many_arguments)]
pub fn selection_range_from_abs(
    anchor_line: i64,
    anchor_col: u16,
    head_line: i64,
    head_col: u16,
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
    })
}

// ANSI 256-color palette
fn ansi_color(idx: u8) -> Color {
    match idx {
        0 => Color::rgb(0, 0, 0),
        1 => Color::rgb(204, 0, 0),
        2 => Color::rgb(78, 154, 6),
        3 => Color::rgb(196, 160, 0),
        4 => Color::rgb(52, 101, 164),
        5 => Color::rgb(117, 80, 123),
        6 => Color::rgb(6, 152, 154),
        7 => Color::rgb(211, 215, 207),
        8 => Color::rgb(85, 87, 83),
        9 => Color::rgb(239, 41, 41),
        10 => Color::rgb(138, 226, 52),
        11 => Color::rgb(252, 233, 79),
        12 => Color::rgb(114, 159, 207),
        13 => Color::rgb(173, 127, 168),
        14 => Color::rgb(52, 226, 226),
        15 => Color::rgb(238, 238, 236),
        16..=231 => {
            let idx = idx - 16;
            let ri = idx / 36;
            let gi = (idx / 6) % 6;
            let bi = idx % 6;
            let r = if ri == 0 { 0 } else { ri * 40 + 55 };
            let g = if gi == 0 { 0 } else { gi * 40 + 55 };
            let b = if bi == 0 { 0 } else { bi * 40 + 55 };
            Color::rgb(r, g, b)
        }
        232..=255 => {
            let v = (idx - 232) * 10 + 8;
            Color::rgb(v, v, v)
        }
    }
}

fn resolve_cell_colors(cell: &vt100::Cell, is_cursor: bool, is_selected: bool) -> (Color, Color) {
    let default_fg = Color::rgb(204, 204, 204);
    let default_bg = Color::rgb(30, 30, 30);

    let mut fg = match cell.fgcolor() {
        vt100::Color::Default => default_fg,
        vt100::Color::Idx(i) => {
            let i = if cell.bold() && i < 8 { i + 8 } else { i };
            ansi_color(i)
        }
        vt100::Color::Rgb(r, g, b) => Color::rgb(r, g, b),
    };

    let mut bg = match cell.bgcolor() {
        vt100::Color::Default => default_bg,
        vt100::Color::Idx(i) => ansi_color(i),
        vt100::Color::Rgb(r, g, b) => Color::rgb(r, g, b),
    };

    if cell.inverse() ^ is_cursor ^ is_selected {
        std::mem::swap(&mut fg, &mut bg);
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

        // Find atlas space
        let mut found = None;
        for (idx, tex) in self.textures.iter_mut().enumerate() {
            if let Some((ax, ay)) = tex.atlas.add_rect(w, h) {
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
            let (ax, ay) = atlas.add_rect(w, h).unwrap();
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
    source_ptr: usize, // pointer identity from Parley's font cache
}

/// Resolved glyph: which font and glyph ID to use for a character.
#[derive(Copy, Clone)]
struct ResolvedGlyph {
    glyph_id: u16,
    font_id: u16, // 0 = primary, 1+ = fallback index + 1
}

/// Resolve a character to a fallback font. Uses Parley for font discovery.
/// Kept as a free function so the caller can pass disjoint struct fields.
fn resolve_fallback(
    font_cx: &mut FontContext,
    layout_cx: &mut LayoutContext<Color>,
    fallback_fonts: &mut Vec<FallbackFont>,
    char_font_map: &mut HashMap<char, Option<ResolvedGlyph>>,
    ch: char,
    font_size: f32,
) -> Option<ResolvedGlyph> {
    if let Some(&cached) = char_font_map.get(&ch) {
        return cached;
    }

    let s = String::from(ch);
    let mut builder = layout_cx.ranged_builder(font_cx, &s, 1.0, false);
    builder.push_default(StyleProperty::Brush(Color::white()));
    builder.push_default(FontStack::from("system-ui"));
    builder.push_default(StyleProperty::FontSize(font_size));
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
                    let source_ptr = data_ref.as_ptr() as usize;
                    let fb_idx = fallback_fonts
                        .iter()
                        .position(|fb| fb.source_ptr == source_ptr && fb.index == index)
                        .unwrap_or_else(|| {
                            let idx = fallback_fonts.len();
                            fallback_fonts.push(FallbackFont {
                                data: data_ref.to_vec(),
                                index,
                                source_ptr,
                            });
                            idx
                        });

                    let resolved = ResolvedGlyph {
                        glyph_id,
                        font_id: (fb_idx + 1) as u16,
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
    q.x0 = pen_x + rendered.offset_x as f32;
    q.y0 = pen_y - rendered.offset_y as f32;
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
    char_font_map: HashMap<char, Option<ResolvedGlyph>>,

    // Rendering
    font_size: f32,
    pub cell_width: f32,
    pub cell_height: f32,
    ascent: f32,
    scale_cx: ScaleContext,
    glyph_cache: GlyphCache,
}

impl TerminalRenderer {
    pub fn new<T: Renderer>(_canvas: &mut Canvas<T>, font_size: f32) -> Self {
        let mut font_cx = FontContext::new();
        let mut layout_cx = LayoutContext::new();

        let sample = "X";
        let mut builder = layout_cx.ranged_builder(&mut font_cx, sample, 1.0, false);
        builder.push_default(FontStack::from("monospace"));
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
            "Font: family={:?} cell={}x{}, ascent={}, size={}",
            font_family, cell_width, cell_height, ascent, font_size
        );

        Self {
            font_data,
            font_index,
            font_family,
            font_cx,
            layout_cx,
            fallback_fonts: Vec::new(),
            char_font_map: HashMap::new(),
            font_size,
            cell_width,
            cell_height,
            ascent,
            scale_cx: ScaleContext::new(),
            glyph_cache: GlyphCache::new(),
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
        let resolved = resolve_fallback(
            &mut self.font_cx,
            &mut self.layout_cx,
            &mut self.fallback_fonts,
            &mut self.char_font_map,
            ch,
            self.font_size,
        )?;
        Some((resolved.glyph_id, resolved.font_id))
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
    ) {
        if text.is_empty() {
            return;
        }

        // Render the glyphs themselves and recover the actual rendered
        // extent (start_x, total_width) so we can stack underline /
        // strikethrough rules on top.
        let (start_x, total_width) = if font_style.bold() || font_style.italic() {
            self.draw_text_styled(canvas, x_px, y_px, text, color, align, font_style)
        } else {
            self.draw_text_plain(canvas, x_px, y_px, text, color, align)
        };

        if font_style.underline() || font_style.strikethrough() {
            let mut path = Path::new();
            let thickness = (self.font_size / 16.0).max(1.0);
            if font_style.underline() {
                let uy = y_px + (self.cell_height - self.ascent) * 0.5;
                path.rect(start_x, uy, total_width, thickness);
            }
            if font_style.strikethrough() {
                let sy = y_px - self.ascent * 0.35;
                path.rect(start_x, sy, total_width, thickness);
            }
            canvas.fill_path(&path, &Paint::color(color));
        }
    }

    /// Per-char glyph rendering for plain (no bold/italic) text. Reuses
    /// the cell renderer's primary font + fallback chain. Returns
    /// `(start_x, total_width)` for stacking underline/strike.
    fn draw_text_plain<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        x_px: f32,
        y_px: f32,
        text: &str,
        color: Color,
        align: vge::command::Align,
    ) -> (f32, f32) {
        struct CharInfo {
            ch: char,
            glyph_id: u16,
            font_id: u16,
            advance: f32,
        }
        let mut infos: Vec<CharInfo> = Vec::with_capacity(text.len());
        let mut total_width = 0.0f32;
        for ch in text.chars() {
            let Some((glyph_id, font_id)) = self.resolve_glyph(ch) else {
                continue;
            };
            let font_ref = self.font_ref_for(font_id);
            let advance = font_ref
                .glyph_metrics(&[])
                .scale(self.font_size)
                .advance_width(glyph_id);
            total_width += advance;
            infos.push(CharInfo {
                ch,
                glyph_id,
                font_id,
                advance,
            });
        }

        let start_x = align_offset(x_px, total_width, align);

        let mut alpha_batches: HashMap<usize, Vec<Quad>> = HashMap::new();
        let mut color_batches: HashMap<usize, Vec<Quad>> = HashMap::new();
        let mut x = start_x;
        for info in &infos {
            if info.ch == ' ' {
                x += info.advance;
                continue;
            }
            let rendered = if info.font_id == 0 {
                let fr = FontRef::from_index(&self.font_data, self.font_index).unwrap();
                self.glyph_cache.get_or_render(
                    canvas,
                    &mut self.scale_cx,
                    fr,
                    info.glyph_id,
                    self.font_size,
                    0,
                )
            } else {
                let fb = &self.fallback_fonts[(info.font_id - 1) as usize];
                let fr = FontRef::from_index(&fb.data, fb.index).unwrap();
                self.glyph_cache.get_or_render(
                    canvas,
                    &mut self.scale_cx,
                    fr,
                    info.glyph_id,
                    self.font_size,
                    info.font_id,
                )
            };
            let rendered = match rendered {
                Some(r) => r,
                None => {
                    x += info.advance;
                    continue;
                }
            };
            push_glyph_quad(
                &mut alpha_batches,
                &mut color_batches,
                rendered,
                x,
                y_px,
            );
            x += info.advance;
        }

        emit_glyph_batches(canvas, &self.glyph_cache, alpha_batches, color_batches, color);
        (start_x, total_width)
    }

    /// Bold/italic-capable text rendering via Parley layout. Asks
    /// Parley to resolve a font face that matches the requested weight
    /// and slant, walks the resulting GlyphRuns, and routes each glyph
    /// through the existing GlyphCache. Different font faces (regular
    /// vs bold vs italic) end up under distinct font_ids in
    /// `fallback_fonts` and so cache independently.
    #[allow(clippy::too_many_arguments)]
    fn draw_text_styled<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        x_px: f32,
        y_px: f32,
        text: &str,
        color: Color,
        align: vge::command::Align,
        font_style: vge::command::FontStyle,
    ) -> (f32, f32) {
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
        builder.push_default(StyleProperty::FontSize(self.font_size));
        builder.push_default(StyleProperty::FontWeight(weight));
        builder.push_default(StyleProperty::FontStyle(pstyle));
        let mut layout: Layout<Color> = builder.build(text);
        layout.break_all_lines(None);
        layout.align(None, Alignment::Start, AlignmentOptions::default());

        let total_width = layout.width();
        let start_x = align_offset(x_px, total_width, align);

        // Pass 1: walk runs, register fonts, collect per-glyph info
        // (since iterating mutates self.fallback_fonts and we need
        // independent borrows for cache lookups in pass 2).
        struct G {
            x: f32,
            y: f32,
            glyph_id: u16,
            font_id: u16,
        }
        let mut glyphs: Vec<G> = Vec::new();
        for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(run_layout) = item {
                    let run = run_layout.run();
                    let font = run.font();
                    let data_ref = font.data.as_ref();
                    let font_index = font.index as usize;
                    let source_ptr = data_ref.as_ptr() as usize;

                    let font_id = match self.fallback_fonts.iter().position(|fb| {
                        fb.source_ptr == source_ptr && fb.index == font_index
                    }) {
                        Some(i) => (i + 1) as u16,
                        None => {
                            let i = self.fallback_fonts.len();
                            self.fallback_fonts.push(FallbackFont {
                                data: data_ref.to_vec(),
                                index: font_index,
                                source_ptr,
                            });
                            (i + 1) as u16
                        }
                    };

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
                        glyphs.push(G {
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

        // Pass 2: render with disjoint borrows.
        let mut alpha_batches: HashMap<usize, Vec<Quad>> = HashMap::new();
        let mut color_batches: HashMap<usize, Vec<Quad>> = HashMap::new();
        for g in &glyphs {
            let fb = &self.fallback_fonts[(g.font_id - 1) as usize];
            let fr = FontRef::from_index(&fb.data, fb.index).unwrap();
            let rendered = self.glyph_cache.get_or_render(
                canvas,
                &mut self.scale_cx,
                fr,
                g.glyph_id,
                self.font_size,
                g.font_id,
            );
            let rendered = match rendered {
                Some(r) => r,
                None => continue,
            };
            push_glyph_quad(
                &mut alpha_batches,
                &mut color_batches,
                rendered,
                start_x + g.x,
                y_px + g.y,
            );
        }

        emit_glyph_batches(canvas, &self.glyph_cache, alpha_batches, color_batches, color);
        (start_x, total_width)
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
    pub fn draw_screen_at<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        screen: &vt100::Screen,
        ox_px: f32,
        oy_px: f32,
        focused_cursor: Option<(u16, u16)>,
        selection: Option<&SelectionRange>,
    ) {
        let (rows, cols) = screen.size();
        let default_bg = Color::rgb(30, 30, 30);
        let selected = |r, c| selection.map(|s| s.contains(r, c)).unwrap_or(false);

        // Cell backgrounds.
        for row in 0..rows {
            for col in 0..cols {
                let cell = match screen.cell(row, col) {
                    Some(c) => c,
                    None => continue,
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let is_cursor = focused_cursor == Some((row, col));
                let sel = selected(row, col);
                let (_, bg) = resolve_cell_colors(cell, is_cursor, sel);
                let w = if cell.is_wide() { 2.0 } else { 1.0 };
                // Selected cells need a bg fill even when the underlying
                // cell uses the default bg, so the highlight is visible.
                if bg != default_bg || sel {
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

        for row in 0..rows {
            for col in 0..cols {
                let cell = match screen.cell(row, col) {
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

                // Block elements (U+2580..U+259F) tile seamlessly only
                // when drawn as primitives; the font glyphs leave gaps
                // because the cell box includes leading. Short-circuit
                // before the font lookup.
                let is_cursor = focused_cursor == Some((row, col));
                let (fg, _) = resolve_cell_colors(cell, is_cursor, selected(row, col));
                let cx = ox_px + col as f32 * self.cell_width;
                let cy = oy_px + row as f32 * self.cell_height;
                if try_draw_block_element(
                    canvas,
                    ch,
                    cx,
                    cy,
                    self.cell_width,
                    self.cell_height,
                    fg,
                ) {
                    continue;
                }

                let (glyph_id, font_id) = {
                    let gid = primary_charmap.map(ch);
                    if gid != 0 {
                        (gid, 0u16)
                    } else {
                        match resolve_fallback(
                            &mut self.font_cx,
                            &mut self.layout_cx,
                            &mut self.fallback_fonts,
                            &mut self.char_font_map,
                            ch,
                            self.font_size,
                        ) {
                            Some(rg) => (rg.glyph_id, rg.font_id),
                            None => continue,
                        }
                    }
                };

                let x = cx;
                let y = cy + self.ascent;

                let rendered = if font_id == 0 {
                    let fr = FontRef::from_index(&self.font_data, self.font_index).unwrap();
                    self.glyph_cache.get_or_render(
                        canvas,
                        &mut self.scale_cx,
                        fr,
                        glyph_id,
                        self.font_size,
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
                        self.font_size,
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
    ) {
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let show_cursor = !screen.hide_cursor() && screen.scrollback() == 0;
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
        self.draw_screen_at(canvas, screen, 0.0, 0.0, focused_cursor, selection);

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
