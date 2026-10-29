//! SSH ControlMaster lifecycle.
//!
//! On `Master::open`, vssh runs a no-op remote command with
//! `ControlMaster=auto` + `ControlPersist=60`. The first call performs
//! interactive auth (password/2FA prompts land on the user's tty);
//! when ssh exits, the multiplexer process stays in the background
//! for 60 seconds. Subsequent `Master::run` calls and the final
//! interactive `exec ssh` reuse the same socket without re-auth.
//!
//! Two cleanup paths:
//!   * Normal: `Drop` runs `ssh -O exit` to tear the master down
//!     immediately when the Master goes out of scope (typically on an
//!     error path).
//!   * Handoff: `Master::disarm` consumes the struct without running
//!     `Drop`, so the master persists for the interactive ssh that
//!     replaces vssh via `execvp`. `ControlPersist=60` is the
//!     backstop if the interactive ssh never starts.
//!
//! A remote command on the vssh command line is the interactive ssh's
//! alone: the master and every call through it run with the options and
//! the destination only (`command_start`), so a probe or an install is
//! never handed to the user's command as its arguments.

use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};

/// Build the `-o ControlPath=...` template using ssh's `%C` token
/// (hash of host+port+user). Keeps the resulting Unix socket path
/// short enough to stay under Linux's 108-byte sockaddr_un limit.
fn control_path_template() -> Result<String> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let cache_dir = PathBuf::from(home).join(".cache").join("vssh");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating {}", cache_dir.display()))?;
    let template = format!("{}/%C", cache_dir.display());
    Ok(template)
}

/// `-o Compression=yes`, unless the user already chose. OpenSSH defaults
/// to no compression, and what a veter session sends compresses very
/// well: chrome redraws repeat themselves (a vmux tab switch is ~2 KB
/// raw and ~40 B through ssh's zlib stream once it has been seen), as
/// does ordinary shell output. It has to be on the master: every
/// session multiplexed over it shares the master's transport.
pub fn compression_opts(ssh_args: &[OsString]) -> &'static [&'static str] {
    let chosen = ssh_args.iter().any(|a| {
        let a = a.to_string_lossy();
        a == "-C" || a.to_ascii_lowercase().contains("compression=")
    });
    if chosen {
        &[]
    } else {
        &["-o", "Compression=yes"]
    }
}

/// Lightweight ssh-style argv scanner. Returns where the remote command
/// starts in the user's `ssh_args`, or `None` when there is none — the
/// args before it are the options and the destination, which is what
/// vssh's own calls through the master need: anything after the host
/// is a command, and a command of the user's in front of the probe
/// turns `sh -c '…'` into arguments of it. We track ssh's
/// short options that take a value so we don't mistake an option's
/// argument for the host or the command.
///
/// We don't need to be 100% accurate on every obscure ssh flag — the
/// failure mode for `exec_ssh` is "we don't inject when we could have"
/// (user manually fixes PATH) or "we inject when we shouldn't" (user
/// passes `--no-fix-path`). The single goal is to make the common cases
/// (`vssh host`, `vssh -p 2222 user@host`, `vssh -J jump host`,
/// `vssh host my-cmd ...`) all work right.
pub fn command_start(ssh_args: &[OsString]) -> Option<usize> {
    // OpenSSH short options that consume the *next* argv as their
    // value. Sourced from ssh(1) on OpenSSH 9.x.
    const TAKES_ARG: &[u8] = b"BbcDEeFIiJLlmOoPpQRSWw";

    let mut i = 0;
    while i < ssh_args.len() {
        let arg = ssh_args[i].as_bytes();
        if arg == b"--" {
            // After `--`, the next arg is the host, anything after is
            // the command.
            return (i + 2 < ssh_args.len()).then_some(i + 2);
        }
        if arg.len() == 2 && arg[0] == b'-' && TAKES_ARG.contains(&arg[1]) {
            i += 2;
            continue;
        }
        if arg.len() > 1 && arg[0] == b'-' {
            // Standalone short flag (`-t`, `-v`, `-4`, …) or short
            // option fused with its value (`-p2222`). Both consume
            // only this one argv element.
            i += 1;
            continue;
        }
        // First positional: this is the host. A command follows iff
        // there's anything after.
        return (i + 1 < ssh_args.len()).then_some(i + 1);
    }
    None
}

pub struct Master {
    ssh_args: Vec<OsString>,
    control_path: String,
}

impl Master {
    /// `ssh_args` are the user's, remote command and all; the master
    /// keeps only the options and the destination (see [`command_start`]).
    pub fn open(mut ssh_args: Vec<OsString>) -> Result<Self> {
        if let Some(start) = command_start(&ssh_args) {
            ssh_args.truncate(start);
        }
        let control_path = control_path_template()?;
        let cp_opt = format!("ControlPath={control_path}");
        log::debug!("opening ControlMaster (ControlPath={control_path})");
        let status = Command::new("ssh")
            .args(["-o", "ControlMaster=auto"])
            .arg("-o")
            .arg(&cp_opt)
            .args(["-o", "ControlPersist=60"])
            .args(compression_opts(&ssh_args))
            .args(&ssh_args)
            .arg("true")
            .status()
            .context("spawning ssh to open ControlMaster")?;
        if !status.success() {
            bail!("ssh master open failed (exit: {status})");
        }
        log::debug!("ControlMaster open");
        Ok(Self {
            ssh_args,
            control_path,
        })
    }

