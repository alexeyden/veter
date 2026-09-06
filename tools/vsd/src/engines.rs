//! Per-session host-engine state and the PTY-reader worker thread.
//!
//! Each [`Session`](crate::session::Session) owns an `Arc<Mutex<EngineState>>`
//! and spawns one of these workers when the session is created. The
//! worker reads from a dup of the inner PTY master, runs the bytes
//! through PRT → VFT → SES → VGE → vt100 in the same order as
//! `veter/src/main.rs::App::process_pty_output`, and writes any
//! engine-generated responses back to the PTY master — when it is the
//! one that should be answering at all, which is
//! [`EngineState::set_renderer_attached`]'s subject.
//!
//! Sessions don't attach yet — the externally visible effect of this
//! module is that PTY output is parsed and accumulated in engine state,
//! ready for the attach path (task #6) to serialize and replay.
//!
//! ## Grid sizing
//!
//! v1 starts every session at the conventional `24×80` grid with a
//! generous default scrollback. The attach path will resize the parser
//! once it learns the renderer's actual grid via the VGE/PRT probes.
//! VGE/PRT engine metrics (cell pixel dims, scale factor) default to
//! 8×16 px / 1.0×; the attach path resets them from the probe response.
//!
//! ## Thread layout
//!
//! - The daemon's accept loop (main thread) constructs the session,
//!   inserts it into the session table, and holds an `OwnedFd` to the
//!   master for the future inbound-input splice path.
//! - The worker thread receives its own `OwnedFd` (a `dup(2)` of the
//!   master) and blocks on reads. It writes engine responses through
//!   that same fd so the inner program sees DSR/VGE/PRT replies.
//! - Both threads share `Arc<Mutex<EngineState>>`. The worker releases
//!   the lock around every `read(2)` so the attach path (later) can
//!   serialize the state without waiting on PTY traffic.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use veter_host::prt::PrtEngine;
use veter_host::ses::SesEngine;
use veter_host::vft::VftEngine;
use veter_host::vge::VgeEngine;

/// Default grid size used until the renderer attaches and reports its
/// actual cell count. Mirrors `veter/src/main.rs`'s startup defaults.
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Default scrollback depth. Matches the host binary's
/// `vt100::Parser::new(..., 10_000)` allocation; the attach path
/// inherits it.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Placeholder cell pixel dimensions used until the renderer's VGE
/// probe response arrives. Pixel-space layout decisions stored in VGE
/// state are anchor-based, so this default does not affect correctness
/// — it only sizes pre-attach `cell_px` reports to inner programs.
pub const DEFAULT_CELL_PX: (u16, u16) = (8, 16);
pub const DEFAULT_SCALE: f32 = 1.0;

/// Host-side state machinery shared between the daemon and the
/// per-session worker thread. See module docs for the threading model.
pub struct EngineState {
    pub parser: vt100::Parser,
    pub vge: VgeEngine,
    pub prt: PrtEngine,
    /// Host-level VFT **relay** (`VftEngine::set_relay`). The daemon
    /// implements no file transfer — see `new` — but it must still
    /// lift `ESC _ VFT …` envelopes out of the stream before the
    /// vt100 sees them. A stuffed payload byte pair `ESC \` closes
    /// the APC string early in the parser and the rest of the file's
    /// bytes land on the mirrored grid as text; the renderer, which
    /// does extract them, would show nothing of the sort, and the
    /// mirror is supposed to match. The portals do the same one level
    /// down, via `PrtEngine::set_vft_relay`.
    pub vft: VftEngine,
    /// SES engine carrying this session's name. Answers a `vmux` SES
    /// probe with `in_session = true` + the name, and turns a `Detach`
    /// command into a self-pipe wake (see `worker_main`).
    pub ses: SesEngine,
    /// Write side of the renderer's stdout while a renderer is
    /// attached. The worker forwards every PTY-master chunk it reads
    /// into this fd verbatim (without the engine transforms) so the
    /// renderer paints exactly what the inner program produces. The
    /// attach handler installs this on attach and clears it on detach
    /// or write error.
    pub renderer_stdout: Option<OwnedFd>,
    /// Whether the engines are currently answering the programs
    /// inside this session — VGE commands, DSR, and the same two one
    /// level down inside every portal. Flipped by
    /// [`EngineState::set_renderer_attached`]; see it for the rule.
    answering: bool,
    /// Write end of a self-pipe the attach handler uses to wake its
    /// `splice_input` loop when the session itself is gone (inner
    /// program EOF / worker fatal error, `vsd kill`, SIGTERM).
    /// Without this the splice stays blocked on the renderer's stdin
    /// and the attached terminal "hangs" — worse, on a shutdown path
    /// the process exits with the handler mid-splice, so its
    /// `RawTty` guard never runs and the user's terminal is left
    /// `-echo -icanon -opost`. Installed by the attach handler before
    /// splicing, cleared on detach. Signal it through
    /// [`Self::signal_attach_shutdown`].
    pub attach_shutdown: Option<OwnedFd>,
    /// Serialises writes to the inner PTY master.
    ///
    /// Two threads write to it: this worker, sending engine replies
    /// (PRT responses, `PortalActivity` events, DSR reports), and the
    /// attach handler's `splice_input`, sending the renderer's input.
    /// Neither is a single `write(2)` — a large renderer reply, such
    /// as a VFT download frame relayed to `vrecv`, exceeds the 4 KiB
    /// pty input buffer and goes out in pieces — so without a lock a
    /// PRT event lands in the middle of one and the envelope reaching
    /// the remote `vmux` is corrupt.
    ///
    /// Deliberately *not* the engines lock: a write to a full pty
    /// buffer blocks, and the engines lock is what the attach path
    /// takes to serialize a snapshot. Grab an `Arc` clone once per
    /// thread rather than reaching through the engines lock on each
    /// write.
    master_write: Arc<Mutex<()>>,
}

