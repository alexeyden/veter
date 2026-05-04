//! Portal Extension (PRT) wire format.
//!
//! See `doc/portal-extension.md` in the parent repo for the full
//! protocol. This crate carries everything that's purely about the
//! bytes on the wire — the streaming APC parser, primitive codec,
//! command/response framing, and command encoders suitable for both
//! host-side parsing and client-side emission.
//!
//! Anything related to host-side state (portal table, per-portal
//! vt100 instances, scrollback anchoring, rendering) lives in the
//! consuming crate.

pub mod apc;
pub mod codec;
pub mod command;
pub mod encode;
pub mod envelope;
pub mod frame;

pub use apc::{ApcStream, Output, TerminalEvent};
pub use codec::{Reader, Writer};
pub use command::{
    AnchorMode, Command, CreatePortalBody, CursorStyle, FocusTarget, UpdateOriginBody,
    WritePortalBody,
};
