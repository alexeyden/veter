use crate::term::BufWrite as _;
use unicode_width::UnicodeWidthChar as _;

const MODE_APPLICATION_KEYPAD: u16 = 0b0000_0000_0000_0001;
const MODE_APPLICATION_CURSOR: u16 = 0b0000_0000_0000_0010;
const MODE_HIDE_CURSOR: u16 = 0b0000_0000_0000_0100;
const MODE_ALTERNATE_SCREEN: u16 = 0b0000_0000_0000_1000;
const MODE_BRACKETED_PASTE: u16 = 0b0000_0000_0001_0000;
/// DECAWM (`?7`) is on by default and `modes` starts at zero, so the
/// bit records the *absence* of auto-wrap. Storing it the other way
/// round would make every existing snapshot decode as "wrap off".
const MODE_NO_AUTOWRAP: u16 = 0b0000_0000_0010_0000;
/// IRM (`CSI 4 h`), the terminfo `smir` / `rmir` pair. The whole
/// non-private SM/RM family used to fall through to `unhandled_csi`,
/// so a program that entered insert mode overwrote the line it meant
/// to shift.
const MODE_INSERT: u16 = 0b0000_0000_0100_0000;
/// LNM (`CSI 20 h`) — LF also does a carriage return.
const MODE_NEWLINE: u16 = 0b0000_0000_1000_0000;
/// DECSCNM (`?5`), the whole-screen reverse video terminfo drives
/// `flash` with.
const MODE_REVERSE_VIDEO: u16 = 0b0000_0001_0000_0000;
/// `?12`, cursor blink. Stored the positive way round, so the default
/// is a steady cursor: xterm blinks by default, but veter never has,
/// and `cnorm` (`\E[?12l\E[?25h`) turns blinking off at the start of
/// every ncurses program anyway. `cvvis` (`\E[?12;25h`) is what asks
/// for it.
const MODE_CURSOR_BLINK: u16 = 0b0000_0010_0000_0000;
/// `?1004`, focus reporting: the renderer sends `CSI I` / `CSI O` as
/// the window gains and loses focus.
const MODE_FOCUS_EVENT: u16 = 0b0000_0100_0000_0000;

/// The cursor shape a program asked for with DECSCUSR
/// (`CSI Ps SP q`), the terminfo `Ss` / `Se` pair. vim, neovim, fish
/// and most shell prompt frameworks switch to [`Bar`](Self::Bar) for
/// insert mode and back on the way out.
///
/// Blinking is *not* part of the shape — DECSCUSR's odd parameters
/// ask for it and its even ones don't, and both fold into
/// [`MODE_CURSOR_BLINK`], the same bit `?12` sets.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum CursorShape {
    /// `CSI 0 SP q`, `CSI 1 SP q`, `CSI 2 SP q`.
    #[default]
    Block,
    /// `CSI 3 SP q`, `CSI 4 SP q`.
    Underline,
    /// `CSI 5 SP q`, `CSI 6 SP q`.
    Bar,
}

/// The xterm mouse handling mode currently in use.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum MouseProtocolMode {
    /// Mouse handling is disabled.
    #[default]
    None,

    /// Mouse button events should be reported on button press. Also known as
    /// X10 mouse mode.
    Press,

    /// Mouse button events should be reported on button press and release.
    /// Also known as VT200 mouse mode.
    PressRelease,

    // Highlight,
    /// Mouse button events should be reported on button press and release, as
    /// well as when the mouse moves between cells while a button is held
    /// down.
    ButtonMotion,

    /// Mouse button events should be reported on button press and release,
    /// and mouse motion events should be reported when the mouse moves
    /// between cells regardless of whether a button is held down or not.
    AnyMotion,
    // DecLocator,
}

/// The encoding to use for the enabled [`MouseProtocolMode`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum MouseProtocolEncoding {
    /// Default single-printable-byte encoding.
    #[default]
    Default,

    /// UTF-8-based encoding.
    Utf8,

    /// SGR-like encoding.
    Sgr,

    /// SGR framing with the position given in pixels rather than cells
    /// (xterm's SGR-Pixels, DECSET 1016). Byte-identical to [`Self::Sgr`]
    /// on the wire; only the meaning of the two coordinates differs, so
    /// the emitter — not the parser — is what has to know.
    SgrPixels,
    // Urxvt,
}

/// Represents the overall terminal state.
#[derive(Clone, Debug)]
pub struct Screen {
    grid: crate::grid::Grid,
    alternate_grid: crate::grid::Grid,

    attrs: crate::attrs::Attrs,
    /// DECSC slot for the main grid. Per-grid, like `Grid::saved_pos`
    /// — see [`Screen::saved_sgr_mut`].
    saved_attrs: crate::attrs::Attrs,
    alternate_saved_attrs: crate::attrs::Attrs,

    charset: crate::charset::CharsetState,
    saved_charset: crate::charset::CharsetState,
    alternate_saved_charset: crate::charset::CharsetState,

    modes: u16,
    cursor_shape: CursorShape,
    mouse_protocol_mode: MouseProtocolMode,
    mouse_protocol_encoding: MouseProtocolEncoding,

    // Last graphic character committed to the grid, used by REP (CSI Ps b).
    // Stored post-charset-translation so re-emitting it is idempotent.
    last_char: Option<char>,
}

impl Screen {
    pub(crate) fn new(
        size: crate::grid::Size,
        scrollback_len: usize,
    ) -> Self {
        let mut grid = crate::grid::Grid::new(size, scrollback_len);
        grid.allocate_rows();
        Self {
            grid,
            alternate_grid: crate::grid::Grid::new(size, 0),

            attrs: crate::attrs::Attrs::default(),
            saved_attrs: crate::attrs::Attrs::default(),
            alternate_saved_attrs: crate::attrs::Attrs::default(),

            charset: crate::charset::CharsetState::default(),
            saved_charset: crate::charset::CharsetState::default(),
            alternate_saved_charset: crate::charset::CharsetState::default(),

            modes: 0,
            cursor_shape: CursorShape::default(),
            mouse_protocol_mode: MouseProtocolMode::default(),
            mouse_protocol_encoding: MouseProtocolEncoding::default(),

            last_char: None,
        }
    }

