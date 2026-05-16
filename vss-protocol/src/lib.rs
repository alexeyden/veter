//! Veter State Snapshot (VSS) wire format.
//!
//! VSS carries a binary engine-state dump from `veterd` to an
//! attaching renderer, replacing the v1 replay-style snapshot. See
//! `doc/session-manager.md` §4 for the protocol's role; the wire
//! format is documented inline against the PRT/VGE/VFT §1.1–1.4
//! conventions (APC envelope, ESC byte-stuffing, primitive codec,
//! framed payload).
//!
//! This crate is wire-format only: streaming APC parser, primitive
//! codec, typed frame bodies, envelope wrapping. Host-side state
//! lives in `veter-host`'s `VssEngine` (snapshot reassembly, restore
//! dispatch); session lifecycle lives in `tools/veterd`.

pub mod apc;
pub mod codec;
pub mod envelope;
pub mod frame;
pub mod frames;

pub use apc::{ApcStream, Output};
pub use codec::{stuff, DecodeError, DecodeResult, Reader, Writer};
pub use envelope::{
    append_downstream, append_frame, append_upstream, encode_accepted, encode_rejected,
    encode_snapshot, for_each_frame, wrap_e2r_envelope, wrap_r2e_envelope,
};
pub use frame::{
    DEFAULT_MAX_FRAGMENT_BYTES, ERR_BAD_PAYLOAD, ERR_UNKNOWN_FRAME, FRM_PRT_FRAGMENT,
    FRM_SNAPSHOT_ACCEPTED, FRM_SNAPSHOT_BEGIN, FRM_SNAPSHOT_END, FRM_SNAPSHOT_REJECTED,
    FRM_VGE_FRAGMENT, FRM_VT_FRAGMENT, MARKER_E2R, MARKER_R2E, PROTOCOL_VERSION,
    REJECT_CAPACITY, REJECT_MALFORMED, REJECT_VERSION_MISMATCH, SNAPSHOT_VERSION,
};
pub use frames::{DownstreamFrame, UpstreamFrame};