    /// Run a remote command via the existing master. `BatchMode=yes`
    /// makes the call fail fast (rather than re-prompting) if the
    /// master happens to be gone.
    pub fn run(&self, cmd: &str) -> Result<Output> {
        let cp_opt = format!("ControlPath={}", self.control_path);
        log::debug!("master.run: {cmd}");
        let output = Command::new("ssh")
            .arg("-o")
            .arg(&cp_opt)
            .args(["-o", "BatchMode=yes"])
            .args(&self.ssh_args)
            .arg(cmd)
            .output()
            .context("spawning ssh via ControlMaster")?;
        Ok(output)
    }

    /// Run a remote command with `reader` piped to its stdin.
    /// Captures stderr for the error path; logs stdout at debug and
    /// returns it, since a remote script can succeed and still have
    /// something to report (see `install::warn_about_missing`).
    pub fn run_with_stdin<R: Read>(&self, cmd: &str, mut reader: R) -> Result<String> {
        let cp_opt = format!("ControlPath={}", self.control_path);
        log::debug!("master.run_with_stdin: {cmd}");
        let mut child = Command::new("ssh")
            .arg("-o")
            .arg(&cp_opt)
            .args(["-o", "BatchMode=yes"])
            .args(&self.ssh_args)
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ssh for stdin command")?;
        let mut stdin = child.stdin.take().context("ssh stdin missing")?;
        std::io::copy(&mut reader, &mut stdin).context("copying to ssh stdin")?;
        drop(stdin);
        let output = child.wait_with_output().context("waiting for ssh")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "remote command failed (exit {}): {}",
                output.status,
                stderr.trim()
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        for line in stdout.lines() {
            log::debug!("remote: {line}");
        }
        Ok(stdout)
    }

    /// Consume the Master without running `Drop`. Returns the
    /// resolved ControlPath template so the caller can pass
    /// `-o ControlPath=…` to the next ssh invocation (typically the
    /// interactive handoff via `execvp`). ControlPersist=60 catches
    /// us if that handoff never happens.
    pub fn disarm(self) -> String {
        let cp = self.control_path.clone();
        std::mem::forget(self);
        cp
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        let cp_opt = format!("ControlPath={}", self.control_path);
        log::debug!("closing ControlMaster");
        let _ = Command::new("ssh")
            .args(["-O", "exit"])
            .arg("-o")
            .arg(&cp_opt)
            .args(&self.ssh_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> &'static [&'static str] {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        compression_opts(&args)
    }

    #[test]
    fn compression_is_on_unless_the_user_chose() {
        assert_eq!(opts(&["host"]), ["-o", "Compression=yes"]);
        assert_eq!(
            opts(&["-c", "aes128-ctr", "host"]),
            ["-o", "Compression=yes"]
        );
        assert!(opts(&["-C", "host"]).is_empty());
        assert!(opts(&["-o", "Compression=no", "host"]).is_empty());
        assert!(opts(&["-ocompression=no", "host"]).is_empty());
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn assert_no_command(args: &[OsString]) {
        assert_eq!(command_start(args), None, "{args:?}");
    }

    fn assert_command(args: &[OsString]) {
        assert!(command_start(args).is_some(), "{args:?}");
    }

    #[test]
    fn the_command_starts_after_the_host() {
        assert_eq!(command_start(&os(&["host", "echo", "hi"])), Some(1));
        assert_eq!(command_start(&os(&["-p", "22", "host", "uptime"])), Some(3));
        assert_eq!(command_start(&os(&["--", "host", "cmd"])), Some(2));
    }

    #[test]
    fn bare_host_has_no_command() {
        assert_no_command(&os(&["host"]));
        assert_no_command(&os(&["user@host"]));
    }

    #[test]
    fn option_with_value_then_bare_host() {
        assert_no_command(&os(&["-p", "2222", "host"]));
        assert_no_command(&os(&["-J", "jump", "host"]));
        assert_no_command(&os(&["-i", "/tmp/key", "host"]));
        assert_no_command(&os(&["-o", "User=foo", "-L", "8080:localhost:80", "host"]));
    }

    #[test]
    fn standalone_flags_then_host() {
        assert_no_command(&os(&["-t", "host"]));
        assert_no_command(&os(&["-v", "-v", "-4", "host"]));
    }

    #[test]
    fn fused_short_flag_with_value() {
        // `-p2222` style — single argv, no following value.
        assert_no_command(&os(&["-p2222", "host"]));
    }

    #[test]
    fn host_then_command() {
        assert_command(&os(&["host", "ls"]));
        assert_command(&os(&["host", "ls", "-la"]));
        assert_command(&os(&["-p", "22", "host", "uptime"]));
    }

    #[test]
    fn double_dash_separator() {
        // `vssh -- host cmd` — `--` ends options.
        assert_command(&os(&["--", "host", "cmd"]));
        assert_no_command(&os(&["--", "host"]));
    }
}
