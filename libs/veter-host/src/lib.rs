//! veter-host — host-side engine state (vt100, VGE, PRT, VFT) without
//! any GUI dependencies.
//!
//! This crate exists so the vsd session daemon
//! (`doc/session-manager.md`) can link the same authoritative engine
//! state machinery the local veter GUI binary uses, without dragging
//! femtovg / winit / glutin / parley / fontconfig into the daemon's
//! dep tree.
//!
//! Nothing here links a GUI toolkit, not even conditionally. The two
//! places the VFT engine needs a desktop — the native file picker and
//! the "open the finished download" step — are the
//! [`vft::DesktopHooks`] trait; the local veter binary installs an
//! implementation backed by `rfd` / `opener`, and headless consumers
//! (vsd) leave the default [`vft::HeadlessHooks`] no-ops in place.
//! That is the right semantics for a daemon anyway — VFT envelopes
//! pass through to the renderer where the real picker lives.

pub mod env;
pub mod prt;
pub mod ses;
pub mod vft;
pub mod vge;
pub mod vss;
