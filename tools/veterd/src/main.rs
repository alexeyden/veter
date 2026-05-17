//! veterd — per-session veter daemon.
//!
//! Usage:
//!
//! ```text
//!   veterd new [-a] NAME [argv ...]   # spawn a new session; -a attaches afterwards
//!   veterd attach NAME                # attach the calling terminal
//!   veterd list                       # enumerate live sessions
//!   veterd kill NAME                  # tear down NAME
//! ```
//!
//! The subcommands are short-lived CLI calls that talk to per-session
//! Unix sockets at `$XDG_RUNTIME_DIR/veterd/<NAME>.sock`. Each
//! session is its own process; `new` re-execs this binary with the
//! hidden `--session NAME [argv...]` flag inside a double-forked
//! detached child. `--foreground-session NAME [argv...]` runs the
//! same session backend but without the daemonisation step, for
//! debugging.
//!
//! `attach` hands the caller's stdio fds to the session over
//! `SCM_RIGHTS`; the session writes a VSS snapshot and splices
//! bytes between the renderer and the inner PTY for the duration
//! of the attach.

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

mod attach;
mod engines;
mod fdpass;
mod ipc;
mod probe;
mod runtime;
mod session;

use ipc::{Request, Response, SessionInfo};

#[derive(Parser, Debug)]
#[command(name = "veterd", about = "Per-session veter daemon.")]
struct Cli {
    /// Internal: run the per-session backend in this process,
    /// detached (used by `new`). The caller is responsible for
    /// having double-forked us and redirected stdio to a log file.
    #[arg(long, value_name = "NAME", hide = true)]
    session: Option<String>,

    /// Internal: run the per-session backend in this process,
    /// foreground (no detachment). Useful for `gdb` / `cargo run`.
    #[arg(long, value_name = "NAME", hide = true, conflicts_with = "session")]
    foreground_session: Option<String>,

    /// argv for `--session` / `--foreground-session`. Ignored
    /// otherwise.
    #[arg(trailing_var_arg = true, hide = true)]
    session_argv: Vec<String>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Spawn a new session.
    New {
        /// Attach the calling terminal once the session is up.
        #[arg(short, long)]
        attach: bool,
        /// Session name.
        name: String,
        /// Command and arguments to run inside the session.
        /// Defaults to `$SHELL` (or `/bin/sh` as a final fallback)
        /// when omitted.
        #[arg(trailing_var_arg = true)]
        argv: Vec<String>,
    },
    /// Attach the calling terminal to NAME. Errors if the session
    /// doesn't exist.
    Attach { name: String },
    /// Enumerate live sessions.
    List,
    /// Tear down NAME (SIGTERM its inner program, unlink its socket).
    /// Idempotent on a missing session.
    Kill { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Internal session-backend modes — invoked by `new` re-execing
    // self. These never return through the CLI dispatcher below.
    if let Some(name) = cli.session {
        return session::run(name, cli.session_argv);
    }
    if let Some(name) = cli.foreground_session {
        return session::run(name, cli.session_argv);
    }

    let cmd = cli
        .cmd
        .ok_or_else(|| anyhow!("missing subcommand (use --help)"))?;
    match cmd {
        Cmd::New { attach, name, argv } => cmd_new(attach, name, argv),
        Cmd::Attach { name } => run_attach(&name),
        Cmd::List => cmd_list(),
        Cmd::Kill { name } => cmd_kill(&name),
    }
}

/// `veterd new` — fork off a detached session process and (optionally)
/// attach to it.
fn cmd_new(attach: bool, name: String, argv: Vec<String>) -> Result<()> {
    runtime::validate_name(&name)?;
    runtime::ensure_runtime_dir()?;
    let sock = runtime::socket_path(&name);
    match runtime::probe_socket(&sock) {
        runtime::SocketProbe::Alive => {
            bail!("session `{name}` already exists");
        }
        runtime::SocketProbe::Missing | runtime::SocketProbe::Stale => {}
    }

    let log_path = runtime::log_path(&name);
    spawn_detached_session(&name, &argv, &log_path)
        .with_context(|| format!("spawning session process for `{name}`"))?;

    // Poll for the socket to appear. 50ms × 60 = 3s deadline; a
    // session process should bind well within that.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match runtime::probe_socket(&sock) {
            runtime::SocketProbe::Alive => {
                if attach {
                    return run_attach(&name);
                }
                return Ok(());
            }
            runtime::SocketProbe::Missing | runtime::SocketProbe::Stale => {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    bail!(
        "session `{name}` failed to come up within 3s; check {}",
        log_path.display()
    )
}

/// Re-exec this binary with `--session NAME [argv...]` in a
/// double-forked detached process. Stdio is redirected to the
/// session's log file. The child calls `setsid(2)` so it leaves the
/// parent's process group and isn't reaped if our controlling tty
/// hangs up. We `.spawn()` without `.wait()` so the child outlives
/// this CLI invocation.
fn spawn_detached_session(
    name: &str,
    argv: &[String],
    log_path: &std::path::Path,
) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating log directory {}", parent.display())
        })?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("opening log {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| "cloning log fd for stderr")?;
    let exe =
        std::env::current_exe().with_context(|| "discovering own binary path")?;

    let mut cmd = Command::new(exe);
    cmd.arg("--session")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if !argv.is_empty() {
        cmd.arg("--").args(argv);
    }
    // SAFETY: pre_exec runs in the child between fork and exec; it
    // must only call async-signal-safe functions. setsid(2) is.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::other)?;
            Ok(())
        });
    }
    let _child = cmd.spawn().with_context(|| "spawn session process")?;
    // Deliberately don't `.wait()` — the child is detached and we
    // want it to outlive this CLI invocation.
    Ok(())
}