impl EngineState {
    /// Construct the engine set for a session named `session_name`.
    /// The name reaches the SES engine so an inner `vmux` can learn
    /// which session it lives in.
    pub fn new(session_name: String) -> Self {
        let vge = VgeEngine::new(DEFAULT_CELL_PX, DEFAULT_SCALE);
        let mut prt = PrtEngine::with_metrics_and_wakeup(
            DEFAULT_CELL_PX,
            DEFAULT_SCALE,
            Arc::new(|| {}),
        );
        // A fresh session is unattached, so it starts out answering
        // its own inner programs. `set_renderer_attached` takes over
        // from the first chunk onwards.
        //
        // VFT is the one extension the daemon never implements, in
        // either state. A transfer's destination is the user's
        // machine — its file picker, its desktop — and that is the
        // renderer's, never ours; a daemon that answered would write
        // the file on the wrong host, silently, alongside the
        // terminal that received the same bytes. So each portal
        // lifts VFT envelopes out of its byte stream (they would
        // otherwise spill onto the mirrored grid, see
        // `VftEngine::set_relay`) and drops them, while the verbatim
        // forward carries the real transfer upstream. `vft-protocol`
        // §1.1 names this exact arrangement; the host-level engine
        // has never been instantiated here for the same reason.
        prt.set_vft_relay(true);
        let mut vft = VftEngine::with_wakeup(Arc::new(|| {}));
        vft.set_relay(true);
        Self {
            parser: vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLS, DEFAULT_SCROLLBACK),
            vge,
            // No-op VFT wakeup: the daemon has no event loop to
            // nudge, and with the relay above there are no per-portal
            // workers left to nudge it from.
            prt,
            vft,
            ses: SesEngine::with_session(session_name),
            renderer_stdout: None,
            answering: true,
            attach_shutdown: None,
            master_write: Arc::new(Mutex::new(())),
        }
    }

    /// Point the reply channel at whoever is the terminal right now.
    ///
    /// While a renderer is attached, the bytes this session produces
    /// reach it and *it* answers them: it is the real terminal, it
    /// holds the real cell metrics, and it is the only party that can
    /// answer a question about the frame it painted (VGE §15
    /// `QueryHit`). The daemon goes quiet, because a second answer
    /// doesn't cancel the first — the client consumes one and the
    /// other lands in whatever is reading that pty by then, which for
    /// an interactive client is its keyboard input.
    ///
    /// While nothing is attached there is no such terminal, so the
    /// daemon answers rather than leave a client hanging: a program
    /// started in a detached session still gets its probe answered,
    /// from the metrics of the last renderer that was here. Sessions
    /// outlive renderers; the programs inside them shouldn't have to
    /// care which state they were started in.
    ///
    /// Callers must derive `attached` from the same `renderer_stdout`
    /// check that decides whether the chunk is forwarded, under the
    /// same lock. That is what makes "answered exactly once" a
    /// property rather than a likelihood: a chunk is answered here iff
    /// it is not sent somewhere that will answer it.
    /// Resize the mirrored grid, the way the renderer's own resize
    /// path does.
    ///
    /// `set_size` alone is not the whole job: a vertical resize moves
    /// the live screen relative to scrollback (xterm-style push/pull),
    /// so both sub-engines have to re-read the line origin their
    /// anchors are relative to. `veter`'s `WindowEvent::Resized` runs
    /// all three; the daemon used to run only the first and lag by a
    /// chunk, leaving anchored elements and portals placed against the
    /// pre-resize origin until the next byte arrived.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.prt.after_vt100_process(&mut self.parser);
        self.vge.after_vt100_process(&mut self.parser);
    }

    /// A handle on the inner PTY master's write lock, for a thread
    /// that is about to start writing to it. See the field.
    pub fn master_write_lock(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.master_write)
    }

    /// Wake an attach handler blocked in `splice_input`, if there is
    /// one. Every shutdown path owes the attached terminal this: the
    /// handler is what restores its termios and sends `DetachNotify`,
    /// and it is blocked on the renderer's stdin until something
    /// pokes the self-pipe. Idempotent — a second byte on the pipe
    /// just wakes a loop that has already left.
    pub fn signal_attach_shutdown(&self) {
        if let Some(fd) = self.attach_shutdown.as_ref() {
            // SAFETY: borrowed from the OwnedFd we hold; we only
            // write, never close it.
            let borrowed =
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd.as_raw_fd()) };
            let _ = nix::unistd::write(borrowed, &[0u8]);
        }
    }

    pub fn set_renderer_attached(&mut self, attached: bool) {
        let should_answer = !attached;
        if self.answering == should_answer {
            return;
        }
        self.answering = should_answer;
        self.vge.set_auto_reply_commands(should_answer);
        self.vge.set_auto_reply_dsr(should_answer);
        self.vge.set_auto_reply_queries(should_answer);
        self.prt.set_portal_auto_reply(should_answer);
    }
}

