//! File Transfer Extension (VFT) wire format.
//!
//! See `doc/file-transfer-extension.md` in the parent repo for the full
//! protocol. This crate carries everything that's purely about the
//! bytes on the wire — the streaming APC parser, primitive codec,
//! command/response/event framing, and command encoders suitable for
//! both host-side parsing and client-side emission.
//!
//! Anything related to host-side state (transfer table, file handles,
//! worker threads, file pickers) lives in the consuming crate.

pub mod apc;
pub mod codec;
pub mod command;
pub mod encode;
pub mod envelope;
pub mod frame;

pub use apc::{ApcStream, Output};
pub use codec::{Reader, Writer};
pub use command::{
    BeginDownloadBody, BeginUploadBody, CancelTransferBody, Command, EndUploadBody,
    ReportDownloadAckBody, RequestAckBody, UploadChunkBody,
};
