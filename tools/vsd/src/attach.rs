//! Attach lifecycle: receive the renderer's stdio fds, ship a state
//! snapshot, and splice live bytes between the renderer and the inner
//! PTY for the duration of the attach.
//!
//! ## Wire flow (matches [`crate::fdpass`] notes)
//!
//! 1. CLI: `Request::Attach { name }` over the IPC socket.
//! 2. CLI: `sendmsg` with one-byte filler + `SCM_RIGHTS` carrying
//!    `[stdin_fd, stdout_fd]`.
//! 3. Daemon (this module): `recvmsg` the fds, validate the session,
//!    spawn a handler thread, reply `Ok`. The IPC socket closes; the
//!    CLI exits.
//! 4. Handler thread:
//!    - Lock engine state.
//!    - Compute the snapshot byte stream (vt100 redraw → VGE state →
//!      PRT state). The order matches the docstring on each
//!      serializer — vt100 is the foundation, VGE elements depend on
//!      the cell grid, PRT portals carry their own per-portal state
//!      inside `WritePortal` envelopes.
//!    - Write the snapshot to the renderer's stdout.
//!    - Install the stdout fd on the shared engine state so the
//!      per-session worker thread forwards live PTY-master output to
//!      the renderer.
//!    - Mark `Session::attached = true`.
//!    - Splice the renderer's stdin into the inner PTY master until
//!      EOF / error. Input never crosses the engines (PRT spec) — the
//!      renderer's keystrokes go straight to the inner program's
//!      controlling tty.
//!    - On detach: clear the renderer-stdout fd and flip
//!      `Session::attached` back to `false`. The session keeps
//!      running.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::engines::EngineState;
use crate::fdpass;
use crate::probe;

/// How long to wait for the renderer to respond to the upstream
/// VGE / PRT probe before falling back to the daemon's defaults.
/// Non-VGE / non-PRT terminals ignore the probe envelopes (they parse
/// them as no-op APCs), so this is also the renderer-capability
/// timeout. 500 ms is a generous round-trip even over a slow SSH
/// connection.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// How long to wait for the renderer's `SnapshotAccepted` /
/// `SnapshotRejected` before giving up on it — `doc/session-manager.md`
/// §4.4 step 4. Longer than [`PROBE_TIMEOUT`] because the snapshot
/// itself is on the wire ahead of the answer: a few hundred KiB of
/// grid and images have to arrive and be applied before the renderer
/// can say anything.
///
/// Silence is not an error. A renderer that doesn't speak VSS never
/// answers, and the attach proceeds — it just means nothing restored
/// our state, which is what the detach path keys its `ESC c` on.
const ACK_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a version-mismatch banner stays on screen before the
/// attach tears itself down (§4.2). Long enough to read one line.
const REJECT_BANNER_HOLD: Duration = Duration::from_secs(2);

/// How long a detach waits for `DetachAccepted` with nothing arriving
/// at all (§4.4). What it outwaits is one round trip — `DetachNotify`
/// out through ssh and the renderer's multiplexer, the answer back —
/// so silence this long means nobody is going to answer.
const DETACH_ACK_IDLE: Duration = Duration::from_secs(2);

/// The most a detach waits for `DetachAccepted` while bytes keep
/// arriving. The backlog ahead of the answer is bounded — a download
/// stops at its unacknowledged window, 128 KiB — so this is only for a
/// renderer that keeps talking and never answers.
const DETACH_ACK_CAP: Duration = Duration::from_secs(10);

/// Upper bound on an attach handler's teardown once its splice has
/// been told to stop: the detach drain, plus the writes and termios
/// restore around it. The session process waits this long for the
/// handler before exiting, since exiting under it would leave the
/// renderer's tty raw.
pub const TEARDOWN_BUDGET: Duration = Duration::from_secs(12);

/// What the renderer said about the snapshot we sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotAck {
    /// It restored our state; it now has a stash of its own view to
    /// put back on detach.
    Accepted,
    /// It refused. `reason` is the `SnapshotRejected` code: 1 version,
    /// 2 malformed, 3 capacity.
    Rejected(u8),
    /// Nothing came back inside [`ACK_TIMEOUT`] — an older renderer,
    /// or one that doesn't speak VSS at all.
    Silent,
}

impl SnapshotAck {
    fn reject_reason(reason: u8) -> &'static str {
        match reason {
            1 => "the renderer and this daemon were built from different \
                  snapshot versions",
            2 => "the renderer could not parse the snapshot",
            3 => "the renderer could not hold a snapshot this large",
            _ => "the renderer refused the snapshot",
        }
    }
}

/// Wait for the renderer's verdict on the snapshot we just wrote.
///
/// Bytes that aren't a VSS upstream envelope go through `probe`, which
/// takes the VGE and PRT envelopes among them — a probe answer that
/// missed [`PROBE_TIMEOUT`] lands here, and it is the daemon's, not the
/// session's. What comes back is the user typing during the attach, and
/// the caller forwards it to the inner PTY exactly as the probe phase
/// does rather than swallowing it.
///
/// Feeding a `prt` envelope to the session instead is what put
/// `insert-last-word`'s output and a payload's raw bytes on the shell's
/// command line at attach time: `ESC _` is that zsh binding, and the
/// rest of the envelope is inserted literally.
fn await_snapshot_ack(
    stdin_fd: &OwnedFd,
    sequence_id: u32,
    timeout: Duration,
    probe: &mut probe::Probe,
) -> Result<(SnapshotAck, Vec<u8>)> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::time::Instant;

    let mut apc = vss_protocol::ApcStream::with_marker(*vss_protocol::MARKER_R2E);
    let mut typeahead = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + timeout;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok((SnapshotAck::Silent, typeahead));
        }
        let ms = (deadline - now).as_millis().min(u128::from(u16::MAX)) as u16;
        let mut fds = [PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(ms)) {
            Ok(0) => return Ok((SnapshotAck::Silent, typeahead)),
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("poll(stdin) waiting for snapshot ack: {e}")),
        }
        let n = match nix::unistd::read(stdin_fd.as_raw_fd(), &mut buf) {
            Ok(0) => return Ok((SnapshotAck::Silent, typeahead)),
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("read(stdin) waiting for snapshot ack: {e}")),
        };
        let out = apc.feed(&buf[..n]);
        typeahead.extend_from_slice(&probe.feed(&out.passthrough));
        for payload in &out.payloads {
            let mut verdict = None;
            let _ = vss_protocol::for_each_frame(payload, |frame_type, _rid, body| {
                if let Ok(frame) = vss_protocol::frames::UpstreamFrame::parse(frame_type, body)
                {
                    verdict = match frame {
                        vss_protocol::frames::UpstreamFrame::SnapshotAccepted { sequence_id: id } => {
                            Some((id, SnapshotAck::Accepted))
                        }
                        vss_protocol::frames::UpstreamFrame::SnapshotRejected {
                            sequence_id: id,
                            reason,
                        } => Some((id, SnapshotAck::Rejected(reason))),
                        // A late answer to an earlier attach's detach.
                        vss_protocol::frames::UpstreamFrame::DetachAccepted { .. } => verdict,
                    };
                }
                Ok::<(), u16>(())
            });
            // An answer to some other snapshot is not ours to act on.
            if let Some((id, ack)) = verdict
                && id == sequence_id
            {
                return Ok((ack, typeahead));
            }
        }
    }
}

/// Cadence for the mid-attach SIGWINCH watcher. The renderer's stdio
/// is a tty fd that's been handed over to us via `SCM_RIGHTS`; we
/// don't share a controlling tty with it, so the kernel doesn't
/// `SIGWINCH` the daemon process when the user resizes their
/// terminal. We poll `TIOCGWINSZ` on stdin at this interval and
/// `TIOCSWINSZ` the inner PTY master + resize the engines on change.
/// 250 ms picks up a resize within one frame of human reaction time
/// without burning measurable CPU.
const WINSIZE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Synchronous part of the attach dispatch. Runs on the session's
/// accept loop; receives the renderer's stdio fds, takes the
/// session's attached flag, and spawns the per-attach handler thread.
/// Returns `Ok(())` if the handler is now running (in which case the
/// caller replies `Response::Ok`). Errors here mean the attach was
/// refused (already attached, dup failure, thread spawn failure).
///
/// `master_writer` must be a fresh `dup(2)` of the session's inner
/// PTY master — the handler thread owns it for the duration of the
/// attach and closes it on detach.
pub fn start(
    stream: &mut UnixStream,
    engines: Arc<Mutex<EngineState>>,
    master_writer: OwnedFd,
    attached: Arc<AtomicBool>,
    session_name: &str,
) -> Result<()> {
    let (stdin, stdout) =
        fdpass::recv_stdio(stream).with_context(|| "receiving renderer stdio fds")?;

    if attached.swap(true, Ordering::AcqRel) {
        // Race-free check: if the flag was already true, refuse.
        // `stdin` / `stdout` / `master_writer` drop here → fds close.
        return Err(anyhow!("session `{session_name}` is already attached"));
    }
    // From here on, any error path must reset `attached` to false
    // before returning so the session isn't stuck looking attached.
    // Clone the IPC socket and hand the clone to the handler thread.
    // The handler keeps it alive for the duration of the attach so
    // the CLI process can stay blocked on a read, keeping its parent
    // (typically a login shell) backgrounded — without this, the
    // shell regains the tty's foreground process group after the CLI
    // exits and starts reading stdin in parallel with us. With two
    // readers on the same tty, keystrokes are race-distributed
    // between them and inputs appear to drop or duplicate. See the
    // long fix-commit message for the trace data that pinned this
    // down.
    let cli_socket = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            attached.store(false, Ordering::Release);
            return Err(e).context("cloning IPC socket for attach handler");
        }
    };

    let session_name_owned = session_name.to_string();
    let flag_for_thread = Arc::clone(&attached);
    let spawn = std::thread::Builder::new()
        .name("vsd-attach".into())
        .spawn(move || {
            if let Err(e) =
                handler_main(stdin, stdout, master_writer, engines, &cli_socket)
            {
                eprintln!(
                    "vsd: attach `{session_name_owned}` ended: {e:#}"
                );
            }
            flag_for_thread.store(false, Ordering::Release);
            // Dropping `cli_socket` here closes the session's last
            // reference to the IPC socket for this attach. The CLI
            // sees EOF on its blocked read and exits, restoring the
            // user's local shell to the foreground.
            drop(cli_socket);
        });
    if let Err(e) = spawn {
        attached.store(false, Ordering::Release);
        return Err(e).context("spawning attach handler thread");
    }
    Ok(())
}