/// What one chunk from the inner PTY produced.
pub struct ChunkOutcome {
    /// Bytes owed to the programs inside the session — engine
    /// responses and events — for the caller to write to the master.
    pub replies: Vec<u8>,
    /// Bytes to forward to the attached renderer, or `None` when
    /// there isn't one.
    pub forward: Option<Vec<u8>>,
}

impl EngineState {
    /// Run one chunk of inner-PTY output through the engines and say
    /// what it owes to each side.
    ///
    /// The stage order here differs from the renderer's in one place:
    /// **SES runs first**, because its passthrough is what gets
    /// forwarded. SES is the `vmux` ↔ `vsd` control channel and the
    /// daemon is its host; the renderer is not a session, so its own
    /// per-portal SES engine would answer the same probe with "not in
    /// a session" — a second, contradictory answer to a question only
    /// this process can answer. Every APC parser passes foreign
    /// markers through verbatim, so moving SES to the front costs
    /// nothing and is what makes the strip possible.
    pub fn process_chunk(&mut self, chunk: &[u8]) -> ChunkOutcome {
        // One decision with several consequences: the chunk goes to
        // the renderer iff one is attached, and we answer it iff it is
        // *not* going to a renderer that will answer it instead. Taken
        // here, under the lock that also guards the engines, so the
        // two can never disagree about a chunk.
        let attached = self.renderer_stdout.is_some();
        self.set_renderer_attached(attached);
        let EngineState {
            parser,
            vge,
            prt,
            vft,
            ses,
            renderer_stdout: _,
            answering: _,
            attach_shutdown,
            master_write: _,
        } = self;

        let ses_passthrough = ses.process_pty_chunk(chunk);
        let prt_chunk = prt.process_pty_chunk_full(&ses_passthrough);
        // VFT is extracted and dropped (the engine is a relay): the
        // transfer belongs to the renderer, but the envelope must not
        // reach the vt100 — see the field's doc.
        let vft_passthrough = vft.process_pty_chunk(&prt_chunk.passthrough);
        // VGE runs last as the terminal stage, driving the vt100
        // itself so element origins resolve against the screen the
        // inner program saw. See
        // `veter_host::vge::drive_terminal_stage`.
        //
        // No hit tester: vsd holds the session state but the renderer
        // painting it is a different process, so a VGE `QueryHit`
        // (§15) is answered `err_no_hit_testing` rather than with
        // geometry nobody here has.
        veter_host::vge::drive_terminal_stage(vge, parser, &vft_passthrough, None);
        // See the matching note in veter's host loop: an over-cap
        // envelope is dropped without a reply, so report it.
        let dropped = prt.take_apc_overflows() + vge.take_apc_overflows();
        if dropped > 0 {
            eprintln!(
                "vsd: dropped {dropped} oversized APC envelope(s); \
                 a client exceeded the payload cap"
            );
        }
        // The portal-set reactions to RIS / DECSTR / 2J / 3J and the
        // alt-screen swaps rode along inside `process_pty_chunk_full`,
        // in stream order with the commands they scope;
        // `after_vt100_process` only has the post-chunk line origin
        // left to reconcile.
        prt.after_vt100_process(parser);
        prt.flush_pending_events();
        prt.drive_and_flush_vft();

        // PRT follows the same "answered exactly once" rule as VGE and
        // DSR. Attached, the renderer runs these same commands off the
        // forwarded chunk and answers them itself, so a reply from
        // here is a second one for the same request id — and a second
        // PRT probe advertising *this* process's limits and accent
        // rather than the terminal's. Drained either way: a discarded
        // queue must not accumulate.
        let prt_replies = prt.take_responses();
        let mut replies = if attached { Vec::new() } else { prt_replies };
        replies.extend_from_slice(&vge.take_responses());
        // SES is the exception, and the reason the strip above exists:
        // the daemon is the only party that knows the session name, so
        // it answers whether or not a renderer is attached.
        replies.extend_from_slice(&ses.take_responses());

        // A SES `Detach` command wakes the attach handler's
        // `splice_input` loop via the self-pipe — the exact same
        // teardown path as the `Ctrl+\ d` hotkey. With no renderer
        // attached `attach_shutdown` is `None` and this is a no-op.
        if ses.take_detach_requests() > 0
            && let Some(fd) = attach_shutdown.as_ref()
        {
            // SAFETY: `fd` is borrowed from the OwnedFd held in
            // `attach_shutdown`; we only write, never close it.
            let borrowed =
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd.as_raw_fd()) };
            let _ = nix::unistd::write(borrowed, &[0u8]);
        }

        // What the renderer gets is the chunk minus the SES envelopes
        // — otherwise verbatim, since it parses PRT / VGE / VFT
        // natively and must see exactly what the inner program wrote.
        let forward = attached.then_some(ses_passthrough);
        ChunkOutcome { replies, forward }
    }
}

