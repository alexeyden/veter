//! Session process: owns one inner PTY + its host engines, listens
//! on its own per-session Unix socket, accepts `Attach` / `Kill` /
//! `Status` requests until the inner PTY child exits or `Kill` is
//! received.
//!
//! Invoked by `vsd new` re-execing itself with the hidden
//! `--session NAME [argv...]` flag (or `--foreground-session …` in
//! debug). The CLI front-end (`main.rs`) handles the user-facing
//! subcommands; this module is the per-session backend.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, Pid};
use std::ffi::CString;

use crate::attach;
use crate::engines::{spawn_worker, EngineState};
use crate::ipc::{Request, Response, SessionInfo};
use crate::runtime;

/// Default winsize before the renderer attaches and reports its
/// actual cells. Matches the v1 daemon's `EngineState` defaults.
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// Per-attach SIGWINCH watcher cadence and probe / accept timeouts
/// live in `attach.rs`. Here we only need the child-poll cadence.
const CHILD_POLL_INTERVAL_MS: u16 = 250;

/// Run the session process. Blocks until the inner PTY child exits,
/// a `Kill` IPC arrives, or SIGTERM is delivered. Returns `Ok(())` on
/// any clean shutdown; `Err` only on a setup-time failure where the
/// session never came up (in which case the CLI's wait-for-socket
/// poll times out and reports the error from the log file).
pub fn run(name: String, argv: Vec<String>) -> Result<()> {
    runtime::validate_name(&name)?;
    runtime::ensure_runtime_dir()?;

    let sock_path = runtime::socket_path(&name);
    // Refuse to start if another session by this name is alive; if
    // the socket file is a leftover from a crash, probe_socket
    // unlinked it so the bind below succeeds.
    match runtime::probe_socket(&sock_path) {
        runtime::SocketProbe::Alive => {
            bail!("session `{name}` already exists at {}", sock_path.display());
        }
        runtime::SocketProbe::Missing | runtime::SocketProbe::Stale => {}
    }

    let argv = if argv.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        vec![shell]
    } else {
        argv
    };

    // Fork the inner PTY child before binding the socket so that if
    // the exec fails (typo in argv) we exit early without leaving a
    // dangling socket file.
    let (master_fd, child_pid) = spawn_inner_pty(&argv)
        .with_context(|| format!("spawning inner program for session `{name}`"))?;

    let engines = match spawn_worker(&master_fd, name.clone()) {
        Ok(e) => e,
        Err(e) => {
            // Reap the child we just spawned so it doesn't become a
            // zombie.
            let _ = kill(child_pid, Signal::SIGKILL);
            let _ = waitpid(child_pid, None);
            return Err(e).context("spawning per-session worker thread");
        }
    };

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            let _ = kill(child_pid, Signal::SIGKILL);
            let _ = waitpid(child_pid, None);
            return Err(e).context(format!("binding {}", sock_path.display()));
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&sock_path)
        .with_context(|| format!("stat {}", sock_path.display()))?
        .permissions();
    perms.set_mode(0o600);
    let _ = std::fs::set_permissions(&sock_path, perms);

    let session = SessionState {
        name,
        master: master_fd,
        child: child_pid,
        created_at: Instant::now(),
        attached: Arc::new(AtomicBool::new(false)),
        engines,
    };

    // SocketGuard unlinks the socket on Drop so any shutdown path —
    // explicit Kill, child exit, panic — leaves the runtime dir
    // clean.
    let _socket_guard = SocketGuard::new(sock_path.clone());

    let result = accept_loop(&listener, &session);

    // If the inner child exited while a renderer was attached, the
    // handler thread is mid-cleanup right now: restoring the tty
    // termios via RawTty's Drop, writing ATTACH_EXIT, draining the
    // shutdown pipe. Exit before it finishes and the OS kills the
    // thread mid-cleanup — the user's tty stays in raw mode and the
    // CLI's `read` on the IPC socket sees the abrupt close without
    // a clean ATTACH_EXIT before it. Wait for the `attached` flag
    // to flip back to `false` (the last act of the handler thread's
    // closure, after Drop has run on all its locals) with a generous
    // timeout in case something jams.
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.attached.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    // Best-effort teardown: stop the inner PTY child and reap it.
    session.shutdown();

    result
}

/// State kept by the session process for the lifetime of its socket.
struct SessionState {
    name: String,
    /// Master side of the inner PTY. The worker thread holds its own
    /// `dup(2)` for reads; this fd stays alive so closing it tears
    /// down the slave on session shutdown.
    master: OwnedFd,
    child: Pid,
    created_at: Instant,
    /// True iff a renderer is currently attached. Flipped atomically
    /// by the attach handler thread.
    attached: Arc<AtomicBool>,
    engines: Arc<Mutex<EngineState>>,
}

