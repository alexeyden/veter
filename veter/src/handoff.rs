//! The terminal's half of `veter_host::handoff`: the socket a
//! `vsd adopt` asks on for this window's pty.
//!
//! The listener only accepts and reads the request. Everything after
//! that — pausing the reader, snapshotting the engines, sending the
//! master, even naming the window's title — needs state that lives on
//! the event-loop thread, so the request goes there with its stream and
//! a wakeup.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use veter_host::handoff::{self, Request, SocketGuard};

/// A `vsd adopt` waiting on its answer.
pub struct Asked {
    pub stream: UnixStream,
    pub request: Request,
}

/// Held for the life of the window; dropping it unlinks the socket.
pub struct Listener {
    _guard: SocketGuard,
    pub requests: Receiver<Asked>,
}

/// Listen on `<runtime>/veter/<pid>.sock`, calling `wake` after queueing
/// each request.
pub fn listen(wake: impl Fn() + Send + 'static) -> io::Result<Listener> {
    handoff::ensure_runtime_dir()?;
    let path = handoff::socket_path(std::process::id());
    // Our pid is ours, so a file already there is a crashed veter's.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let guard = SocketGuard(path);
    let (tx, requests) = mpsc::channel();
    std::thread::Builder::new()
        .name("veter-handoff".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                // `vsd adopt` probes liveness by connecting and hanging
                // up; only a stream that sends a whole request counts.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Ok(request) = Request::read_from(&mut stream) else { continue };
                if tx.send(Asked { stream, request }).is_err() {
                    return;
                }
                wake();
            }
        })?;
    Ok(Listener { _guard: guard, requests })
}
