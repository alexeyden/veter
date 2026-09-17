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

// A second envelope written right after the cancel one: the progress
// bar's `DeleteElement`. The handler re-raises with the default
// disposition, so the process dies without unwinding — no `Drop`, no
// `ProgressUI::teardown`. Without this, a `kill` during a transfer would
// leave the bar's VGE element painted on the terminal permanently, since
// the element is host state that only an explicit delete ever frees.
static SIG_CLEANUP_PTR: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static SIG_CLEANUP_LEN: AtomicUsize = AtomicUsize::new(0);

/// Best-effort async-signal-safe write straight to fd 1. Partial writes
/// are retried; any error (e.g. the terminal already hung up) is ignored
/// — there is nothing useful to do from a signal handler.
unsafe fn raw_write_stdout(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
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

extern "C" fn handle_cancel_signal(sig: libc::c_int) {
    unsafe {
        raw_write_stdout(
            SIG_ENVELOPE_PTR.load(Ordering::SeqCst),
            SIG_ENVELOPE_LEN.load(Ordering::SeqCst),
        );
        // Cancel first, then erase the bar: the host stops streaming
        // before we drop the UI it was driving.
        raw_write_stdout(
            SIG_CLEANUP_PTR.load(Ordering::SeqCst),
            SIG_CLEANUP_LEN.load(Ordering::SeqCst),
        );
    }
    // Restore the default disposition and re-raise so the process
    // terminates with the conventional status for this signal.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Publish an envelope for the signal handler to write after the cancel
/// — used by `VgeProgress` to register its `DeleteElement` so a killed
/// client still takes its progress bar off the screen. Replaces any
/// previously registered cleanup. `LEN` is stored before `PTR` and read
/// after, so a non-null `PTR` always sees a consistent length.
pub fn set_signal_cleanup_envelope(envelope: &[u8]) {
    let leaked: &'static mut [u8] = Box::leak(envelope.to_vec().into_boxed_slice());
    SIG_CLEANUP_LEN.store(leaked.len(), Ordering::SeqCst);
    SIG_CLEANUP_PTR.store(leaked.as_mut_ptr(), Ordering::SeqCst);
}

/// Stop the handler from emitting a cleanup that has already been done
/// (a normal `teardown`), so it can't delete an id the client may have
/// since reused.
pub fn clear_signal_cleanup_envelope() {
    SIG_CLEANUP_PTR.store(std::ptr::null_mut(), Ordering::SeqCst);
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

/// A `TransferAborted` from the host (§8.3), as the error a transfer
/// loop returns when it reads one.
///
/// Distinct from every other failure because it leaves nothing to clean
/// up: the host has already dropped the transfer, and every chunk it
/// sent precedes the event, so a `CancelTransfer` would only draw an
/// `err_unknown_transfer` — or no answer at all, when the abort was a
/// `vsd` session detaching and the daemon now relays the cancel to
/// nobody, leaving [`cancel_and_drain`] to wait out its whole budget.
#[derive(Debug)]
pub struct HostAborted {
    pub transfer_id: String,
    pub reason: u8,
    pub message: String,
}

impl HostAborted {
    /// Decode a `TransferAborted` body, tolerating a truncated one.
    pub fn decode(body: &[u8]) -> Self {
        let mut r = vft_protocol::codec::Reader::new(body);
        Self {
            transfer_id: r.string().unwrap_or("").to_owned(),
            reason: r.u8().unwrap_or(0),
            message: r.string().unwrap_or("").to_owned(),
        }
    }

    /// Whether `err` is the host having already ended `transfer_id`,
    /// so the caller can skip the cancel.
    pub fn ended(err: &anyhow::Error, transfer_id: &str) -> bool {
        err.downcast_ref::<Self>()
            .is_some_and(|a| a.transfer_id == transfer_id)
    }
}

impl std::fmt::Display for HostAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "transfer {} aborted (reason={}): {}",
            self.transfer_id, self.reason, self.message
        )
    }
}

impl std::error::Error for HostAborted {}

/// Say so when [`cancel_and_drain`] gave up before the host confirmed.
///
/// Anything the host writes from here on reaches a terminal already
/// back in canonical mode, where the line editor takes protocol bytes
/// for keystrokes — zsh binds `ESC _` to insert-last-word and `ESC H`
/// to run-help, so a stray envelope becomes an executed command line.
/// Without this note the corruption that follows looks like it came
/// from nowhere.
pub fn warn_if_undrained(drained: bool, budget: Duration) {
    if drained {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = write!(
        out,
        "warning: host did not confirm the cancel within {}s; it may still \
         write to this terminal. If the prompt misbehaves afterwards, press \
         Enter and run `reset`.\r\n",
        budget.as_secs()
    );
    let _ = out.flush();
}

/// Send `CancelTransfer` and then read-and-discard any further host
/// frames for `transfer_id` until the host confirms the abort
/// (`TransferAborted`) or `timeout` elapses. Used on the graceful error
/// paths of both directions so in-flight host→client bytes are consumed
/// by the client rather than leaking onto the shell once it exits.
///
/// Returns `true` if the host confirmed and it is therefore safe to
/// exit, `false` if the deadline passed first — in which case the host
/// may still write frames into a terminal whose line editor will treat
/// them as keystrokes, and the caller should say so.
///
/// Best-effort otherwise: write failures are swallowed, since the caller
/// is already on its way out with a more informative error.
#[must_use = "a false return means frames may still leak onto the shell"]
pub fn cancel_and_drain(
    stream: &ResponseStream,
    transfer_id: &str,
    timeout: Duration,
) -> bool {
    let env = cancel_envelope(transfer_id);
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
        // Poll in short slices: the in-flight chunks arrive continuously
        // while there is buffered data, so `None` here means either a
        // genuine lull (keep waiting for the abort) or that the reader hit
        // EOF (input closed — nothing more can come, so stop).
        let Some(frame) = stream.recv_timeout(remaining.min(Duration::from_millis(250))) else {
            if stream.at_eof() {
                // Input closed: nothing more can arrive, so there is
                // nothing left to leak.
                return true;
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
                    return true;
                }
            } else if frame_type == vft_protocol::frame::RSP_ERR
                && request_id == CANCEL_REQUEST_ID
            {
                return true;
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

    /// Only an abort of *this* transfer excuses the cancel; any other
    /// failure, or an abort of some other transfer, still needs one.
    #[test]
    fn a_host_abort_ends_only_its_own_transfer() {
        let body = vft_protocol::envelope::transfer_aborted_body(
            "vrecv-7",
            vft_protocol::frame::ABORT_HOST_RESET,
            "session detached",
        );
        let err = anyhow::Error::new(HostAborted::decode(&body));
        assert!(HostAborted::ended(&err, "vrecv-7"));
        assert!(!HostAborted::ended(&err, "vrecv-8"));
        assert!(!HostAborted::ended(&anyhow::anyhow!("timed out"), "vrecv-7"));
        assert_eq!(
            err.to_string(),
            "transfer vrecv-7 aborted (reason=4): session detached"
        );
    }

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
