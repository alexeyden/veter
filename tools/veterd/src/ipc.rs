//! Length-prefixed binary IPC between the `veterd` CLI front-end and
//! the long-lived daemon process.
//!
//! Frame layout (both directions):
//!
//! ```text
//! u32  payload_len (LE, excludes this prefix)
//! u8   kind
//! ...  kind-specific body
//! ```
//!
//! Strings are `u32 len` (LE) + UTF-8 bytes. Lists are `u32 count`
//! followed by elements. The protocol is intentionally tiny — it
//! evolves alongside the daemon and lives entirely within this repo,
//! so robust evolution rules (capability negotiation, etc.) are not
//! a v1 concern. Frames are sent over a per-user Unix-domain socket
//! at `$XDG_RUNTIME_DIR/veterd/sock` (mode 0700).

use std::io::{self, Read, Write};

/// Request: client → daemon.
#[derive(Debug, Clone)]
pub enum Request {
    /// Create a new session named `name`, spawning `argv[0] argv[1..]`
    /// inside the session's inner PTY. If `argv` is empty the daemon
    /// uses `$SHELL` (or `/bin/sh` as a final fallback).
    New { name: String, argv: Vec<String> },
    /// Enumerate every session known to the daemon.
    List,
    /// Tear down the named session (signal its inner process and drop
    /// the entry). Idempotent: unknown names yield `Err`.
    Kill { name: String },
    /// Shut the daemon down immediately, killing every session.
    KillServer,
}

/// Response: daemon → client.
#[derive(Debug, Clone)]
pub enum Response {
    Ok,
    Sessions(Vec<SessionInfo>),
    Err(String),
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    /// Seconds since the session was created.
    pub age_secs: u64,
    /// True if the inner PTY child is still running.
    pub alive: bool,
    /// True if a renderer is currently attached. Always false in the
    /// skeleton — flipped to real meaning when task #6 lands.
    pub attached: bool,
}

const REQ_NEW: u8 = 0x01;
const REQ_LIST: u8 = 0x02;
const REQ_KILL: u8 = 0x03;
const REQ_KILL_SERVER: u8 = 0x04;

const RSP_OK: u8 = 0x10;
const RSP_SESSIONS: u8 = 0x11;
const RSP_ERR: u8 = 0x12;

impl Request {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut body = Vec::new();
        match self {
            Request::New { name, argv } => {
                body.push(REQ_NEW);
                write_string(&mut body, name);
                write_string_list(&mut body, argv);
            }
            Request::List => body.push(REQ_LIST),
            Request::Kill { name } => {
                body.push(REQ_KILL);
                write_string(&mut body, name);
            }
            Request::KillServer => body.push(REQ_KILL_SERVER),
        }
        write_frame(w, &body)
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let body = read_frame(r)?;
        if body.is_empty() {
            return Err(invalid("empty request frame"));
        }
        let kind = body[0];
        let mut rest = &body[1..];
        match kind {
            REQ_NEW => {
                let name = read_string(&mut rest)?;
                let argv = read_string_list(&mut rest)?;
                Ok(Request::New { name, argv })
            }
            REQ_LIST => Ok(Request::List),
            REQ_KILL => {
                let name = read_string(&mut rest)?;
                Ok(Request::Kill { name })
            }
            REQ_KILL_SERVER => Ok(Request::KillServer),
            _ => Err(invalid(&format!("unknown request kind 0x{kind:02X}"))),
        }
    }
}

impl Response {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let mut body = Vec::new();
        match self {
            Response::Ok => body.push(RSP_OK),
            Response::Sessions(list) => {
                body.push(RSP_SESSIONS);
                write_u32(&mut body, list.len() as u32);
                for s in list {
                    write_string(&mut body, &s.name);
                    write_u64(&mut body, s.age_secs);
                    body.push(if s.alive { 1 } else { 0 });
                    body.push(if s.attached { 1 } else { 0 });
                }
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
            RSP_SESSIONS => {
                let n = read_u32(&mut rest)? as usize;
                let mut list = Vec::with_capacity(n);
                for _ in 0..n {
                    let name = read_string(&mut rest)?;
                    let age_secs = read_u64(&mut rest)?;
                    let alive = read_u8(&mut rest)? != 0;
                    let attached = read_u8(&mut rest)? != 0;
                    list.push(SessionInfo { name, age_secs, alive, attached });
                }
                Ok(Response::Sessions(list))
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

fn write_string_list(buf: &mut Vec<u8>, list: &[String]) {
    write_u32(buf, list.len() as u32);
    for s in list {
        write_string(buf, s);
    }
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

fn read_string_list(rest: &mut &[u8]) -> io::Result<Vec<String>> {
    let n = read_u32(rest)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_string(rest)?);
    }
    Ok(out)
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
    fn new_round_trip() {
        let req = Request::New {
            name: "cool".into(),
            argv: vec!["bash".into(), "-l".into()],
        };
        match roundtrip_request(req) {
            Request::New { name, argv } => {
                assert_eq!(name, "cool");
                assert_eq!(argv, vec!["bash", "-l"]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn list_round_trip() {
        assert!(matches!(roundtrip_request(Request::List), Request::List));
    }

    #[test]
    fn kill_round_trip() {
        match roundtrip_request(Request::Kill { name: "x".into() }) {
            Request::Kill { name } => assert_eq!(name, "x"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn sessions_response_round_trip() {
        let rsp = Response::Sessions(vec![SessionInfo {
            name: "alpha".into(),
            age_secs: 42,
            alive: true,
            attached: false,
        }]);
        match roundtrip_response(rsp) {
            Response::Sessions(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].name, "alpha");
                assert_eq!(list[0].age_secs, 42);
                assert!(list[0].alive);
                assert!(!list[0].attached);
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
}
