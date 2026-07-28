// Streaming APC envelope extractor (§1.1–1.3) plus side-channel
// observation of a few VT control sequences relevant to VGE state
// (resets, §5.6).
//
// Splits the PTY byte stream into:
//   * `passthrough`: bytes destined for the regular VT parser.
//   * `payloads`:    one Vec<u8> per fully-received VGE APC envelope, with
//                    byte-stuffing already reversed.
//   * `events`:      observational notifications about VT sequences seen
//                    in the stream (e.g. RIS, DECSTR). Bytes still pass
//                    through to vt100 unchanged.
//
// `feed` returns those three as separate bags, which is all a byte
// filter needs. `feed_segments` returns the same content as an ordered
// `Vec<Segment>` instead — required by the terminal stage, which must
// hand the vt100 the text preceding a command before applying it.
//
// Non-VGE APC sequences (e.g. iTerm-style `ESC _G...`) pass through verbatim
// so the underlying VT parser can still handle them. A VGE envelope is
// recognized by the 3-byte uppercase `VGE` marker that follows `ESC _`
// (§1.1: lowercase `vge` is the terminal-to-client direction we never
// receive, so we never match it here).

use super::frame::{
    APC_OPEN, CR, ESC, ESC_MARK_CR, ESC_MARK_LF, ESC_MARK_TAB, ESC_MARK_TILDE,
    ESC_MARK_XON, ESC_MARK_XOFF, LF, MARKER_C2T, ST_CLOSE, TAB, TILDE, XOFF, XON,
};

/// Side-channel events extracted from the byte stream while it flows
/// past us toward vt100. The bytes themselves still pass through; these
/// just notify the engine of state transitions worth reacting to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEvent {
    /// `ESC c` — full reset (§5.6 RIS). VGE state must wipe.
    HardReset,
    /// `ESC [ ! p` — DECSTR soft reset (§5.6). VGE state must wipe.
    SoftReset,
    /// `ESC [ 6 n` — DSR cursor-position query. The host app must
    /// reply with `ESC [ <row> ; <col> R`. vt100 parses but does not
    /// reply, so the engine emits the response itself after vt100
    /// finishes processing the chunk.
    CursorPositionQuery,
    /// `ESC [ 2 J` — erase entire visible screen. The text cells are
    /// wiped in place; vt100 doesn't expose this as a scroll so VGE
    /// elements anchored to the live region would otherwise stick
    /// around. Engines drop top-level elements anchored at or below
    /// `top_of_live_screen`. Scrollback elements are untouched.
    EraseDisplay,
    /// `ESC [ 3 J` — xterm "Erase Saved Lines"; wipes the scrollback
    /// buffer above the live region, NOT the visible screen itself.
    /// Engines drop top-level elements anchored above
    /// `top_of_live_screen`. `clear(1)` typically emits `2J` followed
    /// by `3J`, so the two together wipe all VGE elements.
    EraseScrollback,
}

/// Cap on CSI body length we'll buffer for matching. Long sequences
/// (mostly mode set/reset chains) past this just reset the observer.
const CSI_BUF_CAP: usize = 32;

/// Default cap on a single envelope's **unstuffed** payload.
///
/// The parser buffers a body until `ESC \` closes it, so without a
/// bound a malformed or hostile stream — one that opens an envelope
/// and never closes it — makes the terminal allocate without limit.
/// Over-cap bodies are dropped and the stream resynchronises at the
/// envelope's end.
///
/// Twice the recommended `max_image_bytes` (32 MiB, §11): an
/// UploadImage carries the image plus its id and dimensions, and a
/// host that raises the image cap should raise this with it.
pub const DEFAULT_MAX_PAYLOAD: usize = 64 * 1024 * 1024;

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
    /// Body exceeded `max_payload`. The partial body is already
    /// discarded; bytes are consumed — never passed through, they are
    /// envelope payload, not text — until `ESC \` closes it.
    ApcOverflow,
    /// Saw 0x1B while discarding an over-cap body. Distinguishes the
    /// stuffed `ESC ESC` from the `ESC \` that ends the envelope.
    ApcOverflowEsc,
    /// Inside an `ESC [` CSI sequence. Bytes pass through; we observe to
    /// detect specific finalizers (DECSTR right now). `buf` holds the
    /// parameter / intermediate bytes seen so far.
    Csi { buf: Vec<u8> },
}

