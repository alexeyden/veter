use std::ffi::{CString, OsString};
use std::os::unix::ffi::OsStrExt;

use anyhow::{bail, Context, Result};
use nix::unistd::execvp;

/// Replace this process with `ssh <ssh_args...>`. Returns only on error.
///
/// Wires up two things on top of the user's ssh args:
///   * `-o ControlPath=…` so the interactive shell reuses the
///     already-authenticated ControlMaster.
///   * `-o Compression=yes` (see [`crate::ssh::compression_opts`]), in
///     case the master is gone by now and this ssh connects afresh.
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

    for o in crate::ssh::compression_opts(ssh_args) {
        argv.push(CString::new(*o).unwrap());
    }

    let inject = fix_path && crate::ssh::command_start(ssh_args).is_none();
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
