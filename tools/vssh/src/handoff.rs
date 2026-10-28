use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;

use anyhow::{bail, Context, Result};
use nix::unistd::execvp;

/// Replace this process with `ssh <ssh_args...>`. Returns only on error.
///
/// Wires up two things on top of the user's ssh args:
///   * `-o ControlPath=…` so the interactive shell reuses the
///     already-authenticated ControlMaster.
///   * If `fix_path` is set AND the user did not pass their own
///     remote command, appends `-t exec env PATH=$HOME/.local/bin:$PATH
///     "$SHELL" -l` so the remote login shell sees `~/.local/bin` on
///     PATH (where veter-tools were installed). When the user did
///     pass a command, we leave their argv alone — injecting around
///     their command would collide with their intent.
pub fn exec_ssh(
    ssh_args: &[OsString],
    control_path: Option<&str>,
    fix_path: bool,
) -> Result<()> {
    let argv0 = CString::new("ssh").unwrap();
    let mut argv: Vec<CString> = Vec::with_capacity(ssh_args.len() + 5);
    argv.push(argv0.clone());

    if let Some(cp) = control_path {
        argv.push(CString::new("-o").unwrap());
        argv.push(
            CString::new(format!("ControlPath={cp}"))
                .context("ControlPath contains interior NUL")?,
        );
    }

    let inject = fix_path && !user_provided_remote_command(ssh_args);
    if inject {
        argv.push(CString::new("-t").unwrap());
    }
    for a in ssh_args {
        argv.push(
            CString::new(a.as_bytes())
                .with_context(|| format!("ssh arg contains interior NUL: {a:?}"))?,
        );
    }
    if inject {
        argv.push(
            CString::new(r#"exec env PATH="$HOME/.local/bin:$PATH" "$SHELL" -l"#).unwrap(),
        );
    }

    log::debug!(
        "execvp ssh argv={:?} (cp={:?}, fix_path_injected={})",
        argv,
        control_path,
        inject
    );
    let err = execvp(&argv0, &argv).unwrap_err();
    bail!("exec ssh failed: {err} (is ssh on $PATH?)")
}

/// Lightweight ssh-style argv scanner. Returns true iff the user's
/// `ssh_args` contain a remote command after the host. We track ssh's
/// short options that take a value so we don't mistake an option's
/// argument for the host or the command.
///
/// We don't need to be 100% accurate on every obscure ssh flag — the
/// failure mode is "we don't inject when we could have" (user manually
/// fixes PATH) or "we inject when we shouldn't" (user passes
/// `--no-fix-path`). The single goal is to make the common cases
/// (`vssh host`, `vssh -p 2222 user@host`, `vssh -J jump host`,
/// `vssh host my-cmd ...`) all work right.
fn user_provided_remote_command(ssh_args: &[OsString]) -> bool {
    // OpenSSH short options that consume the *next* argv as their
    // value. Sourced from ssh(1) on OpenSSH 9.x.
    const TAKES_ARG: &[u8] = b"BbcDEeFIiJLlmOoPpQRSWw";

    let mut i = 0;
    while i < ssh_args.len() {
        let arg = ssh_args[i].as_bytes();
        if arg == b"--" {
            // After `--`, the next arg is the host, anything after is
            // the command.
            return i + 2 < ssh_args.len();
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
        return i + 1 < ssh_args.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn bare_host_has_no_command() {
        assert!(!user_provided_remote_command(&os(&["host"])));
        assert!(!user_provided_remote_command(&os(&["user@host"])));
    }

    #[test]
    fn option_with_value_then_bare_host() {
        assert!(!user_provided_remote_command(&os(&["-p", "2222", "host"])));
        assert!(!user_provided_remote_command(&os(&["-J", "jump", "host"])));
        assert!(!user_provided_remote_command(&os(&["-i", "/tmp/key", "host"])));
        assert!(!user_provided_remote_command(&os(&[
            "-o", "User=foo", "-L", "8080:localhost:80", "host"
        ])));
    }

    #[test]
    fn standalone_flags_then_host() {
        assert!(!user_provided_remote_command(&os(&["-t", "host"])));
        assert!(!user_provided_remote_command(&os(&["-v", "-v", "-4", "host"])));
    }

    #[test]
    fn fused_short_flag_with_value() {
        // `-p2222` style — single argv, no following value.
        assert!(!user_provided_remote_command(&os(&["-p2222", "host"])));
    }

    #[test]
    fn host_then_command() {
        assert!(user_provided_remote_command(&os(&["host", "ls"])));
        assert!(user_provided_remote_command(&os(&["host", "ls", "-la"])));
        assert!(user_provided_remote_command(&os(&[
            "-p", "22", "host", "uptime"
        ])));
    }

    #[test]
    fn double_dash_separator() {
        // `vssh -- host cmd` — `--` ends options.
        assert!(user_provided_remote_command(&os(&["--", "host", "cmd"])));
        assert!(!user_provided_remote_command(&os(&["--", "host"])));
    }
}