pub struct ApcStream {
    state: State,
    /// Which 3-byte APC marker to extract. Defaults to the C2T marker
    /// (`VGE` uppercase) used for client→terminal commands. Use
    /// `with_marker(MARKER_T2C)` on the client side to extract the
    /// terminal's lowercase-`vge` responses.
    marker: [u8; 3],
    /// Largest unstuffed body this stream will buffer.
    max_payload: usize,
    /// Envelopes dropped for exceeding `max_payload`. Read and
    /// cleared by the host so a drop can be reported rather than
    /// silently swallowing a sender's command.
    overflows: u32,
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to vt100 verbatim.
    pub passthrough: Vec<u8>,
    /// Fully-received, un-stuffed VGE payloads (one per envelope).
    pub payloads: Vec<Vec<u8>>,
    /// Side-channel events observed in the stream.
    pub events: Vec<TerminalEvent>,
}

/// One piece of the input stream, **in the order it arrived**.
///
/// [`Output`] answers "what was in this chunk"; `Segment` answers
/// "in what order", which is what anything cursor- or grid-dependent
/// needs. A `CreateElement` whose origin resolves against the cursor
/// must see the text that preceded it in the same read, and only the
/// sequence tells you where that boundary is. See
/// `doc/vector-graphics-extension.md` §5.2 on ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Bytes destined for the vt100, verbatim.
    Pass(Vec<u8>),
    /// One fully-received, un-stuffed VGE payload.
    Payload(Vec<u8>),
    /// A side-channel VT sequence observed at this point. The bytes
    /// themselves are inside the immediately preceding `Pass`, so the
    /// event must be applied *after* that text reaches the vt100.
    Event(TerminalEvent),
}

/// Accumulator the parser writes into. Consecutive passthrough bytes
/// coalesce into one `Pass`, so a chunk of plain text is a single
/// segment rather than one per byte.
#[derive(Default)]
struct SegmentSink {
    segments: Vec<Segment>,
}

impl SegmentSink {
    fn push_pass(&mut self, b: u8) {
        match self.segments.last_mut() {
            Some(Segment::Pass(v)) => v.push(b),
            _ => self.segments.push(Segment::Pass(vec![b])),
        }
    }

    fn push_payload(&mut self, payload: Vec<u8>) {
        self.segments.push(Segment::Payload(payload));
    }

