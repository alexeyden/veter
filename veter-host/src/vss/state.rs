// VSS host engine: an APC envelope extractor + fragment reassembler
// that sits in the per-portal (and host-level) byte pipeline. On a
// fully assembled snapshot the engine validates the version and
// hands the three sub-snapshot byte blobs to the caller via
// `take_completed_snapshots`; the caller (`veter`'s
// `App::process_pty_output` or `prt::WritePortal` handler) applies
// them to the owning context's vt100 / VGE / children-PRT engines
// using their `restore_from_binary_snapshot` methods.
//
// The engine itself does not own any of those receiving engines —
// it is intentionally render-context-agnostic so the same code
// works at the host level and inside every nested portal.

use std::collections::BTreeMap;

use vss_protocol::{
    apc::ApcStream,
    encode_rejected,
    envelope::for_each_frame,
    frame::{
        FRM_SNAPSHOT_ACCEPTED, FRM_SNAPSHOT_BEGIN, FRM_SNAPSHOT_END, FRM_SNAPSHOT_REJECTED,
        FRM_VGE_FRAGMENT, FRM_VT_FRAGMENT, FRM_PRT_FRAGMENT, PROTOCOL_VERSION,
        REJECT_MALFORMED, REJECT_VERSION_MISMATCH, SNAPSHOT_VERSION,
    },
    frames::DownstreamFrame,
};

/// Renderer-side reject reasons. Returned alongside the rejected
/// `sequence_id` for callers that want to log or surface diagnostics.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RejectReason {
    VersionMismatch,
    Malformed,
}

/// A fully reassembled and version-validated snapshot, ready for the
/// caller to apply to the owning context's engines.
#[derive(Debug, Clone)]
pub struct CompletedSnapshot {
    pub sequence_id: u32,
    pub rows: u16,
    pub cols: u16,
    /// Bytes passed verbatim to `vt100::Screen::restore_from_binary_snapshot`.
    pub vt_bytes: Vec<u8>,
    /// Bytes passed verbatim to `VgeEngine::restore_from_binary_snapshot`.
    pub vge_bytes: Vec<u8>,
    /// Bytes passed verbatim to `PrtEngine::restore_from_binary_snapshot`.
    pub prt_bytes: Vec<u8>,
}

#[derive(Default)]
struct SnapshotBuilder {
    // Kept for debugging / future cross-checks; the value is
    // validated against `SNAPSHOT_VERSION` at `SnapshotBegin` time.
    #[allow(dead_code)]
    snapshot_version: u32,
    sequence_id: u32,
    rows: u16,
    cols: u16,
    vt_total: Option<u64>,
    vt_chunks: BTreeMap<u64, Vec<u8>>,
    vge_total: Option<u64>,
    vge_chunks: BTreeMap<u64, Vec<u8>>,
    prt_total: Option<u64>,
    prt_chunks: BTreeMap<u64, Vec<u8>>,
}

impl SnapshotBuilder {
    fn add_fragment(
        chunks: &mut BTreeMap<u64, Vec<u8>>,
        total_slot: &mut Option<u64>,
        index: u64,
        total: u64,
        payload: Vec<u8>,
    ) -> Result<(), ()> {
        if total == 0 || index >= total {
            return Err(());
        }
        if let Some(prev) = total_slot {
            if *prev != total {
                return Err(());
            }
        } else {
            *total_slot = Some(total);
        }
        // Reject duplicate indices; we don't try to merge partials.
        if chunks.insert(index, payload).is_some() {
            return Err(());
        }
        Ok(())
    }

    /// Are all three sub-snapshots fully assembled?
    fn is_complete(&self) -> bool {
        let one_ok = |total: Option<u64>, chunks: &BTreeMap<u64, Vec<u8>>| {
            if let Some(t) = total {
                chunks.len() as u64 == t
            } else {
                false
            }
        };
        one_ok(self.vt_total, &self.vt_chunks)
            && one_ok(self.vge_total, &self.vge_chunks)
            && one_ok(self.prt_total, &self.prt_chunks)
    }

