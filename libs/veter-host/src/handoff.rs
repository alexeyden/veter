//! Handing a running terminal's pty to a `vsd` session
//! (`doc/session-manager.md` §2.5).
//!
//! A terminal started without `vsd` can still be turned into a session
//! after the fact. The processes on its pty never notice: their
//! controlling terminal is the *slave*, and the kernel only hangs them
//! up when the last fd on the *master* closes. So veter passes its
//! master to a new session process over `SCM_RIGHTS`, together with a
//! snapshot of the engines that have been parsing it, and stops reading
//! it before the session starts. Bytes the child writes in between stay
//! in the kernel's buffer and reach the new reader in order.
//!
//! veter listens on `$XDG_RUNTIME_DIR/veter/<pid>.sock`. The session
//! process is the one that connects — it is the side that has to come
//! up, so it is the side that asks. A `Describe` is a one-shot
//! question beside it, for `vsd adopt` to tell windows apart when more
//! than one is running:
//!
//! ```text
//! vsd   → veter   Request   "VHO1" | u8 0                         describe
//!                 or        "VHO1" | u8 1 | u8 reattach | str session
//! veter → vsd     Describe  u32 child_pid | u16 rows | u16 cols | str title
//!
//! veter → vsd     Handoff   'H' (+ SCM_RIGHTS master) | u32 child_pid
//!                           | u16 rows | u16 cols | u16 cell_w | u16 cell_h
//!                           | f32 scale | bytes vt | bytes vge | bytes prt
//!                 or        'E' | str reason
//! vsd   → veter   Verdict   u8 0                 adopted; veter lets go
//!                 or        u8 1 | str reason    refused; veter resumes
//! ```
//!
//! `str` and `bytes` are a `u32` LE length then the payload. The engine
//! parts are the same binary snapshots a VSS frame carries (§4.3). This
//! is a conversation between two binaries built from the same tree, so
//! the magic is the whole of its version policy.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};

const MAGIC: &[u8; 4] = b"VHO1";
const KIND_DESCRIBE: u8 = 0;
const KIND_ADOPT: u8 = 1;
const TAG_HANDOFF: u8 = b'H';
const TAG_REFUSED: u8 = b'E';

/// `$XDG_RUNTIME_DIR/veter/`, or `/tmp/veter-<uid>/` without one — the
/// same fallback `vsd` uses for its own directory.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/veter-{}", unsafe { libc::getuid() })));
    base.join("veter")
}

/// Where the terminal with this pid listens.
pub fn socket_path(pid: u32) -> PathBuf {
    runtime_dir().join(format!("{pid}.sock"))
}

/// Create [`runtime_dir`] owner-only. Idempotent.
pub fn ensure_runtime_dir() -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = runtime_dir();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Every terminal that is listening, by pid. A socket nobody answers
/// on is a crashed terminal's leftover and is unlinked on the way.
pub fn list_terminals() -> Vec<(u32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(runtime_dir()) else {
        return Vec::new();
    };
    let mut found: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let pid = path
                .file_name()?
                .to_str()?
                .strip_suffix(".sock")?
                .parse()
                .ok()?;
            Some((pid, path))
        })
        .filter(|(_, path)| match UnixStream::connect(path) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(path);
                false
            }
            Err(_) => false,
        })
        .collect();
    found.sort();
    found
}

/// What a `vsd` process asks a terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Say what this window is, answered with a [`Description`].
    Describe,
    /// Hand the pty over, answered with a [`Handoff`] and closed with a
    /// verdict.
    Adopt {
        /// The session the pty is going to.
        session: String,
        /// Whether the terminal should go on showing it — by attaching
        /// to the session in its own window — or close once it has let
        /// go.
        reattach: bool,
    },
}

impl Request {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut buf = MAGIC.to_vec();
        match self {
            Request::Describe => buf.push(KIND_DESCRIBE),
            Request::Adopt { session, reattach } => {
                buf.push(KIND_ADOPT);
                buf.push(u8::from(*reattach));
                put_bytes(&mut buf, session.as_bytes());
            }
        }
        w.write_all(&buf)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid("not a handoff request"));
        }
        match read_u8(r)? {
            KIND_DESCRIBE => Ok(Request::Describe),
            KIND_ADOPT => {
                let reattach = read_u8(r)? != 0;
                let session = String::from_utf8(read_bytes(r)?)
                    .map_err(|_| invalid("session name is not UTF-8"))?;
                Ok(Request::Adopt { session, reattach })
            }
            k => Err(invalid(&format!("unknown request kind {k}"))),
        }
    }
}

