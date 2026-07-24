//! Keyboard + SGR-mouse input parsing, stateful so escape sequences
//! split across reads are reassembled.
//!
//! Derived from the parsers `vplay` and `vdraw` grew independently, but
//! kept deliberately low-level: keys surface as [`Event::Key`] /
//! [`Event::Ctrl`] rather than as app actions, so the consuming binary
//! owns its own keymap. Mouse events carry which button is involved,
//! since a right-drag usually means something different from a left one.
//!
//! Enable the reports this expects with the usual pair — `?1002` for
//! button-event tracking, `?1006` for SGR encoding:
//!
//! ```text
//! ESC [ ? 1002 h    ESC [ ? 1006 h
//! ```

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// A printable character.
    Key(char),
    /// A control chord, normalised to its lowercase letter — Ctrl+A is
    /// `Ctrl('a')`. Enter, Tab, Backspace and Esc are reported as their
    /// own variants and never appear here.
    Ctrl(char),
    /// Alt/Meta + a printable character (`ESC` followed by the key).
    Alt(char),
    Arrow(Dir),
    Nav(Nav),
    Enter,
    Tab,
    BackTab,
    Escape,
    Backspace,
    Delete,
    /// Function key 1..=12.
    Function(u8),
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

    /// Called on an idle timeout: a lone ESC still buffered is a real
    /// Esc press rather than the start of an unfinished sequence.
    pub fn flush(&mut self) -> Vec<Event> {
        self.drain(true)
    }

    fn drain(&mut self, idle: bool) -> Vec<Event> {
        let mut out = Vec::new();
        let mut i = 0;
        let b = std::mem::take(&mut self.buf);
        while i < b.len() {
            match b[i] {
                0x1B => {
                    let Some(next) = b.get(i + 1).copied() else {
                        if idle {
                            out.push(Event::Escape);
                            i += 1;
                        } else {
                            break; // keep the lone ESC buffered
                        }
                        continue;
                    };
                    match next {
                        b'[' | b'O' => {
                            let Some(len) = find_seq_end(&b[i..]) else {
                                break; // incomplete — wait for more
                            };
                            if let Some(ev) = parse_seq(&b[i..i + len]) {
                                out.push(ev);
                            }
                            i += len;
                        }
                        0x7F | 0x08 => {
                            out.push(Event::Alt('\x7f'));
                            i += 2;
                        }
                        c if (0x20..0x7F).contains(&c) => {
                            out.push(Event::Alt(c as char));
                            i += 2;
                        }
                        _ => {
                            // ESC followed by something we don't model:
                            // report the Esc and re-read the rest.
                            out.push(Event::Escape);
                            i += 1;
                        }
                    }
                }
                b'\r' | b'\n' => {
                    out.push(Event::Enter);
                    i += 1;
                }
                b'\t' => {
                    out.push(Event::Tab);
                    i += 1;
                }
                0x7F | 0x08 => {
                    out.push(Event::Backspace);
                    i += 1;
                }
                c @ 0x01..=0x1A => {
                    out.push(Event::Ctrl((b'a' + c - 1) as char));
                    i += 1;
                }
                c @ 0x20..=0x7E => {
                    out.push(Event::Key(c as char));
                    i += 1;
                }
                c if c >= 0x80 => {
                    // UTF-8 continuation: decode the whole scalar.
                    let len = utf8_len(c);
                    if i + len > b.len() {
                        break; // incomplete multi-byte char
                    }
                    if let Ok(s) = std::str::from_utf8(&b[i..i + len]) {
                        if let Some(ch) = s.chars().next() {
                            out.push(Event::Key(ch));
                        }
                    }
                    i += len;
                }
                _ => i += 1, // remaining C0 controls we don't model
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

/// Length of the CSI/SS3 sequence starting at `s[0] == ESC`, or `None`
/// if it is not yet complete. SGR mouse reports end in `M`/`m`; other
/// sequences end at the first byte in `0x40..=0x7E` after the
/// introducer.
fn find_seq_end(s: &[u8]) -> Option<usize> {
    if s.len() < 3 {
        return None;
    }
    let start = 2; // past ESC and '[' / 'O'
    for (k, &c) in s.iter().enumerate().skip(start) {
        if (0x40..=0x7E).contains(&c) {
            return Some(k + 1);
        }
    }
    None
}

/// Parse a complete CSI/SS3 sequence into an event.
fn parse_seq(s: &[u8]) -> Option<Event> {
    let final_byte = *s.last()?;
    let params = &s[2..s.len() - 1];
    if params.first() == Some(&b'<') {
        return parse_sgr_mouse(&params[1..], final_byte);
    }
    // Strip a modifier suffix (`1;5A` — Ctrl+Up) down to the base param.
    let base: &[u8] = match params.iter().position(|&c| c == b';') {
        Some(p) => &params[..p],
        None => params,
    };
    Some(match (final_byte, base) {
        (b'A', _) => Event::Arrow(Dir::Up),
        (b'B', _) => Event::Arrow(Dir::Down),
        (b'C', _) => Event::Arrow(Dir::Right),
        (b'D', _) => Event::Arrow(Dir::Left),
        (b'H', _) => Event::Nav(Nav::Home),
        (b'F', _) => Event::Nav(Nav::End),
        (b'Z', _) => Event::BackTab,
        (b'P', b"") => Event::Function(1), // SS3 P..S
        (b'Q', b"") => Event::Function(2),
        (b'R', b"") => Event::Function(3),
        (b'S', b"") => Event::Function(4),
        (b'~', p) => match p {
            b"1" | b"7" => Event::Nav(Nav::Home),
            b"2" => Event::Nav(Nav::Insert),
            b"3" => Event::Delete,
            b"4" | b"8" => Event::Nav(Nav::End),
            b"5" => Event::Nav(Nav::PageUp),
            b"6" => Event::Nav(Nav::PageDown),
            b"11" => Event::Function(1),
            b"12" => Event::Function(2),
            b"13" => Event::Function(3),
            b"14" => Event::Function(4),
            b"15" => Event::Function(5),
            b"17" => Event::Function(6),
            b"18" => Event::Function(7),
            b"19" => Event::Function(8),
            b"20" => Event::Function(9),
            b"21" => Event::Function(10),
            b"23" => Event::Function(11),
            b"24" => Event::Function(12),
            _ => return None,
        },
        _ => return None,
    })
}

/// Parse the body of an SGR mouse report (`btn;col;row` plus a final
/// `M` for press / `m` for release).
fn parse_sgr_mouse(body: &[u8], final_byte: u8) -> Option<Event> {
    let text = std::str::from_utf8(body).ok()?;
    let mut it = text.split(';');
    let bits: u32 = it.next()?.parse().ok()?;
    let col: u16 = it.next()?.parse().ok()?;
    let row: u16 = it.next()?.parse().ok()?;
    // SGR reports are 1-based; every consumer wants 0-based cells.
    let col = col.saturating_sub(1);
    let row = row.saturating_sub(1);
    if bits & 0x40 != 0 {
        return Some(if bits & 0b1 == 0 {
            Event::WheelUp { col, row }
        } else {
            Event::WheelDown { col, row }
        });
    }
    if bits & 0x20 != 0 {
        return Some(Event::MouseMove {
            col,
            row,
            held: Button::from_bits(bits),
        });
    }
    let button = Button::from_bits(bits)?;
    Some(if final_byte == b'm' {
        Event::MouseUp { button, col, row }
    } else {
        Event::MouseDown { button, col, row }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(bytes: &[u8]) -> Vec<Event> {
        let mut p = InputParser::new();
        let mut v = p.feed(bytes);
        v.extend(p.flush());
        v
    }

    #[test]
    fn printable_and_control_keys() {
        assert_eq!(events(b"ab"), vec![Event::Key('a'), Event::Key('b')]);
        assert_eq!(events(b"\x03"), vec![Event::Ctrl('c')]);
        assert_eq!(events(b"\r"), vec![Event::Enter]);
        assert_eq!(events(b"\t"), vec![Event::Tab]);
        assert_eq!(events(b"\x7f"), vec![Event::Backspace]);
        assert_eq!(events(b" "), vec![Event::Key(' ')]);
    }

    #[test]
    fn arrows_navigation_and_function_keys() {
        assert_eq!(events(b"\x1b[A"), vec![Event::Arrow(Dir::Up)]);
        assert_eq!(events(b"\x1bOB"), vec![Event::Arrow(Dir::Down)]);
        assert_eq!(events(b"\x1b[5~"), vec![Event::Nav(Nav::PageUp)]);
        assert_eq!(events(b"\x1b[3~"), vec![Event::Delete]);
        assert_eq!(events(b"\x1b[Z"), vec![Event::BackTab]);
        assert_eq!(events(b"\x1b[15~"), vec![Event::Function(5)]);
        // Modified arrows still report their direction.
        assert_eq!(events(b"\x1b[1;5A"), vec![Event::Arrow(Dir::Up)]);
    }

    #[test]
    fn alt_and_bare_escape() {
        assert_eq!(events(b"\x1bf"), vec![Event::Alt('f')]);
        assert_eq!(events(b"\x1b"), vec![Event::Escape]);
    }

    #[test]
    fn split_sequences_are_reassembled() {
        let mut p = InputParser::new();
        assert!(p.feed(b"\x1b[").is_empty());
        assert!(p.feed(b"1;5").is_empty());
        assert_eq!(p.feed(b"C"), vec![Event::Arrow(Dir::Right)]);
    }

    #[test]
    fn a_lone_esc_waits_for_more_bytes_before_committing() {
        let mut p = InputParser::new();
        assert!(p.feed(b"\x1b").is_empty(), "might be a sequence");
        assert_eq!(p.flush(), vec![Event::Escape], "idle → a real Esc");
    }

    #[test]
    fn sgr_mouse_is_zero_based_and_carries_the_button() {
        assert_eq!(
            events(b"\x1b[<0;10;5M"),
            vec![Event::MouseDown {
                button: Button::Left,
                col: 9,
                row: 4
            }]
        );
        assert_eq!(
            events(b"\x1b[<2;1;1m"),
            vec![Event::MouseUp {
                button: Button::Right,
                col: 0,
                row: 0
            }]
        );
        assert_eq!(events(b"\x1b[<64;3;3M"), vec![Event::WheelUp { col: 2, row: 2 }]);
        assert_eq!(
            events(b"\x1b[<65;3;3M"),
            vec![Event::WheelDown { col: 2, row: 2 }]
        );
    }

    #[test]
    fn drag_reports_the_held_button_and_hover_reports_none() {
        assert_eq!(
            events(b"\x1b[<32;4;4M"),
            vec![Event::MouseMove {
                col: 3,
                row: 3,
                held: Some(Button::Left)
            }]
        );
        assert_eq!(
            events(b"\x1b[<35;4;4M"),
            vec![Event::MouseMove {
                col: 3,
                row: 3,
                held: None
            }]
        );
    }

    #[test]
    fn multibyte_characters_survive_a_split_read() {
        let mut p = InputParser::new();
        let ch = "é".as_bytes();
        assert!(p.feed(&ch[..1]).is_empty());
        assert_eq!(p.feed(&ch[1..]), vec![Event::Key('é')]);
    }
}
