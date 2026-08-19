/// A parser for terminal output which produces an in-memory representation of
/// the terminal contents.
pub struct Parser<CB: crate::callbacks::Callbacks = ()> {
    parser: vte::Parser,
    screen: crate::perform::WrappedScreen<CB>,
    /// Trailing bytes of an incomplete UTF-8 sequence, held back from the
    /// end of the last `process` call.
    ///
    /// vte 0.15's own partial-codepoint path drops a byte: split a 2-byte
    /// codepoint after its lead byte and let the next call start with
    /// `<continuation> <one ASCII byte> <multi-byte lead>`, and
    /// `advance_partial_utf8` copies all three, prints only the codepoint,
    /// and reports `valid_up_to()` — 3 — as consumed. The ASCII byte in
    /// the middle is swallowed. Callers feed us whatever a `read(2)` from
    /// a PTY returned, so that boundary lands mid-codepoint routinely and
    /// a space, a letter or a newline disappears out of the middle of the
    /// stream. Never handing vte a chunk that ends mid-sequence keeps that
    /// path unreachable.
    utf8_tail: Vec<u8>,
}

impl Parser {
    /// Creates a new terminal parser of the given size and with the given
    /// amount of scrollback.
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback_len: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: crate::perform::WrappedScreen::new(
                rows,
                cols,
                scrollback_len,
            ),
            utf8_tail: Vec::new(),
        }
    }
}

impl<CB: crate::callbacks::Callbacks> Parser<CB> {
    /// Creates a new terminal parser of the given size and with the given
    /// amount of scrollback. Terminal events will be reported via method
    /// calls on the provided [`Callbacks`](crate::callbacks::Callbacks)
    /// implementation.
    pub fn new_with_callbacks(
        rows: u16,
        cols: u16,
        scrollback_len: usize,
        callbacks: CB,
    ) -> Self {
        Self {
            parser: vte::Parser::new(),
            screen: crate::perform::WrappedScreen::new_with_callbacks(
                rows,
                cols,
                scrollback_len,
                callbacks,
            ),
            utf8_tail: Vec::new(),
        }
    }

    /// Processes the contents of the given byte string, and updates the
    /// in-memory terminal state.
    pub fn process(&mut self, bytes: &[u8]) {
        if self.utf8_tail.is_empty() {
            self.advance_whole_codepoints(bytes);
        } else {
            // At most 3 held bytes, and only when the previous chunk
            // ended mid-codepoint, so the join stays a tiny copy.
            let mut joined = std::mem::take(&mut self.utf8_tail);
            joined.extend_from_slice(bytes);
            self.advance_whole_codepoints(&joined);
        }
    }

    /// Hand vte everything up to the last codepoint boundary and keep
    /// the truncated tail, if any, for the next call.
    fn advance_whole_codepoints(&mut self, bytes: &[u8]) {
        let keep = complete_prefix_len(bytes);
        self.parser.advance(&mut self.screen, &bytes[..keep]);
        self.utf8_tail.extend_from_slice(&bytes[keep..]);
    }

    /// Returns a reference to a [`Screen`](crate::Screen) object containing
    /// the terminal state.
    #[must_use]
    pub fn screen(&self) -> &crate::Screen {
        &self.screen.screen
    }

    /// Returns a mutable reference to a [`Screen`](crate::Screen) object
    /// containing the terminal state.
    #[must_use]
    pub fn screen_mut(&mut self) -> &mut crate::Screen {
        &mut self.screen.screen
    }

    /// Returns a reference to the [`Callbacks`](crate::callbacks::Callbacks)
    /// state object passed into the constructor.
    pub fn callbacks(&self) -> &CB {
        &self.screen.callbacks
    }

    /// Returns a mutable reference to the
    /// [`Callbacks`](crate::callbacks::Callbacks) state object passed into
    /// the constructor.
    pub fn callbacks_mut(&mut self) -> &mut CB {
        &mut self.screen.callbacks
    }
}

