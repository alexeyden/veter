//! Length-prefixed binary IPC between the `vsd` CLI front-end and
//! the per-session daemon processes.
//!
//! Frame layout (both directions):
//!
//! ```text
//! u32  payload_len (LE, excludes this prefix)
//! u8   kind
//! ...  kind-specific body
//! ```
//!
//! Strings are `u32 len` (LE) + UTF-8 bytes. The protocol is
//! intentionally tiny — it evolves alongside `vsd` and lives
//! entirely within this repo, so robust evolution rules are not a
//! v1 concern. Frames are sent over a per-session Unix-domain socket
//! at `$XDG_RUNTIME_DIR/vsd/<NAME>.sock` (mode 0700).
//!
//! Each session is its own process listening on its own socket;
//! `New` / `List` / `KillServer` from the v1 daemon-of-many-sessions
//! design are gone. `New` is now a CLI-only operation (forks the
//! session process); `List` is a directory scan + parallel `Status`
//! round-trips.

use std::io::{self, Read, Write};

/// Request: CLI → session process.
#[derive(Debug, Clone)]
pub enum Request {
    /// Attach the caller's stdio to this session. After this frame
    /// the client immediately follows up with a single `SCM_RIGHTS`
    /// ancillary-data message carrying stdin then stdout (in that
    /// order) over the same socket; see [`crate::fdpass`] for the
    /// helpers. The session takes ownership of those fds, writes the
    /// snapshot, then splices bytes between the renderer and the
    /// inner PTY. Reply: `Ok` on success (client stays blocked on the
    /// socket until the session ends the attach) or `Err(msg)` if the
    /// session is already attached.
    Attach,
    /// Terminate this session: SIGTERM the inner PTY child, unlink
    /// the socket, exit the session process. Idempotent — a second
    /// `Kill` won't reach anyone because the socket is gone.
    Kill,
    /// Return a [`SessionInfo`] snapshot describing this session.
    /// Used by `vsd list` to populate the table.
    Status,
}

/// Response: session process → CLI.
#[derive(Debug, Clone)]
pub enum Response {
    Ok,
    Status(SessionInfo),
    Err(String),
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    /// Seconds since the session was created.
    pub age_secs: u64,
    /// True if the inner PTY child is still running. With per-session
    /// processes a dead child triggers session-process exit, so a
    /// `Status` response is only emitted while the child is alive —
    /// this field is effectively always `true` on the wire. Retained
    /// for table-column parity with the v1 output and as a hedge
    /// against any future "linger after exit" mode.
    pub alive: bool,
    /// True if a renderer is currently attached. The attach handler
    /// thread flips this to `true` after the snapshot ships and back
    /// to `false` on detach.
    pub attached: bool,
}

const REQ_ATTACH: u8 = 0x01;
const REQ_KILL: u8 = 0x02;
const REQ_STATUS: u8 = 0x03;

const RSP_OK: u8 = 0x10;
const RSP_STATUS: u8 = 0x11;
const RSP_ERR: u8 = 0x12;

impl Request {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut body = Vec::new();
        match self {
            Request::Attach => body.push(REQ_ATTACH),
            Request::Kill => body.push(REQ_KILL),
            Request::Status => body.push(REQ_STATUS),
        }
        write_frame(w, &body)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let body = read_frame(r)?;
        if body.is_empty() {
            return Err(invalid("empty request frame"));
        }
        let kind = body[0];
        match kind {
            REQ_ATTACH => Ok(Request::Attach),
            REQ_KILL => Ok(Request::Kill),
            REQ_STATUS => Ok(Request::Status),
            _ => Err(invalid(&format!("unknown request kind 0x{kind:02X}"))),
        }
    }
}

