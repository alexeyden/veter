//! Character-set translation for the SCS (Select Character Set) escape
//! sequences. We track the four designations G0..G3 and the GL pointer
//! (which one of them maps onto the printable ASCII range). SO/SI flip GL
//! between G0 and G1; `ESC ( <c>` / `ESC ) <c>` / `ESC * <c>` / `ESC + <c>`
//! redesignate G0..G3 respectively. Only `B` (ASCII) and `0` (DEC Special
//! Graphics) are recognised — every other code falls back to ASCII, which
//! is how xterm treats unknown SCS designations.

#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum Charset {
    #[default]
    Ascii,
    DecSpecialGraphics,
}

impl Charset {
    pub fn from_designation(code: u8) -> Self {
        match code {
            b'0' => Self::DecSpecialGraphics,
            // `B` is ASCII; everything else (UK, NRCS variants, …) is not
            // implemented and falls back to ASCII.
            _ => Self::Ascii,
        }
    }

    /// Translate a single printed codepoint through this set.
    pub fn translate(self, c: char) -> char {
        match self {
            Self::Ascii => c,
            Self::DecSpecialGraphics => match c {
                '`' => '\u{25C6}',
                'a' => '\u{2592}',
                'b' => '\u{2409}',
                'c' => '\u{240C}',
                'd' => '\u{240D}',
                'e' => '\u{240A}',
                'f' => '\u{00B0}',
                'g' => '\u{00B1}',
                'h' => '\u{2424}',
                'i' => '\u{240B}',
                'j' => '\u{2518}',
                'k' => '\u{2510}',
                'l' => '\u{250C}',
                'm' => '\u{2514}',
                'n' => '\u{253C}',
                'o' => '\u{23BA}',
                'p' => '\u{23BB}',
                'q' => '\u{2500}',
                'r' => '\u{23BC}',
                's' => '\u{23BD}',
                't' => '\u{251C}',
                'u' => '\u{2524}',
                'v' => '\u{2534}',
                'w' => '\u{252C}',
                'x' => '\u{2502}',
                'y' => '\u{2264}',
                'z' => '\u{2265}',
                '{' => '\u{03C0}',
                '|' => '\u{2260}',
                '}' => '\u{00A3}',
                '~' => '\u{00B7}',
                _ => c,
            },
        }
    }
}

/// The four G-set designations plus the GL pointer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CharsetState {
    pub sets: [Charset; 4],
    pub gl: u8,
}

impl Default for CharsetState {
    fn default() -> Self {
        Self {
            sets: [Charset::Ascii; 4],
            gl: 0,
        }
    }
}

impl CharsetState {
    pub fn translate(&self, c: char) -> char {
        // GL is only meaningful for printable ASCII (0x20..0x7F). Everything
        // else (including codepoints arriving via UTF-8) bypasses the table
        // unchanged — this matches xterm's behaviour and avoids corrupting
        // multibyte input when a program forgets to switch back to ASCII.
        if matches!(c, '\u{21}'..='\u{7E}') {
            self.sets[usize::from(self.gl)].translate(c)
        } else {
            c
        }
    }

    /// Designate G0..G3 (selector index 0..=3) to a charset code (e.g.
    /// `b'0'`, `b'B'`). Out-of-range selectors are ignored.
    pub fn designate(&mut self, selector: u8, code: u8) {
        if let Some(slot) = self.sets.get_mut(usize::from(selector)) {
            *slot = Charset::from_designation(code);
        }
    }

    /// SO — select G1 as GL.
    pub fn shift_out(&mut self) {
        self.gl = 1;
    }

    /// SI — select G0 as GL.
    pub fn shift_in(&mut self) {
        self.gl = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dec_special_graphics_table() {
        let cs = Charset::DecSpecialGraphics;
        assert_eq!(cs.translate('q'), '─');
        assert_eq!(cs.translate('x'), '│');
        assert_eq!(cs.translate('l'), '┌');
        assert_eq!(cs.translate('k'), '┐');
        assert_eq!(cs.translate('m'), '└');
        assert_eq!(cs.translate('j'), '┘');
        assert_eq!(cs.translate('w'), '┬');
        assert_eq!(cs.translate('v'), '┴');
        assert_eq!(cs.translate('t'), '├');
        assert_eq!(cs.translate('u'), '┤');
        assert_eq!(cs.translate('n'), '┼');
        // outside the 0x60..=0x7E window — passthrough
        assert_eq!(cs.translate('A'), 'A');
        assert_eq!(cs.translate(' '), ' ');
    }

    #[test]
    fn ascii_is_passthrough() {
        let cs = Charset::Ascii;
        for c in ['q', 'x', 'l', 'A', ' ', '~'] {
            assert_eq!(cs.translate(c), c);
        }
    }

    #[test]
    fn unknown_designation_falls_back_to_ascii() {
        // `A` is UK; we don't implement it. Must not corrupt printed text.
        assert_eq!(Charset::from_designation(b'A'), Charset::Ascii);
        assert_eq!(Charset::from_designation(b'B'), Charset::Ascii);
        assert_eq!(Charset::from_designation(b'0'), Charset::DecSpecialGraphics);
    }

    #[test]
    fn shifts_swap_gl() {
        let mut state = CharsetState::default();
        state.designate(1, b'0');
        // Default GL is G0 (ASCII) — no translation.
        assert_eq!(state.translate('q'), 'q');
        state.shift_out();
        assert_eq!(state.translate('q'), '─');
        state.shift_in();
        assert_eq!(state.translate('q'), 'q');
    }
}