impl Default for Parser {
    /// Returns a parser with dimensions 80x24 and no scrollback.
    fn default() -> Self {
        Self::new(24, 80, 0)
    }
}

impl std::io::Write for Parser {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.process(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How many bytes of `bytes` end on a codepoint boundary — i.e. the
/// length of the prefix that carries no truncated UTF-8 sequence.
///
/// Only the last three bytes can begin one, so this looks back that far
/// for a lead byte and compares the bytes after it against the length
/// that lead announces. Anything that is not a truncated sequence
/// (ASCII, a complete sequence, or malformed bytes vte should turn into
/// a replacement character) is left in the prefix and passed straight
/// through.
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=n.min(3) {
        let b = bytes[n - back];
        // Continuation byte: keep walking back toward its lead.
        if b & 0xC0 == 0x80 {
            continue;
        }
        let need = match b {
            0x00..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            // Malformed lead (or a stray continuation run): not a
            // truncation, so hand it over and let vte report it.
            _ => return n,
        };
        return if back < need { n - back } else { n };
    }
    // Three trailing continuation bytes with no lead in reach: malformed.
    n
}

#[cfg(test)]
mod tests {
    /// A `process` boundary may fall anywhere in the byte stream — a PTY
    /// read returns whatever the kernel had — so no split of the same
    /// bytes may change the screen. It used to: vte drops the byte
    /// between a split 2-byte codepoint and a following multi-byte one
    /// (see the `utf8_tail` doc), which is how a space vanished out of
    /// the middle of a line of prose.
    fn every_split_matches_whole(s: &str) {
        let bytes = s.as_bytes();
        let mut whole = super::Parser::new(4, 60, 0);
        whole.process(bytes);
        let want = whole.screen().contents();
        for split in 1..bytes.len() {
            let mut p = super::Parser::new(4, 60, 0);
            p.process(&bytes[..split]);
            p.process(&bytes[split..]);
            assert_eq!(
                p.screen().contents(),
                want,
                "split at byte {split} of {s:?} changed the screen"
            );
        }
        // The pathological chunking: one byte at a time.
        let mut p = super::Parser::new(4, 60, 0);
        for b in bytes {
            p.process(&[*b]);
        }
        assert_eq!(p.screen().contents(), want, "bytewise feed of {s:?}");
    }

    #[test]
    fn utf8_survives_any_chunk_boundary() {
        // The regression: 2-byte codepoint, one ASCII byte, multi-byte
        // codepoint. Latin-1 accents, Cyrillic and the ×/° class of
        // symbols are all 2-byte, and " — " is 3-byte.
        every_split_matches_whole("café — naïve");
        every_split_matches_whole("é —");
        every_split_matches_whole("— ’ … × ✓");
        every_split_matches_whole("слово — другое слово");
        // Widths, 4-byte codepoints and combining marks, since the same
        // boundary logic carries them.
        every_split_matches_whole("a🚀b 日本語 e\u{301}x");
        // Escape sequences either side of the boundary.
        every_split_matches_whole("\x1b[1mé —\x1b[0m ok");
    }

    #[test]
    fn complete_prefix_len_holds_only_truncated_tails() {
        use super::complete_prefix_len;
        assert_eq!(complete_prefix_len(b"abc"), 3);
        assert_eq!(complete_prefix_len("é".as_bytes()), 2);
        // Lead byte alone, or a sequence one byte short.
        assert_eq!(complete_prefix_len(b"ab\xc3"), 2);
        assert_eq!(complete_prefix_len(b"ab\xe2\x80"), 2);
        assert_eq!(complete_prefix_len(b"ab\xf0\x9f\x9a"), 2);
        // Malformed bytes are vte's to report, not ours to hold.
        assert_eq!(complete_prefix_len(b"ab\xff"), 3);
        assert_eq!(complete_prefix_len(b"ab\x80\x80\x80"), 5);
        assert_eq!(complete_prefix_len(b""), 0);
    }
}
