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
    APC_OPEN, CR, ESC, ESC_MARK_CR, ESC_MARK_LF, ESC_MARK_TAB, ESC_MARK_TILDE,
    ESC_MARK_XON, ESC_MARK_XOFF, LF, MARKER_C2H, ST_CLOSE, TAB, TILDE, XOFF, XON,
};

/// Default cap on a single envelope's **unstuffed** payload.
///
/// The parser buffers a body until `ESC \` closes it, so without a
/// bound a malformed or hostile stream — one that opens an envelope
/// and never closes it — makes the terminal allocate without limit.
/// Over-cap bodies are dropped and the stream resynchronises at the
/// envelope's end.
///
/// SES carries only session names and a detach command.
pub const DEFAULT_MAX_PAYLOAD: usize = 64 * 1024;

/// Parser state.
///
/// `Copy` on purpose: the body of the envelope in flight lives in
/// `ApcStream::body`, not in the variant. Keeping the `Vec` out of the
/// enum is what lets `step` be a branch instead of a move of the whole
/// state, and what lets `feed` consume a whole run of bytes at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal pass-through stream.
    Idle,
    /// Saw 0x1B in Idle; deciding whether it opens APC.
    EscPending,
    /// A deferred ESC was handed to the consumer by
    /// [`ApcStream::flush_pending_esc`], and the very next byte
    /// decides whether that was right. `_` means it wasn't: an
    /// envelope was split across reads at its opening ESC and the
    /// rest has now caught up.
    EscFlushed,
    /// Reading the marker of an envelope whose ESC we already
    /// flushed. Same job as `ApcPrefix`, but on a mismatch it
    /// re-emits only what it holds — the ESC is already gone
    /// downstream, and re-adding it would double it.
    RecoverPrefix,
    /// Inside `ESC _ ...`, buffering the 3 marker bytes to decide
    /// SES vs. some other APC.
    ApcPrefix,
    /// Confirmed non-SES APC — flush everything (including ESC _ and
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed SES — buffer (un-stuffed) bytes until `ESC \`.
    ApcSes,
    /// Saw 0x1B inside `ApcSes`; the next byte decides escape
    /// (`0x1B`) vs ST close (`0x5C`).
    ApcSesEsc,
    /// Saw 0x1B inside `ApcOther`; the next byte decides whether ST
    /// closes the envelope.
    ApcOtherEsc,
    /// Body exceeded `max_payload`. The partial body is already
    /// discarded; bytes are consumed — never passed through, they are
    /// envelope payload, not text — until `ESC \` closes it.
    ApcOverflow,
    /// Saw 0x1B while discarding an over-cap body. Distinguishes the
    /// stuffed `ESC ESC` from the `ESC \` that ends the envelope.
    ApcOverflowEsc,
}

impl State {
    /// States in which every byte up to the next ESC is handled the
    /// same way — copied to passthrough, appended to the body, or
    /// dropped — so `feed` can take the whole run in one call.
    fn bulkable(self) -> bool {
        matches!(
            self,
            State::Idle | State::ApcOther | State::ApcSes | State::ApcOverflow
        )
    }
}

/// Offset of the next ESC in `buf`. Every state transition in this
/// parser is triggered by ESC, so this is the only scan the bulk path
/// needs.
#[inline]
fn next_esc(buf: &[u8]) -> Option<usize> {
    buf.iter().position(|&b| b == ESC)
}

pub struct ApcStream {
    state: State,
    /// Which 3-byte APC marker to extract. Defaults to the host side
    /// (`SES` uppercase, the commands a client sends). Use
    /// `with_marker(*MARKER_H2C)` on the client to extract the host's
    /// lowercase `ses` responses.
    marker: [u8; 3],
    /// Largest unstuffed body this stream will buffer.
    max_payload: usize,
    /// Envelopes dropped for exceeding `max_payload`. Read and
    /// cleared by the host so a drop can be reported rather than
    /// silently swallowing a sender's command.
    overflows: u32,
    /// Un-stuffed body of the envelope in flight, empty otherwise.
    body: Vec<u8>,
    /// The 3 bytes after `ESC _`, while `ApcPrefix` decides whether the
    /// envelope is ours.
    marker_buf: [u8; 3],
    /// How many of `marker_buf` have arrived.
    marker_len: usize,
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
            max_payload: DEFAULT_MAX_PAYLOAD,
            overflows: 0,
            body: Vec::new(),
            marker_buf: [0; 3],
            marker_len: 0,
        }
    }

    pub fn with_marker(marker: [u8; 3]) -> Self {
        Self {
            state: State::Idle,
            marker,
            max_payload: DEFAULT_MAX_PAYLOAD,
            overflows: 0,
            body: Vec::new(),
            marker_buf: [0; 3],
            marker_len: 0,
        }
    }

    /// Override the per-envelope payload cap. Hosts set this from
    /// their own advertised limits so the parser and the command
    /// layer agree on what is too big.
    pub fn with_max_payload(mut self, max_payload: usize) -> Self {
        self.max_payload = max_payload;
        self
    }

    /// Envelopes dropped for exceeding the cap since the last call.
    /// Reading clears the counter.
    pub fn take_overflows(&mut self) -> u32 {
        std::mem::take(&mut self.overflows)
    }

    pub fn feed(&mut self, input: &[u8]) -> Output {
        let mut out = Output::default();
        let mut i = 0;
        while i < input.len() {
            // Bulk path: hand the whole run of bytes before the next
            // ESC to one `extend_from_slice` instead of stepping the
            // state machine per byte. Bulk transfer is almost entirely
            // this path — the per-byte loop it replaces was the host's
            // throughput ceiling under one.
            if self.state.bulkable() {
                let rest = &input[i..];
                let run = next_esc(rest).unwrap_or(rest.len());
                if run > 0 {
                    self.bulk(&rest[..run], &mut out);
                    i += run;
                    continue;
                }
            }
            self.step(input[i], &mut out);
            i += 1;
        }
        out
    }

    /// Consume `run` — guaranteed ESC-free — in a [`State::bulkable`]
    /// state.
    fn bulk(&mut self, run: &[u8], out: &mut Output) {
        match self.state {
            State::Idle | State::ApcOther => out.passthrough.extend_from_slice(run),
            // Over-cap body: these bytes are envelope payload, dropped
            // rather than passed through, until `ESC \\` resyncs us.
            State::ApcOverflow => {}
            State::ApcSes => {
                // Same cap as the per-byte arm: a body may reach
                // `max_payload` exactly, and the byte after it overflows.
                if self.body.len() + run.len() > self.max_payload {
                    self.state = self.overflow();
                } else {
                    self.body.extend_from_slice(run);
                }
            }
            _ => debug_assert!(false, "bulk() in a non-bulkable state"),
        }
    }

    /// Drop the body in flight and swallow the rest of the envelope.
    /// Passing a partial body on would spray binary at the vt100.
    fn overflow(&mut self) -> State {
        self.overflows = self.overflows.saturating_add(1);
        self.body.clear();
        State::ApcOverflow
    }

    /// Drain a deferred lone ESC (state `EscPending`) and return it so
    /// the caller can treat it as the keystroke it probably is.
    ///
    /// Callers should invoke this when the input source has been idle
    /// long enough that a buffered ESC is unambiguously a lone
    /// keystroke rather than the leading byte of an in-flight
    /// ESC-sequence. With no flush, a lone ESC sits in `EscPending`
    /// until the next byte arrives — which, for an interactive
    /// terminal, can mean a modal dismiss key apparently does
    /// nothing.
    ///
    /// "Unambiguously" is a guess, and on a slow link it is sometimes
    /// wrong: an envelope split across reads at its opening ESC, with
    /// the rest more than the idle window behind it, looks exactly
    /// like a keypress. So the guess is not final. The stream moves to
    /// `EscFlushed` rather than `Idle`, and if `_` and a matching
    /// marker do turn up, the envelope is parsed as one — the ESC
    /// went out early, but its payload never reaches the caller as
    /// input. Before this, everything after that ESC was passed
    /// through: a multiplexer typed the marker, header and body at
    /// whichever pane had focus.
    ///
    /// Mid-envelope and mid-CSI states are left alone, because their
    /// bodies must arrive in full. A `RecoverPrefix` whose marker
    /// never completed is released here, minus the ESC the caller
    /// already has.
    pub fn flush_pending_esc(&mut self) -> Vec<u8> {
        match self.state {
            State::EscPending => {
                // Not `Idle`: if the next byte is `_`, this ESC opened
                // an envelope that was split across reads rather than
                // a keypress, and `RecoverPrefix` still parses it.
                self.state = State::EscFlushed;
                vec![ESC]
            }
            // Giving up on a recovery that never completed its marker:
            // hand back the bytes still held. Without the ESC — the
            // consumer got that one already.
            State::RecoverPrefix => {
                let mut out = vec![APC_OPEN];
                out.extend_from_slice(&self.marker_buf[..self.marker_len]);
                self.marker_len = 0;
                self.state = State::Idle;
                out
            }
            _ => Vec::new(),
        }
    }

    fn step(&mut self, b: u8, out: &mut Output) {
        self.state = match self.state {
            State::Idle => {
                if b == ESC {
                    State::EscPending
                } else {
                    out.push_pass(b);
                    State::Idle
                }
            }
            State::EscPending => match b {
                APC_OPEN => {
                    self.marker_len = 0;
                    State::ApcPrefix
                }
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
            State::EscFlushed => match b {
                APC_OPEN => {
                    self.marker_len = 0;
                    State::RecoverPrefix
                }
                ESC => State::EscPending,
                _ => {
                    out.push_pass(b);
                    State::Idle
                }
            },
            State::RecoverPrefix => {
                self.marker_buf[self.marker_len] = b;
                self.marker_len += 1;
                if self.marker_len < 3 {
                    State::RecoverPrefix
                } else if self.marker_buf == self.marker {
                    // The split envelope, back in one piece.
                    self.body.clear();
                    State::ApcSes
                } else {
                    // Someone else's APC, or a `_` the user typed after
                    // pressing Esc. Pass what we hold and return to
                    // Idle rather than entering `ApcOther`: the ESC is
                    // downstream already, so a parser after this one is
                    // in `EscFlushed` too and recovers a foreign
                    // envelope by the same route. Idle also means a
                    // typed `Esc _ a b c` can't leave us hunting for a
                    // terminator that was never coming.
                    out.push_pass(APC_OPEN);
                    out.passthrough.extend_from_slice(&self.marker_buf);
                    State::Idle
                }
            }
            State::ApcPrefix => {
                self.marker_buf[self.marker_len] = b;
                self.marker_len += 1;
                if self.marker_len < 3 {
                    State::ApcPrefix
                } else if self.marker_buf == self.marker {
                    self.body.clear();
                    State::ApcSes
                } else {
                    out.push_pass(ESC);
                    out.push_pass(APC_OPEN);
                    out.passthrough.extend_from_slice(&self.marker_buf);
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
            State::ApcOverflow => {
                if b == ESC {
                    State::ApcOverflowEsc
                } else {
                    State::ApcOverflow
                }
            }
            State::ApcOverflowEsc => {
                // Stuffing guarantees the only bare `ESC \` in an
                // envelope is its terminator, so this resync is exact
                // rather than best-effort.
                if b == ST_CLOSE {
                    State::Idle
                } else {
                    State::ApcOverflow
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
            State::ApcSes => {
                if b == ESC {
                    State::ApcSesEsc
                } else if self.body.len() >= self.max_payload {
                    self.overflow()
                } else {
                    self.body.push(b);
                    State::ApcSes
                }
            }
            State::ApcSesEsc => {
                // The cap has to be enforced here as well as on the
                // plain-byte path. Every byte of a body made entirely
                // of stuffed escapes arrives through this arm, so
                // checking only there let an all-`ESC ESC` stream
                // buffer without bound — the exact shape a hostile
                // sender would use. `ST_CLOSE` is exempt: it completes
                // the envelope rather than appending to it.
                if b != ST_CLOSE && self.body.len() >= self.max_payload {
                    self.state = self.overflow();
                    return;
                }
                match b {
                    ESC => {
                        self.body.push(ESC);
                        State::ApcSes
                    }
                    ST_CLOSE => {
                        out.payloads.push(std::mem::take(&mut self.body));
                        State::Idle
                    }
                    ESC_MARK_TILDE => {
                        self.body.push(TILDE);
                        State::ApcSes
                    }
                    ESC_MARK_XON => {
                        self.body.push(XON);
                        State::ApcSes
                    }
                    ESC_MARK_XOFF => {
                        self.body.push(XOFF);
                        State::ApcSes
                    }
                    ESC_MARK_TAB => {
                        self.body.push(TAB);
                        State::ApcSes
                    }
                    ESC_MARK_LF => {
                        self.body.push(LF);
                        State::ApcSes
                    }
                    ESC_MARK_CR => {
                        self.body.push(CR);
                        State::ApcSes
                    }
                    _ => {
                        self.body.clear();
                        // Only the byte-stuffing escapes (ESC-double, the
                        // transport marks) or ST close are valid inside the
                        // envelope. Treat anything else as malformed: discard
                        // the partial body, emit the stray ESC + byte to
                        // passthrough, and resync.
                        out.push_pass(ESC);
                        out.push_pass(b);
                        State::Idle
                    }
                }
            }
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

    /// Deterministic xorshift. These crates carry no `rand`
    /// dependency, and a fixed seed keeps a failure reproducible.
    fn xorshift(state: &mut u64) -> u8 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state & 0xFF) as u8
    }

    #[test]
    fn stuffing_round_trips_and_stays_transport_clean() {
        // Property over arbitrary payloads: the on-wire body carries
        // none of the six bytes a relay or a cooked tty would rewrite,
        // and the parser recovers the payload byte-for-byte. Random
        // payloads catch what an exhaustive single-byte sweep cannot —
        // that the escape emitted for one byte is never misread as the
        // mark belonging to the next.
        use super::super::frame::{CR, LF, TAB, TILDE, XOFF, XON};
        let hostile = [TAB, LF, CR, TILDE, XON, XOFF];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        for len in [0usize, 1, 2, 3, 7, 64, 257, 1024] {
            for round in 0..16 {
                let body: Vec<u8> = (0..len).map(|_| xorshift(&mut state)).collect();
                let env = envelope_c2h(&body);
                // Envelope body only: `ESC _ <marker>` and the closing
                // `ESC \` are framing, not payload.
                let wire = &env[5..env.len() - 2];
                for b in hostile {
                    assert!(
                        !wire.contains(&b),
                        "byte {b:#04X} leaked (len {len}, round {round})"
                    );
                }
                let mut s = ApcStream::new();
                let out = s.feed(&env);
                assert!(
                    out.passthrough.is_empty(),
                    "leaked passthrough (len {len}, round {round})"
                );
                assert_eq!(
                    out.payloads,
                    vec![body],
                    "round-trip failed (len {len}, round {round})"
                );
            }
        }
    }

    #[test]
    fn over_cap_envelope_is_dropped_and_the_stream_resyncs() {
        // An unbounded parser lets a malformed or hostile stream make
        // the terminal allocate without limit. Over-cap bodies are
        // dropped whole — never half-emitted, and never sprayed at the
        // vt100 as passthrough — and the *next* envelope still parses,
        // which is what makes the drop survivable.
        let mut s = ApcStream::new().with_max_payload(64);
        let mut input = envelope_c2h(&vec![b'x'; 65]);
        input.extend(envelope_c2h(b"after"));
        let out = s.feed(&input);
        assert_eq!(out.payloads, vec![b"after".to_vec()], "resync failed");
        assert!(out.passthrough.is_empty(), "dropped body leaked as text");
        assert_eq!(s.take_overflows(), 1);
        assert_eq!(s.take_overflows(), 0, "counter should clear on read");
    }

    #[test]
    fn at_cap_envelope_still_parses() {
        // Off-by-one guard: the cap is a maximum, not a strict bound.
        let mut s = ApcStream::new().with_max_payload(64);
        let body = vec![b'y'; 64];
        let out = s.feed(&envelope_c2h(&body));
        assert_eq!(out.payloads, vec![body]);
        assert_eq!(s.take_overflows(), 0);
    }

    #[test]
    fn over_cap_body_full_of_escapes_still_resyncs() {
        // The discard path has to keep unstuffing well enough to tell a
        // stuffed `ESC ESC` from the `ESC \` that ends the envelope,
        // or it resynchronises in the middle of the body and emits
        // garbage.
        let mut s = ApcStream::new().with_max_payload(8);
        let hostile: Vec<u8> = std::iter::repeat_n(ESC, 64).collect();
        let mut input = envelope_c2h(&hostile);
        input.extend(envelope_c2h(b"ok"));
        let out = s.feed(&input);
        assert_eq!(out.payloads, vec![b"ok".to_vec()]);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.take_overflows(), 1);
    }

    #[test]
    fn over_cap_envelope_split_across_reads_is_dropped_once() {
        let mut input = envelope_c2h(&vec![b'z'; 300]);
        input.extend(envelope_c2h(b"tail"));
        for cut in 1..input.len() {
            let mut s = ApcStream::new().with_max_payload(16);
            let mut payloads = Vec::new();
            for part in [&input[..cut], &input[cut..]] {
                let out = s.feed(part);
                payloads.extend(out.payloads);
                assert!(out.passthrough.is_empty(), "cut {cut}: leaked text");
            }
            assert_eq!(payloads, vec![b"tail".to_vec()], "cut {cut}");
            assert_eq!(s.take_overflows(), 1, "cut {cut}");
        }
    }
    /// `ESC _ SES … ESC \\`, the shape that gets split.
    fn split_test_envelope() -> Vec<u8> {
        envelope_c2h(b"body")
    }

    // --- a flushed ESC is remembered, not forgotten ------------------

    /// A multiplexer parses this stream for envelopes *and* forwards
    /// what is left to a pane as keystrokes, so an envelope that stops
    /// being recognised gets typed at whatever program is focused.
    /// That is what happened: a chunk boundary landing on the ESC that
    /// opens an envelope, with more than the idle window of latency
    /// behind it, and the ESC went out as a lone keypress. Everything
    /// after it — marker, header, payload — was then nobody's
    /// envelope.
    #[test]
    fn envelope_split_at_esc_survives_an_idle_flush() {
        let env = split_test_envelope();
        let mut s = ApcStream::new();

        assert!(s.feed(&env[..1]).passthrough.is_empty());
        // The idle window elapses; the consumer is handed the ESC.
        assert_eq!(s.flush_pending_esc(), vec![ESC]);

        let out = s.feed(&env[1..]);
        assert_eq!(out.payloads.len(), 1, "envelope lost after the flush");
        assert!(
            out.passthrough.is_empty(),
            "envelope bytes leaked to the consumer as input: {:?}",
            String::from_utf8_lossy(&out.passthrough)
        );
    }

    /// The flush still has its job to do: a real lone Esc reaches the
    /// consumer, and the keystroke after it is not swallowed.
    #[test]
    fn a_real_lone_esc_still_flushes_and_the_next_key_follows() {
        let mut s = ApcStream::new();
        assert!(s.feed(&[ESC]).passthrough.is_empty());
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert_eq!(s.feed(b"a").passthrough, b"a");
    }
}
