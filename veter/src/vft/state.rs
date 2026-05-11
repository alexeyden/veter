// VFT engine: command dispatch, transfer table, deferred-response
// queue, and worker-channel draining.
//
// The engine is driven by two entry points:
//
//   * `process_pty_chunk(bytes)` — fed each PTY chunk after PRT/VGE
//     have peeled off their envelopes. Returns the passthrough bytes
//     that should land on the host vt100. Synchronous responses (and
//     synthetic events that fire while handling a command) are
//     queued internally.
//
//   * `drive()` — called every event-loop tick to drain each active
//     transfer's worker channel. This is what surfaces async events
//     (DownloadChunk, DownloadEnd, Aborted) and fills the deferred
//     EndUpload response slot when its worker reports back.
//
// `take_responses()` returns the wire envelope ready to write to the
// PTY master.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use vft_protocol::apc::ApcStream;
use vft_protocol::codec::Reader;
use vft_protocol::command::{
    parse, BeginDownloadBody, BeginUploadBody, CancelTransferBody, Command, EndUploadBody,
    ReportDownloadAckBody, RequestAckBody, UploadChunkBody,
};
use vft_protocol::envelope::{
    append_frame, download_chunk_body, download_end_body, err_body, ok_begin_download_body,
    ok_begin_upload_body, ok_end_upload_body, transfer_aborted_body, upload_ack_body,
    wrap_h2c_envelope, ProbeBody,
};
use vft_protocol::frame::*;

