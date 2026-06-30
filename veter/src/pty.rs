use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::sync::mpsc::{self, Receiver, Sender};

use nix::pty::{forkpty, ForkptyResult, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{execvp, Pid};

pub struct Pty {
    master: OwnedFd,
    child: Pid,
    /// Output destined for the child is handed to a dedicated writer
    /// thread rather than written inline. A blocking `write(2)` on the
    /// master can stall for as long as the downstream reader is slow to
    /// drain — most acutely a large VFT download relayed through vmux,
    /// where the single-threaded relay empties the master far slower
    /// than a direct client would. Doing that write on the winit
    /// event-loop thread froze the GUI for the whole transfer; the
    /// writer thread absorbs the blocking so `write_all` only ever does
    /// a non-blocking channel push.
    ///
    /// The queue is currently unbounded: step A removes the freeze but
    /// not the memory growth a flooding producer can cause. End-to-end
    /// flow control (the planned VFT ack-windowing, step B) is what
    /// bounds the producer; until then a runaway download grows this
    /// queue instead of stalling the loop.
    writer_tx: Sender<Vec<u8>>,
}

/// Drain queued buffers to the master fd with blocking writes. Runs on
/// its own thread so the caller (the event loop) never blocks. A single
/// consumer of a FIFO channel preserves the byte ordering callers rely
/// on across successive `write_all` calls. Exits when the channel
/// closes (Pty dropped) or the fd reports the child is gone.
fn writer_loop(fd: OwnedFd, rx: Receiver<Vec<u8>>) {
    while let Ok(buf) = rx.recv() {
        let mut data = &buf[..];
        while !data.is_empty() {
            match nix::unistd::write(&fd, data) {
                Ok(0) => return,
                Ok(n) => data = &data[n..],
                Err(nix::errno::Errno::EINTR) => continue,
                // Child exited / fd closed: drop this and any further
                // queued bytes — there's no reader left for them.
                Err(_) => return,
            }
        }
    }
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
            .map_err(io::Error::other)?;

        match result {
            ForkptyResult::Child => {
                unsafe { std::env::set_var("TERM", "xterm-256color") };
                unsafe { std::env::set_var("COLORTERM", "truecolor") };
                // Prefer vmux when available so veter sessions get
                // the multiplexer transparently. `make install`
                // ships vmux alongside veter in the same bindir, so
                // look there first — desktop launchers often start
                // veter with a PATH that omits ~/.local/bin. Fall
                // back to a normal $PATH search, then to the user's
                // shell. execvp only returns on failure.
                let argv0 = CString::new("vmux").unwrap();
                let neighbor = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("vmux")))
                    .filter(|p| p.is_file())
                    .and_then(|p| {
                        CString::new(p.as_os_str().as_bytes()).ok()
                    });
                if let Some(abs) = neighbor {
                    let _ = execvp(&abs, &[&argv0]);
                }
                let _ = execvp(&argv0, &[&argv0]);
                // Honor `$SHELL` (the user's login shell) and fall back
                // to `/bin/sh` if it's unset or unusable, mirroring the
                // convention tmux/screen/alacritty all follow. The
                // CString conversions can fail only on interior NULs,
                // which `$SHELL` won't have in any sane setup.
                let shell_path = std::env::var("SHELL")
                    .ok()
                    .and_then(|s| CString::new(s).ok())
                    .unwrap_or_else(|| CString::new("/bin/sh").unwrap());
                let err = execvp(&shell_path, &[&shell_path]).unwrap_err();
                panic!("exec failed: {err}");
            }
            ForkptyResult::Parent { child, master } => {
                // The writer thread owns its own dup of the master so it
                // can block on `write(2)` independently of the reader
                // thread (full-duplex on the same open file description)
                // and of `resize`/`dup_master` on the main thread.
                let writer_fd = nix::unistd::dup(master.as_raw_fd())
                    .map_err(io::Error::other)?;
                let writer_fd = unsafe { OwnedFd::from_raw_fd(writer_fd) };
                let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>();
                std::thread::spawn(move || writer_loop(writer_fd, writer_rx));
                Ok(Pty {
                    master,
                    child,
                    writer_tx,
                })
            }
        }
    }

    /// Queue `data` for the child. Never blocks: the bytes are handed to
    /// the writer thread and flushed there. Returns an error only if the
    /// writer thread has gone (its fd closed), which callers treat the
    /// same as a failed write.
    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.writer_tx
            .send(data.to_vec())
            .map_err(|_| io::Error::other("pty writer thread is gone"))
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
            .map_err(io::Error::other)?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = kill(self.child, Signal::SIGHUP);
    }
}
