// Typed frame bodies for the VSS extension. Two enums — one per
// direction — keep parse and encode together for each frame.

use super::codec::{Reader, Writer};
use super::frame::*;

/// Engine → renderer frames (marker `VSS`). The renderer reassembles
/// fragments until it has a complete `SnapshotBegin … SnapshotEnd`
/// triple of `Vt/Vge/Prt` fragment groups, then applies the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownstreamFrame {
    SnapshotBegin {
        snapshot_version: u32,
        rows: u16,
        cols: u16,
        sequence_id: u32,
    },
    VtFragment {
        index: u64,
        total: u64,
        payload: Vec<u8>,
    },
    VgeFragment {
        index: u64,
        total: u64,
        payload: Vec<u8>,
    },
    PrtFragment {
        index: u64,
        total: u64,
        payload: Vec<u8>,
    },
    SnapshotEnd {
        sequence_id: u32,
    },
    /// "Restore pre-attach state, attach is ending." Sent by the
    /// engine after live forwarding stops and before the connection
    /// tears down. The renderer's owning context (host or per-portal
    /// VssEngine) is expected to have stashed a binary snapshot of
    /// its pre-attach engine state on the first `SnapshotBegin` of
    /// this attach; on `DetachNotify` it restores from that stash.
    /// No body.
    DetachNotify,
}

impl DownstreamFrame {
    pub fn frame_type(&self) -> u8 {
        match self {
            DownstreamFrame::SnapshotBegin { .. } => FRM_SNAPSHOT_BEGIN,
            DownstreamFrame::VtFragment { .. } => FRM_VT_FRAGMENT,
            DownstreamFrame::VgeFragment { .. } => FRM_VGE_FRAGMENT,
            DownstreamFrame::PrtFragment { .. } => FRM_PRT_FRAGMENT,
            DownstreamFrame::SnapshotEnd { .. } => FRM_SNAPSHOT_END,
            DownstreamFrame::DetachNotify => FRM_DETACH_NOTIFY,
        }
    }

    pub fn encode_body(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            DownstreamFrame::SnapshotBegin {
                snapshot_version,
                rows,
                cols,
                sequence_id,
            } => {
                w.u32(*snapshot_version);
                w.u16(*rows);
                w.u16(*cols);
                w.u32(*sequence_id);
            }
            DownstreamFrame::VtFragment { index, total, payload }
            | DownstreamFrame::VgeFragment { index, total, payload }
            | DownstreamFrame::PrtFragment { index, total, payload } => {
                w.varu(*index);
                w.varu(*total);
                w.bytes(payload);
            }
            DownstreamFrame::SnapshotEnd { sequence_id } => {
                w.u32(*sequence_id);
            }
            DownstreamFrame::DetachNotify => {}
        }
        w.buf
    }

    pub fn parse(frame_type: u8, body: &[u8]) -> Result<Self, u16> {
        let mut r = Reader::new(body);
        // `?` auto-converts DecodeError → u16 via the From impl in codec.rs.
        let frame = match frame_type {
            FRM_SNAPSHOT_BEGIN => DownstreamFrame::SnapshotBegin {
                snapshot_version: r.u32()?,
                rows: r.u16()?,
                cols: r.u16()?,
                sequence_id: r.u32()?,
            },
            FRM_VT_FRAGMENT => {
                let index = r.varu()?;
                let total = r.varu()?;
                let payload = r.bytes()?.to_vec();
                DownstreamFrame::VtFragment { index, total, payload }
            }
            FRM_VGE_FRAGMENT => {
                let index = r.varu()?;
                let total = r.varu()?;
                let payload = r.bytes()?.to_vec();
                DownstreamFrame::VgeFragment { index, total, payload }
            }
            FRM_PRT_FRAGMENT => {
                let index = r.varu()?;
                let total = r.varu()?;
                let payload = r.bytes()?.to_vec();
                DownstreamFrame::PrtFragment { index, total, payload }
            }
            FRM_SNAPSHOT_END => DownstreamFrame::SnapshotEnd {
                sequence_id: r.u32()?,
            },
            FRM_DETACH_NOTIFY => DownstreamFrame::DetachNotify,
            _ => return Err(ERR_UNKNOWN_FRAME),
        };
        if !r.at_end() {
            return Err(ERR_BAD_PAYLOAD);
        }
        Ok(frame)
    }
}

/// Renderer → engine frames (marker `vss`). The engine reads these
/// after sending a snapshot to learn whether the renderer accepted
/// it or rejected it (version mismatch, malformed, capacity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamFrame {
    SnapshotAccepted { sequence_id: u32 },
    SnapshotRejected { sequence_id: u32, reason: u8 },
}