    fn finish(self) -> CompletedSnapshot {
        fn flatten(chunks: BTreeMap<u64, Vec<u8>>) -> Vec<u8> {
            let mut out = Vec::new();
            for (_idx, c) in chunks {
                out.extend_from_slice(&c);
            }
            out
        }
        CompletedSnapshot {
            sequence_id: self.sequence_id,
            rows: self.rows,
            cols: self.cols,
            vt_bytes: flatten(self.vt_chunks),
            vge_bytes: flatten(self.vge_chunks),
            prt_bytes: flatten(self.prt_chunks),
        }
    }
}

/// Per-context VSS engine. One instance lives at the host level and
/// one per portal — same shape, same lifecycle.
pub struct VssEngine {
    apc: ApcStream,
    /// Upstream `SnapshotAccepted` / `SnapshotRejected` envelopes
    /// produced in response to completed snapshots. The caller drains
    /// this with `take_responses` and writes the bytes upstream — for
    /// per-portal engines that's the portal's `EVT_RAW_REPLY` path;
    /// for the host engine it's the PTY master.
    pending_response_bytes: Vec<u8>,
    /// In-flight snapshot being assembled.  `None` between attaches.
    builder: Option<SnapshotBuilder>,
    /// Successfully reassembled snapshots awaiting application by the
    /// caller. Drained via `take_completed_snapshots`.
    completed: Vec<CompletedSnapshot>,
    /// Rejected snapshots — exposed for caller-side logging. Drained
    /// via `take_rejected`. Independent of `pending_response_bytes`,
    /// which carries the on-wire response.
    rejected: Vec<(u32, RejectReason)>,
    /// Count of `DetachNotify` frames observed since the last drain.
    /// The caller uses this to know when to restore pre-attach state.
    /// Drained via `take_detach_signals`.
    detach_signals: usize,
}

impl Default for VssEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl VssEngine {
    pub fn new() -> Self {
        Self {
            apc: ApcStream::new(),
            pending_response_bytes: Vec::new(),
            builder: None,
            completed: Vec::new(),
            rejected: Vec::new(),
            detach_signals: 0,
        }
    }

    /// Feed raw PTY bytes through the VSS layer. Returns whatever
    /// bytes did not belong to a VSS envelope so the caller can
    /// forward them to the next layer (vt100 at the host level, or
    /// the inner program's per-portal vt100).
    pub fn process_pty_chunk(&mut self, input: &[u8]) -> Vec<u8> {
        let out = self.apc.feed(input);
        for payload in out.payloads {
            if let Err(()) = self.handle_payload(&payload) {
                // Bubble malformed envelopes as a Reject when we know
                // the sequence_id; otherwise silently swallow.
                if let Some(seq) = self.builder.as_ref().map(|b| b.sequence_id) {
                    self.reject(seq, REJECT_MALFORMED);
                }
                self.builder = None;
            }
        }
        out.passthrough
    }

