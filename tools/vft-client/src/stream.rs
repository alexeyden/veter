// Background stdin reader that demultiplexes VGE and VFT host
// envelopes onto a single channel of typed `HostFrame` values.
//
// Spawn this AFTER all synchronous probe round-trips (the thread
// owns stdin from that point on; the main thread must not call
// `read_stdin` while the reader is running).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use crate::tty::read_stdin;

#[derive(Debug, Clone)]
pub enum HostFrame {
    Vft {
        frame_type: u8,
        request_id: u32,
        body: Vec<u8>,
    },
    Vge {
        frame_type: u8,
        request_id: u32,
        body: Vec<u8>,
    },
}

pub struct ResponseStream {
    rx: Receiver<HostFrame>,
    eof: Arc<AtomicBool>,
}

impl ResponseStream {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let eof = Arc::new(AtomicBool::new(false));
        let eof_thread = eof.clone();
        std::thread::spawn(move || run_reader(tx, eof_thread));
        Self { rx, eof }
    }

    /// Non-blocking pull. `Some(frame)` if available, `None`
    /// otherwise. `at_eof()` distinguishes a benign empty queue from
    /// the underlying stdin closing.
    pub fn try_recv(&self) -> Option<HostFrame> {
        match self.rx.try_recv() {
            Ok(f) => Some(f),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Block for up to `timeout` waiting on the next frame.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<HostFrame> {
        match self.rx.recv_timeout(timeout) {
            Ok(f) => Some(f),
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// True once the stdin reader has hit EOF / error and exited.
    /// Pending frames in the buffer are still drainable via try_recv
    /// after this flips.
    pub fn at_eof(&self) -> bool {
        self.eof.load(Ordering::Relaxed)
    }

    /// Block until the stream has been idle for `idle_timeout`,
    /// discarding any frames that arrive in the meantime. Used at
    /// exit time so that VGE Ok responses (for the trailing
    /// UpdateCommand / DeleteElement envelopes the progress UI
    /// emits) don't leak into the shell's stdin where readline
    /// would echo them as `^[_vge…^[\` caret notation.
    pub fn drain_idle(&self, idle_timeout: Duration) {
        while self.recv_timeout(idle_timeout).is_some() {}
    }
}

fn run_reader(tx: Sender<HostFrame>, eof: Arc<AtomicBool>) {
    let mut vge_apc =
        vge_protocol::apc::ApcStream::with_marker(*vge_protocol::frame::MARKER_T2C);
    let mut vft_apc =
        vft_protocol::apc::ApcStream::with_marker(*vft_protocol::frame::MARKER_H2C);
    let mut buf = [0u8; 8192];
    loop {
        let n = match read_stdin(&mut buf) {
            Ok(0) => {
                eof.store(true, Ordering::Relaxed);
                return;
            }
            Ok(n) => n,
            Err(_) => {
                eof.store(true, Ordering::Relaxed);
                return;
            }
        };
        let vge_out = vge_apc.feed(&buf[..n]);
        let vft_out = vft_apc.feed(&vge_out.passthrough);
        for payload in vge_out.payloads {
            emit_frames(&payload, &tx, true);
        }
        for payload in vft_out.payloads {
            emit_frames(&payload, &tx, false);
        }
        // vft_out.passthrough contains anything that's neither a VGE
        // nor a VFT envelope (e.g. unsolicited control replies). v1
        // discards it — vsend/vrecv don't drive any further DSR
        // queries after probe time.
    }
}

fn emit_frames(payload: &[u8], tx: &Sender<HostFrame>, vge: bool) {
    let mut r = vft_protocol::codec::Reader::new(payload);
    if r.u8().is_err() {
        return;
    }
    if r.u32().is_err() {
        return;
    }
    while !r.at_end() {
        let Ok(frame_type) = r.u8() else { return };
        let Ok(request_id) = r.u32() else { return };
        let Ok(body_len) = r.u32() else { return };
        let Ok(body) = r.take(body_len as usize) else {
            return;
        };
        let frame = if vge {
            HostFrame::Vge {
                frame_type,
                request_id,
                body: body.to_vec(),
            }
        } else {
            HostFrame::Vft {
                frame_type,
                request_id,
                body: body.to_vec(),
            }
        };
        if tx.send(frame).is_err() {
            return;
        }
    }
}
