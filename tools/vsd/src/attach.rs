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
    let outcome = probe::run(&stdin_fd, &stdout_fd, PROBE_TIMEOUT)
        .with_context(|| "running upstream probe")?;
    apply_probe(&engines, &master_writer_fd, &outcome);
    if !outcome.typeahead.is_empty() {
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
    {
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
        let vss_env = vss_protocol::encode_snapshot(
            vss_protocol::SNAPSHOT_VERSION,
            rows,
            cols,
            1, // sequence_id — only one snapshot per attach for v1
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

        guard.renderer_stdout = Some(
            dup_owned(&stdout_fd).with_context(|| "duping renderer stdout for worker")?,
        );
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
    let result =
        splice_input(&stdin_fd, master_writer_fd, shutdown_read, ipc_socket.as_fd());

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
    let detach_env = vss_protocol::encode_detach_notify();
    let _ = write_all_raw(stdout_fd.as_raw_fd(), &detach_env);
    // Belt-and-suspenders: `ESC c` (RIS — full terminal reset).
    let _ = write_all_raw(stdout_fd.as_raw_fd(), b"\x1bc");

    // Explicitly restore tty termios here (instead of relying on
    // `RawTty::Drop` only). The Drop path was leaving the stdin tty
    // in raw mode in practice — `stty -a` post-detach showed
    // `-echo -icanon -opost` — so an explicit, logged restore goes
    // first. Loop on EINTR (SIGCHLD from the inner program exit can
    // hit us mid-call). The RawTty guard's Drop still runs after
    // and is a no-op iff we landed here cleanly.
    restore_tty_canonical(stdin_fd.as_raw_fd());

    result
}

/// Defensive end-of-attach tty restore: read current termios, OR-in
/// the cooked-mode bits (`ICANON | ECHO | ISIG | IEXTEN`, plus
/// `ICRNL` and `OPOST | ONLCR`), and `tcsetattr` it back. Logs to
/// stderr on failure so we have something to diagnose with.
fn restore_tty_canonical(fd: std::os::fd::RawFd) {
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
    let mut attempts = 0u8;
    loop {
        match tcsetattr(borrowed, SetArg::TCSANOW, &t) {
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
        initial: Option<(u16, u16)>,
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
    initial: Option<(u16, u16)>,
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
        let Some((rows, cols)) = probe::read_winsize(raw_stdin) else {
            // ioctl failed (stdin closed?) — back off and let the
            // splice loop notice the disconnect.
            continue;
        };
        if last == Some((rows, cols)) {
            continue;
        }
        last = Some((rows, cols));

        // Apply: resize the parser, then SIGWINCH the inner program
        // via TIOCSWINSZ on the master. The inner program's redraw
        // bytes flow through the worker thread, which forwards them
        // to the (still-attached) renderer.
        {
            let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
            guard.parser.screen_mut().set_size(rows, cols);
        }
        probe::set_inner_winsize(raw_master, rows, cols);
    }
}

/// Apply probe + winsize results to the per-session engines and to
/// the inner PTY master. SIGWINCHing the inner program here means
/// its redraw bytes arrive before the snapshot is serialized, so the
/// snapshot reflects the renderer's actual grid (modulo whatever
/// races the inner program loses to our `engines.lock()`).
fn apply_probe(
    engines: &Arc<Mutex<EngineState>>,
    master_writer_fd: &OwnedFd,
    outcome: &probe::ProbeOutcome,
) {
    let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());

    if let Some((rows, cols)) = outcome.winsize {
        guard.parser.screen_mut().set_size(rows, cols);
        // SIGWINCH the inner program so its next redraw is at the
        // right size. Best effort; the engines have already been
        // resized so a stale dimension on the slave's tty is the only
        // failure mode.
        probe::set_inner_winsize(master_writer_fd.as_raw_fd(), rows, cols);
    }
    if let Some(vge) = outcome.vge {
        let cell_px = (vge.cell_pixel_width, vge.cell_pixel_height);
        guard.vge.set_dimensions(cell_px, vge.scale_factor);
        guard.prt.set_metrics(cell_px, vge.scale_factor);
    }
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
        for &b in chunk {
            if self.pending_prefix {
                if b == DETACH_SECOND {
                    self.pending_prefix = false;
                    return ScanOutput { forward: out, detach: true };
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
        ScanOutput { forward: out, detach: false }
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
/// (`-echo -icanon -opost`) until they ran `reset` by hand. The
/// other two fds are owned because they're attach-private and should
/// close here.
fn splice_input(
    stdin_fd: &OwnedFd,
    master_writer_fd: OwnedFd,
    shutdown_read: OwnedFd,
    ipc_fd: BorrowedFd<'_>,
) -> Result<()> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    let stdin_raw = stdin_fd.as_raw_fd();
    let writer_raw = master_writer_fd.as_raw_fd();
    let mut buf = [0u8; 4096];
    let mut scanner = DetachScanner::default();
    let mut trace_log = open_input_trace();
    // Strip the renderer's upstream VSS envelopes (`ESC _ vss … ESC \`)
    // before forwarding bytes to the inner PTY. The renderer queues
    // SnapshotAccepted / SnapshotRejected frames in response to the
    // attach-time snapshot; those bytes route back through PRT
    // EVT_RAW_REPLY → vmux → SSH and land here on stdin. v1 of vsd
    // doesn't act on them yet (no version-mismatch UI), but they must
    // not reach the inner shell — `ESC _` is meta-paren and the
    // payload chars get inserted as literal keystrokes.
    let mut vss_filter =
        vss_protocol::ApcStream::with_marker(*vss_protocol::MARKER_R2E);
    // Bounded poll timeout so a lone Esc keystroke that lands in
    // `vss_filter`'s `EscPending` state still reaches the inner PTY
    // when no follow-up byte arrives — same idea as the terminfo
    // `ESCDELAY` (default 100 ms) used by curses apps to disambiguate
    // a bare Esc from the start of a multi-byte escape sequence.
    let esc_timeout_ms = 50u16;
    loop {
        let mut fds = [
            PollFd::new(stdin_fd.as_fd(), PollFlags::POLLIN),
            PollFd::new(shutdown_read.as_fd(), PollFlags::POLLIN),
            PollFd::new(ipc_fd, PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::from(esc_timeout_ms)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("splice poll: {e}")),
        }

        // Shutdown beats stdin: if the session is gone we want to
        // tear down even if there are buffered keystrokes.
        let shutdown_revents = fds[1].revents().unwrap_or(PollFlags::empty());
        if shutdown_revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            return Ok(());
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
            return Ok(());
        }

        let stdin_revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if !stdin_revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            // No input this tick. Flush a held lone Esc so vim et al.
            // see the mode-switch keystroke at all.
            let flushed = vss_filter.flush_pending_esc();
            if !flushed.is_empty() {
                let out = scanner.feed(&flushed);
                if !out.forward.is_empty() {
                    write_all_raw(writer_raw, &out.forward)
                        .with_context(|| "writing flushed Esc to inner PTY")?;
                }
                if out.detach {
                    return Ok(());
                }
            }
            continue;
        }

        let n = match nix::unistd::read(stdin_raw, &mut buf) {
            Ok(0) => {
                if let Some(b) = scanner.flush_on_eof() {
                    let _ = write_all_raw(writer_raw, &[b]);
                }
                return Ok(());
            }
            Ok(n) => n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("renderer stdin read: {e}")),
        };

        if let Some(log) = trace_log.as_mut() {
            let _ = log_input_chunk(log, &buf[..n]);
        }

        // Filter out any renderer-side VSS envelopes; what's left is
        // user keystrokes destined for the inner shell.
        let vss_out = vss_filter.feed(&buf[..n]);
        // `vss_out.payloads` would carry SnapshotAccepted / Rejected
        // bodies if v1.1 grows version-mismatch UX; for now we drop
        // them.
        if vss_out.passthrough.is_empty() {
            continue;
        }

        let out = scanner.feed(&vss_out.passthrough);
        if !out.forward.is_empty() {
            write_all_raw(writer_raw, &out.forward)
                .with_context(|| "writing renderer input to inner PTY")?;
        }
        if out.detach {
            return Ok(());
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
