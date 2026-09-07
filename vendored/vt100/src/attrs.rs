use crate::term::BufWrite as _;

/// Represents a foreground or background color for cells.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum Color {
    /// The default terminal color.
    #[default]
    Default,

    /// An indexed terminal color.
    Idx(u8),

    /// An RGB terminal color. The parameters are (red, green, blue).
    Rgb(u8, u8, u8),
}

/// The shape of a cell's underline.
///
/// SGR `4` selects [`Single`](Self::Single) and `21` selects
/// [`Double`](Self::Double); the rest arrive as the ITU-T T.416
/// subparameter form `4:n`, which is how nvim and every LSP client
/// draw diagnostics. `24` turns the underline off entirely and is
/// modelled as the absence of a style, not a sixth variant.
#[derive(Eq, PartialEq, Debug, Copy, Clone, Default)]
pub enum UnderlineStyle {
    /// `SGR 4`, or `4:1`.
    #[default]
    Single,
    /// `SGR 21`, or `4:2`.
    Double,
    /// `4:3` — the squiggle a spell-checker or a diagnostic draws.
    Curly,
    /// `4:4`.
    Dotted,
    /// `4:5`.
    Dashed,
}

// `mode` is a `u16` and every bit below is spoken for by an SGR the
// terminfo entry for xterm-256color actually emits. It was a `u8`
// until the underline styles arrived and needed three bits of their
// own; widening it cost one byte per `Cell`, paid for by dropping
// `CONTENT_BYTES` from 22 to 21 so the struct stays 32 bytes.
const TEXT_MODE_INTENSITY: u16 = 0b0000_0000_0000_0011;
const TEXT_MODE_BOLD: u16 = 0b0000_0000_0000_0001;
const TEXT_MODE_DIM: u16 = 0b0000_0000_0000_0010;
const TEXT_MODE_ITALIC: u16 = 0b0000_0000_0000_0100;
const TEXT_MODE_UNDERLINE: u16 = 0b0000_0000_0000_1000;
const TEXT_MODE_INVERSE: u16 = 0b0000_0000_0001_0000;
const TEXT_MODE_BLINK: u16 = 0b0000_0000_0010_0000;
const TEXT_MODE_CONCEAL: u16 = 0b0000_0000_0100_0000;
const TEXT_MODE_STRIKETHROUGH: u16 = 0b0000_0000_1000_0000;
const TEXT_MODE_OVERLINE: u16 = 0b0000_0001_0000_0000;
/// Which of the five [`UnderlineStyle`]s, when `TEXT_MODE_UNDERLINE`
/// says there is one at all. Zero — the default — is `Single`, so a
/// plain `SGR 4` needs no style bits set.
const TEXT_MODE_UNDERLINE_STYLE: u16 = 0b0000_1110_0000_0000;
const TEXT_MODE_UNDERLINE_STYLE_SHIFT: u32 = 9;

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub fgcolor: Color,
    pub bgcolor: Color,
    pub mode: u16,
}

impl Attrs {
    pub fn bold(&self) -> bool {
        self.mode & TEXT_MODE_BOLD != 0
    }

    pub fn dim(&self) -> bool {
        self.mode & TEXT_MODE_DIM != 0
    }

    fn intensity(&self) -> u16 {
        self.mode & TEXT_MODE_INTENSITY
    }

    pub fn set_bold(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
        self.mode |= TEXT_MODE_BOLD;
    }

    pub fn set_dim(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
        self.mode |= TEXT_MODE_DIM;
    }

    pub fn set_normal_intensity(&mut self) {
        self.mode &= !TEXT_MODE_INTENSITY;
    }

    pub fn italic(&self) -> bool {
        self.mode & TEXT_MODE_ITALIC != 0
    }

    pub fn set_italic(&mut self, italic: bool) {
        self.set_flag(TEXT_MODE_ITALIC, italic);
    }

    pub fn underline(&self) -> bool {
        self.mode & TEXT_MODE_UNDERLINE != 0
    }

    /// The underline's shape, or `None` when there is no underline.
    pub fn underline_style(&self) -> Option<UnderlineStyle> {
        if !self.underline() {
            return None;
        }
        Some(
            match (self.mode & TEXT_MODE_UNDERLINE_STYLE)
                >> TEXT_MODE_UNDERLINE_STYLE_SHIFT
            {
                1 => UnderlineStyle::Double,
                2 => UnderlineStyle::Curly,
                3 => UnderlineStyle::Dotted,
                4 => UnderlineStyle::Dashed,
                // 0, and anything a future encoder might store that
                // this build doesn't know: a plain underline is the
                // safe reading, since the cell is underlined either way.
                _ => UnderlineStyle::Single,
            },
        )
    }