    /// Drain pending upstream response bytes (one or more
    /// `SnapshotAccepted` / `SnapshotRejected` envelopes).
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response_bytes)
    }

    /// Drain reassembled snapshots ready for the caller to apply.
    pub fn take_completed_snapshots(&mut self) -> Vec<CompletedSnapshot> {
        std::mem::take(&mut self.completed)
    }

    /// Drain log entries for rejected snapshots `(sequence_id, reason)`.
    /// The wire response was already queued via `take_responses`.
    pub fn take_rejected(&mut self) -> Vec<(u32, RejectReason)> {
        std::mem::take(&mut self.rejected)
    }

    /// Drain the count of `DetachNotify` frames observed since the
    /// last call. The caller restores pre-attach engine state when
    /// this is non-zero. Coalesced — multiple notifies in one batch
    /// collapse to a single restore at the caller.
    pub fn take_detach_signals(&mut self) -> usize {
        std::mem::take(&mut self.detach_signals)
    }

    fn handle_payload(&mut self, payload: &[u8]) -> Result<(), ()> {
        let mut result: Result<(), ()> = Ok(());
        for_each_frame(payload, |frame_type, _request_id, body| {
            if result.is_err() {
                return Ok::<(), u16>(());
            }
            let frame = match DownstreamFrame::parse(frame_type, body) {
                Ok(f) => f,
                Err(_) => {
                    result = Err(());
                    return Ok(());
                }
            };
            match frame {
                DownstreamFrame::SnapshotBegin {
                    snapshot_version,
                    rows,
                    cols,
                    sequence_id,
                } => {
                    if snapshot_version != SNAPSHOT_VERSION {
                        self.reject(sequence_id, REJECT_VERSION_MISMATCH);
                        self.builder = None;
                    } else {
                        self.builder = Some(SnapshotBuilder {
                            snapshot_version,
                            sequence_id,
                            rows,
                            cols,
                            ..SnapshotBuilder::default()
                        });
                    }
                }
                DownstreamFrame::VtFragment { index, total, payload }
                | DownstreamFrame::VgeFragment { index, total, payload }
                | DownstreamFrame::PrtFragment { index, total, payload } => {
                    // Re-decode the frame_type to dispatch to the
                    // right slot (we already matched the structure
                    // into one branch above; pattern is the same on
                    // all three).
                    if let Some(b) = self.builder.as_mut() {
                        let (chunks, total_slot) = match frame_type {
                            FRM_VT_FRAGMENT => (&mut b.vt_chunks, &mut b.vt_total),
                            FRM_VGE_FRAGMENT => (&mut b.vge_chunks, &mut b.vge_total),
                            FRM_PRT_FRAGMENT => (&mut b.prt_chunks, &mut b.prt_total),
                            _ => unreachable!(),
                        };
                        if SnapshotBuilder::add_fragment(
                            chunks, total_slot, index, total, payload,
                        )
                        .is_err()
                        {
                            // Malformed fragment — drop the builder
                            // and emit a Reject keyed by its seq.
                            let seq = b.sequence_id;
                            self.builder = None;
                            self.reject(seq, REJECT_MALFORMED);
                        }
                    }
                    // Fragment outside a Begin/End window: silently
                    // ignored, matching the PRT/VGE permissive style.
                }
                DownstreamFrame::SnapshotEnd { sequence_id } => {
                    if let Some(b) = self.builder.as_ref() {
                        if b.sequence_id == sequence_id && b.is_complete() {
                            let b = self.builder.take().unwrap();
                            let cs = b.finish();
                            self.accept(cs.sequence_id);
                            self.completed.push(cs);
                        } else {
                            // Incomplete or mismatched End — reject.
                            self.builder = None;
                            self.reject(sequence_id, REJECT_MALFORMED);
                        }
                    } else {
                        // End with no Begin in flight: ignore. The
                        // engine should not synthesise a reject for a
                        // snapshot it never started parsing.
                    }
                }
                DownstreamFrame::DetachNotify => {
                    // Bump the counter for the caller to observe.
                    // No response is sent — the engine has already
                    // torn down the connection by the time we'd want
                    // to reply.
                    self.detach_signals += 1;
                }
            }
            Ok::<(), u16>(())
        })
        .map_err(|_| ())?;
        result
    }

    fn accept(&mut self, sequence_id: u32) {
        let env = vss_protocol::encode_accepted(sequence_id);
        self.pending_response_bytes.extend_from_slice(&env);
    }

    fn reject(&mut self, sequence_id: u32, reason: u8) {
        let env = encode_rejected(sequence_id, reason);
        self.pending_response_bytes.extend_from_slice(&env);
        let mapped = match reason {
            REJECT_VERSION_MISMATCH => RejectReason::VersionMismatch,
            _ => RejectReason::Malformed,
        };
        self.rejected.push((sequence_id, mapped));
    }
}

