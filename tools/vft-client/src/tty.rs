// Raw-TTY guard + nonblocking poll/read helpers, lifted from vcat
// without modification.

use std::os::fd::AsRawFd;
use std::time::Instant;

use anyhow::{Context, Result};

/// Block until stdin has data ready or `deadline` elapses. Returns
/// `true` if stdin is readable, `false` on timeout.
pub fn poll_stdin_until(deadline: Instant) -> Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;
    let now = Instant::now();
    if now >= deadline {
        return Ok(false);
    }
    let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as u16;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let n = poll(&mut fds, PollTimeout::from(remaining_ms)).context("poll(stdin)")?;
    Ok(n > 0)
}

/// Single read off stdin; returns the byte count (0 on EOF).
pub fn read_stdin(buf: &mut [u8]) -> Result<usize> {
    let fd = std::io::stdin().as_raw_fd();
    let n = nix::unistd::read(fd, buf).context("read(stdin)")?;
    Ok(n)
}

/// Pull anything currently sitting on stdin without blocking. Used at
/// startup as a recovery measure if a previous run left bytes
/// unconsumed (e.g. a half-received envelope).
pub fn drain_stale_stdin() {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
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

/// Number of columns reported by TIOCGWINSZ on stdout, if any.
pub fn winsize_cols() -> Option<u16> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc != 0 || ws.ws_col == 0 {
        None
    } else {
        Some(ws.ws_col)
    }
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
            tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg,
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
            use nix::sys::termios::{tcsetattr, SetArg};
            let _ = unsafe {
                let borrowed = std::os::fd::BorrowedFd::borrow_raw(self.fd);
                tcsetattr(borrowed, SetArg::TCSANOW, &saved)
            };
        }
    }
}
