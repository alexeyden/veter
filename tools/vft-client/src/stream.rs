// Background stdin reader that demultiplexes VGE and VFT host
// envelopes onto a single channel of typed `HostFrame` values.
//
// Spawn this AFTER all synchronous probe round-trips (the thread
// owns stdin from that point on; the main thread must not call
// `read_stdin` while the reader is running).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    interrupt: Arc<AtomicBool>,
}

impl ResponseStream {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let eof = Arc::new(AtomicBool::new(false));
        let interrupt = Arc::new(AtomicBool::new(false));
        let eof_thread = eof.clone();
        let interrupt_thread = interrupt.clone();
        std::thread::spawn(move || run_reader(tx, eof_thread, interrupt_thread));
        Self {
            rx,
            eof,
            interrupt,
        }
    }

    /// True once the reader has seen a bare `ETX` (Ctrl+C) keystroke on
    /// the input. "Bare" means outside any VGE/VFT envelope: the host's
    /// download bytes carry their own `0x03`s *inside* envelopes, which
    /// the APC parser keeps out of the passthrough, so this only ever
    /// flips for an actual interrupt keypress. Long-running command loops
    /// poll it to cancel promptly (the tty runs with `ISIG` cleared, so
    /// Ctrl+C never arrives as a signal). Latching: never clears.
    pub fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::Relaxed)
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

    /// Round-trip a VGE `Probe` and drain any frames that arrive
    /// before its response. Used at exit time to flush VGE Ok
    /// responses for the trailing UpdateCommand / DeleteElement
    /// envelopes the progress UI emitted; otherwise they leak into
    /// the shell's stdin where zsh's zle binds `ESC _` to
    /// `insert-last-word` and pastes the previous argv plus the
    /// literal marker bytes (`<argv> vge <argv> vge …`).
    ///
    /// Since VGE guarantees one response per command in order
    /// (spec §1.2), receiving the Probe's ProbeResponse proves
    /// every prior command's Ok has already been read off the
    /// wire. Returns `false` on timeout or if stdout fails.
    pub fn vge_barrier(&self, request_id: u32, timeout: Duration) -> bool {
        let env = vge_protocol::encode::build_envelope(&[(
            vge_protocol::command::Command::Probe,
            request_id,
        )]);
        {
            let mut out = std::io::stdout().lock();
            if out.write_all(&env).is_err() || out.flush().is_err() {
                return false;
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let Some(frame) = self.recv_timeout(remaining) else {
                return false;
            };
            if let HostFrame::Vge {
                frame_type,
                request_id: rid,
                ..
            } = frame
                && rid == request_id
                && frame_type == vge_protocol::frame::RSP_PROBE
            {
                return true;
            }
        }
    }
}

/// ETX (Ctrl+C). A bare one in the passthrough is a user interrupt
/// keystroke; download-data `0x03`s stay inside VFT envelopes and never
/// reach the passthrough, so this can't be confused with file bytes.
const ETX: u8 = 0x03;

fn run_reader(tx: Sender<HostFrame>, eof: Arc<AtomicBool>, interrupt: Arc<AtomicBool>) {
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
        // vft_out.passthrough is anything that is neither a VGE nor a VFT
        // envelope — normally just user keystrokes (the host's replies are
        // all enveloped). We otherwise discard it, but watch it for a bare
        // Ctrl+C so a long-running transfer can be interrupted from the
        // keyboard even though the tty has `ISIG` cleared.
        if vft_out.passthrough.contains(&ETX) {
            interrupt.store(true, Ordering::Relaxed);
        }
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

#[cfg(test)]
mod tests {
    use vft_protocol::envelope::{append_frame, wrap_h2c_envelope};

    // Mirror the reader thread's parser pipeline (VGE then VFT) and return
    // the residual passthrough that run_reader scans for a bare Ctrl+C.
    fn passthrough(input: &[u8]) -> Vec<u8> {
        let mut vge =
            vge_protocol::apc::ApcStream::with_marker(*vge_protocol::frame::MARKER_T2C);
        let mut vft =
            vft_protocol::apc::ApcStream::with_marker(*vft_protocol::frame::MARKER_H2C);
        let vge_out = vge.feed(input);
        vft.feed(&vge_out.passthrough).passthrough
    }

    #[test]
    fn bare_ctrl_c_reaches_passthrough() {
        // A lone ETX keystroke (optionally amid other typed bytes) lands
        // in the passthrough, so the reader flags it as an interrupt.
        assert!(passthrough(&[0x03]).contains(&0x03));
        assert!(passthrough(b"ab\x03cd").contains(&0x03));
    }

    #[test]
    fn download_data_etx_stays_out_of_passthrough() {
        // An 0x03 carried *inside* a VFT download envelope (the common
        // case for binary file data) is extracted as frame body and must
        // NOT appear in the passthrough — otherwise file bytes would be
        // mistaken for a Ctrl+C. `stuff` does not escape 0x03, so it
        // travels literally in the body, which is exactly the case we must
        // not misread.
        let mut frames = Vec::new();
        append_frame(&mut frames, 0x80 /* DownloadChunk */, 0, &[0x03, 0x00, 0x03, 0xFF]);
        let env = wrap_h2c_envelope(&frames);
        assert!(env.iter().any(|&b| b == 0x03), "fixture must contain a raw 0x03");
        assert!(
            !passthrough(&env).contains(&0x03),
            "download-data 0x03 leaked into the interrupt-scanned passthrough"
        );
    }
}
