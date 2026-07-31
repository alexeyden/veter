//! A filterable, selectable modal list — the machinery behind a command
//! palette, a "jump to X" list, or any pick-one-of-many overlay.
//!
//! The picker owns a [`LineEditor`] for its input line, the full item
//! list, the indices currently passing the filter, and the selection.
//! It is generic over a payload type so the consuming app can hang
//! whatever it wants off a row and match on it at commit time; the
//! picker itself never interprets the payload.

use crate::edit::{COMMAND_MAX_CHARS, EditOutcome, LineEditor};
use crate::input::{Dir, Event, Nav};

/// How the input line relates to the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    /// The whole trimmed buffer is the filter. Plain "pick one" lists.
    Whole,
    /// The buffer is a command line: its first whitespace token filters
    /// the list, and the remaining tokens are the selected command's
    /// arguments.
    CommandLine,
}

/// One row in a picker list.
#[derive(Debug, Clone)]
pub struct PickerItem<P> {
    /// Primary text, left-aligned.
    pub label: String,
    /// Secondary text, right-aligned and dimmed (key, args, tab name).
    pub hint: String,
    /// Lowercased haystack the filter matches against.
    pub filter_key: String,
    /// Text Tab-completion puts on the input line. `None` uses `label`.
    pub complete: Option<String>,
    pub payload: P,
}

impl<P> PickerItem<P> {
    /// A row whose label is also its completion and (lowercased) its
    /// filter key.
    pub fn new(label: impl Into<String>, hint: impl Into<String>, payload: P) -> Self {
        let label = label.into();
        let filter_key = label.to_lowercase();
        PickerItem {
            label,
            hint: hint.into(),
            filter_key,
            complete: None,
            payload,
        }
    }

    /// Widen the filter haystack (aliases, summaries) beyond the label.
    pub fn with_filter_key(mut self, key: impl Into<String>) -> Self {
        self.filter_key = key.into();
        self
    }

    /// Set the text Tab-completion fills in, when it differs from the
    /// label.
    pub fn with_complete(mut self, text: impl Into<String>) -> Self {
        self.complete = Some(text.into());
        self
    }
}

/// Result of feeding input to a [`Picker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerOutcome {
    Noop,
    Redraw,
    Commit,
    Cancel,
}

/// Rows a PgUp / PgDn jump moves the selection through.
pub const PICKER_PAGE: usize = 8;

#[derive(Debug)]
pub struct Picker<P> {
    pub title: String,
    pub mode: FilterMode,
    /// When set, digits 1..9 select and immediately commit the item at
    /// that (1-based) position in the *unfiltered* list — matching a
    /// "press the number" cue drawn next to the rows.
    pub digit_commit: bool,
    /// Filter / command input (the same line editor prompts use).
    pub editor: LineEditor,
    /// Full, unfiltered item list.
    pub items: Vec<PickerItem<P>>,
    /// Indices into `items` passing the current filter, in item order.
    pub matches: Vec<usize>,
    /// Selection as an index into `matches`.
    pub selected: usize,
}

