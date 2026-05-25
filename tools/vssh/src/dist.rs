//! Locate the staged veter-tools dist bundle on the host machine.
//!
//! Two lookup paths, in order:
//!   1. Installed: `<exe dir>/../share/veter/dist/<triple>/{veter-tools.tar.xz,manifest.json}`
//!      (this is what `make install-dist-share` produces; see the
//!      Makefile changes in Phase 5 of the plan).
//!   2. Dev:       `<repo root>/dist/veter-tools-<version>-<triple>.tar.xz`
//!      + sibling `manifest-<triple>.json` (what `make dist-aarch64-manifest`
//!      drops directly into the repo).
//!
//! Returns `Ok(None)` when no bundle is found — vssh keeps working as
//! a thin ssh wrapper without one.

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

pub const DIST_ARCH: &str = "aarch64-unknown-linux-musl";

pub fn locate() -> Result<Option<DistBundle>> {
    let exe = std::env::current_exe().context("locating own exe")?;

    if let Some(bin_dir) = exe.parent() {
        let installed = bin_dir
            .join("..")
            .join("share")
            .join("veter")
            .join("dist")
            .join(DIST_ARCH);
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
        if let Some(b) = try_load_dev(&dev_dir)? {
            log::debug!("dist bundle (dev): {}", b.tarball.display());
            return Ok(Some(b));
        }
    }

    log::debug!("no dist bundle found");
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

fn try_load_dev(dir: &Path) -> Result<Option<DistBundle>> {
    let manifest_path = dir.join(format!("manifest-{DIST_ARCH}.json"));
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest = load_manifest(&manifest_path)?;
    let tarball = dir.join(format!(
        "veter-tools-{}-{}.tar.xz",
        manifest.version, DIST_ARCH
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

