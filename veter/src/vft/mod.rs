// VFT engine lives in the `veter-host` crate so headless consumers
// (vsd) can use it without dragging GUI dependencies. The veter
// binary re-exports everything here for backwards compatibility with
// existing `crate::vft::*` call sites, and supplies the desktop half
// the engine deliberately leaves abstract.

use std::path::{Path, PathBuf};

pub use veter_host::vft::*;

/// The renderer-side [`DesktopHooks`]: a real file dialog and a real
/// default-app launcher, the two things `veter-host` refuses to link
/// a toolkit for. Installed on the host's VFT and PRT engines at
/// startup; `vsd` leaves the headless no-ops in place, so this is the
/// only place in the tree that opens a window on VFT's behalf.
pub struct RendererHooks;

impl DesktopHooks for RendererHooks {
    fn pick_file(&self, title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).pick_file()
    }

    fn open_path(&self, path: &Path) {
        // Best-effort by contract — the transfer already succeeded.
        let _ = opener::open(path);
    }
}