impl<P> Picker<P> {
    pub fn new(title: impl Into<String>, mode: FilterMode, items: Vec<PickerItem<P>>) -> Self {
        let mut p = Picker {
            title: title.into(),
            mode,
            digit_commit: false,
            editor: LineEditor::with_max(String::new(), COMMAND_MAX_CHARS),
            items,
            matches: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Enable the digit-commit shortcut (see [`Picker::digit_commit`]).
    pub fn with_digit_commit(mut self) -> Self {
        self.digit_commit = true;
        self
    }

    /// The substring the list filters on: the first token for a command
    /// line, the whole trimmed buffer otherwise.
    pub fn filter_text(&self) -> String {
        let buf = self.editor.buffer.trim_start();
        match self.mode {
            FilterMode::CommandLine => buf.split_whitespace().next().unwrap_or("").to_string(),
            FilterMode::Whole => buf.trim_end().to_string(),
        }
    }

    /// The argument tokens after the command name, for a command line.
    pub fn args(&self) -> Vec<String> {
        self.editor
            .buffer
            .split_whitespace()
            .skip(1)
            .map(|s| s.to_string())
            .collect()
    }

    /// Everything after the command name, verbatim (bar the surrounding
    /// whitespace) — what a single argument that may itself contain
    /// spaces needs, a path being the obvious one. [`Self::args`] is the
    /// tokenised view of the same text.
    pub fn arg_line(&self) -> &str {
        let buf = self.editor.buffer.trim_start();
        match buf.find(char::is_whitespace) {
            Some(i) => buf[i..].trim(),
            None => "",
        }
    }

    /// Recompute `matches` for the current filter, keeping the
    /// previously selected item selected when it survives, else clamping
    /// to the top.
    pub fn refilter(&mut self) {
        let prev = self.matches.get(self.selected).copied();
        let needle = self.filter_text().to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| needle.is_empty() || it.filter_key.contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.selected = prev
            .and_then(|p| self.matches.iter().position(|&m| m == p))
            .unwrap_or(0);
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    /// Move the selection by `delta`, clamped to the match list.
    pub fn move_sel(&mut self, delta: i64) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as i64 - 1;
        self.selected = (self.selected as i64 + delta).clamp(0, last) as usize;
    }

    /// The selected item's index into `items`, if any.
    pub fn current(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    /// The selected item, if any.
    pub fn current_item(&self) -> Option<&PickerItem<P>> {
        self.current().map(|i| &self.items[i])
    }

    /// Tab-complete the input line to the current selection without
    /// running it. On a command line, replace the first token with the
    /// selection's completion text and leave a trailing space
    /// (preserving any args already typed) so the user can fill in
    /// arguments. Otherwise fill the whole line with it.
    pub fn complete(&mut self) {
        let Some(idx) = self.current() else {
            return;
        };
        let item = &self.items[idx];
        let text = item.complete.clone().unwrap_or_else(|| item.label.clone());
        let newbuf = match self.mode {
            FilterMode::CommandLine => {
                let rest: Vec<&str> = self.editor.buffer.split_whitespace().skip(1).collect();
                let mut b = text;
                b.push(' ');
                b.push_str(&rest.join(" "));
                b
            }
            FilterMode::Whole => text,
        };
        self.editor = LineEditor::with_max(newbuf, self.editor.max);
        self.refilter();
    }

    /// Consume one keystroke from the front of `bytes`, returning how
    /// many bytes were consumed and the resulting outcome. Navigation
    /// keys the line editor doesn't use are intercepted here; everything
    /// else (text edits, cursor motion) routes to the editor and
    /// triggers a refilter.
    pub fn feed(&mut self, bytes: &[u8]) -> (usize, PickerOutcome) {
        let Some(&b) = bytes.first() else {
            return (1, PickerOutcome::Noop);
        };
        match b {
            b'\r' | b'\n' => (1, PickerOutcome::Commit),
            0x07 => (1, PickerOutcome::Cancel), // Ctrl+G
            0x09 => {
                self.complete(); // Tab: fill the input with the selection
                (1, PickerOutcome::Redraw)
            }
            0x0E => {
                self.move_sel(1); // Ctrl+N
                (1, PickerOutcome::Redraw)
            }
            0x10 => {
                self.move_sel(-1); // Ctrl+P
                (1, PickerOutcome::Redraw)
            }
            b'1'..=b'9' if self.digit_commit => {
                let item = (b - b'1') as usize;
                if let Some(pos) = self.matches.iter().position(|&m| m == item) {
                    self.selected = pos;
                    (1, PickerOutcome::Commit)
                } else {
                    (1, PickerOutcome::Noop)
                }
            }
            0x1B => self.feed_escape(bytes),
            _ => {
                let (consumed, outcome) = self.editor.feed(bytes);
                self.after_editor(consumed, outcome)
            }
        }
    }

    /// Apply one already-parsed [`Event`], the counterpart to
    /// [`Picker::feed`] for clients driven by an
    /// [`InputParser`](crate::input::InputParser). Selection keys are
    /// intercepted here; everything else routes to the line editor.
    pub fn feed_event(&mut self, ev: Event) -> PickerOutcome {
        match ev {
            Event::Enter => PickerOutcome::Commit,
            Event::Escape | Event::Ctrl('g') => PickerOutcome::Cancel,
            Event::Tab => {
                self.complete();
                PickerOutcome::Redraw
            }
            Event::Arrow(Dir::Down) | Event::Ctrl('n') => {
                self.move_sel(1);
                PickerOutcome::Redraw
            }
            Event::Arrow(Dir::Up) | Event::Ctrl('p') | Event::BackTab => {
                self.move_sel(-1);
                PickerOutcome::Redraw
            }
            Event::Nav(Nav::PageDown) => {
                self.move_sel(PICKER_PAGE as i64);
                PickerOutcome::Redraw
            }
            Event::Nav(Nav::PageUp) => {
                self.move_sel(-(PICKER_PAGE as i64));
                PickerOutcome::Redraw
            }
            Event::Key(c) if self.digit_commit && c.is_ascii_digit() && c != '0' => {
                let item = (c as u8 - b'1') as usize;
                match self.matches.iter().position(|&m| m == item) {
                    Some(pos) => {
                        self.selected = pos;
                        PickerOutcome::Commit
                    }
                    None => PickerOutcome::Noop,
                }
            }
            other => {
                let outcome = self.editor.feed_event(other);
                self.after_editor(0, outcome).1
            }
        }
    }

    /// Handle an ESC-introduced sequence: Up/Down/PgUp/PgDn/BackTab
    /// drive the selection, everything else (cursor motion, Alt-word,
    /// bare Esc) routes to the editor.
    fn feed_escape(&mut self, bytes: &[u8]) -> (usize, PickerOutcome) {
        let Some(&next) = bytes.get(1) else {
            return (1, PickerOutcome::Cancel); // lone Esc cancels
        };
        if next == b'[' || next == b'O' {
            let mut i = 2;
            while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            if i >= bytes.len() {
                return (bytes.len(), PickerOutcome::Noop); // incomplete
            }
            let params = &bytes[2..i];
            let nav = match (bytes[i], params) {
                (b'A', _) => Some(-1),                       // Up
                (b'B', _) => Some(1),                        // Down
                (b'Z', _) => Some(-1),                       // BackTab
                (b'~', b"5") => Some(-(PICKER_PAGE as i64)), // PgUp
                (b'~', b"6") => Some(PICKER_PAGE as i64),    // PgDn
                _ => None,
            };
            if let Some(delta) = nav {
                self.move_sel(delta);
                return (i + 1, PickerOutcome::Redraw);
            }
            // Cursor motion (Left/Right/Home/End/Delete) → editor.
            let (consumed, outcome) = self.editor.feed(bytes);
            return self.after_editor(consumed, outcome);
        }
        // Alt-<key> word motions etc.
        let (consumed, outcome) = self.editor.feed(bytes);
        self.after_editor(consumed, outcome)
    }

    /// Map a line-editor outcome to a picker outcome, refiltering on
    /// edits.
    fn after_editor(&mut self, consumed: usize, outcome: EditOutcome) -> (usize, PickerOutcome) {
        match outcome {
            EditOutcome::Commit => (consumed, PickerOutcome::Commit),
            EditOutcome::Cancel => (consumed, PickerOutcome::Cancel),
            EditOutcome::Redraw => {
                self.refilter();
                (consumed, PickerOutcome::Redraw)
            }
            EditOutcome::Noop => (consumed, PickerOutcome::Noop),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<PickerItem<usize>> {
        ["resize", "rename-pane", "rename-tab", "new-tab", "zoom"]
            .iter()
            .enumerate()
            .map(|(i, name)| PickerItem::new(*name, "", i))
            .collect()
    }

    fn feed(p: &mut Picker<usize>, keys: &[u8]) -> PickerOutcome {
        let mut i = 0;
        let mut last = PickerOutcome::Noop;
        while i < keys.len() {
            let (n, outcome) = p.feed(&keys[i..]);
            last = outcome;
            i += n.max(1);
        }
        last
    }

    #[test]
    fn typing_filters_and_keeps_the_selection_in_range() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        assert_eq!(p.matches.len(), 5);
        feed(&mut p, b"rename");
        assert_eq!(p.matches.len(), 2);
        assert_eq!(p.selected, 0);
        assert_eq!(p.current_item().unwrap().label, "rename-pane");
    }

    #[test]
    fn args_come_from_the_tokens_after_the_command() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        feed(&mut p, b"rename-tab 2 build");
        assert_eq!(p.filter_text(), "rename-tab");
        assert_eq!(p.args(), vec!["2".to_string(), "build".to_string()]);
        assert_eq!(p.matches.len(), 1, "args must not narrow the filter");
    }

    #[test]
    fn arg_line_keeps_spaces_that_belong_to_the_argument() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        // A path argument: tokenising would tear it in half.
        feed(&mut p, b"zoom /home/me/My Pictures ");
        assert_eq!(p.arg_line(), "/home/me/My Pictures");
        assert_eq!(p.filter_text(), "zoom", "the command token is unaffected");
    }

    #[test]
    fn arg_line_is_empty_without_arguments() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        feed(&mut p, b"zoom");
        assert_eq!(p.arg_line(), "");
        // Trailing whitespace alone is not an argument.
        feed(&mut p, b"   ");
        assert_eq!(p.arg_line(), "");
    }

    #[test]
    fn whole_mode_filters_on_the_entire_buffer() {
        let mut p = Picker::new("Tabs", FilterMode::Whole, items());
        feed(&mut p, b"new tab");
        assert!(p.matches.is_empty(), "the space is part of the needle");
    }

    #[test]
    fn tab_completes_the_selection_and_preserves_args() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        feed(&mut p, b"zo 3\t");
        assert_eq!(p.editor.buffer, "zoom 3");
        assert_eq!(p.editor.cursor, p.editor.char_count());
    }

