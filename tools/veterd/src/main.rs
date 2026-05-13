//! veterd — persistent veter session daemon.
//!
//! Usage:
//!
//! ```text
//!   veterd --foreground         # run the daemon in the foreground (debug)
//!   veterd start                # spawn a detached daemon in the background
//!   veterd new <name> [cmd ...] # create a new session (auto-spawns)
//!   veterd attach <name>        # connect this terminal to a session
//!   veterd list                 # enumerate sessions
//!   veterd kill <name>          # tear down a session
//!   veterd kill-server          # stop the daemon and every session
//! ```
//!
//! The subcommands other than `--foreground` and `start` are short-lived
//! CLI calls that connect to the daemon socket at
//! `$XDG_RUNTIME_DIR/veterd/sock`. They auto-spawn a detached daemon if
//! no socket is responding — the user experience is tmux-like: you can
//! `veterd new foo` cold and the daemon comes up to host it. `--foreground`
//! is the explicit no-detach mode for debugging.
//!
//! `attach` hands the caller's stdin/stdout fds to the daemon over
//! `SCM_RIGHTS`; the daemon then writes a state snapshot and splices
//! the renderer to the inner PTY for the duration of the attach.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

mod attach;
mod daemon;
mod engines;
mod fdpass;
mod ipc;
mod probe;
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
    /// Ensure the daemon is running in the background. Returns
    /// immediately once the socket is responding. Does nothing if a
    /// daemon is already up.
    Start,
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
    /// Attach the current terminal to a session. Blocks until the
    /// daemon acknowledges; the daemon then owns this process's stdio
    /// fds for the duration of the attach.
    Attach { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.foreground {
        return daemon::run();
    }
    let cmd = cli
        .cmd
        .ok_or_else(|| anyhow!("missing subcommand (use --help)"))?;

    // Auto-spawn policy (tmux-style): commands that need a live daemon
    // bring one up if the socket isn't responding. `kill-server` is the
    // exception — spawning a daemon just to shut it down is silly, and
    // the error message is clearer when we report "no daemon running".
    let spawn_policy = match &cmd {
        Cmd::Start | Cmd::New { .. } | Cmd::List | Cmd::Kill { .. } | Cmd::Attach { .. } => {
            SpawnPolicy::Auto
        }
        Cmd::KillServer => SpawnPolicy::None,
    };
    if spawn_policy == SpawnPolicy::Auto {
        ensure_daemon_running().context("ensuring daemon is running")?;
    }

    if let Cmd::Start = cmd {
        // ensure_daemon_running already brought it up (or confirmed it
        // was already up); nothing else to do.
        return Ok(());
    }
    if let Cmd::Attach { name } = cmd {
        return run_attach(&name);
    }
    let req = match cmd {
        Cmd::New { name, argv } => Request::New { name, argv },
        Cmd::List => Request::List,
        Cmd::Kill { name } => Request::Kill { name },
        Cmd::KillServer => Request::KillServer,
        Cmd::Start | Cmd::Attach { .. } => unreachable!("handled above"),
    };
    let resp = call_daemon(req)?;
    render_response(resp)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpawnPolicy {
    Auto,
    None,
}

/// Check whether `$XDG_RUNTIME_DIR/veterd/sock` is responding; if not,
/// fork off a detached daemon process and wait briefly for the socket
/// to appear. Idempotent and race-tolerant — concurrent callers that
/// each try to spawn will each get one process; one of them wins the
/// bind, the others exit on "address in use" almost immediately, and
/// the surviving daemon is what answers everybody's retried connect.
fn ensure_daemon_running() -> Result<()> {
    let sock = daemon::socket_path();
    if UnixStream::connect(&sock).is_ok() {
        return Ok(());
    }

    let log_path = daemon_log_path();
    spawn_detached_daemon(&log_path)
        .with_context(|| "spawning detached daemon")?;

    // Poll for the socket to come up. 50ms × 60 = 3s deadline, which
    // is generous even on a slow Raspberry Pi.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if UnixStream::connect(&sock).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "daemon failed to start within 3s; check the log at {}",
        log_path.display()
    )
}

fn daemon_log_path() -> PathBuf {
    let sock = daemon::socket_path();
    let dir = sock
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("veterd.log")
}

/// Re-exec ourselves with `--foreground` in a detached background
/// process. The child:
///   - has stdio redirected to `/dev/null` (stdin) and the log file
///     (stdout/stderr), so it doesn't fight for the calling terminal;
///   - calls `setsid(2)` via `pre_exec` so it leaves the parent's
///     process group and won't be reaped if the CLI's controlling
///     tty hangs up.
/// We `.spawn()` without ever calling `.wait()` so the child becomes
/// our orphan when the CLI exits.
fn spawn_detached_daemon(log_path: &std::path::Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating daemon log directory {}", parent.display())
        })?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening daemon log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| "cloning log fd for stderr")?;
    let exe =
        std::env::current_exe().with_context(|| "discovering own binary path")?;

    let mut cmd = Command::new(exe);
    cmd.arg("--foreground")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // SAFETY: pre_exec runs in the child between fork and exec; it
    // must only call async-signal-safe functions. setsid(2) is.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::other)?;
            Ok(())
        });
    }
    let _child = cmd.spawn().with_context(|| "spawn daemon")?;
    // Deliberately don't `.wait()` — the child is detached and we
    // want it to outlive this CLI invocation.
    Ok(())
}

/// Connect to the daemon, send `Attach { name }`, hand stdin/stdout
/// over `SCM_RIGHTS`, and wait for the daemon's ack before exiting.
/// The daemon now owns the stdio fds; the CLI exit is intentional.
fn run_attach(name: &str) -> Result<()> {
    use std::os::fd::AsRawFd;

    let sock = daemon::socket_path();
    let mut stream = UnixStream::connect(&sock)
        .with_context(|| format!("connecting to daemon at {}", sock.display()))?;
    Request::Attach { name: name.to_string() }
        .write_to(&mut stream)
        .context("writing attach request")?;

    let stdin_fd = std::io::stdin().as_raw_fd();
    let stdout_fd = std::io::stdout().as_raw_fd();
    fdpass::send_stdio(&stream, stdin_fd, stdout_fd)
        .context("sending stdio fds to daemon")?;

    let resp = Response::read_from(&mut stream).context("reading attach response")?;
    match resp {
        Response::Ok => Ok(()),
        Response::Err(msg) => bail!("{msg}"),
        Response::Sessions(_) => bail!("daemon returned unexpected response variant"),
    }
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