impl Response {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut body = Vec::new();
        match self {
            Response::Ok => body.push(RSP_OK),
            Response::Status(info) => {
                body.push(RSP_STATUS);
                write_string(&mut body, &info.name);
                write_u64(&mut body, info.age_secs);
                body.push(u8::from(info.alive));
                body.push(u8::from(info.attached));
            }
            Response::Err(msg) => {
                body.push(RSP_ERR);
                write_string(&mut body, msg);
            }
        }
        write_frame(w, &body)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let body = read_frame(r)?;
        if body.is_empty() {
            return Err(invalid("empty response frame"));
        }
        let kind = body[0];
        let mut rest = &body[1..];
        match kind {
            RSP_OK => Ok(Response::Ok),
            RSP_STATUS => {
                let name = read_string(&mut rest)?;
                let age_secs = read_u64(&mut rest)?;
                let alive = read_u8(&mut rest)? != 0;
                let attached = read_u8(&mut rest)? != 0;
                Ok(Response::Status(SessionInfo {
                    name,
                    age_secs,
                    alive,
                    attached,
                }))
            }
            RSP_ERR => {
                let msg = read_string(&mut rest)?;
                Ok(Response::Err(msg))
            }
            _ => Err(invalid(&format!("unknown response kind 0x{kind:02X}"))),
        }
    }
}

// ---- codec primitives --------------------------------------------

fn write_frame<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    let len = body.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(body)?;
    w.flush()?;
    Ok(())
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    // Soft cap so a malicious peer can't make us allocate gigabytes.
    if len > 16 * 1024 * 1024 {
        return Err(invalid(&format!("frame too large: {len} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

fn write_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

fn read_u8(rest: &mut &[u8]) -> io::Result<u8> {
    if rest.is_empty() {
        return Err(invalid("unexpected end of frame (u8)"));
    }
    let v = rest[0];
    *rest = &rest[1..];
    Ok(v)
}

fn read_u32(rest: &mut &[u8]) -> io::Result<u32> {
    if rest.len() < 4 {
        return Err(invalid("unexpected end of frame (u32)"));
    }
    let v = u32::from_le_bytes(rest[..4].try_into().unwrap());
    *rest = &rest[4..];
    Ok(v)
}

fn read_u64(rest: &mut &[u8]) -> io::Result<u64> {
    if rest.len() < 8 {
        return Err(invalid("unexpected end of frame (u64)"));
    }
    let v = u64::from_le_bytes(rest[..8].try_into().unwrap());
    *rest = &rest[8..];
    Ok(v)
}

fn read_string(rest: &mut &[u8]) -> io::Result<String> {
    let len = read_u32(rest)? as usize;
    if rest.len() < len {
        return Err(invalid("unexpected end of frame (string)"));
    }
    let s = std::str::from_utf8(&rest[..len])
        .map_err(|_| invalid("string is not valid UTF-8"))?
        .to_owned();
    *rest = &rest[len..];
    Ok(s)
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_request(req: Request) -> Request {
        let mut buf = Vec::new();
        req.write_to(&mut buf).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        Request::read_from(&mut cursor).unwrap()
    }

    fn roundtrip_response(rsp: Response) -> Response {
        let mut buf = Vec::new();
        rsp.write_to(&mut buf).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        Response::read_from(&mut cursor).unwrap()
    }

    #[test]
    fn attach_round_trip() {
        assert!(matches!(
            roundtrip_request(Request::Attach),
            Request::Attach
        ));
    }

    #[test]
    fn kill_round_trip() {
        assert!(matches!(roundtrip_request(Request::Kill), Request::Kill));
    }

    #[test]
    fn status_round_trip() {
        assert!(matches!(
            roundtrip_request(Request::Status),
            Request::Status
        ));
    }

    #[test]
    fn status_response_round_trip() {
        let rsp = Response::Status(SessionInfo {
            name: "alpha".into(),
            age_secs: 42,
            alive: true,
            attached: false,
        });
        match roundtrip_response(rsp) {
            Response::Status(info) => {
                assert_eq!(info.name, "alpha");
                assert_eq!(info.age_secs, 42);
                assert!(info.alive);
                assert!(!info.attached);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn err_response_round_trip() {
        match roundtrip_response(Response::Err("nope".into())) {
            Response::Err(m) => assert_eq!(m, "nope"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn ok_response_round_trip() {
        assert!(matches!(roundtrip_response(Response::Ok), Response::Ok));
    }
}