// Silence the unused-import warning on `PROTOCOL_VERSION` — kept in
// the imports for future use when the wire bumps the envelope
// protocol_version field.
const _: u8 = PROTOCOL_VERSION;
const _: u8 = FRM_SNAPSHOT_ACCEPTED;
const _: u8 = FRM_SNAPSHOT_END;
const _: u8 = FRM_SNAPSHOT_BEGIN;
const _: u8 = FRM_SNAPSHOT_REJECTED;

#[cfg(test)]
mod tests {
    use super::*;
    use vss_protocol::{encode_snapshot, frame::DEFAULT_MAX_FRAGMENT_BYTES};

    #[test]
    fn complete_snapshot_round_trips() {
        let env = encode_snapshot(
            SNAPSHOT_VERSION,
            10,
            20,
            42,
            b"vt-bytes",
            b"vge-bytes",
            b"prt-bytes",
            DEFAULT_MAX_FRAGMENT_BYTES,
        );
        let mut e = VssEngine::new();
        let passthrough = e.process_pty_chunk(&env);
        assert!(passthrough.is_empty(), "envelope must not leak to vt100");

        let completed = e.take_completed_snapshots();
        assert_eq!(completed.len(), 1);
        let c = &completed[0];
        assert_eq!(c.sequence_id, 42);
        assert_eq!(c.rows, 10);
        assert_eq!(c.cols, 20);
        assert_eq!(c.vt_bytes, b"vt-bytes");
        assert_eq!(c.vge_bytes, b"vge-bytes");
        assert_eq!(c.prt_bytes, b"prt-bytes");

        // Upstream Accepted envelope queued.
        let responses = e.take_responses();
        assert!(!responses.is_empty());
        let expected = vss_protocol::encode_accepted(42);
        assert_eq!(responses, expected);
    }

    #[test]
    fn version_mismatch_rejects_and_skips() {
        let env = encode_snapshot(
            SNAPSHOT_VERSION + 1, // wrong version
            1,
            1,
            99,
            b"vt",
            b"vge",
            b"prt",
            DEFAULT_MAX_FRAGMENT_BYTES,
        );
        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&env);
        assert!(e.take_completed_snapshots().is_empty());

        let rejects = e.take_rejected();
        assert_eq!(rejects, vec![(99, RejectReason::VersionMismatch)]);