    /// Resizes the terminal.
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.grid.set_size(crate::grid::Size { rows, cols });
        self.alternate_grid
            .set_size(crate::grid::Size { rows, cols });
    }

    /// Returns the current size of the terminal.
    ///
    /// The return value will be (rows, cols).
    #[must_use]
    pub fn size(&self) -> (u16, u16) {
        let size = self.grid().size();
        (size.rows, size.cols)
    }

    /// Scrolls to the given position in the scrollback.
    ///
    /// This position indicates the offset from the top of the screen, and
    /// should be `0` to put the normal screen in view.
    ///
    /// This affects the return values of methods called on the screen: for
    /// instance, `screen.cell(0, 0)` will return the top left corner of the
    /// screen after taking the scrollback offset into account.
    ///
    /// The value given will be clamped to the actual size of the scrollback.
    pub fn set_scrollback(&mut self, rows: usize) {
        self.grid_mut().set_scrollback(rows);
    }

    /// Returns the current position in the scrollback.
    ///
    /// This position indicates the offset from the top of the screen, and is
    /// `0` when the normal screen is in view.
    #[must_use]
    pub fn scrollback(&self) -> usize {
        self.grid().scrollback()
    }

    /// Monotonic count of lines scrolled off the top of the *main*
    /// (non-alternate) grid. Used by the PRT activity heuristic to
    /// detect that a portal produced meaningful output. Runtime-only
    /// — not preserved across a binary snapshot restore.
    #[must_use]
    pub fn scroll_committed(&self) -> u64 {
        self.grid.scroll_committed()
    }

    /// Absolute scrollback line index of the *main* grid's first live
    /// row, where line 0 is the first row the parser ever displayed.
    /// Lines scrolled off the top count +1 (scroll-region scrolls
    /// excluded — they don't move the screen relative to scrollback),
    /// rows pushed into scrollback by a vertical shrink count +1 each,
    /// rows pulled back out by a vertical grow count -1 each.
    ///
    /// VGE elements and Scrollback-anchored PRT portals live in this
    /// coordinate space. Deliberately the *main* grid's value even
    /// while the alternate screen is up: the alternate screen has no
    /// scrollback, so anchors stay frozen against the main screen they
    /// were placed on, and a resize during the alt screen — which
    /// pushes/pulls main-grid rows — is reflected the moment the alt
    /// screen goes away. Preserved across a binary snapshot restore.
    #[must_use]
    pub fn top_of_live_screen(&self) -> i64 {
        self.grid.top_of_live_screen()
    }

    /// Number of rows currently held in the *main* grid's scrollback
    /// ring (the alternate grid keeps no scrollback). Unlike
    /// [`scrollback`](Self::scrollback), this is the fill level, not
    /// the user's scroll offset.
    #[must_use]
    pub fn scrollback_fill(&self) -> usize {
        self.grid.scrollback_fill()
    }

    /// Offset from the live screen's first row to the bottom-most row
    /// whose text contains `needle`, or `None` if no row does. Negative
    /// reaches into scrollback, `-1` being the row just above the live
    /// screen; adding [`top_of_live_screen`](Self::top_of_live_screen)
    /// gives the matched row's absolute line index.
    ///
    /// Searches the *current* screen, so while the alternate screen is
    /// up the answer is a plain live-row index — the alternate grid
    /// keeps no scrollback, and `top_of_live_screen` is frozen at the
    /// main screen's value, which together make an alt-screen marker
    /// resolve exactly like the viewport-relative offset it is.
    ///
    /// Unaffected by the user's scroll position: where the view is
    /// scrolled to must not change which row a marker names.
    #[must_use]
    pub fn last_row_offset_containing(&self, needle: &str) -> Option<i64> {
        self.grid().last_row_offset_containing(needle)
    }

    /// Returns the text contents of the terminal.
    ///
    /// This will not include any formatting information, and will be in plain
    /// text format.
    #[must_use]
    pub fn contents(&self) -> String {
        let mut contents = String::new();
        self.write_contents(&mut contents);
        contents
    }

    fn write_contents(&self, contents: &mut String) {
        self.grid().write_contents(contents);
    }

    /// Returns the text contents of the terminal by row, restricted to the
    /// given subset of columns.
    ///
    /// This will not include any formatting information, and will be in plain
    /// text format.
    ///
    /// Newlines will not be included.
    pub fn rows(
        &self,
        start: u16,
        width: u16,
    ) -> impl Iterator<Item = String> + '_ {
        self.grid().visible_rows().map(move |row| {
            let mut contents = String::new();
            row.write_contents(&mut contents, start, width, false);
            contents
        })
    }

    /// Returns the text contents of the terminal logically between two cells.
    /// This will include the remainder of the starting row after `start_col`,
    /// followed by the entire contents of the rows between `start_row` and
    /// `end_row`, followed by the beginning of the `end_row` up until
    /// `end_col`. This is useful for things like determining the contents of
    /// a clipboard selection.
    #[must_use]
    pub fn contents_between(
        &self,
        start_row: u16,
        start_col: u16,
        end_row: u16,
        end_col: u16,
    ) -> String {
        match start_row.cmp(&end_row) {
            std::cmp::Ordering::Less => {
                let (_, cols) = self.size();
                let mut contents = String::new();
                for (i, row) in self
                    .grid()
                    .visible_rows()
                    .enumerate()
                    .skip(usize::from(start_row))
                    .take(usize::from(end_row) - usize::from(start_row) + 1)
                {
                    if i == usize::from(start_row) {
                        row.write_contents(
                            &mut contents,
                            start_col,
                            cols - start_col,
                            false,
                        );
                        if !row.wrapped() {
                            contents.push('\n');
                        }
                    } else if i == usize::from(end_row) {
                        row.write_contents(&mut contents, 0, end_col, false);
                    } else {
                        row.write_contents(&mut contents, 0, cols, false);
                        if !row.wrapped() {
                            contents.push('\n');
                        }
                    }
                }
                contents
            }
            std::cmp::Ordering::Equal => {
                if start_col < end_col {
                    self.rows(start_col, end_col - start_col)
                        .nth(usize::from(start_row))
                        .unwrap_or_default()
                } else {
                    String::new()
                }
            }
            std::cmp::Ordering::Greater => String::new(),
        }
    }

    /// Return escape codes sufficient to reproduce the entire contents of the
    /// current terminal state. This is a convenience wrapper around
    /// [`contents_formatted`](Self::contents_formatted) and
    /// [`input_mode_formatted`](Self::input_mode_formatted).
    #[must_use]
    pub fn state_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_formatted(&mut contents);
        self.write_input_mode_formatted(&mut contents);
        contents
    }

    /// Serialize the full `Screen` state as a binary blob suitable for
    /// shipping over the VSS extension's `VtFragment`. Captures every
    /// internal field — visible grid, alternate grid, cursor and saved
    /// cursor, scroll region, origin mode, charset, all input modes —
    /// so [`restore_from_binary_snapshot`](Self::restore_from_binary_snapshot)
    /// can reconstruct an identical `Screen` on the receiver. Closes
    /// every state gap that the v1 replay serializer
    /// [`full_contents_formatted`](Self::full_contents_formatted) leaves
    /// open. OSC-set window/icon titles still belong to a higher level
    /// than `Screen` and are not included here.
    #[must_use]
    pub fn binary_snapshot(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut w = crate::snapshot::Writer::new(&mut buf);
        w.u16(crate::snapshot::SNAPSHOT_KIND_VERSION);

        self.grid.serialize_binary(&mut w);
        self.alternate_grid.serialize_binary(&mut w);

        crate::snapshot::encode_attrs(&mut w, &self.attrs);
        crate::snapshot::encode_attrs(&mut w, &self.saved_attrs);
        crate::snapshot::encode_attrs(&mut w, &self.alternate_saved_attrs);

        crate::snapshot::encode_charset_state(&mut w, &self.charset);
        crate::snapshot::encode_charset_state(&mut w, &self.saved_charset);
        crate::snapshot::encode_charset_state(&mut w, &self.alternate_saved_charset);

        w.u16(self.modes);
        crate::snapshot::encode_cursor_shape(&mut w, self.cursor_shape);
        crate::snapshot::encode_mouse_mode(&mut w, self.mouse_protocol_mode);
        crate::snapshot::encode_mouse_encoding(&mut w, self.mouse_protocol_encoding);

        buf
    }

    /// Decode a payload produced by [`binary_snapshot`](Self::binary_snapshot)
    /// and overwrite this `Screen`'s state with it. Side-effect-free:
    /// no callbacks are invoked. Returns an error on version mismatch
    /// or malformed payload, leaving the existing `Screen` state
    /// untouched.
    pub fn restore_from_binary_snapshot(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), crate::snapshot::SnapshotError> {
        let mut r = crate::snapshot::Reader::new(bytes);
        let kind_version = r.u16()?;
        if kind_version != crate::snapshot::SNAPSHOT_KIND_VERSION {
            return Err(crate::snapshot::SnapshotError::kind_version_mismatch(
                kind_version,
                crate::snapshot::SNAPSHOT_KIND_VERSION,
            ));
        }

        let grid = crate::grid::Grid::deserialize_binary(&mut r)?;
        let alternate_grid = crate::grid::Grid::deserialize_binary(&mut r)?;

        let attrs = crate::snapshot::decode_attrs(&mut r)?;
        let saved_attrs = crate::snapshot::decode_attrs(&mut r)?;
        let alternate_saved_attrs = crate::snapshot::decode_attrs(&mut r)?;

        let charset = crate::snapshot::decode_charset_state(&mut r)?;
        let saved_charset = crate::snapshot::decode_charset_state(&mut r)?;
        let alternate_saved_charset = crate::snapshot::decode_charset_state(&mut r)?;

        let modes = r.u16()?;
        let cursor_shape = crate::snapshot::decode_cursor_shape(&mut r)?;
        let mouse_protocol_mode = crate::snapshot::decode_mouse_mode(&mut r)?;
        let mouse_protocol_encoding = crate::snapshot::decode_mouse_encoding(&mut r)?;

        if !r.at_end() {
            return Err(crate::snapshot::SnapshotError::bad_payload(
                "trailing bytes after vt100 snapshot",
            ));
        }

        self.grid = grid;
        self.alternate_grid = alternate_grid;
        self.attrs = attrs;
        self.saved_attrs = saved_attrs;
        self.alternate_saved_attrs = alternate_saved_attrs;
        self.charset = charset;
        self.saved_charset = saved_charset;
        self.alternate_saved_charset = alternate_saved_charset;
        self.modes = modes;
        self.cursor_shape = cursor_shape;
        self.mouse_protocol_mode = mouse_protocol_mode;
        self.mouse_protocol_encoding = mouse_protocol_encoding;

        Ok(())
    }

    /// Return escape codes sufficient to turn the terminal state of the
    /// screen `prev` into the current terminal state. This is a convenience
    /// wrapper around [`contents_diff`](Self::contents_diff) and
    /// [`input_mode_diff`](Self::input_mode_diff).
    #[must_use]
    pub fn state_diff(&self, prev: &Self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_diff(&mut contents, prev);
        self.write_input_mode_diff(&mut contents, prev);
        contents
    }

    /// Returns the formatted visible contents of the terminal.
    ///
    /// Formatting information will be included inline as terminal escape
    /// codes. The result will be suitable for feeding directly to a raw
    /// terminal parser, and will result in the same visual output.
    #[must_use]
    pub fn contents_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_formatted(&mut contents);
        contents
    }

    fn write_contents_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::HideCursor::new(self.hide_cursor()).write_buf(contents);
        let prev_attrs = self.grid().write_contents_formatted(contents);
        self.attrs.write_escape_code_diff(contents, &prev_attrs);
    }

    /// Returns the formatted visible contents of the terminal by row,
    /// restricted to the given subset of columns.
    ///
    /// Formatting information will be included inline as terminal escape
    /// codes. The result will be suitable for feeding directly to a raw
    /// terminal parser, and will result in the same visual output.
    ///
    /// You are responsible for positioning the cursor before printing each
    /// row, and the final cursor position after displaying each row is
    /// unspecified.
    // the unwraps in this method shouldn't be reachable
    #[allow(clippy::missing_panics_doc)]
    pub fn rows_formatted(
        &self,
        start: u16,
        width: u16,
    ) -> impl Iterator<Item = Vec<u8>> + '_ {
        let mut wrapping = false;
        self.grid().visible_rows().enumerate().map(move |(i, row)| {
            // number of rows in a grid is stored in a u16 (see Size), so
            // visible_rows can never return enough rows to overflow here
            let i = i.try_into().unwrap();
            let mut contents = vec![];
            row.write_contents_formatted(
                &mut contents,
                start,
                width,
                i,
                wrapping,
                None,
                None,
            );
            if start == 0 && width == self.grid.size().cols {
                wrapping = row.wrapped();
            }
            contents
        })
    }

    /// Returns a terminal byte stream sufficient to turn the visible contents
    /// of the screen described by `prev` into the visible contents of the
    /// screen described by `self`.
    ///
    /// The result of rendering `prev.contents_formatted()` followed by
    /// `self.contents_diff(prev)` should be equivalent to the result of
    /// rendering `self.contents_formatted()`. This is primarily useful when
    /// you already have a terminal parser whose state is described by `prev`,
    /// since the diff will likely require less memory and cause less
    /// flickering than redrawing the entire screen contents.
    #[must_use]
    pub fn contents_diff(&self, prev: &Self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_contents_diff(&mut contents, prev);
        contents
    }

    fn write_contents_diff(&self, contents: &mut Vec<u8>, prev: &Self) {
        if self.hide_cursor() != prev.hide_cursor() {
            crate::term::HideCursor::new(self.hide_cursor())
                .write_buf(contents);
        }
        let prev_attrs = self.grid().write_contents_diff(
            contents,
            prev.grid(),
            prev.attrs,
        );
        self.attrs.write_escape_code_diff(contents, &prev_attrs);
    }

    /// Returns a sequence of terminal byte streams sufficient to turn the
    /// visible contents of the subset of each row from `prev` (as described
    /// by `start` and `width`) into the visible contents of the corresponding
    /// row subset in `self`.
    ///
    /// You are responsible for positioning the cursor before printing each
    /// row, and the final cursor position after displaying each row is
    /// unspecified.
    // the unwraps in this method shouldn't be reachable
    #[allow(clippy::missing_panics_doc)]
    pub fn rows_diff<'a>(
        &'a self,
        prev: &'a Self,
        start: u16,
        width: u16,
    ) -> impl Iterator<Item = Vec<u8>> + 'a {
        self.grid()
            .visible_rows()
            .zip(prev.grid().visible_rows())
            .enumerate()
            .map(move |(i, (row, prev_row))| {
                // number of rows in a grid is stored in a u16 (see Size), so
                // visible_rows can never return enough rows to overflow here
                let i = i.try_into().unwrap();
                let mut contents = vec![];
                row.write_contents_diff(
                    &mut contents,
                    prev_row,
                    start,
                    width,
                    i,
                    false,
                    false,
                    crate::grid::Pos { row: i, col: start },
                    crate::attrs::Attrs::default(),
                );
                contents
            })
    }

    /// Returns terminal escape sequences sufficient to set the current
    /// terminal's input modes.
    ///
    /// Supported modes are:
    /// * application keypad
    /// * application cursor
    /// * bracketed paste
    /// * xterm mouse support
    #[must_use]
    pub fn input_mode_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_input_mode_formatted(&mut contents);
        contents
    }

    fn write_input_mode_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::ApplicationKeypad::new(
            self.mode(MODE_APPLICATION_KEYPAD),
        )
        .write_buf(contents);
        crate::term::ApplicationCursor::new(
            self.mode(MODE_APPLICATION_CURSOR),
        )
        .write_buf(contents);
        crate::term::BracketedPaste::new(self.mode(MODE_BRACKETED_PASTE))
            .write_buf(contents);
        crate::term::MouseProtocolMode::new(
            self.mouse_protocol_mode,
            MouseProtocolMode::None,
        )
        .write_buf(contents);
        crate::term::MouseProtocolEncoding::new(
            self.mouse_protocol_encoding,
            MouseProtocolEncoding::Default,
        )
        .write_buf(contents);
    }

    /// Returns terminal escape sequences sufficient to change the previous
    /// terminal's input modes to the input modes enabled in the current
    /// terminal.
    #[must_use]
    pub fn input_mode_diff(&self, prev: &Self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_input_mode_diff(&mut contents, prev);
        contents
    }

    fn write_input_mode_diff(&self, contents: &mut Vec<u8>, prev: &Self) {
        if self.mode(MODE_APPLICATION_KEYPAD)
            != prev.mode(MODE_APPLICATION_KEYPAD)
        {
            crate::term::ApplicationKeypad::new(
                self.mode(MODE_APPLICATION_KEYPAD),
            )
            .write_buf(contents);
        }
        if self.mode(MODE_APPLICATION_CURSOR)
            != prev.mode(MODE_APPLICATION_CURSOR)
        {
            crate::term::ApplicationCursor::new(
                self.mode(MODE_APPLICATION_CURSOR),
            )
            .write_buf(contents);
        }
        if self.mode(MODE_BRACKETED_PASTE) != prev.mode(MODE_BRACKETED_PASTE)
        {
            crate::term::BracketedPaste::new(self.mode(MODE_BRACKETED_PASTE))
                .write_buf(contents);
        }
        crate::term::MouseProtocolMode::new(
            self.mouse_protocol_mode,
            prev.mouse_protocol_mode,
        )
        .write_buf(contents);
        crate::term::MouseProtocolEncoding::new(
            self.mouse_protocol_encoding,
            prev.mouse_protocol_encoding,
        )
        .write_buf(contents);
    }

    /// Returns terminal escape sequences sufficient to set the current
    /// terminal's drawing attributes.
    ///
    /// Supported drawing attributes are:
    /// * fgcolor
    /// * bgcolor
    /// * bold
    /// * dim
    /// * italic
    /// * underline
    /// * inverse
    ///
    /// This is not typically necessary, since
    /// [`contents_formatted`](Self::contents_formatted) will leave
    /// the current active drawing attributes in the correct state, but this
    /// can be useful in the case of drawing additional things on top of a
    /// terminal output, since you will need to restore the terminal state
    /// without the terminal contents necessarily being the same.
    #[must_use]
    pub fn attributes_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_attributes_formatted(&mut contents);
        contents
    }

    fn write_attributes_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::ClearAttrs.write_buf(contents);
        self.attrs.write_escape_code_diff(
            contents,
            &crate::attrs::Attrs::default(),
        );
    }

    /// Returns the current cursor position of the terminal.
    ///
    /// The return value will be (row, col).
    #[must_use]
    pub fn cursor_position(&self) -> (u16, u16) {
        let pos = self.grid().pos();
        (pos.row, pos.col)
    }

    /// Returns terminal escape sequences sufficient to set the current
    /// cursor state of the terminal.
    ///
    /// This is not typically necessary, since
    /// [`contents_formatted`](Self::contents_formatted) will leave
    /// the cursor in the correct state, but this can be useful in the case of
    /// drawing additional things on top of a terminal output, since you will
    /// need to restore the terminal state without the terminal contents
    /// necessarily being the same.
    ///
    /// Note that the bytes returned by this function may alter the active
    /// drawing attributes, because it may require redrawing existing cells in
    /// order to position the cursor correctly (for instance, in the case
    /// where the cursor is past the end of a row). Therefore, you should
    /// ensure to reset the active drawing attributes if necessary after
    /// processing this data, for instance by using
    /// [`attributes_formatted`](Self::attributes_formatted).
    #[must_use]
    pub fn cursor_state_formatted(&self) -> Vec<u8> {
        let mut contents = vec![];
        self.write_cursor_state_formatted(&mut contents);
        contents
    }

    fn write_cursor_state_formatted(&self, contents: &mut Vec<u8>) {
        crate::term::HideCursor::new(self.hide_cursor()).write_buf(contents);
        self.grid()
            .write_cursor_position_formatted(contents, None, None);

        // we don't just call write_attributes_formatted here, because that
        // would still be confusing - consider the case where the user sets
        // their own unrelated drawing attributes (on a different parser
        // instance) and then calls cursor_state_formatted. just documenting
        // it and letting the user handle it on their own is more
        // straightforward.
    }

    /// Returns the [`Cell`](crate::Cell) object at the given location in the
    /// terminal, if it exists.
    #[must_use]
    pub fn cell(&self, row: u16, col: u16) -> Option<&crate::Cell> {
        self.grid().visible_cell(crate::grid::Pos { row, col })
    }

    /// [`cell`](Self::cell), but reading the grid as if the scroll
    /// offset were `offset` rather than the grid's own.
    ///
    /// Two PRT views sharing one buffer scroll independently, so the
    /// offset belongs to the view and the render path passes it in.
    /// `offset` is clamped to the available history.
    #[must_use]
    pub fn cell_at(
        &self,
        offset: usize,
        row: u16,
        col: u16,
    ) -> Option<&crate::Cell> {
        self.grid()
            .visible_cell_at(offset, crate::grid::Pos { row, col })
    }

    /// The visible row at `row` as a cell slice, or an empty slice when
    /// `row` is past the bottom of the grid.
    ///
    /// `cell` re-derives the visible-row iterator on every call, so
    /// fingerprinting a whole grid through it costs one iterator walk
    /// per *cell*. Callers that read a row end to end — the PRT
    /// activity heuristic, `doc/portal-extension.md` §8.10 — take one
    /// walk per *row* through this instead. Indexing the returned slice
    /// by column is equivalent to `cell(row, col)`.
    #[must_use]
    pub fn visible_row_cells(&self, row: u16) -> &[crate::Cell] {
        self.grid()
            .visible_row(row)
            .map_or(&[][..], crate::row::Row::cells_slice)
    }

    /// [`visible_row_cells`](Self::visible_row_cells) at an explicit
    /// view offset. Same one-walk-per-row property.
    #[must_use]
    pub fn visible_row_cells_at(
        &self,
        offset: usize,
        row: u16,
    ) -> &[crate::Cell] {
        self.grid()
            .visible_row_at(offset, row)
            .map_or(&[][..], crate::row::Row::cells_slice)
    }

    /// Returns whether the text in row `row` should wrap to the next line.
    #[must_use]
    pub fn row_wrapped(&self, row: u16) -> bool {
        self.grid()
            .visible_row(row)
            .is_some_and(crate::row::Row::wrapped)
    }

    /// Returns whether the alternate screen is currently in use.
    #[must_use]
    pub fn alternate_screen(&self) -> bool {
        self.mode(MODE_ALTERNATE_SCREEN)
    }

    /// Returns whether the terminal should be in application keypad mode.
    #[must_use]
    pub fn application_keypad(&self) -> bool {
        self.mode(MODE_APPLICATION_KEYPAD)
    }

    /// Returns whether the terminal should be in application cursor mode.
    #[must_use]
    pub fn application_cursor(&self) -> bool {
        self.mode(MODE_APPLICATION_CURSOR)
    }

    /// Returns whether the terminal should be in hide cursor mode.
    #[must_use]
    pub fn hide_cursor(&self) -> bool {
        self.mode(MODE_HIDE_CURSOR)
    }

    /// Whether DECAWM (`?7`) is enabled — the default. With it off,
    /// a character printed at the last column overwrites it instead of
    /// moving to the next row.
    #[must_use]
    pub fn autowrap(&self) -> bool {
        !self.mode(MODE_NO_AUTOWRAP)
    }

    /// Returns whether the terminal should be in bracketed paste mode.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.mode(MODE_BRACKETED_PASTE)
    }

    /// Whether IRM (`CSI 4 h`) is on — printed characters shift the
    /// rest of the row right instead of overwriting it.
    #[must_use]
    pub fn insert_mode(&self) -> bool {
        self.mode(MODE_INSERT)
    }

    /// Whether LNM (`CSI 20 h`) is on — LF also does a carriage return.
    #[must_use]
    pub fn newline_mode(&self) -> bool {
        self.mode(MODE_NEWLINE)
    }

    /// Whether DECSCNM (`?5`) is on: the whole screen renders with
    /// foreground and background swapped. terminfo's `flash` is a
    /// hundred milliseconds of this.
    #[must_use]
    pub fn reverse_video(&self) -> bool {
        self.mode(MODE_REVERSE_VIDEO)
    }

    /// Whether focus reporting (`?1004`) is on, so the renderer should
    /// send `CSI I` / `CSI O` as its window gains and loses focus.
    #[must_use]
    pub fn focus_reporting(&self) -> bool {
        self.mode(MODE_FOCUS_EVENT)
    }

    /// The cursor shape last asked for with DECSCUSR (`CSI Ps SP q`).
    #[must_use]
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    /// Whether the cursor should blink — DECSCUSR's odd parameters and
    /// `?12h` both ask for it. Off by default.
    #[must_use]
    pub fn cursor_blink(&self) -> bool {
        self.mode(MODE_CURSOR_BLINK)
    }

    /// Returns the currently active [`MouseProtocolMode`].
    #[must_use]
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        self.mouse_protocol_mode
    }

    /// Returns the currently active [`MouseProtocolEncoding`].
    #[must_use]
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        self.mouse_protocol_encoding
    }

    /// Returns the currently active foreground color.
    #[must_use]
    pub fn fgcolor(&self) -> crate::Color {
        self.attrs.fgcolor
    }

    /// Returns the currently active background color.
    #[must_use]
    pub fn bgcolor(&self) -> crate::Color {
        self.attrs.bgcolor
    }

    /// Returns whether newly drawn text should be rendered with the bold text
    /// attribute.
    #[must_use]
    pub fn bold(&self) -> bool {
        self.attrs.bold()
    }

    /// Returns whether newly drawn text should be rendered with the dim text
    /// attribute.
    #[must_use]
    pub fn dim(&self) -> bool {
        self.attrs.dim()
    }

    /// Returns whether newly drawn text should be rendered with the italic
    /// text attribute.
    #[must_use]
    pub fn italic(&self) -> bool {
        self.attrs.italic()
    }

    /// Returns whether newly drawn text should be rendered with the
    /// underlined text attribute.
    #[must_use]
    pub fn underline(&self) -> bool {
        self.attrs.underline()
    }

    /// Returns whether newly drawn text should be rendered with the inverse
    /// text attribute.
    #[must_use]
    pub fn inverse(&self) -> bool {
        self.attrs.inverse()
    }

    pub(crate) fn grid(&self) -> &crate::grid::Grid {
        if self.mode(MODE_ALTERNATE_SCREEN) {
            &self.alternate_grid
        } else {
            &self.grid
        }
    }

    fn grid_mut(&mut self) -> &mut crate::grid::Grid {
        if self.mode(MODE_ALTERNATE_SCREEN) {
            &mut self.alternate_grid
        } else {
            &mut self.grid
        }
    }

    fn enter_alternate_grid(&mut self) {
        self.grid_mut().set_scrollback(0);
        self.set_mode(MODE_ALTERNATE_SCREEN);
        self.alternate_grid.allocate_rows();
    }

    fn exit_alternate_grid(&mut self) {
        self.clear_mode(MODE_ALTERNATE_SCREEN);
    }

    fn save_cursor(&mut self) {
        self.grid_mut().save_cursor();
        let (attrs, charset) = (self.attrs, self.charset);
        let (saved_attrs, saved_charset) = self.saved_sgr_mut();
        *saved_attrs = attrs;
        *saved_charset = charset;
    }

    fn restore_cursor(&mut self) {
        self.grid_mut().restore_cursor();
        let (attrs, charset) = {
            let (a, c) = self.saved_sgr_mut();
            (*a, *c)
        };
        self.attrs = attrs;
        self.charset = charset;
    }

    /// The DECSC slot belonging to the grid currently in view.
    ///
    /// `saved_pos` has always been per-grid; the attributes and
    /// charset were not, so a DECSC issued *inside* the alt screen
    /// overwrote what `?1049h` saved on the way in, and `?1049l` then
    /// restored the alt screen's SGR onto the main one. Full-screen
    /// programs save and restore the cursor constantly, so the shell
    /// they returned to inherited whatever colour they last used.
    fn saved_sgr_mut(
        &mut self,
    ) -> (&mut crate::attrs::Attrs, &mut crate::charset::CharsetState) {
        if self.modes & MODE_ALTERNATE_SCREEN != 0 {
            (&mut self.alternate_saved_attrs, &mut self.alternate_saved_charset)
        } else {
            (&mut self.saved_attrs, &mut self.saved_charset)
        }
    }

    fn set_mode(&mut self, mode: u16) {
        self.modes |= mode;
    }

    fn clear_mode(&mut self, mode: u16) {
        self.modes &= !mode;
    }

    fn mode(&self, mode: u16) -> bool {
        self.modes & mode != 0
    }

    fn set_mouse_mode(&mut self, mode: MouseProtocolMode) {
        self.mouse_protocol_mode = mode;
    }

    fn clear_mouse_mode(&mut self, mode: MouseProtocolMode) {
        if self.mouse_protocol_mode == mode {
            self.mouse_protocol_mode = MouseProtocolMode::default();
        }
    }

    fn set_mouse_encoding(&mut self, encoding: MouseProtocolEncoding) {
        self.mouse_protocol_encoding = encoding;
    }

    fn clear_mouse_encoding(&mut self, encoding: MouseProtocolEncoding) {
        if self.mouse_protocol_encoding == encoding {
            self.mouse_protocol_encoding = MouseProtocolEncoding::default();
        }
    }
}

