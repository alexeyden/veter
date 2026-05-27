//! Locate the staged veter-tools dist bundle on the host machine.
//!
//! The lookup is parameterised by the remote's `uname -m` (already
//! known from probing); we map that to the rust target triple we
//! shipped a bundle for and look it up in one of:
//!   1. Installed: `<exe dir>/../share/veter/dist/<triple>/{veter-tools.tar.xz,manifest.json}`
//!      (this is what `make install-dist-share-<triple>` produces).
//!   2. Dev:       `<repo root>/dist/veter-tools-<version>-<triple>.tar.xz`
//!      + sibling `manifest-<triple>.json` (what `make dist-<arch>-manifest`
//!      drops directly into the repo).
//!
//! Returns `Ok(None)` when the remote arch has no known triple or no
//! bundle has been staged for it — vssh keeps working as a thin ssh
//! wrapper without one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub version: String,
    pub arch: String,
    pub sha256: String,
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug)]
pub struct DistBundle {
    pub tarball: PathBuf,
    pub manifest: Manifest,
}

/// Map `uname -m` to the musl rust target triple we cross-build for,
/// or `None` if the remote arch isn't one we ship binaries for.
pub fn triple_for_uname(uname_m: &str) -> Option<&'static str> {
    match uname_m {
        "aarch64" | "arm64" => Some("aarch64-unknown-linux-musl"),
        "x86_64" | "amd64" => Some("x86_64-unknown-linux-musl"),
        _ => None,
    }
}

pub fn locate(uname_m: &str) -> Result<Option<DistBundle>> {
    let Some(triple) = triple_for_uname(uname_m) else {
        log::debug!("no dist triple for uname -m={uname_m}");
        return Ok(None);
    };
    let exe = std::env::current_exe().context("locating own exe")?;

    if let Some(bin_dir) = exe.parent() {
        let installed = bin_dir
            .join("..")
            .join("share")
            .join("veter")
            .join("dist")
            .join(triple);
        if let Some(b) = try_load_installed(&installed)? {
            log::debug!("dist bundle (installed): {}", b.tarball.display());
            return Ok(Some(b));
        }
    }

    // Dev-mode lookup uses CARGO_MANIFEST_DIR baked in at build time,
    // because cargo's target dir can live anywhere (a custom
    // `target-dir`, e.g. /mnt/data/cargo-target) — meaning we can't
    // climb from current_exe() to the repo root in general.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // tools/
        .and_then(|p| p.parent()); // workspace/
    if let Some(root) = workspace_root {
        let dev_dir = root.join("dist");
        if let Some(b) = try_load_dev(&dev_dir, triple)? {
            log::debug!("dist bundle (dev): {}", b.tarball.display());
            return Ok(Some(b));
        }
    }

    log::debug!("no dist bundle found for {triple}");
    Ok(None)
}

fn try_load_installed(dir: &Path) -> Result<Option<DistBundle>> {
    let tarball = dir.join("veter-tools.tar.xz");
    let manifest_path = dir.join("manifest.json");
    if !tarball.is_file() || !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest = load_manifest(&manifest_path)?;
    Ok(Some(DistBundle { tarball, manifest }))
}

fn try_load_dev(dir: &Path, triple: &str) -> Result<Option<DistBundle>> {
    let manifest_path = dir.join(format!("manifest-{triple}.json"));
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest = load_manifest(&manifest_path)?;
    let tarball = dir.join(format!(
        "veter-tools-{}-{}.tar.xz",
        manifest.version, triple
    ));
    if !tarball.is_file() {
        log::warn!(
            "dev manifest {} references missing tarball {}",
            manifest_path.display(),
            tarball.display()
        );
        return Ok(None);
    }
    Ok(Some(DistBundle { tarball, manifest }))
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_mapping() {
        assert_eq!(triple_for_uname("aarch64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(triple_for_uname("arm64"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(triple_for_uname("x86_64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(triple_for_uname("amd64"), Some("x86_64-unknown-linux-musl"));
        assert_eq!(triple_for_uname("riscv64"), None);
        assert_eq!(triple_for_uname(""), None);
    }
}
