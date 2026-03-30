use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{execvp, Pid};

pub struct Pty {
    master: OwnedFd,
    child: Pid,
}

impl Pty {
    pub fn new(rows: u16, cols: u16) -> io::Result<Self> {
        let winsize = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let result = unsafe { forkpty(Some(&winsize), None) }
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        match result {
            ForkptyResult::Child => {
                unsafe { std::env::set_var("TERM", "xterm-256color") };
                let shell = CString::new("bash").unwrap();
                let err = execvp(&shell, &[&shell]).unwrap_err();
                panic!("exec failed: {err}");
            }
            ForkptyResult::Parent { child, master } => Ok(Pty { master, child }),
        }
    }

    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let n = nix::unistd::write(&self.master, &data[written..])
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            written += n;
        }
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
        }
    }

    pub fn dup_master(&self) -> io::Result<OwnedFd> {
        let fd = nix::unistd::dup(self.master.as_raw_fd())
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = kill(self.child, Signal::SIGHUP);
    }
}
