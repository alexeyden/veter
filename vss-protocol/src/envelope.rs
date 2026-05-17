// Envelope wrapping (§4.1) and the high-level `encode_snapshot`
// helper used by veterd. Mirrors the shape of vft-protocol/envelope.rs.

use super::codec::stuff;
use super::frame::*;
use super::frames::{DownstreamFrame, UpstreamFrame};

/// Append a single frame to an unstuffed payload buffer.
/// Frame layout (§1.2): u8 frame_type, u32 request_id, u32 body_length,
/// body[body_length].
///
/// `request_id` is unused for VSS (there's no per-frame request/response
/// correlation — `sequence_id` in `SnapshotBegin/End` serves that role)
/// and should always be `0`. The field is kept in the wire layout for
/// parser commonality with PRT/VGE/VFT.
pub fn append_frame(buf: &mut Vec<u8>, frame_type: u8, request_id: u32, body: &[u8]) {
    buf.push(frame_type);
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

fn wrap(frames_buf: &[u8], marker: &[u8; 3]) -> Vec<u8> {
    // §1.2 unstuffed payload = u8 protocol_version, u32 payload_length,
    // frames. `payload_length` is "length of the rest" — just the frames
    // region.
    let mut unstuffed = Vec::with_capacity(5 + frames_buf.len());
    unstuffed.push(PROTOCOL_VERSION);
    unstuffed.extend_from_slice(&(frames_buf.len() as u32).to_le_bytes());
    unstuffed.extend_from_slice(frames_buf);

    let mut env = Vec::with_capacity(7 + unstuffed.len());
    env.push(ESC);
    env.push(APC_OPEN);
    env.extend_from_slice(marker);
    stuff(&unstuffed, &mut env);
    env.push(ESC);
    env.push(ST_CLOSE);
    env
}

/// Wrap a frame buffer as an engine→renderer envelope (uppercase `VSS`).
pub fn wrap_e2r_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_E2R)
}

/// Wrap a frame buffer as a renderer→engine envelope (lowercase `vss`).
pub fn wrap_r2e_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_R2E)
}

/// Append a typed downstream frame to a frames buffer with
/// `request_id = 0` (the convention for VSS).
pub fn append_downstream(buf: &mut Vec<u8>, frame: &DownstreamFrame) {
    let body = frame.encode_body();
    append_frame(buf, frame.frame_type(), 0, &body);
}

/// Append a typed upstream frame to a frames buffer with
/// `request_id = 0` (the convention for VSS).
pub fn append_upstream(buf: &mut Vec<u8>, frame: &UpstreamFrame) {
    let body = frame.encode_body();
    append_frame(buf, frame.frame_type(), 0, &body);
}

/// Encode a full snapshot as one envelope (or a few, if any sub-snapshot
/// exceeds `max_fragment_bytes`).
///
/// Wire order inside the envelope(s):
/// 1. `SnapshotBegin`
/// 2. `VtFragment*` (one per chunk of `vt_bytes`)
/// 3. `VgeFragment*` (one per chunk of `vge_bytes`)
/// 4. `PrtFragment*` (one per chunk of `prt_bytes`)
/// 5. `SnapshotEnd`
///
/// All frames go in a single envelope when they fit; otherwise the
/// caller can post-process the returned bytes (they're already a
/// complete envelope, so chunked transports are free to split them).
/// For multi-envelope output we'd extend this signature; for v0 a
/// single envelope is enough — `max_fragment_bytes` caps each
/// individual fragment payload but not the envelope total. APC
/// envelopes have no protocol-level size limit; the soft limits in
/// PRT (`max_write_bytes`) and equivalent caps are enforced by the
/// host receiving the bytes, not by the wire format.
pub fn encode_snapshot(
    snapshot_version: u32,
    rows: u16,
    cols: u16,
    sequence_id: u32,
    vt_bytes: &[u8],
    vge_bytes: &[u8],
    prt_bytes: &[u8],
    max_fragment_bytes: usize,
) -> Vec<u8> {
    let mut frames = Vec::with_capacity(
        // generous initial cap; each frame header is 9 bytes
        9 * (3 + frags(vt_bytes.len(), max_fragment_bytes)
            + frags(vge_bytes.len(), max_fragment_bytes)
            + frags(prt_bytes.len(), max_fragment_bytes))
            + vt_bytes.len()
            + vge_bytes.len()
            + prt_bytes.len(),
    );

    append_downstream(
        &mut frames,
        &DownstreamFrame::SnapshotBegin {
            snapshot_version,
            rows,
            cols,
            sequence_id,
        },
    );

    push_fragmented(&mut frames, vt_bytes, max_fragment_bytes, FragmentKind::Vt);
    push_fragmented(&mut frames, vge_bytes, max_fragment_bytes, FragmentKind::Vge);
    push_fragmented(&mut frames, prt_bytes, max_fragment_bytes, FragmentKind::Prt);

    append_downstream(&mut frames, &DownstreamFrame::SnapshotEnd { sequence_id });

    wrap_e2r_envelope(&frames)
}

