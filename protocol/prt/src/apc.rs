// Streaming APC envelope extractor (§1.1–1.3) plus side-channel
// observation of a few VT control sequences relevant to PRT host-screen
// state (resets, erase-display).
//
// Splits the PTY byte stream into:
//   * `passthrough`: bytes destined for the next layer (which is either
//                    another extension's APC stream, or the regular VT
//                    parser).
//   * `payloads`:    one Vec<u8> per fully-received PRT APC envelope, with
//                    byte-stuffing already reversed.
//   * `events`:      observational notifications about VT sequences seen
//                    in the stream (e.g. RIS, DECSTR, 2J/3J). Bytes still
//                    pass through to the next layer unchanged.
//
// Non-PRT APC sequences (e.g. iTerm-style `ESC _G`, VGE's `ESC _VGE`) pass
// through verbatim so the next layer can still handle them. A PRT envelope
// is recognised by the 3-byte uppercase `PRT` marker that follows `ESC _`
// (§1.1: lowercase `prt` is the host-to-client direction we only see
// when running as a client of a parent host — `with_marker(MARKER_T2C)`
// in that case).

use super::frame::{
    APC_OPEN, CR, ESC, ESC_MARK_CR, ESC_MARK_LF, ESC_MARK_TAB, ESC_MARK_TILDE,
    ESC_MARK_XON, ESC_MARK_XOFF, LF, MARKER_C2T, ST_CLOSE, TAB, TILDE, XOFF, XON,
};

/// Side-channel events extracted from the byte stream while it flows
/// past us toward the next layer. The bytes themselves still pass
/// through; these just notify the engine of state transitions worth
/// reacting to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEvent {
    /// `ESC c` — full reset (§5.7 RIS). Portal state must wipe.
    HardReset,
    /// `ESC [ ! p` — DECSTR soft reset (§5.7). Portal state must wipe.
    SoftReset,
    /// `ESC [ 6 n` — DSR cursor-position query. The inner vt100 doesn't
    /// auto-reply, so the engine synthesises `ESC [ <row> ; <col> R`
    /// from the post-process cursor position and folds it into the
    /// portal's RawReply event (§13.4).
    CursorPositionQuery,
    /// `ESC [ 2 J` — erase entire visible screen (§5.8). vt100 wipes the
    /// cells in place but doesn't push them to scrollback, so portals
    /// anchored to the live region would otherwise stay rendered on top
    /// of now-blank text. Engine drops portals whose effective anchor
    /// lies in the live region.
    EraseDisplay,
    /// `ESC [ 3 J` — xterm "Erase Saved Lines" (§5.8); wipes scrollback.
    /// Engine drops Scrollback portals whose `anchor_line` is above
    /// `top_of_live_screen`.
    EraseScrollback,
    /// `ESC [ ? 1049 h` / `ESC [ ? 47 h` — the screen swapped to the
    /// alternate grid (§5.4), so the portal scope swaps with it.
    ///
    /// Observed here rather than polled off the vt100 afterwards
    /// because the swap's *position in the stream* is what decides
    /// which scope a portal command belongs to: a `CreatePortal` that
    /// follows the swap in the same chunk belongs to the alt set.
    AltScreenEnter,
    /// `ESC [ ? 1049 l` / `ESC [ ? 47 l` — back to the main grid
    /// (§5.4). The alt set is dropped and the suspended main set
    /// resumes.
    AltScreenLeave,
}

/// Cap on CSI body length we'll buffer for matching. Long sequences past
/// this just reset the observer.
const CSI_BUF_CAP: usize = 32;

/// Default cap on a single envelope's **unstuffed** payload.
///
/// The parser buffers a body until `ESC \` closes it, so without a
/// bound a malformed or hostile stream — one that opens an envelope
/// and never closes it — makes the terminal allocate without limit.
/// Over-cap bodies are dropped and the stream resynchronises at the
/// envelope's end.
///
/// Four times the recommended `max_write_bytes` (1 MiB, §12);
/// WritePortal is the only PRT body carrying bulk data.
pub const DEFAULT_MAX_PAYLOAD: usize = 4 * 1024 * 1024;

