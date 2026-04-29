//! Vector Graphics Extension (VGE) wire format.
//!
//! See `doc/vector-graphics-extension.md` in the parent repo for the
//! full protocol. This crate carries everything that's purely about the
//! bytes on the wire — the streaming APC parser, primitive codec,
//! command/response framing, path-segment decoder, and command encoders
//! suitable for both terminal-side parsing and client-side emission.
//!
//! Anything related to terminal-side state (element table, scrollback
//! anchoring, rendering) lives in the consuming crate.

pub mod apc;
pub mod codec;
pub mod command;
pub mod encode;
pub mod envelope;
pub mod frame;
pub mod path;

pub use apc::TerminalEvent;
pub use codec::{Point, Reader, Rect, Writer};
pub use command::{
    Align, Color, Command, ConcreteStyle, CreateElementBody, DrawCmd, FontStyle, Style,
    UpdateCommandBody, UpdateCommandsBody, UpdateTextBody, UpdateTextRange,
};
pub use path::{PathNode, PathSegment};
