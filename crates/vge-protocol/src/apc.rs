// Streaming APC envelope extractor (§1.1–1.3).
//
// Splits the PTY byte stream into:
//   * `passthrough`: bytes destined for the regular VT parser.
//   * `payloads`:    one Vec<u8> per fully-received VGE APC envelope, with
//                    byte-stuffing already reversed.
//
// Non-VGE APC sequences (e.g. iTerm-style `ESC _G...`) pass through verbatim
// so the underlying VT parser can still handle them. A VGE envelope is
// recognized by the 3-byte uppercase `VGE` marker that follows `ESC _`
// (§1.1: lowercase `vge` is the terminal-to-client direction we never
// receive, so we never match it here).

use super::frame::{APC_OPEN, ESC, MARKER_C2T, ST_CLOSE};

#[derive(Debug)]
enum State {
    /// Normal pass-through stream.
    Idle,
    /// Saw 0x1B in Idle; deciding whether it opens APC.
    EscPending,
    /// Inside `ESC _ ...`, still buffering the 3 marker bytes to decide
    /// VGE vs. other APC. `marker_buf` accumulates them.
    ApcPrefix { marker_buf: Vec<u8> },
    /// Confirmed non-VGE APC — flush everything (including ESC _ and any
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed VGE — buffer (un-stuffed) bytes until `ESC \`.
    ApcVge { body: Vec<u8> },
    /// Saw 0x1B inside `ApcVge`; the next byte decides escape (`1B`) vs ST
    /// close (`5C`).
    ApcVgeEsc { body: Vec<u8> },
    /// Saw 0x1B inside `ApcOther`; the next byte decides whether ST closes
    /// the envelope.
    ApcOtherEsc,
}

pub struct ApcStream {
    state: State,
    /// Which 3-byte APC marker to extract. Defaults to the C2T marker
    /// (`VGE` uppercase) used for client→terminal commands. Use
    /// `with_marker(MARKER_T2C)` on the client side to extract the
    /// terminal's lowercase-`vge` responses.
    marker: [u8; 3],
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to vt100 verbatim.
    pub passthrough: Vec<u8>,
    /// Fully-received, un-stuffed VGE payloads (one per envelope).
    pub payloads: Vec<Vec<u8>>,
}

impl Output {
    fn push_pass(&mut self, b: u8) {
        self.passthrough.push(b);
    }
}

impl Default for ApcStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ApcStream {
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            marker: *MARKER_C2T,
        }
    }

    pub fn with_marker(marker: [u8; 3]) -> Self {
        Self {
            state: State::Idle,
            marker,
        }
    }

    pub fn feed(&mut self, input: &[u8]) -> Output {
        let mut out = Output::default();
        for &b in input {
            self.step(b, &mut out);
        }
        out
    }

    fn step(&mut self, b: u8, out: &mut Output) {
        // Move out the current state so we can rebuild it without fighting
        // the borrow checker on the `body: Vec<u8>` ownership.
        let st = std::mem::replace(&mut self.state, State::Idle);
        self.state = match st {
            State::Idle => {
                if b == ESC {
                    State::EscPending
                } else {
                    out.push_pass(b);
                    State::Idle
                }
            }
            State::EscPending => match b {
                APC_OPEN => State::ApcPrefix {
                    marker_buf: Vec::with_capacity(3),
                },
                _ => {
                    // Not APC — emit the deferred ESC + this byte and return
                    // to Idle. (Other ESC-led sequences are vt100's problem.)
                    out.push_pass(ESC);
                    if b == ESC {
                        // Two ESCs in a row: hold the second as pending again.
                        State::EscPending
                    } else {
                        out.push_pass(b);
                        State::Idle
                    }
                }
            },
            State::ApcPrefix { mut marker_buf } => {
                marker_buf.push(b);
                if marker_buf.len() < 3 {
                    State::ApcPrefix { marker_buf }
                } else if marker_buf.as_slice() == self.marker {
                    State::ApcVge { body: Vec::new() }
                } else {
                    // Not a VGE envelope — flush ESC _ <marker_buf> to
                    // passthrough and continue treating the rest as
                    // verbatim until ST.
                    out.push_pass(ESC);
                    out.push_pass(APC_OPEN);
                    for &mb in &marker_buf {
                        out.push_pass(mb);
                    }
                    State::ApcOther
                }
            }
            State::ApcOther => {
                if b == ESC {
                    State::ApcOtherEsc
                } else {
                    out.push_pass(b);
                    State::ApcOther
                }
            }
            State::ApcOtherEsc => {
                // Whether or not it terminates APC, we still pass both
                // bytes through to vt100.
                out.push_pass(ESC);
                out.push_pass(b);
                if b == ST_CLOSE {
                    State::Idle
                } else {
                    State::ApcOther
                }
            }
            State::ApcVge { mut body } => {
                if b == ESC {
                    State::ApcVgeEsc { body }
                } else {
                    body.push(b);
                    State::ApcVge { body }
                }
            }
            State::ApcVgeEsc { mut body } => match b {
                ESC => {
                    // Stuffed 0x1B — store one literal ESC.
                    body.push(ESC);
                    State::ApcVge { body }
                }
                ST_CLOSE => {
                    // Envelope complete.
                    out.payloads.push(body);
                    State::Idle
                }
                _ => {
                    // Spec only permits 1B-stuffing or ST close inside the
                    // envelope. Treat anything else as a malformed envelope:
                    // discard the partial body, emit the stray ESC + byte to
                    // passthrough, and resync.
                    out.push_pass(ESC);
                    out.push_pass(b);
                    State::Idle
                }
            },
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(body: &[u8]) -> Vec<u8> {
        let mut v = vec![ESC, APC_OPEN, b'V', b'G', b'E'];
        super::super::codec::stuff(body, &mut v);
        v.push(ESC);
        v.push(ST_CLOSE);
        v
    }

    #[test]
    fn extracts_single_envelope() {
        let mut s = ApcStream::new();
        let body = b"hello";
        let out = s.feed(&envelope(body));
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], body);
    }

    #[test]
    fn unstuffs_esc_byte() {
        let mut s = ApcStream::new();
        let body = &[0x00, 0x1B, 0xFF, 0x1B];
        let out = s.feed(&envelope(body));
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], body);
    }

    #[test]
    fn passes_through_plain_text() {
        let mut s = ApcStream::new();
        let out = s.feed(b"hello world");
        assert_eq!(out.passthrough, b"hello world");
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn split_across_chunks() {
        let env = envelope(b"abcdef");
        for split in 1..env.len() {
            let mut s = ApcStream::new();
            let mut out = Output::default();
            for chunk in &[&env[..split], &env[split..]] {
                let o = s.feed(chunk);
                out.passthrough.extend(o.passthrough);
                out.payloads.extend(o.payloads);
            }
            assert!(out.passthrough.is_empty(), "split {split}: leaked {:?}", out.passthrough);
            assert_eq!(out.payloads.len(), 1, "split {split}: missing payload");
            assert_eq!(&out.payloads[0], b"abcdef", "split {split}");
        }
    }

    #[test]
    fn non_vge_apc_passes_through() {
        // ESC _ G abc ESC \ (kitty graphics-style envelope)
        let mut s = ApcStream::new();
        let mut buf = vec![ESC, APC_OPEN, b'G', b'a', b'b', b'c', ESC, ST_CLOSE];
        let out = s.feed(&buf);
        // Should appear unchanged in passthrough.
        buf.truncate(buf.len()); // no-op, just reuse
        assert_eq!(out.passthrough, vec![ESC, APC_OPEN, b'G', b'a', b'b', b'c', ESC, ST_CLOSE]);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn esc_before_normal_byte_passes_through() {
        let mut s = ApcStream::new();
        // ESC followed by regular char that isn't '_' is just an ESC pair.
        let out = s.feed(&[ESC, b'A']);
        assert_eq!(out.passthrough, vec![ESC, b'A']);
    }

    #[test]
    fn back_to_back_envelopes() {
        let mut s = ApcStream::new();
        let mut buf = envelope(b"one");
        buf.extend(envelope(b"two"));
        let out = s.feed(&buf);
        assert_eq!(out.payloads.len(), 2);
        assert_eq!(&out.payloads[0], b"one");
        assert_eq!(&out.payloads[1], b"two");
    }
}
