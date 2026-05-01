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
// Non-VGE APC sequences (e.g. iTerm-style `ESC _G...`) pass through verbatim
// so the underlying VT parser can still handle them. A VGE envelope is
// recognized by the 3-byte uppercase `VGE` marker that follows `ESC _`
// (§1.1: lowercase `vge` is the terminal-to-client direction we never
// receive, so we never match it here).

use super::frame::{APC_OPEN, ESC, MARKER_C2T, ST_CLOSE};

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
                    out.events.push(TerminalEvent::HardReset);
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
            State::Csi { mut buf } => {
                out.push_pass(b);
                // Final byte? CSI finals are 0x40..=0x7E.
                if (0x40..=0x7E).contains(&b) {
                    // DECSTR is `ESC [ ! p`.
                    if buf.as_slice() == b"!" && b == b'p' {
                        out.events.push(TerminalEvent::SoftReset);
                    }
                    // DSR cursor-position query is `ESC [ 6 n`.
                    if buf.as_slice() == b"6" && b == b'n' {
                        out.events.push(TerminalEvent::CursorPositionQuery);
                    }
                    // Erase In Display:
                    //   `ESC [ 2 J` — wipe live region.
                    //   `ESC [ 3 J` — wipe scrollback.
                    if b == b'J' && buf.as_slice() == b"2" {
                        out.events.push(TerminalEvent::EraseDisplay);
                    }
                    if b == b'J' && buf.as_slice() == b"3" {
                        out.events.push(TerminalEvent::EraseScrollback);
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
}
