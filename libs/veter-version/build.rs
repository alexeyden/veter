//! Stamp the commit the binaries were built from into the binaries.
//!
//! `--version` reporting a crate version alone can't answer the
//! question that actually comes up: are the tools on this machine and
//! the tools on that one the same build? Every crate here is `0.1.0`
//! and stays `0.1.0` across a hundred commits, so the version is the
//! one thing that never differs when the binaries do.
//!
//! No `.git` (a build from a release tarball) is not an error — the
//! sha reads `unknown` and everything still builds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short=9", "HEAD"]);
    let date = git(&["log", "-1", "--format=%cs"]);

    println!(
        "cargo:rustc-env=VETER_GIT_SHA={}",
        sha.as_deref().unwrap_or("unknown")
    );
    println!(
        "cargo:rustc-env=VETER_COMMIT_DATE={}",
        date.as_deref().unwrap_or("unknown")
    );

    // Rebuild when HEAD moves — a commit, a checkout, a rebase. Both
    // files matter: `HEAD` itself changes on checkout, and the ref it
    // names changes on commit while `HEAD` stays put.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) {
        rerun_if_exists(&git_dir.join("HEAD"));
        if let Some(head_ref) = git(&["rev-parse", "--symbolic-full-name", "HEAD"]) {
            rerun_if_exists(&git_dir.join(head_ref));
        }
        // A repo whose refs are packed keeps them in one file instead.
        rerun_if_exists(&git_dir.join("packed-refs"));
    }
}

/// `git` with no repo, no git binary, or a failing command all mean the
/// same thing here: we don't know the commit, and that is survivable.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Watch a path only if it is there. A `rerun-if-changed` naming a
/// missing file re-runs the script on every build, which for a crate
/// every binary depends on means relinking the whole workspace each
/// time.
fn rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
