//! Launching the program that opens a file.
//!
//! Two spawn modes, matching [`crate::config::OpenRule::terminal`]:
//!
//! * [`spawn_detached`] — fire-and-forget in its own session (`setsid`,
//!   null stdio), for GUI apps that outlive the browser. Mirrors veter's
//!   `run_selection_command`.
//! * [`run_in_terminal`] — inherit this process's stdio and block until
//!   the child exits, for programs that take over the tty (an editor,
//!   `vplay`). The caller is responsible for tearing down and restoring
//!   the terminal around the call — see `main`'s suspend/resume.
//!
//! Both run the command through `$SHELL -c`, so `$EDITOR`, pipes and
//! globs in the configured command work.

use std::io;
use std::process::{Command, ExitStatus, Stdio};

/// `$SHELL`, or `/bin/sh`.
fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

/// Launch `command` detached: its own session, null stdio, not waited
/// on (a short reaper thread reaps it so it doesn't linger as a zombie).
/// Never blocks and never touches the terminal.
pub fn spawn_detached(command: &str) -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(shell());
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid() is async-signal-safe and touches no shared state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Run `command` with this process's stdin/stdout/stderr inherited, and
/// block until it exits. The child gets the controlling terminal, so the
/// caller must first drop vfm's raw mode / alt-screen / VGE UI and
/// restore them afterward.
pub fn run_in_terminal(command: &str) -> io::Result<ExitStatus> {
    Command::new(shell())
        .arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
}