/// A `sequence_id` for this attach's snapshot, distinct from every
/// other attach's.
///
/// It is how the renderer tells one attach from the next: its
/// `PreAttachBackup` is keyed on this, so a new id means "this is a
/// different attach, stash what is on screen now". Seeded from the
/// clock rather than starting at 1, so a daemon that restarts doesn't
/// hand a renderer the same id its previous stash was keyed on.
fn next_sequence_id() -> u32 {
    use std::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let seed = NEXT.load(Ordering::Acquire);
    if seed == 0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.subsec_nanos() | 1);
        // Racing threads would both seed; attaches are serialized by
        // the `attached` flag, so this can't actually happen.
        NEXT.store(now, Ordering::Release);
    }
    NEXT.fetch_add(1, Ordering::AcqRel)
}

fn dup_owned(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    let raw = nix::unistd::dup(fd.as_raw_fd()).map_err(std::io::Error::other)?;
    // SAFETY: dup(2) returned a fresh fd we now solely own.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn handler_main(
    stdin_fd: OwnedFd,
    stdout_fd: OwnedFd,
    master_writer_fd: OwnedFd,
    engines: Arc<Mutex<EngineState>>,
    ipc_socket: &UnixStream,
) -> Result<()> {
    // Step 0: put the renderer's tty into raw mode. Without this, the
    // SSH PTY slave we just inherited stays in canonical (line-edited)
    // mode with kernel ECHO on — bytes are buffered until newline, the
    // kernel ECHOes input independently of the inner program, and
    // bash's readline ECHO inside the session adds a second layer.
    // The two echo paths collide, the canonical line buffer fights
    // with raw splicing, and the visible result is dropped or
    // duplicated characters during typing.
    //
    // The RawTty guard restores the saved termios when the handler
    // exits — detach via Ctrl+\ d, EOF, splice error, anything.
    let _raw = RawTty::enable(stdin_fd.as_raw_fd());

    // Step 1: probe the renderer for grid size + cell metrics. Falls
    // back to whatever defaults the engines were created with if the
    // renderer doesn't answer in time (non-VGE / non-PRT terminal).
    //
    // Non-probe bytes that arrive during this phase are kept as
    // `typeahead` and forwarded to the inner PTY after we apply the
    // probe results, so the user's keystrokes during attach aren't
    // dropped.
    let mut outcome = probe::run(&stdin_fd, &stdout_fd, PROBE_TIMEOUT)
        .with_context(|| "running upstream probe")?;
    let theme_event = apply_probe(&engines, &master_writer_fd, &outcome);
    // Every write to the inner PTY master from this thread goes
    // through the same lock the worker takes; see
    // `EngineState::master_write`.
    let master_write = {
        let guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        guard.master_write_lock()
    };
    // Ahead of the typeahead: the client should know what palette it is
    // on before it acts on anything the user typed during the attach.
    if !theme_event.is_empty() {
        let _g = master_write.lock().unwrap_or_else(|e| e.into_inner());
        write_all_raw(master_writer_fd.as_raw_fd(), &theme_event)
            .with_context(|| "announcing the renderer's theme")?;
    }
    if !outcome.typeahead.is_empty() {
        let _g = master_write.lock().unwrap_or_else(|e| e.into_inner());
        write_all_raw(master_writer_fd.as_raw_fd(), &outcome.typeahead)
            .with_context(|| "forwarding probe-phase typeahead")?;
    }

    // Step 2: under the engines lock, compute the snapshot and write
    // it to the renderer. We hold the lock for the duration so the
    // per-session worker can't interleave live bytes between snapshot
    // chunks — otherwise the renderer would see "partial replay +
    // post-replay byte + more replay" and paint inconsistent state.
    //
    // We prefix the snapshot with `CSI ?1049 h` (enter alt-screen,
    // save cursor + DECAWM state). The detach path below pairs it
    // with `CSI ?1049 l` so the renderer ends the attach in exactly
    // the screen state it was in before we started — same trick tmux
    // uses for `attach` / `detach`. Without this, the session's
    // final cursor position and any vt100 modes the inner program
    // tweaked (auto-wrap, scroll region, SGR) leak into the user's
    // shell after the session exits.
    //
    // Step 3: while still under the lock, install the stdout fd on
    // engines so the worker starts forwarding live bytes the moment
    // we release.
    let (ack, sequence_id) = {
        let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot: Vec<u8> = Vec::new();
        // No ATTACH_ENTER (alt-screen wrap) for the VSS path: the
        // snapshot's `modes` byte authoritatively sets whether the
        // portal is on main or alt, and `restore_from_binary_snapshot`
        // replaces the portal vt100 state wholesale. If we ran
        // `CSI ?1049 h` first, ATTACH_ENTER would be processed by the
        // portal vt100 *after* the snapshot apply (the bytes flow
        // through the same pipeline), leaving the portal on an empty
        // alt grid with the session's content hidden in main. The
        // tmux-style "restore pre-attach screen on detach" trick the
        // replay path relied on is a v1.1 follow-up — a dedicated
        // VSS Detach frame would tell the renderer to swap back.
        //
        // VSS binary snapshot — replaces the v1 replay-style command
        // stream. The renderer's per-portal VssEngine (or its host-
        // level one, when vsd attaches directly without an
        // intervening vmux pane) reassembles the fragments and applies
        // the three sub-snapshots to its vt100 / VGE / PRT engines via
        // their `restore_from_binary_snapshot` methods. See
        // `doc/session-manager.md` §4.
        let vt_bytes = guard.parser.screen().binary_snapshot();
        let vge_bytes = guard.vge.binary_snapshot();
        let prt_bytes = guard.prt.binary_snapshot();
        let (rows, cols) = guard.parser.screen().size();
        let sequence_id = next_sequence_id();
        let vss_env = vss_protocol::encode_snapshot(
            vss_protocol::SNAPSHOT_VERSION,
            rows,
            cols,
            sequence_id,
            &vt_bytes,
            &vge_bytes,
            &prt_bytes,
            vss_protocol::DEFAULT_MAX_FRAGMENT_BYTES,
        );
        snapshot.extend_from_slice(&vss_env);

        // Best-effort write of the snapshot. A failed write here means
        // the renderer is already gone; we propagate to the caller so
        // the handler exits and the attached flag flips back.
        let stdout_raw = stdout_fd.as_raw_fd();
        write_all_raw(stdout_raw, &snapshot).with_context(|| "writing snapshot")?;

        // §4.4 step 4 — wait for the renderer's verdict *before*
        // handing it live bytes. Still under the engines lock: the
        // worker blocks rather than forwarding output into a pane
        // that has not accepted the state it belongs to, and nothing
        // is lost — the bytes it already read are processed and
        // forwarded once we release.
        let (ack, typeahead) = await_snapshot_ack(
            &stdin_fd,
            sequence_id,
            ACK_TIMEOUT,
            &mut outcome.probe,
        )
        .with_context(|| "waiting for the renderer's snapshot verdict")?;
        if !typeahead.is_empty() {
            let _g = master_write.lock().unwrap_or_else(|e| e.into_inner());
            write_all_raw(master_writer_fd.as_raw_fd(), &typeahead)
                .with_context(|| "forwarding ack-phase typeahead")?;
        }

        // §4.4 step 6 / §4.2 — a refused snapshot means the renderer
        // has none of this session's state. Forwarding live bytes into
        // that pane would paint a session's output over whatever the
        // user was looking at, with no way back. Say why, hold long
        // enough to read it, and tear the attach down.
        if let SnapshotAck::Rejected(reason) = ack {
            let banner = format!(
                "\r\nvsd: cannot attach — {}.\r\n     \
                 Rebuild `veter` and `vsd` from the same commit.\r\n",
                SnapshotAck::reject_reason(reason)
            );
            let _ = write_all_raw(stdout_raw, banner.as_bytes());
            drop(guard);
            std::thread::sleep(REJECT_BANNER_HOLD);
            restore_tty_canonical(stdin_fd.as_raw_fd(), false);
            return Err(anyhow!(
                "renderer rejected the snapshot (reason {reason})"
            ));
        }

        guard.renderer_stdout = Some(
            dup_owned(&stdout_fd).with_context(|| "duping renderer stdout for worker")?,
        );
        (ack, sequence_id)
    };

    // An answer that missed `PROBE_TIMEOUT` and turned up during the
    // verdict wait: apply it now. Late is not useless — the cell
    // metrics and the `host.*` palette are what every client started in
    // this session reads, and dropping them would leave the daemon on
    // its 8x16 defaults for the session's whole life because the link
    // was slow for one second. Outside the engines lock: `apply_probe`
    // takes it itself.
    let (late_vge, late_prt) = outcome.probe.data();
    if (outcome.vge.is_none() && late_vge.is_some())
        || (outcome.prt.is_none() && late_prt.is_some())
    {
        outcome.vge = late_vge;
        outcome.prt = late_prt;
        let theme_event = apply_probe(&engines, &master_writer_fd, &outcome);
        if !theme_event.is_empty() {
            let _g = master_write.lock().unwrap_or_else(|e| e.into_inner());
            write_all_raw(master_writer_fd.as_raw_fd(), &theme_event)
                .with_context(|| "announcing the renderer's theme")?;
        }
    }

    // Step 4: spawn the SIGWINCH watcher so the renderer can resize
    // its window mid-attach. The watcher polls `TIOCGWINSZ` on its
    // own dup of stdin / master and re-applies on change; it
    // self-terminates when the `_watcher` guard is dropped at the
    // end of this function.
    let _watcher = WinsizeWatcher::spawn(
        &stdin_fd,
        &master_writer_fd,
        Arc::clone(&engines),
        outcome.winsize,
    );

    // Step 5: install the shutdown self-pipe so the worker thread can
    // wake the splice when the session dies (inner program EOF /
    // Ctrl+D from the shell). Without this the splice keeps reading
    // the renderer's stdin and the attached terminal looks hung.
    let (shutdown_read, shutdown_write) = nix::unistd::pipe()
        .with_context(|| "creating attach shutdown self-pipe")?;
    {
        let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        guard.attach_shutdown = Some(shutdown_write);
    }

    // Step 6: splice renderer stdin → inner PTY master until EOF /
    // error / shutdown. The worker thread handles the other direction
    // (PTY master output → renderer stdout) via `renderer_stdout` we
    // installed above. Per the PRT spec, input never crosses the
    // engines — we forward keystrokes verbatim.
    //
    // We also watch the IPC socket the `vsd attach` CLI is blocked on:
    // when the renderer's tab/window dies the CLI process exits and its
    // end of the socket closes. That's the only *always-reliable*
    // disconnect signal — the renderer stdin fd only EOFs/HUPs if the
    // multiplexer that owns the pane promptly tears the pty down, which
    // isn't guaranteed (a `vmux` pane can still hold the master open),
    // so relying on stdin alone left the attach spliced forever and the
    // `attached` flag stuck at `true`, refusing every re-attach.
    let mut input = InputState::new();
    let result = splice_input(
        &stdin_fd,
        &master_writer_fd,
        shutdown_read,
        ipc_socket.as_fd(),
        &master_write,
        &mut input,
    );

    // Step 7: detach — clear the renderer-stdout fd and shutdown pipe
    // on engines so the worker stops writing / signaling, then emit a
    // mode-reset sequence so the portal vt100 (which now holds the
    // session's final state) is left with sane defaults for the inner
    // program that will take over the local pane next (typically
    // ssh's bash). Order matters: we clear `renderer_stdout` first so
    // a final worker write can't interleave between our cleanup
    // bytes. The `_watcher` Drop joins its thread before we leave the
    // function.
    {
        let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        guard.renderer_stdout = None;
        guard.attach_shutdown = None;
    }
    // Tell the renderer to restore its pre-attach state (the local
    // shell / vmux pane it was showing before the attach began). The
    // VSS engine on the renderer side stashed that state on the
    // first `SnapshotBegin` of this attach; `DetachNotify` is what
    // pops the stash. After the renderer applies the restore the
    // portal is back to the exact view it had right before attach,
    // including modes.
    let detach_env = vss_protocol::encode_detach_notify(sequence_id);
    let notified = write_all_raw(stdout_fd.as_raw_fd(), &detach_env).is_ok();
    // `ESC c` (RIS) only if nobody took the snapshot. It used to go
    // out unconditionally, a few bytes behind `DetachNotify` — and
    // vt100's `ris()` builds a fresh `Screen`, so it wiped the
    // pre-attach view the notify had just restored, scrollback,
    // `top_of_live_screen` and VGE anchors included. The stash was
    // dead on arrival. When the renderer never accepted the snapshot
    // there is nothing to restore and a reset *is* the right cleanup:
    // the pane still carries whatever modes the session left behind.
    if ack != SnapshotAck::Accepted {
        let _ = write_all_raw(stdout_fd.as_raw_fd(), b"\x1bc");
    }

    // §4.4 — the renderer isn't done with the session just because we
    // are. Whatever it wrote for the session before it saw the
    // `DetachNotify` — a download's in-flight chunks, replies to the
    // session's last commands, the aborts of its transfers — is still
    // on its way here, and once the tty is back in canonical mode the
    // shell reads it as keystrokes. So keep reading until the renderer
    // says it has let go, passing what is the session's on to it. Only
    // a renderer that took the snapshot speaks VSS, and only one that
    // is still there can answer.
    let drained = if notified
        && ack == SnapshotAck::Accepted
        && matches!(result, Ok(SpliceEnd::Detached))
    {
        drain_until_detached(
            &stdin_fd,
            &master_writer_fd,
            ipc_socket.as_fd(),
            &master_write,
            &mut input,
            sequence_id,
        )
    } else {
        true
    };

    // Explicitly restore tty termios here (instead of relying on
    // `RawTty::Drop` only). The Drop path was leaving the stdin tty
    // in raw mode in practice — `stty -a` post-detach showed
    // `-echo -icanon -opost` — so an explicit, logged restore goes
    // first. Loop on EINTR (SIGCHLD from the inner program exit can
    // hit us mid-call). The RawTty guard's Drop still runs after
    // and is a no-op iff we landed here cleanly.
    //
    // A drain that gave up discards whatever is still queued as it
    // restores: it is more likely the session's than the user's.
    restore_tty_canonical(stdin_fd.as_raw_fd(), !drained);

    result.map(|_| ())
}