impl Screen {
    pub(crate) fn text(&mut self, c: char) {
        let c = self.charset.translate(c);
        let pos = self.grid().pos();
        let size = self.grid().size();
        let attrs = self.attrs;

        let width = c.width();
        if width.is_none() && (u32::from(c)) < 256 {
            // don't even try to draw control characters
            return;
        }
        let width: u16 = width
            .unwrap_or(1)
            .try_into()
            // width() can only return 0, 1, or 2
            .unwrap();

        // A character wider than the whole grid can never be drawn.
        // Bail before `col_wrap`, whose `cols - width` underflows on a
        // one-column grid, and before the write path below, which would
        // otherwise leave a wide head in the last column with no
        // continuation cell after it — the orphan `Row::clear_wide` and
        // the wide-write path both assume away.
        if width > size.cols {
            return;
        }

        // Remember this glyph for REP (CSI Ps b). Zero-width (combining)
        // characters aren't repeatable graphic characters, so skip them.
        if width > 0 {
            self.last_char = Some(c);
        }

        // it doesn't make any sense to wrap if the last column in a row
        // didn't already have contents. don't try to handle the case where a
        // character wraps because there was only one column left in the
        // previous row - literally everything handles this case differently,
        // and this is tmux behavior (and also the simplest). i'm open to
        // reconsidering this behavior, but only with a really good reason
        // (xterm handles this by introducing the concept of triple width
        // cells, which i really don't want to do).
        let mut wrap = false;
        if pos.col > size.cols - width {
            let last_cell = self
                .grid()
                .drawing_cell(crate::grid::Pos {
                    row: pos.row,
                    col: size.cols - 1,
                })
                // pos.row is valid, since it comes directly from
                // self.grid().pos() which we assume to always have a valid
                // row value. size.cols - 1 is also always a valid column.
                .unwrap();
            if last_cell.has_contents() || last_cell.is_wide_continuation() {
                wrap = true;
            }
        }
        if self.mode(MODE_NO_AUTOWRAP) {
            self.grid_mut().col_no_wrap(width);
        } else {
            self.grid_mut().col_wrap(width, wrap);
        }
        let pos = self.grid().pos();

        if width == 0 {
            if pos.col > 0 {
                let mut prev_cell = self
                    .grid_mut()
                    .drawing_cell_mut(crate::grid::Pos {
                        row: pos.row,
                        col: pos.col - 1,
                    })
                    // pos.row is valid, since it comes directly from
                    // self.grid().pos() which we assume to always have a
                    // valid row value. pos.col - 1 is valid because we just
                    // checked for pos.col > 0.
                    .unwrap();
                if prev_cell.is_wide_continuation() {
                    prev_cell = self
                        .grid_mut()
                        .drawing_cell_mut(crate::grid::Pos {
                            row: pos.row,
                            col: pos.col - 2,
                        })
                        // pos.row is valid, since it comes directly from
                        // self.grid().pos() which we assume to always have a
                        // valid row value. we know pos.col - 2 is valid
                        // because the cell at pos.col - 1 is a wide
                        // continuation character, which means there must be
                        // the first half of the wide character before it.
                        .unwrap();
                }
                prev_cell.append(c);
            } else if pos.row > 0 {
                let prev_row = self
                    .grid()
                    .drawing_row(pos.row - 1)
                    // pos.row is valid, since it comes directly from
                    // self.grid().pos() which we assume to always have a
                    // valid row value. pos.row - 1 is valid because we just
                    // checked for pos.row > 0.
                    .unwrap();
                if prev_row.wrapped() {
                    let mut prev_cell = self
                        .grid_mut()
                        .drawing_cell_mut(crate::grid::Pos {
                            row: pos.row - 1,
                            col: size.cols - 1,
                        })
                        // pos.row is valid, since it comes directly from
                        // self.grid().pos() which we assume to always have a
                        // valid row value. pos.row - 1 is valid because we
                        // just checked for pos.row > 0. col of size.cols - 1
                        // is always valid.
                        .unwrap();
                    if prev_cell.is_wide_continuation() {
                        prev_cell = self
                            .grid_mut()
                            .drawing_cell_mut(crate::grid::Pos {
                                row: pos.row - 1,
                                col: size.cols - 2,
                            })
                            // pos.row is valid, since it comes directly from
                            // self.grid().pos() which we assume to always
                            // have a valid row value. pos.row - 1 is valid
                            // because we just checked for pos.row > 0. col of
                            // size.cols - 2 is valid because the cell at
                            // size.cols - 1 is a wide continuation character,
                            // so it must have the first half of the wide
                            // character before it.
                            .unwrap();
                    }
                    prev_cell.append(c);
                }
            }
        } else {
            // IRM (`CSI 4 h`, terminfo `smir`): shift the rest of the
            // row right by this character's width instead of
            // overwriting what is already there. The whole non-private
            // SM/RM family used to go unhandled, so insert mode drew as
            // overwrite and the tail of the line was eaten.
            if self.mode(MODE_INSERT) {
                self.grid_mut().insert_cells(width);
            }

            if self
                .grid()
                .drawing_cell(pos)
                // pos.row is valid because we assume self.grid().pos() to
                // always have a valid row value. pos.col is valid because we
                // called col_wrap() immediately before this, which ensures
                // that self.grid().pos().col has a valid value.
                .unwrap()
                .is_wide_continuation()
            {
                let prev_cell = self
                    .grid_mut()
                    .drawing_cell_mut(crate::grid::Pos {
                        row: pos.row,
                        col: pos.col - 1,
                    })
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() immediately before this, which
                    // ensures that self.grid().pos().col has a valid value.
                    // pos.col - 1 is valid because the cell at pos.col is a
                    // wide continuation character, so it must have the first
                    // half of the wide character before it.
                    .unwrap();
                prev_cell.clear(attrs);
            }

            if self
                .grid()
                .drawing_cell(pos)
                // pos.row is valid because we assume self.grid().pos() to
                // always have a valid row value. pos.col is valid because we
                // called col_wrap() immediately before this, which ensures
                // that self.grid().pos().col has a valid value.
                .unwrap()
                .is_wide()
            {
                let next_cell = self
                    .grid_mut()
                    .drawing_cell_mut(crate::grid::Pos {
                        row: pos.row,
                        col: pos.col + 1,
                    })
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() immediately before this, which
                    // ensures that self.grid().pos().col has a valid value.
                    // pos.col + 1 is valid because the cell at pos.col is a
                    // wide character, so it must have the second half of the
                    // wide character after it.
                    .unwrap();
                next_cell.set(' ', attrs);
            }

            let cell = self
                .grid_mut()
                .drawing_cell_mut(pos)
                // pos.row is valid because we assume self.grid().pos() to
                // always have a valid row value. pos.col is valid because we
                // called col_wrap() immediately before this, which ensures
                // that self.grid().pos().col has a valid value.
                .unwrap();
            cell.set(c, attrs);
            self.grid_mut().col_inc(1);
            if width > 1 {
                let pos = self.grid().pos();
                if self
                    .grid()
                    .drawing_cell(pos)
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() earlier, which ensures that
                    // self.grid().pos().col has a valid value. this is true
                    // even though we just called col_inc, because this branch
                    // only happens if width > 1, and col_wrap takes width
                    // into account.
                    .unwrap()
                    .is_wide()
                {
                    let next_next_pos = crate::grid::Pos {
                        row: pos.row,
                        col: pos.col + 1,
                    };
                    let next_next_cell = self
                        .grid_mut()
                        .drawing_cell_mut(next_next_pos)
                        // pos.row is valid because we assume
                        // self.grid().pos() to always have a valid row value.
                        // pos.col is valid because we called col_wrap()
                        // earlier, which ensures that self.grid().pos().col
                        // has a valid value. this is true even though we just
                        // called col_inc, because this branch only happens if
                        // width > 1, and col_wrap takes width into account.
                        // pos.col + 1 is valid because the cell at pos.col is
                        // wide, and so it must have the second half of the
                        // wide character after it.
                        .unwrap();
                    next_next_cell.clear(attrs);
                    if next_next_pos.col == size.cols - 1 {
                        self.grid_mut()
                            .drawing_row_mut(pos.row)
                            // we assume self.grid().pos().row is always valid
                            .unwrap()
                            .wrap(false);
                    }
                }
                let next_cell = self
                    .grid_mut()
                    .drawing_cell_mut(pos)
                    // pos.row is valid because we assume self.grid().pos() to
                    // always have a valid row value. pos.col is valid because
                    // we called col_wrap() earlier, which ensures that
                    // self.grid().pos().col has a valid value. this is true
                    // even though we just called col_inc, because this branch
                    // only happens if width > 1, and col_wrap takes width
                    // into account.
                    .unwrap();
                next_cell.clear(crate::attrs::Attrs::default());
                next_cell.set_wide_continuation(true);
                self.grid_mut().col_inc(1);
            }
        }
    }

    // control codes

