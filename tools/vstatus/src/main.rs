//! `vstatus` — set the status line of the pane you are running in.
//!
//! A status item is a keyed line of text, optionally with a progress
//! figure, that the pane's multiplexer paints with the pane (vmux: at
//! the right of its title strip, and as a badge on its tab when the pane
//! is out of sight). It travels as PRT `SetStatus`
//! (`doc/portal-extension.md` §9.4).
//!
//! The intended caller is not the pane's foreground program but
//! something running beside it — an agent's hooks, a download script —
//! so this tool only ever *writes*. Every envelope goes out with the
//! no-response sentinel: a reply would land in the pane's input queue
//! and be read by the foreground program as keystrokes. For the same
//! reason there is no probe; a terminal that does not speak PRT drops an
//! APC string, and one that does needs nothing the probe negotiates.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use prt_protocol::command::{Command, StatusLevel, StatusOp, StatusProgress};
use prt_protocol::encode::build_envelope;
use prt_protocol::frame::{MAX_STATUS_KEY_BYTES, MAX_STATUS_TEXT_BYTES, REQ_ID_NO_RESPONSE};

#[derive(Parser, Debug)]
#[command(
    version = veter_version::long_version(env!("CARGO_PKG_VERSION")),
    about = "Set the status line of the veter pane you are running in.",
    after_help = "Items are keyed, so independent writers in one pane don't overwrite each \
                  other. Namespace your keys (claude.tool, claude.state) and clear them \
                  together with `clear --prefix claude.`."
)]
struct Cli {
    /// Pane tty to write to. Defaults to `$VMUX_PANE_TTY`, then `/dev/tty`.
    #[arg(long, global = true)]
    tty: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Set (or replace) a status item.
    Set {
        #[command(flatten)]
        key: KeyArg,
        #[command(flatten)]
        look: Look,
        /// The text. Several words are joined with spaces; control
        /// characters become spaces and anything past 256 bytes is cut.
        text: Vec<String>,
    },
    /// Remove a status item.
    Clear {
        #[command(flatten)]
        key: KeyArg,
        /// Treat the key as a prefix: remove every item whose key starts
        /// with it.
        #[arg(long)]
        prefix: bool,
        /// Remove every item in the pane.
        #[arg(long, conflicts_with_all = ["key", "prefix"])]
        all: bool,
    },
    /// Run a command with a status item up while it runs.
    ///
    /// The item is removed when the command succeeds. When it fails the
    /// item stays, at error level with the exit status, until the key is
    /// set or cleared again. Exits with the command's status.
    Run {
        /// Key of the item.
        #[arg(short, long, default_value = "run")]
        key: String,
        /// Text of the item. Defaults to the command line.
        #[arg(short, long)]
        text: Option<String>,
        /// Show an indeterminate progress bar while it runs.
        #[arg(long)]
        spin: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct KeyArg {
    /// Key of the item. Empty is the pane's default item.
    #[arg(short, long, default_value = "")]
    key: String,
}

#[derive(Args, Debug)]
struct Look {
    #[arg(short, long, value_enum, default_value_t = Level::Info)]
    level: Level,
    /// Progress in percent, 0–100.
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(0..=100))]
    progress: Option<u8>,
    /// Indeterminate progress (a busy bar with no figure).
    #[arg(long, conflicts_with = "progress")]
    spin: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Level {
    Info,
    Warn,
    Error,
}

impl From<Level> for StatusLevel {
    fn from(l: Level) -> Self {
        match l {
            Level::Info => StatusLevel::Info,
            Level::Warn => StatusLevel::Warn,
            Level::Error => StatusLevel::Error,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("vstatus: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let out = Out::open(cli.tty)?;
    match cli.cmd {
        Cmd::Set { key, look, text } => {
            out.send(set_op(&key.key, &look, &text.join(" "))?)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Clear { key, prefix, all } => {
            let op = if all {
                StatusOp::Remove { key: String::new(), prefix: true }
            } else {
                StatusOp::Remove { key: checked_key(&key.key)?, prefix }
            };
            out.send(op)?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Run { key, text, spin, command } => run_command(&out, &key, text, spin, &command),
    }
}

fn set_op(key: &str, look: &Look, text: &str) -> Result<StatusOp> {
    let progress = if look.spin {
        Some(StatusProgress::Indeterminate)
    } else {
        look.progress.map(StatusProgress::Percent)
    };
    Ok(StatusOp::Set {
        key: checked_key(key)?,
        level: look.level.into(),
        progress,
        text: one_line(text),
    })
}

/// Keys are identifiers the caller chose, so a bad one is an error
/// rather than something to quietly repair: a repaired key would no
/// longer match the `clear` meant for it.
fn checked_key(key: &str) -> Result<String> {
    anyhow::ensure!(
        key.len() <= MAX_STATUS_KEY_BYTES,
        "key is longer than {MAX_STATUS_KEY_BYTES} bytes"
    );
    anyhow::ensure!(!key.chars().any(char::is_control), "key contains control characters");
    Ok(key.to_owned())
}

/// Text is whatever the caller had to hand — a multi-line shell command
/// from a hook, a file name — so it is made to fit rather than refused:
/// whitespace runs (newlines included) collapse to one space, other
/// controls are dropped, and the result is cut at a character boundary
/// to the §9.4 limit, with an ellipsis to show it was.
fn one_line(text: &str) -> String {
    let mut s = String::with_capacity(text.len().min(MAX_STATUS_TEXT_BYTES));
    for word in text.split(|c: char| c.is_whitespace()).filter(|w| !w.is_empty()) {
        if !s.is_empty() {
            s.push(' ');
        }
        s.extend(word.chars().filter(|c| !c.is_control()));
    }
    if s.len() <= MAX_STATUS_TEXT_BYTES {
        return s;
    }
    let mut cut = MAX_STATUS_TEXT_BYTES - '…'.len_utf8();
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push('…');
    s
}

/// Where envelopes go, or nowhere.
///
/// Without a sign that a veter pane is on the other end, writing is
/// skipped and every command succeeds as a no-op, so a hook installed
/// globally does nothing in another terminal. The signs: an explicit
/// `--tty`, vmux's `$VMUX_PANE_TTY`, veter's `$VETER` — or an SSH
/// session, since neither variable crosses the hop while the pane at the
/// local end may well be veter's, and a terminal that isn't drops an APC
/// string unseen.
struct Out(Option<(PathBuf, std::fs::File)>);

impl Out {
    fn open(explicit: Option<PathBuf>) -> Result<Self> {
        let set = |v: &str| std::env::var_os(v).is_some_and(|v| !v.is_empty());
        let veter_pane = explicit.is_some()
            || set("VMUX_PANE_TTY")
            || set("VETER")
            || set("SSH_CONNECTION")
            || set("SSH_TTY");
        if !veter_pane {
            return Ok(Self(None));
        }
        let path = vge_render::tty::pane_tty(explicit.as_deref())?;
        // Append, which a tty ignores, so that a regular file standing
        // in for one (`--tty`, the tests) collects every envelope.
        let tty = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Self(Some((path, tty))))
    }

    /// One envelope, one `write`. The pane's foreground program writes
    /// to the same tty, and a single write is not interleaved with
    /// another writer's.
    fn send(&self, op: StatusOp) -> Result<()> {
        let Some((path, tty)) = &self.0 else { return Ok(()) };
        let envelope = build_envelope(&[(Command::SetStatus(op), REQ_ID_NO_RESPONSE)]);
        (&*tty).write_all(&envelope).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

fn run_command(
    out: &Out,
    key: &str,
    text: Option<String>,
    spin: bool,
    command: &[String],
) -> Result<ExitCode> {
    let key = checked_key(key)?;
    let label = one_line(&text.unwrap_or_else(|| command.join(" ")));
    let look = Look { level: Level::Info, progress: None, spin };
    out.send(set_op(&key, &look, &label)?)?;

    let mut child = match std::process::Command::new(&command[0]).args(&command[1..]).spawn() {
        Ok(c) => c,
        Err(e) => {
            out.send(StatusOp::Remove { key, prefix: false })?;
            return Err(e).with_context(|| format!("run {}", command[0]));
        }
    };
    // The terminal's Ctrl+C and Ctrl+\ reach the child too, being in the
    // same foreground process group. Ignore them here, as `system(3)`
    // does, so the item is still cleaned up after the child has handled
    // them. Only after the spawn: an ignored disposition survives exec.
    // SAFETY: setting a signal disposition to SIG_IGN.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
    let status = child.wait().context("wait for the command")?;

    let code = match (status.code(), std::os::unix::process::ExitStatusExt::signal(&status)) {
        (Some(c), _) => c,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    };
    if code == 0 {
        out.send(StatusOp::Remove { key, prefix: false })?;
    } else {
        out.send(StatusOp::Set {
            key,
            level: StatusLevel::Error,
            progress: None,
            text: one_line(&format!("{label} — exit {code}")),
        })?;
    }
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_collapses_whitespace_and_drops_controls() {
        assert_eq!(one_line("cargo test \\\n  -p vmux\t--lib"), "cargo test \\ -p vmux --lib");
        assert_eq!(one_line("a\x1b[2Jb\x07"), "a[2Jb");
        assert_eq!(one_line("  \n "), "");
    }

    #[test]
    fn one_line_cuts_at_a_char_boundary_within_the_limit() {
        let long = "ж".repeat(MAX_STATUS_TEXT_BYTES);
        let s = one_line(&long);
        assert!(s.len() <= MAX_STATUS_TEXT_BYTES);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn what_vstatus_builds_is_what_the_host_accepts() {
        let look = Look { level: Level::Warn, progress: Some(42), spin: false };
        let op = set_op("dl", &look, &"x\n".repeat(400)).unwrap();
        let body = prt_protocol::encode::set_status_body(&op);
        let parsed = prt_protocol::command::parse(prt_protocol::frame::CMD_SET_STATUS, &body);
        assert!(matches!(parsed, Ok(Command::SetStatus(back)) if back == op));
    }

    #[test]
    fn bad_keys_are_refused_not_repaired() {
        assert!(checked_key("claude.tool").is_ok());
        assert!(checked_key("a\nb").is_err());
        assert!(checked_key(&"k".repeat(MAX_STATUS_KEY_BYTES + 1)).is_err());
    }
}
