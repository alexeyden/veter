//! veterd — persistent veter session daemon.
//!
//! Usage:
//!
//! ```text
//!   veterd --foreground         # run the daemon in the foreground
//!   veterd new <name> [cmd ...] # create a new session
//!   veterd list                 # enumerate sessions
//!   veterd kill <name>          # tear down a session
//!   veterd kill-server          # stop the daemon and every session
//! ```
//!
//! The subcommands other than `--foreground` are short-lived CLI calls
//! that connect to the daemon socket at
//! `$XDG_RUNTIME_DIR/veterd/sock`. The `--foreground` mode runs the
//! daemon's accept loop in the current process. `attach` and the
//! `SCM_RIGHTS`-driven stdio handover land in task #6.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

mod daemon;
mod engines;
mod ipc;
mod session;

use ipc::{Request, Response};

#[derive(Parser, Debug)]
#[command(name = "veterd", about = "Persistent veter session daemon.")]
struct Cli {
    /// Run the daemon's accept loop in this process. Without this
    /// flag, the binary acts as a thin CLI talking to an already-
    /// running daemon over the socket.
    #[arg(long)]
    foreground: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Spawn a new session.
    New {
        name: String,
        /// Command and arguments to run inside the session. Defaults
        /// to `$SHELL` (or `/bin/sh`) when omitted.
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Enumerate the daemon's sessions.
    List,
    /// Terminate the named session.
    Kill { name: String },
    /// Shut the daemon down and tear down every session.
    KillServer,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.foreground {
        return daemon::run();
    }
    let cmd = cli
        .cmd
        .ok_or_else(|| anyhow!("missing subcommand (use --help)"))?;
    let req = match cmd {
        Cmd::New { name, argv } => Request::New { name, argv },
        Cmd::List => Request::List,
        Cmd::Kill { name } => Request::Kill { name },
        Cmd::KillServer => Request::KillServer,
    };
    let resp = call_daemon(req)?;
    render_response(resp)
}

/// Connect to the daemon socket and round-trip a single request.
fn call_daemon(req: Request) -> Result<Response> {
    let sock = daemon::socket_path();
    let stream = UnixStream::connect(&sock)
        .with_context(|| format!("connecting to daemon at {}", sock.display()))?;
    let mut writer = BufWriter::new(
        stream.try_clone().with_context(|| "clone unix stream")?,
    );
    let mut reader = BufReader::new(stream);
    req.write_to(&mut writer).context("writing request")?;
    Response::read_from(&mut reader).context("reading response")
}

fn render_response(resp: Response) -> Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Sessions(list) => {
            if list.is_empty() {
                println!("(no sessions)");
                return Ok(());
            }
            println!(
                "{:<24}  {:>10}  {:<5}  {}",
                "NAME", "AGE", "ALIVE", "ATTACHED"
            );
            for s in list {
                println!(
                    "{:<24}  {:>10}  {:<5}  {}",
                    s.name,
                    format_age(s.age_secs),
                    if s.alive { "yes" } else { "no" },
                    if s.attached { "yes" } else { "no" },
                );
            }
            Ok(())
        }
        Response::Err(msg) => bail!("{msg}"),
    }
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

