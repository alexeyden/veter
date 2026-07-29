//! System clipboard — the one outside vfm.
//!
//! vfm's own `y`/`d`/`p` clipboard ([`crate::App::clipboard`]) never
//! leaves the process; this module is the other one, the one Dolphin and
//! Firefox can see. There are two routes into it, because the clipboard
//! belongs to whoever is *rendering*, not to whoever owns the files:
//!
//! * **Paths, as text** — OSC 52 ([`osc52_set`]). Works everywhere vfm
//!   runs, including inside a PRT portal and across an SSH hop: veter
//!   turns a portal's OSC 52 into a host-side clipboard write
//!   (`doc/portal-extension.md` §8.4), so the text lands on the machine
//!   the user is actually looking at.
//! * **The files themselves** — a `text/uri-list` selection set directly
//!   against the local display server ([`SysClip::set_files`]). A URI is
//!   only meaningful to the pasting application if vfm's paths *are* the
//!   renderer's paths, so this route refuses to run inside an SSH
//!   session: there a forwarded `$DISPLAY` would reach the right
//!   clipboard with the wrong paths. Callers fall back to the text route.
//!
//! ## A selection is a promise, not a store
//!
//! X11 and Wayland both keep the *owning process* on the hook: the bytes
//! are served on demand, not copied into the server, so whoever sets a
//! selection has to still be running when the paste happens. Neither of
//! arboard's Linux backends survives its process on its own —
//! `Options::foreground(false)` spawns a *thread*, not the forked child
//! `wl-copy` uses — so a copy made in vfm's own process would evaporate
//! the moment vfm exited.
//!
//! So vfm does not own the selection. [`SysClip::set_files`] re-execs
//! this same binary as [`SERVE_ARG`], hands it the paths down a pipe and
//! forgets about it; the helper `setsid`s away from the terminal and
//! blocks in [`serve_from_stdin`] serving the selection until some other
//! application takes it, then exits. Copying from vfm therefore behaves
//! like copying from any GUI app: it outlives the window it came from.
//! It also keeps the display-server connection (and, on X11, arboard's
//! background thread) out of vfm's own process entirely.

use std::path::{Path, PathBuf};

/// Largest text we will push through one OSC 52 sequence, in bytes
/// before base64. veter's parser grows its OSC buffer without limit, but
/// other terminals do not, and a selection big enough to hit this is not
/// a useful clipboard anyway — it is a directory listing.
pub const MAX_OSC52_TEXT: usize = 64 * 1024;

/// What [`SysClip::set_files`] managed to do.
// Without the feature only `Unavailable` is ever built, but the caller
// still matches on all three — keeping the shape identical is the point
// of the split impl below.
#[cfg_attr(not(feature = "system-clipboard"), allow(dead_code))]
pub enum FileCopy {
    Ok,
    /// There is no local selection for us to own. Not an error: the
    /// caller should fall back to copying paths as text, which still
    /// reaches the renderer's clipboard.
    Unavailable(&'static str),
    /// We had a clipboard and the write failed anyway.
    Failed(String),
}

/// One absolute path per line — the format every "copy path" expects,
/// and what a shell paste of several files wants too.
pub fn paths_as_text(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str(&absolute(p).to_string_lossy());
        out.push('\n');
    }
    out
}

/// The OSC 52 sequence that sets the `c` (clipboard) selection to
/// `text`. BEL-terminated: the form every terminal that implements OSC
/// 52 at all accepts.
pub fn osc52_set(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 4 / 3 + 16);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(b64_encode(text.as_bytes()).as_bytes());
    out.push(0x07);
    out
}

/// Best-effort absolutisation. vfm normally holds canonical paths (the
/// startup `canonicalize` and every `cd` since), but `file://` URIs and
/// a pasted path are both wrong if a relative one ever slips through.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Standard base64, `=`-padded — the encoding OSC 52 specifies for the
/// set form. The inverse of the decoder in `veter/src/clipboard.rs`.
fn b64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Hidden argv that turns this binary into the selection-owning helper
/// described in the module docs. Checked before anything else in `main`:
/// the helper has no terminal, so it must not reach the tty check.
pub const SERVE_ARG: &str = "--clipboard-serve";

/// Reason we never even try the file route inside an SSH session. See
/// the module docs: a forwarded display would take the URIs to the right
/// clipboard on the wrong machine.
#[cfg(feature = "system-clipboard")]
const WHY_SSH: &str = "not on the machine drawing this terminal";
/// Reason we give when there is no display server to talk to.
#[cfg(feature = "system-clipboard")]
const WHY_NO_DISPLAY: &str = "no desktop session to hold a file selection";

#[cfg(feature = "system-clipboard")]
pub struct SysClip;

#[cfg(feature = "system-clipboard")]
impl SysClip {
    pub fn new() -> Self {
        SysClip
    }