/// Dup the master fd twice (one read handle, one write handle) and
/// spawn the per-session worker. Returns the shared engine handle so
/// the attach path can lock it to serialize a snapshot or to forward
/// live output.
pub fn spawn_worker(
    master: &OwnedFd,
    session_name: String,
) -> Result<Arc<Mutex<EngineState>>> {
    let reader_fd = dup_owned(master).context("dup(master) for worker reader")?;
    let writer_fd = dup_owned(master).context("dup(master) for worker writer")?;
    let engines = Arc::new(Mutex::new(EngineState::new(session_name)));
    let engines_for_worker = Arc::clone(&engines);
    std::thread::Builder::new()
        .name("vsd-worker".into())
        .spawn(move || worker_main(reader_fd, writer_fd, engines_for_worker))
        .context("spawn worker thread")?;
    Ok(engines)
}

fn dup_owned(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    let raw = nix::unistd::dup(fd.as_raw_fd()).map_err(std::io::Error::other)?;
    // SAFETY: dup(2) returned a fresh fd we now solely own.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Worker thread entry point. Mirrors the host pipeline in
/// `veter/src/main.rs::App::process_pty_output`: PRT extracts its
/// envelopes (and observes RIS/DECSTR/2J/3J), VGE extracts its
/// envelopes from the PRT passthrough, and the host vt100 parses
/// whatever remains. After every chunk we run both engines'
/// `after_vt100_process` hooks and write back any pending responses.
///
/// VFT is intentionally **not** implemented in vsd, at any depth: per
/// the architecture sketch in `doc/session-manager.md`, VFT envelopes
/// ride through the daemon verbatim (the pass-through contract in
/// `doc/file-transfer-extension.md` §1.1 makes this normative). No
/// host-level engine is instantiated at all, and the per-portal ones
/// in the PRT tree are relays: they lift the envelopes out of the
/// byte stream, so the mirrored grid stays exact, and drop them.
fn worker_main(reader_fd: OwnedFd, writer_fd: OwnedFd, engines: Arc<Mutex<EngineState>>) {
    let mut reader = std::fs::File::from(reader_fd);
    let mut writer = std::fs::File::from(writer_fd);
    // Grabbed once: the attach handler's splice thread writes to the
    // same master, and a reply interleaved into the middle of a
    // multi-write renderer frame corrupts it. See the field.
    let master_write = {
        let guard = match engines.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.master_write_lock()
    };
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // EIO on Linux means the slave was closed — treat as EOF.
                if e.raw_os_error() == Some(libc::EIO) {
                    break;
                }
                eprintln!("vsd: worker read error: {e}");
                break;
            }
        };

        let outcome = {
            let mut guard = match engines.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.process_chunk(&buf[..n])
        };
        let ChunkOutcome { replies: to_write, forward } = outcome;

        if !to_write.is_empty() {
            // Best effort: a failed write back to the inner program
            // means it has gone away; the next read will EOF and we'll
            // exit the loop. The engines lock is already released
            // here, so blocking on a full pty buffer under the write
            // lock can't stall an attach mid-snapshot.
            let _w = match master_write.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Err(e) = writer.write_all(&to_write) {
                eprintln!("vsd: worker write error: {e}");
                break;
            }
        }

        if let Some(forward) = forward {
            // Write outside the engines lock so a slow renderer
            // doesn't stall the engines. We dup the fd briefly to
            // avoid holding the lock during the write; on write error
            // we clear `renderer_stdout` so the next chunk doesn't
            // retry into a closed pipe.
            // A `dup`, not the raw fd number: a detach drops the
            // OwnedFd the moment we release the lock, and the next
            // `dup`/`open` anywhere in the process can hand that
            // number to something else — which would then receive
            // this chunk of the session's output.
            let stdout = {
                let guard = match engines.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard
                    .renderer_stdout
                    .as_ref()
                    .and_then(|fd| dup_owned(fd).ok())
            };
            if let Some(stdout) = stdout {
                let raw = stdout.as_raw_fd();
                let mut wrote_ok = true;
                let mut off = 0;
                while off < forward.len() {
                    match nix::unistd::write(
                        // SAFETY: `raw` is borrowed from the `stdout`
                        // dup we own for this write. write(2) takes a
                        // BorrowedFd.
                        unsafe {
                            std::os::fd::BorrowedFd::borrow_raw(raw)
                        },
                        &forward[off..],
                    ) {
                        Ok(0) => {
                            wrote_ok = false;
                            break;
                        }
                        Ok(k) => off += k,
                        Err(nix::errno::Errno::EINTR) => continue,
                        Err(_) => {
                            wrote_ok = false;
                            break;
                        }
                    }
                }
                if !wrote_ok {
                    let mut guard = match engines.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.renderer_stdout = None;
                }
            }
        }
    }

    // Worker is exiting (PTY EOF, EIO, or other read error). If an
    // attach handler is currently splicing renderer-stdin into the
    // master, it needs to be woken — otherwise the attached terminal
    // hangs after the user types `exit` or Ctrl+D inside the session.
    // A single byte on the self-pipe is enough; the attach handler's
    // poll loop wakes, sees the readable fd, and tears the splice down.
    {
        let mut guard = match engines.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.signal_attach_shutdown();
        // Dropping the taken fd closes the write end; if there was no
        // attach it simply closes here without effect.
        guard.attach_shutdown = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prt_protocol::codec::Reader;
    use prt_protocol::command::{AnchorMode, Command, CreatePortalBody, WritePortalBody};
    use prt_protocol::encode::build_envelope;
    use prt_protocol::frame::{EVT_RAW_REPLY, MARKER_T2C};

    /// A session with one portal open, standing in for a `vmux` pane.
    fn session_with_portal(attached: bool) -> EngineState {
        let mut st = EngineState::new("s".into());
        st.set_renderer_attached(attached);
        let create = Command::CreatePortal(CreatePortalBody {
            id: "p1".into(),
            size_w: 80,
            size_h: 24,
            origin_x: 0,
            origin_y: 0,
            anchor_mode: AnchorMode::Live,
            is_visible: true,
            draw_order: 0,
            flags: 0,
            scrollback_lines: 100,
        });
        let env = build_envelope(&[(create, 1)]);
        let _ = st.prt.process_pty_chunk_full(&env);
        let _ = st.prt.take_responses();
        st
    }

    /// Write `data` to the portal the way `vmux` relays its pane's
    /// output, and return everything that came back for the program
    /// inside it (the `RawReply` payloads, concatenated).
    fn write_to_portal(st: &mut EngineState, data: Vec<u8>) -> Vec<u8> {
        let env = build_envelope(&[(
            Command::WritePortal(WritePortalBody {
                id: "p1".into(),
                data,
            }),
            2,
        )]);
        let _ = st.prt.process_pty_chunk_full(&env);
        st.prt.flush_pending_events();
        let resp = st.prt.take_responses();

        let mut inner = Vec::new();
        let mut s = prt_protocol::apc::ApcStream::with_marker(*MARKER_T2C);
        let out = s.feed(&resp);
        for payload in out.payloads() {
            let mut r = Reader::new(payload);
            let _version = r.u8();
            let _payload_len = r.u32();
            while !r.at_end() {
                let Ok(ft) = r.u8() else { break };
                let _rid = r.u32().unwrap_or(0);
                let Ok(body_len) = r.u32() else { break };
                let Ok(body) = r.take(body_len as usize) else { break };
                if ft == EVT_RAW_REPLY {
                    let mut br = Reader::new(body);
                    let _id = br.string();
                    inner.extend_from_slice(br.bytes().unwrap_or(&[]));
                }
            }
        }
        inner
    }

    fn vge_probe_envelope() -> Vec<u8> {
        let mut frames = Vec::new();
        vge_protocol::envelope::append_frame(
            &mut frames,
            vge_protocol::frame::CMD_PROBE,
            7,
            &[],
        );
        vge_protocol::envelope::wrap_c2t_envelope(&frames)
    }

    fn has_probe_response(bytes: &[u8]) -> bool {
        let mut s =
            vge_protocol::apc::ApcStream::with_marker(*vge_protocol::frame::MARKER_T2C);
        let out = s.feed(bytes);
        out.payloads.iter().any(|p| {
            let mut r = Reader::new(p);
            let _version = r.u8();
            let _payload_len = r.u32();
            r.u8().ok() == Some(vge_protocol::frame::RSP_PROBE)
        })
    }

    /// Attached, the renderer answers the client inside the pane —
    /// it is the real terminal — so the daemon must not answer too.
    /// Two replies to one probe is what made `vplay` quit on startup:
    /// it consumed the first and read the second as keystrokes.
    #[test]
    fn attached_portal_vge_probe_is_left_to_the_renderer() {
        let mut st = session_with_portal(true);
        let reply = write_to_portal(&mut st, vge_probe_envelope());
        assert!(
            !has_probe_response(&reply),
            "daemon answered a probe the renderer is also answering"
        );
    }

    /// Detached there is no renderer to answer, and a session that
    /// outlives its renderer still has to be usable — a client
    /// started here gets its probe answered by the daemon.
    #[test]
    fn detached_portal_vge_probe_is_answered_here() {
        let mut st = session_with_portal(false);
        let reply = write_to_portal(&mut st, vge_probe_envelope());
        assert!(
            has_probe_response(&reply),
            "nobody answered the probe: the client would time out"
        );
    }

    /// Same rule for the other reply a portal owes its inner program.
    #[test]
    fn attached_portal_dsr_is_left_to_the_renderer() {
        let mut st = session_with_portal(true);
        let reply = write_to_portal(&mut st, b"\x1b[6n".to_vec());
        assert!(
            reply.is_empty(),
            "daemon sent a cursor report the renderer also sends: {:?}",
            String::from_utf8_lossy(&reply)
        );
    }

    #[test]
    fn detached_portal_dsr_is_answered_here() {
        let mut st = session_with_portal(false);
        let reply = write_to_portal(&mut st, b"\x1b[6n".to_vec());
        assert_eq!(String::from_utf8_lossy(&reply), "\u{1b}[1;1R");
    }

    /// Queries that arrive while the renderer owns the reply channel
    /// are consumed, not banked: reattaching, or detaching, must not
    /// flush a burst of stale cursor reports at the inner program.
    #[test]
    fn queries_swallowed_while_attached_do_not_pile_up() {
        let mut st = session_with_portal(true);
        let _ = write_to_portal(&mut st, b"\x1b[6n\x1b[6n".to_vec());
        st.set_renderer_attached(false);
        let reply = write_to_portal(&mut st, b"x".to_vec());
        assert!(
            reply.is_empty(),
            "detaching replayed queries the renderer already answered: {:?}",
            String::from_utf8_lossy(&reply)
        );
    }

    /// DA1 follows the same rule as the VGE probe and DSR: exactly one
    /// party answers. Attached, that party is the renderer reading the
    /// forwarded chunk; the daemon must stay quiet or the program in
    /// the pane reads the spare reply as keystrokes.
    #[test]
    fn portal_da1_follows_the_attached_switch() {
        for attached in [true, false] {
            let mut st = session_with_portal(attached);
            let reply = write_to_portal(&mut st, b"\x1b[c".to_vec());
            assert_eq!(
                reply == veter_host::query::DA1,
                !attached,
                "attached={attached}: wrong party answered DA1: {:?}",
                String::from_utf8_lossy(&reply)
            );
        }
    }

    /// …and the same one level up, for a client talking straight to
    /// the session with no `vmux` in between.
    #[test]
    fn host_level_da1_follows_the_attached_switch() {
        for attached in [true, false] {
            let mut st = EngineState::new("s".into());
            st.set_renderer_attached(attached);
            let prt_chunk = st.prt.process_pty_chunk_full(b"\x1b[c");
            let ses_pass = st.ses.process_pty_chunk(&prt_chunk.passthrough);
            veter_host::vge::drive_terminal_stage(
                &mut st.vge,
                &mut st.parser,
                &ses_pass,
                None,
            );
            let out = st.vge.take_responses();
            assert_eq!(
                out == veter_host::query::DA1,
                !attached,
                "attached={attached}: wrong party answered the host DA1"
            );
        }
    }

    /// An `EngineState` that believes a renderer is attached.
    ///
    /// `process_chunk` derives that from `renderer_stdout` itself —
    /// under the same lock, so the "answer it iff we don't forward it"
    /// decision can't disagree with itself — so a test has to hand it
    /// a real fd. The pipe is never read; nothing here writes to it.
    fn attached_state() -> (EngineState, OwnedFd) {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        let mut st = EngineState::new("s".into());
        st.renderer_stdout = Some(write_fd);
        (st, read_fd)
    }

    /// PRT follows the same "answered exactly once" rule as VGE and
    /// DSR. Attached, the renderer runs the same commands off the
    /// forwarded chunk, so a reply from here is a second one for the
    /// same request id — and a PRT probe from here advertises the
    /// daemon's limits and accent instead of the terminal's.
    #[test]
    fn attached_prt_commands_are_answered_by_the_renderer_alone() {
        let (mut st, _read) = attached_state();
        let out = st.process_chunk(&build_envelope(&[(Command::Probe, 3)]));
        assert!(
            out.replies.is_empty(),
            "the daemon answered a PRT command the renderer also answers"
        );
        assert!(out.forward.is_some(), "the renderer got nothing to answer");
    }

    #[test]
    fn detached_prt_commands_are_answered_here() {
        let mut st = EngineState::new("s".into());
        let out = st.process_chunk(&build_envelope(&[(Command::Probe, 3)]));
        assert!(
            !out.replies.is_empty(),
            "nobody answered the probe: the client would time out"
        );
        assert!(out.forward.is_none(), "forwarded with no renderer attached");
    }

    /// SES is the one channel only the daemon can answer — it is the
    /// process that knows the session name — so the renderer must not
    /// see the envelopes at all. Its own per-portal SES engine would
    /// answer the same probe with "not in a session", which is the
    /// opposite of the truth.
    #[test]
    fn ses_envelopes_are_stripped_from_what_the_renderer_sees() {
        let (mut st, _read) = attached_state();
        let mut frames = Vec::new();
        veter_host::ses::envelope::append_frame(
            &mut frames,
            veter_host::ses::frame::CMD_PROBE,
            11,
            &[],
        );
        let mut chunk = b"before".to_vec();
        chunk.extend_from_slice(&veter_host::ses::envelope::wrap_c2h_envelope(&frames));
        chunk.extend_from_slice(b"after");

        let out = st.process_chunk(&chunk);
        assert_eq!(
            out.forward.as_deref(),
            Some(b"beforeafter".as_ref()),
            "a SES envelope reached the renderer"
        );
        assert!(
            !out.replies.is_empty(),
            "the daemon didn't answer the SES probe it consumed"
        );
    }

    /// Everything else still reaches the renderer byte for byte: it
    /// parses PRT / VGE / VFT natively and has to see what the inner
    /// program actually wrote.
    #[test]
    fn everything_but_ses_is_forwarded_verbatim() {
        let (mut st, _read) = attached_state();
        let mut chunk = b"text ".to_vec();
        chunk.extend_from_slice(&vge_probe_envelope());
        chunk.extend_from_slice(&build_envelope(&[(Command::Probe, 3)]));
        chunk.extend_from_slice(&vft_probe_envelope());
        chunk.extend_from_slice(b" more");

        assert_eq!(st.process_chunk(&chunk).forward.as_deref(), Some(&chunk[..]));
    }

    fn vft_probe_envelope() -> Vec<u8> {
        let mut frames = Vec::new();
        veter_host::vft::envelope::append_frame(
            &mut frames,
            veter_host::vft::frame::CMD_PROBE,
            5,
            &[],
        );
        veter_host::vft::envelope::wrap_c2h_envelope(&frames)
    }

    /// A `vsend` inside a pane talks to the *renderer* — that is
    /// where the user's filesystem and file picker are. The daemon
    /// answers nothing, attached or not: a second answer from the
    /// wrong machine is worse than none.
    #[test]
    fn portal_vft_is_never_answered_here() {
        for attached in [true, false] {
            let mut st = session_with_portal(attached);
            let reply = write_to_portal(&mut st, vft_probe_envelope());
            assert!(
                reply.is_empty(),
                "attached={attached}: daemon answered a VFT command: {:?}",
                String::from_utf8_lossy(&reply)
            );
        }
    }

    /// …and the envelope is still lifted out of the stream, so file
    /// bytes never reach the grid this session will hand the next
    /// renderer as a snapshot. Directly on the session pty, not in a
    /// pane: the host pipeline needs the relay stage too.
    #[test]
    fn host_level_vft_never_reaches_the_mirrored_grid() {
        let mut st = EngineState::new("s".into());
        // A chunk carrying the stuffed pair that ends an APC string
        // early in the vt100 parser (§1.3 turns a literal ESC into
        // `ESC ESC`, so file bytes `ESC \` arrive as `ESC ESC \`).
        let mut chunk = b"before\x1b_VFT".to_vec();
        chunk.extend_from_slice(b"\x1b\x1b\\FILEBYTES\x1b\\after".as_ref());

        let prt_chunk = st.prt.process_pty_chunk_full(&chunk);
        let vft_passthrough = st.vft.process_pty_chunk(&prt_chunk.passthrough);
        let ses_pass = st.ses.process_pty_chunk(&vft_passthrough);
        veter_host::vge::drive_terminal_stage(&mut st.vge, &mut st.parser, &ses_pass, None);

        let screen = st.parser.screen().contents();
        assert_eq!(screen.trim_end(), "beforeafter", "file bytes reached the grid");
    }

    /// The host engines follow the same switch: a client talking
    /// straight to the session (no `vmux` in between) gets one answer
    /// in either state.
    #[test]
    fn host_level_replies_follow_the_same_switch() {
        for attached in [true, false] {
            let mut st = EngineState::new("s".into());
            st.set_renderer_attached(attached);
            let chunk = vge_probe_envelope();
            let prt_chunk = st.prt.process_pty_chunk_full(&chunk);
            let ses_pass = st.ses.process_pty_chunk(&prt_chunk.passthrough);
            veter_host::vge::drive_terminal_stage(
                &mut st.vge,
                &mut st.parser,
                &ses_pass,
                None,
            );
            let out = st.vge.take_responses();
            assert_eq!(
                has_probe_response(&out),
                !attached,
                "attached={attached}: wrong party answered the host probe"
            );
        }
    }
}