#[derive(Debug)]
enum State {
    /// Normal pass-through stream.
    Idle,
    /// Saw 0x1B in Idle; deciding whether it opens APC.
    EscPending,
    /// Inside `ESC _ ...`, still buffering the 3 marker bytes to decide
    /// PRT vs. other APC. `marker_buf` accumulates them.
    ApcPrefix { marker_buf: Vec<u8> },
    /// Confirmed non-PRT APC — flush everything (including ESC _ and any
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed PRT — buffer (un-stuffed) bytes until `ESC \`.
    ApcPrt { body: Vec<u8> },
    /// Saw 0x1B inside `ApcPrt`; the next byte decides escape (`1B`) vs
    /// ST close (`5C`).
    ApcPrtEsc { body: Vec<u8> },
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
    /// detect specific finalizers (DECSTR, 2J/3J). `buf` holds the
    /// parameter / intermediate bytes seen so far.
    Csi { buf: Vec<u8> },
}

pub struct ApcStream {
    state: State,
    /// Which 3-byte APC marker to extract. Defaults to the C2T marker
    /// (`PRT` uppercase) used for client-to-host commands. Use
    /// `with_marker(MARKER_T2C)` on the client side to extract the
    /// host's lowercase `prt` responses and events.
    marker: [u8; 3],
    /// Largest unstuffed body this stream will buffer.
    max_payload: usize,
    /// Envelopes dropped for exceeding `max_payload`. Read and
    /// cleared by the host so a drop can be reported rather than
    /// silently swallowing a sender's command.
    overflows: u32,
}

/// One thing the stream produced, in the order the bytes carried it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A fully-received, un-stuffed PRT payload (one per envelope).
    Payload(Vec<u8>),
    /// A VT sequence observed at this point in the stream.
    Event(TerminalEvent),
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to the next layer verbatim.
    pub passthrough: Vec<u8>,
    /// Envelopes and observed events, interleaved in stream order.
    ///
    /// The order is load-bearing, which is why this is one list and
    /// not two: an event like `AltScreenEnter` changes the scope a
    /// portal command lands in (§5.4), so an engine that dispatched
    /// every payload first and reacted to the events afterwards would
    /// file a command under the wrong screen.
    pub items: Vec<Item>,
}

impl Output {
    fn push_pass(&mut self, b: u8) {
        self.passthrough.push(b);
    }

    fn push_payload(&mut self, body: Vec<u8>) {
        self.items.push(Item::Payload(body));
    }

    fn push_event(&mut self, ev: TerminalEvent) {
        self.items.push(Item::Event(ev));
    }

    /// The payloads alone, in stream order — for callers with no
    /// screen state of their own to keep in step (probes, clients).
    pub fn payloads(&self) -> impl Iterator<Item = &[u8]> {
        self.items.iter().filter_map(|i| match i {
            Item::Payload(p) => Some(p.as_slice()),
            Item::Event(_) => None,
        })
    }

    /// Owned variant of [`Self::payloads`].
    pub fn into_payloads(self) -> impl Iterator<Item = Vec<u8>> {
        self.items.into_iter().filter_map(|i| match i {
            Item::Payload(p) => Some(p),
            Item::Event(_) => None,
        })
    }

    /// The observed events alone, in stream order.
    pub fn events(&self) -> impl Iterator<Item = TerminalEvent> + '_ {
        self.items.iter().filter_map(|i| match i {
            Item::Event(ev) => Some(*ev),
            Item::Payload(_) => None,
        })
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

    pub fn feed(&mut self, input: &[u8]) -> Output {
        let mut out = Output::default();
        for &b in input {
            self.step(b, &mut out);
        }
        out
    }

    /// Drain a deferred lone ESC (state `EscPending`) and return it as a
    /// single-byte `Vec`. Other states — mid-envelope, mid-CSI, etc. —
    /// are left alone because their bodies must arrive in full.
    ///
    /// Callers should invoke this when the input source has been idle
    /// long enough that a buffered ESC is unambiguously a lone keystroke
    /// rather than the leading byte of an in-flight ESC-sequence. With
    /// no flush, a lone ESC sits in `EscPending` until the next byte
    /// arrives — which, for an interactive terminal, can mean a modal
    /// dismiss key apparently does nothing.
    pub fn flush_pending_esc(&mut self) -> Vec<u8> {
        if matches!(self.state, State::EscPending) {
            self.state = State::Idle;
            vec![ESC]
        } else {
            Vec::new()
        }
    }

    fn step(&mut self, b: u8, out: &mut Output) {
        // Move out the current state so we can rebuild it without fighting
        // the borrow checker on owned `Vec<u8>` body buffers.
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
                    out.push_pass(ESC);
                    out.push_pass(b'[');
                    State::Csi {
                        buf: Vec::with_capacity(8),
                    }
                }
                b'c' => {
                    out.push_pass(ESC);
                    out.push_pass(b'c');
                    out.push_event(TerminalEvent::HardReset);
                    State::Idle
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
            State::ApcPrefix { mut marker_buf } => {
                marker_buf.push(b);
                if marker_buf.len() < 3 {
                    State::ApcPrefix { marker_buf }
                } else if marker_buf.as_slice() == self.marker {
                    State::ApcPrt { body: Vec::new() }
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
            State::ApcPrt { mut body } => {
                if b == ESC {
                    State::ApcPrtEsc { body }
                } else if body.len() >= self.max_payload {
                    // Drop what we have and swallow the rest of the
                    // envelope. Passing the partial body through
                    // would spray binary at the vt100.
                    self.overflows = self.overflows.saturating_add(1);
                    State::ApcOverflow
                } else {
                    body.push(b);
                    State::ApcPrt { body }
                }
            }
            State::ApcPrtEsc { mut body } => {
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
                        body.push(ESC);
                        State::ApcPrt { body }
                    }
                    ST_CLOSE => {
                        out.push_payload(body);
                        State::Idle
                    }
                    ESC_MARK_TILDE => {
                        body.push(TILDE);
                        State::ApcPrt { body }
                    }
                    ESC_MARK_XON => {
                        body.push(XON);
                        State::ApcPrt { body }
                    }
                    ESC_MARK_XOFF => {
                        body.push(XOFF);
                        State::ApcPrt { body }
                    }
                    ESC_MARK_TAB => {
                        body.push(TAB);
                        State::ApcPrt { body }
                    }
                    ESC_MARK_LF => {
                        body.push(LF);
                        State::ApcPrt { body }
                    }
                    ESC_MARK_CR => {
                        body.push(CR);
                        State::ApcPrt { body }
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
                if (0x40..=0x7E).contains(&b) {
                    if buf.as_slice() == b"!" && b == b'p' {
                        out.push_event(TerminalEvent::SoftReset);
                    }
                    // DSR cursor-position query is `ESC [ 6 n`.
                    if buf.as_slice() == b"6" && b == b'n' {
                        out.push_event(TerminalEvent::CursorPositionQuery);
                    }
                    if b == b'J' && buf.as_slice() == b"2" {
                        out.push_event(TerminalEvent::EraseDisplay);
                    }
                    if b == b'J' && buf.as_slice() == b"3" {
                        out.push_event(TerminalEvent::EraseScrollback);
                    }
                    if let Some(ev) = alt_screen_event(&buf, b) {
                        out.push_event(ev);
                    }
                    State::Idle
                } else {
                    buf.push(b);
                    if buf.len() > CSI_BUF_CAP {
                        State::Csi { buf: Vec::new() }
                    } else {
                        State::Csi { buf }
                    }
                }
            }
        };
    }
}

/// Classify a completed CSI as an alt-screen swap, given its
/// parameter bytes and final byte.
///
/// Only the two mode numbers the screen model actually implements
/// count: DECSET/DECRST `1049` (save-cursor + switch, what every
/// full-screen program uses) and the bare `47`. `1047` is deliberately
/// absent — the vt100 in this tree ignores it, and an observer that
/// disagreed with the screen it is shadowing would swap the portal
/// scope under a screen that never moved.
fn alt_screen_event(params: &[u8], final_byte: u8) -> Option<TerminalEvent> {
    if final_byte != b'h' && final_byte != b'l' {
        return None;
    }
    // DEC private modes only: `ESC [ ? … h`.
    let params = params.strip_prefix(b"?")?;
    // A DECSET may carry several modes at once (`?1049;1002h`).
    let hit = params
        .split(|&c| c == b';')
        .any(|p| p == b"1049" || p == b"47");
    if !hit {
        return None;
    }
    Some(if final_byte == b'h' {
        TerminalEvent::AltScreenEnter
    } else {
        TerminalEvent::AltScreenLeave
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::frame::MARKER_T2C;

    /// The two category views the tests assert on. `Output` keeps one
    /// ordered list so the engine can apply payloads and events in
    /// sequence; a test that only cares about one kind takes it here.
    fn payloads(out: &Output) -> Vec<Vec<u8>> {
        out.payloads().map(<[u8]>::to_vec).collect()
    }

    fn events(out: &Output) -> Vec<TerminalEvent> {
        out.events().collect()
    }

    fn envelope(body: &[u8]) -> Vec<u8> {
        let mut v = vec![ESC, APC_OPEN, b'P', b'R', b'T'];
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
        assert_eq!(payloads(&out).len(), 1);
        assert_eq!(&payloads(&out)[0], body);
    }

    #[test]
    fn unstuffs_esc_byte() {
        let mut s = ApcStream::new();
        let body = &[0x00, 0x1B, 0xFF, 0x1B];
        let out = s.feed(&envelope(body));
        assert_eq!(payloads(&out).len(), 1);
        assert_eq!(&payloads(&out)[0], body);
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
        assert_eq!(payloads(&out).len(), 1);
        assert_eq!(&payloads(&out)[0], body);
    }

    #[test]
    fn passes_through_plain_text() {
        let mut s = ApcStream::new();
        let out = s.feed(b"hello world");
        assert_eq!(out.passthrough, b"hello world");
        assert!(payloads(&out).is_empty());
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
                out.items.extend(o.items);
            }
            assert!(
                out.passthrough.is_empty(),
                "split {split}: leaked {:?}",
                out.passthrough
            );
            assert_eq!(payloads(&out).len(), 1, "split {split}: missing payload");
            assert_eq!(&payloads(&out)[0], b"abcdef", "split {split}");
        }
    }

    #[test]
    fn vge_envelope_passes_through_for_prt_stream() {
        // ESC _ V G E ...  ESC \  — a VGE envelope must come back
        // unchanged in passthrough, so a PRT-then-VGE pipeline can pick
        // it up at the next layer.
        let mut s = ApcStream::new();
        let env = vec![
            ESC, APC_OPEN, b'V', b'G', b'E', b'a', b'b', b'c', ESC, ST_CLOSE,
        ];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(payloads(&out).is_empty());
    }

    #[test]
    fn non_prt_apc_passes_through() {
        // ESC _ G abc ESC \ (kitty graphics-style envelope)
        let mut s = ApcStream::new();
        let env = vec![ESC, APC_OPEN, b'G', b'a', b'b', b'c', ESC, ST_CLOSE];
        let out = s.feed(&env);
        assert_eq!(out.passthrough, env);
        assert!(payloads(&out).is_empty());
    }

    #[test]
    fn esc_before_normal_byte_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC, b'A']);
        assert_eq!(out.passthrough, vec![ESC, b'A']);
    }

    #[test]
    fn flush_pending_esc_emits_deferred_lone_esc() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC]);
        assert!(out.passthrough.is_empty());
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        // Idempotent — second flush has nothing to drain.
        assert!(s.flush_pending_esc().is_empty());
        // After flush, parser is back to Idle and accepts a fresh envelope.
        let out = s.feed(&envelope(b"x"));
        assert_eq!(payloads(&out), vec![b"x".to_vec()]);
    }

    #[test]
    fn flush_pending_esc_leaves_mid_envelope_alone() {
        // If we're mid-PRT-envelope, flushing must not corrupt the body.
        let mut s = ApcStream::new();
        let env = envelope(b"abc");
        let out = s.feed(&env[..env.len() - 1]); // everything but ST_CLOSE
        assert!(payloads(&out).is_empty());
        assert!(s.flush_pending_esc().is_empty());
        let out = s.feed(&env[env.len() - 1..]);
        assert_eq!(payloads(&out), vec![b"abc".to_vec()]);
    }

    #[test]
    fn back_to_back_envelopes() {
        let mut s = ApcStream::new();
        let mut buf = envelope(b"one");
        buf.extend(envelope(b"two"));
        let out = s.feed(&buf);
        assert_eq!(payloads(&out).len(), 2);
        assert_eq!(&payloads(&out)[0], b"one");
        assert_eq!(&payloads(&out)[1], b"two");
    }

    #[test]
    fn t2c_marker_extracts_lowercase_envelopes() {
        // A client-side stream uses MARKER_T2C to pick up `prt` host
        // responses; uppercase `PRT` envelopes should pass through.
        let mut s = ApcStream::with_marker(*MARKER_T2C);
        let mut t2c = vec![ESC, APC_OPEN, b'p', b'r', b't'];
        super::super::codec::stuff(b"resp", &mut t2c);
        t2c.push(ESC);
        t2c.push(ST_CLOSE);

        let c2t = envelope(b"cmd");
        let mut all = t2c.clone();
        all.extend_from_slice(&c2t);

        let out = s.feed(&all);
        assert_eq!(payloads(&out).len(), 1);
        assert_eq!(&payloads(&out)[0], b"resp");
        assert_eq!(out.passthrough, c2t);
    }

    #[test]
    fn ris_emits_hard_reset_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(&[ESC, b'c']);
        assert_eq!(out.passthrough, vec![ESC, b'c']);
        assert_eq!(events(&out), vec![TerminalEvent::HardReset]);
        assert!(payloads(&out).is_empty());
    }

    #[test]
    fn decstr_emits_soft_reset_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[!p");
        assert_eq!(out.passthrough, b"\x1b[!p");
        assert_eq!(events(&out), vec![TerminalEvent::SoftReset]);
        assert!(payloads(&out).is_empty());
    }

    #[test]
    fn dsr_cursor_query_emits_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[6n");
        assert_eq!(out.passthrough, b"\x1b[6n");
        assert_eq!(events(&out), vec![TerminalEvent::CursorPositionQuery]);
    }

    #[test]
    fn ed_2_emits_erase_display_event_and_passes_through() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[2J");
        assert_eq!(out.passthrough, b"\x1b[2J");
        assert_eq!(events(&out), vec![TerminalEvent::EraseDisplay]);
    }

    #[test]
    fn ed_3_emits_erase_scrollback_event() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[3J");
        assert_eq!(events(&out), vec![TerminalEvent::EraseScrollback]);
    }

    #[test]
    fn clear_command_sequence_emits_both_events() {
        // ncurses `clear` sends ESC[H ESC[2J ESC[3J — the engine should
        // see both EraseDisplay and EraseScrollback so it can drop every
        // portal in the current host screen.
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[H\x1b[2J\x1b[3J");
        assert_eq!(
            events(&out),
            vec![
                TerminalEvent::EraseDisplay,
                TerminalEvent::EraseScrollback,
            ]
        );
    }

    /// §5.4 — the alt-screen swap has to be reported *where it
    /// happens*, because it decides which portal scope the commands
    /// around it belong to.
    #[test]
    fn alt_screen_swaps_are_observed_in_stream_order() {
        let mut s = ApcStream::new();
        let mut input = b"\x1b[?1049h".to_vec();
        input.extend(envelope(b"cmd"));
        input.extend_from_slice(b"\x1b[?1049l");
        let out = s.feed(&input);
        assert_eq!(out.passthrough, b"\x1b[?1049h\x1b[?1049l");
        assert_eq!(
            out.items,
            vec![
                Item::Event(TerminalEvent::AltScreenEnter),
                Item::Payload(b"cmd".to_vec()),
                Item::Event(TerminalEvent::AltScreenLeave),
            ]
        );
    }

    #[test]
    fn alt_screen_swap_spellings() {
        let mut s = ApcStream::new();
        // The bare `47` form, and a DECSET carrying several modes.
        assert_eq!(events(&s.feed(b"\x1b[?47h")), vec![TerminalEvent::AltScreenEnter]);
        assert_eq!(events(&s.feed(b"\x1b[?47l")), vec![TerminalEvent::AltScreenLeave]);
        assert_eq!(
            events(&s.feed(b"\x1b[?1049;1002h")),
            vec![TerminalEvent::AltScreenEnter]
        );
        // Other private modes are not swaps…
        assert!(events(&s.feed(b"\x1b[?1002h")).is_empty());
        assert!(events(&s.feed(b"\x1b[?25l")).is_empty());
        // …nor is `1047`, which the screen model this shadows ignores:
        // reporting a swap the screen never makes would move the
        // portal scope out from under it.
        assert!(events(&s.feed(b"\x1b[?1047h")).is_empty());
        // Non-private `h`/`l` (ANSI modes) are a different namespace.
        assert!(events(&s.feed(b"\x1b[4h")).is_empty());
        // And the sequence still reaches the screen either way.
        assert_eq!(s.feed(b"\x1b[?1049h").passthrough, b"\x1b[?1049h");
    }

    #[test]
    fn alt_screen_swap_split_across_chunks() {
        let mut s = ApcStream::new();
        let mut all = Output::default();
        for chunk in [&b"\x1b[?10"[..], &b"49h"[..]] {
            let o = s.feed(chunk);
            all.passthrough.extend(o.passthrough);
            all.items.extend(o.items);
        }
        assert_eq!(all.passthrough, b"\x1b[?1049h");
        assert_eq!(events(&all), vec![TerminalEvent::AltScreenEnter]);
    }

    #[test]
    fn ed_partial_does_not_emit_erase_display() {
        // ESC[J / ESC[0J / ESC[1J are partial erases (cursor-relative).
        let mut s = ApcStream::new();
        assert!(s.feed(b"\x1b[J").events().next().is_none());
        assert!(s.feed(b"\x1b[0J").events().next().is_none());
        assert!(s.feed(b"\x1b[1J").events().next().is_none());
    }

    #[test]
    fn ris_split_across_chunks() {
        let mut s = ApcStream::new();
        let mut all = Output::default();
        for chunk in &[&b"\x1b"[..], &b"c"[..]] {
            let o = s.feed(chunk);
            all.passthrough.extend(o.passthrough);
            all.items.extend(o.items);
        }
        assert_eq!(all.passthrough, b"\x1bc");
        assert_eq!(events(&all), vec![TerminalEvent::HardReset]);
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
                    payloads(&out),
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
        assert_eq!(payloads(&out), vec![b"after".to_vec()], "resync failed");
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
        assert_eq!(payloads(&out), vec![body]);
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
        assert_eq!(payloads(&out), vec![b"ok".to_vec()]);
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
                assert!(out.passthrough.is_empty(), "cut {cut}: leaked text");
                payloads.extend(out.into_payloads());
            }
            assert_eq!(payloads, vec![b"tail".to_vec()], "cut {cut}");
            assert_eq!(s.take_overflows(), 1, "cut {cut}");
        }
    }
}