/// Defensive end-of-attach tty restore: read current termios, OR-in
/// the cooked-mode bits (`ICANON | ECHO | ISIG | IEXTEN`, plus
/// `ICRNL` and `OPOST | ONLCR`), and `tcsetattr` it back. Logs to
/// stderr on failure so we have something to diagnose with. `flush`
/// discards the unread input queue on the way (`TCSAFLUSH`).
fn restore_tty_canonical(fd: std::os::fd::RawFd, flush: bool) {
    use nix::errno::Errno;
    use nix::sys::termios::{tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg};
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut t = match tcgetattr(borrowed) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("vsd: post-detach tcgetattr failed: {e}");
            return;
        }
    };
    t.local_flags |=
        LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::IEXTEN;
    t.input_flags |= InputFlags::ICRNL;
    t.output_flags |= OutputFlags::OPOST | OutputFlags::ONLCR;
    let when = if flush { SetArg::TCSAFLUSH } else { SetArg::TCSANOW };
    let mut attempts = 0u8;
    loop {
        match tcsetattr(borrowed, when, &t) {
            Ok(()) => return,
            Err(Errno::EINTR) if attempts < 5 => {
                attempts += 1;
                continue;
            }
            Err(e) => {
                eprintln!(
                    "vsd: post-detach tcsetattr failed after {attempts} retries: {e}"
                );
                return;
            }
        }
    }
}

