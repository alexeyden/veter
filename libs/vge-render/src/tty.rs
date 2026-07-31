//! Raw-tty guard plus nonblocking poll/read helpers and terminal-size
//! queries. Lifted from vcat (which in turn matched vge-cli's copy) so
//! every client shares one implementation.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};

/// Block until stdin has data ready or `deadline` elapses. Returns
/// `true` if stdin is readable, `false` on timeout.
///
/// A signal arriving mid-poll (SIGWINCH on every terminal resize, since
/// these clients install a handler for it) makes `poll` fail with
/// `EINTR`. That is normal, not an error: the call is retried against
/// the remaining time rather than propagated, which would take the
/// whole app down on a window resize.
pub fn poll_stdin_until(deadline: Instant) -> Result<bool> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::BorrowedFd;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as u16;
        let fd = std::io::stdin().as_raw_fd();
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(remaining_ms)) {
            Ok(n) => return Ok(n > 0),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e).context("poll(stdin)"),
        }
    }
}

/// Block until stdin or `extra` has data ready (or hangs up), or until
/// `deadline` elapses. Returns `(stdin_ready, extra_ready)`. Lets an event
/// loop stay responsive to input while also waiting on a background pipe
/// (e.g. an in-flight ffmpeg decode).
pub fn poll_stdin_and(
    extra: std::os::fd::RawFd,
    deadline: Instant,
) -> Result<(bool, bool)> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::BorrowedFd;
    let now = Instant::now();
    if now >= deadline {
        return Ok((false, false));
    }
    let wake = PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR;
    // Retried on EINTR for the same reason as `poll_stdin_until`.
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok((false, false));
        }
        let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as u16;
        let stdin_fd = std::io::stdin().as_raw_fd();
        let in_b = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let ex_b = unsafe { BorrowedFd::borrow_raw(extra) };
        let mut fds = [
            PollFd::new(in_b, PollFlags::POLLIN),
            PollFd::new(ex_b, PollFlags::POLLIN),
        ];
        match poll(&mut fds, PollTimeout::from(remaining_ms)) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e).context("poll(stdin+fd)"),
        }
        let ready = |i: usize| {
            fds[i]
                .revents()
                .map(|r| r.intersects(wake))
                .unwrap_or(false)
        };
        return Ok((ready(0), ready(1)));
    }
}

/// Single read off stdin; returns the byte count (0 on EOF).
///
/// Retries on `EINTR`: a signal can land between the poll that reported
/// readiness and this read.
pub fn read_stdin(buf: &mut [u8]) -> Result<usize> {
    let fd = std::io::stdin().as_raw_fd();
    loop {
        match nix::unistd::read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(e).context("read(stdin)"),
        }
    }
}

/// Pull anything currently sitting on stdin without blocking. Used at
/// startup as a recovery measure if a previous run left bytes
/// unconsumed (e.g. a half-received envelope).
pub fn drain_stale_stdin() {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::BorrowedFd;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::ZERO) {
            Ok(n) if n > 0 => {
                if read_stdin(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
            _ => break,
        }
    }
}

/// The full `winsize` from TIOCGWINSZ on stdout, if available.
pub fn winsize() -> Option<libc::winsize> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc != 0 { None } else { Some(ws) }
}

/// Number of columns reported by TIOCGWINSZ on stdout, if any.
pub fn winsize_cols() -> Option<u16> {
    winsize().map(|ws| ws.ws_col).filter(|c| *c != 0)
}

/// Number of rows reported by TIOCGWINSZ on stdout, if any.
pub fn winsize_rows() -> Option<u16> {
    winsize().map(|ws| ws.ws_row).filter(|r| *r != 0)
}

// --- SIGWINCH ---

static SIGWINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigwinch(_: libc::c_int) {
    SIGWINCH.store(true, Ordering::SeqCst);
}

/// Install a SIGWINCH handler that sets a process-global flag, and
/// return a reference to that flag. Poll it with [`take_sigwinch`] in
/// the event loop. Idempotent — calling twice just re-installs the same
/// handler.
pub fn install_sigwinch() -> &'static AtomicBool {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(handle_sigwinch),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        let _ = sigaction(Signal::SIGWINCH, &action);
    }
    &SIGWINCH
}

/// Atomically read-and-clear the SIGWINCH flag. Returns `true` if a
/// resize was pending since the last call.
pub fn take_sigwinch(flag: &AtomicBool) -> bool {
    flag.swap(false, Ordering::SeqCst)
}

/// RAII guard that flips the controlling tty into raw mode on
/// construction and restores the previous attributes on drop. The
/// original termios snapshot is captured via `tcgetattr(stdin)`.
pub struct RawTty {
    fd: std::os::fd::RawFd,
    saved: Option<nix::sys::termios::Termios>,
}

impl RawTty {
    pub fn enable() -> Result<Self> {
        use nix::sys::termios::{
            InputFlags, LocalFlags, OutputFlags, SetArg, tcgetattr, tcsetattr,
        };
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        let saved = tcgetattr(&stdin).context("tcgetattr")?;
        let mut raw = saved.clone();
        raw.local_flags &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ISIG);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.input_flags &= !(InputFlags::IXON
            | InputFlags::IXOFF
            | InputFlags::INLCR
            | InputFlags::ICRNL
            | InputFlags::IGNCR);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr (raw)")?;
        Ok(Self {
            fd,
            saved: Some(saved),
        })
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        if let Some(saved) = self.saved.take() {
            use nix::sys::termios::{SetArg, tcsetattr};
            let _ = unsafe {
                let borrowed = std::os::fd::BorrowedFd::borrow_raw(self.fd);
                tcsetattr(borrowed, SetArg::TCSANOW, &saved)
            };
        }
    }
}
