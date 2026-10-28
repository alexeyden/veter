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
use veter_host::handoff;
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
    let sock_path = claim_name(&name)?;

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

    let engines = match spawn_worker(&master_fd, EngineState::new(name.clone())) {
        Ok(e) => e,
        Err(e) => {
            // Reap the child we just spawned so it doesn't become a
            // zombie.
            let _ = kill(child_pid, Signal::SIGKILL);
            let _ = waitpid(child_pid, None);
            return Err(e).context("spawning per-session worker thread");
        }
    };

    let (listener, socket_guard) = match bind_socket(&sock_path) {
        Ok(bound) => bound,
        Err(e) => {
            let _ = kill(child_pid, Signal::SIGKILL);
            let _ = waitpid(child_pid, None);
            return Err(e);
        }
    };

    serve(
        SessionState {
            name,
            master: master_fd,
            child: Child::Spawned(child_pid),
            created_at: Instant::now(),
            attached: Arc::new(AtomicBool::new(false)),
            child_reaped: AtomicBool::new(false),
            engines,
        },
        listener,
        socket_guard,
    )
}

/// Run a session over a pty some terminal already owns (`vsd adopt`,
/// `doc/session-manager.md` §2.5). Asks the terminal listening at
/// `from` for its master and a snapshot of its engines, and tells it
/// whether to go on showing the session (`reattach`) once it has let go.
///
/// Until the verdict goes back the terminal still holds the pty and has
/// only paused reading it, so every failure up to that point is
/// reported to it — it resumes and nothing is lost. The order below is
/// what keeps that true: everything that can fail happens before the
/// worker reads its first byte.
pub fn run_adopted(name: String, from: PathBuf, reattach: bool) -> Result<()> {
    let sock_path = claim_name(&name)?;

    let stream = UnixStream::connect(&from)
        .with_context(|| format!("connecting to the terminal at {}", from.display()))?;
    // The terminal drains what it has already read before answering,
    // which under a flood is a while, but not this long.
    stream.set_read_timeout(Some(HANDOFF_TIMEOUT))?;
    handoff::Request::Adopt { session: name.clone(), reattach }
        .write_to(&mut &stream)
        .context("asking the terminal for its pty")?;
    let (master, h) = handoff::recv_handoff(&stream)
        .context("reading the terminal's handoff")?
        .map_err(|reason| anyhow!("the terminal refused: {reason}"))?;

    let refuse = |e: anyhow::Error| {
        let _ = handoff::send_verdict(&mut &stream, Err(&format!("{e:#}")));
        e
    };

    let child = adopt_child(Pid::from_raw(h.child_pid as i32)).map_err(refuse)?;
    let mut state = EngineState::new(name.clone());
    // Metrics and size first: a restored portal inherits the metrics of
    // the engine it lands in, and the restore puts the grid back to the
    // receiving size, which has to be the terminal's.
    state.vge.set_dimensions(h.cell_px, h.scale);
    state.prt.set_metrics(h.cell_px, h.scale);
    state.resize(h.rows, h.cols);
    veter_host::pipeline::restore(
        &mut state.parser,
        &mut state.vge,
        &mut state.prt,
        &h.vt,
        &h.vge,
        &h.prt,
    )
    .map_err(|reason| refuse(anyhow!("cannot restore the terminal's snapshot: {reason:?}")))?;
    let (listener, socket_guard) = bind_socket(&sock_path).map_err(refuse)?;
    let engines = spawn_worker(&master, state)
        .context("spawning per-session worker thread")
        .map_err(refuse)?;

    handoff::send_verdict(&mut &stream, Ok(())).context("confirming the handoff")?;
    drop(stream);

    serve(
        SessionState {
            name,
            master,
            child,
            created_at: Instant::now(),
            attached: Arc::new(AtomicBool::new(false)),
            child_reaped: AtomicBool::new(false),
            engines,
        },
        listener,
        socket_guard,
    )
}

/// How long `run_adopted` waits on the terminal at each step.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Validate `name` and make sure no live session holds it. Returns the
/// socket path to bind; a crashed session's leftover is unlinked.
fn claim_name(name: &str) -> Result<PathBuf> {
    runtime::validate_name(name)?;
    runtime::ensure_runtime_dir()?;

    let sock_path = runtime::socket_path(name);
    match runtime::probe_socket(&sock_path) {
        runtime::SocketProbe::Alive => {
            bail!("session `{name}` already exists at {}", sock_path.display());
        }
        runtime::SocketProbe::Missing | runtime::SocketProbe::Stale => {}
    }
    Ok(sock_path)
}

