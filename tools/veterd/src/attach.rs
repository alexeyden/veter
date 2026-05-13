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

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::engines::EngineState;
use crate::fdpass;
use crate::probe;
use crate::session::Session;

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

/// Synchronous part of the attach dispatch. Runs on the accept-loop
/// thread; receives the renderer's stdio fds, validates the session,
/// and spawns the per-attach handler thread. Returns `Ok(())` if the
/// handler is now running (in which case the dispatch caller replies
/// `Response::Ok`). Errors here mean the attach was refused.
pub fn start(
    stream: &mut UnixStream,
    sessions: &mut HashMap<String, Session>,
    name: &str,
) -> Result<()> {
    let (stdin, stdout) =
        fdpass::recv_stdio(stream).with_context(|| "receiving renderer stdio fds")?;

    let session = sessions
        .get(name)
        .ok_or_else(|| anyhow!("no such session: {name}"))?;
    if session.attached.swap(true, Ordering::AcqRel) {
        // Race-free check: if it was already true, refuse. The fds we
        // just received go out of scope (dropped → closed).
        return Err(anyhow!("session `{name}` is already attached"));
    }
    // From here on, any error path must reset `attached` to false
    // before returning so the session isn't stuck looking attached.
    let attached_flag = Arc::clone(&session.attached);
    let engines = Arc::clone(&session.engines);
    let master_writer = match dup_owned(&session.master) {
        Ok(fd) => fd,
        Err(e) => {
            attached_flag.store(false, Ordering::Release);
            return Err(e).context("duping inner-PTY master for attach writer");
        }
    };

    let session_name = name.to_string();
    let flag_for_thread = Arc::clone(&attached_flag);
    let spawn = std::thread::Builder::new()
        .name("veterd-attach".into())
        .spawn(move || {
            if let Err(e) = handler_main(stdin, stdout, master_writer, engines) {
                eprintln!("veterd: attach `{session_name}` ended: {e:#}");
            }
            flag_for_thread.store(false, Ordering::Release);
        });
    if let Err(e) = spawn {
        attached_flag.store(false, Ordering::Release);
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
    // Step 3: while still under the lock, install the stdout fd on
    // engines so the worker starts forwarding live bytes the moment
    // we release.
    {
        let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        let mut snapshot: Vec<u8> = Vec::new();
        snapshot.extend_from_slice(&guard.parser.screen().full_contents_formatted());
        snapshot.extend_from_slice(&guard.vge.serialize_state());
        snapshot.extend_from_slice(&guard.prt.serialize_state());

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

    // Step 5: splice renderer stdin → inner PTY master until EOF /
    // error. The worker thread handles the other direction (PTY
    // master output → renderer stdout) via `renderer_stdout` we just
    // installed. Per the PRT spec, input never crosses the engines —
    // we forward keystrokes verbatim.
    let result = splice_input(stdin_fd, master_writer_fd);

    // Step 6: detach — clear the renderer-stdout fd on engines so the
    // worker stops writing. Dropping `stdout_fd` here also closes our
    // copy of the renderer's stdout, which the local user sees as EOF
    // / disconnect from the daemon. The `_watcher` Drop joins its
    // thread before we leave the function.
    {
        let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        guard.renderer_stdout = None;
    }
    result
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
                eprintln!("veterd: winsize watcher: dup(stdin) failed: {e}; resize disabled for this attach");
                return Self { stop, handle: None };
            }
        };
        let master_dup = match dup_owned(master_fd) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("veterd: winsize watcher: dup(master) failed: {e}; resize disabled for this attach");
                return Self { stop, handle: None };
            }
        };
        let handle = std::thread::Builder::new()
            .name("veterd-winsize".into())
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

/// Detach hotkey prefix byte. Per `doc/session-manager.md` §6 veterd
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
/// program gone), or when the [`DetachScanner`] fires.
fn splice_input(stdin_fd: OwnedFd, master_writer_fd: OwnedFd) -> Result<()> {
    let mut reader = std::fs::File::from(stdin_fd);
    let writer_raw = master_writer_fd.as_raw_fd();
    let mut buf = [0u8; 4096];
    let mut scanner = DetachScanner::default();
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => {
                if let Some(b) = scanner.flush_on_eof() {
                    let _ = write_all_raw(writer_raw, &[b]);
                }
                return Ok(());
            }
            Ok(n) => n,
            Err(e) => return Err(anyhow!("renderer stdin read: {e}")),
        };

        let out = scanner.feed(&buf[..n]);
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
                eprintln!("veterd: tcgetattr on renderer stdin failed: {e}; \
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
            eprintln!("veterd: tcsetattr raw on renderer stdin failed: {e}; \
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
        if let Some(saved) = self.saved.take() {
            use nix::sys::termios::{tcsetattr, SetArg};
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(self.fd) };
            // Best-effort restore — at this point the attach is ending
            // and we can't do anything useful with the error.
            let _ = tcsetattr(borrowed, SetArg::TCSANOW, &saved);
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
}