    pub(crate) fn bs(&mut self) {
        self.grid_mut().col_dec(1);
    }

    pub(crate) fn tab(&mut self) {
        self.grid_mut().col_tab();
    }

    pub(crate) fn lf(&mut self) {
        self.grid_mut().row_inc_scroll(1);
        // LNM (`CSI 20 h`): LF, VT and FF also carry the cursor back
        // to column one.
        if self.mode(MODE_NEWLINE) {
            self.cr();
        }
    }

    pub(crate) fn vt(&mut self) {
        self.lf();
    }

    pub(crate) fn ff(&mut self) {
        self.lf();
    }

    pub(crate) fn cr(&mut self) {
        self.grid_mut().col_set(0);
    }

    // escape codes

    // ESC 7
    pub(crate) fn decsc(&mut self) {
        self.save_cursor();
    }

    // ESC 8
    pub(crate) fn decrc(&mut self) {
        self.restore_cursor();
    }

    // ESC =
    pub(crate) fn deckpam(&mut self) {
        self.set_mode(MODE_APPLICATION_KEYPAD);
    }

    // ESC >
    pub(crate) fn deckpnm(&mut self) {
        self.clear_mode(MODE_APPLICATION_KEYPAD);
    }

    // ESC M
    pub(crate) fn ri(&mut self) {
        self.grid_mut().row_dec_scroll(1);
    }

    /// IND (`ESC D`) — index: down one row, scrolling at the bottom of
    /// the scroll region. RI's counterpart, and it was a no-op here:
    /// anything using it to advance a line (less, and any program
    /// driving a scroll region by hand) painted over the row it was
    /// already on.
    pub(crate) fn ind(&mut self) {
        self.grid_mut().row_inc_scroll(1);
    }

    /// NEL (`ESC E`) — next line: IND plus a carriage return.
    pub(crate) fn nel(&mut self) {
        self.grid_mut().row_inc_scroll(1);
        self.grid_mut().col_set(0);
    }

    // SO (0x0E) / SI (0x0F): swap GL between G1 and G0.
    pub(crate) fn shift_out(&mut self) {
        self.charset.shift_out();
    }

    pub(crate) fn shift_in(&mut self) {
        self.charset.shift_in();
    }

    // ESC ( <c> / ESC ) <c> / ESC * <c> / ESC + <c> — designate G0..G3.
    pub(crate) fn designate_charset(&mut self, selector: u8, code: u8) {
        self.charset.designate(selector, code);
    }

    // ESC c
    pub(crate) fn ris(&mut self) {
        *self = Self::new(self.grid.size(), self.grid.scrollback_len());
    }

    // CSI ! p -- DECSTR, soft terminal reset.
    //
    // terminfo's `is2` and `rs2` for xterm-256color both lead with this,
    // so it is the init string of essentially every ncurses program and
    // what `tput init` sends. Going unhandled, it left a dead TUI's
    // scroll region, origin mode, hidden cursor and SGR in place with
    // nothing short of RIS able to clear them.
    //
    // The reset list is the VT510 manual's, which is what xterm, foot and
    // kitty all implement. Two deliberate readings of it:
    //
    // * DECAWM comes back *on*. The manual says reset, but `is2` never
    //   re-enables it and every ncurses program would lose wrapping;
    //   real terminals restore their power-on default, which is on.
    // * The screen is not cleared, the cursor does not move, and the
    //   alternate screen is not left. DECSTR resets modes, not content.
    //
    // Mouse reporting and bracketed paste are likewise untouched: they
    // postdate the manual, and the program that turned them on is the
    // one that turns them off.
    pub(crate) fn decstr(&mut self) {
        self.clear_mode(MODE_HIDE_CURSOR);
        self.clear_mode(MODE_INSERT);
        self.clear_mode(MODE_NEWLINE);
        self.clear_mode(MODE_NO_AUTOWRAP);
        self.clear_mode(MODE_APPLICATION_CURSOR);
        self.clear_mode(MODE_APPLICATION_KEYPAD);
        self.clear_mode(MODE_REVERSE_VIDEO);
        self.cursor_shape = CursorShape::default();
        self.clear_mode(MODE_CURSOR_BLINK);
        self.attrs = crate::attrs::Attrs::default();
        self.charset = crate::charset::CharsetState::default();
        // `set_origin_mode` and `set_scroll_region` both home the
        // cursor. DECSTR does not move it, so put it back.
        let pos = self.grid().pos();
        let rows = self.grid().size().rows;
        self.grid_mut().set_origin_mode(false);
        self.grid_mut().set_scroll_region(0, rows - 1);
        self.grid_mut().set_pos(pos);
        // "Save cursor state: home position" — the DECSC slot resets
        // even though the live cursor stays put.
        let (attrs, charset) = self.saved_sgr_mut();
        *attrs = crate::attrs::Attrs::default();
        *charset = crate::charset::CharsetState::default();
        self.grid_mut().reset_saved_cursor();
    }

    // CSI h -- SM, and CSI l -- RM. The non-private half of the mode
    // family; the DEC-private `?` spellings are `decset` / `decrst`.
    pub(crate) fn sm(
        &mut self,
        params: &vte::Params,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        for param in params {
            match param {
                [4] => self.set_mode(MODE_INSERT),
                [20] => self.set_mode(MODE_NEWLINE),
                _ => unhandled(self),
            }
        }
    }

    pub(crate) fn rm(
        &mut self,
        params: &vte::Params,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        for param in params {
            match param {
                [4] => self.clear_mode(MODE_INSERT),
                [20] => self.clear_mode(MODE_NEWLINE),
                _ => unhandled(self),
            }
        }
    }

    // CSI Ps SP q -- DECSCUSR, cursor style.
    //
    // 0 and 1 are both "blinking block": 0 means "the terminal's
    // default", and a blinking block is what DEC's was.
    pub(crate) fn decscusr(&mut self, style: u16) {
        let (shape, blink) = match style {
            0 | 1 => (CursorShape::Block, true),
            2 => (CursorShape::Block, false),
            3 => (CursorShape::Underline, true),
            4 => (CursorShape::Underline, false),
            5 => (CursorShape::Bar, true),
            6 => (CursorShape::Bar, false),
            // An unknown style is not a reason to change the cursor.
            _ => return,
        };
        self.cursor_shape = shape;
        if blink {
            self.set_mode(MODE_CURSOR_BLINK);
        } else {
            self.clear_mode(MODE_CURSOR_BLINK);
        }
    }

    // CSI I -- CHT, cursor forward tabulation.
    pub(crate) fn cht(&mut self, count: u16) {
        for _ in 0..count {
            self.grid_mut().col_tab();
        }
    }

    // CSI Z -- CBT, cursor backward tabulation (terminfo `cbt`).
    pub(crate) fn cbt(&mut self, count: u16) {
        for _ in 0..count {
            self.grid_mut().col_back_tab();
        }
    }

    // csi codes

    // CSI @
    pub(crate) fn ich(&mut self, count: u16) {
        self.grid_mut().insert_cells(count);
    }

    // CSI Ps b -- REP: repeat the preceding graphic character `count` times.
    // ncurses emits this (via the `rep` terminfo capability) to fill runs such
    // as jtop's gauge bars; without it only the single seed glyph survives.
    pub(crate) fn rep(&mut self, count: u16) {
        if let Some(c) = self.last_char {
            for _ in 0..count {
                self.text(c);
            }
        }
    }

    // CSI A
    pub(crate) fn cuu(&mut self, offset: u16) {
        self.grid_mut().row_dec_clamp(offset);
    }

    // CSI B
    pub(crate) fn cud(&mut self, offset: u16) {
        self.grid_mut().row_inc_clamp(offset);
    }

    // CSI C
    pub(crate) fn cuf(&mut self, offset: u16) {
        self.grid_mut().col_inc_clamp(offset);
    }

    // CSI D
    pub(crate) fn cub(&mut self, offset: u16) {
        self.grid_mut().col_dec(offset);
    }

    // CSI E
    pub(crate) fn cnl(&mut self, offset: u16) {
        self.grid_mut().col_set(0);
        self.grid_mut().row_inc_clamp(offset);
    }

    // CSI F
    pub(crate) fn cpl(&mut self, offset: u16) {
        self.grid_mut().col_set(0);
        self.grid_mut().row_dec_clamp(offset);
    }

    // CSI G
    pub(crate) fn cha(&mut self, col: u16) {
        self.grid_mut().col_set(col - 1);
    }

    // CSI H
    pub(crate) fn cup(&mut self, (row, col): (u16, u16)) {
        self.grid_mut().set_pos(crate::grid::Pos {
            row: row - 1,
            col: col - 1,
        });
    }

    // CSI J
    pub(crate) fn ed(
        &mut self,
        mode: u16,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        let attrs = self.attrs;
        match mode {
            0 => self.grid_mut().erase_all_forward(attrs),
            1 => self.grid_mut().erase_all_backward(attrs),
            2 => self.grid_mut().erase_all(attrs),
            // xterm "Erase Saved Lines" — drops scrollback rows; the
            // live screen and cursor are untouched. Standard `clear(1)`
            // emits this after `2J` so the user-visible scrollback
            // doesn't survive a clear.
            3 => self.grid_mut().clear_scrollback(),
            _ => unhandled(self),
        }
    }

    // CSI ? J
    pub(crate) fn decsed(
        &mut self,
        mode: u16,
        unhandled: impl FnMut(&mut Self),
    ) {
        self.ed(mode, unhandled);
    }

    // CSI K
    pub(crate) fn el(
        &mut self,
        mode: u16,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        let attrs = self.attrs;
        match mode {
            0 => self.grid_mut().erase_row_forward(attrs),
            1 => self.grid_mut().erase_row_backward(attrs),
            2 => self.grid_mut().erase_row(attrs),
            _ => unhandled(self),
        }
    }

    // CSI ? K
    pub(crate) fn decsel(
        &mut self,
        mode: u16,
        unhandled: impl FnMut(&mut Self),
    ) {
        self.el(mode, unhandled);
    }

    // CSI L
    pub(crate) fn il(&mut self, count: u16) {
        self.grid_mut().insert_lines(count);
    }

    // CSI M
    pub(crate) fn dl(&mut self, count: u16) {
        self.grid_mut().delete_lines(count);
    }

    // CSI P
    pub(crate) fn dch(&mut self, count: u16) {
        self.grid_mut().delete_cells(count);
    }

    // CSI S
    pub(crate) fn su(&mut self, count: u16) {
        self.grid_mut().scroll_up(count);
    }

    // CSI T
    pub(crate) fn sd(&mut self, count: u16) {
        self.grid_mut().scroll_down(count);
    }

    // CSI X
    pub(crate) fn ech(&mut self, count: u16) {
        let attrs = self.attrs;
        self.grid_mut().erase_cells(count, attrs);
    }

    // CSI d
    pub(crate) fn vpa(&mut self, row: u16) {
        self.grid_mut().row_set(row - 1);
    }

    // CSI ? h
    pub(crate) fn decset(
        &mut self,
        params: &vte::Params,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        for param in params {
            match param {
                [1] => self.set_mode(MODE_APPLICATION_CURSOR),
                // DECSCNM — terminfo drives `flash` with `?5h … ?5l`.
                [5] => self.set_mode(MODE_REVERSE_VIDEO),
                [6] => self.grid_mut().set_origin_mode(true),
                [7] => self.clear_mode(MODE_NO_AUTOWRAP),
                [9] => self.set_mouse_mode(MouseProtocolMode::Press),
                [12] => self.set_mode(MODE_CURSOR_BLINK),
                [25] => self.clear_mode(MODE_HIDE_CURSOR),
                // 47 and 1047 differ only in whether leaving clears the
                // alt grid, which `exit_alternate_grid` does not do for
                // either; 1048 is DECSC/DECRC under another name.
                [47 | 1047] => self.enter_alternate_grid(),
                [1004] => self.set_mode(MODE_FOCUS_EVENT),
                [1048] => self.decsc(),
                [1000] => {
                    self.set_mouse_mode(MouseProtocolMode::PressRelease);
                }
                [1002] => {
                    self.set_mouse_mode(MouseProtocolMode::ButtonMotion);
                }
                [1003] => self.set_mouse_mode(MouseProtocolMode::AnyMotion),
                [1005] => {
                    self.set_mouse_encoding(MouseProtocolEncoding::Utf8);
                }
                [1006] => {
                    self.set_mouse_encoding(MouseProtocolEncoding::Sgr);
                }
                [1016] => {
                    self.set_mouse_encoding(MouseProtocolEncoding::SgrPixels);
                }
                [1049] => {
                    self.decsc();
                    self.alternate_grid.clear();
                    self.enter_alternate_grid();
                }
                [2004] => self.set_mode(MODE_BRACKETED_PASTE),
                _ => unhandled(self),
            }
        }
    }

    // CSI ? l
    pub(crate) fn decrst(
        &mut self,
        params: &vte::Params,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        for param in params {
            match param {
                [1] => self.clear_mode(MODE_APPLICATION_CURSOR),
                [5] => self.clear_mode(MODE_REVERSE_VIDEO),
                [6] => self.grid_mut().set_origin_mode(false),
                [7] => self.set_mode(MODE_NO_AUTOWRAP),
                [9] => self.clear_mouse_mode(MouseProtocolMode::Press),
                [12] => self.clear_mode(MODE_CURSOR_BLINK),
                [25] => self.set_mode(MODE_HIDE_CURSOR),
                [47 | 1047] => {
                    self.exit_alternate_grid();
                }
                [1004] => self.clear_mode(MODE_FOCUS_EVENT),
                [1048] => self.decrc(),
                [1000] => {
                    self.clear_mouse_mode(MouseProtocolMode::PressRelease);
                }
                [1002] => {
                    self.clear_mouse_mode(MouseProtocolMode::ButtonMotion);
                }
                [1003] => {
                    self.clear_mouse_mode(MouseProtocolMode::AnyMotion);
                }
                [1005] => {
                    self.clear_mouse_encoding(MouseProtocolEncoding::Utf8);
                }
                [1006] => {
                    self.clear_mouse_encoding(MouseProtocolEncoding::Sgr);
                }
                [1016] => {
                    self.clear_mouse_encoding(MouseProtocolEncoding::SgrPixels);
                }
                [1049] => {
                    self.exit_alternate_grid();
                    self.decrc();
                }
                [2004] => self.clear_mode(MODE_BRACKETED_PASTE),
                _ => unhandled(self),
            }
        }
    }

