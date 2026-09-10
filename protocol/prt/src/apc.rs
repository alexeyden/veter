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
    /// `ESC [ ? 6 n` — DECXCPR, the DEC-private spelling of the same
    /// question, answered `ESC [ ? <row> ; <col> R` and folded into
    /// the portal's RawReply the same way (§13.4).
    ExtendedCursorPositionQuery,
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
    /// PRT vs. other APC. They accumulate in `marker_buf`.
    ApcPrefix,
    /// Confirmed non-PRT APC — flush everything (including ESC _ and any
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed PRT — buffer (un-stuffed) bytes into `body` until `ESC \`.
    ApcPrt,
    /// Saw 0x1B inside `ApcPrt`; the next byte decides escape (`1B`) vs
    /// ST close (`5C`).
    ApcPrtEsc,
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
    /// detect specific finalizers (DECSTR, 2J/3J). `csi` holds the
    /// parameter / intermediate bytes seen so far.
    Csi,
}

impl State {
    /// States in which every byte up to the next ESC is handled the
    /// same way — copied to passthrough, appended to the body, or
    /// dropped — so `feed` can take the whole run in one call.
    fn bulkable(self) -> bool {
        matches!(
            self,
            State::Idle | State::ApcOther | State::ApcPrt | State::ApcOverflow
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
    /// Un-stuffed body of the envelope in flight (`ApcPrt` /
    /// `ApcPrtEsc`), empty otherwise.
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
    /// Parameter / intermediate bytes of the CSI being observed.
    csi: Vec<u8>,
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
            body: Vec::new(),
            marker_buf: [0; 3],
            marker_len: 0,
            recover_flushed: None,
            csi: Vec::new(),
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
            csi: Vec::new(),
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
            // state machine per byte. A `WritePortal` flood is almost
            // entirely this path — the per-byte loop it replaces was
            // the host's throughput ceiling under one.
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
            // rather than passed through, until `ESC \` resyncs us.
            State::ApcOverflow => {}
            State::ApcPrt => {
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
                b'[' => {
                    out.push_pass(ESC);
                    out.push_pass(b'[');
                    self.csi.clear();
                    State::Csi
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
                    State::ApcPrt
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
                    State::ApcPrt
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
            State::ApcPrt => {
                if b == ESC {
                    State::ApcPrtEsc
                } else if self.body.len() >= self.max_payload {
                    self.overflow()
                } else {
                    self.body.push(b);
                    State::ApcPrt
                }
            }
            State::ApcPrtEsc => {
                // The cap has to be enforced here as well as on the
                // plain-byte path. Every byte of a body made entirely
                // of stuffed escapes arrives through this arm, so
                // checking only there let an all-`ESC ESC` stream
                // buffer without bound — the exact shape a hostile
                // sender would use. `ST_CLOSE` is exempt: it completes
                // the envelope rather than appending to it.
                if b != ST_CLOSE && self.body.len() >= self.max_payload {
                    self.overflow()
                } else {
                    match b {
                        ESC => {
                            self.body.push(ESC);
                            State::ApcPrt
                        }
                        ST_CLOSE => {
                            out.push_payload(std::mem::take(&mut self.body));
                            State::Idle
                        }
                        ESC_MARK_TILDE => {
                            self.body.push(TILDE);
                            State::ApcPrt
                        }
                        ESC_MARK_XON => {
                            self.body.push(XON);
                            State::ApcPrt
                        }
                        ESC_MARK_XOFF => {
                            self.body.push(XOFF);
                            State::ApcPrt
                        }
                        ESC_MARK_TAB => {
                            self.body.push(TAB);
                            State::ApcPrt
                        }
                        ESC_MARK_LF => {
                            self.body.push(LF);
                            State::ApcPrt
                        }
                        ESC_MARK_CR => {
                            self.body.push(CR);
                            State::ApcPrt
                        }
                        _ => {
                            // Only the byte-stuffing escapes (ESC-double, the
                            // transport marks) or ST close are valid inside the
                            // envelope. Treat anything else as a malformed envelope:
                            // discard the partial body, emit the stray ESC + byte to
                            // passthrough, and resync.
                            self.body.clear();
                            out.push_pass(ESC);
                            out.push_pass(b);
                            State::Idle
                        }
                    }
                }
            }
            State::Csi => {
                out.push_pass(b);
                if (0x40..=0x7E).contains(&b) {
                    if self.csi.as_slice() == b"!" && b == b'p' {
                        out.push_event(TerminalEvent::SoftReset);
                    }
                    // DSR cursor-position query is `ESC [ 6 n`;
                    // DECXCPR is the same with a DEC-private `?`.
                    if self.csi.as_slice() == b"6" && b == b'n' {
                        out.push_event(TerminalEvent::CursorPositionQuery);
                    }
                    if self.csi.as_slice() == b"?6" && b == b'n' {
                        out.push_event(TerminalEvent::ExtendedCursorPositionQuery);
                    }
                    if b == b'J' && self.csi.as_slice() == b"2" {
                        out.push_event(TerminalEvent::EraseDisplay);
                    }
                    if b == b'J' && self.csi.as_slice() == b"3" {
                        out.push_event(TerminalEvent::EraseScrollback);
                    }
                    if let Some(ev) = alt_screen_event(&self.csi, b) {
                        out.push_event(ev);
                    }
                    State::Idle
                } else {
                    self.csi.push(b);
                    if self.csi.len() > CSI_BUF_CAP {
                        self.csi.clear();
                    }
                    State::Csi
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
    use super::super::envelope::wrap_c2t_envelope;
    use super::super::frame::MARKER_T2C;


    /// Every byte of `input`, fed one at a time, must produce exactly
    /// what feeding it in one call produces. This is the invariant the
    /// bulk path in `feed` rests on: it consumes a whole run of
    /// non-ESC bytes at once, so any divergence between the two — a
    /// cap enforced a byte late, a body not cleared on resync, a
    /// passthrough run merged where it should have split — shows up
    /// here.
    fn assert_bulk_matches_per_byte(input: &[u8], max_payload: usize) {
        let mut bulk = ApcStream::new().with_max_payload(max_payload);
        let whole = bulk.feed(input);

        // Against the per-byte state machine itself.
        let mut slow = ApcStream::new().with_max_payload(max_payload);
        let stepwise = slow.feed_stepwise(input);
        assert_eq!(whole.passthrough, stepwise.passthrough, "passthrough differs");
        assert_eq!(whole.items, stepwise.items, "items differ");
        assert_eq!(
            bulk.take_overflows(),
            slow.take_overflows(),
            "overflow count differs"
        );

        // And against the same bytes arriving one read at a time, which
        // is the other way a run gets split — by the PTY, not by us.
        let mut split = ApcStream::new().with_max_payload(max_payload);
        let mut pass = Vec::new();
        let mut items = Vec::new();
        for &b in input {
            let out = split.feed(&[b]);
            pass.extend_from_slice(&out.passthrough);
            items.extend(out.items);
        }
        assert_eq!(whole.passthrough, pass, "passthrough differs across reads");
        assert_eq!(whole.items, items, "items differ across reads");
    }

    /// A stream with a bit of everything the parser branches on, built
    /// deterministically so a failure is reproducible.
    fn mixed_stream() -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(b"plain text before\r\n");
        // A real envelope, stuffed.
        s.extend_from_slice(&wrap_c2t_envelope(b"payload one\x1b\x07\r\n~"));
        // Someone else's APC — passes through verbatim.
        s.extend_from_slice(b"\x1b_VGEnot ours\x1b\\");
        // Observed control sequences.
        s.extend_from_slice(b"\x1b[2J\x1b[3J\x1b[!p\x1b[6n\x1b[?1049h\x1b[?1049l\x1bc");
        // A lone ESC followed by something that is not a sequence.
        s.extend_from_slice(b"\x1bZ");
        // Binary body carrying every byte value, transport marks
        // included, so the stuffing escapes land mid-run.
        let body: Vec<u8> = (0u16..512).map(|i| (i % 256) as u8).collect();
        s.extend_from_slice(&wrap_c2t_envelope(&body));
        s.extend_from_slice(b"tail\r\n");
        s
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree() {
        assert_bulk_matches_per_byte(&mixed_stream(), DEFAULT_MAX_PAYLOAD);
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree_at_the_payload_cap() {
        // Caps small enough that the bulk path's "does this run cross
        // it?" test and the per-byte path's "is the body full?" test
        // have to reach the same verdict — including a body landing
        // exactly on the cap, which is allowed, and one byte past,
        // which is not.
        let stream = mixed_stream();
        for cap in [0, 1, 16, 511, 512, 513, 1024] {
            assert_bulk_matches_per_byte(&stream, cap);
        }
        // The boundary itself. Hand-built rather than wrapped, because
        // what matters is the exact *body* length: 512 bytes with
        // nothing to stuff, so the whole payload reaches the bulk path
        // as one run and the cap has to be decided there, the same way
        // the per-byte arm decides it. 512 under a 512-byte cap is a
        // payload; the same body under 511 is an overflow.
        let mut clean = Vec::from(&b"\x1b_PRT"[..]);
        clean.resize(clean.len() + 512, b'A');
        clean.extend_from_slice(b"\x1b\\");
        for cap in [511, 512, 513] {
            assert_bulk_matches_per_byte(&clean, cap);
        }
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree_on_a_malformed_escape() {
        // `ESC q` inside an envelope is not a valid stuffing escape:
        // the partial body is discarded and the stream resyncs. The
        // body buffer now outlives the state that held it, so a
        // missing `clear()` would leak those bytes into the *next*
        // envelope — visible here as a payload mismatch.
        let mut s = Vec::new();
        s.extend_from_slice(b"\x1b_PRTleading body\x1bq trailing\x1b\\");
        s.extend_from_slice(&wrap_c2t_envelope(b"the next one"));
        assert_bulk_matches_per_byte(&s, DEFAULT_MAX_PAYLOAD);
    }

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

    /// DECXCPR (`ESC [ ? 6 n`) is DSR with a DEC-private `?`, and the
    /// two must not be confused: their replies differ by that `?`.
    #[test]
    fn decxcpr_and_dsr_are_told_apart() {
        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[?6n");
        assert_eq!(events(&out), vec![TerminalEvent::ExtendedCursorPositionQuery]);
        assert_eq!(out.passthrough, b"\x1b[?6n");

        let mut s = ApcStream::new();
        let out = s.feed(b"\x1b[6n");
        assert_eq!(events(&out), vec![TerminalEvent::CursorPositionQuery]);
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
    /// `ESC _ PRT … ESC \\`, the shape that gets split.
    fn split_test_envelope() -> Vec<u8> {
        wrap_c2t_envelope(b"body")
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
        assert_eq!(out.payloads().count(), 1, "envelope lost after the flush");
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

    /// Esc, a pause, then `_` and ordinary text — `Esc _` is a motion
    /// in vim, so it is a real thing to type. The `_` must come out as
    /// itself, the ESC must not be doubled (the flush already
    /// delivered it), and the parser must not be left hunting for a
    /// terminator that was never coming.
    #[test]
    fn typed_underscore_after_a_flushed_esc_is_not_an_envelope() {
        let mut s = ApcStream::new();
        s.feed(&[ESC]);
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        assert_eq!(s.feed(b"_abcdef").passthrough, b"_abcdef");
        // And still usable afterwards.
        let out = s.feed(&split_test_envelope());
        assert_eq!(out.payloads().count(), 1);
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
        assert_eq!(out.payloads().count(), 1);
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
        assert_eq!(out.payloads().count(), 1, "envelope lost after two idle flushes");
        assert!(
            out.passthrough.is_empty(),
            "envelope leaked as input: {:?}",
            String::from_utf8_lossy(&out.passthrough)
        );
    }

    /// Esc, then `_`, both landing before any idle window elapses —
    /// two ordinary keystrokes at a rate a person types at. The pair
    /// reaches `ApcPrefix`, which used to have no flush arm at all, so
    /// both bytes sat there until three more arrived. Raising the
    /// escape-time is what made this reachable: with a short one the
    /// ESC flushed before the `_` landed and the recover path carried
    /// it, which is why the bug hid behind the fix for the split.
    #[test]
    fn typed_esc_underscore_is_not_swallowed() {
        let mut s = ApcStream::new();
        assert!(s.feed(&[ESC]).passthrough.is_empty());
        assert!(s.feed(b"_").passthrough.is_empty());
        // One window: the ESC, as for any deferred lone Esc.
        assert_eq!(s.flush_pending_esc(), vec![ESC]);
        // A second with still no marker: the `_` follows it.
        assert_eq!(s.flush_pending_esc(), vec![APC_OPEN]);
        assert_eq!(s.flush_pending_esc(), Vec::<u8>::new());
    }

    /// The same flush, but the bytes really were an envelope whose
    /// marker was late. Releasing the ESC out of `ApcPrefix` must not
    /// cost the envelope — `RecoverPrefix` picks it up exactly as it
    /// does for a flush out of `EscPending`.
    #[test]
    fn a_split_envelope_survives_a_flush_from_apc_prefix() {
        let env = split_test_envelope();
        let mut s = ApcStream::new();

        assert!(s.feed(&env[..2]).passthrough.is_empty()); // `ESC _`
        assert_eq!(s.flush_pending_esc(), vec![ESC]);

        let out = s.feed(&env[2..]);
        assert_eq!(out.payloads().count(), 1, "envelope lost after the flush");
        assert!(
            out.passthrough.is_empty(),
            "envelope leaked as input: {:?}",
            String::from_utf8_lossy(&out.passthrough)
        );
    }

    /// What a caller arms its escape-time timer on: true exactly when
    /// a flush would hand something over, so the timer starts when
    /// bytes are first held and stops once they are all released.
    #[test]
    fn has_deferred_bytes_tracks_what_a_flush_would_release() {
        let mut s = ApcStream::new();
        assert!(!s.has_deferred_bytes(), "idle");

        s.feed(&[ESC]);
        assert!(s.has_deferred_bytes(), "lone ESC");
        s.feed(b"_");
        assert!(s.has_deferred_bytes(), "ESC _");
        s.flush_pending_esc();
        assert!(s.has_deferred_bytes(), "the `_` is still owed");
        s.flush_pending_esc();
        assert!(!s.has_deferred_bytes(), "everything released");

        // A marker byte arriving mid-recovery is owed again.
        s.feed(b"P");
        assert!(s.has_deferred_bytes(), "marker byte held");
        s.flush_pending_esc();
        assert!(!s.has_deferred_bytes());

        // Mid-envelope is not a deferral: those bytes are payload, and
        // no timer should be waiting on them.
        let mut s = ApcStream::new();
        let env = split_test_envelope();
        s.feed(&env[..env.len() - 1]);
        assert!(!s.has_deferred_bytes(), "mid-envelope");
    }

    /// Someone else's envelope, split the same way, running through a
    /// chain of parsers like the one a multiplexer keeps. The first
    /// parser hands on `_` and the marker *without* an ESC, because
    /// the parser behind it was handed that same ESC by the same
    /// flush and is itself still waiting to see what it opened.
    #[test]
    fn a_foreign_split_envelope_is_recovered_by_the_next_parser() {
        const FOREIGN: &[u8; 3] = b"xyz";
        let mut env = vec![ESC, APC_OPEN];
        env.extend_from_slice(FOREIGN);
        env.extend_from_slice(b"body");
        env.extend_from_slice(&[ESC, ST_CLOSE]);

        let mut first = ApcStream::new();
        let mut second = ApcStream::with_marker(*FOREIGN);

        first.feed(&env[..1]);
        second.feed(&first.flush_pending_esc());
        assert_eq!(second.flush_pending_esc(), vec![ESC]);

        let first_out = first.feed(&env[1..]);
        let second_out = second.feed(&first_out.passthrough);
        assert_eq!(second_out.payloads().count(), 1, "foreign envelope lost in the chain");
        assert!(second_out.passthrough.is_empty());
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
        assert_eq!(out.payloads().count(), 1, "envelope lost after a typed `Esc _`");
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
        assert_eq!(out.payloads().count(), 1);
        assert!(out.passthrough.is_empty());
    }
}
