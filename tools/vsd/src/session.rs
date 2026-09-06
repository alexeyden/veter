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

/// Write end of the SIGTERM self-pipe, as a raw fd, for the signal
/// handler to reach. `-1` until [`install_sigterm_handler`] runs.
///
/// A signal handler may call only async-signal-safe functions, so it
/// writes one byte here and does nothing else; the accept loop polls
/// the read end and does the real work. Without a handler at all,
/// SIGTERM's default action kills the process outright — skipping the
/// socket unlink, the child's own teardown, and (if a renderer is
/// attached) the termios restore that leaves the user's terminal
/// usable. `doc/session-manager.md` lists SIGTERM as a clean exit
/// path.
static SIGTERM_PIPE_WRITE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_sigterm(_signum: libc::c_int) {
    let fd = SIGTERM_PIPE_WRITE.load(Ordering::Acquire);
    if fd >= 0 {
        // write(2) is async-signal-safe. A failure here means the
        // pipe is full — one byte is already pending, which is all
        // the accept loop needs.
        unsafe {
            libc::write(fd, [0u8].as_ptr().cast(), 1);
        }
    }
}

/// Install the SIGTERM (and SIGHUP) handler and return the read end of
/// its self-pipe for the accept loop to poll.
fn install_sigterm_handler() -> Result<OwnedFd> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    let (read_fd, write_fd) =
        nix::unistd::pipe().context("creating SIGTERM self-pipe")?;
    SIGTERM_PIPE_WRITE.store(write_fd.as_raw_fd(), Ordering::Release);
    // Leaked on purpose: the handler holds the raw number for the
    // life of the process, and closing it would leave the handler
    // writing to a recycled fd.
    std::mem::forget(write_fd);

    let action = SigAction::new(
        SigHandler::Handler(on_sigterm),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: `on_sigterm` calls only `write(2)` and an atomic load,
    // both async-signal-safe.
    unsafe {
        sigaction(Signal::SIGTERM, &action).context("installing SIGTERM handler")?;
        // A closing terminal sends SIGHUP; the session is meant to
        // outlive its renderer, but if the *daemon's* own terminal
        // goes away it should still exit cleanly rather than be
        // killed mid-attach.
        sigaction(Signal::SIGHUP, &action).context("installing SIGHUP handler")?;
    }
    Ok(read_fd)
}

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
        child_reaped: AtomicBool::new(false),
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
    /// Set once `child_alive` has seen the child exit and reaped it.
    /// After that the pid is free for the kernel to reuse, so
    /// `shutdown`'s `SIGTERM` would land on whatever process got it
    /// next — someone else's, on a busy machine.
    child_reaped: AtomicBool,
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
        if self.child_reaped.load(Ordering::Acquire) {
            return false;
        }
        match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => true,
            // Either it exited (and this call reaped it) or it is not
            // ours to wait for. Either way the pid is no longer a
            // handle on our child.
            Ok(_) | Err(_) => {
                self.child_reaped.store(true, Ordering::Release);
                false
            }
        }
    }

    fn shutdown(&self) {
        if self.child_reaped.load(Ordering::Acquire) {
            return;
        }
        let _ = kill(self.child, Signal::SIGTERM);
        let _ = waitpid(self.child, Some(WaitPidFlag::WNOHANG));
        self.child_reaped.store(true, Ordering::Release);
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
    let sigterm_fd = install_sigterm_handler()?;

    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        if !session.child_alive() {
            return Ok(());
        }

        let mut fds = [
            PollFd::new(listener.as_fd(), PollFlags::POLLIN),
            PollFd::new(sigterm_fd.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::from(CHILD_POLL_INTERVAL_MS)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(anyhow!("accept poll: {e}")),
        }

        // A signal arrived. Same teardown as `Kill`: wake the attach
        // handler so it restores the terminal it put in raw mode,
        // then let `run` do the waiting and the unlinking.
        if fds[1]
            .revents()
            .unwrap_or(PollFlags::empty())
            .intersects(PollFlags::POLLIN)
        {
            let guard = session.engines.lock().unwrap_or_else(|e| e.into_inner());
            guard.signal_attach_shutdown();
            return Ok(());
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
            // Wake an attached renderer's handler. `run` waits below
            // for `attached` to clear, but nothing else pokes the
            // handler — it is blocked in `splice_input` on the
            // renderer's stdin — so the wait would time out and the
            // process would exit with the handler mid-splice: no
            // `DetachNotify`, and no termios restore, leaving the
            // user's terminal `-echo -icanon -opost`.
            {
                let guard = session
                    .engines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.signal_attach_shutdown();
            }
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
            // A session's children must look exactly like a direct
            // veter child to an out-of-band client, so export the same
            // discovery variables the GUI host sets. `vsd` links the
            // same engines, so the caps it advertises are the same ones.
            for (name, value) in
                veter_host::env::host_vars(env!("CARGO_PKG_VERSION"))
            {
                unsafe { std::env::set_var(name, value) };
            }
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
