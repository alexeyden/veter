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
    /// `ESC [ ? 6 n` — DECXCPR, the DEC-private spelling of the same
    /// question. Answered `ESC [ ? <row> ; <col> R`, with the `?` the
    /// sender is matching on: a client that asked the private form and
    /// got the plain reply has to guess whether the terminal answered
    /// it or something else did.
    ExtendedCursorPositionQuery,
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
    /// `ESC [ c` / `ESC [ 0 c` — DA1, primary device attributes. Like
    /// DSR, vt100 parses it and replies to nothing, so the engine
    /// answers. Unlike DSR this one is not optional in practice: vim,
    /// neovim and tmux send DA1 at startup and *block* on the answer
    /// until their own timeout, so a terminal that stays silent costs
    /// every one of them a visible stall on every launch.
    DeviceAttributes1,
    /// `ESC [ > c` / `ESC [ > 0 c` — DA2, secondary device attributes:
    /// "which terminal are you, and what version".
    DeviceAttributes2,
    /// `ESC [ > q` / `ESC [ > 0 q` — XTVERSION, the modern spelling of
    /// the same question, answered with a name rather than a number.
    XtVersion,
    /// `ESC [ ? Ps $ p` (private) or `ESC [ Ps $ p` (ANSI) — DECRQM,
    /// "is mode `Ps` set?". Answered from the vt100's own mode state,
    /// or as "not recognised" for a mode the screen doesn't track —
    /// which is still an answer, and still unblocks the sender.
    ModeQuery { private: bool, mode: u16 },
    /// `ESC [ ? u` — the kitty keyboard protocol's "which progressive
    /// enhancement flags are in effect?". Answered from the vt100's own
    /// per-screen flag stack, which is where the `CSI > … u` pushes
    /// that set them landed.
    ///
    /// The bare `ESC [ u` spelling is *not* this: with no parameters
    /// and no private prefix it is SCORC, the ANSI.SYS cursor restore,
    /// which is why the match below is on the `?` and not on the final
    /// byte alone.
    KeyboardFlagsQuery,
}

/// Cap on CSI body length we'll buffer for matching. Long sequences
/// (mostly mode set/reset chains) past this just reset the observer.
const CSI_BUF_CAP: usize = 32;

/// Match a completed CSI — `params` is everything between `ESC [` and
/// the final byte `final_byte` — against the terminal queries nothing
/// in this stack used to answer.
///
/// Kept separate from the reset/erase matches above because these four
/// share a shape: the sender is *waiting* for a reply, so failing to
/// recognise one costs a stall rather than a missed repaint.
fn query_event(params: &[u8], final_byte: u8) -> Option<TerminalEvent> {
    match final_byte {
        // DA1 `CSI c` / `CSI 0 c`; DA2 `CSI > c` / `CSI > 0 c`.
        b'c' => match params {
            b"" | b"0" => Some(TerminalEvent::DeviceAttributes1),
            b">" | b">0" => Some(TerminalEvent::DeviceAttributes2),
            _ => None,
        },
        // XTVERSION `CSI > q` / `CSI > 0 q`. The `>` is what separates
        // it from DECLL (`CSI Ps q`) and DECSCUSR (`CSI Ps SP q`).
        b'q' => match params {
            b">" | b">0" => Some(TerminalEvent::XtVersion),
            _ => None,
        },
        // DECRQM `CSI ? Ps $ p` / `CSI Ps $ p`. The `$` intermediate
        // is what separates it from DECSTR (`CSI ! p`).
        b'p' => {
            let rest = params.strip_suffix(b"$")?;
            let (private, digits) = match rest.strip_prefix(b"?") {
                Some(d) => (true, d),
                None => (false, rest),
            };
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            // A mode number past u16 can't be one we know; treat the
            // sequence as unrecognised rather than wrapping it.
            let mode: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
            Some(TerminalEvent::ModeQuery { private, mode })
        }
        // The kitty keyboard flags query `CSI ? u`. The private prefix
        // carries the whole distinction from SCORC (`CSI u`), so an
        // empty parameter string must not match here.
        b'u' => match params {
            b"?" => Some(TerminalEvent::KeyboardFlagsQuery),
            _ => None,
        },
        _ => None,
    }
}

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

