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

use anyhow::{anyhow, Context, Result};

use crate::engines::EngineState;
use crate::fdpass;
use crate::session::Session;

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
    // Step 1: under the engines lock, compute the snapshot and write
    // it to the renderer. We hold the lock for the duration so the
    // per-session worker can't interleave live bytes between snapshot
    // chunks — otherwise the renderer would see "partial replay +
    // post-replay byte + more replay" and paint inconsistent state.
    //
    // Step 2: while still under the lock, install the stdout fd on
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

    // Step 3: splice renderer stdin → inner PTY master until EOF /
    // error. The worker thread handles the other direction (PTY
    // master output → renderer stdout) via `renderer_stdout` we just
    // installed. Per the PRT spec, input never crosses the engines —
    // we forward keystrokes verbatim.
    let result = splice_input(stdin_fd, master_writer_fd);

    // Step 4: detach — clear the renderer-stdout fd on engines so the
    // worker stops writing. Dropping `stdout_fd` here also closes our
    // copy of the renderer's stdout, which the local user sees as EOF
    // / disconnect from the daemon.
    {
        let mut guard = engines.lock().unwrap_or_else(|e| e.into_inner());
        guard.renderer_stdout = None;
    }
    result
}

/// Renderer-stdin → inner-PTY-master forwarding loop. Returns on EOF
/// of stdin (renderer disconnected cleanly) or on write error
/// (inner program gone).
fn splice_input(stdin_fd: OwnedFd, master_writer_fd: OwnedFd) -> Result<()> {
    let mut reader = std::fs::File::from(stdin_fd);
    let writer_raw = master_writer_fd.as_raw_fd();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) => return Err(anyhow!("renderer stdin read: {e}")),
        };
        write_all_raw(writer_raw, &buf[..n])
            .with_context(|| "writing renderer input to inner PTY")?;
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