/// Per-attach SIGWINCH watcher. The daemon doesn't share a
/// controlling tty with the renderer's PTY slave (the attach handler
/// runs in a worker thread of a process started independently of the
/// SSH login session), so the kernel never delivers `SIGWINCH` to us
/// directly. We poll `TIOCGWINSZ` on the renderer's stdin and
/// `TIOCSWINSZ` the inner PTY + resize the engines whenever the size
/// changes. See [`WINSIZE_POLL_INTERVAL`] for the cadence trade-off.
struct WinsizeWatcher {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WinsizeWatcher {
    /// Spawn the watcher. `initial` is the size we already applied
    /// during the probe so we don't trip the "size changed" branch on
    /// the very first poll. The fds are dup'd so the watcher's
    /// lifetime is independent of the caller's fds — dropping this
    /// guard cleanly closes its dups.
    fn spawn(
        stdin_fd: &OwnedFd,
        master_fd: &OwnedFd,
        engines: Arc<Mutex<EngineState>>,
        initial: Option<probe::Winsize>,
    ) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        // Dup fails extraordinarily rarely (EMFILE). On failure we
        // skip spawning the watcher; mid-attach resize just won't
        // work for this attach. The handler logs and proceeds.
        let stdin_dup = match dup_owned(stdin_fd) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("vsd: winsize watcher: dup(stdin) failed: {e}; resize disabled for this attach");
                return Self { stop, handle: None };
            }
        };
        let master_dup = match dup_owned(master_fd) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("vsd: winsize watcher: dup(master) failed: {e}; resize disabled for this attach");
                return Self { stop, handle: None };
            }
        };
        let handle = std::thread::Builder::new()
            .name("vsd-winsize".into())
            .spawn(move || {
                winsize_main(stdin_dup, master_dup, engines, initial, stop_for_thread)
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for WinsizeWatcher {
    fn drop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn winsize_main(
    stdin_fd: OwnedFd,
    master_fd: OwnedFd,
    engines: Arc<Mutex<EngineState>>,
    initial: Option<probe::Winsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let raw_stdin = stdin_fd.as_raw_fd();
    let raw_master = master_fd.as_raw_fd();
    let mut last = initial;
    while !stop.load(std::sync::atomic::Ordering::Acquire) {
        std::thread::sleep(WINSIZE_POLL_INTERVAL);
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let Some(ws) = probe::read_winsize(raw_stdin) else {
            // ioctl failed (stdin closed?) — back off and let the
            // splice loop notice the disconnect.
            continue;
        };
        // Comparing the whole winsize (not just rows/cols) means a
        // font-size or DPI change on the renderer, which moves the
        // pixel dimensions alone, still reaches the inner program.
        if last == Some(ws) {
            continue;
        }
        last = Some(ws);

        // Apply: resize the parser, then SIGWINCH the inner program
        // via TIOCSWINSZ on the master. The inner program's redraw
        // bytes flow through the worker thread, which forwards them
        // to the (still-attached) renderer.
        {
            let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
            guard.resize(ws.rows, ws.cols);
        }
        probe::set_inner_winsize(raw_master, ws);
    }
}

/// Apply probe + winsize results to the per-session engines and to
/// the inner PTY master. SIGWINCHing the inner program here means
/// its redraw bytes arrive before the snapshot is serialized, so the
/// snapshot reflects the renderer's actual grid (modulo whatever
/// races the inner program loses to our `engines.lock()`).
/// Returns bytes the caller must write to the inner PTY master — the
/// theme announcement, when the attaching renderer brought a palette.
/// Empty otherwise.
fn apply_probe(
    engines: &Arc<Mutex<EngineState>>,
    master_writer_fd: &OwnedFd,
    outcome: &probe::ProbeOutcome,
) -> Vec<u8> {
    let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(ws) = outcome.winsize {
        guard.resize(ws.rows, ws.cols);
        // SIGWINCH the inner program so its next redraw is at the
        // right size. Best effort; the engines have already been
        // resized so a stale dimension on the slave's tty is the only
        // failure mode.
        probe::set_inner_winsize(master_writer_fd.as_raw_fd(), ws);
    }
    if let Some(vge) = outcome.vge {
        let cell_px = (vge.cell_pixel_width, vge.cell_pixel_height);
        guard.vge.set_dimensions(cell_px, vge.scale_factor);
        guard.prt.set_metrics(cell_px, vge.scale_factor);
    }
    let Some(palette) = outcome.prt.and_then(accent_palette) else {
        return Vec::new();
    };
    // §7.3 — the daemon paints nothing, so it has no palette of its
    // own; it borrows the attached renderer's, the same way it borrows
    // its cell metrics, and keeps it after a detach so a client started
    // in a detached session still gets a themed accent.
    guard.vge.seed_host_styles(palette.clone(), 0);
    guard.prt.set_host_palette(palette.clone());

    // Announce it to the session's client (§8.13).
    //
    // This has to be written from here rather than left to the worker,
    // and it is the one piece of PRT output the daemon emits while a
    // renderer is attached. The suppression it steps around exists
    // because the renderer runs the same commands off the forwarded
    // chunk and answers them itself — a rule about *replies*, which
    // carry a request id that would then be answered twice. An
    // unsolicited event answers nothing, and the renderer emits none of
    // these: its palette is set once at startup, before the session's
    // client exists, so nothing on its side ever changes.
    //
    // Which is also why the client needs telling at all. Reattaching
    // does not change any palette — it swaps in a renderer that may
    // have a *different* one, and the client is a long-lived process
    // still holding the colours it read from whichever renderer it
    // probed first.
    theme_announcement(&palette)
}

/// One `HostThemeChanged` event (§8.13) as a host→client envelope, for
/// the session's client to read off its own stdin. Empty when the
/// palette has no accent to report, since the event's whole payload is
/// the palette.
fn theme_announcement(palette: &veter_host::vge::HostThemePalette) -> Vec<u8> {
    // Depth 0: the session's client is the top-level one.
    let Some(accent) = palette.contextual_rgba8(0) else {
        return Vec::new();
    };
    let body = prt_protocol::envelope::host_theme_changed_body(
        "",
        accent,
        palette.colors.map(veter_host::vge::HostThemeColors::to_rgba8),
    );
    let mut frames = Vec::new();
    prt_protocol::envelope::append_frame(
        &mut frames,
        prt_protocol::frame::EVT_HOST_THEME_CHANGED,
        0,
        &body,
    );
    prt_protocol::envelope::wrap_t2c_envelope(&frames)
}

#[cfg(test)]
mod theme_announcement_tests {
    use super::*;
    use prt_protocol::frame::{EVT_HOST_THEME_CHANGED, MARKER_T2C};

    fn color(r: f32, g: f32, b: f32) -> vge_protocol::command::Color {
        vge_protocol::command::Color { r, g, b, a: 1.0 }
    }

    /// What the session's client actually reads off its stdin: a
    /// well-formed t2c envelope carrying the event, decodable by the
    /// same parser every other host→client frame goes through.
    #[test]
    fn the_announcement_decodes_as_a_host_theme_changed_event() {
        let colors = veter_host::vge::HostThemeColors {
            bg: color(0.0, 0.0, 0.0),
            fg: color(1.0, 1.0, 1.0),
            surface: color(0.2, 0.2, 0.2),
            surface_inset: color(0.1, 0.1, 0.1),
            text: color(0.9, 0.9, 0.9),
            text_dim: color(0.5, 0.5, 0.5),
            text_on_accent: color(1.0, 1.0, 1.0),
            warn: color(1.0, 0.0, 0.0),
        };
        let palette = veter_host::vge::HostThemePalette {
            accents: vec![color(1.0, 0.0, 0.0)],
            colors: Some(colors),
        };

        let bytes = theme_announcement(&palette);
        let mut apc = prt_protocol::apc::ApcStream::with_marker(*MARKER_T2C);
        let payload = apc
            .feed(&bytes)
            .into_payloads()
            .next()
            .expect("one envelope");

        let mut r = prt_protocol::codec::Reader::new(&payload);
        let _version = r.u8().unwrap();
        let _payload_len = r.u32().unwrap();
        assert_eq!(r.u8().unwrap(), EVT_HOST_THEME_CHANGED);
        let _rid = r.u32().unwrap();
        let body_len = r.u32().unwrap() as usize;
        let body = r.take(body_len).unwrap();

        let (id, accent, got) =
            prt_protocol::envelope::parse_host_theme_changed(body).unwrap();
        assert_eq!(id, "", "the host itself names no portal");
        assert_eq!(accent, [255, 0, 0, 255]);
        assert_eq!(got, Some(colors.to_rgba8()));
        assert!(r.at_end(), "exactly one frame");
    }

    /// A renderer that themes nothing leaves the client on its own
    /// colours rather than being told about a palette that isn't there.
    #[test]
    fn an_empty_palette_announces_nothing() {
        assert!(
            theme_announcement(&veter_host::vge::HostThemePalette::default()).is_empty()
        );
    }
}

/// The renderer's `host.*` palette, if it themes them at all.
///
/// The probe reports one *accent* — the one `host.accent` resolves to
/// at the probing engine's depth — not the renderer's whole accent
/// list, so every depth inside the session resolves to that same
/// accent while the daemon is the one answering. The rest of the
/// palette does not vary with depth, so it comes across whole. A
/// renderer re-seeds its own palette over both on attach.
fn accent_palette(prt: probe::PrtProbeData) -> Option<veter_host::vge::HostThemePalette> {
    use prt_protocol::frame::FEAT_VGE_HOST_THEMED_STYLES;
    if prt.vge_features? & FEAT_VGE_HOST_THEMED_STYLES == 0 {
        return None;
    }
    let [r, g, b, a] = prt.accent_rgba?;
    Some(veter_host::vge::HostThemePalette {
        accents: vec![vge_protocol::command::Color {
            r: f32::from(r) / 255.0,
            g: f32::from(g) / 255.0,
            b: f32::from(b) / 255.0,
            a: f32::from(a) / 255.0,
        }],
        colors: prt
            .theme_rgba
            .map(veter_host::vge::HostThemeColors::from_rgba8),
    })
}

/// Detach hotkey prefix byte. Per `doc/session-manager.md` §6 vsd
/// owns the trigger, not local vmux; `Ctrl+\` is distinct from
/// vmux's default `Ctrl+Space` so the two can never collide.
const DETACH_PREFIX: u8 = 0x1C; // Ctrl+\
const DETACH_SECOND: u8 = b'd';

/// Outcome of feeding one chunk of renderer-stdin bytes through the
/// detach-hotkey state machine.
struct ScanOutput {
    /// Bytes ready to be written to the inner PTY master.
    forward: Vec<u8>,
    /// True if the chunk contained the detach sequence; the caller
    /// should write `forward` (the bytes that arrived before the
    /// trigger) and then exit the splice loop cleanly.
    detach: bool,
}

/// The envelopes that ride the renderer's stdin: the host→client
/// direction of every protocol in the family. Their bodies are payload
/// on their way to the session's client, not keystrokes, and a binary
/// one — a VFT download, which reaches a `vmux` inside the session as a
/// PRT `RawReply` — carries a `Ctrl+\` `d` pair about once every 64 KiB,
/// since stuffing leaves both bytes alone.
///
/// Known markers rather than any `ESC _`: a user typing `Esc _` (a vim
/// motion) must not switch the hotkey off until some `ESC \` happens
/// along, and nobody types `Esc _ vft`. A protocol added to the family
/// has to be added here.
const INPUT_MARKERS: [&[u8; 3]; 5] = [
    prt_protocol::frame::MARKER_T2C,
    vge_protocol::frame::MARKER_T2C,
    vft_protocol::frame::MARKER_H2C,
    ses_protocol::frame::MARKER_H2C,
    vss_protocol::frame::MARKER_R2E,
];

/// Where the renderer's stdin is relative to the envelopes in it.
///
/// Observation only: the scanner holds back nothing but a detach
/// prefix, so it needs none of the escape-time recovery the protocol
/// parsers carry. An envelope split across reads is tracked the same
/// whatever the gap between them.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Envelope {
    /// Keystrokes.
    #[default]
    Outside,
    /// Saw `ESC` outside an envelope.
    Esc,
    /// Saw `ESC _` and this many bytes of a marker in [`INPUT_MARKERS`].
    Marker([u8; 3], usize),
    /// Inside an envelope body.
    Body,
    /// Saw `ESC` inside a body: `ESC ESC` and `ESC <mark>` are stuffed
    /// payload, `ESC \` closes it.
    BodyEsc,
}

impl Envelope {
    /// Advance past `b`. Returns whether `b` belongs to an envelope
    /// body (or is the `ESC \` that closes one) and so is not a
    /// keystroke.
    fn step(&mut self, b: u8) -> bool {
        const ESC: u8 = 0x1B;
        let (next, payload) = match (*self, b) {
            (Envelope::Body, ESC) => (Envelope::BodyEsc, true),
            (Envelope::Body, _) => (Envelope::Body, true),
            (Envelope::BodyEsc, b'\\') => (Envelope::Outside, true),
            // Stuffing turns every ESC in a body into `ESC ESC`, so an
            // `ESC _` can't be payload: the body we thought we were in
            // was never one (the same call `ApcOtherEsc` makes in the
            // protocol parsers), and this is an opener.
            (Envelope::BodyEsc, b'_') => (Envelope::Marker([0; 3], 0), false),
            (Envelope::BodyEsc, _) => (Envelope::Body, true),
            (_, ESC) => (Envelope::Esc, false),
            (Envelope::Esc, b'_') => (Envelope::Marker([0; 3], 0), false),
            (Envelope::Marker(mut marker, len), _) => {
                marker[len] = b;
                let prefix = &marker[..=len];
                if !INPUT_MARKERS.iter().any(|m| m.starts_with(prefix)) {
                    (Envelope::Outside, false)
                } else if len + 1 == marker.len() {
                    (Envelope::Body, false)
                } else {
                    (Envelope::Marker(marker, len + 1), false)
                }
            }
            (Envelope::Outside | Envelope::Esc, _) => (Envelope::Outside, false),
        };
        *self = next;
        payload
    }
}

/// State carried between chunks of renderer-stdin so the prefix
/// scan works even if the user types `Ctrl+\` and `d` arrive in
/// separate reads.
#[derive(Default)]
struct DetachScanner {
    /// True iff the *last* byte we saw was the detach prefix and we
    /// haven't yet decided what to do with it — i.e. we owe the
    /// inner PTY one prefix byte unless the next byte cancels it
    /// (which only happens for `d`).
    pending_prefix: bool,
    /// Only keystrokes can trigger a detach; see [`INPUT_MARKERS`].
    envelope: Envelope,
    /// Set once the attach is over: from then on only envelope bytes
    /// are forwarded. A keystroke typed after a detach was meant for
    /// the shell the pane is going back to, not the session — though
    /// that shell can't be handed it either, so it is dropped.
    draining: bool,
    /// While draining, the start of what may be an envelope opener
    /// (`ESC`, `ESC _`, part of a marker). Until the marker settles it
    /// could as well be a typed `Esc _`, so it is released with the
    /// envelope or dropped with the keystrokes.
    opener: Vec<u8>,
}

impl DetachScanner {
    /// Feed one chunk of renderer-stdin bytes and split them into
    /// "forward to inner PTY" and "detach detected" outputs.
    ///
    /// Trade-off: a lone `Ctrl+\` does not reach the inner PTY
    /// until the user types something afterwards. This is the same
    /// shape as tmux/screen prefix keys; the follow-up byte is
    /// usually right behind.
    fn feed(&mut self, chunk: &[u8]) -> ScanOutput {
        let mut out = Vec::with_capacity(chunk.len() + 1);
        let mut detach = false;
        for &b in chunk {
            // A pending prefix is always resolved by the next byte, and
            // envelope bytes only follow an `ESC _`, so no prefix is
            // ever pending across this.
            if self.envelope.step(b) {
                out.push(b);
            } else if self.draining {
                match self.envelope {
                    // Any ESC outside a body starts over.
                    Envelope::Esc => {
                        self.opener.clear();
                        self.opener.push(b);
                    }
                    Envelope::Marker(..) => self.opener.push(b),
                    // That byte completed a marker.
                    Envelope::Body => {
                        out.append(&mut self.opener);
                        out.push(b);
                    }
                    // A keystroke after the attach ended; see `draining`.
                    Envelope::Outside | Envelope::BodyEsc => self.opener.clear(),
                }
            } else if self.pending_prefix {
                if b == DETACH_SECOND {
                    // The trigger ends the attach, not the chunk: an
                    // envelope for the session may still follow it.
                    self.start_draining();
                    detach = true;
                    continue;
                }
                // Not a detach — release the buffered prefix.
                out.push(DETACH_PREFIX);
                if b == DETACH_PREFIX {
                    // Another prefix arrived immediately; stay
                    // pending for the next byte.
                    self.pending_prefix = true;
                } else {
                    out.push(b);
                    self.pending_prefix = false;
                }
            } else if b == DETACH_PREFIX {
                self.pending_prefix = true;
            } else {
                out.push(b);
            }
        }
        ScanOutput { forward: out, detach }
    }

    /// The attach is over, whichever way it ended: forward only what
    /// belongs to the session from here on.
    fn start_draining(&mut self) {
        self.draining = true;
        self.pending_prefix = false;
    }

    /// On stdin EOF, flush any buffered prefix so the inner PTY
    /// sees the byte the user typed (writes to a dying tty are
    /// benign).
    fn flush_on_eof(&mut self) -> Option<u8> {
        if std::mem::take(&mut self.pending_prefix) {
            Some(DETACH_PREFIX)
        } else {
            None
        }
    }
}

/// How [`splice_input`] stopped, when it stopped cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpliceEnd {
    /// The attach ended with the renderer still there — the detach
    /// hotkey, a SES `Detach`, the session exiting — so whatever it
    /// had in flight for the session is still on its way.
    Detached,
    /// The renderer went away: its stdin closed, or the `vsd attach`
    /// CLI did.
    RendererGone,
}

/// The parse state of the renderer's stdin. It outlives the splice: an
/// attach can end mid-envelope (a SES `Detach` lands between two
/// reads), and [`drain_until_detached`] has to pick the stream up
/// exactly where [`splice_input`] left it — a fresh parser would take
/// the rest of that envelope for keystrokes.
struct InputState {
    /// Strips the renderer's upstream VSS envelopes (`ESC _ vss …
    /// ESC \`). The attach-time `SnapshotAccepted` was already read by
    /// `await_snapshot_ack` before the splice began, so during the
    /// splice anything here is a straggler, dropped; during the drain
    /// it is where `DetachAccepted` turns up. Neither may reach the
    /// inner shell: `ESC _` is meta-paren and the payload chars get
    /// inserted as literal keystrokes.
    vss_filter: vss_protocol::ApcStream,
    scanner: DetachScanner,
    trace_log: Option<std::fs::File>,
}

impl InputState {
    fn new() -> Self {
        Self {
            vss_filter: vss_protocol::ApcStream::with_marker(*vss_protocol::MARKER_R2E),
            scanner: DetachScanner::default(),
            trace_log: open_input_trace(),
        }
    }

    fn trace(&mut self, chunk: &[u8]) {
        if let Some(log) = self.trace_log.as_mut() {
            let _ = log_input_chunk(log, chunk);
        }
    }

    /// One read during the drain: what to forward to the session, and
    /// whether the renderer has answered this attach's `DetachNotify`.
    fn drain_chunk(&mut self, chunk: &[u8], sequence_id: u32) -> (Vec<u8>, bool) {
        let vss_out = self.vss_filter.feed(chunk);
        let answered = vss_out
            .payloads
            .iter()
            .any(|payload| answers_detach(payload, sequence_id));
        (self.scanner.feed(&vss_out.passthrough).forward, answered)
    }
}

/// Whether an upstream VSS payload carries `DetachAccepted` for this
/// attach. One for another attach is a late answer to a drain that
/// gave up, and proves nothing about ours.
fn answers_detach(payload: &[u8], sequence_id: u32) -> bool {
    let mut found = false;
    let _ = vss_protocol::for_each_frame(payload, |frame_type, _rid, body| {
        if let Ok(vss_protocol::UpstreamFrame::DetachAccepted { sequence_id: id }) =
            vss_protocol::UpstreamFrame::parse(frame_type, body)
        {
            found |= id == sequence_id;
        }
        Ok::<(), u16>(())
    });
    found
}

/// Renderer-stdin → inner-PTY-master forwarding loop. Returns on EOF
/// of stdin (renderer disconnected cleanly), on write error (inner
/// program gone), when the [`DetachScanner`] fires, when the
/// `shutdown_read` self-pipe becomes readable (the per-session worker
/// signaled session shutdown — typically because the inner program
/// exited), or when `ipc_fd` (the socket the `vsd attach` CLI blocks
/// on) becomes ready — the CLI closing it is the reliable "the
/// renderer's tab/window died" signal that doesn't depend on the
/// multiplexer having torn down the renderer's pty (see the call site).
/// Note `stdin_fd` is borrowed, not moved: the caller (`handler_main`)
/// needs the OwnedFd alive *after* this returns so the post-detach
/// tty restore (`restore_tty_canonical` + `RawTty::Drop`) can call
/// `tcsetattr` on a live fd. An earlier version took it by value,
/// which closed the fd on return and left every restore attempt
/// silently failing with `EBADF` — the user's tty stayed in raw mode
/// (`-echo -icanon -opost`) until they ran `reset` by hand. The master
/// writer is borrowed for the same kind of reason: the drain after a
/// detach still writes to it. `shutdown_read` is attach-private and
/// closes here.
fn splice_input(
    stdin_fd: &OwnedFd,
    master_writer_fd: &OwnedFd,
    shutdown_read: OwnedFd,
    ipc_fd: BorrowedFd<'_>,
    master_write: &Mutex<()>,
    input: &mut InputState,
) -> Result<SpliceEnd> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    let stdin_raw = stdin_fd.as_raw_fd();
    let writer_raw = master_writer_fd.as_raw_fd();
    // The worker thread writes engine replies to the same master. A
    // renderer frame larger than the 4 KiB pty input buffer goes out
    // in pieces, and without this lock a PRT event lands in the
    // middle of one — corrupting the envelope for the remote vmux or
    // vrecv on the other end. See `EngineState::master_write`.
    let write_master = |data: &[u8]| -> Result<()> {
        let _g = master_write.lock().unwrap_or_else(|e| e.into_inner());
        write_all_raw(writer_raw, data)
    };
    let mut buf = [0u8; 4096];
    // The escape-time window for bytes `vss_filter` holds back: a lone
    // Esc landing in its `EscPending` state has to reach the inner PTY
    // when no follow-up byte arrives, or vim never sees the mode
    // switch. Same trade-off as vmux's `ESCAPE_TIME_MS`, and as the
    // terminfo `ESCDELAY` curses apps use — ncurses documents 1000 ms
    // there, which nobody actually waits any more.
    //
    // This window sits in series with the renderer's, so it is the
    // smaller of the two: what it has to outwait is a straggling
    // snapshot ack split across reads on a local pty, not a network
    // gap. Tracked as a deadline rather than "a poll that saw no
    // stdin", so an attach that is holding nothing costs no wakeups.
    const ESCAPE_TIME: Duration = Duration::from_millis(25);
    const IDLE_TICK_MS: u16 = 1000;
    let mut esc_deadline: Option<std::time::Instant> = None;
    loop {
        let mut fds = [
            PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN),
            PollFd::new(shutdown_read.as_fd(), PollFlags::POLLIN),
            PollFd::new(ipc_fd, PollFlags::POLLIN),
        ];
        let timeout = match esc_deadline {
            Some(deadline) => u16::try_from(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u16::MAX),
            None => IDLE_TICK_MS,
        };
        match poll(&mut fds, PollTimeout::from(timeout)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("splice poll: {e}")),
        }

        // Shutdown beats stdin: if the session is gone we want to
        // tear down even if there are buffered keystrokes.
        let shutdown_revents = fds[1].revents().unwrap_or(PollFlags::empty());
        if shutdown_revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            return Ok(SpliceEnd::Detached);
        }

        // The IPC socket the `vsd attach` CLI holds open for the whole
        // attach: any readability or hangup means the CLI process is
        // gone (its tab/window died). The CLI is contractually silent
        // after the attach handshake, so we don't read — a live fd only
        // becomes ready here on peer close. Treat it as a detach so the
        // handler unwinds and the `attached` flag clears even when the
        // renderer's pty master is still held open by the multiplexer.
        let ipc_revents = fds[2].revents().unwrap_or(PollFlags::empty());
        if ipc_revents
            .intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR)
        {
            return Ok(SpliceEnd::RendererGone);
        }

        let stdin_revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if !stdin_revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            // No input this pass. Once the window has elapsed, what is
            // held was typed: release it so vim et al. see the
            // mode-switch keystroke at all. A flush out of `ApcPrefix`
            // lets the ESC go and keeps the `_`, so the window may have
            // to run again for the rest.
            if esc_deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                let flushed = input.vss_filter.flush_pending_esc();
                esc_deadline = input
                    .vss_filter
                    .has_deferred_bytes()
                    .then(|| std::time::Instant::now() + ESCAPE_TIME);
                if !flushed.is_empty() {
                    let out = input.scanner.feed(&flushed);
                    if !out.forward.is_empty() {
                        write_master(&out.forward)
                            .with_context(|| "writing flushed Esc to inner PTY")?;
                    }
                    if out.detach {
                        return Ok(SpliceEnd::Detached);
                    }
                }
            }
            continue;
        }

        let n = match nix::unistd::read(stdin_raw, &mut buf) {
            Ok(0) => {
                if let Some(b) = input.scanner.flush_on_eof() {
                    let _ = write_master(&[b]);
                }
                return Ok(SpliceEnd::RendererGone);
            }
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("renderer stdin read: {e}")),
        };
        input.trace(&buf[..n]);

        // Filter out any renderer-side VSS envelopes; what's left is
        // user keystrokes and the envelopes bound for the session's
        // client, which the detach scanner tells apart.
        let vss_out = input.vss_filter.feed(&buf[..n]);
        // Bytes that just arrived restart the window: they are evidence
        // that the rest of a split envelope is on its way.
        esc_deadline = input
            .vss_filter
            .has_deferred_bytes()
            .then(|| std::time::Instant::now() + ESCAPE_TIME);
        if vss_out.passthrough.is_empty() {
            continue;
        }

        let out = input.scanner.feed(&vss_out.passthrough);
        if !out.forward.is_empty() {
            write_master(&out.forward)
                .with_context(|| "writing renderer input to inner PTY")?;
        }
        if out.detach {
            return Ok(SpliceEnd::Detached);
        }
    }
}