/// Parser state.
///
/// `Copy` on purpose: the body of the envelope in flight lives in
/// `ApcStream::body`, not in the variant. Keeping the `Vec` out of the
/// enum is what lets `step` be a branch instead of a move of the whole
/// state, and what lets `feed_segments` consume a whole run of bytes at
/// once.
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
    /// VGE vs. other APC. They accumulate in `marker_buf`.
    ApcPrefix,
    /// Confirmed non-VGE APC — flush everything (including ESC _ and any
    /// already-consumed marker bytes) to passthrough until ST.
    ApcOther,
    /// Confirmed VGE — buffer (un-stuffed) bytes until `ESC \`.
    ApcVge,
    /// Saw 0x1B inside `ApcVge`; the next byte decides escape (`1B`) vs ST
    /// close (`5C`).
    ApcVgeEsc,
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
    /// detect specific finalizers (DECSTR right now). `csi` holds the
    /// parameter / intermediate bytes seen so far.
    Csi,
}

impl State {
    /// States in which every byte up to the next ESC is handled the
    /// same way — copied to passthrough, appended to the body, or
    /// dropped — so `feed_segments` can take the whole run in one call.
    fn bulkable(self) -> bool {
        matches!(
            self,
            State::Idle | State::ApcOther | State::ApcVge | State::ApcOverflow
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
    /// Parameter / intermediate bytes of the CSI being observed.
    csi: Vec<u8>,
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

    /// Bulk form of [`Self::push_pass`] for a whole ESC-free run.
    fn push_pass_slice(&mut self, bytes: &[u8]) {
        match self.segments.last_mut() {
            Some(Segment::Pass(v)) => v.extend_from_slice(bytes),
            _ => self.segments.push(Segment::Pass(bytes.to_vec())),
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

    /// Split `input` into ordered segments. This is the form the
    /// terminal stage — the engine sitting immediately before the
    /// vt100 — consumes, so it can interleave `parser.process` with
    /// command application exactly as the sender wrote them.
    pub fn feed_segments(&mut self, input: &[u8]) -> Vec<Segment> {
        let mut sink = SegmentSink::default();
        let mut i = 0;
        while i < input.len() {
            // Bulk path: hand the whole run of bytes before the next
            // ESC to one `extend_from_slice` instead of stepping the
            // state machine per byte. A client streaming an image, or a
            // pane dumping text, is almost entirely this path — the
            // per-byte loop it replaces was the host's throughput
            // ceiling under one.
            if self.state.bulkable() {
                let rest = &input[i..];
                let run = next_esc(rest).unwrap_or(rest.len());
                if run > 0 {
                    self.bulk(&rest[..run], &mut sink);
                    i += run;
                    continue;
                }
            }
            self.step(input[i], &mut sink);
            i += 1;
        }
        sink.segments
    }

    /// Consume `run` — guaranteed ESC-free — in a [`State::bulkable`]
    /// state.
    fn bulk(&mut self, run: &[u8], out: &mut SegmentSink) {
        match self.state {
            State::Idle | State::ApcOther => out.push_pass_slice(run),
            // Over-cap body: these bytes are envelope payload, dropped
            // rather than passed through, until `ESC \` resyncs us.
            State::ApcOverflow => {}
            State::ApcVge => {
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

    /// Reference implementation for the tests: one `step` per byte,
    /// with no bulk path. `feed_segments` must agree with this
    /// exactly — that is the whole contract of the run-at-a-time scan.
    #[cfg(test)]
    fn feed_segments_stepwise(&mut self, input: &[u8]) -> Vec<Segment> {
        let mut sink = SegmentSink::default();
        for &b in input {
            self.step(b, &mut sink);
        }
        sink.segments
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

    fn step(&mut self, b: u8, out: &mut SegmentSink) {
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
                    // CSI start — ESC + [ go to vt100, we observe the
                    // body for DECSTR.
                    out.push_pass(ESC);
                    out.push_pass(b'[');
                    self.csi.clear();
                    State::Csi
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
                    State::ApcVge
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
                    out.push_pass_slice(&self.marker_buf[already..]);
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
                    State::ApcVge
                } else {
                    // Not a VGE envelope — flush ESC _ <marker_buf> to
                    // passthrough and continue treating the rest as
                    // verbatim until ST.
                    out.push_pass(ESC);
                    out.push_pass(APC_OPEN);
                    out.push_pass_slice(&self.marker_buf);
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
            State::ApcVge => {
                if b == ESC {
                    State::ApcVgeEsc
                } else if self.body.len() >= self.max_payload {
                    self.overflow()
                } else {
                    self.body.push(b);
                    State::ApcVge
                }
            }
            State::ApcVgeEsc => {
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
                        // Stuffed 0x1B — store one literal ESC.
                        self.body.push(ESC);
                        State::ApcVge
                    }
                    ST_CLOSE => {
                        // Envelope complete.
                        out.push_payload(std::mem::take(&mut self.body));
                        State::Idle
                    }
                    ESC_MARK_TILDE => {
                        self.body.push(TILDE);
                        State::ApcVge
                    }
                    ESC_MARK_XON => {
                        self.body.push(XON);
                        State::ApcVge
                    }
                    ESC_MARK_XOFF => {
                        self.body.push(XOFF);
                        State::ApcVge
                    }
                    ESC_MARK_TAB => {
                        self.body.push(TAB);
                        State::ApcVge
                    }
                    ESC_MARK_LF => {
                        self.body.push(LF);
                        State::ApcVge
                    }
                    ESC_MARK_CR => {
                        self.body.push(CR);
                        State::ApcVge
                    }
                    _ => {
                        self.body.clear();
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
            State::Csi => {
                out.push_pass(b);
                // Final byte? CSI finals are 0x40..=0x7E.
                if (0x40..=0x7E).contains(&b) {
                    // DECSTR is `ESC [ ! p`.
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
                    // Erase In Display:
                    //   `ESC [ 2 J` — wipe live region.
                    //   `ESC [ 3 J` — wipe scrollback.
                    if b == b'J' && self.csi.as_slice() == b"2" {
                        out.push_event(TerminalEvent::EraseDisplay);
                    }
                    if b == b'J' && self.csi.as_slice() == b"3" {
                        out.push_event(TerminalEvent::EraseScrollback);
                    }
                    if let Some(ev) = query_event(&self.csi, b) {
                        out.push_event(ev);
                    }
                    State::Idle
                } else {
                    self.csi.push(b);
                    if self.csi.len() > CSI_BUF_CAP {
                        // Pathological / unrecognised — give up on
                        // matching but keep passing bytes until we hit
                        // a final.
                        self.csi.clear();
                    }
                    State::Csi
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::envelope::wrap_c2t_envelope;

    /// `feed_segments` must produce exactly what stepping the state
    /// machine byte by byte produces — including how passthrough runs
    /// coalesce into `Pass` segments, since the terminal stage feeds
    /// each one to the vt100 in order.
    fn assert_bulk_matches_per_byte(input: &[u8], max_payload: usize) {
        let mut bulk = ApcStream::new().with_max_payload(max_payload);
        let whole = bulk.feed_segments(input);
        let mut slow = ApcStream::new().with_max_payload(max_payload);
        let stepwise = slow.feed_segments_stepwise(input);
        assert_eq!(whole, stepwise, "segments differ");
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
        s.extend_from_slice(&wrap_c2t_envelope(b"payload one\x1b\x07\r\n~"));
        // Someone else's APC — passes through verbatim.
        s.extend_from_slice(b"\x1b_PRTnot ours\x1b\\");
        // Observed control sequences, then a lone ESC.
        s.extend_from_slice(b"\x1b[2J\x1b[3J\x1b[!p\x1b[6n\x1bc\x1bZ");
        let body: Vec<u8> = (0u16..512).map(|i| (i % 256) as u8).collect();
        s.extend_from_slice(&wrap_c2t_envelope(&body));
        s.extend_from_slice(b"tail\r\n");
        for cap in [0, 1, 16, 512, DEFAULT_MAX_PAYLOAD] {
            assert_bulk_matches_per_byte(&s, cap);
        }
    }

    #[test]
    fn bulk_and_per_byte_feeds_agree_at_the_payload_cap() {
        // A 512-byte body with nothing to stuff reaches the bulk path
        // as one run, so the cap is decided there; these three caps
        // straddle it.
        let mut clean = Vec::from(&b"\x1b_VGE"[..]);
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
        let mut s = Vec::from(&b"\x1b_VGEleading body\x1bq trailing\x1b\\"[..]);
        s.extend_from_slice(&wrap_c2t_envelope(b"the next one"));
        assert_bulk_matches_per_byte(&s, DEFAULT_MAX_PAYLOAD);
    }

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

    /// Every query is observed *and* still passed through: the vt100
    /// behind us has to see the bytes it always saw.
    fn events_of(input: &[u8]) -> (Vec<TerminalEvent>, Vec<u8>) {
        let mut s = ApcStream::new();
        let out = s.feed(input);
        (out.events, out.passthrough)
    }

    /// DECXCPR is DSR with a DEC-private `?`, and the two must not be
    /// confused: their replies differ by that same `?`.
    #[test]
    fn decxcpr_and_dsr_are_told_apart() {
        let (events, pass) = events_of(b"\x1b[?6n");
        assert_eq!(events, vec![TerminalEvent::ExtendedCursorPositionQuery]);
        assert_eq!(pass, b"\x1b[?6n");

        let (events, _) = events_of(b"\x1b[6n");
        assert_eq!(events, vec![TerminalEvent::CursorPositionQuery]);
    }

    #[test]
    fn da1_is_observed_in_both_spellings() {
        for input in [b"\x1b[c".as_ref(), b"\x1b[0c".as_ref()] {
            let (events, pass) = events_of(input);
            assert_eq!(events, vec![TerminalEvent::DeviceAttributes1]);
            assert_eq!(pass, input);
        }
    }

    #[test]
    fn da2_is_observed_and_not_confused_with_da1() {
        for input in [b"\x1b[>c".as_ref(), b"\x1b[>0c".as_ref()] {
            let (events, _) = events_of(input);
            assert_eq!(events, vec![TerminalEvent::DeviceAttributes2]);
        }
    }

    #[test]
    fn xtversion_is_observed() {
        for input in [b"\x1b[>q".as_ref(), b"\x1b[>0q".as_ref()] {
            let (events, _) = events_of(input);
            assert_eq!(events, vec![TerminalEvent::XtVersion]);
        }
    }

    #[test]
    fn keyboard_flags_query_is_observed() {
        let (events, pass) = events_of(b"\x1b[?u");
        assert_eq!(events, vec![TerminalEvent::KeyboardFlagsQuery]);
        assert_eq!(pass, b"\x1b[?u", "the query still reaches the vt100");
    }

    /// `CSI u` is SCORC, the ANSI.SYS cursor restore, and it is common:
    /// matching it as the keyboard query would answer a cursor restore
    /// with an escape sequence the program reads as keystrokes. The
    /// stack operations are not queries either — the vt100 applies
    /// those and there is nothing to reply to.
    #[test]
    fn scorc_and_the_stack_operations_are_not_the_query() {
        for input in [
            b"\x1b[u".as_ref(),   // SCORC
            b"\x1b[>1u".as_ref(), // push
            b"\x1b[<u".as_ref(),  // pop
            b"\x1b[=1;2u".as_ref(), // set
        ] {
            let (events, _) = events_of(input);
            assert!(events.is_empty(), "matched {input:?} as a query");
        }
    }

    /// `CSI Ps q` is DECLL and `CSI Ps SP q` is DECSCUSR — both common,
    /// neither a version query. The `>` is the whole difference.
    #[test]
    fn cursor_style_and_leds_are_not_xtversion() {
        for input in [b"\x1b[2q".as_ref(), b"\x1b[2 q".as_ref(), b"\x1b[q".as_ref()] {
            let (events, _) = events_of(input);
            assert!(events.is_empty(), "matched {input:?} as a query");
        }
    }

    #[test]
    fn decrqm_is_observed_in_both_flavours() {
        let (events, _) = events_of(b"\x1b[?2004$p");
        assert_eq!(
            events,
            vec![TerminalEvent::ModeQuery { private: true, mode: 2004 }]
        );
        let (events, _) = events_of(b"\x1b[4$p");
        assert_eq!(
            events,
            vec![TerminalEvent::ModeQuery { private: false, mode: 4 }]
        );
    }

    /// DECSTR is `CSI ! p` and DECRQM is `CSI Ps $ p`; they share a
    /// final byte and nothing else. Matching one as the other would
    /// wipe VGE state on a mode query.
    #[test]
    fn decstr_is_not_a_mode_query() {
        let (events, _) = events_of(b"\x1b[!p");
        assert_eq!(events, vec![TerminalEvent::SoftReset]);
    }

    /// A mode number too large for `u16` is not one we could answer,
    /// and must not wrap into one we would.
    #[test]
    fn absurd_mode_number_is_not_a_query() {
        let (events, _) = events_of(b"\x1b[?99999999$p");
        assert!(events.is_empty());
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
    /// `ESC _ VGE … ESC \\`, the shape that gets split.
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
