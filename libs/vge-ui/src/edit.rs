//! A single-line text editor with a readline-flavored keybinding set —
//! the input line behind rename prompts, command palettes, and any other
//! "type a string and press Enter" modal.
//!
//! Two input paths are offered and bind identically: [`LineEditor::feed`]
//! consumes raw terminal bytes, and [`LineEditor::feed_event`] takes an
//! already-parsed [`crate::input::Event`], for clients that route
//! everything through an [`crate::input::InputParser`].
//!
//! Byte input is ASCII-only (non-ASCII / non-printable bytes are
//! dropped); `feed_event` accepts whatever character the parser decoded.
//! Every operation routes through char positions, so the buffer stays
//! UTF-8-correct either way.

use crate::input::{Dir, Event, Nav};

/// Result of feeding input bytes to a [`LineEditor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// Nothing visible changed (e.g. an unrecognised / incomplete key).
    Noop,
    /// Buffer or cursor moved — the modal should be re-rendered.
    Redraw,
    /// Enter pressed — commit the current buffer.
    Commit,
    /// Esc / Ctrl+G pressed — abandon the edit.
    Cancel,
}

/// Default cap for a short name field (the historical rename limit).
pub const NAME_MAX_CHARS: usize = 32;
/// Default cap for a command line — roomier, so a full
/// `rename-tab 2 some longer name` fits in a palette.
pub const COMMAND_MAX_CHARS: usize = 256;

#[derive(Debug, Clone)]
pub struct LineEditor {
    /// The edited text.
    pub buffer: String,
    /// Insertion point as a char index in `0..=char_count`.
    pub cursor: usize,
    /// Max length, in chars. Inserts past this are dropped.
    pub max: usize,
}

impl LineEditor {
    /// Start editing `buffer` with the cursor at its end, capped at
    /// [`NAME_MAX_CHARS`].
    pub fn new(buffer: String) -> Self {
        Self::with_max(buffer, NAME_MAX_CHARS)
    }

    /// Start editing `buffer` with an explicit max length.
    pub fn with_max(buffer: String, max: usize) -> Self {
        let cursor = buffer.chars().count();
        LineEditor {
            buffer,
            cursor,
            max,
        }
    }

