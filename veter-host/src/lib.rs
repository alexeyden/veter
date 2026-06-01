//! veter-host — host-side engine state (vt100, VGE, PRT, VFT) without
//! any GUI dependencies.
//!
//! This crate exists so the vsd session daemon
//! (`doc/session-manager.md`) can link the same authoritative engine
//! state machinery the local veter GUI binary uses, without dragging
//! femtovg / winit / glutin / parley / fontconfig into the daemon's
//! dep tree.
//!
//! The local veter binary depends on this crate with the `"gui"`
//! feature enabled, which activates `rfd` (file picker) and `opener`
//! (default-app launcher) inside the VFT worker. Headless consumers
//! (vsd) leave the feature off; those hooks degrade to no-ops,
//! which is the right semantics for a daemon — VFT envelopes pass
//! through to the renderer where the real picker lives.

pub mod prt;
pub mod ses;
pub mod vft;
pub mod vge;
pub mod vss;
