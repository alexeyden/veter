//! The commit a binary was built from, for `--version`.
//!
//! Every crate in this workspace is `0.1.0` and has been for a hundred
//! commits, so "which build is this?" — the question behind comparing a
//! local tool with the one installed on a remote host — has no answer
//! in the crate version. This adds one. See `build.rs`.

/// Short commit sha, or `unknown` when built outside a git checkout.
pub const GIT_SHA: &str = env!("VETER_GIT_SHA");

/// Commit date, `YYYY-MM-DD`, or `unknown`.
pub const COMMIT_DATE: &str = env!("VETER_COMMIT_DATE");

/// `0.1.0 (29e88bd25 2026-08-27)` — pass the caller's own
/// `env!("CARGO_PKG_VERSION")`.
///
/// Returns `&'static str` because that is what clap's `version` wants,
/// and the string is built once and leaked: one small allocation per
/// process, for a value that lives as long as the process anyway.
///
/// No `-dirty` marker, deliberately. Nothing tells a build script that
/// the working tree changed, so the flag would have to be recomputed on
/// every build — meaning a relink of every binary in the workspace
/// after each `git status` — and if it isn't, it lies. A sha that is
/// occasionally less specific beats a cleanliness claim that is
/// occasionally false.
pub fn long_version(pkg_version: &str) -> &'static str {
    Box::leak(format!("{pkg_version} ({GIT_SHA} {COMMIT_DATE})").into_boxed_str())
}
