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
    MouseDown {
        button: Button,
        col: u16,
        row: u16,
    },
    MouseUp {
        button: Button,
        col: u16,
        row: u16,
    },
    /// Pointer motion. `held` is the dragged button, if any.
    MouseMove {
        col: u16,
        row: u16,
        held: Option<Button>,
    },
    WheelUp {
        col: u16,
        row: u16,
    },
    WheelDown {
        col: u16,
        row: u16,
    },
}

#[derive(Default)]
pub struct InputParser {
    buf: Vec<u8>,
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
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
                                    if let Some(ev) = parse_sgr_mouse(&b[i..i + len]) {
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

fn parse_sgr_mouse(s: &[u8]) -> Option<Event> {
    // s = ESC [ < b ; col ; row (M|m)
    let final_byte = *s.last()?;
    let text = std::str::from_utf8(&s[3..s.len() - 1]).ok()?;
    let mut parts = text.split(';');
    let b: u32 = parts.next()?.parse().ok()?;
    let col: u32 = parts.next()?.parse().ok()?;
    let row: u32 = parts.next()?.parse().ok()?;
    // SGR is 1-indexed; the rest of the editor works in 0-indexed cells.
    let col = col.saturating_sub(1) as u16;
    let row = row.saturating_sub(1) as u16;

    if b & 64 != 0 {
        // Wheel: 64 = up, 65 = down.
        return Some(if b & 1 == 0 {
            Event::WheelUp { col, row }
        } else {
            Event::WheelDown { col, row }
        });
    }
    if b & 32 != 0 {
        // Motion; the button bits carry the held button (3 = none).
        return Some(Event::MouseMove {
            col,
            row,
            held: Button::from_bits(b),
        });
    }
    let button = Button::from_bits(b)?;
    Some(match final_byte {
        b'M' => Event::MouseDown { button, col, row },
        _ => Event::MouseUp { button, col, row },
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
    fn other_tilde_sequences_are_not_delete() {
        let mut p = InputParser::new();
        // Home (1~), Insert (2~), PgUp (5~) must not delete the selection.
        assert!(p.feed(b"\x1b[1~\x1b[2~\x1b[5~").is_empty());
    }

    #[test]
    fn sgr_mouse_buttons_and_wheel() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"\x1b[<0;10;5M\x1b[<2;10;5M\x1b[<0;11;5m\x1b[<64;3;3M"),
            vec![
                Event::MouseDown {
                    button: Button::Left,
                    col: 9,
                    row: 4
                },
                Event::MouseDown {
                    button: Button::Right,
                    col: 9,
                    row: 4
                },
                Event::MouseUp {
                    button: Button::Left,
                    col: 10,
                    row: 4
                },
                Event::WheelUp { col: 2, row: 2 },
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
                    col: 6,
                    row: 1,
                    held: Some(Button::Right)
                },
                Event::MouseMove {
                    col: 6,
                    row: 1,
                    held: None
                },
            ]
        );
    }
}
