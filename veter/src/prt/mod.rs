// PRT engine state + portal tree live in the `veter-host` crate
// (shared with veterd); the renderer half stays in this binary
// because it depends on femtovg / the live `TerminalRenderer`.
// Re-export the state surface here so existing `crate::prt::*` call
// sites keep working.

pub mod render;

pub use veter_host::prt::*;