    // CSI m
    pub(crate) fn sgr(
        &mut self,
        params: &vte::Params,
        mut unhandled: impl FnMut(&mut Self),
    ) {
        // XXX really i want to just be able to pass in a default Params
        // instance with a 0 in it, but vte doesn't allow creating new Params
        // instances
        if params.is_empty() {
            self.attrs = crate::attrs::Attrs::default();
            return;
        }

        let mut iter = params.iter();

        macro_rules! next_param {
            () => {
                match iter.next() {
                    Some(n) => n,
                    _ => return,
                }
            };
        }

        macro_rules! to_u8 {
            ($n:expr) => {
                if let Some(n) = u16_to_u8($n) {
                    n
                } else {
                    return;
                }
            };
        }

        macro_rules! next_param_u8 {
            () => {
                if let &[n] = next_param!() {
                    to_u8!(n)
                } else {
                    return;
                }
            };
        }

        loop {
            match next_param!() {
                [0] => self.attrs = crate::attrs::Attrs::default(),
                [1] => self.attrs.set_bold(),
                [2] => self.attrs.set_dim(),
                [3] => self.attrs.set_italic(true),
                [4] => self.attrs.set_underline(true),
                // `4:n` — the T.416 subparameter form, and the only
                // spelling that can name an underline *shape*. It used
                // to match no arm at all, so an nvim diagnostic asking
                // for a curly underline got no underline whatsoever.
                [4, style] => self.attrs.set_underline_style(
                    underline_style_from_subparam(*style),
                ),
                // 6 is "rapid blink"; no terminal distinguishes the two
                // rates, so both land on the same bit.
                [5 | 6] => self.attrs.set_blink(true),
                [7] => self.attrs.set_inverse(true),
                [8] => self.attrs.set_conceal(true),
                [9] => self.attrs.set_strikethrough(true),
                [21] => self
                    .attrs
                    .set_underline_style(Some(crate::UnderlineStyle::Double)),
                [22] => self.attrs.set_normal_intensity(),
                [23] => self.attrs.set_italic(false),
                [24] => self.attrs.set_underline(false),
                [25] => self.attrs.set_blink(false),
                [27] => self.attrs.set_inverse(false),
                [28] => self.attrs.set_conceal(false),
                [29] => self.attrs.set_strikethrough(false),
                [53] => self.attrs.set_overline(true),
                [55] => self.attrs.set_overline(false),
                // SGR 58/59 set the underline's *colour*. The colour
                // itself is not stored — a per-cell `Color` costs four
                // bytes in a struct that has none to spare, and the
                // underline is drawn in the text colour — but the
                // parameters have to be consumed either way: left to
                // the fallthrough, `58;2;r;g;b` resumed at `2`, which
                // is SGR "dim", and quietly dimmed the text.
                [58, _, ..] | [59] => {}
                [58] => match next_param!() {
                    [2] => {
                        let _ = next_param_u8!();
                        let _ = next_param_u8!();
                        let _ = next_param_u8!();
                    }
                    [5] => {
                        let _ = next_param_u8!();
                    }
                    _ => {
                        unhandled(self);
                        return;
                    }
                },
                [n] if (30..=37).contains(n) => {
                    self.attrs.fgcolor = crate::Color::Idx(to_u8!(*n) - 30);
                }
                [38, 2, r, g, b] => {
                    self.attrs.fgcolor =
                        crate::Color::Rgb(to_u8!(*r), to_u8!(*g), to_u8!(*b));
                }
                // The ITU-T T.416 form `38:2:<colour-space>:r:g:b`,
                // whose colour-space id is almost always empty. Every
                // library that emits colons emits this one, and it was
                // being dropped on the floor — the text came out in
                // the previous colour with no hint why.
                [38, 2, _colour_space, r, g, b] => {
                    self.attrs.fgcolor =
                        crate::Color::Rgb(to_u8!(*r), to_u8!(*g), to_u8!(*b));
                }
                [38, 5, i] => {
                    self.attrs.fgcolor = crate::Color::Idx(to_u8!(*i));
                }
                [38] => match next_param!() {
                    [2] => {
                        let r = next_param_u8!();
                        let g = next_param_u8!();
                        let b = next_param_u8!();
                        self.attrs.fgcolor = crate::Color::Rgb(r, g, b);
                    }
                    [5] => {
                        self.attrs.fgcolor =
                            crate::Color::Idx(next_param_u8!());
                    }
                    _ => {
                        unhandled(self);
                        return;
                    }
                },
                [39] => {
                    self.attrs.fgcolor = crate::Color::Default;
                }
                [n] if (40..=47).contains(n) => {
                    self.attrs.bgcolor = crate::Color::Idx(to_u8!(*n) - 40);
                }
                [48, 2, r, g, b] => {
                    self.attrs.bgcolor =
                        crate::Color::Rgb(to_u8!(*r), to_u8!(*g), to_u8!(*b));
                }
                // See the `38:2::r:g:b` note above.
                [48, 2, _colour_space, r, g, b] => {
                    self.attrs.bgcolor =
                        crate::Color::Rgb(to_u8!(*r), to_u8!(*g), to_u8!(*b));
                }
                [48, 5, i] => {
                    self.attrs.bgcolor = crate::Color::Idx(to_u8!(*i));
                }
                [48] => match next_param!() {
                    [2] => {
                        let r = next_param_u8!();
                        let g = next_param_u8!();
                        let b = next_param_u8!();
                        self.attrs.bgcolor = crate::Color::Rgb(r, g, b);
                    }
                    [5] => {
                        self.attrs.bgcolor =
                            crate::Color::Idx(next_param_u8!());
                    }
                    _ => {
                        unhandled(self);
                        return;
                    }
                },
                [49] => {
                    self.attrs.bgcolor = crate::Color::Default;
                }
                [n] if (90..=97).contains(n) => {
                    self.attrs.fgcolor = crate::Color::Idx(to_u8!(*n) - 82);
                }
                [n] if (100..=107).contains(n) => {
                    self.attrs.bgcolor = crate::Color::Idx(to_u8!(*n) - 92);
                }
                _ => unhandled(self),
            }
        }
    }

    // CSI r
    pub(crate) fn decstbm(&mut self, (top, bottom): (u16, u16)) {
        self.grid_mut().set_scroll_region(top - 1, bottom - 1);
    }
}

/// `4:n` — the underline styles of ITU-T T.416, as spoken by every
/// terminal that draws more than one. `4:0` is the "no underline"
/// spelling of `SGR 24`; anything past `4:5` is unnamed, and a plain
/// underline is the closest honest reading of it.
fn underline_style_from_subparam(n: u16) -> Option<crate::UnderlineStyle> {
    match n {
        0 => None,
        2 => Some(crate::UnderlineStyle::Double),
        3 => Some(crate::UnderlineStyle::Curly),
        4 => Some(crate::UnderlineStyle::Dotted),
        5 => Some(crate::UnderlineStyle::Dashed),
        _ => Some(crate::UnderlineStyle::Single),
    }
}

fn u16_to_u8(i: u16) -> Option<u8> {
    if i > u16::from(u8::MAX) {
        None
    } else {
        // safe because we just ensured that the value fits in a u8
        Some(i.try_into().unwrap())
    }
}

#[cfg(test)]
mod scs_tests {
    use crate::Parser;

    fn cell_at(p: &Parser, row: u16, col: u16) -> &str {
        p.screen().cell(row, col).unwrap().contents()
    }

    #[test]
    fn rep_repeats_preceding_graphic_char() {
        let mut p = Parser::new(2, 10, 0);
        // Print `|` once, then REP 4 -> five `|` total (ncurses gauge fill).
        p.process(b"|\x1b[4b");
        for col in 0..5 {
            assert_eq!(cell_at(&p, 0, col), "|");
        }
        assert_eq!(cell_at(&p, 0, 5), "");
        assert_eq!(p.screen().cursor_position(), (0, 5));
    }

    #[test]
    fn rep_default_param_is_one() {
        let mut p = Parser::new(2, 10, 0);
        // No parameter defaults to a single repeat.
        p.process(b"X\x1b[b");
        assert_eq!(cell_at(&p, 0, 0), "X");
        assert_eq!(cell_at(&p, 0, 1), "X");
        assert_eq!(cell_at(&p, 0, 2), "");
    }

    #[test]
    fn rep_repeats_charset_translated_glyph() {
        let mut p = Parser::new(2, 10, 0);
        // DEC line-drawing `q` (─) then REP must repeat the translated glyph,
        // not the raw `q`.
        p.process(b"\x1b(0q\x1b[2b");
        assert_eq!(cell_at(&p, 0, 0), "─");
        assert_eq!(cell_at(&p, 0, 1), "─");
        assert_eq!(cell_at(&p, 0, 2), "─");
    }

    #[test]
    fn rep_without_preceding_char_is_noop() {
        let mut p = Parser::new(2, 10, 0);
        p.process(b"\x1b[5b");
        assert_eq!(cell_at(&p, 0, 0), "");
        assert_eq!(p.screen().cursor_position(), (0, 0));
    }

    #[test]
    fn esc_paren_zero_then_back() {
        let mut p = Parser::new(2, 10, 0);
        // Switch G0 to DEC Special Graphics, draw `lqqk`, then back to ASCII
        // and draw `AB`.
        p.process(b"\x1b(0lqqk\x1b(BAB");
        assert_eq!(cell_at(&p, 0, 0), "┌");
        assert_eq!(cell_at(&p, 0, 1), "─");
        assert_eq!(cell_at(&p, 0, 2), "─");
        assert_eq!(cell_at(&p, 0, 3), "┐");
        assert_eq!(cell_at(&p, 0, 4), "A");
        assert_eq!(cell_at(&p, 0, 5), "B");
    }

    #[test]
    fn so_si_swap_between_g0_and_g1() {
        let mut p = Parser::new(2, 10, 0);
        // Designate G1 as DEC Special Graphics, leave G0 ASCII. SO selects
        // G1, SI returns to G0.
        p.process(b"\x1b)0A\x0eq\x0fB");
        assert_eq!(cell_at(&p, 0, 0), "A"); // G0 ASCII
        assert_eq!(cell_at(&p, 0, 1), "─"); // after SO, GL=G1=DEC
        assert_eq!(cell_at(&p, 0, 2), "B"); // after SI, GL=G0=ASCII
    }

    #[test]
    fn ris_resets_charset() {
        let mut p = Parser::new(2, 10, 0);
        p.process(b"\x1b(0");
        // RIS should drop us back to ASCII.
        p.process(b"\x1bc");
        p.process(b"q");
        assert_eq!(cell_at(&p, 0, 0), "q");
    }

    #[test]
    fn decsc_decrc_save_and_restore_charset() {
        let mut p = Parser::new(2, 10, 0);
        // Save while ASCII, switch to DEC, restore — must be back to ASCII.
        p.process(b"\x1b7\x1b(0\x1b8q");
        assert_eq!(cell_at(&p, 0, 0), "q");
    }

    #[test]
    fn alt_screen_1049_preserves_charset() {
        let mut p = Parser::new(2, 10, 0);
        // Establish DEC on G0, enter alt screen via 1049, switch back to
        // ASCII inside, leave alt screen — main charset must still be DEC.
        p.process(b"\x1b(0");
        p.process(b"\x1b[?1049h");
        p.process(b"\x1b(B");
        p.process(b"\x1b[?1049l");
        p.process(b"q");
        assert_eq!(cell_at(&p, 0, 0), "─");
    }

    #[test]
    fn unicode_is_not_corrupted_by_dec_set() {
        // `é` is outside the 0x21..=0x7E range, so even with DEC active it
        // must pass through unchanged.
        let mut p = Parser::new(2, 10, 0);
        p.process("\x1b(0é".as_bytes());
        assert_eq!(cell_at(&p, 0, 0), "é");
    }
}

#[cfg(test)]
mod resize_tests {
    use crate::Parser;

    /// 15 numbered lines on a 10-row screen: 6 scrolled into history,
    /// `line6`..`line14` visible, cursor on the blank bottom row.
    fn parser_with_lines() -> Parser {
        let mut p = Parser::new(10, 20, 100);
        for i in 0..15 {
            p.process(format!("line{i}\r\n").as_bytes());
        }
        p
    }

    #[test]
    fn vertical_shrink_pushes_top_rows_keeping_cursor_line() {
        let mut p = parser_with_lines();
        assert_eq!(p.screen().cursor_position(), (9, 0));
        assert_eq!(p.screen().top_of_live_screen(), 6);

        p.screen_mut().set_size(6, 20);
        // The four rows above the kept window went into scrollback
        // (not truncated off the bottom) and the cursor line survives
        // as the new bottom row.
        assert_eq!(p.screen().cursor_position(), (5, 0));
        assert_eq!(p.screen().scrollback_fill(), 10);
        assert_eq!(p.screen().top_of_live_screen(), 10);
        assert!(p.screen().contents().starts_with("line10"));
    }

    #[test]
    fn vertical_shrink_grow_roundtrip_restores_layout() {
        let mut p = parser_with_lines();
        let before = p.screen().contents();
        p.screen_mut().set_size(6, 20);
        p.screen_mut().set_size(10, 20);
        assert_eq!(p.screen().cursor_position(), (9, 0));
        assert_eq!(p.screen().scrollback_fill(), 6);
        assert_eq!(p.screen().top_of_live_screen(), 6);
        assert_eq!(p.screen().contents(), before);
    }

    #[test]
    fn grow_without_scrollback_extends_bottom() {
        let mut p = Parser::new(4, 20, 100);
        p.process(b"a\r\nb");
        // Nothing in scrollback to pull; growing adds blank rows at
        // the bottom and leaves the cursor line alone.
        p.screen_mut().set_size(8, 20);
        assert_eq!(p.screen().cursor_position(), (1, 1));
        assert_eq!(p.screen().top_of_live_screen(), 0);
    }

    #[test]
    fn resize_with_scroll_region_keeps_legacy_truncation() {
        let mut p = Parser::new(10, 20, 100);
        p.process(b"\x1b[2;5r");
        p.screen_mut().set_size(6, 20);
        assert_eq!(p.screen().top_of_live_screen(), 0);
        assert_eq!(p.screen().scrollback_fill(), 0);
    }

    #[test]
    fn alt_screen_shrink_keeps_top_rows_instead_of_discarding_them() {
        // A full-screen program (no scroll region, cursor parked near
        // the bottom by its own cursor-relative redraw) on the
        // alternate screen, which has no scrollback to push into.
        let mut p = Parser::new(10, 20, 100);
        p.process(b"\x1b[?1049h");
        // 9 lines, not 10: the trailing `\r\n` after the last one would
        // otherwise overflow the 10-row screen and scroll line0 off
        // for real, which is a different code path than the resize
        // this test is about.
        for i in 0..9 {
            p.process(format!("line{i}\r\n").as_bytes());
        }
        p.process(b"\x1b[8;1H"); // park the cursor mid-screen, Ink-style

        p.screen_mut().set_size(6, 20);
        // Before the fix this deleted `(cursor.row + 1) - 6 = 2` rows
        // off the top with nowhere to put them, corrupting content the
        // program never asked to move. The alt grid has no scrollback,
        // so a shrink must fall back to plain truncate/extend: the top
        // rows stay exactly where the program left them.
        assert!(p.screen().contents().starts_with("line0"));
        assert_eq!(p.screen().scrollback_fill(), 0);

        p.screen_mut().set_size(10, 20);
        p.process(b"\x1b[?1049l");
        // The main screen underneath was never touched by the alt
        // screen's resize.
        assert_eq!(p.screen().top_of_live_screen(), 0);
    }