/// Read the renderer's stdin after a `DetachNotify` until it answers
/// `DetachAccepted` for this attach (§4.4), forwarding the envelopes
/// on it to the session and dropping everything else.
///
/// The envelopes are the session's: replies and events the renderer
/// produced for it before the notify reached it, which its programs
/// may be blocked on, and the `TransferAborted` that tells a `vrecv`
/// its download is over. The renderer answers only once it has let go
/// of the session, so nothing after the answer is.
///
/// Returns whether the answer came. Gives up after [`DETACH_ACK_IDLE`]
/// of silence or [`DETACH_ACK_CAP`] in all, and at once when the
/// renderer goes away.
fn drain_until_detached(
    stdin_fd: &OwnedFd,
    master_writer_fd: &OwnedFd,
    ipc_fd: BorrowedFd<'_>,
    master_write: &Mutex<()>,
    input: &mut InputState,
    sequence_id: u32,
) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::time::Instant;

    input.scanner.start_draining();
    let cap = Instant::now() + DETACH_ACK_CAP;
    let mut idle = Instant::now() + DETACH_ACK_IDLE;
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        let deadline = cap.min(idle);
        if now >= deadline {
            return false;
        }
        let ms = u16::try_from((deadline - now).as_millis()).unwrap_or(u16::MAX);
        let mut fds = [
            PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN),
            PollFd::new(ipc_fd, PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::from(ms)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return false,
        }
        let ipc_revents = fds[1].revents().unwrap_or(PollFlags::empty());
        if ipc_revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR) {
            return false;
        }
        if fds[0].revents().unwrap_or(PollFlags::empty()).is_empty() {
            continue;
        }
        let n = match nix::unistd::read(stdin_fd.as_raw_fd(), &mut buf) {
            Ok(0) => return false,
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return false,
        };
        idle = Instant::now() + DETACH_ACK_IDLE;
        input.trace(&buf[..n]);

        let (forward, answered) = input.drain_chunk(&buf[..n], sequence_id);
        if !forward.is_empty() {
            let _g = master_write.lock().unwrap_or_else(|e| e.into_inner());
            // A session that has exited can't take it, and that is
            // fine: the point of reading it was to keep it off the
            // renderer's shell.
            let _ = write_all_raw(master_writer_fd.as_raw_fd(), &forward);
        }
        if answered {
            return true;
        }
    }
}