use super::path;
use super::worker::{self, WorkerCmd, WorkerEvt};
pub use super::worker::Wakeup;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_concurrent_transfers: u32,
    pub max_chunk_bytes: u32,
    pub max_path_bytes: u32,
    /// `0` means no host-side limit on a single transfer's size.
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        // Spec §11 recommended budget.
        Self {
            max_concurrent_transfers: 8,
            max_chunk_bytes: 4 * 1024 * 1024,
            max_path_bytes: 4096,
            max_file_bytes: 0,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Direction {
    Upload,
    Download,
}

struct TransferHandle {
    direction: Direction,
    /// Monotonic byte cursor used by upload offset checks (§6.2). For
    /// downloads this is the running `bytes_sent` mirror.
    bytes_received: u64,
    /// Shared with the upload worker; updated after each successful
    /// write so RequestAck (§8.1) can answer with a fresh value.
    bytes_processed: Arc<AtomicU64>,
    /// `0` when the client did not declare a size (§6.1).
    total_bytes: u64,
    /// Recorded for diagnostics; the worker owns the working copy.
    #[allow(dead_code)]
    resolved_path: PathBuf,
    cmd_tx: Sender<WorkerCmd>,
    evt_rx: Receiver<WorkerEvt>,
    /// EndUpload was issued; further UploadChunks fail.
    closing: bool,
    /// Echoed back into the worker's Finalize step. Only meaningful
    /// for uploads.
    upload_mode: u32,
    upload_mtime: i64,
    /// Set on deferred-form uploads (§6.1): after finalisation the
    /// worker calls `opener::open(final_path)` so the user's default
    /// app pops up for the just-uploaded file.
    open_after_finalize: bool,
}

enum ResponseSlot {
    Ready {
        frame_type: u8,
        request_id: u32,
        body: Vec<u8>,
    },
    /// Filled later by `fill_slot` when an async worker reports back.
    /// Subsequent slots cannot flush until this one resolves (§1.2
    /// strict response ordering).
    Pending { request_id: u32 },
}

pub struct VftEngine {
    apc: ApcStream,
    /// Strictly-ordered slots for per-command responses. Front is the
    /// next response to emit.
    response_queue: VecDeque<ResponseSlot>,
    /// Frames (responses popped from `response_queue` plus events) that
    /// the next `take_responses()` will wrap into a single envelope.
    ready_frames: Vec<u8>,
    transfers: HashMap<String, TransferHandle>,
    limits: Limits,
    wakeup: Wakeup,
}

impl VftEngine {
    /// Convenience wrapper for tests and ad-hoc usage that takes a
    /// closure rather than a pre-wrapped `Arc`. Production paths use
    /// `with_wakeup` so a single `Arc<dyn Fn>` is shared with every
    /// per-portal engine; under cargo's bin-only build this method is
    /// only reachable from `cargo test`, hence the lint allow.
    #[allow(dead_code)]
    pub fn new<F: Fn() + Send + Sync + 'static>(wakeup: F) -> Self {
        Self::with_wakeup(Arc::new(wakeup))
    }

    /// Construct an engine that shares an existing wakeup `Arc`.
    /// Used by the PRT engine when it spawns a per-portal VFT engine
    /// for §10: the host's wakeup is cloned once at engine
    /// construction and re-used for every portal so a worker in any
    /// scope ultimately ticks the same main loop.
    pub fn with_wakeup(wakeup: Wakeup) -> Self {
        Self {
            apc: ApcStream::new(),
            response_queue: VecDeque::new(),
            ready_frames: Vec::new(),
            transfers: HashMap::new(),
            limits: Limits::default(),
            wakeup,
        }
    }

    /// Read the engine's currently-active limits. Useful for
    /// diagnostics; consumers normally just construct with `new(...)`.
    #[allow(dead_code)]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Override the default limits. Currently only used in tests.
    #[allow(dead_code)]
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Number of currently active transfers — useful for diagnostics
    /// and tests.
    #[allow(dead_code)]
    pub fn active_transfers(&self) -> usize {
        self.transfers.len()
    }

    /// Ingest raw PTY bytes. Returns the passthrough byte slice that
    /// should be forwarded to the next layer (typically the host
    /// vt100 parser). Synchronous command handling happens here;
    /// asynchronous worker events are surfaced by `drive()`.
    pub fn process_pty_chunk(&mut self, input: &[u8]) -> Vec<u8> {
        let out = self.apc.feed(input);
        for payload in out.payloads {
            self.handle_envelope_payload(&payload);
        }
        out.passthrough
    }

    /// Poll every active transfer's worker channel and act on any
    /// pending events. The host calls this every event-loop tick so
    /// downloads can stream chunks without waiting for further client
    /// input on the PTY.
    pub fn drive(&mut self) {
        let ids: Vec<String> = self.transfers.keys().cloned().collect();
        for id in ids {
            self.drain_worker(&id);
        }
    }

    /// Take queued response/event bytes ready to write to the PTY
    /// master. Wraps them into a single host→client envelope.
    pub fn take_responses(&mut self) -> Vec<u8> {
        if self.ready_frames.is_empty() {
            return Vec::new();
        }
        let frames = std::mem::take(&mut self.ready_frames);
        wrap_h2c_envelope(&frames)
    }

    /// Abort every active transfer with the given reason. Called by
    /// the host when a full or soft reset (RIS / DECSTR) is observed
    /// elsewhere in the byte stream (§5.6). Workers are signalled to
    /// cancel, and a `TransferAborted` event is emitted for each
    /// transfer on its way out.
    pub fn abort_all(&mut self, reason: u8, message: &str) {
        let ids: Vec<String> = self.transfers.keys().cloned().collect();
        for id in ids {
            if let Some(h) = self.transfers.remove(&id) {
                let _ = h.cmd_tx.send(WorkerCmd::Cancel);
                self.emit_event(
                    EVT_TRANSFER_ABORTED,
                    transfer_aborted_body(&id, reason, message),
                );
            }
        }
    }

    // -------- envelope unpacking + dispatch ----------------------------

    fn handle_envelope_payload(&mut self, payload: &[u8]) {
        let mut r = Reader::new(payload);
        let version = match r.u8() {
            Ok(v) => v,
            Err(_) => return,
        };
        if version > PROTOCOL_VERSION {
            // We can't parse a future version; drop silently — there's
            // no request_id we could attach an Err to without a frame
            // header.
            return;
        }
        let _payload_len = match r.u32() {
            Ok(v) => v,
            Err(_) => return,
        };
        while !r.at_end() {
            let frame_type = match r.u8() {
                Ok(v) => v,
                Err(_) => return,
            };
            let request_id = match r.u32() {
                Ok(v) => v,
                Err(_) => return,
            };
            let body_len = match r.u32() {
                Ok(v) => v as usize,
                Err(_) => return,
            };
            let body = match r.take(body_len) {
                Ok(b) => b,
                Err(_) => return,
            };
            self.dispatch(frame_type, request_id, body);
        }
    }

    fn dispatch(&mut self, frame_type: u8, request_id: u32, body: &[u8]) {
        let cmd = match parse(frame_type, body) {
            Ok(c) => c,
            Err(code) => {
                self.push_err(request_id, code, "");
                return;
            }
        };
        match cmd {
            Command::Probe => self.handle_probe(request_id),
            Command::BeginUpload(b) => self.handle_begin_upload(request_id, b),
            Command::UploadChunk(b) => self.handle_upload_chunk(request_id, b),
            Command::EndUpload(b) => self.handle_end_upload(request_id, b),
            Command::BeginDownload(b) => self.handle_begin_download(request_id, b),
            Command::ReportDownloadAck(b) => self.handle_report_download_ack(request_id, b),
            Command::RequestAck(b) => self.handle_request_ack(request_id, b),
            Command::CancelTransfer(b) => self.handle_cancel_transfer(request_id, b),
        }
    }

    // -------- per-command handlers -------------------------------------

    fn handle_probe(&mut self, rid: u32) {
        let body = ProbeBody {
            protocol_version: PROTOCOL_VERSION as u16,
            max_concurrent_transfers: self.limits.max_concurrent_transfers,
            max_chunk_bytes: self.limits.max_chunk_bytes,
            max_path_bytes: self.limits.max_path_bytes,
            max_file_bytes: self.limits.max_file_bytes,
            features: FEAT_UPLOAD | FEAT_DOWNLOAD,
        }
        .encode();
        self.push_ready(rid, RSP_PROBE, body);
    }

    fn handle_begin_upload(&mut self, rid: u32, b: BeginUploadBody) {
        if self.transfers.contains_key(&b.transfer_id) {
            self.push_err(rid, ERR_DUPLICATE_TRANSFER, "id in use");
            return;
        }
        if self.transfers.len() as u32 >= self.limits.max_concurrent_transfers {
            self.push_err(rid, ERR_TOO_MANY_TRANSFERS, "transfer budget exhausted");
            return;
        }
        if b.host_path.len() > self.limits.max_path_bytes as usize {
            self.push_err(rid, ERR_PATH_TOO_LONG, "host_path too long");
            return;
        }
        if self.limits.max_file_bytes != 0 && b.total_bytes > self.limits.max_file_bytes {
            self.push_err(rid, ERR_TOO_MANY_BYTES, "exceeds max_file_bytes");
            return;
        }

        let deferred = b.host_path.is_empty();
        let resolved_path = if deferred {
            path::deferred_upload_destination(&b.basename)
        } else {
            match path::resolve(&b.host_path) {
                Ok(p) => p,
                Err(e) => {
                    self.push_err(rid, e.code, e.message);
                    return;
                }
            }
        };

        let mut opts = OpenOptions::new();
        opts.write(true).truncate(true);
        if b.flags & FLAG_OVERWRITE != 0 {
            opts.create(true);
        } else {
            opts.create_new(true);
        }
        let file = match opts.open(&resolved_path) {
            Ok(f) => f,
            Err(e) => {
                let code = match e.kind() {
                    std::io::ErrorKind::AlreadyExists => ERR_PATH_EXISTS,
                    std::io::ErrorKind::PermissionDenied => ERR_PATH_DENIED,
                    std::io::ErrorKind::NotFound => ERR_PATH_INVALID,
                    _ => ERR_IO,
                };
                self.push_err(rid, code, &e.to_string());
                return;
            }
        };

        let bytes_processed = Arc::new(AtomicU64::new(0));
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let bp = bytes_processed.clone();
        let final_path = resolved_path.clone();
        let wakeup = self.wakeup.clone();
        std::thread::spawn(move || {
            worker::run_upload(file, final_path, bp, cmd_rx, evt_tx, wakeup);
        });

        let resolved_str = resolved_path.to_string_lossy().into_owned();
        self.transfers.insert(
            b.transfer_id.clone(),
            TransferHandle {
                direction: Direction::Upload,
                bytes_received: 0,
                bytes_processed,
                total_bytes: b.total_bytes,
                resolved_path,
                cmd_tx,
                evt_rx,
                closing: false,
                upload_mode: b.mode,
                upload_mtime: b.mtime,
                open_after_finalize: deferred,
            },
        );
        self.push_ready(rid, RSP_OK, ok_begin_upload_body(&resolved_str));
    }

    fn handle_upload_chunk(&mut self, rid: u32, b: UploadChunkBody) {
        let h = match self.transfers.get_mut(&b.transfer_id) {
            Some(h) if h.direction == Direction::Upload && !h.closing => h,
            _ => {
                self.push_err(rid, ERR_UNKNOWN_TRANSFER, "no such upload");
                return;
            }
        };
        if b.data.len() as u32 > self.limits.max_chunk_bytes {
            self.push_err(rid, ERR_CHUNK_TOO_LARGE, "chunk exceeds cap");
            return;
        }
        if b.offset != h.bytes_received {
            self.push_err(rid, ERR_CHUNK_OFFSET, "offset mismatch");
            return;
        }
        if h.total_bytes != 0 && b.offset + b.data.len() as u64 > h.total_bytes {
            self.push_err(rid, ERR_TOO_MANY_BYTES, "chunk overruns total_bytes");
            return;
        }
        let chunk_len = b.data.len() as u64;
        let cmd = WorkerCmd::Write { data: b.data };
        if h.cmd_tx.send(cmd).is_err() {
            self.push_err(rid, ERR_INTERNAL, "worker gone");
            return;
        }
        h.bytes_received += chunk_len;
        self.push_ok(rid);
    }

    fn handle_end_upload(&mut self, rid: u32, b: EndUploadBody) {
        let h = match self.transfers.get_mut(&b.transfer_id) {
            Some(h) if h.direction == Direction::Upload && !h.closing => h,
            _ => {
                self.push_err(rid, ERR_UNKNOWN_TRANSFER, "no such upload");
                return;
            }
        };
        if h.total_bytes != 0 && h.bytes_received != h.total_bytes {
            self.push_err(rid, ERR_PREMATURE_END, "bytes_received != total_bytes");
            return;
        }
        h.closing = true;
        let cmd = WorkerCmd::Finalize {
            request_id: rid,
            mode: h.upload_mode,
            mtime: h.upload_mtime,
            open_after: h.open_after_finalize,
        };
        if h.cmd_tx.send(cmd).is_err() {
            self.push_err(rid, ERR_INTERNAL, "worker gone");
            return;
        }
        // Worker will respond via WorkerEvt::Finalised (§drive); the
        // matching slot becomes Ready then.
        self.push_pending(rid);
    }

    fn handle_begin_download(&mut self, rid: u32, b: BeginDownloadBody) {
        if self.transfers.contains_key(&b.transfer_id) {
            self.push_err(rid, ERR_DUPLICATE_TRANSFER, "id in use");
            return;
        }
        if self.transfers.len() as u32 >= self.limits.max_concurrent_transfers {
            self.push_err(rid, ERR_TOO_MANY_TRANSFERS, "transfer budget exhausted");
            return;
        }
        if b.host_path.len() > self.limits.max_path_bytes as usize {
            self.push_err(rid, ERR_PATH_TOO_LONG, "host_path too long");
            return;
        }
        if b.host_path.is_empty() {
            // Deferred form (file picker) is deferred to a follow-up
            // PR; the spec lets the host return err_picker_unavailable
            // when it cannot satisfy the request (§7.1).
            self.push_err(rid, ERR_PICKER_UNAVAILABLE, "file picker not implemented");
            return;
        }
        let resolved_path = match path::resolve(&b.host_path) {
            Ok(p) => p,
            Err(e) => {
                self.push_err(rid, e.code, e.message);
                return;
            }
        };
        let file = match File::open(&resolved_path) {
            Ok(f) => f,
            Err(e) => {
                let code = match e.kind() {
                    std::io::ErrorKind::NotFound => ERR_PATH_MISSING,
                    std::io::ErrorKind::PermissionDenied => ERR_PATH_DENIED,
                    _ => ERR_IO,
                };
                self.push_err(rid, code, &e.to_string());
                return;
            }
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(e) => {
                self.push_err(rid, ERR_IO, &e.to_string());
                return;
            }
        };
        let total_bytes = meta.len();
        if self.limits.max_file_bytes != 0 && total_bytes > self.limits.max_file_bytes {
            self.push_err(rid, ERR_TOO_MANY_BYTES, "exceeds max_file_bytes");
            return;
        }
        let (mode, mtime) = file_meta_unix(&meta);

        let chunk_size = if b.chunk_size_hint == 0 {
            std::cmp::min(256 * 1024, self.limits.max_chunk_bytes)
        } else {
            std::cmp::min(b.chunk_size_hint, self.limits.max_chunk_bytes)
        };

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let wakeup = self.wakeup.clone();
        std::thread::spawn(move || {
            worker::run_download(file, chunk_size, cmd_rx, evt_tx, wakeup);
        });

        let resolved_str = resolved_path.to_string_lossy().into_owned();
        self.transfers.insert(
            b.transfer_id.clone(),
            TransferHandle {
                direction: Direction::Download,
                bytes_received: 0,
                bytes_processed: Arc::new(AtomicU64::new(0)),
                total_bytes,
                resolved_path,
                cmd_tx,
                evt_rx,
                closing: false,
                upload_mode: 0,
                upload_mtime: 0,
                open_after_finalize: false,
            },
        );
        self.push_ready(
            rid,
            RSP_OK,
            ok_begin_download_body(&resolved_str, total_bytes, mode, mtime),
        );
    }

    fn handle_report_download_ack(&mut self, rid: u32, b: ReportDownloadAckBody) {
        let h = match self.transfers.get(&b.transfer_id) {
            Some(h) if h.direction == Direction::Download => h,
            _ => {
                self.push_err(rid, ERR_UNKNOWN_TRANSFER, "no such download");
                return;
            }
        };
        let _ = h; // Currently informational only — no host-side UI yet.
        self.push_ok(rid);
    }

    fn handle_request_ack(&mut self, rid: u32, b: RequestAckBody) {
        let (bytes_received, bytes_processed) = match self.transfers.get(&b.transfer_id) {
            Some(h) if h.direction == Direction::Upload => {
                (h.bytes_received, h.bytes_processed.load(Ordering::Relaxed))
            }
            _ => {
                self.push_err(rid, ERR_UNKNOWN_TRANSFER, "no such upload");
                return;
            }
        };
        self.push_ok(rid);
        self.emit_event(
            EVT_UPLOAD_ACK,
            upload_ack_body(&b.transfer_id, bytes_received, bytes_processed),
        );
    }

    fn handle_cancel_transfer(&mut self, rid: u32, b: CancelTransferBody) {
        let Some(h) = self.transfers.get(&b.transfer_id) else {
            self.push_err(rid, ERR_UNKNOWN_TRANSFER, "no such transfer");
            return;
        };
        let _ = h.cmd_tx.send(WorkerCmd::Cancel);
        self.push_ok(rid);
        // The worker will emit WorkerEvt::Aborted, which `drive()`
        // converts into a TransferAborted event and reaps the
        // transfer.
    }

    // -------- worker draining ------------------------------------------

    fn drain_worker(&mut self, id: &str) {
        let events: Vec<WorkerEvt> = {
            let Some(h) = self.transfers.get(id) else {
                return;
            };
            let mut buf = Vec::new();
            while let Ok(evt) = h.evt_rx.try_recv() {
                buf.push(evt);
            }
            buf
        };
        for evt in events {
            self.handle_worker_event(id, evt);
        }
    }

    fn handle_worker_event(&mut self, id: &str, evt: WorkerEvt) {
        match evt {
            WorkerEvt::DownloadChunk { offset, data } => {
                self.emit_event(EVT_DOWNLOAD_CHUNK, download_chunk_body(id, offset, &data));
                if let Some(h) = self.transfers.get_mut(id) {
                    h.bytes_received = offset + data.len() as u64;
                }
            }
            WorkerEvt::DownloadEnd { bytes_sent } => {
                self.emit_event(EVT_DOWNLOAD_END, download_end_body(id, bytes_sent));
                self.transfers.remove(id);
            }
            WorkerEvt::Finalised {
                request_id,
                final_path,
                bytes_written,
            } => {
                let final_path_str = final_path.to_string_lossy().into_owned();
                let body = ok_end_upload_body(&final_path_str, bytes_written);
                self.fill_slot(request_id, RSP_OK, body);
                self.transfers.remove(id);
            }
            WorkerEvt::Aborted {
                reason,
                message,
                pending_request_id,
            } => {
                self.emit_event(
                    EVT_TRANSFER_ABORTED,
                    transfer_aborted_body(id, reason, &message),
                );
                if let Some(rid) = pending_request_id {
                    let code = match reason {
                        ABORT_DISK_FULL => ERR_DISK_FULL,
                        ABORT_IO_ERROR => ERR_IO,
                        _ => ERR_INTERNAL,
                    };
                    self.fill_slot(rid, RSP_ERR, err_body(code, &message));
                }
                self.transfers.remove(id);
            }
        }
    }

    // -------- response queue helpers -----------------------------------

    fn push_ready(&mut self, rid: u32, frame_type: u8, body: Vec<u8>) {
        self.response_queue.push_back(ResponseSlot::Ready {
            frame_type,
            request_id: rid,
            body,
        });
        self.flush_ready_slots();
    }

    fn push_pending(&mut self, rid: u32) {
        self.response_queue
            .push_back(ResponseSlot::Pending { request_id: rid });
    }

    fn push_ok(&mut self, rid: u32) {
        self.push_ready(rid, RSP_OK, Vec::new());
    }

    fn push_err(&mut self, rid: u32, code: u16, msg: &str) {
        self.push_ready(rid, RSP_ERR, err_body(code, msg));
    }

    fn fill_slot(&mut self, rid: u32, frame_type: u8, body: Vec<u8>) {
        for slot in self.response_queue.iter_mut() {
            if let ResponseSlot::Pending { request_id } = slot {
                if *request_id == rid {
                    *slot = ResponseSlot::Ready {
                        frame_type,
                        request_id: rid,
                        body,
                    };
                    break;
                }
            }
        }
        self.flush_ready_slots();
    }

    fn flush_ready_slots(&mut self) {
        while matches!(self.response_queue.front(), Some(ResponseSlot::Ready { .. })) {
            if let Some(ResponseSlot::Ready {
                frame_type,
                request_id,
                body,
            }) = self.response_queue.pop_front()
            {
                append_frame(&mut self.ready_frames, frame_type, request_id, &body);
            }
        }
    }

    fn emit_event(&mut self, frame_type: u8, body: Vec<u8>) {
        // Events have request_id=0 (§4.2) and don't gate on the
        // response queue.
        append_frame(&mut self.ready_frames, frame_type, 0, &body);
    }
}

