// Per-transfer worker threads. Each active VFT transfer owns one
// worker that performs blocking filesystem I/O, so the engine's
// command-parser thread never waits on a write/read syscall (§9 in
// the spec).

use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use vft_protocol::frame::*;

/// Result of a native file-picker dialog spawned for a deferred-form
/// BeginDownload (§7.1).
pub enum PickerResult {
    Selected(PathBuf),
    Cancelled,
}

/// Open a native file dialog (sync, blocking), post the result onto
/// `tx`, and tick the engine via `wakeup` so `drive()` consumes the
/// outcome on the next main-loop pass. Errors from the dialog
/// backend (e.g. no display, GTK init failure) surface as
/// `Cancelled` — the engine then returns `err_cancelled` to the
/// client, which is the same code the user-cancel path uses.
///
/// Under `cfg(test)` the GTK dialog is suppressed and an immediate
/// `Cancelled` is sent, so unit tests can exercise the spawn /
/// drain / response-slot plumbing without blocking on a real UI.
pub fn run_picker(title: String, tx: Sender<PickerResult>, wakeup: Wakeup) {
    #[cfg(test)]
    {
        let _ = title;
        let _ = tx.send(PickerResult::Cancelled);
        wakeup();
        return;
    }
    #[cfg(not(test))]
    {
        let result = std::panic::catch_unwind(move || {
            rfd::FileDialog::new().set_title(title).pick_file()
        });
        let outcome = match result {
            Ok(Some(p)) => PickerResult::Selected(p),
            _ => PickerResult::Cancelled,
        };
        let _ = tx.send(outcome);
        wakeup();
    }
}

/// Closure called by a worker after pushing an event. The host wires
/// this to the winit event-loop proxy so the main thread ticks even
/// when the PTY is silent (§ "drive cycles" in the engine).
pub type Wakeup = Arc<dyn Fn() + Send + Sync>;

/// Commands the engine pushes to a worker.
pub enum WorkerCmd {
    /// Append `data` to the destination file. The engine has already
    /// validated the chunk's offset against its running cursor before
    /// queuing this command (see VftEngine::handle_upload_chunk), so
    /// the worker just writes sequentially.
    Write { data: Vec<u8> },
    /// Stop reading new chunks and exit. The download worker yields
    /// `Aborted { reason = client_cancel }` before exiting.
    Cancel,
    /// Finalise the upload: flush, fsync, set permissions / mtime,
    /// optionally launch the user's default app for the result, and
    /// reply with `Finalised`. `request_id` is echoed in the reply
    /// so the engine can match it to the originating EndUpload.
    /// `open_after` triggers `opener::open(final_path)` after the
    /// file is durable; used by deferred-form uploads (§6.1) to
    /// hand the result to the user's default viewer.
    Finalize {
        request_id: u32,
        mode: u32,
        mtime: i64,
        open_after: bool,
    },
}

/// Events a worker pushes to the engine.
pub enum WorkerEvt {
    /// Download produced a chunk.
    DownloadChunk { offset: u64, data: Vec<u8> },
    /// Download read EOF and exited normally.
    DownloadEnd { bytes_sent: u64 },
    /// Upload finalised cleanly. Body for the deferred EndUpload Ok.
    Finalised {
        request_id: u32,
        final_path: PathBuf,
        bytes_written: u64,
    },
    /// Transfer ended abnormally. `reason` is one of the §8.3
    /// `ABORT_*` constants. `pending_request_id` is `Some(_)` if the
    /// abort happens while an EndUpload is still waiting on its
    /// FinalizeOk; the engine fills the matching slot with `Err`.
    Aborted {
        reason: u8,
        message: String,
        pending_request_id: Option<u32>,
    },
}