    #[test]
    fn arrows_and_ctrl_np_move_the_selection() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        assert_eq!(feed(&mut p, b"\x1b[B"), PickerOutcome::Redraw);
        assert_eq!(p.selected, 1);
        feed(&mut p, b"\x0e");
        assert_eq!(p.selected, 2);
        feed(&mut p, b"\x10\x10\x10\x10");
        assert_eq!(p.selected, 0, "clamped at the top");
    }

    #[test]
    fn enter_commits_and_esc_cancels() {
        let mut p = Picker::new("Command", FilterMode::CommandLine, items());
        assert_eq!(feed(&mut p, b"\r"), PickerOutcome::Commit);
        assert_eq!(feed(&mut p, b"\x1b"), PickerOutcome::Cancel);
    }

    #[test]
    fn digit_commit_selects_by_position_only_when_enabled() {
        let mut plain = Picker::new("Tabs", FilterMode::Whole, items());
        feed(&mut plain, b"2");
        assert_eq!(plain.editor.buffer, "2", "digits are filter text");

        let mut numbered = Picker::new("Move", FilterMode::Whole, items()).with_digit_commit();
        assert_eq!(feed(&mut numbered, b"3"), PickerOutcome::Commit);
        assert_eq!(numbered.current_item().unwrap().label, "rename-tab");
    }

    #[test]
    fn the_event_path_matches_the_byte_path() {
        let mut bytes = Picker::new("Command", FilterMode::CommandLine, items());
        let mut evs = Picker::new("Command", FilterMode::CommandLine, items());
        feed(&mut bytes, b"zo 3\t");
        for ev in [
            Event::Key('z'),
            Event::Key('o'),
            Event::Key(' '),
            Event::Key('3'),
            Event::Tab,
        ] {
            evs.feed_event(ev);
        }
        assert_eq!(evs.editor.buffer, bytes.editor.buffer);

        assert_eq!(evs.feed_event(Event::Arrow(Dir::Down)), PickerOutcome::Redraw);
        assert_eq!(evs.feed_event(Event::Enter), PickerOutcome::Commit);
        assert_eq!(evs.feed_event(Event::Escape), PickerOutcome::Cancel);
    }

    #[test]
    fn filter_key_widens_the_haystack_beyond_the_label() {
        let items = vec![
            PickerItem::new("delete-pane", "", 0usize).with_filter_key("delete-pane kill-pane close"),
        ];
        let mut p = Picker::new("Command", FilterMode::CommandLine, items);
        feed(&mut p, b"kill");
        assert_eq!(p.matches.len(), 1);
    }
}
