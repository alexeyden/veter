// Streaming APC envelope extractor (§1.1–1.3 of the extension specs).
//
// Splits the PTY byte stream into:
//   * `passthrough`: bytes destined for the next layer.
//   * `payloads`:    one Vec<u8> per fully-received VSS APC envelope,
//                    with byte-stuffing already reversed.
//
// VSS carries no on-screen state, so unlike PRT and VGE this parser
// does not observe any control sequences (RIS, DECSTR, 2J/3J, DSR).
// Bytes that are not part of a `VSS` (or `vss`) APC envelope pass
// through verbatim — including foreign APC sequences. This matches
// the foreign-marker pass-through rule in §1.1 of every other spec.

use super::frame::{APC_OPEN, ESC, MARKER_E2R, ST_CLOSE};

#[derive(Debug)]
enum State {
    /// Normal pass-through stream.
    Idle,
    /// Saw 0x1B in Idle; deciding whether it opens APC.
    EscPending,
    /// Inside `ESC _ ...`, buffering the 3 marker bytes to decide
    /// VSS vs. some other APC.
    ApcPrefix { marker_buf: Vec<u8> },
    /// Confirmed non-VSS APC — flush everything (including ESC _ and
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed VSS — buffer (un-stuffed) bytes until `ESC \`.
    ApcVss { body: Vec<u8> },
    /// Saw 0x1B inside `ApcVss`; the next byte decides escape
    /// (`0x1B`) vs ST close (`0x5C`).
    ApcVssEsc { body: Vec<u8> },
    /// Saw 0x1B inside `ApcOther`; the next byte decides whether ST
    /// closes the envelope.
    ApcOtherEsc,
}

pub struct ApcStream {
    state: State,
    /// Which 3-byte APC marker to extract. Defaults to the engine
    /// side (`VSS` uppercase). Use `with_marker(MARKER_R2E)` on the
    /// engine to extract the renderer's lowercase `vss` upstream
    /// frames.
    marker: [u8; 3],
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to the next layer verbatim.
    pub passthrough: Vec<u8>,
    /// Fully-received, un-stuffed VSS payloads (one per envelope).
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
    /// Extract engine-to-renderer envelopes (uppercase `VSS`). This is
    /// what the renderer-side VssEngine uses.
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            marker: *MARKER_E2R,
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

    /// Drain a deferred lone ESC (state `EscPending`) and return it as
    /// a single-byte `Vec`. Other states — mid-envelope, etc. — are
    /// left alone because their bodies must arrive in full.
    pub fn flush_pending_esc(&mut self) -> Vec<u8> {
        if matches!(self.state, State::EscPending) {
            self.state = State::Idle;
            vec![ESC]
        } else {
            Vec::new()
        }
    }

    fn step(&mut self, b: u8, out: &mut Output) {
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
                ESC => {
                    out.push_pass(ESC);
                    State::EscPending
                }
                _ => {
                    out.push_pass(ESC);
                    out.push_pass(b);
                    State::Idle
                }
            },
            State::ApcPrefix { mut marker_buf } => {
                marker_buf.push(b);
                if marker_buf.len() < 3 {
                    State::ApcPrefix { marker_buf }
                } else if marker_buf.as_slice() == self.marker {
                    State::ApcVss { body: Vec::new() }
                } else {
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
                out.push_pass(ESC);
                out.push_pass(b);
                if b == ST_CLOSE {
                    State::Idle
                } else {
                    State::ApcOther
                }
            }
            State::ApcVss { mut body } => {
                if b == ESC {
                    State::ApcVssEsc { body }
                } else {
                    body.push(b);
                    State::ApcVss { body }
                }
            }
            State::ApcVssEsc { mut body } => match b {
                ESC => {
                    body.push(ESC);
                    State::ApcVss { body }
                }
                ST_CLOSE => {
                    out.payloads.push(body);
                    State::Idle
                }
                _ => {
                    // Spec only permits 1B-stuffing or ST close inside the
                    // envelope. Treat anything else as malformed: discard
                    // the partial body, emit the stray ESC + byte to
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
    use super::super::frame::MARKER_R2E;
    use super::*;

    fn envelope_e2r(body: &[u8]) -> Vec<u8> {
        let mut v = vec![ESC, APC_OPEN, b'V', b'S', b'S'];
        super::super::codec::stuff(body, &mut v);
        v.push(ESC);
        v.push(ST_CLOSE);
        v
    }

    #[test]
    fn extracts_single_envelope() {
        let mut s = ApcStream::new();
        let body = b"hello";
        let out = s.feed(&envelope_e2r(body));
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], body);
    }

    #[test]
    fn unstuffs_esc_byte() {
        let mut s = ApcStream::new();
        let body = &[0x00, 0x1B, 0xFF, 0x1B];
        let out = s.feed(&envelope_e2r(body));
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
        let env = envelope_e2r(b"abcdef");
        for split in 1..env.len() {
            let mut s = ApcStream::new();
            let mut out = Output::default();
            for chunk in &[&env[..split], &env[split..]] {
                let o = s.feed(chunk);
                out.passthrough.extend(o.passthrough);
                out.payloads.extend(o.payloads);
            }
            assert!(
                out.passthrough.is_empty(),
                "split {split}: leaked {:?}",
                out.passthrough
            );
            assert_eq!(out.payloads.len(), 1, "split {split}: missing payload");
            assert_eq!(&out.payloads[0], b"abcdef", "split {split}");
        }
    }

    #[test]
    fn prt_envelope_passes_through() {
        let mut s = ApcStream::new();
        let env = vec![
            ESC, APC_OPEN, b'P', b'R', b'T', b'a', b'b', b'c', ESC, ST_CLOSE,
        ];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn vge_envelope_passes_through() {
        let mut s = ApcStream::new();
        let env = vec![
            ESC, APC_OPEN, b'V', b'G', b'E', b'a', b'b', b'c', ESC, ST_CLOSE,
        ];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn vft_envelope_passes_through() {
        let mut s = ApcStream::new();
        let env = vec![
            ESC, APC_OPEN, b'V', b'F', b'T', b'a', b'b', b'c', ESC, ST_CLOSE,
        ];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn kitty_graphics_apc_passes_through() {
        let mut s = ApcStream::new();
        let env = vec![ESC, APC_OPEN, b'G', b'a', b'b', b'c', ESC, ST_CLOSE];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn esc_before_normal_byte_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC, b'A']);
        assert_eq!(out.passthrough, vec![ESC, b'A']);
    }

    #[test]
    fn csi_sequence_passes_through_unchanged() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[2J\x1b[H");
        assert_eq!(out.passthrough, b"\x1b[2J\x1b[H");
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn flush_pending_esc_emits_deferred_lone_esc() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC]);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert!(s.flush_pending_esc().is_empty());
        let out = s.feed(&envelope_e2r(b"x"));
        assert_eq!(out.payloads, vec![b"x".to_vec()]);
    }

    #[test]
    fn flush_pending_esc_leaves_mid_envelope_alone() {
        let mut s = ApcStream::new();
        let env = envelope_e2r(b"abc");
        let out = s.feed(&env[..env.len() - 1]);
        assert!(out.payloads.is_empty());
        assert!(s.flush_pending_esc().is_empty());
        let out = s.feed(&env[env.len() - 1..]);
        assert_eq!(out.payloads, vec![b"abc".to_vec()]);
    }

    #[test]
    fn back_to_back_envelopes() {
        let mut s = ApcStream::new();
        let mut buf = envelope_e2r(b"one");
        buf.extend(envelope_e2r(b"two"));
        let out = s.feed(&buf);
        assert_eq!(out.payloads.len(), 2);
        assert_eq!(&out.payloads[0], b"one");
        assert_eq!(&out.payloads[1], b"two");
    }

    #[test]
    fn r2e_marker_extracts_lowercase_envelopes() {
        // An engine-side stream uses MARKER_R2E to pick up `vss`
        // renderer responses; uppercase `VSS` envelopes pass through.
        let mut s = ApcStream::with_marker(*MARKER_R2E);
        let mut r2e = vec![ESC, APC_OPEN, b'v', b's', b's'];
        super::super::codec::stuff(b"resp", &mut r2e);
        r2e.push(ESC);
        r2e.push(ST_CLOSE);

        let e2r = envelope_e2r(b"cmd");
        let mut all = r2e.clone();
        all.extend_from_slice(&e2r);

        let out = s.feed(&all);
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], b"resp");
        assert_eq!(out.passthrough, e2r);
    }

    #[test]
    fn malformed_envelope_resyncs() {
        let mut s = ApcStream::new();
        let mut env = vec![ESC, APC_OPEN, b'V', b'S', b'S', b'b', b'a', b'd'];
        env.push(ESC);
        env.push(b'X');
        env.extend_from_slice(b"after");

        let out = s.feed(&env);
        assert!(out.payloads.is_empty());
        assert_eq!(out.passthrough, b"\x1bXafter");
    }
}
