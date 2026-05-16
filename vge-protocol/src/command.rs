// Typed command parsing for VGE frames (§3, §6, §7).
//
// Phase II adds the TinyVG-style shape opcodes (FillPolygon, FillPath,
// the four DrawLine* variants, and the three OutlineFill* variants),
// gradient and ref styles, and SetGlobalStyle. Image opcodes remain
// rejected.

use super::codec::{DecodeError, DecodeResult, Point, Reader, Rect};
use super::frame::*;
use super::path::{read_path_segments, PathSegment};

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Style {
    Flat(Color),
    LinearGradient {
        p0: Point,
        p1: Point,
        c0: Color,
        c1: Color,
    },
    RadialGradient {
        center: Point,
        outer: Point,
        c_inner: Color,
        c_outer: Color,
    },
    Ref(String),
}

/// A style that can live in the global style table — Style minus Ref.
/// SetGlobalStyle uses this so "no nested refs" is a type-level invariant.
#[derive(Debug, Clone, PartialEq)]
pub enum ConcreteStyle {
    Flat(Color),
    LinearGradient {
        p0: Point,
        p1: Point,
        c0: Color,
        c1: Color,
    },
    RadialGradient {
        center: Point,
        outer: Point,
        c_inner: Color,
        c_outer: Color,
    },
}

impl ConcreteStyle {
    pub fn from_style(s: Style) -> Result<Self, DecodeError> {
        match s {
            Style::Flat(c) => Ok(ConcreteStyle::Flat(c)),
            Style::LinearGradient { p0, p1, c0, c1 } => {
                Ok(ConcreteStyle::LinearGradient { p0, p1, c0, c1 })
            }
            Style::RadialGradient {
                center,
                outer,
                c_inner,
                c_outer,
            } => Ok(ConcreteStyle::RadialGradient {
                center,
                outer,
                c_inner,
                c_outer,
            }),
            Style::Ref(_) => Err(DecodeError::bad_payload()),
        }
    }