    #[test]
    fn shrink_without_scrollback_capacity_truncates_instead_of_discarding() {
        // Even on the main screen, a grid configured with zero
        // scrollback capacity has nowhere to push rows into, so a
        // shrink must not delete them either.
        let mut p = Parser::new(10, 20, 0);
        for i in 0..9 {
            p.process(format!("line{i}\r\n").as_bytes());
        }
        p.process(b"\x1b[8;1H");

        p.screen_mut().set_size(6, 20);
        assert!(p.screen().contents().starts_with("line0"));
        assert_eq!(p.screen().top_of_live_screen(), 0);
    }
}

/// `top_of_live_screen` is the absolute line coordinate VGE elements
/// and Scrollback portals anchor to, so it has to stay exact through
/// the cases that used to fool the host's probe-and-hash heuristic:
/// width changes, saturated scrollback, and vertical push/pull.
#[cfg(test)]
mod top_of_live_screen_tests {
    use crate::Parser;

    #[test]
    fn advances_by_exact_scroll_count() {
        let mut p = Parser::new(5, 20, 100);
        // 12 CRLF-terminated lines on a 5-row screen: the cursor
        // reaches the bottom row after 4 line feeds, so the remaining
        // 8 each scroll one line into history.
        p.process(&b"x\r\n".repeat(12));
        assert_eq!(p.screen().top_of_live_screen(), 8);
    }

    #[test]
    fn counts_scrolls_past_scrollback_saturation() {
        // Tiny scrollback: the 4-line cap saturates immediately.
        let mut p = Parser::new(2, 10, 4);
        p.process(&b"\n".repeat(20));
        // 20 LFs on a 2-row screen: the first moves the cursor to the
        // bottom row, every later one scrolls — 19 exactly, even
        // though only 4 scrollback rows survive.
        assert_eq!(p.screen().top_of_live_screen(), 19);
        assert_eq!(p.screen().scrollback_fill(), 4);
    }

    #[test]
    fn width_resize_does_not_move_it() {
        let mut p = Parser::new(10, 80, 100);
        p.process(&b"line\r\n".repeat(15));
        let before = p.screen().top_of_live_screen();
        for cols in [70, 50, 90] {
            p.screen_mut().set_size(10, cols);
            p.process(b"$ ");
        }
        assert_eq!(p.screen().top_of_live_screen(), before);
    }

    #[test]
    fn vertical_resize_keeps_cursor_line_anchored() {
        let mut p = Parser::new(10, 80, 100);
        p.process(&b"line\r\n".repeat(15));
        // The cursor's absolute line is the invariant a vcat image is
        // placed against; shrink + grow must preserve it throughout.
        let cursor_abs = |p: &Parser| {
            p.screen().top_of_live_screen()
                + i64::from(p.screen().cursor_position().0)
        };
        let anchor = cursor_abs(&p);

        p.screen_mut().set_size(6, 80); // shrink: pushes rows
        assert_eq!(cursor_abs(&p), anchor);

        p.screen_mut().set_size(12, 80); // grow: pulls them back
        assert_eq!(cursor_abs(&p), anchor);
    }

    #[test]
    fn scroll_region_scrolls_do_not_count() {
        let mut p = Parser::new(10, 20, 100);
        // Restrict scrolling to rows 2..=5, park the cursor at the
        // region bottom, and force region scrolls.
        p.process(b"\x1b[2;5r\x1b[5;1H");
        p.process(&b"\n".repeat(6));
        assert_eq!(p.screen().top_of_live_screen(), 0);
    }

    #[test]
    fn alt_screen_freezes_it_until_the_main_screen_returns() {
        let mut p = Parser::new(5, 20, 100);
        p.process(&b"x\r\n".repeat(12));
        let before = p.screen().top_of_live_screen();

        // Scrolling the alternate screen must not move the main
        // screen's line origin — the alt grid has no scrollback and
        // main-screen anchors have to survive the round trip.
        p.process(b"\x1b[?1049h");
        p.process(&b"y\r\n".repeat(20));
        assert_eq!(p.screen().top_of_live_screen(), before);

        p.process(b"\x1b[?1049l");
        assert_eq!(p.screen().top_of_live_screen(), before);
    }

    #[test]
    fn resize_during_alt_screen_moves_the_main_origin() {
        let mut p = Parser::new(10, 80, 100);
        p.process(&b"line\r\n".repeat(15));
        let before = p.screen().top_of_live_screen();

        // A shrink while the alt screen is up still pushes main-grid
        // rows into scrollback, so the main screen's text genuinely
        // moved and anchors have to follow it.
        p.process(b"\x1b[?1049h");
        p.screen_mut().set_size(6, 80);
        p.process(b"\x1b[?1049l");
        assert_eq!(p.screen().top_of_live_screen(), before + 4);
    }

    #[test]
    fn survives_a_snapshot_roundtrip() {
        let mut p = Parser::new(5, 20, 100);
        p.process(&b"x\r\n".repeat(12));
        let bytes = p.screen().binary_snapshot();

        let mut q = Parser::new(5, 20, 100);
        q.screen_mut().restore_from_binary_snapshot(&bytes).unwrap();
        assert_eq!(q.screen().top_of_live_screen(), 8);
        // And keeps counting from the restored origin.
        q.process(&b"x\r\n".repeat(3));
        assert_eq!(q.screen().top_of_live_screen(), 11);
    }
}

/// Marker search, the other half of that coordinate space: an offset
/// from the top of the live screen that `top_of_live_screen` turns into
/// an absolute line.
#[cfg(test)]
mod marker_search_tests {
    use crate::Parser;

    /// `n` numbered lines on a 5-row screen. Line `i` is written at
    /// absolute line `i`, so a search answer is checkable against the
    /// number printed on the row. Bracketed so that `row[1]` is not
    /// also a substring of `row[10]`.
    fn numbered(n: usize, scrollback: usize) -> Parser {
        let mut p = Parser::new(5, 20, scrollback);
        for i in 0..n {
            p.process(format!("row[{i}]\r\n").as_bytes());
        }
        p
    }

    /// Absolute line the search resolves `needle` to.
    fn line_of(p: &Parser, needle: &str) -> Option<i64> {
        Some(
            p.screen().top_of_live_screen()
                + p.screen().last_row_offset_containing(needle)?,
        )
    }

    #[test]
    fn a_live_row_is_a_non_negative_offset() {
        let p = numbered(4, 100);
        // Nothing has scrolled, so offsets and absolute lines coincide.
        assert_eq!(p.screen().last_row_offset_containing("row[2]"), Some(2));
        assert_eq!(p.screen().top_of_live_screen(), 0);
    }

    #[test]
    fn a_scrolled_off_row_is_a_negative_offset() {
        // The bug this exists for: two reserved regions in one message
        // scroll the first marker away before anything anchors to it.
        // 12 lines on a 5-row screen leaves `row[0]`..`row[7]` in
        // history.
        let p = numbered(12, 100);
        assert_eq!(p.screen().top_of_live_screen(), 8);
        assert_eq!(p.screen().last_row_offset_containing("row[1]"), Some(-7));
        // Which is still the row that printed it.
        assert_eq!(line_of(&p, "row[1]"), Some(1));
        assert_eq!(line_of(&p, "row[9]"), Some(9));
    }

    #[test]
    fn the_live_screen_wins_over_scrollback() {
        // An application that reprints its token every frame anchors to
        // the copy on screen, never to a stale one in history.
        let mut p = numbered(12, 100);
        p.process(b"row[1] again\r\n");
        assert_eq!(line_of(&p, "row[1]"), Some(12));
    }

    #[test]
    fn the_users_scroll_position_is_not_an_input() {
        // The offset is relative to the live screen, which is the space
        // `top_of_live_screen` names. Scrolling the *view* must not
        // move a marker's line, or an anchor placed while the user
        // happened to be scrolled up would land somewhere else.
        let mut p = numbered(12, 100);
        let live = p.screen().last_row_offset_containing("row[1]");
        p.screen_mut().set_scrollback(6);
        assert_eq!(p.screen().last_row_offset_containing("row[1]"), live);
        p.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(p.screen().last_row_offset_containing("row[1]"), live);
    }

    #[test]
    fn a_row_dropped_from_the_ring_is_gone() {
        // Past the ring there is no row to name, and no way to tell how
        // far back it went — `None`, so the caller can fall back rather
        // than anchor to a guess.
        let p = numbered(12, 2);
        assert_eq!(p.screen().last_row_offset_containing("row[1]"), None);
        assert_eq!(p.screen().last_row_offset_containing("row[6]"), Some(-2));
    }

    #[test]
    fn absent_and_empty_needles_match_nothing() {
        let p = numbered(4, 100);
        assert_eq!(p.screen().last_row_offset_containing("nope"), None);
        // An empty needle trivially matches every row; refusing it
        // keeps a client from anchoring to the bottom row by accident.
        assert_eq!(p.screen().last_row_offset_containing(""), None);
    }

    #[test]
    fn the_alternate_screen_searches_itself() {
        // No scrollback there, and `top_of_live_screen` stays frozen at
        // the main screen's value, so an alt-screen marker resolves as
        // the viewport offset it is.
        let mut p = numbered(12, 100);
        let frozen = p.screen().top_of_live_screen();
        p.process(b"\x1b[?1049h");
        p.process(b"alt-token\r\n");
        assert_eq!(p.screen().last_row_offset_containing("alt-token"), Some(0));
        // The main screen's rows are not visible from here.
        assert_eq!(p.screen().last_row_offset_containing("row[9]"), None);
        assert_eq!(p.screen().top_of_live_screen(), frozen);
    }
}

#[cfg(test)]
mod binary_snapshot_tests {
    use crate::Parser;

    /// Apply `bytes` to a fresh parser, then snapshot. Used to verify
    /// that encode→decode→encode is byte-equal.
    fn snapshot_after(bytes: &[u8], rows: u16, cols: u16, scrollback: usize) -> Vec<u8> {
        let mut p = Parser::new(rows, cols, scrollback);
        p.process(bytes);
        p.screen().binary_snapshot()
    }

    fn restore_into_fresh(bytes: &[u8], rows: u16, cols: u16, scrollback: usize) -> Parser {
        let mut p = Parser::new(rows, cols, scrollback);
        p.screen_mut()
            .restore_from_binary_snapshot(bytes)
            .expect("restore");
        p
    }

