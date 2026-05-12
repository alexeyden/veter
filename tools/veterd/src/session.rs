//! One persistent session inside the daemon.
//!
//! A session owns the inner PTY and the child process running on its
//! slave side (default `$SHELL`). The daemon keeps these alive across
//! renderer attach/detach cycles. Future commits (task #6) extend
//! this struct with the per-session host engines (vt100 / PRT / VGE)
//! that parse the PTY output to keep an authoritative state model
//! for snapshot replay.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, Pid};
use std::ffi::CString;

pub struct Session {
    pub name: String,
    /// Master side of the inner PTY. Used by task #6's attach path
    /// to splice bytes between the daemon and the connected
    /// renderer. The skeleton holds onto it so the slave doesn't
    /// see EOF on its end.
    #[allow(dead_code)]
    pub master: OwnedFd,
    pub child: Pid,
    pub created_at: Instant,
    /// Whether a renderer is currently attached. Reserved for task #6
    /// — the skeleton never flips this from `false`.
    pub attached: bool,
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
            ForkptyResult::Parent { child, master } => Ok(Self {
                name,
                master,
                child,
                created_at: Instant::now(),
                attached: false,
            }),
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

    /// Raw fd accessor for the attach path (task #6).
    #[allow(dead_code)]
    pub fn master_fd(&self) -> RawFd {
        self.master.as_raw_fd()
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
