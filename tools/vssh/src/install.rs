//! Decide whether to install veter-tools on a remote host, and do it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::args::Cli;
use crate::dist::{DistBundle, Manifest};
use crate::probe::ProbeResult;
use crate::ssh::Master;

#[derive(Debug)]
pub enum Action {
    Skip(SkipReason),
    Install,
    RefuseSystem { path: PathBuf },
}

#[derive(Debug)]
#[allow(dead_code)] // Fields are read via the Debug derive in log calls.
pub enum SkipReason {
    /// No staged dist tarball on the host.
    NoBundle,
    /// Remote arch doesn't match what we shipped for.
    UnsupportedArch { remote: String, bundle: String },
    /// Remote manifest sha matches local — already current.
    UpToDate,
    /// Remote $HOME isn't writable (read-only filesystem, kiosk, etc).
    HomeNotWritable,
    /// User passed `--no-update`.
    Disabled,
}

pub fn decide(bundle: Option<&DistBundle>, remote: &ProbeResult, cli: &Cli) -> Action {
    if cli.no_update {
        return Action::Skip(SkipReason::Disabled);
    }
    let Some(bundle) = bundle else {
        return Action::Skip(SkipReason::NoBundle);
    };
    if !arch_compatible(&remote.arch, &bundle.manifest.arch) {
        return Action::Skip(SkipReason::UnsupportedArch {
            remote: remote.arch.clone(),
            bundle: bundle.manifest.arch.clone(),
        });
    }
    if let Some(p) = &remote.vmux_path {
        if !is_user_local_bin(p) && !cli.overwrite_system {
            return Action::RefuseSystem { path: p.clone() };
        }
    }
    if !cli.force_update && already_up_to_date(&bundle.manifest, &remote.installed_manifest) {
        return Action::Skip(SkipReason::UpToDate);
    }
    if !remote.home_writable {
        return Action::Skip(SkipReason::HomeNotWritable);
    }
    Action::Install
}

pub fn perform(master: &Master, bundle: &DistBundle) -> Result<()> {
    log::info!(
        "installing veter-tools (sha256 {}) to remote ~/.local/bin/",
        &bundle.manifest.sha256[..16.min(bundle.manifest.sha256.len())]
    );
    upload_and_extract(master, &bundle.tarball)?;
    write_remote_manifest(master, &bundle.manifest)?;
    log::info!("install complete");
    Ok(())
}

/// Streams the tarball into a temp dir on the remote, then copies
/// the executables into `~/.local/bin`. We don't extract directly
/// into `~/.local/bin` because the tarball's top-level entry is
/// `veter-tools-<version>/` and we only want the binaries (not the
/// README) at the destination.
fn upload_and_extract(master: &Master, tarball: &std::path::Path) -> Result<()> {
    let remote_cmd = r#"set -e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
tar -xJpf - -C "$tmp"
mkdir -p "$HOME/.local/bin"
for t in vmux vcat vsend vrecv veterd; do
  src=$(ls "$tmp"/veter-tools-*/"$t" 2>/dev/null | head -n1)
  if [ -n "$src" ] && [ -f "$src" ]; then
    install -m 0755 "$src" "$HOME/.local/bin/$t"
    echo "installed $t"
  fi
done"#;
    let file = std::fs::File::open(tarball)
        .with_context(|| format!("opening {}", tarball.display()))?;
    master.run_with_stdin(remote_cmd, file)
}

fn write_remote_manifest(master: &Master, manifest: &Manifest) -> Result<()> {
    let mut json = serde_json::to_string(manifest).context("serializing manifest")?;
    json.push('\n');
    let cmd = r#"mkdir -p "$HOME/.local/share/veter-tools" && cat > "$HOME/.local/share/veter-tools/manifest.json""#;
    master.run_with_stdin(cmd, json.as_bytes())
}

/// `uname -m` ↔ rust target-triple compatibility. The triple's first
/// hyphen-separated component is the architecture token (`aarch64`,
/// `x86_64`, …) and must match `uname -m` exactly.
fn arch_compatible(uname_m: &str, triple: &str) -> bool {
    triple.starts_with(&format!("{uname_m}-"))
}

/// True iff `path` looks like `<...>/.local/bin/<name>`. We can't
/// expand the remote `$HOME` from here, so a path-shape check is the
/// best we can do without an extra round trip.
fn is_user_local_bin(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    if dir.file_name().and_then(|s| s.to_str()) != Some("bin") {
        return false;
    }
    let Some(parent) = dir.parent() else {
        return false;
    };
    parent.file_name().and_then(|s| s.to_str()) == Some(".local")
}

