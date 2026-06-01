//! Session Extension (SES) wire format.
//!
//! SES is a small APC-framed control channel between a multiplexer
//! client (`vmux`) and its immediate host. The host may be local
//! `veter` (no session) or a `vsd --session` process (a named,
//! persistent session). SES lets the client learn the session name
//! it lives inside and ask the host to detach it. See
//! `doc/session-extension.md` for the protocol's role; the wire
//! format mirrors PRT/VGE/VFT/VSS §1.1–1.4 (APC envelope, ESC
//! byte-stuffing, primitive codec, framed payload).
//!
//! This crate is wire-format only: streaming APC parser, primitive
//! codec, typed frame bodies, envelope wrapping. Host-side state
//! lives in `veter-host`'s `SesEngine`.

pub mod apc;
pub mod codec;
pub mod envelope;
pub mod frame;
pub mod frames;

pub use apc::{ApcStream, Output};
pub use codec::{DecodeError, DecodeResult, Reader, Writer, stuff};
pub use envelope::{
    append_command, append_frame, append_host_frame, encode_command, encode_host_frame,
    for_each_frame, wrap_c2h_envelope, wrap_h2c_envelope,
};
pub use frame::{
    CMD_DETACH, CMD_PROBE, ERR_BAD_PAYLOAD, ERR_INTERNAL, ERR_NOT_IN_SESSION, ERR_UNKNOWN_COMMAND,
    ERR_UNKNOWN_FRAME, MARKER_C2H, MARKER_H2C, PROTOCOL_VERSION, RSP_ERR, RSP_OK, RSP_PROBE,
};
pub use frames::{Command, HostFrame};