#[cfg(unix)]
fn file_meta_unix(meta: &std::fs::Metadata) -> (u32, i64) {
    use std::os::unix::fs::MetadataExt;
    let mode = meta.mode() & 0o7777;
    let mtime = meta.mtime();
    (mode, mtime)
}

#[cfg(not(unix))]
fn file_meta_unix(meta: &std::fs::Metadata) -> (u32, i64) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (0, mtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vft_protocol::apc::ApcStream;
    use vft_protocol::command::{
        BeginDownloadBody as DLBody, BeginUploadBody as ULBody, CancelTransferBody as CTBody,
        EndUploadBody as EUBody, ReportDownloadAckBody as RDABody, RequestAckBody as RABody,
        UploadChunkBody as UCBody,
    };
    use vft_protocol::encode::build_envelope;

    fn drain_response_envelope(engine: &mut VftEngine) -> Vec<Vec<u8>> {
        // Take the engine's pending response envelope and re-parse it
        // into a list of frame bodies (without their headers) for easy
        // assertions.
        let env = engine.take_responses();
        if env.is_empty() {
            return Vec::new();
        }
        let mut s = ApcStream::with_marker(*MARKER_H2C);
        let out = s.feed(&env);
        let mut frames = Vec::new();
        for payload in out.payloads {
            let mut r = Reader::new(&payload);
            let _v = r.u8().unwrap();
            let _len = r.u32().unwrap();
            while !r.at_end() {
                let frame_type = r.u8().unwrap();
                let request_id = r.u32().unwrap();
                let body_len = r.u32().unwrap() as usize;
                let body = r.take(body_len).unwrap().to_vec();
                frames.push((frame_type, request_id, body));
            }
        }
        // Strip the (frame_type, request_id) tuple so callers see just
        // the body Vec<u8>; assertions about types are done via
        // explicit_frames() helper below.
        frames.into_iter().map(|(_, _, b)| b).collect()
    }

    fn explicit_frames(engine: &mut VftEngine) -> Vec<(u8, u32, Vec<u8>)> {
        let env = engine.take_responses();
        if env.is_empty() {
            return Vec::new();
        }
        let mut s = ApcStream::with_marker(*MARKER_H2C);
        let out = s.feed(&env);
        let mut frames = Vec::new();
        for payload in out.payloads {
            let mut r = Reader::new(&payload);
            let _v = r.u8().unwrap();
            let _len = r.u32().unwrap();
            while !r.at_end() {
                let frame_type = r.u8().unwrap();
                let request_id = r.u32().unwrap();
                let body_len = r.u32().unwrap() as usize;
                let body = r.take(body_len).unwrap().to_vec();
                frames.push((frame_type, request_id, body));
            }
        }
        frames
    }

    fn feed(engine: &mut VftEngine, cmds: &[(Command, u32)]) {
        let env = build_envelope(cmds);
        let _ = engine.process_pty_chunk(&env);
    }

    /// Drive the engine until either the predicate succeeds or the
    /// deadline elapses. Used in tests that wait on async worker
    /// events (DownloadChunk, Finalised, …).
    fn drive_until<F: FnMut(&mut VftEngine) -> bool>(
        engine: &mut VftEngine,
        mut pred: F,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            engine.drive();
            if pred(engine) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn probe_returns_features_and_limits() {
        let mut e = VftEngine::new(|| {});
        feed(&mut e, &[(Command::Probe, 1)]);
        let frames = explicit_frames(&mut e);
        assert_eq!(frames.len(), 1);
        let (ft, rid, body) = &frames[0];
        assert_eq!(*ft, RSP_PROBE);
        assert_eq!(*rid, 1);
        let mut r = Reader::new(body);
        assert_eq!(r.u16().unwrap(), 1);
        assert_eq!(r.u32().unwrap(), 8); // max_concurrent_transfers
        let _ = r.u32().unwrap(); // max_chunk_bytes
        let _ = r.u32().unwrap(); // max_path_bytes
        let _ = r.u64().unwrap(); // max_file_bytes
        assert_eq!(r.u8().unwrap(), FEAT_UPLOAD | FEAT_DOWNLOAD);
    }

    #[test]
    fn upload_round_trip_writes_file() {
        let dir = std::env::temp_dir().join(format!("vft-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("upload.bin");
        let _ = std::fs::remove_file(&path);

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 11,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_OK);

        feed(
            &mut e,
            &[(
                Command::UploadChunk(UCBody {
                    transfer_id: "t".into(),
                    offset: 0,
                    data: b"hello world".to_vec(),
                }),
                2,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_OK);

        feed(
            &mut e,
            &[(
                Command::EndUpload(EUBody {
                    transfer_id: "t".into(),
                }),
                3,
            )],
        );
        // EndUpload is deferred — no frames yet.
        assert!(e.take_responses().is_empty());

        let ok = drive_until(
            &mut e,
            |eng| !eng.ready_frames.is_empty(),
            std::time::Duration::from_secs(3),
        );
        assert!(ok, "EndUpload did not finalise within timeout");

        let frames = explicit_frames(&mut e);
        let (ft, rid, body) = frames.into_iter().find(|(_, r, _)| *r == 3).unwrap();
        assert_eq!(ft, RSP_OK);
        assert_eq!(rid, 3);
        let mut r = Reader::new(&body);
        let final_path = r.string().unwrap().to_owned();
        assert_eq!(r.u64().unwrap(), 11);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"hello world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upload_offset_mismatch_is_err() {
        let path = std::env::temp_dir().join(format!("vft-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 10,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = explicit_frames(&mut e);
        feed(
            &mut e,
            &[(
                Command::UploadChunk(UCBody {
                    transfer_id: "t".into(),
                    offset: 5, // wrong: should be 0
                    data: b"abc".to_vec(),
                }),
                2,
            )],
        );
        let frames = explicit_frames(&mut e);
        let (ft, _rid, body) = &frames[0];
        assert_eq!(*ft, RSP_ERR);
        let mut r = Reader::new(body);
        assert_eq!(r.u16().unwrap(), ERR_CHUNK_OFFSET);
        // Cancel so the worker thread exits cleanly.
        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                3,
            )],
        );
        let _ = explicit_frames(&mut e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upload_premature_end_is_err() {
        let path = std::env::temp_dir().join(format!("vft-premature-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 10,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = explicit_frames(&mut e);
        // No chunks; EndUpload prematurely.
        feed(
            &mut e,
            &[(
                Command::EndUpload(EUBody {
                    transfer_id: "t".into(),
                }),
                2,
            )],
        );
        let frames = explicit_frames(&mut e);
        let (ft, _rid, body) = &frames[0];
        assert_eq!(*ft, RSP_ERR);
        let mut r = Reader::new(body);
        assert_eq!(r.u16().unwrap(), ERR_PREMATURE_END);

        // Cancel for clean teardown.
        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                3,
            )],
        );
        let _ = explicit_frames(&mut e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upload_overwrite_flag_replaces_existing() {
        let path = std::env::temp_dir().join(format!("vft-overwrite-{}", std::process::id()));
        std::fs::write(&path, b"existing").unwrap();

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 3,
                    flags: FLAG_OVERWRITE,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_OK);

        feed(
            &mut e,
            &[
                (
                    Command::UploadChunk(UCBody {
                        transfer_id: "t".into(),
                        offset: 0,
                        data: b"new".to_vec(),
                    }),
                    2,
                ),
                (
                    Command::EndUpload(EUBody {
                        transfer_id: "t".into(),
                    }),
                    3,
                ),
            ],
        );
        // First Ok (UploadChunk) is sync.
        let _ = explicit_frames(&mut e);

        let ok = drive_until(
            &mut e,
            |eng| eng.transfers.get("t").is_none(),
            std::time::Duration::from_secs(3),
        );
        assert!(ok);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upload_no_overwrite_errors_on_existing_file() {
        let path = std::env::temp_dir().join(format!("vft-no-ow-{}", std::process::id()));
        std::fs::write(&path, b"existing").unwrap();

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 3,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_ERR);
        let mut r = Reader::new(&frames[0].2);
        assert_eq!(r.u16().unwrap(), ERR_PATH_EXISTS);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn download_round_trip_emits_chunks_and_end() {
        let path = std::env::temp_dir().join(format!("vft-dl-{}", std::process::id()));
        std::fs::write(&path, b"hello world").unwrap();

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginDownload(DLBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    chunk_size_hint: 4,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_OK);
        let mut r = Reader::new(&frames[0].2);
        let _resolved = r.string().unwrap();
        assert_eq!(r.u64().unwrap(), 11);

        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut ended = false;
        let ok = drive_until(
            &mut e,
            |eng| {
                for (ft, _rid, body) in explicit_frames(eng) {
                    let mut r = Reader::new(&body);
                    match ft {
                        EVT_DOWNLOAD_CHUNK => {
                            let _ = r.string().unwrap();
                            let _ = r.u64().unwrap();
                            let data = r.bytes().unwrap().to_vec();
                            chunks.push(data);
                        }
                        EVT_DOWNLOAD_END => {
                            let _ = r.string().unwrap();
                            let _ = r.u64().unwrap();
                            ended = true;
                        }
                        _ => {}
                    }
                }
                ended
            },
            std::time::Duration::from_secs(3),
        );
        assert!(ok, "no DownloadEnd within timeout");
        let joined: Vec<u8> = chunks.into_iter().flatten().collect();
        assert_eq!(joined, b"hello world");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn begin_download_picker_unavailable_for_empty_path() {
        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginDownload(DLBody {
                    transfer_id: "t".into(),
                    host_path: "".into(),
                    chunk_size_hint: 0,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_ERR);
        let mut r = Reader::new(&frames[0].2);
        assert_eq!(r.u16().unwrap(), ERR_PICKER_UNAVAILABLE);
    }

    #[test]
    fn request_ack_returns_byte_counters_for_upload() {
        let path = std::env::temp_dir().join(format!("vft-ack-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 100,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = explicit_frames(&mut e);
        feed(
            &mut e,
            &[(
                Command::UploadChunk(UCBody {
                    transfer_id: "t".into(),
                    offset: 0,
                    data: vec![0u8; 50],
                }),
                2,
            )],
        );
        let _ = explicit_frames(&mut e);

        // Wait for the worker to process the write so bytes_processed
        // is up to date.
        let ok = drive_until(
            &mut e,
            |eng| {
                let h = eng.transfers.get("t").unwrap();
                h.bytes_processed.load(Ordering::Relaxed) == 50
            },
            std::time::Duration::from_secs(3),
        );
        assert!(ok);

        feed(
            &mut e,
            &[(
                Command::RequestAck(RABody {
                    transfer_id: "t".into(),
                }),
                3,
            )],
        );
        let frames = explicit_frames(&mut e);
        // Two frames: Ok(rid=3) + UploadAck event.
        let (_ft, _rid, ack_body) = frames
            .iter()
            .find(|(ft, _, _)| *ft == EVT_UPLOAD_ACK)
            .unwrap();
        let mut r = Reader::new(ack_body);
        assert_eq!(r.string().unwrap(), "t");
        assert_eq!(r.u64().unwrap(), 50); // bytes_received
        assert_eq!(r.u64().unwrap(), 50); // bytes_processed

        // Cancel for clean teardown.
        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                4,
            )],
        );
        let _ = explicit_frames(&mut e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cancel_emits_aborted_event() {
        let path = std::env::temp_dir().join(format!("vft-cancel-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 100,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = drain_response_envelope(&mut e);

        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                2,
            )],
        );
        let _ = explicit_frames(&mut e); // takes the Ok

        let ok = drive_until(
            &mut e,
            |eng| eng.transfers.get("t").is_none(),
            std::time::Duration::from_secs(3),
        );
        assert!(ok);
        let frames = explicit_frames(&mut e);
        assert!(
            frames.iter().any(|(ft, _, _)| *ft == EVT_TRANSFER_ABORTED),
            "expected TransferAborted event"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn duplicate_transfer_id_is_err() {
        let path = std::env::temp_dir().join(format!("vft-dup-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 0,
                    flags: 0,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = explicit_frames(&mut e);
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 0,
                    flags: FLAG_OVERWRITE,
                    mode: 0,
                    mtime: 0,
                }),
                2,
            )],
        );
        let frames = explicit_frames(&mut e);
        let (ft, _rid, body) = &frames[0];
        assert_eq!(*ft, RSP_ERR);
        let mut r = Reader::new(body);
        assert_eq!(r.u16().unwrap(), ERR_DUPLICATE_TRANSFER);

        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                3,
            )],
        );
        let _ = drain_response_envelope(&mut e);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn report_download_ack_unknown_transfer_is_err() {
        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::ReportDownloadAck(RDABody {
                    transfer_id: "ghost".into(),
                    bytes_confirmed: 0,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_ERR);
        let mut r = Reader::new(&frames[0].2);
        assert_eq!(r.u16().unwrap(), ERR_UNKNOWN_TRANSFER);
    }

    #[test]
    fn deferred_form_upload_writes_to_tmp() {
        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: "".into(),
                    basename: "deferred-test.bin".into(),
                    total_bytes: 4,
                    flags: FLAG_OVERWRITE,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let frames = explicit_frames(&mut e);
        assert_eq!(frames[0].0, RSP_OK);
        let mut r = Reader::new(&frames[0].2);
        let resolved = r.string().unwrap().to_owned();
        assert!(resolved.ends_with("deferred-test.bin"));

        feed(
            &mut e,
            &[
                (
                    Command::UploadChunk(UCBody {
                        transfer_id: "t".into(),
                        offset: 0,
                        data: b"abcd".to_vec(),
                    }),
                    2,
                ),
                (
                    Command::EndUpload(EUBody {
                        transfer_id: "t".into(),
                    }),
                    3,
                ),
            ],
        );
        let _ = explicit_frames(&mut e);

        let ok = drive_until(
            &mut e,
            |eng| eng.transfers.get("t").is_none(),
            std::time::Duration::from_secs(3),
        );
        assert!(ok);
        assert_eq!(std::fs::read(&resolved).unwrap(), b"abcd");
        let _ = std::fs::remove_file(&resolved);
    }

    #[test]
    fn deferred_form_upload_sets_open_after_finalize_flag() {
        // The actual `opener::open` call is suppressed under cfg(test),
        // so this test just inspects the engine's bookkeeping to
        // confirm the deferred form opts in.
        let path = std::env::temp_dir().join(format!("vft-open-flag-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: "".into(),
                    basename: "open-flag.bin".into(),
                    total_bytes: 0,
                    flags: FLAG_OVERWRITE,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = explicit_frames(&mut e);
        assert!(
            e.transfers.get("t").unwrap().open_after_finalize,
            "deferred-form uploads should request open-after-finalize"
        );

        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                2,
            )],
        );
        let _ = explicit_frames(&mut e);
    }

    #[test]
    fn explicit_form_upload_does_not_set_open_after_finalize_flag() {
        let path = std::env::temp_dir()
            .join(format!("vft-no-open-flag-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut e = VftEngine::new(|| {});
        feed(
            &mut e,
            &[(
                Command::BeginUpload(ULBody {
                    transfer_id: "t".into(),
                    host_path: path.to_string_lossy().into_owned(),
                    basename: "".into(),
                    total_bytes: 0,
                    flags: FLAG_OVERWRITE,
                    mode: 0,
                    mtime: 0,
                }),
                1,
            )],
        );
        let _ = explicit_frames(&mut e);
        assert!(
            !e.transfers.get("t").unwrap().open_after_finalize,
            "explicit-form uploads should NOT request open-after-finalize"
        );

        feed(
            &mut e,
            &[(
                Command::CancelTransfer(CTBody {
                    transfer_id: "t".into(),
                }),
                2,
            )],
        );
        let _ = explicit_frames(&mut e);
        let _ = std::fs::remove_file(&path);
    }
}