fn already_up_to_date(local: &Manifest, remote: &Option<Manifest>) -> bool {
    match remote {
        Some(r) => r.sha256 == local.sha256,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_bundle(sha: &str, arch: &str) -> DistBundle {
        DistBundle {
            tarball: PathBuf::from("/tmp/x.tar.xz"),
            manifest: Manifest {
                version: "0.1.4".into(),
                arch: arch.into(),
                sha256: sha.into(),
                tools: vec!["vmux".into()],
            },
        }
    }

    fn mk_probe(arch: &str, vmux: Option<&str>, remote_sha: Option<&str>) -> ProbeResult {
        ProbeResult {
            arch: arch.into(),
            installed_manifest: remote_sha.map(|s| Manifest {
                version: "0.1.0".into(),
                arch: "aarch64-unknown-linux-musl".into(),
                sha256: s.into(),
                tools: vec![],
            }),
            vmux_path: vmux.map(PathBuf::from),
            home_writable: true,
        }
    }

    fn mk_cli() -> Cli {
        Cli {
            verbose: false,
            no_update: false,
            force_update: false,
            overwrite_system: false,
            fix_path: false,
            ssh_args: vec!["host".into()],
        }
    }

    #[test]
    fn fresh_install() {
        let b = mk_bundle("abc", "aarch64-unknown-linux-musl");
        let p = mk_probe("aarch64", None, None);
        assert!(matches!(decide(Some(&b), &p, &mk_cli()), Action::Install));
    }

    #[test]
    fn no_bundle_skips() {
        let p = mk_probe("aarch64", None, None);
        assert!(matches!(
            decide(None, &p, &mk_cli()),
            Action::Skip(SkipReason::NoBundle)
        ));
    }

    #[test]
    fn wrong_arch_skips() {
        let b = mk_bundle("abc", "aarch64-unknown-linux-musl");
        let p = mk_probe("x86_64", None, None);
        assert!(matches!(
            decide(Some(&b), &p, &mk_cli()),
            Action::Skip(SkipReason::UnsupportedArch { .. })
        ));
    }

    #[test]
    fn up_to_date_skips() {
        let b = mk_bundle("samesha", "aarch64-unknown-linux-musl");
        let p = mk_probe("aarch64", Some("/home/u/.local/bin/vmux"), Some("samesha"));
        assert!(matches!(
            decide(Some(&b), &p, &mk_cli()),
            Action::Skip(SkipReason::UpToDate)
        ));
    }

    #[test]
    fn force_update_bypasses_up_to_date() {
        let b = mk_bundle("samesha", "aarch64-unknown-linux-musl");
        let p = mk_probe("aarch64", Some("/home/u/.local/bin/vmux"), Some("samesha"));
        let mut cli = mk_cli();
        cli.force_update = true;
        assert!(matches!(decide(Some(&b), &p, &cli), Action::Install));
    }

    #[test]
    fn no_update_skips_even_when_outdated() {
        let b = mk_bundle("newsha", "aarch64-unknown-linux-musl");
        let p = mk_probe("aarch64", None, None);
        let mut cli = mk_cli();
        cli.no_update = true;
        assert!(matches!(
            decide(Some(&b), &p, &cli),
            Action::Skip(SkipReason::Disabled)
        ));
    }

    #[test]
    fn refuses_system_vmux() {
        let b = mk_bundle("newsha", "aarch64-unknown-linux-musl");
        let p = mk_probe("aarch64", Some("/usr/bin/vmux"), None);
        assert!(matches!(
            decide(Some(&b), &p, &mk_cli()),
            Action::RefuseSystem { .. }
        ));
    }

    #[test]
    fn overwrite_system_proceeds() {
        let b = mk_bundle("newsha", "aarch64-unknown-linux-musl");
        let p = mk_probe("aarch64", Some("/usr/bin/vmux"), None);
        let mut cli = mk_cli();
        cli.overwrite_system = true;
        assert!(matches!(decide(Some(&b), &p, &cli), Action::Install));
    }

    #[test]
    fn home_ro_skips() {
        let b = mk_bundle("newsha", "aarch64-unknown-linux-musl");
        let mut p = mk_probe("aarch64", None, None);
        p.home_writable = false;
        assert!(matches!(
            decide(Some(&b), &p, &mk_cli()),
            Action::Skip(SkipReason::HomeNotWritable)
        ));
    }

    #[test]
    fn user_local_bin_path_check() {
        assert!(is_user_local_bin(Path::new("/home/u/.local/bin/vmux")));
        assert!(is_user_local_bin(Path::new("/root/.local/bin/vmux")));
        assert!(!is_user_local_bin(Path::new("/usr/bin/vmux")));
        assert!(!is_user_local_bin(Path::new("/usr/local/bin/vmux")));
        assert!(!is_user_local_bin(Path::new("/opt/.local/somewhere/bin/vmux")));
    }
}
