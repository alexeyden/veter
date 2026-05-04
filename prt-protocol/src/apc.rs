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

use super::frame::{APC_OPEN, ESC, MARKER_C2T, ST_CLOSE};

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
}

/// Cap on CSI body length we'll buffer for matching. Long sequences past
/// this just reset the observer.
const CSI_BUF_CAP: usize = 32;

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
}

#[derive(Default)]
pub struct Output {
    /// Bytes that should go to the next layer verbatim.
    pub passthrough: Vec<u8>,
    /// Fully-received, un-stuffed PRT payloads (one per envelope).
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
                    out.events.push(TerminalEvent::HardReset);
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
                } else {
                    body.push(b);
                    State::ApcPrt { body }
                }
            }
            State::ApcPrtEsc { mut body } => match b {
                ESC => {
                    body.push(ESC);
                    State::ApcPrt { body }
                }
                ST_CLOSE => {
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
                if (0x40..=0x7E).contains(&b) {
                    if buf.as_slice() == b"!" && b == b'p' {
                        out.events.push(TerminalEvent::SoftReset);
                    }
                    // DSR cursor-position query is `ESC [ 6 n`.
                    if buf.as_slice() == b"6" && b == b'n' {
                        out.events.push(TerminalEvent::CursorPositionQuery);
                    }
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
    use super::super::frame::MARKER_T2C;

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
        assert!(out.payloads.is_empty());
    }

    #[test]
    fn non_prt_apc_passes_through() {
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
        assert_eq!(out.payloads.len(), 1);
        assert_eq!(&out.payloads[0], b"resp");
        assert_eq!(out.passthrough, c2t);
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
        // ncurses `clear` sends ESC[H ESC[2J ESC[3J — the engine should
        // see both EraseDisplay and EraseScrollback so it can drop every
        // portal in the current host screen.
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[H\x1b[2J\x1b[3J");
        assert_eq!(
            out.events,
            vec![
                TerminalEvent::EraseDisplay,
                TerminalEvent::EraseScrollback,
            ]
        );
    }

    #[test]
    fn ed_partial_does_not_emit_erase_display() {
        // ESC[J / ESC[0J / ESC[1J are partial erases (cursor-relative).
        let mut s = ApcStream::new();
        assert!(s.feed(b"\x1b[J").events.is_empty());
        assert!(s.feed(b"\x1b[0J").events.is_empty());
        assert!(s.feed(b"\x1b[1J").events.is_empty());
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
}
