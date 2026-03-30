use std::collections::HashMap;

use femtovg::{
    Atlas, Canvas, Color, DrawCommand, GlyphDrawCommands, ImageFlags, ImageSource, Paint, Path,
    Quad, Renderer,
};
use imgref::{Img, ImgRef};
use parley::{
    layout::{Alignment, Layout, PositionedLayoutItem},
    style::{FontStack, StyleProperty},
    AlignmentOptions, FontContext, LayoutContext,
};
use rgb::RGBA8;
use swash::{
    scale::{image::Content, Render, ScaleContext, Scaler, Source, StrikeWith},
    zeno::Format,
    FontRef,
};

const TEXTURE_SIZE: usize = 512;

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

fn resolve_cell_colors(cell: &vt100::Cell, is_cursor: bool) -> (Color, Color) {
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

    if cell.inverse() ^ is_cursor {
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
        scaler: &mut Scaler<'_>,
        glyph_id: u16,
        font_size: f32,
    ) -> Option<RenderedGlyph> {
        let key = GlyphCacheKey {
            glyph_id,
            font_size_tenths: (font_size * 10.0) as u32,
        };

        if let Some(cached) = self.entries.get(&key) {
            return *cached;
        }

        let result = self.render_glyph(canvas, scaler, glyph_id);
        self.entries.insert(key, result);
        result
    }

    fn render_glyph<T: Renderer>(
        &mut self,
        canvas: &mut Canvas<T>,
        scaler: &mut Scaler<'_>,
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

// --- Terminal renderer ---

pub struct TerminalRenderer {
    font_data: Vec<u8>,
    font_index: usize,
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
        let mut cell_width = (font_size * 0.6).ceil();
        let mut cell_height = (font_size * 1.2).ceil();
        let mut ascent = font_size;

        for line in layout.lines() {
            for item in line.items() {
                if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                    let run = glyph_run.run();
                    let font = run.font();
                    font_data = font.data.as_ref().to_vec();
                    font_index = font.index as usize;

                    let font_ref = FontRef::from_index(&font_data, font_index).unwrap();
                    let metrics = font_ref.metrics(&[]).scale(font_size);
                    ascent = metrics.ascent;
                    cell_height = (metrics.ascent + metrics.descent + metrics.leading).ceil();

                    let glyph_metrics = font_ref.glyph_metrics(&[]).scale(font_size);
                    let charmap = font_ref.charmap();
                    let m_glyph = charmap.map('M');
                    cell_width = glyph_metrics.advance_width(m_glyph).ceil();
                    break;
                }
            }
            break;
        }

        eprintln!(
            "Font: cell={}x{}, ascent={}, size={}",
            cell_width, cell_height, ascent, font_size
        );

        Self {
            font_data,
            font_index,
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

    pub fn render<T: Renderer>(&mut self, canvas: &mut Canvas<T>, screen: &vt100::Screen) {
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let show_cursor = !screen.hide_cursor();
        let default_bg = Color::rgb(30, 30, 30);

        // Draw cell backgrounds
        for row in 0..rows {
            for col in 0..cols {
                let cell = match screen.cell(row, col) {
                    Some(c) => c,
                    None => continue,
                };
                if cell.is_wide_continuation() {
                    continue;
                }

                let is_cursor = show_cursor && row == cursor_row && col == cursor_col;
                let (_, bg) = resolve_cell_colors(&cell, is_cursor);
                let w = if cell.is_wide() { 2.0 } else { 1.0 };

                if bg != default_bg {
                    let x = col as f32 * self.cell_width;
                    let y = row as f32 * self.cell_height;
                    let mut path = Path::new();
                    path.rect(x, y, self.cell_width * w, self.cell_height);
                    canvas.fill_path(&path, &Paint::color(bg));
                }
            }
        }

        // Draw glyphs
        let font_ref = FontRef::from_index(&self.font_data, self.font_index).unwrap();
        let charmap = font_ref.charmap();
        let mut scaler = self.scale_cx.builder(font_ref).size(self.font_size).hint(true).build();

        // Batch quads by (fg_color, texture_index) for alpha glyphs
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

                let glyph_id = charmap.map(ch);
                if glyph_id == 0 {
                    continue;
                }

                let is_cursor = show_cursor && row == cursor_row && col == cursor_col;
                let (fg, _) = resolve_cell_colors(&cell, is_cursor);

                let x = col as f32 * self.cell_width;
                let y = row as f32 * self.cell_height + self.ascent;

                let rendered = match self.glyph_cache.get_or_render(
                    canvas,
                    &mut scaler,
                    glyph_id,
                    self.font_size,
                ) {
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

        // Draw alpha glyphs grouped by color
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

        // Draw color glyphs (emoji etc.)
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
}
