//! Keyboard + SGR-mouse input parsing. Stateful, so escape sequences
//! split across reads are reassembled.
//!
//! Derived from `vplay`'s parser, with two changes the editor needs:
//! printable keys surface as `Key(char)` rather than being pre-bound to
//! actions (phase 2 maps tool letters without touching the parser), and
//! mouse events carry *which* button, since right-drag pans while
//! left-drag draws.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    fn from_bits(b: u32) -> Option<Self> {
        Some(match b & 0b11 {
            0 => Button::Left,
            1 => Button::Middle,
            2 => Button::Right,
            _ => return None, // 3 = no button
        })
    }
}

/// Where the pointer is, in the two resolutions a mouse report can
/// carry.
///
/// `col`/`row` are the 0-indexed cell — what chrome hit-testing wants,
/// since the palette is laid out in cells. `x`/`y` are the same
/// position in *fractional* cells, which is what the camera and the
/// snap grid want.
///
/// Under SGR-Pixels (?1016) the fractional pair is exact. Under a
/// cell-only encoding the report says which cell the pointer is in and
/// nothing finer, so `x`/`y` are that cell's centre: the unbiased
/// estimate, and the reason `drag::snap`'s grid sits on cell centres.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub col: u16,
    pub row: u16,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Quit,
    /// Cancel / deselect. Distinct from `Quit` — an editor needs Esc.
    Escape,
    Key(char),
    Arrow(Dir),
    /// Delete or Backspace — remove the selection, or a character
    /// while editing text.
    Delete,
    /// Return / Enter — commit text, or start editing a selection.
    Enter,
    /// Ctrl-Z / Ctrl-Y / Ctrl-S.
    Undo,
    Redo,
    Save,
    MouseDown { button: Button, at: Pos },
    MouseUp { button: Button, at: Pos },
    /// Pointer motion. `held` is the dragged button, if any.
    MouseMove { at: Pos, held: Option<Button> },
    WheelUp { at: Pos },
    WheelDown { at: Pos },
}

#[derive(Default)]
pub struct InputParser {
    buf: Vec<u8>,
    /// Cell size in pixels once the caller has switched the terminal to
    /// SGR-Pixels (?1016). `None` means reports still name cells.
    cell: Option<(f32, f32)>,
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read mouse reports as SGR-Pixels: the two coordinates are pixels
    /// within the pane rather than cells, and `cell` — the probe's
    /// `cell_pixel_*` — converts them to the fractional cell position
    /// the camera works in.
    ///
    /// Call this only once ?1016 has actually been written to the
    /// terminal, and write it only once the VGE probe has answered: a
    /// terminal that ignored the request keeps sending cells, and
    /// reading a cell number as a pixel puts the pointer a factor of
    /// `cell` away from where it is.
    pub fn set_pixel_mouse(&mut self, cell: (f32, f32)) {
        self.cell = Some((cell.0.max(1.0), cell.1.max(1.0)));
    }

    /// Feed freshly-read bytes; returns the events that completed.
    /// Incomplete escape sequences are retained for the next call.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        self.drain(false)
    }

    /// Called on idle timeout: a lone ESC still buffered is a real Esc
    /// press rather than the start of an unfinished sequence.
    pub fn flush(&mut self) -> Vec<Event> {
        self.drain(true)
    }

    fn drain(&mut self, eof: bool) -> Vec<Event> {
        let mut out = Vec::new();
        let mut i = 0;
        let b = std::mem::take(&mut self.buf);
        while i < b.len() {
            match b[i] {
                0x1B => {
                    if i + 1 >= b.len() {
                        if eof {
                            out.push(Event::Escape);
                            i += 1;
                        } else {
                            break; // keep the lone ESC buffered
                        }
                        continue;
                    }
                    if b[i + 1] == b'[' {
                        if i + 2 < b.len() && b[i + 2] == b'<' {
                            match find_mouse_end(&b[i..]) {
                                Some(len) => {
                                    if let Some(ev) = parse_sgr_mouse(&b[i..i + len], self.cell) {
                                        out.push(ev);
                                    }
                                    i += len;
                                }
                                None => break,
                            }
                        } else {
                            match find_csi_end(&b[i..]) {
                                Some(len) => {
                                    if let Some(ev) = parse_csi(&b[i..i + len]) {
                                        out.push(ev);
                                    }
                                    i += len;
                                }
                                None => break,
                            }
                        }
                    } else if b[i + 1] == b'O' {
                        // SS3 cursor keys: ESC O <A|B|C|D>
                        if i + 2 < b.len() {
                            if let Some(ev) = arrow_from_final(b[i + 2]) {
                                out.push(ev);
                            }
                            i += 3;
                        } else {
                            break;
                        }
                    } else {
                        // Alt-<key> and friends: ignore the prefix.
                        i += 2;
                    }
                }
                0x03 => {
                    out.push(Event::Quit);
                    i += 1;
                }
                // Backspace (0x08) and DEL (0x7F) both delete.
                0x08 | 0x7F => {
                    out.push(Event::Delete);
                    i += 1;
                }
                // Raw mode gives CR for Return; accept LF too.
                0x0D | 0x0A => {
                    out.push(Event::Enter);
                    i += 1;
                }
                // Raw mode delivers these as bare control bytes; flow
                // control is off, so 0x13 is Ctrl-S and not XOFF.
                0x1A => {
                    out.push(Event::Undo);
                    i += 1;
                }
                0x19 => {
                    out.push(Event::Redo);
                    i += 1;
                }
                0x13 => {
                    out.push(Event::Save);
                    i += 1;
                }
                c if c.is_ascii_graphic() || c == b' ' => {
                    out.push(Event::Key(c as char));
                    i += 1;
                }
                c if c >= 0x80 => {
                    // Multi-byte UTF-8 — Cyrillic, accented Latin, CJK,
                    // emoji. Text fields hold a `String`, so the scalar
                    // has to be decoded here rather than byte-fed.
                    let len = utf8_len(c);
                    if i + len > b.len() {
                        if !eof {
                            break; // split read: keep the tail buffered
                        }
                        i = b.len(); // truncated; nothing more is coming
                        continue;
                    }
                    if let Ok(s) = std::str::from_utf8(&b[i..i + len])
                        && let Some(ch) = s.chars().next()
                    {
                        out.push(Event::Key(ch));
                    }
                    i += len;
                }
                _ => {
                    i += 1; // ignore other control bytes
                }
            }
        }
        if i < b.len() {
            self.buf.extend_from_slice(&b[i..]);
        }
        out
    }
}

