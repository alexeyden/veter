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
//! Phase 2 caveat: if the user passes a trailing remote command on
//! the vssh CLI, our master-open appends `true` after it (a harmless
//! second statement for any shell). Phase 3 will resolve host/user
//! via `ssh -G` and construct a clean master-open call that ignores
//! the trailing command.

use std::ffi::OsString;
use std::io::Read;
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

pub struct Master {
    ssh_args: Vec<OsString>,
    control_path: String,
}

impl Master {
    pub fn open(ssh_args: Vec<OsString>) -> Result<Self> {
        let control_path = control_path_template()?;
        let cp_opt = format!("ControlPath={control_path}");
        log::debug!("opening ControlMaster (ControlPath={control_path})");
        let status = Command::new("ssh")
            .args(["-o", "ControlMaster=auto"])
            .arg("-o")
            .arg(&cp_opt)
            .args(["-o", "ControlPersist=60"])
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
    /// Captures stderr for the error path; logs stdout at debug.
    pub fn run_with_stdin<R: Read>(&self, cmd: &str, mut reader: R) -> Result<()> {
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
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            log::debug!("remote: {line}");
        }
        Ok(())
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
