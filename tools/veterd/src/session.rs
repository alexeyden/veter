//! One persistent session inside the daemon.
//!
//! A session owns the inner PTY and the child process running on its
//! slave side (default `$SHELL`). The daemon keeps these alive across
//! renderer attach/detach cycles. Per-session host engines (vt100 /
//! PRT / VGE) parse the PTY output continuously via the worker thread
//! in [`crate::engines`], so the attach path can replay an
//! authoritative state snapshot when a renderer connects.

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, Pid};
use std::ffi::CString;

use crate::engines::{spawn_worker, EngineState};

pub struct Session {
    pub name: String,
    /// Master side of the inner PTY. Used by task #6's attach path
    /// to splice renderer-stdin bytes into the inner program. The
    /// worker thread holds its own dup for reads and engine-response
    /// writes; this fd stays around so closing it can drop the slave.
    #[allow(dead_code)]
    pub master: OwnedFd,
    pub child: Pid,
    pub created_at: Instant,
    /// Whether a renderer is currently attached. The attach-handler
    /// thread flips this to `true` after the snapshot replay, and back
    /// to `false` on detach (either side closes its fd). The accept
    /// loop reads it for `Request::List` and to reject duplicate
    /// attaches.
    pub attached: Arc<AtomicBool>,
    /// Authoritative host engine state. The worker thread holds one
    /// `Arc` of this and mutates it under the lock; the attach handler
    /// thread holds another `Arc` so it can serialize a snapshot and
    /// install / clear the renderer-stdout fd.
    pub engines: Arc<Mutex<EngineState>>,
}

impl Session {
    /// Spawn a new session running `argv` on the slave side of a fresh
    /// pseudo-terminal. `argv[0]` must be either an absolute path or a
    /// PATH-resolvable command. The child inherits the daemon's env.
    pub fn spawn(name: String, argv: &[String]) -> Result<Self> {
        if argv.is_empty() {
            bail!("empty argv");
        }
        // Generous default size; the renderer will resize on attach.
        let winsize = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: forkpty is unsafe because the child must not touch
        // any non-async-signal-safe state until exec. We exec
        // immediately and bail on failure.
        let fork = unsafe { forkpty(Some(&winsize), None) }
            .with_context(|| "forkpty failed")?;
        match fork {
            ForkptyResult::Parent { child, master } => {
                let engines = match spawn_worker(&master) {
                    Ok(e) => e,
                    Err(e) => {
                        // We've already forked; reap the child so it
                        // doesn't leak into the daemon's process table.
                        let _ = kill(child, Signal::SIGKILL);
                        let _ = waitpid(child, None);
                        return Err(e.context("spawning session worker thread"));
                    }
                };
                Ok(Self {
                    name,
                    master,
                    child,
                    created_at: Instant::now(),
                    attached: Arc::new(AtomicBool::new(false)),
                    engines,
                })
            }
            ForkptyResult::Child => {
                // Build the argv as C strings up front (allocations
                // are non-signal-safe but we're past the fork point
                // with a single thread, so this is fine on Linux).
                let cmd = CString::new(argv[0].as_str())
                    .expect("argv[0] contained NUL");
                let cargs: Vec<CString> = argv
                    .iter()
                    .map(|s| CString::new(s.as_str()).expect("argv contained NUL"))
                    .collect();
                let err = execvp(&cmd, &cargs).err();
                // execvp returned: it failed. Print and die loudly so
                // the daemon parent's session.alive flag flips quickly.
                eprintln!("veterd: execvp({:?}) failed: {:?}", argv, err);
                std::process::exit(127);
            }
        }
    }

    /// Convenience: is this session currently attached? Reads through
    /// the shared `Arc<AtomicBool>` that the attach handler maintains.
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::Acquire)
    }

    /// Send SIGTERM and reap. Returns Ok even if the child is already
    /// gone — `Kill` is meant to be idempotent at the IPC layer.
    pub fn shutdown(&mut self) {
        // Best-effort SIGTERM. If the child has already exited, kill
        // returns ESRCH; we don't care.
        let _ = kill(self.child, Signal::SIGTERM);
        // Non-blocking reap so we don't stall the daemon on a child
        // that refuses to die immediately. The remaining wait happens
        // in `is_alive()` on subsequent ticks.
        let _ = waitpid(self.child, Some(WaitPidFlag::WNOHANG));
    }

    /// True iff the child PID is still in the runnable / sleeping
    /// state. Calls `waitpid` non-blocking, returning false once the
    /// child has been reaped.
    pub fn is_alive(&self) -> bool {
        match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(_) => false,
            // Already reaped by a prior call.
            Err(_) => false,
        }
    }

    pub fn age_secs(&self) -> u64 {
        self.created_at.elapsed().as_secs()
    }
}