/// `veterd attach NAME` — connect to the session's socket, hand
/// stdio over `SCM_RIGHTS`, then block on the socket until the
/// session ends the attach.
fn run_attach(name: &str) -> Result<()> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    runtime::validate_name(name)?;
    let sock = runtime::socket_path(name);
    let mut stream = UnixStream::connect(&sock).with_context(|| {
        format!("connecting to session `{name}` at {}", sock.display())
    })?;
    Request::Attach
        .write_to(&mut stream)
        .context("writing attach request")?;

    let stdin_fd = std::io::stdin().as_raw_fd();
    let stdout_fd = std::io::stdout().as_raw_fd();
    fdpass::send_stdio(&stream, stdin_fd, stdout_fd)
        .context("sending stdio fds to session")?;

    let resp = Response::read_from(&mut stream).context("reading attach response")?;
    match resp {
        Response::Ok => {}
        Response::Err(msg) => bail!("{msg}"),
        Response::Status(_) => bail!("session returned unexpected response variant"),
    }

    // Hold the tty's foreground process group by staying blocked on
    // a read. The session's handler thread holds the other end of
    // this socket; it drops the handle when the attach ends (detach
    // hotkey, EOF, error). At that point our `read` returns
    // `UnexpectedEof` and the CLI exits, handing the tty back to the
    // parent shell.
    let mut sink = [0u8; 32];
    loop {
        match stream.read(&mut sink) {
            Ok(0) => return Ok(()),
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("waiting for attach to end"),
        }
    }
}

/// `veterd list` — directory scan + parallel `Status` round-trips.
fn cmd_list() -> Result<()> {
    let names = runtime::enumerate_sessions();
    let mut infos: Vec<SessionInfo> = Vec::with_capacity(names.len());
    for name in names {
        match status_of(&name) {
            Ok(info) => infos.push(info),
            Err(_) => {
                // Session went away between enumerate and probe.
                // Silently skip — runtime::probe_socket already
                // unlinks stale entries.
            }
        }
    }
    render_session_table(&infos);
    Ok(())
}

fn status_of(name: &str) -> Result<SessionInfo> {
    let sock = runtime::socket_path(name);
    let mut stream = UnixStream::connect(&sock)
        .with_context(|| format!("connecting to {}", sock.display()))?;
    Request::Status
        .write_to(&mut stream)
        .context("writing status request")?;
    match Response::read_from(&mut stream).context("reading status response")? {
        Response::Status(info) => Ok(info),
        Response::Err(msg) => Err(anyhow!("{msg}")),
        Response::Ok => Err(anyhow!("session returned Ok where Status was expected")),
    }
}

fn render_session_table(infos: &[SessionInfo]) {
    if infos.is_empty() {
        println!("(no sessions)");
        return;
    }
    println!(
        "{:<24}  {:>10}  {:<5}  {}",
        "NAME", "AGE", "ALIVE", "ATTACHED"
    );
    for s in infos {
        println!(
            "{:<24}  {:>10}  {:<5}  {}",
            s.name,
            format_age(s.age_secs),
            if s.alive { "yes" } else { "no" },
            if s.attached { "yes" } else { "no" },
        );
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

/// `veterd kill NAME` — send `Request::Kill` to the per-session
/// socket. Idempotent: missing socket → quiet success (matches v1
/// `Kill` shape).
fn cmd_kill(name: &str) -> Result<()> {
    runtime::validate_name(name)?;
    let sock = runtime::socket_path(name);
    let stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no such session: {name}");
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            // Stale socket from a crashed session.
            let _ = std::fs::remove_file(&sock);
            bail!("no such session: {name} (stale socket cleaned)");
        }
        Err(e) => {
            return Err(e)
                .with_context(|| format!("connecting to {}", sock.display()));
        }
    };
    let mut writer = BufWriter::new(
        stream.try_clone().with_context(|| "clone unix stream")?,
    );
    let mut reader = BufReader::new(stream);
    Request::Kill
        .write_to(&mut writer)
        .context("writing kill request")?;
    match Response::read_from(&mut reader).context("reading kill response")? {
        Response::Ok => Ok(()),
        Response::Err(msg) => bail!("{msg}"),
        Response::Status(_) => bail!("session returned unexpected response variant"),
    }
}

// Suppress an `unused import` warning on `PathBuf` — kept for future
// helpers like log-rotation that take owned paths.
#[allow(dead_code)]
fn _path_buf_marker(_: PathBuf) {}