/// Convenience for a single `DetachNotify` frame in its own
/// envelope. Used by the engine (veterd) at attach teardown to ask
/// the renderer to restore its pre-attach state.
pub fn encode_detach_notify() -> Vec<u8> {
    let mut frames = Vec::new();
    append_downstream(&mut frames, &DownstreamFrame::DetachNotify);
    wrap_e2r_envelope(&frames)
}

/// Convenience for a single upstream Accept frame in its own envelope.
pub fn encode_accepted(sequence_id: u32) -> Vec<u8> {
    let mut frames = Vec::new();
    append_upstream(
        &mut frames,
        &UpstreamFrame::SnapshotAccepted { sequence_id },
    );
    wrap_r2e_envelope(&frames)
}

/// Convenience for a single upstream Reject frame in its own envelope.
pub fn encode_rejected(sequence_id: u32, reason: u8) -> Vec<u8> {
    let mut frames = Vec::new();
    append_upstream(
        &mut frames,
        &UpstreamFrame::SnapshotRejected { sequence_id, reason },
    );
    wrap_r2e_envelope(&frames)
}

#[derive(Copy, Clone)]
enum FragmentKind {
    Vt,
    Vge,
    Prt,
}

fn frags(total: usize, chunk: usize) -> usize {
    if total == 0 {
        1
    } else {
        total.div_ceil(chunk.max(1))
    }
}

fn push_fragmented(
    frames: &mut Vec<u8>,
    payload: &[u8],
    max_fragment_bytes: usize,
    kind: FragmentKind,
) {
    let chunk = max_fragment_bytes.max(1);
    let total = frags(payload.len(), chunk) as u64;
    if payload.is_empty() {
        // Always emit at least one zero-length fragment so the renderer
        // sees the sub-snapshot is present (and trivially complete).
        let frame = match kind {
            FragmentKind::Vt => DownstreamFrame::VtFragment {
                index: 0,
                total: 1,
                payload: Vec::new(),
            },
            FragmentKind::Vge => DownstreamFrame::VgeFragment {
                index: 0,
                total: 1,
                payload: Vec::new(),
            },
            FragmentKind::Prt => DownstreamFrame::PrtFragment {
                index: 0,
                total: 1,
                payload: Vec::new(),
            },
        };
        append_downstream(frames, &frame);
        return;
    }
    for (i, slice) in payload.chunks(chunk).enumerate() {
        let frame = match kind {
            FragmentKind::Vt => DownstreamFrame::VtFragment {
                index: i as u64,
                total,
                payload: slice.to_vec(),
            },
            FragmentKind::Vge => DownstreamFrame::VgeFragment {
                index: i as u64,
                total,
                payload: slice.to_vec(),
            },
            FragmentKind::Prt => DownstreamFrame::PrtFragment {
                index: i as u64,
                total,
                payload: slice.to_vec(),
            },
        };
        append_downstream(frames, &frame);
    }
}

