//! Target-tty resolution, cell metrics, and reading a reply back.
//!
//! Two modes matter and they are not the same. A tool run *as* a pane's
//! foreground program can write a command and read the reply on the same
//! tty. A tool run from outside — a hook, a script driving someone
//! else's pane — must not read: the reply lands in the pane's input
//! queue where the foreground program is blocked in `read()`, and
//! whoever the kernel wakes gets the bytes. That is what
//! `--no-response` is for, and why the metrics below come from
//! `TIOCGWINSZ` and the environment rather than a protocol round-trip.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::proto::{Frame, Proto, parse_payload};

/// Where to write. `--tty`, else `$VMUX_PANE_TTY` (set by vmux inside a
/// pane), else the controlling terminal.
pub fn resolve(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os("VMUX_PANE_TTY").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    Ok(PathBuf::from("/dev/tty"))
}

/// Cell metrics and the static limits, obtained without a round-trip.
///
/// `TIOCGWINSZ` carries pixel dimensions on a veter pty, so cell size
/// divides out exactly; `$VETER_LIMITS` carries the §11.1 caps the
/// probe would otherwise report. Together they let an out-of-band
/// client size an image without ever reading from the tty.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Caps {
    pub cell_px: [u16; 2],
    pub cols: u16,
    pub rows: u16,
    pub max_image_bytes: u64,
    pub max_images: u64,
    pub max_write_bytes: u64,
    pub max_nesting_depth: u64,
    pub supported_image_encodings: u8,
    /// False when `$VETER` is unset — the caller is probably not under
    /// a veter terminal and nothing will consume what we write.
    pub veter: bool,
}

impl Caps {
    pub fn probe(tty: &std::fs::File) -> Result<Self> {
        let veter = std::env::var("VETER").is_ok_and(|v| !v.is_empty());
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: `ws` is a valid winsize for the duration of the call.
        let rc = unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, &mut ws) };
        if rc != 0 {
            bail!("TIOCGWINSZ on the target tty failed: {}", std::io::Error::last_os_error());
        }
        if ws.ws_col == 0 || ws.ws_row == 0 {
            bail!("terminal reports a zero-sized grid");
        }
        let limits = std::env::var("VETER_LIMITS").unwrap_or_default();
        let get = |k: &str, d: u64| limit(&limits, k).unwrap_or(d);
        Ok(Self {
            cell_px: [
                if ws.ws_col > 0 { ws.ws_xpixel / ws.ws_col } else { 0 },
                if ws.ws_row > 0 { ws.ws_ypixel / ws.ws_row } else { 0 },
            ],
            cols: ws.ws_col,
            rows: ws.ws_row,
            max_image_bytes: get("mib", 32 * 1024 * 1024),
            max_images: get("mi", 1024),
            max_write_bytes: get("mwb", 1024 * 1024),
            max_nesting_depth: get("nest", 8),
            supported_image_encodings: get("enc", 1) as u8,
            veter,
        })
    }
}

/// One `key=value` from `$VETER_LIMITS`. An unparseable value reads as
/// absent so the caller falls back to the §11 default rather than zero.
fn limit(limits: &str, key: &str) -> Option<u64> {
    limits
        .split(',')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.trim().parse().ok())
}

/// Raw mode on stdin for the duration of a read-back, restored on drop.
/// Without it the terminal would echo the reply and line-buffer it, and
/// ICRNL would rewrite 0x0D inside the payload.
pub struct RawTty(libc::termios);

impl RawTty {
    pub fn enable() -> Result<Self> {
        let fd = std::io::stdin().as_raw_fd();
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
            bail!("tcgetattr: {}", std::io::Error::last_os_error());
        }
        let saved = t;
        t.c_lflag &= !(libc::ICANON | libc::ECHO);
        t.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON);
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
            bail!("tcsetattr: {}", std::io::Error::last_os_error());
        }
        Ok(Self(saved))
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        let fd = std::io::stdin().as_raw_fd();
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.0) };
    }
}

/// Read one response envelope from stdin, or `None` on timeout.
pub fn read_response(proto: Proto, timeout: Duration) -> Result<Option<Vec<Frame>>> {
    let fd = std::io::stdin().as_raw_fd();
    let mut apc = vge_protocol::apc::ApcStream::with_marker(proto.response_marker());
    let deadline = Instant::now() + timeout;

    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        let ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, count matches.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            return Err(anyhow!("poll: {}", std::io::Error::last_os_error()));
        }
        if n == 0 {
            return Ok(None);
        }
        let mut buf = [0u8; 8192];
        // SAFETY: writing into our own buffer, length matches.
        let nread = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if nread <= 0 {
            return Ok(None);
        }
        let out = apc.feed(&buf[..nread as usize]);
        if let Some(payload) = out.payloads.into_iter().next() {
            return Ok(Some(parse_payload(proto, &payload).context("decode response")?));
        }
    }
}