/// Loop around `nix::unistd::write` so a short write or EINTR doesn't
/// drop bytes on the floor. Writes to a `RawFd` directly so we don't
/// have to clone an `OwnedFd` into a `File` and back.
/// Open the splice-input trace file when `VETERD_DEBUG_INPUT=1` is set.
/// One file per attach (truncated on open) so consecutive runs don't
/// mix. Returns `None` if the env var is unset or the file can't be
/// opened — tracing is purely diagnostic.
fn open_input_trace() -> Option<std::fs::File> {
    if std::env::var_os("VETERD_DEBUG_INPUT")
        .map(|v| v != "0" && !v.is_empty())
        != Some(true)
    {
        return None;
    }
    let dir = crate::runtime::runtime_dir();
    let path = dir.join("input.log");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// Append one chunk of received bytes to the trace log as
/// `[seconds.millis] hexdump  |ascii|` — easy to eyeball.
fn log_input_chunk(log: &mut std::fs::File, chunk: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut line = format!(
        "[{:>10}.{:03}] {:3} bytes: ",
        ts.as_secs(),
        ts.subsec_millis(),
        chunk.len()
    );
    for &b in chunk {
        line.push_str(&format!("{:02X} ", b));
    }
    line.push('|');
    for &b in chunk {
        line.push(if b.is_ascii_graphic() || b == b' ' {
            b as char
        } else {
            '.'
        });
    }
    line.push_str("|\n");
    log.write_all(line.as_bytes())
}

fn write_all_raw(raw: std::os::fd::RawFd, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
        match nix::unistd::write(borrowed, data) {
            Ok(0) => return Err(anyhow!("write returned 0")),
            Ok(n) => data = &data[n..],
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("write: {e}")),
        }
    }
    Ok(())
}

/// RAII guard that flips the renderer's tty into raw mode for the
/// duration of an attach and restores the previous attributes on drop.
///
/// Why this matters: the fds we receive over `SCM_RIGHTS` reference the
/// renderer's controlling tty (the SSH PTY slave for a remote attach,
/// or a local terminal for a local one). Whatever termios that tty was
/// in when the CLI handed off is what we inherit — usually canonical
/// mode with kernel ECHO on, because that's how bash leaves its tty
/// at the prompt.
///
/// Canonical mode line-buffers stdin (the daemon only sees bytes once
/// the user hits Enter) and ECHOes keystrokes from the kernel. Inside
/// the session the inner bash's readline ALSO ECHOes via its own
/// line discipline, so the user gets two echo paths fighting each
/// other plus a kernel line buffer ahead of our splice loop. Symptom:
/// characters appear to drop or duplicate at random as the two
/// pipelines drift.
///
/// vmux and tmux both put their own tty in raw mode for the same
/// reason. We do exactly that here, and restore on drop so a detach
/// (Ctrl+\ d, EOF, or any error) leaves the user's shell in the
/// termios state they started in.
struct RawTty {
    fd: std::os::fd::RawFd,
    saved: Option<nix::sys::termios::Termios>,
}

