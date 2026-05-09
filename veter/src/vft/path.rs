// Host-side path resolution for BeginUpload / BeginDownload (§5.3).
//
// v1 policy is permissive: tilde expansion and resolution against cwd
// are applied; canonicalisation is deferred so we can also resolve
// upload destinations whose final component does not yet exist.

use std::env;
use std::path::PathBuf;

use vft_protocol::frame::*;

#[derive(Debug)]
pub struct PathError {
    pub code: u16,
    pub message: &'static str,
}

/// Resolve a user-supplied host path to an absolute path.
///
/// Applies `~/`, `~`, and `$HOME` expansion (only the leading-`~` form
/// is officially supported by the spec; anything more elaborate is
/// host-defined). Relative paths are joined to the host's current
/// working directory. The path is **not** canonicalised — the file may
/// not exist yet (e.g. on upload).
pub fn resolve(input: &str) -> Result<PathBuf, PathError> {
    if input.is_empty() {
        return Err(PathError {
            code: ERR_BAD_PAYLOAD,
            message: "empty path",
        });
    }
    let expanded = if input == "~" {
        home()?
    } else if let Some(rest) = input.strip_prefix("~/") {
        let mut p = home()?;
        p.push(rest);
        p
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        return Ok(expanded);
    }
    let cwd = env::current_dir().map_err(|_| PathError {
        code: ERR_INTERNAL,
        message: "current_dir failed",
    })?;
    Ok(cwd.join(expanded))
}

fn home() -> Result<PathBuf, PathError> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(PathError {
            code: ERR_PATH_INVALID,
            message: "HOME unset",
        })
}

/// Pick a destination for the deferred-form BeginUpload (§6.1). Uses
/// `${TMPDIR:-/tmp}` and the supplied basename, falling back to a
/// generated name if `basename` is empty or contains a path separator.
pub fn deferred_upload_destination(basename: &str) -> PathBuf {
    let dir = env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let name = sanitise_basename(basename);
    dir.join(name)
}

fn sanitise_basename(input: &str) -> String {
    // Strip any directory components and obviously dangerous bytes.
    // A traversal-like input ("../foo") collapses to "foo".
    let last = std::path::Path::new(input)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if last.is_empty() || last == "." || last == ".." {
        return generated_name();
    }
    last.to_string()
}

fn generated_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("vft-upload-{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_absolute_passthrough() {
        let p = resolve("/etc/hosts").unwrap();
        assert_eq!(p, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn tilde_only_expands_to_home() {
        // Skip if HOME is unset (unlikely in test env).
        if let Some(home) = env::var_os("HOME") {
            let p = resolve("~").unwrap();
            assert_eq!(p, PathBuf::from(home));
        }
    }

    #[test]
    fn tilde_slash_expansion() {
        if let Some(home) = env::var_os("HOME") {
            let p = resolve("~/foo/bar").unwrap();
            let expected = PathBuf::from(home).join("foo").join("bar");
            assert_eq!(p, expected);
        }
    }

    #[test]
    fn relative_path_joins_cwd() {
        let p = resolve("relative.txt").unwrap();
        assert!(p.is_absolute());
        assert!(p.ends_with("relative.txt"));
    }

    #[test]
    fn empty_path_is_bad_payload() {
        let err = resolve("").unwrap_err();
        assert_eq!(err.code, ERR_BAD_PAYLOAD);
    }

    #[test]
    fn deferred_destination_strips_path_components() {
        let p = deferred_upload_destination("../etc/passwd");
        assert!(p.ends_with("passwd"));
        // Must not contain any traversal segments.
        assert!(!p.to_string_lossy().contains(".."));
    }

    #[test]
    fn deferred_destination_generates_name_for_empty() {
        let p = deferred_upload_destination("");
        assert!(p.file_name().is_some());
    }

    #[test]
    fn deferred_destination_generates_name_for_dotdot() {
        let p = deferred_upload_destination("..");
        assert!(p.file_name().is_some());
        assert_ne!(p.file_name().unwrap(), "..");
    }
}