impl SessionState {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            name: self.name.clone(),
            age_secs: self.created_at.elapsed().as_secs(),
            alive: self.child_alive(),
            attached: self.attached.load(Ordering::Acquire),
        }
    }

    fn child_alive(&self) -> bool {
        match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            Ok(_) => false,
            Err(_) => false,
        }
    }

    fn shutdown(&self) {
        let _ = kill(self.child, Signal::SIGTERM);
        let _ = waitpid(self.child, Some(WaitPidFlag::WNOHANG));
    }
}

/// Wrap a socket path so it's unlinked on Drop regardless of the
/// shutdown route (clean exit, error, panic).
struct SocketGuard {
    path: PathBuf,
}

impl SocketGuard {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Main accept loop. Sits in `poll(2)` on the listening socket and
/// returns when either:
///   - the inner PTY child has exited (the worker thread's read
///     loop hit EOF and we observe it via `child_alive`), or
///   - a `Kill` request set the shutdown flag.
fn accept_loop(listener: &UnixListener, session: &SessionState) -> Result<()> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    listener
        .set_nonblocking(true)
        .context("setting listener non-blocking")?;

    let shutdown = Arc::new(AtomicBool::new(false));

    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        if !session.child_alive() {
            return Ok(());
        }

        let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(CHILD_POLL_INTERVAL_MS)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("accept poll: {e}")),
        }

        let revents = fds[0].revents().unwrap_or(PollFlags::empty());
        if !revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            continue;
        }

        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                eprintln!("vsd: accept error on `{}`: {e}", session.name);
                continue;
            }
        };

        // Each request is one-shot — read, dispatch, reply, close.
        // `Attach` is the exception: dispatch keeps the stream alive
        // for the duration of the attach; we move it into the
        // handler thread.
        handle_connection(stream, session, &shutdown);
    }
}

fn handle_connection(
    mut stream: UnixStream,
    session: &SessionState,
    shutdown: &Arc<AtomicBool>,
) {
    let req = match Request::read_from(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            // UnexpectedEof = peer probed liveness by connect-and-close.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return;
            }
            eprintln!("vsd: bad request on `{}`: {e}", session.name);
            let _ = Response::Err(format!("bad request: {e}")).write_to(&mut stream);
            return;
        }
    };
    match req {
        Request::Attach => {
            let master = match dup_owned(&session.master) {
                Ok(fd) => fd,
                Err(e) => {
                    let _ = Response::Err(format!("dup master: {e}"))
                        .write_to(&mut stream);
                    return;
                }
            };
            match attach::start(
                &mut stream,
                Arc::clone(&session.engines),
                master,
                Arc::clone(&session.attached),
                &session.name,
            ) {
                Ok(()) => {
                    let _ = Response::Ok.write_to(&mut stream);
                }
                Err(e) => {
                    let _ = Response::Err(format!("{e:#}")).write_to(&mut stream);
                }
            }
        }
        Request::Kill => {
            shutdown.store(true, Ordering::Release);
            let _ = Response::Ok.write_to(&mut stream);
        }
        Request::Status => {
            let _ = Response::Status(session.info()).write_to(&mut stream);
        }
    }
}

/// `forkpty` + `execvp` the user's program on the slave side of a
/// fresh pseudo-terminal. Returns the parent-side master fd and the
/// child PID.
fn spawn_inner_pty(argv: &[String]) -> Result<(OwnedFd, Pid)> {
    if argv.is_empty() {
        bail!("empty argv");
    }
    let winsize = Winsize {
        ws_row: DEFAULT_ROWS,
        ws_col: DEFAULT_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the child must call only async-signal-safe code between
    // fork and exec; we exec immediately and bail if it returns.
    let fork = unsafe { forkpty(Some(&winsize), None) }
        .with_context(|| "forkpty failed")?;
    match fork {
        ForkptyResult::Parent { child, master } => Ok((master, child)),
        ForkptyResult::Child => {
            // Build argv as C strings — alloc is non-signal-safe but
            // we're single-threaded past the fork, which Linux
            // tolerates.
            let cmd = CString::new(argv[0].as_str())
                .expect("argv[0] contained NUL");
            let cargs: Vec<CString> = argv
                .iter()
                .map(|s| CString::new(s.as_str()).expect("argv contained NUL"))
                .collect();
            let err = execvp(&cmd, &cargs).err();
            eprintln!("vsd: execvp({:?}) failed: {:?}", argv, err);
            std::process::exit(127);
        }
    }
}

/// `dup(2)` an OwnedFd and wrap the result back into an OwnedFd. We
/// reuse this pattern in a few spots; centralising avoids the
/// `from_raw_fd` `unsafe` blocks proliferating.
fn dup_owned(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    let raw = nix::unistd::dup(fd.as_raw_fd()).map_err(std::io::Error::other)?;
    // SAFETY: dup(2) returned a fresh fd we now solely own.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}