/// Upload worker. Drains `cmd_rx` for chunk writes and a final
/// Finalize. Holds the destination `File` and the worker-side counter
/// of bytes successfully written (the shared atomic is what the
/// engine reads when it answers RequestAck §8.1).
pub fn run_upload(
    mut file: File,
    final_path: PathBuf,
    bytes_processed: Arc<AtomicU64>,
    cmd_rx: Receiver<WorkerCmd>,
    evt_tx: Sender<WorkerEvt>,
    wakeup: Wakeup,
) {
    let send = |evt: WorkerEvt| {
        let _ = evt_tx.send(evt);
        wakeup();
    };
    loop {
        match cmd_rx.recv() {
            Ok(WorkerCmd::Write { data }) => {
                if let Err(e) = file.write_all(&data) {
                    send(WorkerEvt::Aborted {
                        reason: classify_io_error(&e),
                        message: e.to_string(),
                        pending_request_id: None,
                    });
                    return;
                }
                bytes_processed.fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            Ok(WorkerCmd::Finalize {
                request_id,
                mode,
                mtime,
                open_after,
            }) => {
                if let Err(e) = file.flush() {
                    send(WorkerEvt::Aborted {
                        reason: ABORT_IO_ERROR,
                        message: format!("flush: {e}"),
                        pending_request_id: Some(request_id),
                    });
                    return;
                }
                if let Err(e) = file.sync_all() {
                    send(WorkerEvt::Aborted {
                        reason: ABORT_IO_ERROR,
                        message: format!("fsync: {e}"),
                        pending_request_id: Some(request_id),
                    });
                    return;
                }
                drop(file);
                if mode != 0 {
                    apply_mode(&final_path, mode);
                }
                if mtime != 0 {
                    apply_mtime(&final_path, mtime);
                }
                if open_after {
                    maybe_open_default(&final_path);
                }
                let bytes_written = bytes_processed.load(Ordering::Relaxed);
                send(WorkerEvt::Finalised {
                    request_id,
                    final_path,
                    bytes_written,
                });
                return;
            }
            Ok(WorkerCmd::Cancel) => {
                // Best-effort: drop the file handle, leave the partial
                // file on disk for the host's policy to clean up.
                drop(file);
                send(WorkerEvt::Aborted {
                    reason: ABORT_CLIENT_CANCEL,
                    message: String::new(),
                    pending_request_id: None,
                });
                return;
            }
            Err(_) => {
                // Engine dropped its sender — nothing to flush. Exit
                // silently; the engine has already reaped this
                // transfer.
                return;
            }
        }
    }
}

/// Download worker. Reads chunks of up to `chunk_size` bytes from
/// `file` and pushes them as `DownloadChunk` events. Emits
/// `DownloadEnd` on EOF and `Aborted` on error or cancel.
pub fn run_download(
    mut file: File,
    chunk_size: u32,
    cmd_rx: Receiver<WorkerCmd>,
    evt_tx: Sender<WorkerEvt>,
    wakeup: Wakeup,
) {
    let send = |evt: WorkerEvt| {
        let _ = evt_tx.send(evt);
        wakeup();
    };
    let mut buf = vec![0u8; chunk_size as usize];
    let mut sent: u64 = 0;
    loop {
        // Check for cancel without blocking; otherwise read the next
        // chunk. The cmd channel is the only signal a download worker
        // listens for, so try_recv is sufficient.
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Cancel) => {
                send(WorkerEvt::Aborted {
                    reason: ABORT_CLIENT_CANCEL,
                    message: String::new(),
                    pending_request_id: None,
                });
                return;
            }
            Ok(_) => {
                // Other commands aren't meaningful for downloads —
                // ignore (defensive).
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
        }

        match file.read(&mut buf) {
            Ok(0) => {
                send(WorkerEvt::DownloadEnd { bytes_sent: sent });
                return;
            }
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                send(WorkerEvt::DownloadChunk {
                    offset: sent,
                    data: chunk,
                });
                sent += n as u64;
            }
            Err(e) => {
                send(WorkerEvt::Aborted {
                    reason: classify_io_error(&e),
                    message: e.to_string(),
                    pending_request_id: None,
                });
                return;
            }
        }
    }
}

fn classify_io_error(e: &std::io::Error) -> u8 {
    use std::io::ErrorKind::*;
    match e.kind() {
        // ENOSPC on Linux maps to StorageFull on stable Rust >=1.83;
        // older Rust versions surface it as Other. Match on raw_os
        // when available so we still detect ENOSPC there.
        StorageFull => ABORT_DISK_FULL,
        _ => {
            #[cfg(unix)]
            if let Some(28) = e.raw_os_error() {
                return ABORT_DISK_FULL;
            }
            ABORT_IO_ERROR
        }
    }
}

/// Hand the finalised file to the user's default application. Used
/// by deferred-form uploads (§6.1) so a `vsend ./screenshot.png` lands
/// in `$TMPDIR` and then immediately pops up in the user's image
/// viewer. Errors are swallowed — failing to launch the app does not
/// abort the transfer; the file is already durable.
///
/// Disabled under `cfg(test)` so unit tests don't spawn the user's
/// real applications.
#[cfg(not(test))]
fn maybe_open_default(path: &PathBuf) {
    let _ = opener::open(path);
}

#[cfg(test)]
fn maybe_open_default(_path: &PathBuf) {
    // intentional no-op in test builds
}

#[cfg(unix)]
fn apply_mode(path: &PathBuf, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(mode & 0o7777);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn apply_mode(_path: &PathBuf, _mode: u32) {
    // Non-POSIX platforms ignore mode bits silently — the spec allows
    // hosts to clamp or ignore them (§6.1).
}

fn apply_mtime(path: &PathBuf, mtime_secs: i64) {
    // Best-effort: use filetime if available via std (Rust 1.75+) or
    // utimes(2) on unix. The standard library currently only exposes
    // an unstable set_modified, so we drop into libc::utimes on unix
    // and silently no-op elsewhere.
    #[cfg(unix)]
    {
        use std::ffi::CString;
        let Some(c) = path.to_str().and_then(|s| CString::new(s).ok()) else {
            return;
        };
        let tv = [
            libc::timeval {
                tv_sec: mtime_secs as libc::time_t,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: mtime_secs as libc::time_t,
                tv_usec: 0,
            },
        ];
        unsafe {
            // Safe: tv has 2 elements as utimes requires.
            libc::utimes(c.as_ptr(), tv.as_ptr());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mtime_secs);
    }
}
