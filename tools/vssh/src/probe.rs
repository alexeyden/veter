//! Build and parse the remote probe.
//!
//! The probe runs a tightly-scoped `sh -c` script that emits
//! `KEY=value` lines between literal `VSSH<<<` / `VSSH>>>` sentinels.
//! Output before/after the sentinels (login banners, MOTD, noisy rc
//! files) is discarded. The manifest is left as a single-line JSON
//! string for the host to parse with `serde_json`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::dist::Manifest;
use crate::ssh::Master;

#[derive(Debug)]
pub struct ProbeResult {
    /// Raw `uname -m` (e.g. `aarch64`, `x86_64`).
    pub arch: String,
    /// Parsed manifest at `~/.local/share/veter-tools/manifest.json`,
    /// `None` if absent or unparseable.
    pub installed_manifest: Option<Manifest>,
    /// Whatever `command -v vmux` resolved to. `None` if no vmux is
    /// on PATH.
    pub vmux_path: Option<PathBuf>,
    /// Whether `$HOME` is writable to the connecting user.
    pub home_writable: bool,
}

const PROBE_SCRIPT: &str = r#"sh -c '
printf "VSSH<<<\n"
printf "ARCH=%s\n" "$(uname -m)"
if m=$(cat "$HOME/.local/share/veter-tools/manifest.json" 2>/dev/null); then
  printf "MANIFEST=%s\n" "$m"
else
  printf "MANIFEST=NONE\n"
fi
if p=$(command -v vmux 2>/dev/null); then
  printf "VMUX_PATH=%s\n" "$p"
else
  printf "VMUX_PATH=NONE\n"
fi
if [ -w "$HOME" ]; then printf "HOME_WRITABLE=yes\n"; else printf "HOME_WRITABLE=no\n"; fi
printf "VSSH>>>\n"
'"#;

pub fn probe(master: &Master) -> Result<ProbeResult> {
    let output = master.run(PROBE_SCRIPT)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("probe failed (exit {}): {}", output.status, stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse(&stdout)
}

fn parse(s: &str) -> Result<ProbeResult> {
    let start = s
        .find("VSSH<<<")
        .context("probe output missing VSSH<<< sentinel")?;
    let after_start = &s[start + "VSSH<<<".len()..];
    let end = after_start
        .find("VSSH>>>")
        .context("probe output missing VSSH>>> sentinel")?;
    let body = &after_start[..end];

    let mut arch: Option<String> = None;
    let mut manifest: Option<Manifest> = None;
    let mut vmux_path: Option<PathBuf> = None;
    let mut home_writable = false;

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "ARCH" => arch = Some(v.to_string()),
            "MANIFEST" => {
                if v != "NONE" {
                    match serde_json::from_str::<Manifest>(v) {
                        Ok(m) => manifest = Some(m),
                        Err(e) => log::warn!("remote manifest parse error: {e}"),
                    }
                }
            }
            "VMUX_PATH" => {
                if v != "NONE" {
                    vmux_path = Some(PathBuf::from(v));
                }
            }
            "HOME_WRITABLE" => home_writable = v == "yes",
            _ => log::debug!("unknown probe key: {k}"),
        }
    }

    Ok(ProbeResult {
        arch: arch.context("probe missing ARCH")?,
        installed_manifest: manifest,
        vmux_path,
        home_writable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_aarch64_remote() {
        let s = "Welcome to Ubuntu\nLast login: ...\n\
                 VSSH<<<\n\
                 ARCH=aarch64\n\
                 MANIFEST=NONE\n\
                 VMUX_PATH=NONE\n\
                 HOME_WRITABLE=yes\n\
                 VSSH>>>\n";
        let r = parse(s).unwrap();
        assert_eq!(r.arch, "aarch64");
        assert!(r.installed_manifest.is_none());
        assert!(r.vmux_path.is_none());
        assert!(r.home_writable);
    }

    #[test]
    fn installed_remote() {
        let s = "VSSH<<<\n\
                 ARCH=aarch64\n\
                 MANIFEST={\"version\":\"0.1.4\",\"arch\":\"aarch64-unknown-linux-musl\",\"sha256\":\"abc123\",\"tools\":[\"vmux\",\"vcat\"]}\n\
                 VMUX_PATH=/home/u/.local/bin/vmux\n\
                 HOME_WRITABLE=yes\n\
                 VSSH>>>";
        let r = parse(s).unwrap();
        let m = r.installed_manifest.unwrap();
        assert_eq!(m.version, "0.1.4");
        assert_eq!(m.sha256, "abc123");
        assert_eq!(r.vmux_path.unwrap(), PathBuf::from("/home/u/.local/bin/vmux"));
    }

    #[test]
    fn ignores_pre_and_post_sentinel_noise() {
        let s = "ARCH=fake-from-bashrc\n\
                 VSSH<<<\n\
                 ARCH=x86_64\n\
                 MANIFEST=NONE\n\
                 VMUX_PATH=NONE\n\
                 HOME_WRITABLE=yes\n\
                 VSSH>>>\n\
                 ARCH=fake-trailing\n";
        let r = parse(s).unwrap();
        assert_eq!(r.arch, "x86_64");
    }

    #[test]
    fn missing_sentinels_errors() {
        let s = "ARCH=x86_64\n";
        assert!(parse(s).is_err());
    }

    #[test]
    fn malformed_manifest_logs_and_continues() {
        let s = "VSSH<<<\n\
                 ARCH=aarch64\n\
                 MANIFEST={not valid json\n\
                 VMUX_PATH=NONE\n\
                 HOME_WRITABLE=yes\n\
                 VSSH>>>\n";
        let r = parse(s).unwrap();
        assert!(r.installed_manifest.is_none());
        assert_eq!(r.arch, "aarch64");
    }
}
