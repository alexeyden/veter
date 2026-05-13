//! Daemon process — single long-lived background process per user.
//!
//! Listens on a per-user Unix socket at
//! `$XDG_RUNTIME_DIR/veterd/sock` (mode 0700) and dispatches one
//! `Request` → `Response` per accepted connection. Sessions live in
//! a single `HashMap<String, Session>` owned by the accept loop; v1
//! is single-threaded so there's no lock to worry about.
//!
//! Attach / detach (task #6) will extend this with stdio fd handover
//! via `SCM_RIGHTS` and per-session host-engine processing of the
//! inner PTY's output.

use std::collections::HashMap;
use std::fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::attach;
use crate::ipc::{Request, Response, SessionInfo};
use crate::session::Session;

/// Per-user socket path, honouring `$XDG_RUNTIME_DIR` when set.
pub fn socket_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Fallback when XDG_RUNTIME_DIR is not set (e.g. plain
            // sshd without pam_systemd). `/tmp` is world-writable so
            // we'll mkdir a uid-scoped subdir below.
            PathBuf::from(format!("/tmp/veterd-{}", nix::unistd::getuid().as_raw()))
        });
    runtime.join("veterd").join("sock")
}

fn ensure_socket_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        // Tighten perms so other local users can't poke our socket.
        let mut perms = fs::metadata(parent)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o700);
        let _ = fs::set_permissions(parent, perms);
    }
    Ok(())
}

/// Run the daemon's accept loop until a `KillServer` request comes in
/// or the listener fails. Caller is responsible for daemonizing (or
/// not — `--foreground` is the only mode the skeleton supports).
pub fn run() -> Result<()> {
    let sock = socket_path();
    ensure_socket_dir(&sock)?;
    // Clean up any stale socket from a previous crash. If a daemon
    // is genuinely running we'll find out when bind() fails below;
    // a future revision can probe-then-unlink instead.
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding {}", sock.display()))?;
    eprintln!("veterd: listening on {}", sock.display());

    let mut sessions: HashMap<String, Session> = HashMap::new();
    let mut quit = false;

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                handle_connection(stream, &mut sessions, &mut quit);
                if quit {
                    break;
                }
            }
            Err(e) => {
                eprintln!("veterd: accept error: {e}");
            }
        }
    }

    // Shut down every session on exit.
    for (_, mut s) in sessions.drain() {
        s.shutdown();
    }
    let _ = fs::remove_file(&sock);
    Ok(())
}

fn handle_connection(
    mut stream: UnixStream,
    sessions: &mut HashMap<String, Session>,
    quit: &mut bool,
) {
    // No `BufReader` wrapping here: an Attach request follows up with
    // a `sendmsg(2)` carrying `SCM_RIGHTS` ancillary data, and any
    // bytes BufReader buffers ahead would also strip the cmsg they
    // were delivered with. The frames are tiny — direct reads are
    // fine.
    let req = match Request::read_from(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            // UnexpectedEof here means the peer connected and closed
            // without sending — that's how the CLI's auto-spawn path
            // probes whether the socket is responsive. Treat it as a
            // ping, not an error.
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                return;
            }
            eprintln!("veterd: bad request: {e}");
            let _ = Response::Err(format!("bad request: {e}")).write_to(&mut stream);
            return;
        }
    };
    let resp = dispatch(req, &mut stream, sessions, quit);
    if let Err(e) = resp.write_to(&mut stream) {
        eprintln!("veterd: write reply: {e}");
    }
}

fn dispatch(
    req: Request,
    stream: &mut UnixStream,
    sessions: &mut HashMap<String, Session>,
    quit: &mut bool,
) -> Response {
    match req {
        Request::New { name, argv } => match new_session(sessions, name, argv) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Err(format!("{e:#}")),
        },
        Request::List => Response::Sessions(list_sessions(sessions)),
        Request::Kill { name } => match sessions.remove(&name) {
            Some(mut s) => {
                s.shutdown();
                Response::Ok
            }
            None => Response::Err(format!("no such session: {name}")),
        },
        Request::KillServer => {
            *quit = true;
            Response::Ok
        }
        Request::Attach { name } => match attach::start(stream, sessions, &name) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Err(format!("{e:#}")),
        },
    }
}

fn new_session(
    sessions: &mut HashMap<String, Session>,
    name: String,
    argv: Vec<String>,
) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("session name must be non-empty");
    }
    if sessions.contains_key(&name) {
        anyhow::bail!("session `{name}` already exists");
    }
    let argv = if argv.is_empty() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        vec![shell]
    } else {
        argv
    };
    let session = Session::spawn(name.clone(), &argv)?;
    sessions.insert(name, session);
    Ok(())
}

fn list_sessions(sessions: &HashMap<String, Session>) -> Vec<SessionInfo> {
    let mut out: Vec<SessionInfo> = sessions
        .values()
        .map(|s| SessionInfo {
            name: s.name.clone(),
            age_secs: s.age_secs(),
            alive: s.is_alive(),
            attached: s.is_attached(),
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}