impl UpstreamFrame {
    pub fn frame_type(&self) -> u8 {
        match self {
            UpstreamFrame::SnapshotAccepted { .. } => FRM_SNAPSHOT_ACCEPTED,
            UpstreamFrame::SnapshotRejected { .. } => FRM_SNAPSHOT_REJECTED,
        }
    }

    pub fn encode_body(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            UpstreamFrame::SnapshotAccepted { sequence_id } => {
                w.u32(*sequence_id);
            }
            UpstreamFrame::SnapshotRejected { sequence_id, reason } => {
                w.u32(*sequence_id);
                w.u8(*reason);
            }
        }
        w.buf
    }

    pub fn parse(frame_type: u8, body: &[u8]) -> Result<Self, u16> {
        let mut r = Reader::new(body);
        let f = match frame_type {
            FRM_SNAPSHOT_ACCEPTED => UpstreamFrame::SnapshotAccepted {
                sequence_id: r.u32()?,
            },
            FRM_SNAPSHOT_REJECTED => UpstreamFrame::SnapshotRejected {
                sequence_id: r.u32()?,
                reason: r.u8()?,
            },
            _ => return Err(ERR_UNKNOWN_FRAME),
        };
        if !r.at_end() {
            return Err(ERR_BAD_PAYLOAD);
        }
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_begin_round_trip() {
        let f = DownstreamFrame::SnapshotBegin {
            snapshot_version: 7,
            rows: 24,
            cols: 80,
            sequence_id: 0xDEAD_BEEF,
        };
        let body = f.encode_body();
        let parsed = DownstreamFrame::parse(f.frame_type(), &body).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn vt_fragment_round_trip() {
        let f = DownstreamFrame::VtFragment {
            index: 1,
            total: 3,
            payload: vec![0x00, 0x1B, 0xFF, b'a'],
        };
        let body = f.encode_body();
        let parsed = DownstreamFrame::parse(f.frame_type(), &body).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn vge_and_prt_fragments_round_trip() {
        for f in &[
            DownstreamFrame::VgeFragment {
                index: 0,
                total: 1,
                payload: vec![0xAA; 1024],
            },
            DownstreamFrame::PrtFragment {
                index: 5,
                total: 6,
                payload: vec![],
            },
        ] {
            let body = f.encode_body();
            let parsed = DownstreamFrame::parse(f.frame_type(), &body).unwrap();
            assert_eq!(&parsed, f);
        }
    }

    #[test]
    fn snapshot_end_round_trip() {
        let f = DownstreamFrame::SnapshotEnd {
            sequence_id: 0xCAFE_BABE,
        };
        let body = f.encode_body();
        let parsed = DownstreamFrame::parse(f.frame_type(), &body).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn unknown_frame_type_errors() {
        assert_eq!(
            DownstreamFrame::parse(0xFE, &[]).unwrap_err(),
            ERR_UNKNOWN_FRAME
        );
    }

    #[test]
    fn detach_notify_round_trip() {
        let f = DownstreamFrame::DetachNotify;
        let body = f.encode_body();
        assert!(body.is_empty());
        let parsed = DownstreamFrame::parse(f.frame_type(), &body).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn trailing_garbage_in_body_errors() {
        let mut body = DownstreamFrame::SnapshotEnd { sequence_id: 1 }.encode_body();
        body.push(0xFF);
        assert_eq!(
            DownstreamFrame::parse(FRM_SNAPSHOT_END, &body).unwrap_err(),
            ERR_BAD_PAYLOAD
        );
    }

    #[test]
    fn truncated_body_errors() {
        // SnapshotBegin needs 4+2+2+4 = 12 bytes; pass 11.
        let body = vec![0u8; 11];
        assert_eq!(
            DownstreamFrame::parse(FRM_SNAPSHOT_BEGIN, &body).unwrap_err(),
            ERR_BAD_PAYLOAD
        );
    }

    #[test]
    fn accepted_round_trip() {
        let f = UpstreamFrame::SnapshotAccepted {
            sequence_id: 0x1234_5678,
        };
        let body = f.encode_body();
        let parsed = UpstreamFrame::parse(f.frame_type(), &body).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn rejected_round_trip() {
        for reason in &[
            REJECT_VERSION_MISMATCH,
            REJECT_MALFORMED,
            REJECT_CAPACITY,
        ] {
            let f = UpstreamFrame::SnapshotRejected {
                sequence_id: 9,
                reason: *reason,
            };
            let body = f.encode_body();
            let parsed = UpstreamFrame::parse(f.frame_type(), &body).unwrap();
            assert_eq!(parsed, f);
        }
    }

    #[test]
    fn upstream_unknown_frame_errors() {
        assert_eq!(
            UpstreamFrame::parse(0x99, &[]).unwrap_err(),
            ERR_UNKNOWN_FRAME
        );
    }
}