/// Byte length of a UTF-8 scalar from its lead byte.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn arrow_from_final(f: u8) -> Option<Event> {
    Some(Event::Arrow(match f {
        b'A' => Dir::Up,
        b'B' => Dir::Down,
        b'C' => Dir::Right,
        b'D' => Dir::Left,
        _ => return None,
    }))
}

/// Length of a CSI sequence starting at `s[0] == ESC`, including the
/// final byte (0x40..=0x7E). `None` if not yet complete.
fn find_csi_end(s: &[u8]) -> Option<usize> {
    let mut j = 2; // after ESC [
    while j < s.len() {
        if (0x40..=0x7E).contains(&s[j]) {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

fn parse_csi(s: &[u8]) -> Option<Event> {
    // ESC [ 3 ~ is Delete; everything else we handle is a cursor key.
    if s.last() == Some(&b'~') {
        let params = &s[2..s.len() - 1];
        return (params == b"3").then_some(Event::Delete);
    }
    arrow_from_final(*s.last()?)
}

/// Length of an SGR mouse report `ESC [ < ... (M|m)`.
fn find_mouse_end(s: &[u8]) -> Option<usize> {
    let mut j = 3; // after ESC [ <
    while j < s.len() {
        if s[j] == b'M' || s[j] == b'm' {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

/// `cell` is `Some` when the terminal is reporting in SGR-Pixels; the
/// framing is identical either way, only the meaning of the two
/// numbers differs.
fn parse_sgr_mouse(s: &[u8], cell: Option<(f32, f32)>) -> Option<Event> {
    // s = ESC [ < b ; x ; y (M|m)
    let final_byte = *s.last()?;
    let text = std::str::from_utf8(&s[3..s.len() - 1]).ok()?;
    let mut parts = text.split(';');
    let b: u32 = parts.next()?.parse().ok()?;
    let px: u32 = parts.next()?.parse().ok()?;
    let py: u32 = parts.next()?.parse().ok()?;
    // Both encodings are 1-indexed; the rest of the editor works in
    // 0-indexed cells.
    let (x, y) = match cell {
        Some((cw, ch)) => (
            px.saturating_sub(1) as f32 / cw,
            py.saturating_sub(1) as f32 / ch,
        ),
        // Cell coordinates say nothing about where inside the cell the
        // pointer is; its centre is the unbiased guess (see [`Pos`]).
        None => (
            px.saturating_sub(1) as f32 + 0.5,
            py.saturating_sub(1) as f32 + 0.5,
        ),
    };
    let at = Pos {
        col: x as u16,
        row: y as u16,
        x,
        y,
    };

    if b & 64 != 0 {
        // Wheel: 64 = up, 65 = down.
        return Some(if b & 1 == 0 {
            Event::WheelUp { at }
        } else {
            Event::WheelDown { at }
        });
    }
    if b & 32 != 0 {
        // Motion; the button bits carry the held button (3 = none).
        return Some(Event::MouseMove {
            at,
            held: Button::from_bits(b),
        });
    }
    let button = Button::from_bits(b)?;
    Some(match final_byte {
        b'M' => Event::MouseDown { button, at },
        _ => Event::MouseUp { button, at },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_keys_surface_raw() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"sB+"),
            vec![Event::Key('s'), Event::Key('B'), Event::Key('+')]
        );
    }

    #[test]
    fn ctrl_c_quits_but_esc_does_not() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(&[0x03]), vec![Event::Quit]);
        assert!(p.feed(b"\x1b").is_empty());
        assert_eq!(p.flush(), vec![Event::Escape]);
    }

    #[test]
    fn parses_arrows_and_reassembles_splits() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"\x1b[A\x1b[B"),
            vec![Event::Arrow(Dir::Up), Event::Arrow(Dir::Down)]
        );
        assert!(p.feed(b"\x1b[").is_empty());
        assert_eq!(p.feed(b"C"), vec![Event::Arrow(Dir::Right)]);
    }

    #[test]
    fn delete_arrives_from_csi_and_from_raw_bytes() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"\x1b[3~\x7f\x08"),
            vec![Event::Delete, Event::Delete, Event::Delete]
        );
    }

    #[test]
    fn enter_arrives_as_cr_or_lf() {
        let mut p = InputParser::new();
        assert_eq!(p.feed(b"\r\n"), vec![Event::Enter, Event::Enter]);
    }

    #[test]
    fn space_is_a_key_not_a_command() {
        // Text editing needs spaces to reach the buffer intact.
        let mut p = InputParser::new();
        assert_eq!(p.feed(b"a b"), vec![
            Event::Key('a'),
            Event::Key(' '),
            Event::Key('b')
        ]);
    }

    #[test]
    fn non_ascii_text_reaches_the_buffer() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed("привет".as_bytes()),
            "привет".chars().map(Event::Key).collect::<Vec<_>>()
        );
        assert_eq!(p.feed("é漢🙂".as_bytes()), vec![
            Event::Key('é'),
            Event::Key('漢'),
            Event::Key('🙂')
        ]);
    }

    #[test]
    fn a_scalar_split_across_reads_is_reassembled() {
        let mut p = InputParser::new();
        let ch = "ж".as_bytes();
        assert!(p.feed(&ch[..1]).is_empty());
        assert_eq!(p.feed(&ch[1..]), vec![Event::Key('ж')]);
    }

    #[test]
    fn a_truncated_scalar_does_not_wedge_the_parser() {
        // Without the eof drop, the partial lead byte stays buffered and
        // every later keystroke is stuck behind it.
        let mut p = InputParser::new();
        assert!(p.feed(&"ж".as_bytes()[..1]).is_empty());
        assert!(p.flush().is_empty());
        assert_eq!(p.feed(b"a"), vec![Event::Key('a')]);
    }

    #[test]
    fn other_tilde_sequences_are_not_delete() {
        let mut p = InputParser::new();
        // Home (1~), Insert (2~), PgUp (5~) must not delete the selection.
        assert!(p.feed(b"\x1b[1~\x1b[2~\x1b[5~").is_empty());
    }

    /// Cell coordinates land mid-cell, which is all a cell report can
    /// honestly say about where inside it the pointer is.
    fn cell(col: u16, row: u16) -> Pos {
        Pos {
            col,
            row,
            x: col as f32 + 0.5,
            y: row as f32 + 0.5,
        }
    }

    #[test]
    fn sgr_mouse_buttons_and_wheel() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"\x1b[<0;10;5M\x1b[<2;10;5M\x1b[<0;11;5m\x1b[<64;3;3M"),
            vec![
                Event::MouseDown {
                    button: Button::Left,
                    at: cell(9, 4),
                },
                Event::MouseDown {
                    button: Button::Right,
                    at: cell(9, 4),
                },
                Event::MouseUp {
                    button: Button::Left,
                    at: cell(10, 4),
                },
                Event::WheelUp { at: cell(2, 2) },
            ]
        );
    }

    #[test]
    fn sgr_drag_reports_the_held_button() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"\x1b[<34;7;2M\x1b[<35;7;2M"),
            vec![
                Event::MouseMove {
                    at: cell(6, 1),
                    held: Some(Button::Right),
                },
                Event::MouseMove {
                    at: cell(6, 1),
                    held: None,
                },
            ]
        );
    }

    /// In pixel mode the same framing carries pixels: the cell is
    /// derived, and the fractional position is the real one rather
    /// than the cell's centre. This is the whole point of ?1016 for a
    /// drawing tool — two clicks in one cell are two positions.
    #[test]
    fn pixel_mouse_resolves_within_the_cell() {
        let mut p = InputParser::new();
        p.set_pixel_mouse((10.0, 20.0));
        // Pixel 26,45 (1-indexed) is 2.5 cells across, 2.2 down.
        let evs = p.feed(b"\x1b[<0;26;45M");
        let Event::MouseDown { at, .. } = evs[0] else {
            panic!("expected a press, got {evs:?}");
        };
        assert_eq!((at.col, at.row), (2, 2));
        assert!((at.x - 2.5).abs() < 1e-6, "{}", at.x);
        assert!((at.y - 2.2).abs() < 1e-6, "{}", at.y);

        // A pixel further left in the same cell is a different point
        // but the same cell — the distinction a cell report cannot
        // make.
        let evs = p.feed(b"\x1b[<0;22;45M");
        let Event::MouseDown { at: near, .. } = evs[0] else {
            panic!("expected a press");
        };
        assert_eq!((near.col, near.row), (2, 2));
        assert!(near.x < at.x);
    }
}