/// Read a complete payload off the wire (after APC unstuffing) and
/// yield its frames as `(frame_type, request_id, body)` tuples by
/// invoking `visit` for each frame. Returns `Err` on header / size
/// inconsistencies. Used by the engine to consume upstream renderer
/// envelopes and by the renderer to consume the snapshot envelope.
pub fn for_each_frame<F>(payload: &[u8], mut visit: F) -> Result<(), u16>
where
    F: FnMut(u8, u32, &[u8]) -> Result<(), u16>,
{
    use super::codec::Reader;
    let mut r = Reader::new(payload);
    let version = r.u8()?;
    if version != PROTOCOL_VERSION {
        return Err(ERR_BAD_PAYLOAD);
    }
    let payload_len = r.u32()? as usize;
    if payload_len + 5 != payload.len() {
        return Err(ERR_BAD_PAYLOAD);
    }
    while !r.at_end() {
        let frame_type = r.u8()?;
        let request_id = r.u32()?;
        let body_len = r.u32()? as usize;
        let body = r.take(body_len)?;
        visit(frame_type, request_id, body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apc::ApcStream;
    use crate::frame::MARKER_R2E;

    #[test]
    fn encode_snapshot_round_trip_single_envelope() {
        let env = encode_snapshot(
            1,
            24,
            80,
            42,
            b"vt-payload-bytes",
            b"vge-payload-bytes-also",
            b"prt-payload-bytes-recursive",
            DEFAULT_MAX_FRAGMENT_BYTES,
        );

        // Decode through the APC parser and the per-frame visitor.
        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);

        let mut frames = Vec::new();
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            frames.push(DownstreamFrame::parse(t, body).map_err(|_| 0u16)?);
            Ok(())
        })
        .unwrap();

        assert!(matches!(
            frames[0],
            DownstreamFrame::SnapshotBegin {
                snapshot_version: 1,
                rows: 24,
                cols: 80,
                sequence_id: 42,
            }
        ));
        let last = frames.len() - 1;
        assert!(matches!(
            frames[last],
            DownstreamFrame::SnapshotEnd { sequence_id: 42 }
        ));

        // Reassemble each sub-snapshot from its fragments and check
        // the payloads round-trip.
        fn reassemble(frames: &[DownstreamFrame], take: fn(&DownstreamFrame) -> Option<(u64, u64, &[u8])>) -> Vec<u8> {
            let mut chunks: Vec<(u64, &[u8])> = frames.iter().filter_map(take).map(|(i, _, p)| (i, p)).collect();
            chunks.sort_by_key(|(i, _)| *i);
            let mut out = Vec::new();
            for (_, p) in chunks { out.extend_from_slice(p); }
            out
        }

        let vt = reassemble(&frames, |f| match f {
            DownstreamFrame::VtFragment { index, total, payload } => Some((*index, *total, payload.as_slice())),
            _ => None,
        });
        let vge = reassemble(&frames, |f| match f {
            DownstreamFrame::VgeFragment { index, total, payload } => Some((*index, *total, payload.as_slice())),
            _ => None,
        });
        let prt = reassemble(&frames, |f| match f {
            DownstreamFrame::PrtFragment { index, total, payload } => Some((*index, *total, payload.as_slice())),
            _ => None,
        });

        assert_eq!(vt, b"vt-payload-bytes");
        assert_eq!(vge, b"vge-payload-bytes-also");
        assert_eq!(prt, b"prt-payload-bytes-recursive");
    }

    #[test]
    fn encode_snapshot_fragments_large_payloads() {
        // Force fragmentation by setting tiny max_fragment_bytes.
        let vt = vec![0xAA; 100];
        let vge = vec![0xBB; 50];
        let prt = vec![0xCC; 30];
        let env = encode_snapshot(0, 1, 1, 7, &vt, &vge, &prt, 16);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let mut frames = Vec::new();
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            frames.push(DownstreamFrame::parse(t, body).map_err(|_| 0u16)?);
            Ok(())
        })
        .unwrap();

        // Expect: 1 Begin + ceil(100/16)=7 + ceil(50/16)=4 + ceil(30/16)=2 + 1 End = 15 frames.
        assert_eq!(frames.len(), 15);

        // Per sub-snapshot, indices are 0..total and total is consistent.
        for kind_pick in [
            |f: &DownstreamFrame| matches!(f, DownstreamFrame::VtFragment { .. }),
            |f: &DownstreamFrame| matches!(f, DownstreamFrame::VgeFragment { .. }),
            |f: &DownstreamFrame| matches!(f, DownstreamFrame::PrtFragment { .. }),
        ] {
            let group: Vec<_> = frames.iter().filter(|f| kind_pick(f)).collect();
            assert!(!group.is_empty());
            let total = match group[0] {
                DownstreamFrame::VtFragment { total, .. }
                | DownstreamFrame::VgeFragment { total, .. }
                | DownstreamFrame::PrtFragment { total, .. } => *total,
                _ => unreachable!(),
            };
            assert_eq!(group.len() as u64, total);
            for (i, f) in group.iter().enumerate() {
                let (got_i, got_total) = match f {
                    DownstreamFrame::VtFragment { index, total, .. }
                    | DownstreamFrame::VgeFragment { index, total, .. }
                    | DownstreamFrame::PrtFragment { index, total, .. } => (*index, *total),
                    _ => unreachable!(),
                };
                assert_eq!(got_i, i as u64);
                assert_eq!(got_total, total);
            }
        }
    }

    #[test]
    fn empty_sub_snapshot_emits_one_zero_length_fragment() {
        let env = encode_snapshot(0, 1, 1, 0, b"", b"vge", b"", DEFAULT_MAX_FRAGMENT_BYTES);
        let mut s = ApcStream::new();
        let out = s.feed(&env);
        let mut frames = Vec::new();
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            frames.push(DownstreamFrame::parse(t, body).map_err(|_| 0u16)?);
            Ok(())
        })
        .unwrap();

        let vt_frames: Vec<_> = frames.iter().filter(|f| matches!(f, DownstreamFrame::VtFragment { .. })).collect();
        let prt_frames: Vec<_> = frames.iter().filter(|f| matches!(f, DownstreamFrame::PrtFragment { .. })).collect();
        assert_eq!(vt_frames.len(), 1);
        assert_eq!(prt_frames.len(), 1);
        for f in &vt_frames {
            assert!(matches!(f, DownstreamFrame::VtFragment { index: 0, total: 1, payload } if payload.is_empty()));
        }
        for f in &prt_frames {
            assert!(matches!(f, DownstreamFrame::PrtFragment { index: 0, total: 1, payload } if payload.is_empty()));
        }
    }

    #[test]
    fn snapshot_with_esc_bytes_in_payload_unstuffs() {
        // Stuffing happens at envelope encode and reverses at the APC
        // parser. Each sub-snapshot payload contains raw ESC bytes.
        let vt = vec![0x00, 0x1B, 0xFF, 0x1B, 0x1B];
        let env = encode_snapshot(0, 1, 1, 0, &vt, b"", b"", DEFAULT_MAX_FRAGMENT_BYTES);
        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let mut got = Vec::new();
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            if t == FRM_VT_FRAGMENT {
                let f = DownstreamFrame::parse(t, body).map_err(|_| 0u16)?;
                if let DownstreamFrame::VtFragment { payload, .. } = f {
                    got.extend(payload);
                }
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(got, vt);
    }

    #[test]
    fn upstream_accepted_round_trip() {
        let env = encode_accepted(123);
        let mut s = ApcStream::with_marker(*MARKER_R2E);
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);

        let mut frames = Vec::new();
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            frames.push(UpstreamFrame::parse(t, body).map_err(|_| 0u16)?);
            Ok(())
        })
        .unwrap();
        assert_eq!(frames, vec![UpstreamFrame::SnapshotAccepted { sequence_id: 123 }]);
    }

    #[test]
    fn upstream_rejected_round_trip() {
        let env = encode_rejected(456, REJECT_VERSION_MISMATCH);
        let mut s = ApcStream::with_marker(*MARKER_R2E);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);
        let mut frames = Vec::new();
        for_each_frame(&out.payloads[0], |t, _rid, body| {
            frames.push(UpstreamFrame::parse(t, body).map_err(|_| 0u16)?);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            frames,
            vec![UpstreamFrame::SnapshotRejected {
                sequence_id: 456,
                reason: REJECT_VERSION_MISMATCH,
            }]
        );
    }

    #[test]
    fn for_each_frame_rejects_wrong_version() {
        let mut payload = Vec::new();
        payload.push(99); // wrong protocol_version
        payload.extend_from_slice(&0u32.to_le_bytes());
        let r = for_each_frame(&payload, |_, _, _| Ok::<(), u16>(()));
        assert!(r.is_err());
    }

    #[test]
    fn for_each_frame_rejects_length_mismatch() {
        let mut payload = Vec::new();
        payload.push(PROTOCOL_VERSION);
        payload.extend_from_slice(&100u32.to_le_bytes()); // claims 100 bytes follow
        payload.extend_from_slice(&[0u8; 5]); // only 5 actually follow
        let r = for_each_frame(&payload, |_, _, _| Ok::<(), u16>(()));
        assert!(r.is_err());
    }
}