/// What a terminal says about itself. Only what the terminal alone
/// knows: everything readable from `/proc` — its age, what runs under
/// its child — the asker reads there.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    /// The window title as the programs inside last set it.
    pub title: String,
}

impl Description {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.child_pid.to_le_bytes());
        buf.extend_from_slice(&self.rows.to_le_bytes());
        buf.extend_from_slice(&self.cols.to_le_bytes());
        put_bytes(&mut buf, self.title.as_bytes());
        w.write_all(&buf)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self {
            child_pid: read_u32(r)?,
            rows: read_u16(r)?,
            cols: read_u16(r)?,
            title: String::from_utf8_lossy(&read_bytes(r)?).into_owned(),
        })
    }
}

/// Ask the terminal listening at `path` to describe itself.
pub fn describe(path: &Path) -> io::Result<Description> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    Request::Describe.write_to(&mut stream)?;
    Description::read_from(&mut stream)
}

/// Everything but the master fd, which rides beside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Handoff {
    /// The terminal's child: the session leader on the pty's slave.
    pub child_pid: u32,
    pub rows: u16,
    pub cols: u16,
    /// The terminal's cell metrics, so a program that probes the
    /// session before anything attaches is told the truth.
    pub cell_px: (u16, u16),
    pub scale: f32,
    pub vt: Vec<u8>,
    pub vge: Vec<u8>,
    pub prt: Vec<u8>,
}

/// Send the master and the snapshot. The fd travels with the tag byte,
/// since a stream socket needs at least one byte of data to carry it.
pub fn send_handoff(sock: &UnixStream, master: BorrowedFd<'_>, h: &Handoff) -> io::Result<()> {
    let fds = [master.as_raw_fd()];
    let tag = [TAG_HANDOFF];
    sendmsg::<()>(
        sock.as_raw_fd(),
        &[io::IoSlice::new(&tag)],
        &[ControlMessage::ScmRights(&fds)],
        MsgFlags::empty(),
        None,
    )
    .map_err(io::Error::other)?;
    let mut buf = Vec::with_capacity(32 + h.vt.len() + h.vge.len() + h.prt.len());
    buf.extend_from_slice(&h.child_pid.to_le_bytes());
    buf.extend_from_slice(&h.rows.to_le_bytes());
    buf.extend_from_slice(&h.cols.to_le_bytes());
    buf.extend_from_slice(&h.cell_px.0.to_le_bytes());
    buf.extend_from_slice(&h.cell_px.1.to_le_bytes());
    buf.extend_from_slice(&h.scale.to_le_bytes());
    put_bytes(&mut buf, &h.vt);
    put_bytes(&mut buf, &h.vge);
    put_bytes(&mut buf, &h.prt);
    (&*sock).write_all(&buf)
}

/// Say why there is nothing to hand over.
pub fn send_refusal(sock: &UnixStream, reason: &str) -> io::Result<()> {
    let mut buf = vec![TAG_REFUSED];
    put_bytes(&mut buf, reason.as_bytes());
    (&*sock).write_all(&buf)
}

/// The terminal's answer to a [`Request`]: the master and the snapshot,
/// or the reason it refused.
pub fn recv_handoff(sock: &UnixStream) -> io::Result<Result<(OwnedFd, Handoff), String>> {
    let mut tag = [0u8; 1];
    let mut iov = [io::IoSliceMut::new(&mut tag)];
    let mut cmsg = nix::cmsg_space!([RawFd; 1]);
    let msg = recvmsg::<()>(sock.as_raw_fd(), &mut iov, Some(&mut cmsg), MsgFlags::empty())
        .map_err(io::Error::other)?;
    if msg.bytes == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal hung up"));
    }
    let mut fds: Vec<OwnedFd> = Vec::new();
    for c in msg.cmsgs().map_err(io::Error::other)? {
        if let ControlMessageOwned::ScmRights(raw) = c {
            // SAFETY: recvmsg just allocated these for us.
            fds.extend(raw.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }));
        }
    }
    let mut r = sock;
    match tag[0] {
        TAG_REFUSED => {
            let reason = String::from_utf8_lossy(&read_bytes(&mut r)?).into_owned();
            Ok(Err(reason))
        }
        TAG_HANDOFF => {
            if fds.len() != 1 {
                return Err(invalid("handoff arrived without exactly one fd"));
            }
            let child_pid = read_u32(&mut r)?;
            let rows = read_u16(&mut r)?;
            let cols = read_u16(&mut r)?;
            let cell_px = (read_u16(&mut r)?, read_u16(&mut r)?);
            let scale = f32::from_bits(read_u32(&mut r)?);
            let h = Handoff {
                child_pid,
                rows,
                cols,
                cell_px,
                scale,
                vt: read_bytes(&mut r)?,
                vge: read_bytes(&mut r)?,
                prt: read_bytes(&mut r)?,
            };
            Ok(Ok((fds.pop().unwrap(), h)))
        }
        t => Err(invalid(&format!("unknown handoff tag 0x{t:02X}"))),
    }
}

