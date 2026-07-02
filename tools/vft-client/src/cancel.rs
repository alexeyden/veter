// Cancel-on-exit safety net for `vsend` / `vrecv`.
//
// A VFT transfer that the client abandons without a clean end
// (`EndUpload` Ok / `DownloadEnd`) leaves the host side running: an
// upload's partial destination file lingers, and — far worse — a
// download worker keeps streaming raw, byte-stuffed file bytes into the
// PTY. Once the client process is gone those bytes land on whatever now
// owns the tty (typically a shell), which interprets them as keystrokes:
// garbage commands, then often a crash.
//
// This module gives the client two backstops that both emit a
// `CancelTransfer` (§8.2) so the host stops and releases the transfer:
//
//   * `CancelGuard` — a RAII guard. If it is dropped still armed (an
//     early `return Err`, a panic, unwinding), its `Drop` writes the
//     cancel envelope to stdout. Call `disarm()` on the paths that have
//     already handled cleanup (clean completion, or an explicit
//     `cancel_and_drain`).
//
//   * a signal handler for SIGINT / SIGTERM / SIGHUP. Because the client
//     runs with `ISIG` cleared (raw mode), Ctrl-C arrives as a byte, not
//     a signal — but an external `kill`, or the terminal hanging up
//     (SIGHUP), would otherwise terminate the process without running
//     any `Drop`. The handler writes the same cancel envelope with an
//     async-signal-safe raw `write(2)`, then restores the default
//     disposition and re-raises so the exit status is still correct.
//
// For downloads the guard alone cannot swallow bytes already in flight;
// `cancel_and_drain` (used on the graceful error paths) additionally
// reads and discards incoming chunks until the host confirms the abort,
// so the terminal stays clean. The initial-burst cap on the host
// (`INITIAL_DOWNLOAD_BURST_BYTES`) bounds what can leak on the paths that
// cannot drain (signals / panics).

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use vft_protocol::command::{CancelTransferBody, Command};
use vft_protocol::encode::build_envelope;

use crate::stream::{HostFrame, ResponseStream};

/// Request id used for client-issued `CancelTransfer` frames. Response
/// ids only need to match within a transfer's own command stream; a
/// dedicated high value keeps it clearly distinct from the begin/ack ids.
pub const CANCEL_REQUEST_ID: u32 = u32::MAX;

/// Build the on-wire `CancelTransfer` envelope for `transfer_id`.
pub fn cancel_envelope(transfer_id: &str) -> Vec<u8> {
    build_envelope(&[(
        Command::CancelTransfer(CancelTransferBody {
            transfer_id: transfer_id.to_string(),
        }),
        CANCEL_REQUEST_ID,
    )])
}

// ---- signal handler plumbing -----------------------------------------

// The precomputed cancel envelope, published for the signal handler.
// Stored as a leaked (`Box::into_raw`) buffer so it stays valid for the
// whole process lifetime; the handler reads it with plain atomic loads,
// which are async-signal-safe. `LEN` is stored before `PTR` and read
// after, so a non-null `PTR` always sees a consistent length.
static SIG_ENVELOPE_PTR: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static SIG_ENVELOPE_LEN: AtomicUsize = AtomicUsize::new(0);
static SIG_HANDLERS_INSTALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_cancel_signal(sig: libc::c_int) {
    let ptr = SIG_ENVELOPE_PTR.load(Ordering::SeqCst);
    let len = SIG_ENVELOPE_LEN.load(Ordering::SeqCst);
    if !ptr.is_null() && len > 0 {
        // Best-effort async-signal-safe write straight to fd 1. Partial
        // writes are retried; any error (e.g. the terminal already hung
        // up) is ignored — there is nothing useful to do from here.
        let mut off = 0usize;
        while off < len {
            let n = unsafe {
                libc::write(
                    libc::STDOUT_FILENO,
                    ptr.add(off) as *const libc::c_void,
                    len - off,
                )
            };
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
    }
    // Restore the default disposition and re-raise so the process
    // terminates with the conventional status for this signal.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

fn install_signal_handler(envelope: &[u8]) {
    // Publish the envelope (leak a copy; it is tiny and lives for the
    // process). LEN first, then PTR, matching the handler's read order.
    let leaked: &'static mut [u8] = Box::leak(envelope.to_vec().into_boxed_slice());
    SIG_ENVELOPE_LEN.store(leaked.len(), Ordering::SeqCst);
    SIG_ENVELOPE_PTR.store(leaked.as_mut_ptr(), Ordering::SeqCst);

    // Install the handlers once per process.
    if SIG_HANDLERS_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            unsafe {
                libc::signal(sig, handle_cancel_signal as *const () as libc::sighandler_t);
            }
        }
    }
}