        let responses = e.take_responses();
        let expected = vss_protocol::encode_rejected(99, REJECT_VERSION_MISMATCH);
        assert_eq!(responses, expected);
    }

    #[test]
    fn fragmented_payload_reassembles() {
        // Force fragmentation with a tiny chunk size.
        let vt = vec![0xAA; 64];
        let vge = vec![0xBB; 33];
        let prt = vec![0xCC; 17];
        let env = encode_snapshot(SNAPSHOT_VERSION, 1, 1, 7, &vt, &vge, &prt, 8);

        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&env);
        let completed = e.take_completed_snapshots();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].vt_bytes, vt);
        assert_eq!(completed[0].vge_bytes, vge);
        assert_eq!(completed[0].prt_bytes, prt);
    }

    #[test]
    fn split_envelope_across_pty_chunks() {
        let env = encode_snapshot(
            SNAPSHOT_VERSION,
            1,
            1,
            3,
            b"vt",
            b"vge",
            b"prt",
            DEFAULT_MAX_FRAGMENT_BYTES,
        );
        let mid = env.len() / 2;
        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&env[..mid]);
        let _ = e.process_pty_chunk(&env[mid..]);
        let completed = e.take_completed_snapshots();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].sequence_id, 3);
    }

    #[test]
    fn non_vss_bytes_pass_through() {
        let mut e = VssEngine::new();
        let passthrough = e.process_pty_chunk(b"hello\x1b[2Jworld");
        assert_eq!(passthrough, b"hello\x1b[2Jworld");
        assert!(e.take_completed_snapshots().is_empty());
    }

    #[test]
    fn foreign_apc_envelopes_pass_through() {
        // PRT / VGE / VFT envelopes must not be consumed.
        let prt_envelope = b"\x1b_PRTabc\x1b\\";
        let mut e = VssEngine::new();
        let passthrough = e.process_pty_chunk(prt_envelope);
        assert_eq!(passthrough, prt_envelope);
    }

    #[test]
    fn end_without_begin_silently_ignored() {
        // Build a payload with just a SnapshotEnd frame, no Begin.
        let mut frames = Vec::new();
        vss_protocol::envelope::append_downstream(
            &mut frames,
            &DownstreamFrame::SnapshotEnd { sequence_id: 5 },
        );
        let env = vss_protocol::envelope::wrap_e2r_envelope(&frames);
        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&env);
        assert!(e.take_completed_snapshots().is_empty());
        assert!(e.take_rejected().is_empty());
        assert!(e.take_responses().is_empty());
    }

    #[test]
    fn duplicate_fragment_index_rejects() {
        // Manually build an envelope with two VtFragment frames at index=0.
        let mut frames = Vec::new();
        vss_protocol::envelope::append_downstream(
            &mut frames,
            &DownstreamFrame::SnapshotBegin {
                snapshot_version: SNAPSHOT_VERSION,
                rows: 1,
                cols: 1,
                sequence_id: 11,
            },
        );
        vss_protocol::envelope::append_downstream(
            &mut frames,
            &DownstreamFrame::VtFragment {
                index: 0,
                total: 2,
                payload: vec![0xAA],
            },
        );
        vss_protocol::envelope::append_downstream(
            &mut frames,
            &DownstreamFrame::VtFragment {
                index: 0, // duplicate
                total: 2,
                payload: vec![0xBB],
            },
        );
        let env = vss_protocol::envelope::wrap_e2r_envelope(&frames);
        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&env);
        // No completed snapshot, builder cleared, reject queued.
        assert!(e.take_completed_snapshots().is_empty());
        let rejects = e.take_rejected();
        assert_eq!(rejects, vec![(11, RejectReason::Malformed)]);
    }

    #[test]
    fn detach_notify_bumps_counter_no_response() {
        let env = vss_protocol::encode_detach_notify();
        let mut e = VssEngine::new();
        let passthrough = e.process_pty_chunk(&env);
        assert!(passthrough.is_empty());
        assert_eq!(e.take_detach_signals(), 1);
        // Drained — subsequent reads return 0 until next notify.
        assert_eq!(e.take_detach_signals(), 0);
        // No upstream response queued.
        assert!(e.take_responses().is_empty());
    }

    #[test]
    fn back_to_back_detach_notifies_coalesce_count() {
        let mut env = vss_protocol::encode_detach_notify();
        env.extend(vss_protocol::encode_detach_notify());
        env.extend(vss_protocol::encode_detach_notify());
        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&env);
        assert_eq!(e.take_detach_signals(), 3);
    }

    #[test]
    fn back_to_back_snapshots_round_trip() {
        let env1 = encode_snapshot(SNAPSHOT_VERSION, 1, 1, 100, b"a", b"b", b"c", 256);
        let env2 = encode_snapshot(SNAPSHOT_VERSION, 1, 1, 101, b"x", b"y", b"z", 256);
        let mut joined = env1.clone();
        joined.extend_from_slice(&env2);
        let mut e = VssEngine::new();
        let _ = e.process_pty_chunk(&joined);
        let c = e.take_completed_snapshots();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].sequence_id, 100);
        assert_eq!(c[1].sequence_id, 101);
        // Two Accept envelopes back-to-back.
        let resp = e.take_responses();
        let mut expect = vss_protocol::encode_accepted(100);
        expect.extend(vss_protocol::encode_accepted(101));
        assert_eq!(resp, expect);
    }
}