impl RawTty {
    /// Enable raw mode on `fd`. Returns a guard that restores the
    /// saved termios on drop. Failure to read or apply the termios is
    /// logged and the guard becomes a no-op; better to attach without
    /// raw mode than to refuse the attach entirely.
    fn enable(fd: std::os::fd::RawFd) -> Self {
        use nix::sys::termios::{
            tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg,
        };
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
        let saved = match tcgetattr(borrowed) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("vsd: tcgetattr on renderer stdin failed: {e}; \
                           attaching without raw mode");
                return Self { fd, saved: None };
            }
        };
        let mut raw = saved.clone();
        // Mirror what vmux's RawTty guard does: drop the line discipline
        // bits that fight a raw splice (ICANON, ECHO, signals from
        // user-typed keys), drop output post-processing on this fd
        // (output goes to the renderer via a separate channel anyway),
        // and disable input transformations like XON/XOFF and CR↔LF
        // remapping so the bytes the program sees match what was typed.
        raw.local_flags &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ISIG);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.input_flags &= !(InputFlags::IXON
            | InputFlags::IXOFF
            | InputFlags::INLCR
            | InputFlags::ICRNL
            | InputFlags::IGNCR);
        if let Err(e) = tcsetattr(borrowed, SetArg::TCSANOW, &raw) {
            eprintln!("vsd: tcsetattr raw on renderer stdin failed: {e}; \
                       attaching without raw mode");
            return Self { fd, saved: None };
        }
        Self {
            fd,
            saved: Some(saved),
        }
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        if let Some(mut restored) = self.saved.take() {
            use nix::sys::termios::{tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg};
            // Belt-and-suspenders: whatever state the tty was in when
            // we enabled raw mode (potentially mid-readline, mid-vim,
            // mid-anything), assert a sane post-detach cooked tty
            // here so the local shell that takes over isn't stuck
            // without echo or line discipline. The user's shell will
            // typically tweak modes again on its first readline
            // cycle; this is just so they can SEE their typing while
            // they get there.
            restored.local_flags |=
                LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG | LocalFlags::IEXTEN;
            restored.input_flags |= InputFlags::ICRNL;
            restored.output_flags |= OutputFlags::OPOST | OutputFlags::ONLCR;
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd) };
            // Best-effort restore — at this point the attach is ending
            // and we can't do anything useful with the error.
            let _ = tcsetattr(borrowed, SetArg::TCSANOW, &restored);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_chunks(chunks: &[&[u8]]) -> (Vec<u8>, bool) {
        let mut s = DetachScanner::default();
        let mut forwarded = Vec::new();
        for chunk in chunks {
            let out = s.feed(chunk);
            forwarded.extend_from_slice(&out.forward);
            if out.detach {
                return (forwarded, true);
            }
        }
        if let Some(b) = s.flush_on_eof() {
            forwarded.push(b);
        }
        (forwarded, false)
    }

    /// Feed `bytes` through a pipe as if the renderer had sent them,
    /// and ask what verdict came back.
    fn ack_of(bytes: &[u8], timeout_ms: u64) -> (SnapshotAck, Vec<u8>) {
        ack_and_probe(bytes, timeout_ms).0
    }

    /// The same, keeping the probe parsers so a test can ask what they
    /// picked up along the way.
    fn ack_and_probe(
        bytes: &[u8],
        timeout_ms: u64,
    ) -> ((SnapshotAck, Vec<u8>), probe::Probe) {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        nix::unistd::write(&write_fd, bytes).expect("write");
        drop(write_fd);
        let mut probe = probe::Probe::new();
        let out = await_snapshot_ack(
            &read_fd,
            42,
            Duration::from_millis(timeout_ms),
            &mut probe,
        )
        .expect("ack");
        (out, probe)
    }

    /// One PRT probe response, as the renderer answers the daemon's
    /// attach probe.
    fn prt_probe_response() -> Vec<u8> {
        let body = prt_protocol::envelope::ProbeBody {
            protocol_version: 1,
            max_portals: 64,
            max_portal_cells_w: 1024,
            max_portal_cells_h: 512,
            max_scrollback_lines: 100_000,
            max_write_bytes: 1 << 20,
            features: 0xFF,
            max_nesting_depth: 8,
            vge_features: None,
            accent_rgba: None,
            theme_rgba: None,
        };
        let mut frames = Vec::new();
        prt_protocol::envelope::append_frame(
            &mut frames,
            prt_protocol::frame::RSP_PROBE,
            1,
            &body.encode(),
        );
        prt_protocol::envelope::wrap_t2c_envelope(&frames)
    }

    /// The reported attach corruption: the renderer's probe answer
    /// missed the probe phase's timeout by one slow round trip and
    /// arrived here, where everything that wasn't VSS used to be called
    /// typeahead and written to the session's pty. zsh reads `ESC _` as
    /// insert-last-word, so the session's prompt grew the previous
    /// command's last word and then the envelope's bytes.
    #[test]
    fn a_late_probe_answer_is_read_here_not_typed_at_the_session() {
        let bytes = [
            &prt_probe_response()[..],
            b"ls -l\r",
            &vss_protocol::encode_accepted(42),
        ]
        .concat();
        let ((ack, typeahead), probe) = ack_and_probe(&bytes, 500);
        assert_eq!(ack, SnapshotAck::Accepted);
        assert_eq!(typeahead, b"ls -l\r", "an envelope reached the session");
        assert!(
            probe.data().1.is_some(),
            "the late answer was dropped instead of read"
        );
    }

    #[test]
    fn an_accept_for_our_sequence_id_is_read() {
        let (ack, typeahead) = ack_of(&vss_protocol::encode_accepted(42), 500);
        assert_eq!(ack, SnapshotAck::Accepted);
        assert!(typeahead.is_empty());
    }

    #[test]
    fn a_reject_carries_its_reason() {
        let (ack, _) = ack_of(&vss_protocol::encode_rejected(42, 1), 500);
        assert_eq!(ack, SnapshotAck::Rejected(1));
    }

    /// A renderer that doesn't speak VSS answers nothing, and the
    /// attach goes ahead anyway — but the keystrokes the user typed
    /// while waiting must not be eaten.
    #[test]
    fn silence_keeps_the_user_s_typeahead() {
        let (ack, typeahead) = ack_of(b"ls -l\r", 60);
        assert_eq!(ack, SnapshotAck::Silent);
        assert_eq!(typeahead, b"ls -l\r");
    }

    /// An answer about a different attach is not ours to act on.
    #[test]
    fn an_ack_for_another_snapshot_is_ignored() {
        let (ack, typeahead) = ack_of(&vss_protocol::encode_accepted(7), 60);
        assert_eq!(ack, SnapshotAck::Silent);
        assert!(typeahead.is_empty(), "envelope leaked into the inner PTY");
    }

    #[test]
    fn plain_text_passes_through() {
        let (out, detached) = feed_chunks(&[b"hello world\n"]);
        assert_eq!(out, b"hello world\n");
        assert!(!detached);
    }

    #[test]
    fn single_prefix_then_normal_byte_passes_both_through() {
        let (out, detached) = feed_chunks(&[&[DETACH_PREFIX, b'x']]);
        assert_eq!(out, &[DETACH_PREFIX, b'x']);
        assert!(!detached);
    }

    #[test]
    fn detach_in_one_chunk() {
        let (out, detached) = feed_chunks(&[b"abc", &[DETACH_PREFIX, DETACH_SECOND], b"ignored"]);
        // Bytes after the trigger are discarded along with the trigger.
        assert_eq!(out, b"abc");
        assert!(detached);
    }

    #[test]
    fn detach_split_across_chunks() {
        // Prefix arrives in chunk N, `d` in chunk N+1 — still detaches.
        let (out, detached) = feed_chunks(&[&[DETACH_PREFIX], &[DETACH_SECOND]]);
        assert_eq!(out, b"");
        assert!(detached);
    }

    #[test]
    fn prefix_then_prefix_then_letter() {
        // First prefix has no follow-up other than another prefix —
        // the first prefix is released, the second stays pending until
        // resolved by `q`, which is not detach.
        let (out, detached) = feed_chunks(&[&[DETACH_PREFIX, DETACH_PREFIX, b'q']]);
        assert_eq!(out, &[DETACH_PREFIX, DETACH_PREFIX, b'q']);
        assert!(!detached);
    }

    #[test]
    fn prefix_then_prefix_then_detach_letter() {
        // Two prefixes in a row, then `d`: first prefix releases as a
        // normal byte, second forms the detach sequence with `d`.
        let (out, detached) =
            feed_chunks(&[&[DETACH_PREFIX, DETACH_PREFIX, DETACH_SECOND]]);
        assert_eq!(out, &[DETACH_PREFIX]);
        assert!(detached);
    }

    #[test]
    fn dangling_prefix_flushes_on_eof() {
        // No follow-up byte — EOF releases the buffered prefix.
        let (out, detached) = feed_chunks(&[&[DETACH_PREFIX]]);
        assert_eq!(out, &[DETACH_PREFIX]);
        assert!(!detached);
    }

    /// What a `vrecv` inside the session is sent, as it reaches this
    /// splice from a renderer with a `vmux` pane in between: a VFT
    /// `DownloadChunk` wrapped in the PRT `RawReply` addressed to the
    /// session client's pane. `data` is the file's bytes.
    fn download_through_a_pane(data: &[u8]) -> Vec<u8> {
        let mut vft = Vec::new();
        vft_protocol::envelope::append_frame(
            &mut vft,
            vft_protocol::frame::EVT_DOWNLOAD_CHUNK,
            0,
            &vft_protocol::envelope::download_chunk_body("vrecv-1", 0, data),
        );
        let reply = vft_protocol::envelope::wrap_h2c_envelope(&vft);
        let mut prt = Vec::new();
        prt_protocol::envelope::append_frame(
            &mut prt,
            prt_protocol::frame::EVT_RAW_REPLY,
            0,
            &prt_protocol::envelope::raw_reply_body("pane-1", &reply),
        );
        prt_protocol::envelope::wrap_t2c_envelope(&prt)
    }

    /// A file that happens to contain `Ctrl+\` `d` used to end the
    /// attach mid-download, dropping the rest of the transfer onto the
    /// renderer's shell as keystrokes. The ESCs around the pair put it
    /// behind every kind of stuffed escape a body can hold, nested
    /// terminator included.
    #[test]
    fn a_detach_pair_inside_an_envelope_is_payload() {
        let data = [
            b'x', 0x1B, b'\\', DETACH_PREFIX, DETACH_SECOND, 0x1B, b'_', b'\t', DETACH_PREFIX,
        ];
        let env = download_through_a_pane(&data);

        assert_eq!(feed_chunks(&[&env]), (env.clone(), false));
        // Wherever the reads split it, and byte by byte.
        for at in 0..=env.len() {
            let (head, tail) = env.split_at(at);
            assert_eq!(feed_chunks(&[head, tail]), (env.clone(), false), "split at {at}");
        }
        let bytes: Vec<&[u8]> = env.chunks(1).collect();
        assert_eq!(feed_chunks(&bytes), (env, false));
    }

    #[test]
    fn the_hotkey_still_works_between_envelopes() {
        let env = download_through_a_pane(&[DETACH_PREFIX, DETACH_SECOND]);
        let (out, detached) = feed_chunks(&[&env, b"ls", &[DETACH_PREFIX, DETACH_SECOND]]);
        assert_eq!(out, [env.as_slice(), b"ls"].concat());
        assert!(detached);
    }

    /// `Esc _` is a vim motion. Typed, it opens nothing, so it must not
    /// leave the hotkey waiting for an `ESC \` that is never coming —
    /// including when what follows starts out like a marker.
    #[test]
    fn a_typed_esc_underscore_leaves_the_hotkey_armed() {
        for typed in [&b"\x1b_"[..], b"\x1b_ciw", b"\x1b_pr", b"\x1b\x1b_v"] {
            let (out, detached) = feed_chunks(&[typed, &[DETACH_PREFIX, DETACH_SECOND]]);
            assert_eq!(out, typed, "{typed:?}");
            assert!(detached, "{typed:?}");
        }
    }

    /// The trigger ends the attach, not the chunk it arrived in: an
    /// envelope behind it is still the session's, and a keystroke
    /// behind it is not.
    #[test]
    fn what_follows_the_trigger_in_its_chunk_is_drained() {
        let env = download_through_a_pane(b"data");
        let chunk = [&b"ab"[..], &[DETACH_PREFIX, DETACH_SECOND], &env, b"cd"].concat();
        let mut s = DetachScanner::default();
        let out = s.feed(&chunk);
        assert!(out.detach);
        assert_eq!(out.forward, [&b"ab"[..], &env].concat());
    }

    /// A SES `Detach` or the session exiting can end the splice between
    /// two reads of one envelope. The drain has to continue with the
    /// splice's parser: a fresh one would take the rest of the envelope
    /// for keystrokes and drop it, leaving the session's client with
    /// half an envelope it waits on forever.
    #[test]
    fn a_drain_picks_up_an_envelope_the_splice_was_part_way_through() {
        let env = download_through_a_pane(&[DETACH_PREFIX, DETACH_SECOND, 0x1B]);
        let (head, tail) = env.split_at(env.len() / 2);
        let mut input = InputState::new();
        let head_out = input.scanner.feed(&input.vss_filter.feed(head).passthrough);
        assert!(!head_out.detach);

        input.scanner.start_draining();
        let chunk = [tail, b"typed", &vss_protocol::encode_detach_accepted(9)].concat();
        let (forward, answered) = input.drain_chunk(&chunk, 9);
        assert!(answered);
        assert_eq!([head_out.forward, forward].concat(), env);
    }

    /// Stdin as a pipe holding `bytes`, closed behind them when
    /// `close` is set; the inner PTY master as a pipe to read back.
    fn drain_over_pipes(bytes: &[u8], close: bool, sequence_id: u32) -> (bool, Vec<u8>) {
        use std::io::Read;
        let (stdin_read, stdin_write) = nix::unistd::pipe().expect("stdin pipe");
        nix::unistd::write(&stdin_write, bytes).expect("write stdin");
        let _held = (!close).then_some(stdin_write);
        let (master_read, master_write) = nix::unistd::pipe().expect("master pipe");
        let (_cli, daemon) = UnixStream::pair().expect("ipc socketpair");

        let mut input = InputState::new();
        let answered = drain_until_detached(
            &stdin_read,
            &master_write,
            daemon.as_fd(),
            &Mutex::new(()),
            &mut input,
            sequence_id,
        );
        drop(master_write);
        let mut forwarded = Vec::new();
        std::fs::File::from(master_read)
            .read_to_end(&mut forwarded)
            .expect("read master");
        (answered, forwarded)
    }

    /// Everything ahead of this attach's answer is read: the session's
    /// envelopes reach it, the keystrokes don't, and an answer meant
    /// for an earlier attach doesn't end the wait.
    #[test]
    fn the_drain_forwards_the_session_s_envelopes_up_to_its_answer() {
        let env = download_through_a_pane(b"chunk");
        let bytes = [
            &b"ls\r"[..],
            &env,
            &vss_protocol::encode_detach_accepted(8),
            b"x",
            &env,
            &vss_protocol::encode_detach_accepted(9),
        ]
        .concat();
        let (answered, forwarded) = drain_over_pipes(&bytes, false, 9);
        assert!(answered);
        assert_eq!(forwarded, [env.as_slice(), &env].concat());
    }

    #[test]
    fn a_drain_stops_when_the_renderer_goes_away() {
        let env = download_through_a_pane(b"chunk");
        let (answered, forwarded) = drain_over_pipes(&env, true, 9);
        assert!(!answered);
        assert_eq!(forwarded, env);
    }

    #[test]
    fn detach_consumes_letter_after_real_prefix_byte() {
        // The bytes preceding the trigger are forwarded; the trigger
        // itself is fully consumed.
        let (out, detached) = feed_chunks(&[b"vim", &[DETACH_PREFIX], b"d after"]);
        assert_eq!(out, b"vim");
        assert!(detached);
    }

    /// Reproduces the "session already attached" re-attach failure: when
    /// the renderer/tab dies but the multiplexer that owns the pane still
    /// holds the renderer's pty master open, the renderer stdin fd never
    /// HUPs, so the pre-fix splice loop (which polled only stdin + the
    /// shutdown pipe) blocked forever and the `attached` flag stuck at
    /// `true`. The always-reliable death signal is the `vsd attach` CLI
    /// closing its IPC socket, which the splice loop now also polls.
    #[test]
    fn renderer_death_via_ipc_close_clears_attached_flag() {
        use crate::engines::EngineState;
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::sync::atomic::AtomicBool;
        use std::time::Instant;

        // IPC socketpair standing in for the CLI ↔ daemon connection.
        let (cli_side, mut daemon_side) = UnixStream::pair().expect("ipc socketpair");

        // Renderer stdio = a pty slave. We keep the MASTER open (and
        // draining) for the whole test to model a multiplexer that hasn't
        // torn the pane pty down yet — so stdin never delivers EOF/HUP and
        // the *only* disconnect signal is the CLI closing `cli_side`.
        let renderer = nix::pty::openpty(None, None).expect("renderer pty");
        // A second dup of the master feeds a drain thread so the ~KB
        // attach snapshot write doesn't block on a full pty buffer; the
        // original master fd stays in this thread so the slave keeps a
        // live master (no HUP) until the test ends.
        let master_dup = dup_owned(&renderer.master).expect("dup renderer master");
        let drain = std::thread::spawn(move || {
            let mut f = std::fs::File::from(master_dup);
            let mut buf = [0u8; 4096];
            // Reads until the slave side is fully closed (all dups gone).
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });

        // Hand the slave over as both stdin and stdout, exactly like the
        // real CLI's `send_stdio(stdin, stdout)`.
        let slave_raw = renderer.slave.as_raw_fd();
        crate::fdpass::send_stdio(&cli_side, slave_raw, slave_raw)
            .expect("send renderer stdio");

        // The session's inner PTY master the handler splices input into.
        let inner = nix::pty::openpty(None, None).expect("inner pty");
        let master_writer = inner.master;

        let engines = Arc::new(Mutex::new(EngineState::new("repro".into())));
        let attached = Arc::new(AtomicBool::new(false));

        start(
            &mut daemon_side,
            Arc::clone(&engines),
            master_writer,
            Arc::clone(&attached),
            "repro",
        )
        .expect("attach start");

        // `start` flips the flag synchronously before spawning the handler.
        assert!(attached.load(Ordering::Acquire), "flag set on attach");

        // Drop our copy of the slave so only the handler's dups reference
        // it; the master stays open via this thread + the drain thread.
        drop(renderer.slave);

        // Simulate the tab/renderer dying: the `vsd attach` CLI process
        // exits, closing its end of the IPC socket. stdin stays HUP-free.
        drop(cli_side);

        // The splice loop must notice the IPC close and tear the attach
        // down, clearing the flag. Allow generous slack for the 500 ms
        // probe timeout the handler runs first.
        let deadline = Instant::now() + Duration::from_secs(3);
        while attached.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !attached.load(Ordering::Acquire),
            "attached flag must clear after the renderer's IPC socket closes"
        );

        // Let the drain thread finish: closing both master fds HUPs the
        // slave (whatever dups the handler still holds also drop as the
        // handler thread has returned), so its read loop ends.
        drop(renderer.master);
        drop(inner.slave);
        let _ = drain.join();
    }
}