/// Bind the session's socket owner-only. The guard unlinks it on any
/// shutdown path — explicit Kill, child exit, panic.
fn bind_socket(sock_path: &std::path::Path) -> Result<(UnixListener, SocketGuard)> {
    let listener = UnixListener::bind(sock_path)
        .with_context(|| format!("binding {}", sock_path.display()))?;
    let guard = SocketGuard::new(sock_path.to_path_buf());
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o600));
    Ok((listener, guard))
}

/// Serve `session` until its child exits, a `Kill` arrives, or SIGTERM.
fn serve(session: SessionState, listener: UnixListener, _socket_guard: SocketGuard) -> Result<()> {
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
    let deadline = Instant::now() + crate::attach::TEARDOWN_BUDGET;
    while session.attached.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }

    // Best-effort teardown: stop the inner PTY child and reap it.
    session.shutdown();

    result
}

/// The program on the session's pty.
enum Child {
    /// Forked by this process (`vsd new`), so ours to wait for.
    Spawned(Pid),
    /// Inherited with a terminal's pty (`vsd adopt`). It is still the
    /// terminal's child — or init's, once the terminal exits — so
    /// `waitpid` cannot see it; a pidfd can, and unlike the bare pid it
    /// cannot come to name some other process once this one is reaped.
    Adopted(OwnedFd),
}

fn adopt_child(pid: Pid) -> Result<Child> {
    // SAFETY: plain syscall; a non-negative return is a fresh fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.as_raw(), 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("pidfd_open({pid})"));
    }
    Ok(Child::Adopted(unsafe { OwnedFd::from_raw_fd(fd as i32) }))
}

/// State kept by the session process for the lifetime of its socket.
struct SessionState {
    name: String,
    /// Master side of the inner PTY. The worker thread holds its own
    /// `dup(2)` for reads; this fd stays alive so closing it tears
    /// down the slave on session shutdown.
    master: OwnedFd,
    child: Child,
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
        let alive = match &self.child {
            Child::Spawned(pid) => {
                // Either it exited (and this call reaped it) or it is
                // not ours to wait for. Either way the pid is no longer
                // a handle on our child.
                matches!(
                    waitpid(*pid, Some(WaitPidFlag::WNOHANG)),
                    Ok(WaitStatus::StillAlive)
                )
            }
            Child::Adopted(pidfd) => {
                // A pidfd turns readable once the process has exited.
                use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
                use std::os::fd::AsFd;
                let mut fds = [PollFd::new(pidfd.as_fd(), PollFlags::POLLIN)];
                !matches!(poll(&mut fds, PollTimeout::ZERO), Ok(n) if n > 0)
            }
        };
        if !alive {
            self.child_reaped.store(true, Ordering::Release);
        }
        alive
    }

    fn shutdown(&self) {
        if self.child_reaped.load(Ordering::Acquire) {
            return;
        }
        match &self.child {
            Child::Spawned(pid) => {
                let _ = kill(*pid, Signal::SIGTERM);
                let _ = waitpid(*pid, Some(WaitPidFlag::WNOHANG));
            }
            Child::Adopted(pidfd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    pidfd.as_raw_fd(),
                    libc::SIGTERM,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            },
        }
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
        Request::Attach { steal } => {
            if steal && session.attached.load(Ordering::Acquire) {
                take_over_attach(session);
            }
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

/// End the attach in progress so another can take its place
/// (`vsd attach -d`).
///
/// This is the same wake a `Kill` gives the attach handler, and the
/// handler answers it with the ordinary detach: the old renderer gets
/// `DetachNotify` and with it the screen it showed before attaching,
/// and its `vsd attach` exits. Blocks the accept loop until the
/// handler has let go, bounded by the teardown budget; if it has not
/// by then, the attach that follows is refused as usual.
fn take_over_attach(session: &SessionState) {
    {
        let guard = session.engines.lock().unwrap_or_else(|e| e.into_inner());
        guard.signal_attach_shutdown();
    }
    let deadline = Instant::now() + crate::attach::TEARDOWN_BUDGET;
    while session.attached.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
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