    fn push_event(&mut self, ev: TerminalEvent) {
        self.segments.push(Segment::Event(ev));
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
            max_payload: DEFAULT_MAX_PAYLOAD,
            overflows: 0,
        }
    }

    pub fn with_marker(marker: [u8; 3]) -> Self {
        Self {
            state: State::Idle,
            marker,
            max_payload: DEFAULT_MAX_PAYLOAD,
            overflows: 0,
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

    /// Split `input` into ordered segments. This is the form the
    /// terminal stage — the engine sitting immediately before the
    /// vt100 — consumes, so it can interleave `parser.process` with
    /// command application exactly as the sender wrote them.
    pub fn feed_segments(&mut self, input: &[u8]) -> Vec<Segment> {
        let mut sink = SegmentSink::default();
        for &b in input {
            self.step(b, &mut sink);
        }
        sink.segments
    }

    /// Order-free view of the same extraction: everything the chunk
    /// contained, grouped by kind. Correct for every engine that is a
    /// plain byte filter, and for client-side parsing of the
    /// terminal's replies.
    pub fn feed(&mut self, input: &[u8]) -> Output {
        let mut out = Output::default();
        for seg in self.feed_segments(input) {
            match seg {
                Segment::Pass(bytes) => out.passthrough.extend_from_slice(&bytes),
                Segment::Payload(p) => out.payloads.push(p),
                Segment::Event(e) => out.events.push(e),
            }
        }
        out
    }

    /// Drain a deferred lone ESC (state `EscPending`) and return it as a
    /// single-byte `Vec`. Other states — mid-envelope, mid-CSI, etc. —
    /// are left alone because their bodies must arrive in full.
    ///
    /// Callers should invoke this when the input source has been idle
    /// long enough that a buffered ESC is unambiguously a lone keystroke
    /// rather than the leading byte of an in-flight ESC-sequence.
    pub fn flush_pending_esc(&mut self) -> Vec<u8> {
        if matches!(self.state, State::EscPending) {
            self.state = State::Idle;
            vec![ESC]
        } else {
            Vec::new()
        }
    }

    fn step(&mut self, b: u8, out: &mut SegmentSink) {
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
                b'[' => {
                    // CSI start — ESC + [ go to vt100, we observe the
                    // body for DECSTR.
                    out.push_pass(ESC);
                    out.push_pass(b'[');
                    State::Csi {
                        buf: Vec::with_capacity(8),
                    }
                }
                b'c' => {
                    // RIS — full terminal reset (§5.6).
                    out.push_pass(ESC);
                    out.push_pass(b'c');
                    out.push_event(TerminalEvent::HardReset);
                    State::Idle
                }
                ESC => {
                    // Two ESCs in a row: emit the deferred ESC and hold
                    // the second as pending again.
                    out.push_pass(ESC);
                    State::EscPending
                }
                _ => {
                    // Not APC, not CSI, not RIS — emit the deferred ESC
                    // + this byte. Other ESC-led sequences are vt100's
                    // problem.
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
                } else if body.len() >= self.max_payload {
                    // Drop what we have and swallow the rest of the
                    // envelope. Passing the partial body through
                    // would spray binary at the vt100.
                    self.overflows = self.overflows.saturating_add(1);
                    State::ApcOverflow
                } else {
                    body.push(b);
                    State::ApcVge { body }
                }
            }
            State::ApcVgeEsc { mut body } => {
                // The cap has to be enforced here as well as on the
                // plain-byte path. Every byte of a body made entirely
                // of stuffed escapes arrives through this arm, so
                // checking only there let an all-`ESC ESC` stream
                // buffer without bound — the exact shape a hostile
                // sender would use. `ST_CLOSE` is exempt: it completes
                // the envelope rather than appending to it.
                if b != ST_CLOSE && body.len() >= self.max_payload {
                    self.overflows = self.overflows.saturating_add(1);
                    self.state = State::ApcOverflow;
                    return;
                }
                match b {
                    ESC => {
                        // Stuffed 0x1B — store one literal ESC.
                        body.push(ESC);
                        State::ApcVge { body }
                    }
                    ST_CLOSE => {
                        // Envelope complete.
                        out.push_payload(body);
                        State::Idle
                    }
                    ESC_MARK_TILDE => {
                        body.push(TILDE);
                        State::ApcVge { body }
                    }
                    ESC_MARK_XON => {
                        body.push(XON);
                        State::ApcVge { body }
                    }
                    ESC_MARK_XOFF => {
                        body.push(XOFF);
                        State::ApcVge { body }
                    }
                    ESC_MARK_TAB => {
                        body.push(TAB);
                        State::ApcVge { body }
                    }
                    ESC_MARK_LF => {
                        body.push(LF);
                        State::ApcVge { body }
                    }
                    ESC_MARK_CR => {
                        body.push(CR);
                        State::ApcVge { body }
                    }
                    _ => {
                        // Only the byte-stuffing escapes (ESC-double, the
                        // transport marks) or ST close are valid inside the
                        // envelope. Treat anything else as a malformed envelope:
                        // discard the partial body, emit the stray ESC + byte to
                        // passthrough, and resync.
                        out.push_pass(ESC);
                        out.push_pass(b);
                        State::Idle
                    }
                }
            }
            State::Csi { mut buf } => {
                out.push_pass(b);
                // Final byte? CSI finals are 0x40..=0x7E.
                if (0x40..=0x7E).contains(&b) {
                    // DECSTR is `ESC [ ! p`.
                    if buf.as_slice() == b"!" && b == b'p' {
                        out.push_event(TerminalEvent::SoftReset);
                    }
                    // DSR cursor-position query is `ESC [ 6 n`.
                    if buf.as_slice() == b"6" && b == b'n' {
                        out.push_event(TerminalEvent::CursorPositionQuery);
                    }
                    // Erase In Display:
                    //   `ESC [ 2 J` — wipe live region.
                    //   `ESC [ 3 J` — wipe scrollback.
                    if b == b'J' && buf.as_slice() == b"2" {
                        out.push_event(TerminalEvent::EraseDisplay);
                    }
                    if b == b'J' && buf.as_slice() == b"3" {
                        out.push_event(TerminalEvent::EraseScrollback);
                    }
                    State::Idle
                } else {
                    buf.push(b);
                    if buf.len() > CSI_BUF_CAP {
                        // Pathological / unrecognised — give up on
                        // matching but keep passing bytes until we hit
                        // a final.
                        State::Csi { buf: Vec::new() }
                    } else {
                        State::Csi { buf }
                    }
                }
            }
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
    fn unstuffs_transport_hostile_bytes() {
        // A body carrying ESC, ~, XON and XOFF (interleaved with the
        // newline that makes ~ dangerous) round-trips exactly, and the
        // on-wire envelope is free of literal ~ / XON / XOFF.
        use super::super::frame::{TILDE, XOFF, XON};
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
    fn flush_pending_esc_emits_deferred_lone_esc() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC]);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert!(s.flush_pending_esc().is_empty());
        let out = s.feed(&envelope(b"x"));
        assert_eq!(out.payloads, vec![b"x".to_vec()]);
    }

    #[test]
    fn flush_pending_esc_leaves_mid_envelope_alone() {
        let mut s = ApcStream::new();
        let env = envelope(b"abc");
        let out = s.feed(&env[..env.len() - 1]);
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
    fn ris_emits_hard_reset_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC, b'c']);
        assert_eq!(out.passthrough, vec![ESC, b'c']);
        assert_eq!(out.events, vec![TerminalEvent::HardReset]);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn decstr_emits_soft_reset_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[!p");
        assert_eq!(out.passthrough, b"\x1b[!p");
        assert_eq!(out.events, vec![TerminalEvent::SoftReset]);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn dsr_cursor_query_emits_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[6n");
        assert_eq!(out.passthrough, b"\x1b[6n");
        assert_eq!(out.events, vec![TerminalEvent::CursorPositionQuery]);
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn ed_2_emits_erase_display_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[2J");
        assert_eq!(out.passthrough, b"\x1b[2J");
        assert_eq!(out.events, vec![TerminalEvent::EraseDisplay]);
    }

    #[test]
    fn ed_3_emits_erase_scrollback_event() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[3J");
        assert_eq!(out.events, vec![TerminalEvent::EraseScrollback]);
    }

    #[test]
    fn clear_command_sequence_emits_both_events() {
        // ncurses `clear` sends ESC[H ESC[2J ESC[3J — the engine
        // should see both EraseDisplay and EraseScrollback so it can
        // wipe live and scrollback elements together.
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[H\x1b[2J\x1b[3J");
        assert_eq!(
            out.events,
            vec![
                TerminalEvent::EraseDisplay,
                TerminalEvent::EraseScrollback
            ]
        );
    }

    #[test]
    fn ed_partial_does_not_emit_erase_display() {
        // ESC[J / ESC[0J / ESC[1J are partial erases (cursor-relative)
        // — they don't wipe the whole screen so we don't react to them.
        let mut s = ApcStream::new();
        assert!(s.feed(b"\x1b[J").events.is_empty());
        assert!(s.feed(b"\x1b[0J").events.is_empty());
        assert!(s.feed(b"\x1b[1J").events.is_empty());
    }

    #[test]
    fn other_csi_passes_through_without_events() {
        let mut s = ApcStream::new();
        // CSI cursor home + a private-mode set; no VGE-relevant events.
        let out = s.feed(b"\x1b[H\x1b[?1049h");
        assert_eq!(out.passthrough, b"\x1b[H\x1b[?1049h");
        assert!(out.events.is_empty());
    }

    #[test]
    fn ris_split_across_chunks() {
        let mut s = ApcStream::new();
        let mut all = Output::default();
        for chunk in &[&b"\x1b"[..], &b"c"[..]] {
            let o = s.feed(chunk);
            all.passthrough.extend(o.passthrough);
            all.events.extend(o.events);
        }
        assert_eq!(all.passthrough, b"\x1bc");
        assert_eq!(all.events, vec![TerminalEvent::HardReset]);
    }

    #[test]
    fn decstr_split_across_chunks() {
        let bytes = b"\x1b[!p";
        for split in 1..bytes.len() {
            let mut s = ApcStream::new();
            let mut all = Output::default();
            for chunk in &[&bytes[..split], &bytes[split..]] {
                let o = s.feed(chunk);
                all.passthrough.extend(o.passthrough);
                all.events.extend(o.events);
            }
            assert_eq!(all.passthrough, bytes, "split {split}");
            assert_eq!(all.events, vec![TerminalEvent::SoftReset], "split {split}");
        }
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
    fn segments_preserve_stream_order() {
        // The whole point of the segmented form: a command that arrived
        // between two runs of text must be applied between them, not
        // before both. `feed` alone cannot express this.
        let mut s = ApcStream::new();
        let mut input = b"line1\r\n".to_vec();
        input.extend(envelope(b"cmd"));
        input.extend(b"line2\r\n");
        assert_eq!(
            s.feed_segments(&input),
            vec![
                Segment::Pass(b"line1\r\n".to_vec()),
                Segment::Payload(b"cmd".to_vec()),
                Segment::Pass(b"line2\r\n".to_vec()),
            ]
        );
    }

    #[test]
    fn segments_place_events_after_their_bytes() {
        // The CSI bytes themselves belong to the preceding Pass, so the
        // event must follow them — an engine reacting to `2J` has to
        // see the vt100 state the sequence produced, not the one before
        // it.
        let mut s = ApcStream::new();
        let segs = s.feed_segments(b"a\x1b[2Jb");
        assert_eq!(
            segs,
            vec![
                Segment::Pass(b"a\x1b[2J".to_vec()),
                Segment::Event(TerminalEvent::EraseDisplay),
                Segment::Pass(b"b".to_vec()),
            ]
        );
    }

    #[test]
    fn segments_coalesce_runs_of_passthrough() {
        // One segment per byte would make the caller re-enter the vt100
        // parser for every character.
        let mut s = ApcStream::new();
        let segs = s.feed_segments(b"hello world");
        assert_eq!(segs, vec![Segment::Pass(b"hello world".to_vec())]);
    }

    #[test]
    fn feed_and_feed_segments_agree() {
        // The two views must never disagree about content — only about
        // whether order is preserved.
        let mut input = b"before".to_vec();
        input.extend(envelope(b"one"));
        input.extend(b"\x1b[3Jmiddle");
        input.extend(envelope(b"two"));
        input.extend(b"after\x1bc");

        let mut a = ApcStream::new();
        let out = a.feed(&input);
        let mut b = ApcStream::new();
        let segs = b.feed_segments(&input);

        let mut pass = Vec::new();
        let mut payloads = Vec::new();
        let mut events = Vec::new();
        for seg in segs {
            match seg {
                Segment::Pass(v) => pass.extend_from_slice(&v),
                Segment::Payload(p) => payloads.push(p),
                Segment::Event(e) => events.push(e),
            }
        }
        assert_eq!(out.passthrough, pass);
        assert_eq!(out.payloads, payloads);
        assert_eq!(out.events, events);
    }

    #[test]
    fn segments_survive_a_split_envelope() {
        // Read boundaries are the terminal's, not the client's, so an
        // envelope routinely straddles two chunks. The segment on the
        // far side must still land after the text that preceded it.
        let env = envelope(b"body");
        for split in 1..env.len() {
            let mut s = ApcStream::new();
            let mut first = b"pre".to_vec();
            first.extend_from_slice(&env[..split]);
            let mut segs = s.feed_segments(&first);
            let mut rest = env[split..].to_vec();
            rest.extend_from_slice(b"post");
            segs.extend(s.feed_segments(&rest));
            let payload_at = segs
                .iter()
                .position(|s| matches!(s, Segment::Payload(_)))
                .unwrap_or_else(|| panic!("split {split}: no payload"));
            let pre_at = segs
                .iter()
                .position(|s| matches!(s, Segment::Pass(v) if v.starts_with(b"pre")))
                .unwrap();
            let post_at = segs
                .iter()
                .rposition(|s| matches!(s, Segment::Pass(v) if v.ends_with(b"post")))
                .unwrap();
            assert!(pre_at < payload_at, "split {split}: payload before `pre`");
            assert!(payload_at < post_at, "split {split}: payload after `post`");
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
}