    /// `None` turns the underline off; `Some(style)` turns it on with
    /// that shape.
    pub fn set_underline_style(&mut self, style: Option<UnderlineStyle>) {
        self.mode &= !TEXT_MODE_UNDERLINE_STYLE;
        match style {
            None => self.mode &= !TEXT_MODE_UNDERLINE,
            Some(style) => {
                self.mode |= TEXT_MODE_UNDERLINE;
                let bits = match style {
                    UnderlineStyle::Single => 0,
                    UnderlineStyle::Double => 1,
                    UnderlineStyle::Curly => 2,
                    UnderlineStyle::Dotted => 3,
                    UnderlineStyle::Dashed => 4,
                };
                self.mode |= bits << TEXT_MODE_UNDERLINE_STYLE_SHIFT;
            }
        }
    }

    /// Turn a plain single underline on or off, the `SGR 4` / `SGR 24`
    /// pair.
    pub fn set_underline(&mut self, underline: bool) {
        self.set_underline_style(underline.then_some(UnderlineStyle::Single));
    }

    pub fn inverse(&self) -> bool {
        self.mode & TEXT_MODE_INVERSE != 0
    }

    pub fn set_inverse(&mut self, inverse: bool) {
        self.set_flag(TEXT_MODE_INVERSE, inverse);
    }

    pub fn blink(&self) -> bool {
        self.mode & TEXT_MODE_BLINK != 0
    }

    pub fn set_blink(&mut self, blink: bool) {
        self.set_flag(TEXT_MODE_BLINK, blink);
    }

    pub fn conceal(&self) -> bool {
        self.mode & TEXT_MODE_CONCEAL != 0
    }

    pub fn set_conceal(&mut self, conceal: bool) {
        self.set_flag(TEXT_MODE_CONCEAL, conceal);
    }

    pub fn strikethrough(&self) -> bool {
        self.mode & TEXT_MODE_STRIKETHROUGH != 0
    }

    pub fn set_strikethrough(&mut self, strikethrough: bool) {
        self.set_flag(TEXT_MODE_STRIKETHROUGH, strikethrough);
    }

    pub fn overline(&self) -> bool {
        self.mode & TEXT_MODE_OVERLINE != 0
    }

    pub fn set_overline(&mut self, overline: bool) {
        self.set_flag(TEXT_MODE_OVERLINE, overline);
    }

    fn set_flag(&mut self, bit: u16, on: bool) {
        if on {
            self.mode |= bit;
        } else {
            self.mode &= !bit;
        }
    }

    pub fn write_escape_code_diff(
        &self,
        contents: &mut Vec<u8>,
        other: &Self,
    ) {
        if self != other && self == &Self::default() {
            crate::term::ClearAttrs.write_buf(contents);
            return;
        }

        let attrs = crate::term::Attrs::default();

        let attrs = if self.fgcolor == other.fgcolor {
            attrs
        } else {
            attrs.fgcolor(self.fgcolor)
        };
        let attrs = if self.bgcolor == other.bgcolor {
            attrs
        } else {
            attrs.bgcolor(self.bgcolor)
        };
        let attrs = if self.intensity() == other.intensity() {
            attrs
        } else {
            attrs.intensity(match self.intensity() {
                0 => crate::term::Intensity::Normal,
                TEXT_MODE_BOLD => crate::term::Intensity::Bold,
                TEXT_MODE_DIM => crate::term::Intensity::Dim,
                _ => unreachable!(),
            })
        };
        let attrs = if self.italic() == other.italic() {
            attrs
        } else {
            attrs.italic(self.italic())
        };
        let attrs = if self.underline_style() == other.underline_style() {
            attrs
        } else {
            attrs.underline(self.underline_style())
        };
        let attrs = if self.inverse() == other.inverse() {
            attrs
        } else {
            attrs.inverse(self.inverse())
        };
        let attrs = if self.blink() == other.blink() {
            attrs
        } else {
            attrs.blink(self.blink())
        };
        let attrs = if self.conceal() == other.conceal() {
            attrs
        } else {
            attrs.conceal(self.conceal())
        };
        let attrs = if self.strikethrough() == other.strikethrough() {
            attrs
        } else {
            attrs.strikethrough(self.strikethrough())
        };
        let attrs = if self.overline() == other.overline() {
            attrs
        } else {
            attrs.overline(self.overline())
        };

        attrs.write_buf(contents);
    }
}
