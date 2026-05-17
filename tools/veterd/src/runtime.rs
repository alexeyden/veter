//! Per-session runtime helpers: socket-path layout, runtime-directory
//! creation, stale-socket detection.
//!
//! Filesystem layout is flat:
//!
//! ```text
//! $XDG_RUNTIME_DIR/veterd/
//!   <NAME>.sock       per-session Unix-domain socket (mode 0700)
//!   <NAME>.log        per-session stderr/stdout for detached `new`
//! ```
//!
//! `<NAME>` is the same ≤64-byte UTF-8 string PRT accepts as a portal
//! id. Liveness is connection-based: a probe `connect(2)` that returns
//! `ECONNREFUSED` means the previous session process died without
//! cleaning up, and the CLI auto-unlinks the file.

use std::fs;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Max session name length, mirroring PRT portal id rules (§6.8 of
/// `doc/portal-extension.md`).
pub const MAX_NAME_BYTES: usize = 64;

/// Validate a user-supplied session name. Rejects empty names, names
/// over the size cap, names containing path separators, NULs, or
/// whitespace. The resulting `.sock` / `.log` filenames are safe to
/// concatenate with the runtime dir.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name must be non-empty");
    }
    if name.len() > MAX_NAME_BYTES {
        anyhow::bail!(
            "session name exceeds {MAX_NAME_BYTES} bytes ({} given)",
            name.len()
        );
    }
    for c in name.chars() {
        if c == '/' || c == '\0' || c.is_whitespace() || c.is_control() {
            anyhow::bail!(
                "session name contains illegal character {:?} (no slash, NUL, whitespace, or control codes)",
                c
            );
        }
    }
    Ok(())
}

/// `$XDG_RUNTIME_DIR/veterd/`. Falls back to `/tmp/veterd-<uid>/` if
/// the env var is unset (e.g. plain sshd without pam_systemd).
pub fn runtime_dir() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "/tmp/veterd-{}",
                nix::unistd::getuid().as_raw()
            ))
        });
    runtime.join("veterd")
}

/// Per-session socket path: `<runtime>/<NAME>.sock`.
pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
}

/// Per-session log path: `<runtime>/<NAME>.log`. Used by `new` when
/// it detaches the session process — stdout/stderr go here.
pub fn log_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.log"))
}

/// Ensure `runtime_dir()` exists and is mode 0700. Idempotent.
pub fn ensure_runtime_dir() -> Result<()> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&dir)
        .with_context(|| format!("stat {}", dir.display()))?
        .permissions();
    perms.set_mode(0o700);
    let _ = fs::set_permissions(&dir, perms);
    Ok(())
}

/// Result of [`probe_socket`].
#[derive(Debug, PartialEq, Eq)]
pub enum SocketProbe {
    /// The path doesn't exist at all.
    Missing,
    /// The path exists but no process is `accept`-ing on it. The
    /// file has been removed.
    Stale,
    /// The path exists and a process accepted a connect — the
    /// session is alive.
    Alive,
}

/// Probe a per-session socket without blocking the caller. If the
/// socket is `Stale` the file is unlinked as a side effect so the
/// caller can simply re-bind on it.
pub fn probe_socket(path: &Path) -> SocketProbe {
    if !path.exists() {
        return SocketProbe::Missing;
    }
    match UnixStream::connect(path) {
        Ok(_) => SocketProbe::Alive,
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            let _ = fs::remove_file(path);
            SocketProbe::Stale
        }
        Err(_) => {
            // Other errors (ENOENT race, permission, …) — treat as
            // Missing to avoid mistakenly nuking a path we can't read.
            SocketProbe::Missing
        }
    }
}

/// Enumerate session names whose `.sock` files live under
/// [`runtime_dir`]. Best-effort: returns an empty vec if the
/// directory doesn't exist yet. Stale sockets are auto-unlinked
/// during the walk so callers don't have to.
pub fn enumerate_sessions() -> Vec<String> {
    let dir = runtime_dir();
    let read_dir = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(name) = file.strip_suffix(".sock") else {
            continue;
        };
        // Reject any leftover oddballs that don't match name rules.
        if validate_name(name).is_err() {
            continue;
        }
        match probe_socket(&path) {
            SocketProbe::Alive => names.push(name.to_owned()),
            SocketProbe::Missing | SocketProbe::Stale => {}
        }
    }
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn validate_name_accepts_normal_strings() {
        validate_name("foo").unwrap();
        validate_name("test-1").unwrap();
        validate_name("foo.bar").unwrap();
        validate_name(&"x".repeat(MAX_NAME_BYTES)).unwrap();
    }

    #[test]
    fn validate_name_rejects_obvious_bad_inputs() {
        assert!(validate_name("").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name("a\tb").is_err());
        assert!(validate_name("a\0b").is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_BYTES + 1)).is_err());
    }

    #[test]
    fn probe_returns_missing_for_nonexistent_path() {
        let tmp = std::env::temp_dir().join("veterd-test-missing.sock");
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(probe_socket(&tmp), SocketProbe::Missing);
    }

    #[test]
    fn probe_unlinks_stale_socket() {
        let tmp = std::env::temp_dir().join(format!(
            "veterd-test-stale-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        // Create a socket file that has no listener — connect returns
        // ECONNREFUSED on Linux/macOS for this case.
        std::fs::File::create(&tmp).unwrap();
        // Treat plain regular file like a stale socket: probe should
        // return Missing because it isn't a socket. Test instead with
        // a real listener that we drop.
        std::fs::remove_file(&tmp).unwrap();
        let listener = UnixListener::bind(&tmp).unwrap();
        drop(listener);
        assert!(tmp.exists());
        assert_eq!(probe_socket(&tmp), SocketProbe::Stale);
        assert!(!tmp.exists());
    }

    #[test]
    fn probe_returns_alive_for_listening_socket() {
        let tmp = std::env::temp_dir().join(format!(
            "veterd-test-alive-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        let _listener = UnixListener::bind(&tmp).unwrap();
        assert_eq!(probe_socket(&tmp), SocketProbe::Alive);
        let _ = std::fs::remove_file(&tmp);
    }
}
