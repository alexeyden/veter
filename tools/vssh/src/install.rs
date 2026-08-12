//! Decide whether to install veter-tools on a remote host, and do it.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

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
    upload_and_extract(master, bundle)?;
    write_remote_manifest(master, &bundle.manifest)?;
    log::info!("install complete");
    Ok(())
}

/// What to install when the staged manifest predates the `tools`
/// field. This list used to be hardcoded in the remote command, where
/// it silently drifted behind the Makefile's `DIST_TOOLS` — `vfm`,
/// `vdraw` and `vproto` shipped in the tarball for releases without
/// ever being copied out of it. It survives only so an old bundle
/// installs something rather than nothing; the manifest is the source
/// of truth for everything current.
const LEGACY_TOOLS: &[&str] = &["vplay", "vmux", "vcat", "vsend", "vrecv", "vsd"];

/// What to copy out of the bundle into `~/.local/bin`: exactly the
/// binaries the manifest lists.
fn install_names(manifest: &Manifest) -> Vec<String> {
    if manifest.tools.is_empty() {
        LEGACY_TOOLS.iter().map(|s| (*s).to_string()).collect()
    } else {
        manifest.tools.clone()
    }
}

/// Names get spliced into a remote shell command, so they're held to
/// a charset that needs no quoting to be safe. The manifest is our
/// own file, but it's still a file on disk — anything outside this
/// charset means a corrupt (or hostile) manifest, which is worth
/// failing the install over rather than quoting around.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The shell script that runs on the remote: extract the tarball
/// streamed to its stdin into a temp dir, then copy `names` out of it
/// into `~/.local/bin`. We don't extract straight into `~/.local/bin`
/// because the tarball's top-level entry is `veter-tools-<version>/`
/// and we only want the listed entries (not the README) at the
/// destination.
fn remote_install_script(names: &[String]) -> Result<String> {
    if let Some(bad) = names.iter().find(|n| !is_safe_name(n)) {
        bail!("manifest lists an unusable tool name {bad:?}");
    }
    let list = names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        r#"set -e
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
tar -xJpf - -C "$tmp"
mkdir -p "$HOME/.local/bin"
for t in {list}; do
  src=$(ls "$tmp"/veter-tools-*/"$t" 2>/dev/null | head -n1)
  if [ -n "$src" ] && [ -f "$src" ]; then
    install -m 0755 "$src" "$HOME/.local/bin/$t"
    echo "installed $t"
  else
    echo "missing $t"
  fi
done"#
    ))
}

/// Streams the tarball into a temp dir on the remote, then copies
/// the executables into `~/.local/bin`.
fn upload_and_extract(master: &Master, bundle: &DistBundle) -> Result<()> {
    let names = install_names(&bundle.manifest);
    let remote_cmd = remote_install_script(&names)?;
    let file = std::fs::File::open(&bundle.tarball)
        .with_context(|| format!("opening {}", bundle.tarball.display()))?;
    let stdout = master.run_with_stdin(&remote_cmd, file)?;
    warn_about_missing(&stdout);
    Ok(())
}

/// The remote loop prints one `installed <name>` / `missing <name>`
/// line per expected entry. A `missing` line means the manifest and
/// the tarball disagree; the loop can't fail on that by itself (a
/// name it never finds simply doesn't get copied), which is how the
/// hardcoded list stayed wrong for so long without anyone noticing.
fn warn_about_missing(stdout: &str) {
    let missing: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("missing "))
        .collect();
    if !missing.is_empty() {
        log::warn!(
            "manifest lists {} not present in the tarball; not installed",
            missing.join(", ")
        );
    }
}

fn write_remote_manifest(master: &Master, manifest: &Manifest) -> Result<()> {
    let mut json = serde_json::to_string(manifest).context("serializing manifest")?;
    json.push('\n');
    let cmd = r#"mkdir -p "$HOME/.local/share/veter-tools" && cat > "$HOME/.local/share/veter-tools/manifest.json""#;
    master.run_with_stdin(cmd, json.as_bytes())?;
    Ok(())
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

    fn mk_manifest(tools: &[&str]) -> Manifest {
        Manifest {
            version: "0.1.7".into(),
            arch: "x86_64-unknown-linux-musl".into(),
            sha256: "abc".into(),
            tools: tools.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn install_names_come_from_the_manifest() {
        let m = mk_manifest(&["vmux", "vcat", "vfm", "vdraw", "vproto"]);
        assert_eq!(
            install_names(&m),
            vec!["vmux", "vcat", "vfm", "vdraw", "vproto"]
        );
    }

    #[test]
    fn install_names_fall_back_for_a_manifest_without_tools() {
        let m = mk_manifest(&[]);
        assert_eq!(install_names(&m), LEGACY_TOOLS.to_vec());
    }

    /// vplace drives the local terminal, so it is not in the bundle
    /// and must not be installed even if a stale manifest names it.
    #[test]
    fn scripts_are_not_part_of_the_bundle() {
        let m = mk_manifest(&["vmux", "vfm"]);
        assert!(!install_names(&m).contains(&"vplace".to_string()));
    }

    #[test]
    fn script_lists_every_name() {
        let names = install_names(&mk_manifest(&["vmux", "vfm"]));
        let script = remote_install_script(&names).unwrap();
        assert!(script.contains("for t in 'vmux' 'vfm'; do"));
    }

    #[test]
    fn script_refuses_an_unusable_name() {
        let names = vec!["vmux".to_string(), "a b".to_string()];
        assert!(remote_install_script(&names).is_err());
    }

    #[test]
    fn safe_name_check() {
        assert!(is_safe_name("vfm"));
        assert!(is_safe_name("vplace"));
        assert!(is_safe_name("v-tool_2.0"));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name("."));
        assert!(!is_safe_name(".."));
        assert!(!is_safe_name("-rf"));
        assert!(!is_safe_name("a b"));
        assert!(!is_safe_name("a'b"));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name("a$b"));
        assert!(!is_safe_name("a;rm -rf ~"));
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
