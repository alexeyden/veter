// Streaming APC envelope extractor (§1.1–1.3 of the extension specs).
//
// Splits the PTY byte stream into:
//   * `passthrough`: bytes destined for the next layer.
//   * `payloads`:    one Vec<u8> per fully-received SES APC envelope,
//                    with byte-stuffing already reversed.
//
// SES carries no on-screen state, so unlike PRT and VGE this parser
// does not observe any control sequences (RIS, DECSTR, 2J/3J, DSR).
// Bytes that are not part of a `SES` (or `ses`) APC envelope pass
// through verbatim — including foreign APC sequences. This matches
// the foreign-marker pass-through rule in §1.1 of every other spec.

use super::frame::{
    APC_OPEN, ESC, ESC_MARK_TILDE, ESC_MARK_XON, ESC_MARK_XOFF, MARKER_C2H, ST_CLOSE, TILDE, XOFF,
    XON,
};

#[derive(Debug)]
enum State {
    /// Normal pass-through stream.
    Idle,
    /// Saw 0x1B in Idle; deciding whether it opens APC.
    EscPending,
    /// Inside `ESC _ ...`, buffering the 3 marker bytes to decide
    /// SES vs. some other APC.
    ApcPrefix { marker_buf: Vec<u8> },
    /// Confirmed non-SES APC — flush everything (including ESC _ and
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed SES — buffer (un-stuffed) bytes until `ESC \`.
    ApcSes { body: Vec<u8> },
    /// Saw 0x1B inside `ApcSes`; the next byte decides escape
    /// (`0x1B`) vs ST close (`0x5C`).
    ApcSesEsc { body: Vec<u8> },
    /// Saw 0x1B inside `ApcOther`; the next byte decides whether ST
    /// closes the envelope.
    ApcOtherEsc,
}

pub struct ApcStream {
    state: State,
    /// Which 3-byte APC marker to extract. Defaults to the host side
    /// (`SES` uppercase, the commands a client sends). Use
    /// `with_marker(*MARKER_H2C)` on the client to extract the host's
    /// lowercase `ses` responses.
    marker: [u8; 3],
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to the next layer verbatim.
    pub passthrough: Vec<u8>,
    /// Fully-received, un-stuffed SES payloads (one per envelope).
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
    /// Extract client-to-host envelopes (uppercase `SES`). This is what
    /// a host-side `SesEngine` uses.
    pub fn new() -> Self {
        Self {
            state: State::Idle,
            marker: *MARKER_C2H,
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
                    State::ApcSes { body: Vec::new() }
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
            State::ApcSes { mut body } => {
                if b == ESC {
                    State::ApcSesEsc { body }
                } else {
                    body.push(b);
                    State::ApcSes { body }
                }
            }
            State::ApcSesEsc { mut body } => match b {
                ESC => {
                    body.push(ESC);
                    State::ApcSes { body }
                }
                ST_CLOSE => {
                    out.payloads.push(body);
                    State::Idle
                }
                ESC_MARK_TILDE => {
                    body.push(TILDE);
                    State::ApcSes { body }
                }
                ESC_MARK_XON => {
                    body.push(XON);
                    State::ApcSes { body }
                }
                ESC_MARK_XOFF => {
                    body.push(XOFF);
                    State::ApcSes { body }
                }
                _ => {
                    // Only the byte-stuffing escapes (ESC-double, the
                    // transport marks) or ST close are valid inside the
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
    use super::super::frame::MARKER_H2C;
    use super::*;

    fn envelope_c2h(body: &[u8]) -> Vec<u8> {
        let mut v = vec![ESC, APC_OPEN, b'S', b'E', b'S'];
        super::super::codec::stuff(body, &mut v);
        v.push(ESC);
        v.push(ST_CLOSE);
        v
    }

    #[test]
    fn extracts_single_envelope() {
        let mut s = ApcStream::new();
        let body = b"hello";
        let out = s.feed(&envelope_c2h(body));
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], body);
    }

    #[test]
    fn unstuffs_esc_byte() {
        let mut s = ApcStream::new();
        let body = &[0x00, 0x1B, 0xFF, 0x1B];
        let out = s.feed(&envelope_c2h(body));
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], body);
    }

    #[test]
    fn unstuffs_transport_hostile_bytes() {
        // A body carrying ESC, ~, XON and XOFF (interleaved with the
        // newline that makes ~ dangerous) round-trips exactly, and the
        // on-wire envelope is free of literal ~ / XON / XOFF.
        use super::super::frame::{TILDE, XOFF, XON};
        let mut s = ApcStream::new();
        let body = &[b'\n', TILDE, 0x00, ESC, XON, b'\r', TILDE, XOFF, 0xFF];
        let env = envelope_c2h(body);
        assert!(!env.contains(&TILDE), "wire envelope leaked a literal ~");
        assert!(!env.contains(&XON), "wire envelope leaked a literal XON");
        assert!(!env.contains(&XOFF), "wire envelope leaked a literal XOFF");
        let out = s.feed(&env);
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
        let env = envelope_c2h(b"abcdef");
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
    fn vss_envelope_passes_through() {
        let mut s = ApcStream::new();
        let env = vec![
            ESC, APC_OPEN, b'V', b'S', b'S', b'a', b'b', b'c', ESC, ST_CLOSE,
        ];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn back_to_back_envelopes() {
        let mut s = ApcStream::new();
        let mut buf = envelope_c2h(b"one");
        buf.extend(envelope_c2h(b"two"));
        let out = s.feed(&buf);
        assert_eq!(out.payloads.len(), 2);
        assert_eq!(&out.payloads[0], b"one");
        assert_eq!(&out.payloads[1], b"two");
    }

    #[test]
    fn h2c_marker_extracts_lowercase_envelopes() {
        // A client-side stream uses MARKER_H2C to pick up `ses` host
        // responses; uppercase `SES` envelopes pass through.
        let mut s = ApcStream::with_marker(*MARKER_H2C);
        let mut h2c = vec![ESC, APC_OPEN, b's', b'e', b's'];
        super::super::codec::stuff(b"resp", &mut h2c);
        h2c.push(ESC);
        h2c.push(ST_CLOSE);

        let c2h = envelope_c2h(b"cmd");
        let mut all = h2c.clone();
        all.extend_from_slice(&c2h);

        let out = s.feed(&all);
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], b"resp");
        assert_eq!(out.passthrough, c2h);
    }

    #[test]
    fn flush_pending_esc_emits_deferred_lone_esc() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC]);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert!(s.flush_pending_esc().is_empty());
        let out = s.feed(&envelope_c2h(b"x"));
        assert_eq!(out.payloads, vec![b"x".to_vec()]);
    }

    #[test]
    fn malformed_envelope_resyncs() {
        let mut s = ApcStream::new();
        let mut env = vec![ESC, APC_OPEN, b'S', b'E', b'S', b'b', b'a', b'd'];
        env.push(ESC);
        env.push(b'X');
        env.extend_from_slice(b"after");

        let out = s.feed(&env);
        assert!(out.payloads.is_empty());
        assert_eq!(out.passthrough, b"\x1bXafter");
    }
}
