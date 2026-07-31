// The desktop affordances the VFT engine needs but cannot provide
// itself, expressed as a trait the host implements.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Two things a VFT transfer occasionally has to ask the desktop for,
/// neither of which the engine can do on its own: a native file
/// dialog (§7.1, deferred-form `BeginDownload` — the client names no
/// path and the user picks one) and handing a finished file to the
/// user's default application (§6.1, deferred-form upload — how
/// `vsend ./shot.png` lands in `$TMPDIR` and then pops open in an
/// image viewer).
///
/// Both are called from worker threads and may block for as long as
/// the user takes to answer, hence `Send + Sync`.
///
/// The engine has no opinion about who implements this. `veter` backs
/// it with the platform dialog and launcher; `vsd` runs
/// [`HeadlessHooks`], because a daemon has no display and the VFT
/// envelopes pass through to the renderer where the real picker
/// lives anyway.
pub trait DesktopHooks: Send + Sync {
    /// Ask the user to choose a file, blocking until they answer.
    /// `None` covers every "no file" outcome — user cancelled, no
    /// display, dialog backend failed — because the engine answers
    /// the client with `err_cancelled` in all of them.
    fn pick_file(&self, title: &str) -> Option<PathBuf>;

    /// Open `path` in the user's default application. Best-effort:
    /// the file is already durable by the time this runs, so a
    /// failure to launch anything must not fail the transfer.
    fn open_path(&self, path: &Path);
}

/// The no-op implementation: no picker, no launcher. What `vsd` and
/// the engine's own tests run with, and the engine's default until a
/// host installs something else.
pub struct HeadlessHooks;

impl DesktopHooks for HeadlessHooks {
    fn pick_file(&self, _title: &str) -> Option<PathBuf> {
        None
    }
    fn open_path(&self, _path: &Path) {}
}

/// Shared handle to the host's hooks. One instance is cloned into
/// every per-portal engine, so a `vsend` running inside a nested
/// portal reaches the same renderer as one on the host screen.
pub type Hooks = Arc<dyn DesktopHooks>;

/// A handle to [`HeadlessHooks`], for engines constructed without a
/// host (tests, headless consumers, client-side helpers).
pub fn headless() -> Hooks {
    Arc::new(HeadlessHooks)
}