/// The session process's last word: whether it took the pty.
pub fn send_verdict<W: Write>(w: &mut W, verdict: Result<(), &str>) -> io::Result<()> {
    let mut buf = Vec::new();
    match verdict {
        Ok(()) => buf.push(0),
        Err(reason) => {
            buf.push(1);
            put_bytes(&mut buf, reason.as_bytes());
        }
    }
    w.write_all(&buf)
}

pub fn recv_verdict<R: Read>(r: &mut R) -> io::Result<Result<(), String>> {
    match read_u8(r)? {
        0 => Ok(Ok(())),
        _ => Ok(Err(String::from_utf8_lossy(&read_bytes(r)?).into_owned())),
    }
}

/// Remove `path` on drop, so any exit leaves the runtime dir clean.
pub struct SocketGuard(pub PathBuf);

impl SocketGuard {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Snapshots are a few hundred KiB at most; this only stops a garbage
/// length from allocating the machine.
const MAX_PART_BYTES: usize = 256 << 20;

fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
    buf.extend_from_slice(b);
}

fn read_u8<R: Read>(r: &mut R) -> io::Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16<R: Read>(r: &mut R) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_bytes<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let len = read_u32(r)? as usize;
    if len > MAX_PART_BYTES {
        return Err(invalid("handoff part too large"));
    }
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        for req in [
            Request::Describe,
            Request::Adopt { session: "work".into(), reattach: true },
        ] {
            let mut buf = Vec::new();
            req.write_to(&mut buf).unwrap();
            assert_eq!(Request::read_from(&mut &buf[..]).unwrap(), req);
        }
    }

    #[test]
    fn description_roundtrip() {
        let d = Description { child_pid: 7, rows: 30, cols: 100, title: "~/Dev — nvim".into() };
        let mut buf = Vec::new();
        d.write_to(&mut buf).unwrap();
        assert_eq!(Description::read_from(&mut &buf[..]).unwrap(), d);
    }

    #[test]
    fn handoff_carries_the_fd_and_the_snapshot() {
        let (a, b) = UnixStream::pair().unwrap();
        let (pipe_r, pipe_w) = nix::unistd::pipe().unwrap();
        let h = Handoff {
            child_pid: 42,
            rows: 30,
            cols: 100,
            cell_px: (9, 18),
            scale: 1.5,
            vt: vec![1, 2, 3],
            vge: vec![],
            prt: vec![7; 5000],
        };
        send_handoff(&a, std::os::fd::AsFd::as_fd(&pipe_w), &h).unwrap();
        drop(pipe_w);
        let (fd, got) = recv_handoff(&b).unwrap().unwrap();
        assert_eq!(got, h);
        // The fd is a working copy of the pipe's write end.
        std::fs::File::from(fd).write_all(b"ok").unwrap();
        let mut s = String::new();
        std::fs::File::from(pipe_r).read_to_string(&mut s).unwrap();
        assert_eq!(s, "ok");
    }

    #[test]
    fn refusal_and_verdicts() {
        let (a, b) = UnixStream::pair().unwrap();
        send_refusal(&a, "no pty").unwrap();
        assert_eq!(recv_handoff(&b).unwrap().unwrap_err(), "no pty");
        let mut buf = Vec::new();
        send_verdict(&mut buf, Ok(())).unwrap();
        send_verdict(&mut buf, Err("taken")).unwrap();
        let mut r = &buf[..];
        assert_eq!(recv_verdict(&mut r).unwrap(), Ok(()));
        assert_eq!(recv_verdict(&mut r).unwrap(), Err("taken".into()));
    }
}
