//! Keyboard + SGR-mouse input parsing for the event loop. Stateful so
//! escape sequences split across reads are reassembled.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Quit,
    ZoomIn,
    ZoomOut,
    Fit,
    Actual,
    Arrow(Dir),
    StepNext,
    StepPrev,
    /// Left-button press at a 0-indexed cell.
    MouseDown {
        col: u16,
        row: u16,
    },
    MouseUp {
        col: u16,
        row: u16,
    },
    /// Pointer motion; `pressed` is true while a button is held (drag).
    MouseMove {
        col: u16,
        row: u16,
        pressed: bool,
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

    /// Called on idle timeout: a lone ESC still buffered is treated as
    /// Quit (rather than the start of an unfinished sequence).
    pub fn flush(&mut self) -> Vec<Event> {
        self.drain(true)
    }

    fn drain(&mut self, eof: bool) -> Vec<Event> {
        let mut out = Vec::new();
        let mut i = 0;
        let b = std::mem::take(&mut self.buf);
        while i < b.len() {
            let c = b[i];
            match c {
                0x1B => {
                    // Need at least one more byte to know the kind.
                    if i + 1 >= b.len() {
                        if eof {
                            out.push(Event::Quit);
                            i += 1;
                        } else {
                            break; // keep the lone ESC buffered
                        }
                        continue;
                    }
                    if b[i + 1] == b'[' {
                        if i + 2 < b.len() && b[i + 2] == b'<' {
                            // SGR mouse: ESC [ < ... (M|m)
                            match find_mouse_end(&b[i..]) {
                                Some(len) => {
                                    if let Some(ev) = parse_sgr_mouse(&b[i..i + len]) {
                                        out.push(ev);
                                    }
                                    i += len;
                                }
                                None => break, // incomplete
                            }
                        } else {
                            // CSI: ESC [ ... <final 0x40..=0x7E>
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
                        // Some other ESC-prefixed key we don't handle.
                        out.push(Event::Quit);
                        i += 2;
                    }
                }
                b'q' | 0x03 => {
                    out.push(Event::Quit);
                    i += 1;
                }
                b'+' | b'=' => {
                    out.push(Event::ZoomIn);
                    i += 1;
                }
                b'-' | b'_' => {
                    out.push(Event::ZoomOut);
                    i += 1;
                }
                b'0' => {
                    out.push(Event::Fit);
                    i += 1;
                }
                b'1' => {
                    out.push(Event::Actual);
                    i += 1;
                }
                b'.' => {
                    out.push(Event::StepNext);
                    i += 1;
                }
                b',' => {
                    out.push(Event::StepPrev);
                    i += 1;
                }
                _ => {
                    i += 1; // ignore other bytes
                }
            }
        }
        // Retain anything we couldn't consume yet.
        if i < b.len() {
            self.buf.extend_from_slice(&b[i..]);
        }
        out
    }
}

fn arrow_from_final(f: u8) -> Option<Event> {
    match f {
        b'A' => Some(Event::Arrow(Dir::Up)),
        b'B' => Some(Event::Arrow(Dir::Down)),
        b'C' => Some(Event::Arrow(Dir::Right)),
        b'D' => Some(Event::Arrow(Dir::Left)),
        _ => None,
    }
}

/// Length of a CSI sequence starting at `s[0] == ESC`, including the
/// final byte (0x40..=0x7E). `None` if not yet complete.
fn find_csi_end(s: &[u8]) -> Option<usize> {
    // s[0]=ESC, s[1]='['
    let mut j = 2;
    while j < s.len() {
        let c = s[j];
        if (0x40..=0x7E).contains(&c) {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

fn parse_csi(s: &[u8]) -> Option<Event> {
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
    let body = &s[3..s.len() - 1];
    let text = std::str::from_utf8(body).ok()?;
    let mut parts = text.split(';');
    let b: u32 = parts.next()?.parse().ok()?;
    let col: u32 = parts.next()?.parse().ok()?;
    let row: u32 = parts.next()?.parse().ok()?;
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
    let motion = b & 32 != 0;
    let button = b & 0b11;
    if motion {
        // Held-button drag (button bits carry the held button; 3 = none).
        return Some(Event::MouseMove {
            col,
            row,
            pressed: button != 3,
        });
    }
    match final_byte {
        b'M' if button == 0 => Some(Event::MouseDown { col, row }),
        b'm' => Some(Event::MouseUp { col, row }),
        // Middle/right press: treat as nothing actionable but report the
        // position so the cursor readout still moves.
        b'M' => Some(Event::MouseMove {
            col,
            row,
            pressed: false,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys() {
        let mut p = InputParser::new();
        let evs = p.feed(b"+-01.,q");
        assert_eq!(
            evs,
            vec![
                Event::ZoomIn,
                Event::ZoomOut,
                Event::Fit,
                Event::Actual,
                Event::StepNext,
                Event::StepPrev,
                Event::Quit,
            ]
        );
    }

    #[test]
    fn parses_arrows() {
        let mut p = InputParser::new();
        let evs = p.feed(b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert_eq!(
            evs,
            vec![
                Event::Arrow(Dir::Up),
                Event::Arrow(Dir::Down),
                Event::Arrow(Dir::Right),
                Event::Arrow(Dir::Left),
            ]
        );
    }

    #[test]
    fn split_arrow_reassembles() {
        let mut p = InputParser::new();
        assert!(p.feed(b"\x1b[").is_empty());
        assert_eq!(p.feed(b"C"), vec![Event::Arrow(Dir::Right)]);
    }

    #[test]
    fn lone_esc_flushes_to_quit() {
        let mut p = InputParser::new();
        assert!(p.feed(b"\x1b").is_empty());
        assert_eq!(p.flush(), vec![Event::Quit]);
    }

    #[test]
    fn sgr_mouse_press_and_wheel() {
        let mut p = InputParser::new();
        let evs = p.feed(b"\x1b[<0;10;5M\x1b[<64;3;3M");
        assert_eq!(
            evs,
            vec![
                Event::MouseDown { col: 9, row: 4 },
                Event::WheelUp { col: 2, row: 2 },
            ]
        );
    }
}