    pub fn char_count(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Byte offset of char index `idx` (or buffer end if past the end).
    fn byte_offset(&self, idx: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(idx)
            .map(|(i, _)| i)
            .unwrap_or(self.buffer.len())
    }

    /// Remove chars in the half-open range `[a, b)`. Caller fixes up the
    /// cursor afterwards.
    fn delete_range(&mut self, a: usize, b: usize) {
        if a >= b {
            return;
        }
        self.buffer = self
            .buffer
            .chars()
            .enumerate()
            .filter(|(i, _)| *i < a || *i >= b)
            .map(|(_, c)| c)
            .collect();
    }

    pub fn insert(&mut self, c: char) {
        if self.char_count() >= self.max {
            return;
        }
        let at = self.byte_offset(self.cursor);
        self.buffer.insert(at, c);
        self.cursor += 1;
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.char_count() {
            self.cursor += 1;
        }
    }

    /// Char index of the previous word start (alphanumeric-delimited).
    pub fn prev_word(&self) -> usize {
        let chars: Vec<char> = self.buffer.chars().collect();
        let mut i = self.cursor;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    /// Char index of the next word end (alphanumeric-delimited).
    pub fn next_word(&self) -> usize {
        let chars: Vec<char> = self.buffer.chars().collect();
        let n = chars.len();
        let mut i = self.cursor;
        while i < n && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < n && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.delete_range(self.cursor - 1, self.cursor);
            self.cursor -= 1;
        }
    }

    pub fn delete_forward(&mut self) {
        self.delete_range(self.cursor, self.cursor + 1);
    }

    pub fn kill_to_end(&mut self) {
        self.delete_range(self.cursor, self.char_count());
    }

    pub fn kill_to_start(&mut self) {
        self.delete_range(0, self.cursor);
        self.cursor = 0;
    }

    pub fn kill_word_back(&mut self) {
        let start = self.prev_word();
        self.delete_range(start, self.cursor);
        self.cursor = start;
    }

    pub fn kill_word_forward(&mut self) {
        let end = self.next_word();
        self.delete_range(self.cursor, end);
    }

    /// Consume one keystroke from the front of `bytes`, returning how
    /// many bytes were consumed and the resulting outcome. A whole
    /// escape sequence is consumed at once when one is present in this
    /// read, so a lone trailing Esc still reads as an immediate cancel.
    pub fn feed(&mut self, bytes: &[u8]) -> (usize, EditOutcome) {
        let Some(&b) = bytes.first() else {
            return (1, EditOutcome::Noop);
        };
        match b {
            b'\r' | b'\n' => (1, EditOutcome::Commit),
            0x1B => self.feed_escape(bytes),
            0x01 => {
                self.cursor = 0; // Ctrl+A
                (1, EditOutcome::Redraw)
            }
            0x05 => {
                self.cursor = self.char_count(); // Ctrl+E
                (1, EditOutcome::Redraw)
            }
            0x02 => {
                self.move_left(); // Ctrl+B
                (1, EditOutcome::Redraw)
            }
            0x06 => {
                self.move_right(); // Ctrl+F
                (1, EditOutcome::Redraw)
            }
            0x04 => {
                self.delete_forward(); // Ctrl+D
                (1, EditOutcome::Redraw)
            }
            0x08 | 0x7F => {
                self.backspace(); // Ctrl+H / DEL
                (1, EditOutcome::Redraw)
            }
            0x0B => {
                self.kill_to_end(); // Ctrl+K
                (1, EditOutcome::Redraw)
            }
            0x15 => {
                self.kill_to_start(); // Ctrl+U
                (1, EditOutcome::Redraw)
            }
            0x17 => {
                self.kill_word_back(); // Ctrl+W
                (1, EditOutcome::Redraw)
            }
            0x07 => (1, EditOutcome::Cancel), // Ctrl+G
            0x20..=0x7E => {
                self.insert(b as char);
                (1, EditOutcome::Redraw)
            }
            _ => (1, EditOutcome::Noop),
        }
    }

    /// Apply one already-parsed [`Event`], for clients that route all
    /// input through an [`InputParser`] rather than feeding raw bytes.
    /// The bindings match [`LineEditor::feed`] exactly.
    ///
    /// [`InputParser`]: crate::input::InputParser
    pub fn feed_event(&mut self, ev: Event) -> EditOutcome {
        match ev {
            Event::Enter => EditOutcome::Commit,
            Event::Escape => EditOutcome::Cancel,
            Event::Ctrl('g') => EditOutcome::Cancel,
            Event::Key(c) => {
                self.insert(c);
                EditOutcome::Redraw
            }
            Event::Backspace => {
                self.backspace();
                EditOutcome::Redraw
            }
            Event::Delete => {
                self.delete_forward();
                EditOutcome::Redraw
            }
            Event::Arrow(Dir::Left) => {
                self.move_left();
                EditOutcome::Redraw
            }
            Event::Arrow(Dir::Right) => {
                self.move_right();
                EditOutcome::Redraw
            }
            Event::Nav(Nav::Home) => {
                self.cursor = 0;
                EditOutcome::Redraw
            }
            Event::Nav(Nav::End) => {
                self.cursor = self.char_count();
                EditOutcome::Redraw
            }
            Event::Ctrl(c) => {
                match c {
                    'a' => self.cursor = 0,
                    'e' => self.cursor = self.char_count(),
                    'b' => self.move_left(),
                    'f' => self.move_right(),
                    'd' => self.delete_forward(),
                    'h' => self.backspace(),
                    'k' => self.kill_to_end(),
                    'u' => self.kill_to_start(),
                    'w' => self.kill_word_back(),
                    _ => return EditOutcome::Noop,
                }
                EditOutcome::Redraw
            }
            Event::Alt(c) => {
                match c {
                    'b' | 'B' => self.cursor = self.prev_word(),
                    'f' | 'F' => self.cursor = self.next_word(),
                    'd' | 'D' => self.kill_word_forward(),
                    '\x7f' => self.kill_word_back(),
                    _ => return EditOutcome::Noop,
                }
                EditOutcome::Redraw
            }
            _ => EditOutcome::Noop,
        }
    }

    /// Handle an ESC-introduced sequence: Alt-<key> bindings, CSI/SS3
    /// cursor keys, or a bare Esc (cancel). `bytes[0]` is `0x1B`.
    fn feed_escape(&mut self, bytes: &[u8]) -> (usize, EditOutcome) {
        // Lone Esc with nothing following in this read → cancel.
        let Some(&next) = bytes.get(1) else {
            return (1, EditOutcome::Cancel);
        };
        match next {
            b'b' | b'B' => {
                self.cursor = self.prev_word(); // Alt+B
                (2, EditOutcome::Redraw)
            }
            b'f' | b'F' => {
                self.cursor = self.next_word(); // Alt+F
                (2, EditOutcome::Redraw)
            }
            b'd' | b'D' => {
                self.kill_word_forward(); // Alt+D
                (2, EditOutcome::Redraw)
            }
            0x7F | 0x08 => {
                self.kill_word_back(); // Alt+Backspace
                (2, EditOutcome::Redraw)
            }
            b'[' | b'O' => self.feed_csi(bytes),
            // ESC + unrecognised byte: treat the ESC as a cancel and
            // leave the trailing byte for the next loop iteration.
            _ => (1, EditOutcome::Cancel),
        }
    }

    /// Handle a CSI/SS3 cursor-key sequence. `bytes[0..2]` is `ESC [` or
    /// `ESC O`.
    fn feed_csi(&mut self, bytes: &[u8]) -> (usize, EditOutcome) {
        // Scan to the final byte (0x40..=0x7E) of the sequence.
        let mut i = 2;
        while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            // Incomplete in this read — drop the partial prefix rather
            // than leak raw bytes; a split sequence is rare for these.
            return (bytes.len(), EditOutcome::Noop);
        }
        let params = &bytes[2..i];
        match bytes[i] {
            b'C' => self.move_right(),               // Right
            b'D' => self.move_left(),                // Left
            b'H' => self.cursor = 0,                 // Home
            b'F' => self.cursor = self.char_count(), // End
            b'~' => match params {
                b"1" | b"7" => self.cursor = 0,
                b"4" | b"8" => self.cursor = self.char_count(),
                b"3" => self.delete_forward(), // Delete
                _ => {}
            },
            _ => {}
        }
        (i + 1, EditOutcome::Redraw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed every byte of `keys` to a fresh editor over `start`,
    /// returning the final outcome as a short tag.
    fn feed(start: &str, keys: &[u8]) -> (LineEditor, &'static str) {
        let mut ed = LineEditor::new(start.to_string());
        let mut i = 0;
        let mut last = "noop";
        while i < keys.len() {
            let (n, outcome) = ed.feed(&keys[i..]);
            last = match outcome {
                EditOutcome::Noop => "noop",
                EditOutcome::Redraw => "redraw",
                EditOutcome::Commit => "commit",
                EditOutcome::Cancel => "cancel",
            };
            i += n.max(1);
        }
        (ed, last)
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let (ed, out) = feed("", b"abc");
        assert_eq!(ed.buffer, "abc");
        assert_eq!(ed.cursor, 3);
        assert_eq!(out, "redraw");
    }

    #[test]
    fn enter_commits_and_esc_cancels() {
        assert_eq!(feed("x", b"\r").1, "commit");
        assert_eq!(feed("x", b"\x1b").1, "cancel");
        assert_eq!(feed("x", b"\x07").1, "cancel");
    }

    #[test]
    fn word_motions_and_kills() {
        // Alt+B to the start of "world", then kill it.
        let (ed, _) = feed("hello world", b"\x1bb\x0b");
        assert_eq!(ed.buffer, "hello ");
        let (ed, _) = feed("hello world", b"\x17");
        assert_eq!(ed.buffer, "hello ");
        let (ed, _) = feed("hello world", b"\x01\x1bd");
        assert_eq!(ed.buffer, " world");
    }

    #[test]
    fn ctrl_a_e_and_arrows_move_without_editing() {
        let (ed, _) = feed("abc", b"\x01");
        assert_eq!(ed.cursor, 0);
        let (ed, _) = feed("abc", b"\x01\x1b[C");
        assert_eq!(ed.cursor, 1);
        let (ed, _) = feed("abc", b"\x01\x1b[F");
        assert_eq!(ed.cursor, 3);
        assert_eq!(ed.buffer, "abc");
    }

    #[test]
    fn kill_to_start_and_end() {
        let (ed, _) = feed("abcdef", b"\x01\x1b[C\x1b[C\x0b");
        assert_eq!(ed.buffer, "ab");
        let (ed, _) = feed("abcdef", b"\x01\x1b[C\x1b[C\x15");
        assert_eq!(ed.buffer, "cdef");
    }

    #[test]
    fn inserts_stop_at_the_max_length() {
        let mut ed = LineEditor::with_max(String::new(), 3);
        for c in "abcdef".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.buffer, "abc");
    }

    #[test]
    fn the_event_path_matches_the_byte_path() {
        use crate::input::{Dir, Event, Nav};
        let mut ed = LineEditor::new("hello world".into());
        assert_eq!(ed.feed_event(Event::Ctrl('w')), EditOutcome::Redraw);
        assert_eq!(ed.buffer, "hello ");
        assert_eq!(ed.feed_event(Event::Nav(Nav::Home)), EditOutcome::Redraw);
        assert_eq!(ed.cursor, 0);
        assert_eq!(ed.feed_event(Event::Arrow(Dir::Right)), EditOutcome::Redraw);
        assert_eq!(ed.feed_event(Event::Delete), EditOutcome::Redraw);
        assert_eq!(ed.buffer, "hllo ");
        assert_eq!(ed.feed_event(Event::Enter), EditOutcome::Commit);
        assert_eq!(ed.feed_event(Event::Escape), EditOutcome::Cancel);
        // Non-ASCII typing survives the event path.
        let mut ed = LineEditor::new(String::new());
        ed.feed_event(Event::Key('é'));
        assert_eq!(ed.buffer, "é");
    }

    #[test]
    fn incomplete_escape_is_swallowed_not_leaked() {
        let mut ed = LineEditor::new("ab".into());
        let (n, outcome) = ed.feed(b"\x1b[2");
        assert_eq!(n, 3, "the whole partial sequence is consumed");
        assert_eq!(outcome, EditOutcome::Noop);
        assert_eq!(ed.buffer, "ab");
    }
}