fn clear_signal_envelope() {
    // Leave the handlers installed (harmless), but stop them from
    // emitting a cancel for a transfer that has already ended cleanly.
    SIG_ENVELOPE_PTR.store(std::ptr::null_mut(), Ordering::SeqCst);
}

// ---- RAII guard -------------------------------------------------------

/// RAII net that emits `CancelTransfer` for `transfer_id` if the client
/// exits without disarming it. Also installs the signal handler so a
/// `kill` / hangup cancels too. Construct it right after the transfer
/// becomes active on the host; `disarm()` once the transfer has ended
/// (cleanly, or after an explicit `cancel_and_drain`).
pub struct CancelGuard {
    envelope: Vec<u8>,
    armed: bool,
}

impl CancelGuard {
    pub fn new(transfer_id: &str) -> Self {
        let envelope = cancel_envelope(transfer_id);
        install_signal_handler(&envelope);
        Self {
            envelope,
            armed: true,
        }
    }

    /// Mark the transfer as handled; `Drop` and the signal handler will
    /// no longer emit a cancel.
    pub fn disarm(&mut self) {
        self.armed = false;
        clear_signal_envelope();
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.armed {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(&self.envelope);
            let _ = out.flush();
            clear_signal_envelope();
        }
    }
}

/// Send `CancelTransfer` and then read-and-discard any further host
/// frames for `transfer_id` until the host confirms the abort
/// (`TransferAborted`) or `timeout` elapses. Used on the graceful
/// download error paths so in-flight `DownloadChunk` bytes are consumed
/// by the client rather than leaking onto the shell once it exits.
///
/// Best-effort: write / timeout failures are swallowed, since the caller
/// is already on its way out with a more informative error.
pub fn cancel_and_drain(stream: &ResponseStream, transfer_id: &str, timeout: Duration) {
    let env = cancel_envelope(transfer_id);
    {
        let mut out = std::io::stdout().lock();
        if out.write_all(&env).is_err() || out.flush().is_err() {
            return;
        }
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        // Poll in short slices: the in-flight chunks arrive continuously
        // while there is buffered data, so `None` here means either a
        // genuine lull (keep waiting for the abort) or that the reader hit
        // EOF (input closed — nothing more can come, so stop).
        let Some(frame) = stream.recv_timeout(remaining.min(Duration::from_millis(250))) else {
            if stream.at_eof() {
                return;
            }
            continue;
        };
        // Discard every frame; we only watch for the signals that tell us
        // the host has stopped streaming and freed the transfer:
        //   * TransferAborted for our id — the normal cancel outcome; all
        //     pre-cancel chunks precede it, so by now they're drained.
        //   * an Err response to our own CancelTransfer — the transfer was
        //     already gone (err_unknown_transfer), so nothing more is
        //     coming; return without waiting out the timeout.
        if let HostFrame::Vft {
            frame_type,
            request_id,
            body,
        } = frame
        {
            if frame_type == vft_protocol::frame::EVT_TRANSFER_ABORTED {
                let mut r = vft_protocol::codec::Reader::new(&body);
                if r.string().map(|id| id == transfer_id).unwrap_or(false) {
                    return;
                }
            } else if frame_type == vft_protocol::frame::RSP_ERR
                && request_id == CANCEL_REQUEST_ID
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vft_protocol::apc::ApcStream;
    use vft_protocol::codec::Reader;
    use vft_protocol::frame::{CMD_CANCEL_TRANSFER, MARKER_C2H};

    #[test]
    fn cancel_envelope_encodes_a_parseable_cancel_command() {
        let env = cancel_envelope("vrecv-42");
        // It is a well-formed client→host VFT envelope...
        let mut s = ApcStream::with_marker(*MARKER_C2H);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1, "expected exactly one envelope");
        // ...carrying a single CancelTransfer frame for our transfer id
        // with the dedicated cancel request id.
        let mut r = Reader::new(&out.payloads[0]);
        assert_eq!(r.u8().unwrap(), 0, "protocol version");
        let _payload_len = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        let body_len = r.u32().unwrap() as usize;
        let body = r.take(body_len).unwrap();
        assert_eq!(frame_type, CMD_CANCEL_TRANSFER);
        assert_eq!(request_id, CANCEL_REQUEST_ID);
        let cmd = vft_protocol::command::parse(frame_type, body).unwrap();
        match cmd {
            vft_protocol::command::Command::CancelTransfer(b) => {
                assert_eq!(b.transfer_id, "vrecv-42");
            }
            other => panic!("expected CancelTransfer, got {other:?}"),
        }
        assert!(r.at_end(), "trailing bytes after the frame");
    }
}
