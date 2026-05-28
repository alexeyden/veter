// VGE host engine state. Renderer code lives in the `veter` binary's
// `vge::render`; this module contains only the protocol-agnostic
// state machinery so headless consumers (veterd) can link it.
//
// Wire-format types live in the `vge-protocol` crate and are
// re-exported here for convenience.

pub mod snapshot;
pub mod state;

pub use snapshot::SnapshotError;
pub use state::{GpuImageId, HostThemePalette, VgeEngine, VgeState};

pub use vge_protocol::*;
