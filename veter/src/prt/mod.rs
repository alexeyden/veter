// Host-side PRT: portal table, per-portal vt100 instances, and command
// dispatch. Wire-format types live in the `prt-protocol` crate and are
// re-exported here for convenience.
//
// Phase 5 wires the engine into `main.rs`; until then the re-exports
// here have no in-crate consumers and would otherwise warn.
#![allow(unused_imports)]

pub mod portal;
pub mod render;
pub mod state;

pub use portal::{Portal, PortalAnchor, PortalSet};
pub use state::{FocusKind, Limits, PrtEngine, PrtState};

// Re-export the wire-format crate so existing call sites don't need to
// know about the split.
pub use prt_protocol::*;