    /// Put `paths` on the system clipboard as files (a `text/uri-list`
    /// selection), so a paste in a file manager or a file-upload dialog
    /// picks up the files themselves rather than their names.
    ///
    /// Returns as soon as the helper is spawned — the copy is complete
    /// from the user's point of view, and vfm never blocks on the
    /// display server.
    pub fn set_files(&mut self, paths: &[PathBuf]) -> FileCopy {
        if vge_render::is_ssh_session() {
            return FileCopy::Unavailable(WHY_SSH);
        }
        // The helper would find this out for itself, but it has nowhere
        // to report it from behind `setsid` and null stderr — so the
        // check that decides between "copied" and "copied paths instead"
        // has to happen here.
        if !has_display() {
            return FileCopy::Unavailable(WHY_NO_DISPLAY);
        }
        match spawn_owner(paths) {
            Ok(()) => FileCopy::Ok,
            Err(e) => FileCopy::Failed(e),
        }
    }
}

/// Is there a display server to hand a selection to at all?
#[cfg(feature = "system-clipboard")]
fn has_display() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Re-exec this binary as the selection owner and feed it `paths`,
/// NUL-separated (the only separator no filename can contain) on stdin.
#[cfg(feature = "system-clipboard")]
fn spawn_owner(paths: &[PathBuf]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg(SERVE_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe and touches no shared state.
    // It is what detaches the helper from vfm's terminal, so closing the
    // terminal does not SIGHUP the clipboard out from under the user.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "helper has no stdin".to_string())?;
    let mut buf = Vec::new();
    for p in paths {
        buf.extend_from_slice(absolute(p).as_os_str().as_bytes());
        buf.push(0);
    }
    let sent = stdin.write_all(&buf).map_err(|e| e.to_string());
    drop(stdin);
    // The helper lives as long as it owns the selection — possibly hours
    // — so reap it on a thread rather than leaving a zombie behind. It
    // is `setsid`-detached, so vfm exiting first is fine: init adopts it.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    sent
}

/// The helper process: read NUL-separated paths from stdin, take the
/// selection, and hold it until another application takes it away.
/// Returns the process exit code.
#[cfg(feature = "system-clipboard")]
pub fn serve_from_stdin() -> i32 {
    use arboard::SetExtLinux;
    use std::ffi::OsStr;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;

    let mut buf = Vec::new();
    if std::io::stdin().read_to_end(&mut buf).is_err() {
        return 1;
    }
    let paths: Vec<PathBuf> = buf
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(OsStr::from_bytes(s)))
        .collect();
    if paths.is_empty() {
        return 1;
    }
    let Ok(mut cb) = arboard::Clipboard::new() else {
        return 1;
    };
    // `wait()` is the whole point: it blocks, serving paste requests,
    // until some other application takes the selection. Without it this
    // process would exit immediately and the clipboard would go empty.
    match cb.set().wait().file_list(&paths) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Without the feature there is no display-server dependency at all and
/// every copy takes the OSC 52 path. This is what the musl builds that
/// `make dist-<arch>-build` ships to remote hosts want: nothing there
/// could own a selection anyway.
#[cfg(not(feature = "system-clipboard"))]
pub struct SysClip;

#[cfg(not(feature = "system-clipboard"))]
impl SysClip {
    pub fn new() -> Self {
        SysClip
    }

    pub fn set_files(&mut self, _paths: &[PathBuf]) -> FileCopy {
        FileCopy::Unavailable("built without system-clipboard support")
    }
}

#[cfg(not(feature = "system-clipboard"))]
pub fn serve_from_stdin() -> i32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // RFC 4648 §10, plus the empty case. These are what veter's
        // decoder round-trips against.
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(plain.as_bytes()), encoded, "{plain:?}");
        }
    }

    #[test]
    fn base64_handles_non_ascii() {
        // Paths are bytes, not ASCII — a filename with an umlaut in it
        // must survive the trip.
        assert_eq!(b64_encode("äöü".as_bytes()), "w6TDtsO8");
    }

    #[test]
    fn osc52_wraps_the_payload() {
        assert_eq!(osc52_set("foo"), b"\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn osc52_payload_is_pure_base64() {
        // veter's vt100 only treats the OSC 52 set form as a clipboard
        // write when every payload byte is in the base64 alphabet
        // (`vt100/src/perform.rs`) — anything else falls through to
        // `unhandled_osc` and is silently dropped.
        const ALPHABET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=";
        let seq = osc52_set("/home/u/a file.txt\n/tmp/ä\n");
        let payload = &seq[b"\x1b]52;c;".len()..seq.len() - 1];
        assert!(payload.iter().all(|b| ALPHABET.contains(b)));
    }

    #[test]
    fn paths_are_newline_separated_and_absolute() {
        let text = paths_as_text(&[PathBuf::from("/a/b"), PathBuf::from("/c")]);
        assert_eq!(text, "/a/b\n/c\n");
        // A relative path is absolutised rather than emitted as-is: a
        // `file://` URI built from it would point somewhere else.
        let text = paths_as_text(&[PathBuf::from("rel")]);
        assert!(text.starts_with('/'), "{text:?}");
        assert!(text.ends_with("rel\n"), "{text:?}");
    }
}
