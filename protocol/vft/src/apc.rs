// Streaming APC envelope extractor (§1.1–1.3).
//
// Splits the PTY byte stream into:
//   * `passthrough`: bytes destined for the next layer.
//   * `payloads`:    one Vec<u8> per fully-received VFT APC envelope, with
//                    byte-stuffing already reversed.
//
// VFT carries no on-screen state, so unlike PRT and VGE this parser does
// not observe any control sequences (RIS, DECSTR, 2J/3J, DSR). Bytes
// that are not part of a `VFT` (or `vft`) APC envelope pass through
// verbatim — including non-VFT APC sequences.

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
/// Transfers are chunked (§5), so no single envelope is large; this
/// is a backstop well above any legitimate chunk.
pub const DEFAULT_MAX_PAYLOAD: usize = 8 * 1024 * 1024;

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
    /// Inside `ESC _ ...`, still buffering the 3 marker bytes to decide
    /// VFT vs. other APC.
    ApcPrefix,
    /// Confirmed non-VFT APC — flush everything (including ESC _ and
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed VFT — buffer (un-stuffed) bytes until `ESC \`.
    ApcVft,
    /// Saw 0x1B inside `ApcVft`; the next byte decides escape (`1B`)
    /// vs ST close (`5C`).
    ApcVftEsc,
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
            State::Idle | State::ApcOther | State::ApcVft | State::ApcOverflow
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
    /// Which 3-byte APC marker to extract. Defaults to the C2H marker
    /// (`VFT` uppercase) used for client-to-host commands. Use
    /// `with_marker(MARKER_H2C)` on the client side to extract the
    /// host's lowercase `vft` responses and events.
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
    /// In `RecoverPrefix`, how many of `marker_buf`'s bytes an earlier
    /// `flush_pending_esc` already handed to the caller — `None` before
    /// the first flush of this recovery, at which point the `_` goes
    /// out too. Set back to `None` whenever `RecoverPrefix` is
    /// (re-)entered. A repeat idle flush must not re-emit what an
    /// earlier one already sent, and the final exit (match or
    /// mismatch) must only emit the bytes that are still unsent.
    recover_flushed: Option<usize>,
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to the next layer verbatim.
    pub passthrough: Vec<u8>,
    /// Fully-received, un-stuffed VFT payloads (one per envelope).
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
            marker: *MARKER_C2H,
            max_payload: DEFAULT_MAX_PAYLOAD,
            overflows: 0,
            body: Vec::new(),
            marker_buf: [0; 3],
            marker_len: 0,
            recover_flushed: None,
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
            recover_flushed: None,
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
            State::ApcVft => {
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

    /// Reference implementation for the tests: one `step` per byte,
    /// with no bulk path. `feed` must agree with this exactly — that
    /// is the whole contract of the run-at-a-time scan.
    #[cfg(test)]
    fn feed_stepwise(&mut self, input: &[u8]) -> Output {
        let mut out = Output::default();
        for &b in input {
            self.step(b, &mut out);
        }
        out
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
    /// bodies must arrive in full. A `RecoverPrefix` still short of its
    /// marker hands back whatever it holds — but, unlike the ESC
    /// above, it does *not* give up on recovering the envelope:
    /// nothing stops the rest of a genuinely split envelope from
    /// arriving behind a second idle gap (a slow link, or nesting —
    /// `vmux -> ssh -> vsd -> vmux` stacks several of these idle
    /// windows), and every byte flushed so far is one this pane
    /// already typed regardless. So the state stays `RecoverPrefix`;
    /// only the accounting of what has already gone out moves,
    /// tracked by `recover_flushed` so a later flush or the eventual
    /// match/mismatch exit doesn't repeat it.
    pub fn flush_pending_esc(&mut self) -> Vec<u8> {
        match self.state {
            State::EscPending => {
                // Not `Idle`: if the next byte is `_`, this ESC opened
                // an envelope that was split across reads rather than
                // a keypress, and `RecoverPrefix` still parses it.
                self.state = State::EscFlushed;
                vec![ESC]
            }
            State::ApcPrefix => {
                // `ESC _` in hand and the marker still short. The same
                // call as `EscPending`, one byte later: release the ESC
                // and hand the rest to `RecoverPrefix`, so a marker
                // that turns up late still reassembles the envelope.
                // The `_` itself waits for the next idle window — by
                // which point two windows have passed with no marker,
                // which is as much evidence as this parser can get that
                // nobody is sending one.
                //
                // Without this arm a typed `Esc _` is swallowed
                // outright until three more bytes arrive. It was nearly
                // unreachable while the escape-time was short enough
                // that the ESC always flushed before the `_` landed,
                // and reachable by ordinary typing once it wasn't.
                self.recover_flushed = None;
                self.state = State::RecoverPrefix;
                vec![ESC]
            }
            State::RecoverPrefix => {
                let mut out = Vec::new();
                let already = match self.recover_flushed {
                    None => {
                        out.push(APC_OPEN);
                        0
                    }
                    Some(n) => n,
                };
                out.extend_from_slice(&self.marker_buf[already..self.marker_len]);
                self.recover_flushed = Some(self.marker_len);
                out
            }
            _ => Vec::new(),
        }
    }

    /// Whether [`ApcStream::flush_pending_esc`] has anything to hand
    /// over — the parser is sitting on bytes that belong downstream if
    /// no follow-up byte arrives.
    ///
    /// Callers arm their escape-time timer on this rather than on "a
    /// poll returned nothing", so the wait is bounded by the timer
    /// instead of by the next quiet moment on an unrelated fd.
    pub fn has_deferred_bytes(&self) -> bool {
        match self.state {
            State::EscPending | State::ApcPrefix => true,
            State::RecoverPrefix => self.recover_flushed != Some(self.marker_len),
            _ => false,
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
                    self.recover_flushed = None;
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
                    // The split envelope, back in one piece. Whatever
                    // an idle flush already forwarded from the prefix
                    // is stuck at the pane, but the rest is ours again.
                    self.body.clear();
                    self.recover_flushed = None;
                    State::ApcVft
                } else {
                    // Someone else's APC, or a `_` the user typed after
                    // pressing Esc. Pass whatever of `marker_buf` an
                    // idle flush hasn't already sent, and return to
                    // Idle rather than entering `ApcOther`: the ESC is
                    // downstream already, so a parser after this one is
                    // in `EscFlushed` too and recovers a foreign
                    // envelope by the same route. Idle also means a
                    // typed `Esc _ a b c` can't leave us hunting for a
                    // terminator that was never coming.
                    let already = match self.recover_flushed {
                        None => {
                            out.push_pass(APC_OPEN);
                            0
                        }
                        Some(n) => n,
                    };
                    out.passthrough.extend_from_slice(&self.marker_buf[already..]);
                    self.recover_flushed = None;
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
                    State::ApcVft
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
                if b == APC_OPEN {
                    // `ESC _` inside what we took for someone else's
                    // APC. APC strings don't nest, and every envelope in
                    // this family stuffs a literal ESC as `ESC ESC`
                    // (§1.3), so this is not payload — we were never
                    // inside an envelope at all. The way in is a user
                    // typing `Esc _`, which is a motion in vim and looks
                    // exactly like an opener whose marker matches
                    // nothing. Take it as the opener it is. Otherwise we
                    // would stay in `ApcOther` until some ST happened
                    // along, passing every envelope that arrived first
                    // through as text — which a multiplexer types at
                    // whichever pane has focus.
                    self.marker_len = 0;
                    State::ApcPrefix
                } else {
                    out.push_pass(ESC);
                    out.push_pass(b);
                    if b == ST_CLOSE {
                        State::Idle
                    } else {
                        State::ApcOther
                    }
                }
            }
            State::ApcVft => {
                if b == ESC {
                    State::ApcVftEsc
                } else if self.body.len() >= self.max_payload {
                    self.overflow()
                } else {
                    self.body.push(b);
                    State::ApcVft
                }
            }
            State::ApcVftEsc => {
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
                        State::ApcVft
                    }
                    ST_CLOSE => {
                        out.payloads.push(std::mem::take(&mut self.body));
                        State::Idle
                    }
                    ESC_MARK_TILDE => {
                        self.body.push(TILDE);
                        State::ApcVft
                    }
                    ESC_MARK_XON => {
                        self.body.push(XON);
                        State::ApcVft
                    }
                    ESC_MARK_XOFF => {
                        self.body.push(XOFF);
                        State::ApcVft
                    }
                    ESC_MARK_TAB => {
                        self.body.push(TAB);
                        State::ApcVft
                    }
                    ESC_MARK_LF => {
                        self.body.push(LF);
                        State::ApcVft
                    }
                    ESC_MARK_CR => {
                        self.body.push(CR);
                        State::ApcVft
                    }
                    _ => {
                        self.body.clear();
                        // Spec permits only the §1.3 escapes (ESC-double, the
                        // transport marks) or ST close inside the envelope.
                        // Anything else is malformed: discard the partial
                        // body, emit the stray ESC + byte to passthrough, and
                        // resync.
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

    fn envelope(body: &[u8]) -> Vec<u8> {
        let mut v = vec![ESC, APC_OPEN, b'V', b'F', b'T'];
        super::super::codec::stuff(body, &mut v);
        v.push(ESC);
        v.push(ST_CLOSE);
        v
    }

    /// `feed` must produce exactly what stepping the state machine byte
    /// by byte produces. VFT is the parser a file transfer's bytes
    /// actually flow through, so the run-at-a-time scan is load-bearing
    /// here and its equivalence is worth pinning.
    fn assert_bulk_matches_per_byte(input: &[u8], max_payload: usize) {
        let mut bulk = ApcStream::new().with_max_payload(max_payload);
        let whole = bulk.feed(input);
        let mut slow = ApcStream::new().with_max_payload(max_payload);
        let stepwise = slow.feed_stepwise(input);
        assert_eq!(whole.passthrough, stepwise.passthrough, "passthrough differs");
        assert_eq!(whole.payloads, stepwise.payloads, "payloads differ");
        assert_eq!(
            bulk.take_overflows(),
            slow.take_overflows(),
            "overflow count differs"
        );
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree() {
        let mut s = Vec::new();
        s.extend_from_slice(b"plain text before\r\n");
        // An upload chunk's worth of binary, every byte value present
        // so the stuffing escapes land mid-run.
        let body: Vec<u8> = (0u16..1024).map(|i| (i % 256) as u8).collect();
        s.extend_from_slice(&envelope(&body));
        // Someone else's APC — passes through verbatim.
        s.extend_from_slice(b"\x1b_PRTnot ours\x1b\\");
        s.extend_from_slice(b"\x1bZtail\r\n");
        for cap in [0, 1, 16, 1024, DEFAULT_MAX_PAYLOAD] {
            assert_bulk_matches_per_byte(&s, cap);
        }
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree_at_the_payload_cap() {
        // A 512-byte body with nothing to stuff reaches the bulk path
        // as one run, so the cap is decided there; these three caps
        // straddle it.
        let mut clean = Vec::from(&b"\x1b_VFT"[..]);
        clean.resize(clean.len() + 512, b'A');
        clean.extend_from_slice(b"\x1b\\");
        for cap in [511, 512, 513] {
            assert_bulk_matches_per_byte(&clean, cap);
        }
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree_on_a_malformed_escape() {
        // `ESC q` is not a valid stuffing escape: the partial body is
        // discarded and the stream resyncs. A body buffer that outlives
        // its state without being cleared would leak those bytes into
        // the next envelope.
        let mut s = Vec::from(&b"\x1b_VFTleading body\x1bq trailing\x1b\\"[..]);
        s.extend_from_slice(&envelope(b"the next one"));
        assert_bulk_matches_per_byte(&s, DEFAULT_MAX_PAYLOAD);
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
    fn unstuffs_transport_hostile_bytes() {
        // A body carrying ESC, ~, XON and XOFF (interleaved with the
        // newline that makes ~ dangerous) round-trips exactly, and the
        // on-wire envelope is free of literal ~ / XON / XOFF.
        let mut s = ApcStream::new();
        let body = &[b'\n', TILDE, 0x00, ESC, XON, b'\r', TILDE, XOFF, 0xFF];
        let env = envelope(body);
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
        let env = envelope(b"abcdef");
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
        // ESC _ P R T ... ESC \ — a PRT envelope must come back unchanged
        // in passthrough so the layered parser can pick it up.
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
    fn kitty_graphics_apc_passes_through() {
        // ESC _ G abc ESC \ (kitty graphics-style envelope)
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
        // VFT does not interpret CSI in any way.
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
        // Idempotent — second flush has nothing to drain.
        assert!(s.flush_pending_esc().is_empty());
        // After flush, parser is back to Idle and accepts a fresh
        // envelope.
        let out = s.feed(&envelope(b"x"));
        assert_eq!(out.payloads, vec![b"x".to_vec()]);
    }

    #[test]
    fn flush_pending_esc_leaves_mid_envelope_alone() {
        let mut s = ApcStream::new();
        let env = envelope(b"abc");
        let out = s.feed(&env[..env.len() - 1]); // everything but ST_CLOSE
        assert!(out.payloads.is_empty());
        assert!(s.flush_pending_esc().is_empty());
        let out = s.feed(&env[env.len() - 1..]);
        assert_eq!(out.payloads, vec![b"abc".to_vec()]);
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

    #[test]
    fn h2c_marker_extracts_lowercase_envelopes() {
        // A client-side stream uses MARKER_H2C to pick up `vft` host
        // responses; uppercase `VFT` envelopes should pass through.
        let mut s = ApcStream::with_marker(*MARKER_H2C);
        let mut h2c = vec![ESC, APC_OPEN, b'v', b'f', b't'];
        super::super::codec::stuff(b"resp", &mut h2c);
        h2c.push(ESC);
        h2c.push(ST_CLOSE);

        let c2h = envelope(b"cmd");
        let mut all = h2c.clone();
        all.extend_from_slice(&c2h);

        let out = s.feed(&all);
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], b"resp");
        assert_eq!(out.passthrough, c2h);
    }

    #[test]
    fn malformed_envelope_resyncs() {
        // ESC _ V F T ... ESC followed by garbage instead of ST_CLOSE
        // should resync to passthrough rather than swallowing later data.
        let mut s = ApcStream::new();
        let mut env = vec![ESC, APC_OPEN, b'V', b'F', b'T', b'b', b'a', b'd'];
        env.push(ESC);
        env.push(b'X'); // not stuffing, not ST_CLOSE
        env.extend_from_slice(b"after");

        let out = s.feed(&env);
        // Body was discarded; the stray ESC X and trailing "after"
        // surface as passthrough.
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
                let env = envelope(&body);
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
        let mut input = envelope(&vec![b'x'; 65]);
        input.extend(envelope(b"after"));
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
        let out = s.feed(&envelope(&body));
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
        let mut input = envelope(&hostile);
        input.extend(envelope(b"ok"));
        let out = s.feed(&input);
        assert_eq!(out.payloads, vec![b"ok".to_vec()]);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.take_overflows(), 1);
    }

    #[test]
    fn over_cap_envelope_split_across_reads_is_dropped_once() {
        let mut input = envelope(&vec![b'z'; 300]);
        input.extend(envelope(b"tail"));
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
    /// `ESC _ VFT … ESC \\`, the shape that gets split.
    fn split_test_envelope() -> Vec<u8> {
        envelope(b"body")
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


    /// Esc, `_`, then nothing: the marker never completes on the
    /// first flush either. A repeat idle flush must not duplicate
    /// what an earlier one already released, and — unlike the plain
    /// ESC case — must not give up on the recovery: the marker can
    /// still complete later. Once it turns out to mismatch, only the
    /// not-yet-flushed tail comes out.
    #[test]
    fn a_repeated_flush_does_not_give_up_or_duplicate() {
        let mut s = ApcStream::new();
        s.feed(&[ESC]);
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert!(s.feed(b"_P").passthrough.is_empty());
        assert_eq!(s.flush_pending_esc(), b"_P".to_vec());
        // A second idle gap with nothing new to report: nothing is
        // re-sent, and the recovery is still alive.
        assert_eq!(s.flush_pending_esc(), Vec::<u8>::new());
        // The rest of the marker mismatches — a plain `Esc _ P x y`,
        // not an envelope. Only the tail an earlier flush hadn't
        // already sent comes out; `_P` isn't repeated.
        assert_eq!(s.feed(b"xy").passthrough, b"xy");
        // And still usable afterwards.
        let out = s.feed(&split_test_envelope());
        assert_eq!(out.payloads.len(), 1);
    }

    /// The case the previous test's old behavior broke: the marker
    /// completes only after a *second* idle flush already released
    /// part of the prefix. Nesting stacks idle windows (`vmux -> ssh
    /// -> vsd -> vmux`, each hop with its own), so surviving one flush
    /// mid-recovery isn't enough — it has to survive more than one.
    #[test]
    fn recovery_survives_more_than_one_idle_flush() {
        let env = split_test_envelope();
        let mut s = ApcStream::new();

        assert!(s.feed(&env[..1]).passthrough.is_empty()); // ESC alone
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert!(s.feed(&env[1..2]).passthrough.is_empty()); // `_`
        // No marker bytes have arrived yet — the flush still owes the
        // caller the `_` itself.
        assert_eq!(s.flush_pending_esc(), vec![APC_OPEN]);
        assert!(s.feed(&env[2..3]).passthrough.is_empty()); // marker byte 1
        // Released, unavoidably — but the recovery keeps going rather
        // than resyncing to Idle.
        assert_eq!(s.flush_pending_esc(), env[2..3].to_vec());

        let out = s.feed(&env[3..]);
        assert_eq!(out.payloads.len(), 1, "envelope lost after two idle flushes");
        assert!(
            out.passthrough.is_empty(),
            "envelope leaked as input: {:?}",
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

    /// `Esc _` is a motion in vim, and typing it used to poison the
    /// parser: the three bytes after `_` are read as a marker, match
    /// nothing, and leave the stream inside a foreign APC until an ST
    /// turns up. The next real envelope then went through as text —
    /// the same garbage-at-the-prompt as a split envelope, from a
    /// keystroke instead of a chunk boundary.
    #[test]
    fn typed_esc_underscore_does_not_swallow_the_next_envelope() {
        let mut s = ApcStream::new();

        // Typed fast enough to land in one read, so no idle flush
        // rescues it.
        let out = s.feed(b"\x1b_abcdef");
        assert_eq!(
            out.passthrough, b"\x1b_abcdef",
            "what the user typed must reach the pane, in order"
        );

        // An envelope arriving behind it is still an envelope.
        let out = s.feed(&split_test_envelope());
        assert_eq!(out.payloads.len(), 1, "envelope lost after a typed `Esc _`");
        assert!(
            out.passthrough.is_empty(),
            "envelope leaked as input: {:?}",
            String::from_utf8_lossy(&out.passthrough)
        );
    }

    /// The resync must not cost us the pass-through contract: a real
    /// foreign envelope still arrives downstream whole, terminator
    /// included, and leaves the stream ready for the next one.
    #[test]
    fn a_real_foreign_envelope_still_passes_through_verbatim() {
        let mut foreign = vec![ESC, APC_OPEN];
        foreign.extend_from_slice(b"xyz");
        // A payload with the family's own stuffing in it: a literal
        // ESC travels as `ESC ESC`, which must not read as an opener.
        foreign.extend_from_slice(b"bo");
        foreign.extend_from_slice(&[ESC, ESC]);
        foreign.extend_from_slice(b"_dy");
        foreign.extend_from_slice(&[ESC, ST_CLOSE]);

        let mut s = ApcStream::new();
        let out = s.feed(&foreign);
        assert_eq!(out.passthrough, foreign, "foreign envelope altered");

        let out = s.feed(&split_test_envelope());
        assert_eq!(out.payloads.len(), 1);
        assert!(out.passthrough.is_empty());
    }
}
