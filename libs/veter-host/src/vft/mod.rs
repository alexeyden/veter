// Host-side VFT engine: command dispatch, transfer table, worker
// threads. Wire-format types live in the `vft-protocol` crate and are
// re-exported here for convenience.
//
// The file picker and the post-finalize "open in default app" step
// are the engine's only desktop dependencies, and it takes them as
// the `DesktopHooks` trait rather than linking a toolkit — see
// `hooks`. `veter` installs a real implementation; `vsd` leaves the
// default `HeadlessHooks` in place, which is the right semantics for
// a daemon since the VFT envelopes pass through to the renderer where
// the real picker lives.
#![allow(unused_imports)]

pub mod hooks;
pub mod path;
pub mod state;
pub mod worker;

pub use hooks::{DesktopHooks, HeadlessHooks, Hooks};
pub use state::{Limits, VftEngine};
pub use worker::Wakeup;

pub use vft_protocol::*;
