// Terminal-side VGE: state machine + femtovg renderer.
// Wire-format types live in the `vge-protocol` crate and are re-exported
// here for convenience.

pub mod render;
pub mod state;

pub use state::{GpuImageId, VgeEngine, VgeState};

// Re-export the wire-format crate so existing call sites don't need to
// know about the split.
pub use vge_protocol::*;
