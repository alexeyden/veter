//! Keyboard + SGR-mouse input parsing for the event loop. Stateful so
//! escape sequences split across reads are reassembled.

use std::time::{Duration, Instant};

/// How long an unterminated escape sequence may sit in the buffer
/// before it is thrown away. A control string split across reads (a
/// protocol reply crossing a network boundary) completes in
/// milliseconds; one still open after this was never going to close —
/// `Esc` followed by `_` typed by hand — and holding it any longer
/// would swallow every keystroke queued behind it.
const SEQ_TIMEOUT: Duration = Duration::from_secs(1);

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
    /// A cursor key. Mode-dependent: the horizontal pair seeks in video
    /// mode and cycles the directory's stills in image mode.
    Arrow(Dir),
    /// `hjkl` — always a pan, in both modes, so the keyboard can still
    /// pan horizontally where `Arrow` means something else.
    Pan(Dir),
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

pub struct InputParser {
    buf: Vec<u8>,
    /// When input last arrived. An incomplete sequence is only
    /// abandoned once nothing has come in for [`SEQ_TIMEOUT`], so a
    /// reply split across reads is still reassembled.
    last_input: Instant,
}

impl Default for InputParser {
    fn default() -> Self {
        Self {
            buf: Vec::new(),
            last_input: Instant::now(),
        }
    }
}

impl InputParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed freshly-read bytes; returns the events that completed.
    /// Incomplete escape sequences are retained for the next call.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        self.last_input = Instant::now();
        self.drain(false)
    }

    /// Called on idle timeout: a lone ESC still buffered is treated as
    /// Quit (rather than the start of an unfinished sequence), and a
    /// longer sequence that has stopped arriving is discarded.
    ///
    /// Dropping it is the lesser evil. If the rest does turn up later
    /// its bytes read as keys — which is what a whole envelope used to
    /// do, and needs a second-long stall mid-envelope to happen at all
    /// — whereas holding it holds every keystroke behind it too, and a
    /// sequence that never completes would take the keyboard with it.
    pub fn flush(&mut self) -> Vec<Event> {
        let events = self.drain(true);
        if !self.buf.is_empty() && self.last_input.elapsed() > SEQ_TIMEOUT {
            self.buf.clear();
        }
        events
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
                    } else if matches!(b[i + 1], b'_' | b'P' | b']' | b'^' | b'X') {
                        // A control string — APC / DCS / OSC / PM / SOS —
                        // not a keypress. These reach us as replies the
                        // terminal sent: a VGE response envelope
                        // (`ESC _ vge … ESC \`), an OSC colour report,
                        // a DCS answer. Read as keys they are a burst of
                        // garbage that starts with a Quit, so skip the
                        // whole string and emit nothing.
                        match find_string_end(&b[i..]) {
                            Some(len) => i += len,
                            None => break, // incomplete — keep buffering
                        }
                    } else if b[i + 1] == b'\\' {
                        // A stray ST with no string open. Nothing to do
                        // with it, but it is certainly not a keypress.
                        i += 2;
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
                b'h' => {
                    out.push(Event::Pan(Dir::Left));
                    i += 1;
                }
                b'j' => {
                    out.push(Event::Pan(Dir::Down));
                    i += 1;
                }
                b'k' => {
                    out.push(Event::Pan(Dir::Up));
                    i += 1;
                }
                b'l' => {
                    out.push(Event::Pan(Dir::Right));
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

/// Length of the control string starting at `s[0] == ESC`, including
/// its terminator: `ESC \` (ST) for APC / DCS / PM / SOS, and either
/// that or BEL for OSC. `None` while the terminator is still on its
/// way.
///
/// A literal ESC inside a VGE or PRT payload arrives byte-stuffed as
/// `ESC ESC` (§1.3 of the extension specs), so the scan steps over
/// escaped pairs instead of stopping at the first ESC it meets —
/// otherwise a payload carrying `ESC ESC \` would look like the end of
/// the envelope and the rest of it would spill out as keystrokes.
fn find_string_end(s: &[u8]) -> Option<usize> {
    let osc = s[1] == b']';
    let mut i = 2;
    while i < s.len() {
        match s[i] {
            0x07 if osc => return Some(i + 1),
            0x1B => {
                if *s.get(i + 1)? == b'\\' {
                    return Some(i + 2);
                }
                i += 2; // stuffed `ESC ESC` — step over the pair
            }
            _ => i += 1,
        }
    }
    None
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
    fn parses_pan_keys() {
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(b"hjkl"),
            vec![
                Event::Pan(Dir::Left),
                Event::Pan(Dir::Down),
                Event::Pan(Dir::Up),
                Event::Pan(Dir::Right),
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

    /// Build a VGE ProbeResponse envelope — what the terminal sends
    /// back, and what a client can find on its stdin when something
    /// upstream answers a command twice.
    fn probe_response_envelope() -> Vec<u8> {
        use vge_protocol::envelope::{ProbeBody, append_frame, wrap_t2c_envelope};
        let body = ProbeBody {
            protocol_version: 1,
            cell_pixel_width: 9,
            cell_pixel_height: 20,
            scale_factor: 1.0,
            max_elements: 4096,
            max_commands_per_element: 256,
            max_text_bytes: 65536,
            max_image_bytes: 32 << 20,
            max_images: 1024,
            supported_image_encodings: 0x03,
            max_nesting_depth: 8,
        };
        let mut frames = Vec::new();
        append_frame(&mut frames, vge_protocol::frame::RSP_PROBE, 1, &body.encode());
        wrap_t2c_envelope(&frames)
    }

    #[test]
    fn stray_vge_envelope_is_not_input() {
        // The bug this guards: under `vsd` an inner client used to get
        // two replies to its probe, and the straggler landed in the
        // event loop. `ESC _` read as "an ESC-prefixed key we don't
        // handle" — Quit — and vplay exited the moment it started.
        let mut p = InputParser::new();
        assert!(p.feed(&probe_response_envelope()).is_empty());
        // The envelope is fully consumed: a keystroke behind it still
        // arrives, and arrives as itself.
        assert_eq!(p.feed(b"+"), vec![Event::ZoomIn]);
    }

    #[test]
    fn split_vge_envelope_reassembles() {
        // Split mid-payload, the way a reply crossing a network
        // boundary reaches us.
        let env = probe_response_envelope();
        let cut = env.len() / 2;
        let mut p = InputParser::new();
        assert!(p.feed(&env[..cut]).is_empty());
        assert!(p.flush().is_empty(), "a half-arrived envelope is not a key");
        assert!(p.feed(&env[cut..]).is_empty());
        assert_eq!(p.feed(b"q"), vec![Event::Quit]);
    }

    #[test]
    fn osc_reply_is_not_input() {
        // OSC ends at BEL as well as at ST. `10;rgb:…` would otherwise
        // read as Fit, StepPrev, a pan and a zoom.
        let mut p = InputParser::new();
        assert!(p.feed(b"\x1b]10;rgb:1e1e/1e1e/1e1e\x07").is_empty());
        assert_eq!(p.feed(b"0"), vec![Event::Fit]);
    }

    #[test]
    fn stuffed_esc_does_not_end_the_envelope_early() {
        // §1.3 byte-stuffing: a literal ESC in a payload arrives as
        // `ESC ESC`. Scanning naively for `ESC \\` would cut the
        // envelope short here and spill `q` as a keypress.
        let mut p = InputParser::new();
        assert!(p.feed(b"\x1b_vge\x1b\x1b\\q\x1b\\").is_empty());
        assert_eq!(p.feed(b"."), vec![Event::StepNext]);
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