    /// Re-wrap as a Style for uniform handling at render time.
    pub fn as_style(&self) -> Style {
        match self {
            ConcreteStyle::Flat(c) => Style::Flat(*c),
            ConcreteStyle::LinearGradient { p0, p1, c0, c1 } => Style::LinearGradient {
                p0: *p0,
                p1: *p1,
                c0: *c0,
                c1: *c1,
            },
            ConcreteStyle::RadialGradient {
                center,
                outer,
                c_inner,
                c_outer,
            } => Style::RadialGradient {
                center: *center,
                outer: *outer,
                c_inner: *c_inner,
                c_outer: *c_outer,
            },
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Debug, Copy, Clone, Default)]
pub struct FontStyle(pub u8);

impl FontStyle {
    /// Bold bit (0x01). The phase-1 renderer doesn't yet vary font
    /// weight, so this is parsed but not rendered.
    #[allow(dead_code)]
    pub fn bold(self) -> bool {
        self.0 & 0x01 != 0
    }
    /// Italic bit (0x02). Parsed but not yet rendered.
    #[allow(dead_code)]
    pub fn italic(self) -> bool {
        self.0 & 0x02 != 0
    }
    pub fn underline(self) -> bool {
        self.0 & 0x04 != 0
    }
    pub fn strikethrough(self) -> bool {
        self.0 & 0x08 != 0
    }
}

#[derive(Debug, Clone)]
pub enum DrawCmd {
    FillRectangles {
        fill: Style,
        rects: Vec<Rect>,
    },
    DrawText {
        origin: Point,
        align: Align,
        fill: Style,
        font_style: FontStyle,
        text: String,
    },
    FillPolygon {
        fill: Style,
        points: Vec<Point>,
    },
    FillPath {
        fill: Style,
        segments: Vec<PathSegment>,
    },
    DrawLines {
        stroke: Style,
        line_width: f32,
        lines: Vec<(Point, Point)>,
    },
    DrawLineLoop {
        stroke: Style,
        line_width: f32,
        points: Vec<Point>,
    },
    DrawLineStrip {
        stroke: Style,
        line_width: f32,
        points: Vec<Point>,
    },
    DrawLinePath {
        stroke: Style,
        line_width: f32,
        segments: Vec<PathSegment>,
    },
    OutlineFillPolygon {
        fill: Style,
        stroke: Style,
        line_width: f32,
        points: Vec<Point>,
    },
    OutlineFillRectangles {
        fill: Style,
        stroke: Style,
        line_width: f32,
        rects: Vec<Rect>,
    },
    OutlineFillPath {
        fill: Style,
        stroke: Style,
        line_width: f32,
        segments: Vec<PathSegment>,
    },
    DrawImage {
        target_rect: Rect,
        image_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct CreateElementBody {
    pub id: String, // empty = anonymous
    pub commands: Vec<DrawCmd>,
    pub origin: Point,
    pub is_visible: bool,
    pub draw_order: i32,
    /// If `Some`, this element's parent ID (§9.1). MUST be non-empty.
    pub parent: Option<String>,
    /// If `Some`, this element has a clip rect (§9.2) of this size.
    pub size: Option<Point>,
}

#[derive(Debug, Clone)]
pub struct UpdateCommandsBody {
    pub id: String,
    pub commands: Vec<DrawCmd>,
}

#[derive(Debug, Clone)]
pub struct UpdateCommandBody {
    pub id: String,
    pub index: usize,
    pub command: DrawCmd,
}

#[derive(Debug, Clone)]
pub enum UpdateTextRange {
    Whole,
    Range { start: usize, end: usize },
}

#[derive(Debug, Clone)]
pub struct UpdateTextBody {
    pub id: String,
    pub command_index: usize,
    pub range: UpdateTextRange,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub struct UploadImageBody {
    pub id: String,
    /// 0x01 Raw, 0x02 WebP. Kept as raw u8 here so the engine can
    /// decide whether the encoding is supported and surface
    /// `err_image_decode` / `err_bad_payload` cleanly.
    pub encoding: u8,
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct UpdateImageBody {
    pub id: String,
    pub command_index: usize,
    pub new_image_id: String,
}

// Spec-driven naming: every command shares the verb, so the
// "variant ends with enum name" pattern is unavoidable.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum Command {
    Probe,
    CreateElement(CreateElementBody),
    DeleteElement { id: String },
    UpdateCommands(UpdateCommandsBody),
    UpdateCommand(UpdateCommandBody),
    UpdateText(UpdateTextBody),
    UpdateOrigin { id: String, origin: Point },
    UpdateVisibility { id: String, is_visible: bool },
    UpdateDrawOrder { id: String, draw_order: i32 },
    ClearAll,
    SetGlobalStyle { id: String, style: ConcreteStyle },
    UploadImage(UploadImageBody),
    DropImage { id: String },
    UpdateImage(UpdateImageBody),
    UpdateSize { id: String, new_size: Point },
}

const MAX_ID_BYTES: usize = 64;

fn read_id(r: &mut Reader<'_>, allow_empty: bool) -> DecodeResult<String> {
    let s = r.string()?;
    if !allow_empty && s.is_empty() {
        return Err(DecodeError::bad_payload());
    }
    if s.len() > MAX_ID_BYTES {
        return Err(DecodeError::bad_payload());
    }
    Ok(s.to_owned())
}

fn read_color(r: &mut Reader<'_>) -> DecodeResult<Color> {
    let fmt = r.u8()?;
    match fmt {
        COLOR_RGBA8888 => {
            let rb = r.u8()?;
            let gb = r.u8()?;
            let bb = r.u8()?;
            let ab = r.u8()?;
            Ok(Color {
                r: rb as f32 / 255.0,
                g: gb as f32 / 255.0,
                b: bb as f32 / 255.0,
                a: ab as f32 / 255.0,
            })
        }
        COLOR_RGB565 => {
            let packed = r.u16()?;
            let r5 = (packed >> 11) & 0x1F;
            let g6 = (packed >> 5) & 0x3F;
            let b5 = packed & 0x1F;
            Ok(Color {
                r: r5 as f32 / 31.0,
                g: g6 as f32 / 63.0,
                b: b5 as f32 / 31.0,
                a: 1.0,
            })
        }
        _ => Err(DecodeError::bad_payload()),
    }
}

/// Read a Style (§7.3). All four kinds are supported. `Style::Ref` only
/// makes sense in client-supplied draw commands; SetGlobalStyle uses
/// `read_concrete_style` which rejects refs at decode time.
fn read_style(r: &mut Reader<'_>) -> DecodeResult<Style> {
    let kind = r.u8()?;
    match kind {
        STYLE_FLAT => Ok(Style::Flat(read_color(r)?)),
        STYLE_LINEAR_GRADIENT => {
            let p0 = r.point()?;
            let p1 = r.point()?;
            let c0 = read_color(r)?;
            let c1 = read_color(r)?;
            Ok(Style::LinearGradient { p0, p1, c0, c1 })
        }
        STYLE_RADIAL_GRADIENT => {
            let center = r.point()?;
            let outer = r.point()?;
            let c_inner = read_color(r)?;
            let c_outer = read_color(r)?;
            Ok(Style::RadialGradient {
                center,
                outer,
                c_inner,
                c_outer,
            })
        }
        STYLE_REF => {
            let id = r.string()?;
            if id.is_empty() || id.len() > MAX_ID_BYTES {
                return Err(DecodeError::bad_payload());
            }
            Ok(Style::Ref(id.to_owned()))
        }
        _ => Err(DecodeError::bad_payload()),
    }
}

/// Like `read_style` but rejects `Style::Ref`. Used by SetGlobalStyle
/// where nested refs are forbidden by §7.3.
pub fn read_concrete_style(r: &mut Reader<'_>) -> DecodeResult<ConcreteStyle> {
    ConcreteStyle::from_style(read_style(r)?)
}

/// Validate a stroke line_width: finite and non-negative. Negative or NaN
/// values would propagate into femtovg as garbage strokes.
fn validate_line_width(w: f32) -> DecodeResult<f32> {
    if !w.is_finite() || w < 0.0 {
        return Err(DecodeError::bad_payload());
    }
    Ok(w)
}

pub fn read_draw_cmd(r: &mut Reader<'_>) -> DecodeResult<DrawCmd> {
    let op = r.u8()?;
    match op {
        OP_FILL_RECTANGLES => {
            let fill = read_style(r)?;
            let n = r.varu()? as usize;
            let mut rects = Vec::with_capacity(n);
            for _ in 0..n {
                rects.push(r.rect()?);
            }
            Ok(DrawCmd::FillRectangles { fill, rects })
        }
        OP_DRAW_TEXT => {
            let origin = r.point()?;
            let align_byte = r.u8()?;
            let align = match align_byte {
                0 => Align::Left,
                1 => Align::Center,
                2 => Align::Right,
                _ => return Err(DecodeError::bad_payload()),
            };
            let fill = read_style(r)?;
            let font_style = FontStyle(r.u8()?);
            let text = r.string()?.to_owned();
            Ok(DrawCmd::DrawText {
                origin,
                align,
                fill,
                font_style,
                text,
            })
        }
        OP_FILL_POLYGON => {
            let fill = read_style(r)?;
            let n = r.varu()? as usize;
            if n < 3 {
                return Err(DecodeError::bad_payload());
            }
            let mut points = Vec::with_capacity(n);
            for _ in 0..n {
                points.push(r.point()?);
            }
            Ok(DrawCmd::FillPolygon { fill, points })
        }
        OP_FILL_PATH => {
            let fill = read_style(r)?;
            let segments = read_path_segments(r)?;
            Ok(DrawCmd::FillPath { fill, segments })
        }
        OP_DRAW_LINES => {
            let stroke = read_style(r)?;
            let line_width = validate_line_width(r.f32()?)?;
            let n = r.varu()? as usize;
            let mut lines = Vec::with_capacity(n);
            for _ in 0..n {
                let a = r.point()?;
                let b = r.point()?;
                lines.push((a, b));
            }
            Ok(DrawCmd::DrawLines {
                stroke,
                line_width,
                lines,
            })
        }
        OP_DRAW_LINE_LOOP | OP_DRAW_LINE_STRIP => {
            let stroke = read_style(r)?;
            let line_width = validate_line_width(r.f32()?)?;
            let n = r.varu()? as usize;
            if n < 2 {
                return Err(DecodeError::bad_payload());
            }
            let mut points = Vec::with_capacity(n);
            for _ in 0..n {
                points.push(r.point()?);
            }
            if op == OP_DRAW_LINE_LOOP {
                Ok(DrawCmd::DrawLineLoop {
                    stroke,
                    line_width,
                    points,
                })
            } else {
                Ok(DrawCmd::DrawLineStrip {
                    stroke,
                    line_width,
                    points,
                })
            }
        }
        OP_DRAW_LINE_PATH => {
            let stroke = read_style(r)?;
            let line_width = validate_line_width(r.f32()?)?;
            let segments = read_path_segments(r)?;
            Ok(DrawCmd::DrawLinePath {
                stroke,
                line_width,
                segments,
            })
        }
        OP_OUTLINE_FILL_POLYGON => {
            let fill = read_style(r)?;
            let stroke = read_style(r)?;
            let line_width = validate_line_width(r.f32()?)?;
            let n = r.varu()? as usize;
            if n < 3 {
                return Err(DecodeError::bad_payload());
            }
            let mut points = Vec::with_capacity(n);
            for _ in 0..n {
                points.push(r.point()?);
            }
            Ok(DrawCmd::OutlineFillPolygon {
                fill,
                stroke,
                line_width,
                points,
            })
        }
        OP_OUTLINE_FILL_RECTANGLES => {
            let fill = read_style(r)?;
            let stroke = read_style(r)?;
            let line_width = validate_line_width(r.f32()?)?;
            let n = r.varu()? as usize;
            let mut rects = Vec::with_capacity(n);
            for _ in 0..n {
                rects.push(r.rect()?);
            }
            Ok(DrawCmd::OutlineFillRectangles {
                fill,
                stroke,
                line_width,
                rects,
            })
        }
        OP_OUTLINE_FILL_PATH => {
            let fill = read_style(r)?;
            let stroke = read_style(r)?;
            let line_width = validate_line_width(r.f32()?)?;
            let segments = read_path_segments(r)?;
            Ok(DrawCmd::OutlineFillPath {
                fill,
                stroke,
                line_width,
                segments,
            })
        }
        OP_DRAW_IMAGE => {
            let target_rect = r.rect()?;
            let image_id = read_id(r, false)?;
            Ok(DrawCmd::DrawImage {
                target_rect,
                image_id,
            })
        }
        _ => Err(DecodeError::bad_payload()),
    }
}

fn read_commands(r: &mut Reader<'_>) -> DecodeResult<Vec<DrawCmd>> {
    let n = r.varu()? as usize;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(read_draw_cmd(r)?);
    }
    Ok(v)
}

pub fn parse(frame_type: u8, body: &[u8]) -> Result<Command, u16> {
    let mut r = Reader::new(body);
    match frame_type {
        CMD_PROBE => {
            // Body is empty (§2.1). Tolerate trailing bytes? Spec doesn't
            // forbid them explicitly but we're strict.
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::Probe)
        }
        CMD_CREATE_ELEMENT => {
            let id = read_id(&mut r, true)?;
            let commands = read_commands(&mut r)?;
            let origin = r.point()?;
            let is_visible = r.u8()? != 0;
            let draw_order = r.i32()?;
            // §9.4 optional trailing block. Presence is decided strictly
            // by remaining body bytes — old/v1 clients omit this entirely.
            let mut parent: Option<String> = None;
            let mut size: Option<Point> = None;
            if !r.at_end() {
                let extra_flags = r.u8()?;
                // Reserved bits must be zero.
                if extra_flags & !0b11 != 0 {
                    return Err(ERR_BAD_PAYLOAD);
                }
                if extra_flags & 0b01 != 0 {
                    parent = Some(read_id(&mut r, false)?);
                }
                if extra_flags & 0b10 != 0 {
                    let p = r.point()?;
                    if !p.x.is_finite() || !p.y.is_finite() || p.x < 0.0 || p.y < 0.0 {
                        return Err(ERR_BAD_PAYLOAD);
                    }
                    size = Some(p);
                }
            }
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::CreateElement(CreateElementBody {
                id,
                commands,
                origin,
                is_visible,
                draw_order,
                parent,
                size,
            }))
        }
        CMD_DELETE_ELEMENT => {
            let id = read_id(&mut r, false)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::DeleteElement { id })
        }
        CMD_UPDATE_COMMANDS => {
            let id = read_id(&mut r, false)?;
            let commands = read_commands(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateCommands(UpdateCommandsBody { id, commands }))
        }
        CMD_UPDATE_COMMAND => {
            let id = read_id(&mut r, false)?;
            let index = r.varu()? as usize;
            let command = read_draw_cmd(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateCommand(UpdateCommandBody {
                id,
                index,
                command,
            }))
        }
        CMD_UPDATE_TEXT => {
            let id = read_id(&mut r, false)?;
            let command_index = r.varu()? as usize;
            let mode = r.u8()?;
            let range = match mode {
                0 => UpdateTextRange::Whole,
                1 => {
                    let start = r.varu()? as usize;
                    let end = r.varu()? as usize;
                    UpdateTextRange::Range { start, end }
                }
                _ => return Err(ERR_BAD_PAYLOAD),
            };
            let replacement = r.string()?.to_owned();
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateText(UpdateTextBody {
                id,
                command_index,
                range,
                replacement,
            }))
        }
        CMD_UPDATE_ORIGIN => {
            let id = read_id(&mut r, false)?;
            let origin = r.point()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateOrigin { id, origin })
        }
        CMD_UPDATE_VISIBILITY => {
            let id = read_id(&mut r, false)?;
            let is_visible = r.u8()? != 0;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateVisibility { id, is_visible })
        }
        CMD_UPDATE_DRAW_ORDER => {
            let id = read_id(&mut r, false)?;
            let draw_order = r.i32()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateDrawOrder { id, draw_order })
        }
        CMD_CLEAR_ALL => {
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::ClearAll)
        }
        CMD_SET_GLOBAL_STYLE => {
            let id = read_id(&mut r, false)?;
            let style = read_concrete_style(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::SetGlobalStyle { id, style })
        }
        CMD_UPLOAD_IMAGE => {
            let id = read_id(&mut r, false)?;
            let encoding = r.u8()?;
            let width = r.u32()?;
            let height = r.u32()?;
            let data = r.bytes()?.to_vec();
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UploadImage(UploadImageBody {
                id,
                encoding,
                width,
                height,
                data,
            }))
        }
        CMD_DROP_IMAGE => {
            let id = read_id(&mut r, false)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::DropImage { id })
        }
        CMD_UPDATE_IMAGE => {
            let id = read_id(&mut r, false)?;
            let command_index = r.varu()? as usize;
            let new_image_id = read_id(&mut r, false)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateImage(UpdateImageBody {
                id,
                command_index,
                new_image_id,
            }))
        }
        CMD_UPDATE_SIZE => {
            let id = read_id(&mut r, false)?;
            let new_size = r.point()?;
            if !new_size.x.is_finite() || !new_size.y.is_finite()
                || new_size.x < 0.0 || new_size.y < 0.0
            {
                return Err(ERR_BAD_PAYLOAD);
            }
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UpdateSize { id, new_size })
        }
        _ => Err(ERR_UNKNOWN_COMMAND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Writer;

    fn flat_white() -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(STYLE_FLAT);
        w.u8(COLOR_RGBA8888);
        w.u8(0xFF);
        w.u8(0xFF);
        w.u8(0xFF);
        w.u8(0xFF);
        w.buf
    }

    #[test]
    fn probe_round_trip() {
        let cmd = parse(CMD_PROBE, &[]).unwrap();
        assert!(matches!(cmd, Command::Probe));
    }

    #[test]
    fn create_element_with_rect() {
        let mut w = Writer::new();
        w.str("foo");
        w.varu(1); // n_commands
        w.u8(OP_FILL_RECTANGLES);
        w.buf.extend_from_slice(&flat_white());
        w.varu(1); // n_rects
        w.f32(0.0);
        w.f32(0.0);
        w.f32(2.0);
        w.f32(1.0);
        // origin
        w.f32(5.0);
        w.f32(3.0);
        // is_visible
        w.u8(1);
        // draw_order
        for b in 0i32.to_le_bytes() {
            w.u8(b);
        }
        let cmd = parse(CMD_CREATE_ELEMENT, &w.buf).unwrap();
        match cmd {
            Command::CreateElement(b) => {
                assert_eq!(b.id, "foo");
                assert_eq!(b.commands.len(), 1);
                assert_eq!(b.origin, Point { x: 5.0, y: 3.0 });
                assert!(b.is_visible);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unknown_opcode_in_create_fails() {
        let mut w = Writer::new();
        w.str("a");
        w.varu(1);
        w.u8(0x99); // unknown op
        // The parse for this opcode will fail before reading the rest of
        // the body; that's what we want.
        assert!(matches!(parse(CMD_CREATE_ELEMENT, &w.buf), Err(ERR_BAD_PAYLOAD)));
    }

    #[test]
    fn unknown_command_returns_error() {
        assert!(matches!(parse(0x77, &[]), Err(ERR_UNKNOWN_COMMAND)));
    }

    fn linear_grad_bytes() -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(STYLE_LINEAR_GRADIENT);
        w.f32(0.0);
        w.f32(0.0); // p0
        w.f32(1.0);
        w.f32(1.0); // p1
        w.u8(COLOR_RGBA8888);
        w.u8(0xFF);
        w.u8(0x00);
        w.u8(0x00);
        w.u8(0xFF); // c0 red
        w.u8(COLOR_RGBA8888);
        w.u8(0x00);
        w.u8(0x00);
        w.u8(0xFF);
        w.u8(0xFF); // c1 blue
        w.buf
    }

    #[test]
    fn linear_gradient_decodes() {
        // Wrap the style inside a FillRectangles command so we hit the
        // top-level read_draw_cmd entry point.
        let mut w = Writer::new();
        w.u8(OP_FILL_RECTANGLES);
        w.buf.extend_from_slice(&linear_grad_bytes());
        w.varu(0);
        let mut r = Reader::new(&w.buf);
        let cmd = read_draw_cmd(&mut r).unwrap();
        match cmd {
            DrawCmd::FillRectangles {
                fill: Style::LinearGradient { c0, c1, .. },
                ..
            } => {
                assert!((c0.r - 1.0).abs() < 1e-3);
                assert!((c1.b - 1.0).abs() < 1e-3);
            }
            _ => panic!("expected linear gradient"),
        }
    }

    #[test]
    fn ref_style_decodes() {
        let mut w = Writer::new();
        w.u8(STYLE_REF);
        w.str("accent");
        let mut r = Reader::new(&w.buf);
        let s = read_style(&mut r).unwrap();
        match s {
            Style::Ref(id) => assert_eq!(id, "accent"),
            _ => panic!("expected ref"),
        }
    }

    #[test]
    fn empty_ref_id_rejected() {
        let mut w = Writer::new();
        w.u8(STYLE_REF);
        w.str("");
        let mut r = Reader::new(&w.buf);
        assert!(read_style(&mut r).is_err());
    }

    #[test]
    fn set_global_style_with_flat() {
        let mut w = Writer::new();
        w.str("theme/accent");
        w.u8(STYLE_FLAT);
        w.u8(COLOR_RGBA8888);
        w.u8(0xFF);
        w.u8(0xAA);
        w.u8(0x33);
        w.u8(0xFF);
        let cmd = parse(CMD_SET_GLOBAL_STYLE, &w.buf).unwrap();
        match cmd {
            Command::SetGlobalStyle { id, style } => {
                assert_eq!(id, "theme/accent");
                assert!(matches!(style, ConcreteStyle::Flat(_)));
            }
            _ => panic!("expected set_global_style"),
        }
    }

    #[test]
    fn set_global_style_rejects_ref() {
        let mut w = Writer::new();
        w.str("a");
        w.u8(STYLE_REF);
        w.str("b");
        assert!(matches!(
            parse(CMD_SET_GLOBAL_STYLE, &w.buf),
            Err(ERR_BAD_PAYLOAD)
        ));
    }

    #[test]
    fn fill_polygon_min_3_points() {
        let mut w = Writer::new();
        w.u8(OP_FILL_POLYGON);
        w.buf.extend_from_slice(&flat_white());
        w.varu(2); // only 2 points
        w.f32(0.0);
        w.f32(0.0);
        w.f32(1.0);
        w.f32(1.0);
        let mut r = Reader::new(&w.buf);
        assert!(read_draw_cmd(&mut r).is_err());
    }

    #[test]
    fn line_width_negative_rejected() {
        let mut w = Writer::new();
        w.u8(OP_DRAW_LINES);
        w.buf.extend_from_slice(&flat_white());
        w.f32(-0.5); // negative line_width
        w.varu(0);
        let mut r = Reader::new(&w.buf);
        assert!(read_draw_cmd(&mut r).is_err());
    }

    #[test]
    fn outline_fill_path_decodes() {
        let mut w = Writer::new();
        w.u8(OP_OUTLINE_FILL_PATH);
        w.buf.extend_from_slice(&flat_white()); // fill
        w.buf.extend_from_slice(&flat_white()); // stroke
        w.f32(0.1); // line_width
        w.varu(1); // n_segments
        w.f32(0.0);
        w.f32(0.0); // start
        w.varu(1); // n_nodes
        w.u8(0); // LineTo
        w.f32(1.0);
        w.f32(1.0);
        let mut r = Reader::new(&w.buf);
        let cmd = read_draw_cmd(&mut r).unwrap();
        assert!(matches!(cmd, DrawCmd::OutlineFillPath { .. }));
    }
}