    #[test]
    fn empty_screen_roundtrips_byte_equal() {
        let bytes1 = snapshot_after(b"", 4, 16, 100);
        let restored = restore_into_fresh(&bytes1, 4, 16, 100);
        let bytes2 = restored.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn visible_screen_with_attrs_roundtrips_byte_equal() {
        let bytes1 = snapshot_after(
            b"\x1b[31mhello\x1b[m \x1b[1mworld\x1b[m\r\nsecond line",
            5,
            20,
            100,
        );
        let restored = restore_into_fresh(&bytes1, 5, 20, 100);
        let bytes2 = restored.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn scrollback_roundtrips_byte_equal() {
        // 10 lines, only 3 visible — 7 should land in scrollback.
        let mut input = Vec::new();
        for i in 0..10 {
            input.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        let bytes1 = snapshot_after(&input, 3, 10, 100);
        let restored = restore_into_fresh(&bytes1, 3, 10, 100);
        let bytes2 = restored.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);
        // Trailing `\r\n` after "line 9" scrolls the visible area one
        // more time, so 8 rows (not 7) end up in scrollback. The
        // byte-equality assertion above is the load-bearing one;
        // this sanity-check just confirms the scrollback is populated.
        let sb = restored.screen().grid().scrollback_rows().count();
        assert!(sb >= 7, "expected >= 7 scrollback rows, got {sb}");
    }

    #[test]
    fn cell_at_reads_at_a_view_offset_without_moving_the_grid() {
        // 3 visible rows, 10 lines written — 8 land in scrollback.
        let mut p = Parser::new(3, 10, 100);
        for i in 0..10 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        let row_at = |off: usize, row: u16| -> String {
            (0..10)
                .filter_map(|c| p.screen().cell_at(off, row, c))
                .map(crate::Cell::contents)
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // A view offset of k shifts the window k lines up, so row r at
        // offset k is the same text as row r-k at offset 0. This is the
        // whole contract two PRT views on one buffer depend on.
        assert_eq!(row_at(1, 1), row_at(0, 0));
        assert_eq!(row_at(1, 2), row_at(0, 1));
        assert_eq!(row_at(2, 2), row_at(0, 0));
        // ...and the offsets genuinely differ, so the assertions above
        // aren't comparing an all-blank grid to itself.
        assert_ne!(row_at(0, 0), row_at(2, 0));

        // Reading never moves the buffer: it stays live for everyone.
        assert_eq!(p.screen().scrollback(), 0);
        // An offset past the ring clamps instead of underflowing.
        let _ = row_at(9_999, 0);
    }

    #[test]
    fn alt_screen_state_preserved() {
        // Enter alt-screen, write something on it; the primary grid
        // still has its pre-alt contents.
        let mut p1 = Parser::new(4, 10, 50);
        p1.process(b"primary\r\n");
        // Save cursor + enter alt-screen via DECSET 1049.
        p1.process(b"\x1b[?1049h");
        p1.process(b"ALT");
        let bytes1 = p1.screen().binary_snapshot();

        // Restore into a fresh parser, then re-snapshot.
        let mut p2 = Parser::new(4, 10, 50);
        p2.screen_mut()
            .restore_from_binary_snapshot(&bytes1)
            .unwrap();
        let bytes2 = p2.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);

        // Exit alt-screen — restored "primary" must reappear.
        p2.process(b"\x1b[?1049l");
        // The visible row 0 should contain "primary" again.
        let row0: String = (0..10)
            .filter_map(|c| p2.screen().cell(0, c))
            .map(|c| if c.has_contents() { c.contents().to_string() } else { " ".into() })
            .collect();
        assert!(row0.starts_with("primary"), "got {row0:?}");
    }

    #[test]
    fn decsc_saved_cursor_preserved() {
        // Move to (1, 2), save cursor, then move elsewhere; saved
        // position must survive a snapshot round-trip.
        let mut p1 = Parser::new(5, 20, 50);
        p1.process(b"\x1b[2;3H"); // CUP row=2 col=3 (1-based) → (1,2) 0-based
        p1.process(b"\x1b7"); // DECSC: save cursor
        p1.process(b"\x1b[5;10H"); // move elsewhere
        let bytes1 = p1.screen().binary_snapshot();

        let p2 = restore_into_fresh(&bytes1, 5, 20, 50);
        let bytes2 = p2.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);

        // DECRC on the restored parser should land back at (1, 2).
        let mut p2 = p2;
        p2.process(b"\x1b8"); // DECRC: restore cursor
        assert_eq!(p2.screen().cursor_position(), (1, 2));
    }

    #[test]
    fn scroll_region_preserved() {
        let mut p1 = Parser::new(10, 20, 50);
        // DECSTBM rows 3..=7 (1-based).
        p1.process(b"\x1b[3;7r");
        let bytes1 = p1.screen().binary_snapshot();
        let bytes2 = restore_into_fresh(&bytes1, 10, 20, 50)
            .screen()
            .binary_snapshot();
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn charset_state_preserved() {
        // Designate G1 = DEC special graphics; SO selects G1.
        let mut p1 = Parser::new(3, 20, 50);
        p1.process(b"\x1b)0"); // designate G1 = special graphics
        p1.process(b"\x0e"); // SO — shift to G1 (GL = 1)
        p1.process(b"qx"); // 'q' → ─, 'x' → │
        let bytes1 = p1.screen().binary_snapshot();
        let bytes2 = restore_into_fresh(&bytes1, 3, 20, 50)
            .screen()
            .binary_snapshot();
        assert_eq!(bytes1, bytes2);

        // After restore, typing more 'q'/'x' should still be in graphics.
        let mut p2 = restore_into_fresh(&bytes1, 3, 20, 50);
        p2.process(b"q");
        let next_cell = p2.screen().cell(0, 2).unwrap();
        assert_eq!(next_cell.contents(), "─");
    }

    #[test]
    fn mouse_mode_and_input_modes_preserved() {
        let mut p1 = Parser::new(4, 16, 0);
        p1.process(b"\x1b[?1000h"); // mouse X11 press
        p1.process(b"\x1b[?1006h"); // SGR encoding
        p1.process(b"\x1b[?2004h"); // bracketed paste
        p1.process(b"\x1b="); // application keypad
        let bytes1 = p1.screen().binary_snapshot();
        let p2 = restore_into_fresh(&bytes1, 4, 16, 0);
        let bytes2 = p2.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);
        assert!(p2.screen().bracketed_paste());
        assert!(p2.screen().application_keypad());
    }

    #[test]
    fn sgr_pixel_mouse_encoding_roundtrips() {
        // ?1016 is SGR framing with pixel coordinates; it supersedes
        // ?1006 the way any other encoding selection does, and `?1016l`
        // drops back to the legacy encoding rather than to ?1006.
        let mut p1 = Parser::new(4, 16, 0);
        p1.process(b"\x1b[?1002h\x1b[?1006h\x1b[?1016h");
        assert_eq!(
            p1.screen().mouse_protocol_encoding(),
            crate::MouseProtocolEncoding::SgrPixels
        );
        let bytes1 = p1.screen().binary_snapshot();
        let p2 = restore_into_fresh(&bytes1, 4, 16, 0);
        assert_eq!(
            p2.screen().mouse_protocol_encoding(),
            crate::MouseProtocolEncoding::SgrPixels
        );
        assert_eq!(bytes1, p2.screen().binary_snapshot());
        // The re-emitted input modes name the mode the sender chose.
        assert!(
            p2.screen()
                .input_mode_formatted()
                .windows(8)
                .any(|w| w == b"[?1016h")
                || p2.screen().input_mode_formatted().ends_with(b"\x1b[?1016h")
        );

        p1.process(b"\x1b[?1016l");
        assert_eq!(
            p1.screen().mouse_protocol_encoding(),
            crate::MouseProtocolEncoding::Default
        );
    }

    #[test]
    fn wide_character_cell_roundtrips() {
        // CJK and emoji are wide; the continuation cell carries the flag.
        let mut p1 = Parser::new(3, 10, 0);
        p1.process("日本".as_bytes());
        let bytes1 = p1.screen().binary_snapshot();
        let p2 = restore_into_fresh(&bytes1, 3, 10, 0);
        let bytes2 = p2.screen().binary_snapshot();
        assert_eq!(bytes1, bytes2);
        assert!(p2.screen().cell(0, 0).unwrap().is_wide());
        assert!(p2.screen().cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(p2.screen().cell(0, 0).unwrap().contents(), "日");
    }

    #[test]
    fn version_mismatch_rejects() {
        let p = Parser::new(1, 1, 0);
        let mut bytes = p.screen().binary_snapshot();
        // First two bytes are the u16 SNAPSHOT_KIND_VERSION; corrupt them.
        bytes[0] = 0xFF;
        bytes[1] = 0xFF;
        let mut p2 = Parser::new(1, 1, 0);
        let err = p2.screen_mut().restore_from_binary_snapshot(&bytes);
        assert!(err.is_err());
    }

    #[test]
    fn truncated_payload_rejects() {
        let p = Parser::new(2, 4, 0);
        let bytes = p.screen().binary_snapshot();
        let mut p2 = Parser::new(2, 4, 0);
        // Lop off the tail; should fail somewhere in decoding.
        let err = p2.screen_mut().restore_from_binary_snapshot(&bytes[..bytes.len() - 1]);
        assert!(err.is_err());
    }

    #[test]
    fn trailing_garbage_rejects() {
        let p = Parser::new(2, 4, 0);
        let mut bytes = p.screen().binary_snapshot();
        bytes.push(0xAA);
        let mut p2 = Parser::new(2, 4, 0);
        let err = p2.screen_mut().restore_from_binary_snapshot(&bytes);
        assert!(err.is_err());
    }

    /// Offset of the main grid's `pos.row` field: the u16 kind version,
    /// then the grid's `rows` / `cols`.
    const POS_ROW_OFFSET: usize = 2 + 2 + 2;

    /// A cursor past the last row must be refused, not installed. It
    /// used to restore cleanly and panic on the next printed byte —
    /// and these bytes arrive over SSH from a build we don't control,
    /// so "the sender wouldn't do that" isn't a guarantee.
    #[test]
    fn cursor_outside_the_grid_rejects() {
        let mut p = Parser::new(24, 80, 100);
        p.process(b"hello");
        let mut bytes = p.screen().binary_snapshot();
        bytes[POS_ROW_OFFSET..POS_ROW_OFFSET + 2]
            .copy_from_slice(&200u16.to_le_bytes());

        let mut p2 = Parser::new(24, 80, 100);
        assert!(p2.screen_mut().restore_from_binary_snapshot(&bytes).is_err());
        // The rejected snapshot left the screen usable.
        p2.process(b"x");
        assert_eq!(p2.screen().cell(0, 0).unwrap().contents(), "x");
    }

    /// An absurd row count is refused rather than handed to
    /// `Vec::with_capacity`, which aborts the process — past any
    /// `Result` the caller could act on.
    #[test]
    fn implausible_row_count_rejects_instead_of_aborting() {
        let p = Parser::new(2, 4, 0);
        let bytes = p.screen().binary_snapshot();
        // The grid's row-count varu follows the fixed header: kind
        // version + rows/cols + pos + saved_pos + scroll top/bottom +
        // two mode bools.
        let count_at = 2 + (2 + 2) + (2 + 2) + (2 + 2) + 2 + 2 + 1 + 1;
        let mut corrupt = bytes[..count_at].to_vec();
        // LEB128 for a value near u64::MAX.
        corrupt.extend_from_slice(&[0xFF; 9]);
        corrupt.push(0x01);
        corrupt.extend_from_slice(&bytes[count_at..]);

        let mut p2 = Parser::new(2, 4, 0);
        assert!(p2
            .screen_mut()
            .restore_from_binary_snapshot(&corrupt)
            .is_err());
    }
}

#[cfg(test)]
mod wide_char_safety_tests {
    use crate::Parser;

    /// A width shrink can cut the continuation half off a wide
    /// character, leaving its head alone in the last column. Erasing
    /// that cell stepped to `col + 1` and panicked the whole process.
    #[test]
    fn erase_over_a_wide_head_orphaned_by_a_shrink() {
        let mut p = Parser::new(24, 4, 100);
        p.process("ab\u{4e00}".as_bytes());
        p.screen_mut().set_size(24, 3);
        p.process(b"\x1b[1;3H\x1b[K");
        assert_eq!(p.screen().cell(0, 2).unwrap().contents(), "");
    }

    /// Same orphan, reached by overwriting it instead of erasing it.
    #[test]
    fn print_over_a_wide_head_orphaned_by_a_shrink() {
        let mut p = Parser::new(24, 4, 100);
        p.process("ab\u{4e00}".as_bytes());
        p.screen_mut().set_size(24, 3);
        p.process(b"\x1b[1;3Hx");
        assert_eq!(p.screen().cell(0, 2).unwrap().contents(), "x");
    }

    /// The shrink itself must leave no orphan behind: the head is
    /// cleared along with its lost continuation.
    #[test]
    fn a_width_shrink_leaves_no_orphaned_wide_head() {
        let mut p = Parser::new(24, 4, 100);
        p.process("ab\u{4e00}".as_bytes());
        p.screen_mut().set_size(24, 3);
        assert!(!p.screen().cell(0, 2).unwrap().is_wide());
    }

    /// A character wider than the grid can't be drawn at all.
    /// `cols - width` used to underflow before anything noticed.
    #[test]
    fn wide_char_on_a_one_column_grid_is_dropped() {
        let mut p = Parser::new(24, 1, 100);
        p.process("\u{4e00}".as_bytes());
        assert_eq!(p.screen().cell(0, 0).unwrap().contents(), "");
        assert_eq!(p.screen().cursor_position(), (0, 0));
        // Still usable afterwards.
        p.process(b"x");
        assert_eq!(p.screen().cell(0, 0).unwrap().contents(), "x");
    }

    /// A snapshot carrying an orphaned head — from a build whose
    /// resize path predates the repair — is fixed up on the way in
    /// rather than becoming a panic on the receiver.
    #[test]
    fn restored_orphaned_wide_head_is_repaired() {
        let mut p = Parser::new(24, 4, 100);
        p.process("ab\u{4e00}".as_bytes());
        p.screen_mut().set_size(24, 3);
        let bytes = p.screen().binary_snapshot();

        let mut p2 = Parser::new(24, 3, 100);
        p2.screen_mut()
            .restore_from_binary_snapshot(&bytes)
            .expect("restore");
        assert!(!p2.screen().cell(0, 2).unwrap().is_wide());
        p2.process(b"\x1b[1;3H\x1b[K");
    }
}

#[cfg(test)]
mod xterm_semantics_tests {
    use crate::Parser;

    fn row(p: &Parser, row: u16) -> String {
        let (_, cols) = p.screen().size();
        (0..cols)
            .map(|c| {
                let s = p.screen().cell(row, c).unwrap().contents();
                if s.is_empty() { " ".to_string() } else { s.to_string() }
            })
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// DECSTBM homes the cursor to the page's home position, not the
    /// region's. Homing to the region put the cursor rows down the
    /// screen for every program that sets a region without DECOM.
    #[test]
    fn decstbm_homes_to_absolute_one_one() {
        let mut p = Parser::new(24, 80, 0);
        p.process(b"\x1b[5;20r");
        assert_eq!(p.screen().cursor_position(), (0, 0));
    }

    /// …and to the region's top when origin mode says coordinates are
    /// region-relative.
    #[test]
    fn decstbm_homes_to_the_region_under_origin_mode() {
        let mut p = Parser::new(24, 80, 0);
        p.process(b"\x1b[?6h\x1b[5;20r");
        assert_eq!(p.screen().cursor_position(), (4, 0));
    }

    /// HVP (CSI f) is CUP by another name. apt's fancy progress bar
    /// reserves the bottom row with DECSTBM and then parks its status
    /// line there with `CSI <rows> ; 0 f`; while HVP went unhandled the
    /// bar was drawn wherever the cursor happened to be and then
    /// scrolled up with the text, one smear per redraw.
    #[test]
    fn hvp_addresses_the_cursor_like_cup() {
        let mut p = Parser::new(4, 10, 0);
        // Region rows 1..3 — the bottom row is reserved.
        p.process(b"\x1b[1;3r");
        p.process(b"a\r\nb\r\nc");
        // Park the status line on the reserved row and come back.
        p.process(b"\x1b7\x1b[4;0fbar\x1b8");
        assert_eq!(row(&p, 3), "bar");
        // Further output scrolls the region only; the bar stays put.
        p.process(b"\r\nd");
        assert_eq!(row(&p, 0), "b");
        assert_eq!(row(&p, 2), "d");
        assert_eq!(row(&p, 3), "bar", "the status line scrolled away");
    }

    /// SCOSC / SCORC (CSI s / CSI u), the ANSI.SYS spelling of
    /// DECSC / DECRC.
    #[test]
    fn scosc_and_scorc_save_and_restore_the_cursor() {
        let mut p = Parser::new(4, 10, 0);
        p.process(b"\x1b[2;3H\x1b[s\x1b[4;1H\x1b[u");
        assert_eq!(p.screen().cursor_position(), (1, 2));
    }

    /// DECSTR is terminfo's `is2` *and* `rs2` for xterm-256color, so it
    /// opens essentially every ncurses program. Unhandled, a dead TUI's
    /// scroll region, origin mode, hidden cursor and SGR all outlived
    /// it, and only RIS could clear them.
    #[test]
    fn decstr_resets_the_modes_the_vt510_manual_lists() {
        let mut p = Parser::new(10, 20, 0);
        p.process(b"\x1b[3;6r\x1b[?6h\x1b[7m\x1b[?25l\x1b[4h\x1b[?7l\x1b(0");
        p.process(b"\x1b[!p");

        // Origin mode off, so absolute addressing reaches row 1 again.
        p.process(b"\x1b[H");
        assert_eq!(p.screen().cursor_position(), (0, 0));
        assert!(!p.screen().hide_cursor());
        assert!(!p.screen().insert_mode());
        // DECAWM comes back on: `is2` never re-enables it, so a reset
        // that left it off would cost every ncurses program its wrapping.
        assert!(p.screen().autowrap());
        p.process(b"q");
        assert!(!p.screen().cell(0, 0).unwrap().inverse(), "SGR survived");
        // `\x1b(0` had put G0 on DEC line drawing, where `q` is a
        // horizontal rule; back on ASCII it is a `q` again.
        assert_eq!(row(&p, 0), "q", "G0 charset survived");
    }

    /// DECSTR resets modes, not content: the cursor stays where it is
    /// and the screen keeps its text.
    #[test]
    fn decstr_moves_neither_the_cursor_nor_the_text() {
        let mut p = Parser::new(4, 10, 0);
        p.process(b"hello\r\nworld\x1b[!p");
        assert_eq!(p.screen().cursor_position(), (1, 5));
        assert_eq!(row(&p, 0), "hello");
        assert_eq!(row(&p, 1), "world");
    }

    /// IRM (terminfo `smir`). The whole non-private SM/RM family went
    /// unhandled, so insert mode drew as overwrite.
    #[test]
    fn irm_shifts_the_rest_of_the_row_right() {
        let mut p = Parser::new(2, 10, 0);
        p.process(b"abcdef\x1b[1;1H\x1b[4hXY");
        assert_eq!(row(&p, 0), "XYabcdef");
        p.process(b"\x1b[4l\x1b[1;1HZ");
        assert_eq!(row(&p, 0), "ZYabcdef");
    }

    /// LNM (`CSI 20 h`) — LF also returns to column one.
    #[test]
    fn lnm_makes_lf_carry_the_cursor_home() {
        let mut p = Parser::new(3, 10, 0);
        p.process(b"\x1b[20habc\nd");
        assert_eq!(row(&p, 0), "abc");
        assert_eq!(row(&p, 1), "d");
    }

    /// CBT (terminfo `cbt`) and CHT walk the eight-column tab stops.
    #[test]
    fn cht_and_cbt_walk_the_tab_stops() {
        let mut p = Parser::new(2, 40, 0);
        p.process(b"\x1b[3I");
        assert_eq!(p.screen().cursor_position(), (0, 24));
        p.process(b"\x1b[2Z");
        assert_eq!(p.screen().cursor_position(), (0, 8));
        // Column 0 is a floor, not a wrap.
        p.process(b"\x1b[9Z");
        assert_eq!(p.screen().cursor_position(), (0, 0));
    }

    /// DECSCUSR (terminfo `Ss` / `Se`) — the odd parameters ask for a
    /// blink, the even ones don't.
    #[test]
    fn decscusr_sets_the_cursor_shape_and_blink() {
        let mut p = Parser::new(2, 10, 0);
        assert_eq!(p.screen().cursor_shape(), crate::CursorShape::Block);
        assert!(!p.screen().cursor_blink());

        p.process(b"\x1b[5 q");
        assert_eq!(p.screen().cursor_shape(), crate::CursorShape::Bar);
        assert!(p.screen().cursor_blink());

        p.process(b"\x1b[4 q");
        assert_eq!(p.screen().cursor_shape(), crate::CursorShape::Underline);
        assert!(!p.screen().cursor_blink());

        // `Se` is `CSI 2 SP q` — back to a steady block.
        p.process(b"\x1b[2 q");
        assert_eq!(p.screen().cursor_shape(), crate::CursorShape::Block);
        assert!(!p.screen().cursor_blink());
    }

    /// `?12` is the other way to ask for a blink, and `cnorm`
    /// (`\E[?12l\E[?25h`) is how every ncurses program turns it off.
    #[test]
    fn decset_12_drives_the_same_blink_bit() {
        let mut p = Parser::new(2, 10, 0);
        p.process(b"\x1b[?12;25h");
        assert!(p.screen().cursor_blink());
        assert!(!p.screen().hide_cursor());
        p.process(b"\x1b[?12l\x1b[?25h");
        assert!(!p.screen().cursor_blink());
    }

    /// The SGR attributes terminfo names and the parser used to drop.
    #[test]
    fn the_sgr_attributes_terminfo_emits_all_land() {
        let mut p = Parser::new(2, 20, 0);
        p.process(b"\x1b[5;8;9;53mx");
        let c = p.screen().cell(0, 0).unwrap();
        assert!(c.blink() && c.conceal() && c.strikethrough() && c.overline());

        p.process(b"\x1b[25;28;29;55my");
        let c = p.screen().cell(0, 1).unwrap();
        assert!(
            !c.blink() && !c.conceal() && !c.strikethrough() && !c.overline()
        );
    }

    /// `4:3` is how nvim and every LSP client draw a diagnostic. It
    /// matched no arm at all, so the underline was lost outright —
    /// not even a plain one survived.
    #[test]
    fn colon_underline_styles_are_applied() {
        let mut p = Parser::new(2, 20, 0);
        p.process(b"\x1b[4:3mx\x1b[4:0my\x1b[21mz");
        let x = p.screen().cell(0, 0).unwrap();
        assert_eq!(x.underline_style(), Some(crate::UnderlineStyle::Curly));
        assert!(!p.screen().cell(0, 1).unwrap().underline(), "4:0 is off");
        assert_eq!(
            p.screen().cell(0, 2).unwrap().underline_style(),
            Some(crate::UnderlineStyle::Double),
        );
    }

    /// SGR 58 is the underline colour. The colour is not stored, but
    /// its parameters must still be consumed: left to the fallthrough,
    /// `58;2;r;g;b` resumed at `2`, which is SGR "dim".
    #[test]
    fn sgr_58_does_not_leak_into_the_dim_attribute() {
        for seq in [
            b"\x1b[58;2;255;0;0mx".as_ref(),
            b"\x1b[58:2::255:0:0mx".as_ref(),
            b"\x1b[58;5;9mx".as_ref(),
        ] {
            let mut p = Parser::new(2, 20, 0);
            p.process(seq);
            let c = p.screen().cell(0, 0).unwrap();
            assert!(!c.dim(), "seq {seq:?} dimmed the text");
            assert_eq!(c.fgcolor(), crate::Color::Default);
        }
    }

    /// An attribute the parser doesn't model must not swallow the
    /// parameters after it.
    #[test]
    fn an_unknown_sgr_does_not_eat_the_rest_of_the_sequence() {
        let mut p = Parser::new(2, 20, 0);
        p.process(b"\x1b[73;31mx");
        assert_eq!(p.screen().cell(0, 0).unwrap().fgcolor(), crate::Color::Idx(1));
    }

    /// DECSCNM, which terminfo drives `flash` with.
    #[test]
    fn decscnm_tracks_reverse_video() {
        let mut p = Parser::new(2, 10, 0);
        assert!(!p.screen().reverse_video());
        p.process(b"\x1b[?5h");
        assert!(p.screen().reverse_video());
        p.process(b"\x1b[?5l");
        assert!(!p.screen().reverse_video());
    }

    /// `?1048` is DECSC/DECRC under another name, and `?1047` shares
    /// the alternate grid with `?47`.
    #[test]
    fn decset_1047_and_1048_are_the_alt_screen_and_saved_cursor() {
        let mut p = Parser::new(4, 10, 0);
        p.process(b"\x1b[2;3H\x1b[?1048h\x1b[4;1H\x1b[?1048l");
        assert_eq!(p.screen().cursor_position(), (1, 2));

        p.process(b"\x1b[?1047h");
        assert!(p.screen().alternate_screen());
        p.process(b"\x1b[?1047l");
        assert!(!p.screen().alternate_screen());
    }

    /// Every new attribute has to survive the snapshot the daemon
    /// ships to an attaching renderer.
    #[test]
    fn the_new_attributes_and_cursor_shape_round_trip_a_snapshot() {
        let mut p1 = Parser::new(4, 20, 10);
        p1.process(b"\x1b[4:3;5;9;53mstyled\x1b[6 q\x1b[?5h\x1b[4h");
        let mut p2 = Parser::new(4, 20, 10);
        p2.screen_mut()
            .restore_from_binary_snapshot(&p1.screen().binary_snapshot())
            .unwrap();
        let c = p2.screen().cell(0, 0).unwrap();
        assert_eq!(c.underline_style(), Some(crate::UnderlineStyle::Curly));
        assert!(c.blink() && c.strikethrough() && c.overline());
        assert_eq!(p2.screen().cursor_shape(), crate::CursorShape::Bar);
        assert!(!p2.screen().cursor_blink());
        assert!(p2.screen().reverse_video());
        assert!(p2.screen().insert_mode());
    }

    /// IL and DL are ignored with the cursor outside the scroll
    /// region. Without the guard they shuffled rows the command has no
    /// business touching.
    #[test]
    fn il_and_dl_outside_the_scroll_region_are_no_ops() {
        for seq in [b"\x1b[L".as_ref(), b"\x1b[M".as_ref()] {
            let mut p = Parser::new(6, 10, 0);
            p.process(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
            // Region rows 3..4 (1-based), cursor parked on row 1.
            p.process(b"\x1b[3;4r\x1b[1;1H");
            p.process(seq);
            for (i, expected) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
                let i = u16::try_from(i).unwrap();
                assert_eq!(&row(&p, i), expected, "seq {seq:?} moved row {i}");
            }
        }
    }

    /// DL must not pull rows from below the region into it.
    #[test]
    fn dl_is_bounded_by_the_scroll_region() {
        let mut p = Parser::new(6, 10, 0);
        p.process(b"a\r\nb\r\nc\r\nd\r\ne\r\nf");
        p.process(b"\x1b[2;3r\x1b[2;1H\x1b[9M");
        assert_eq!(row(&p, 0), "a");
        assert_eq!(row(&p, 1), "");
        assert_eq!(row(&p, 2), "");
        assert_eq!(row(&p, 3), "d", "DL reached past the region");
        assert_eq!(row(&p, 5), "f");
    }

    /// RI with the cursor above a scroll region moves it (or, at row
    /// 0, does nothing) — it does not scroll a region it isn't in.
    #[test]
    fn ri_above_the_scroll_region_does_not_scroll_it() {
        let mut p = Parser::new(5, 10, 0);
        p.process(b"a\r\nb\r\nc\r\nd\r\ne");
        p.process(b"\x1b[3;5r\x1b[1;1H\x1bM");
        assert_eq!(p.screen().cursor_position(), (0, 0));
        for (i, expected) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            let i = u16::try_from(i).unwrap();
            assert_eq!(&row(&p, i), expected, "row {i} moved");
        }
    }

    /// With no region set the whole grid is the region, so the
    /// ordinary "RI at the top scrolls" case still works.
    #[test]
    fn ri_at_the_top_still_scrolls_without_a_region() {
        let mut p = Parser::new(3, 10, 0);
        p.process(b"a\r\nb\r\nc\x1b[1;1H\x1bM");
        assert_eq!(row(&p, 0), "");
        assert_eq!(row(&p, 1), "a");
    }

    /// IND advances a row, scrolling at the bottom of the region. It
    /// was a no-op, so anything using it to advance a line painted
    /// over the row it was already on.
    #[test]
    fn ind_advances_a_row() {
        let mut p = Parser::new(4, 10, 0);
        p.process(b"abc\x1bD");
        assert_eq!(p.screen().cursor_position(), (1, 3));
        p.process(b"x");
        assert_eq!(row(&p, 0), "abc");
        assert_eq!(row(&p, 1), "   x");
    }

    /// NEL is IND plus a carriage return.
    #[test]
    fn nel_advances_a_row_and_returns_to_column_zero() {
        let mut p = Parser::new(4, 10, 0);
        p.process(b"abc\x1bEx");
        assert_eq!(row(&p, 1), "x");
        assert_eq!(p.screen().cursor_position(), (1, 1));
    }

    #[test]
    fn ind_scrolls_at_the_bottom_of_the_region() {
        let mut p = Parser::new(4, 10, 0);
        p.process(b"a\r\nb\r\nc\r\nd");
        p.process(b"\x1b[1;3r\x1b[3;1H\x1bD");
        assert_eq!(row(&p, 0), "b");
        assert_eq!(row(&p, 1), "c");
        assert_eq!(row(&p, 2), "");
        assert_eq!(row(&p, 3), "d", "the row below the region moved");
    }

    /// A DECSC inside the alt screen used to overwrite what `?1049h`
    /// saved on the way in, so the shell a full-screen program
    /// returned to inherited whatever colour it last used.
    #[test]
    fn decsc_in_the_alt_screen_leaves_the_main_screen_s_sgr_alone() {
        let mut p = Parser::new(4, 10, 0);
        // Main screen: red text, then into the alt screen.
        p.process(b"\x1b[31m\x1b[?1049h");
        // A full-screen program saving and restoring around green.
        p.process(b"\x1b[32m\x1b7\x1b[34m\x1b8");
        p.process(b"\x1b[?1049l");
        assert_eq!(p.screen().fgcolor(), crate::Color::Idx(1), "main SGR clobbered");
    }

    /// EL at the pending-wrap position: `pos.col` is `cols` there, so
    /// `cols..cols` erased nothing. xterm erases from the last column.
    #[test]
    fn el_at_the_pending_wrap_position_erases_the_last_column() {
        let mut p = Parser::new(2, 4, 0);
        p.process(b"abcd");
        assert_eq!(row(&p, 0), "abcd");
        p.process(b"\x1b[K");
        assert_eq!(row(&p, 0), "abc");
    }

    /// The ITU-T T.416 colon form with an empty colour-space id, which
    /// is what almost everything that emits colons emits.
    #[test]
    fn colon_truecolor_with_an_empty_colour_space_is_applied() {
        let mut p = Parser::new(2, 10, 0);
        p.process(b"\x1b[38:2::10:20:30mx");
        assert_eq!(p.screen().fgcolor(), crate::Color::Rgb(10, 20, 30));
        p.process(b"\x1b[48:2::40:50:60my");
        assert_eq!(p.screen().bgcolor(), crate::Color::Rgb(40, 50, 60));
    }

    #[test]
    fn colon_truecolor_without_the_colour_space_still_works() {
        let mut p = Parser::new(2, 10, 0);
        p.process(b"\x1b[38:2:10:20:30mx");
        assert_eq!(p.screen().fgcolor(), crate::Color::Rgb(10, 20, 30));
    }

    /// With DECAWM off the cursor stays on the row and the last cell
    /// is overwritten, rather than the text wrapping.
    #[test]
    fn decawm_off_overwrites_the_last_column() {
        let mut p = Parser::new(4, 4, 0);
        p.process(b"\x1b[?7l");
        p.process(b"abcdef");
        assert_eq!(p.screen().cursor_position().0, 0, "output wrapped");
        assert_eq!(row(&p, 0), "abcf");
        assert_eq!(row(&p, 1), "");
    }

    #[test]
    fn decawm_is_on_by_default_and_can_be_turned_back_on() {
        let mut p = Parser::new(4, 4, 0);
        assert!(p.screen().autowrap());
        p.process(b"\x1b[?7l");
        assert!(!p.screen().autowrap());
        p.process(b"\x1b[?7h");
        assert!(p.screen().autowrap());
        p.process(b"abcde");
        assert_eq!(p.screen().cursor_position(), (1, 1));
    }

    /// `Grid::set_size` resizes every row every time, so clearing the
    /// soft-wrap flag on a same-length resize meant any resize at all
    /// broke the wrap joining of every wrapped line on screen.
    #[test]
    fn a_height_only_resize_keeps_soft_wrap_flags() {
        let mut p = Parser::new(3, 4, 100);
        p.process(b"abcdef");
        let before = p.screen().contents();
        p.screen_mut().set_size(5, 4);
        assert_eq!(p.screen().contents(), before);
    }

    #[test]
    fn a_same_size_resize_keeps_soft_wrap_flags() {
        let mut p = Parser::new(3, 4, 100);
        p.process(b"abcdef");
        let before = p.screen().contents();
        p.screen_mut().set_size(3, 4);
        assert_eq!(p.screen().contents(), before);
    }
}
