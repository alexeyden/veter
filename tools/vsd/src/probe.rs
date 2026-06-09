//! Upstream probe: ask the renderer for its cell pixel dimensions,
//! scale factor, and protocol limits before serializing a snapshot.
//!
//! ## Why probe
//!
//! At session creation the daemon doesn't know the renderer's grid or
//! cell metrics — sessions can be created without anyone attached, and
//! a future attacher could be running on a Retina laptop or a 4K
//! desktop. The host engines default to a conservative 24×80 grid at
//! 8×16 px / 1.0× scale until a real attacher arrives. This module
//! rectifies that during the handshake:
//!
//! 1. The daemon (which owns the renderer's stdio fds via `SCM_RIGHTS`)
//!    reads `TIOCGWINSZ` on stdin to learn the renderer's actual grid
//!    in rows × cols.
//! 2. It writes a VGE `Probe` envelope and a PRT `Probe` envelope to
//!    stdout.
//! 3. It reads from stdin until both responses arrive or a short
//!    timeout fires. Non-probe bytes (a user racing the attach by
//!    typing) are kept as "typeahead" so they can be forwarded to the
//!    inner PTY immediately after the probe phase ends — otherwise
//!    those keystrokes would be silently dropped.
//!
//! Renderers that don't speak VGE or PRT just don't answer; the daemon
//! falls back to its compile-time defaults for any missing metric. The
//! snapshot still lands as plain vt100 plus envelopes the renderer
//! will ignore, so a non-VGE renderer at least sees text.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

/// Subset of VGE `ProbeBody` we plumb back into the engines. We
/// deliberately only carry what the snapshot path needs: cell metrics,
/// scale, and the renderer's supported image encodings (for future use
/// when the image serializer learns to preserve encoded bytes).
#[derive(Debug, Clone, Copy)]
pub struct VgeProbeData {
    #[allow(dead_code)]
    pub protocol_version: u16,
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
    pub scale_factor: f32,
    #[allow(dead_code)] // future image-encoding negotiation
    pub supported_image_encodings: u8,
}

/// Subset of PRT `ProbeBody` that the daemon stores for reference.
/// PRT metrics other than nesting depth aren't currently consumed by
/// the snapshot path — they're decoded so future limits work
/// (e.g. clamping outbound `WritePortal` payloads to
/// `max_write_bytes`).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // decoded for future limit-enforcement passes
pub struct PrtProbeData {
    pub protocol_version: u16,
    pub max_portals: u32,
    pub max_portal_cells_w: u32,
    pub max_portal_cells_h: u32,
    pub max_scrollback_lines: u32,
    pub max_write_bytes: u32,
    pub features: u8,
    pub max_nesting_depth: u8,
}

/// Outcome of one probe round.
#[derive(Debug)]
pub struct ProbeOutcome {
    pub vge: Option<VgeProbeData>,
    /// PRT probe data is decoded but not yet consumed — the snapshot
    /// path only needs VGE cell metrics for now. Keeping it on the
    /// outcome means future limit-enforcement work can pick it up
    /// without re-running the probe.
    #[allow(dead_code)]
    pub prt: Option<PrtProbeData>,
    /// (rows, cols) read via `TIOCGWINSZ` on the renderer's stdin.
    /// `None` if the ioctl failed or returned an obviously empty
    /// size — in which case the daemon keeps the 24×80 default.
    pub winsize: Option<(u16, u16)>,
    /// Bytes received from stdin during the probe phase that weren't
    /// part of a probe response envelope. The attach handler must
    /// forward these to the inner PTY master before entering the
    /// regular splice loop so the user's typeahead isn't lost.
    pub typeahead: Vec<u8>,
}

/// Run one upstream probe. Writes the envelopes to `stdout_fd`, reads
/// from `stdin_fd` until both responses arrive or `timeout` elapses.
pub fn run(stdin_fd: &OwnedFd, stdout_fd: &OwnedFd, timeout: Duration) -> Result<ProbeOutcome> {
    let winsize = read_winsize(stdin_fd.as_raw_fd());
    let vge_env = build_vge_probe();
    let prt_env = build_prt_probe();
    {
        // Write the two probe envelopes in one burst so the renderer
        // can serve them in either order.
        let raw = stdout_fd.as_raw_fd();
        let mut combined = Vec::with_capacity(vge_env.len() + prt_env.len());
        combined.extend_from_slice(&vge_env);
        combined.extend_from_slice(&prt_env);
        // SAFETY: borrow_raw aliases the OwnedFd we already hold.
        let mut sink = unsafe { std::fs::File::from_raw_fd(libc::dup(raw)) };
        sink.write_all(&combined)?;
        sink.flush()?;
        drop(sink);
    }

    use prt_protocol::apc::ApcStream as PrtApc;
    use prt_protocol::frame::MARKER_T2C as PRT_T2C;
    use vge_protocol::apc::ApcStream as VgeApc;
    use vge_protocol::frame::MARKER_T2C as VGE_T2C;

    let mut vge_apc = VgeApc::with_marker(*VGE_T2C);
    let mut prt_apc = PrtApc::with_marker(*PRT_T2C);
    let mut vge: Option<VgeProbeData> = None;
    let mut prt: Option<PrtProbeData> = None;
    let mut typeahead: Vec<u8> = Vec::new();

    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    while vge.is_none() || prt.is_none() {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline - now;
        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: borrowed for the duration of the poll call only.
        let borrowed = stdin_fd.as_fd();
        let mut pollfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        let ready = poll(&mut pollfds, PollTimeout::from(ms as u16))
            .map_err(|e| anyhow!("poll(stdin): {e}"))?;
        if ready == 0 {
            break;
        }
        let n = nix::unistd::read(stdin_fd.as_raw_fd(), &mut buf)
            .map_err(|e| anyhow!("read(stdin): {e}"))?;
        if n == 0 {
            break;
        }
        // Trace probe-phase reads so diagnostic correlation with the
        // splice phase doesn't lose any bytes. Same env var
        // (`VETERD_DEBUG_INPUT=1`), same log path.
        if std::env::var_os("VETERD_DEBUG_INPUT")
            .map(|v| v != "0" && !v.is_empty())
            == Some(true)
        {
            log_probe_chunk(&buf[..n]);
        }
        // Run VGE filter first; its passthrough feeds the PRT filter.
        let vge_out = vge_apc.feed(&buf[..n]);
        let prt_out = prt_apc.feed(&vge_out.passthrough);
        typeahead.extend_from_slice(&prt_out.passthrough);
        if vge.is_none() {
            for payload in vge_out.payloads {
                if let Some(data) = parse_vge_probe_payload(&payload) {
                    vge = Some(data);
                    break;
                }
            }
        }
        if prt.is_none() {
            for payload in prt_out.payloads {
                if let Some(data) = parse_prt_probe_payload(&payload) {
                    prt = Some(data);
                    break;
                }
            }
        }
    }

    Ok(ProbeOutcome { vge, prt, winsize, typeahead })
}

fn build_vge_probe() -> Vec<u8> {
    use vge_protocol::encode::build_envelope;
    use vge_protocol::Command;
    build_envelope(&[(Command::Probe, 1)])
}

fn build_prt_probe() -> Vec<u8> {
    use prt_protocol::encode::build_envelope;
    use prt_protocol::Command;
    build_envelope(&[(Command::Probe, 1)])
}

fn parse_vge_probe_payload(payload: &[u8]) -> Option<VgeProbeData> {
    use vge_protocol::codec::Reader;
    use vge_protocol::frame::RSP_PROBE;
    let mut r = Reader::new(payload);
    let _version = r.u8().ok()?;
    let _payload_len = r.u32().ok()?;
    let frame_type = r.u8().ok()?;
    if frame_type != RSP_PROBE {
        return None;
    }
    let _req_id = r.u32().ok()?;
    let _body_len = r.u32().ok()?;
    let protocol_version = r.u16().ok()?;
    let cell_pixel_width = r.u16().ok()?;
    let cell_pixel_height = r.u16().ok()?;
    let scale_factor = r.f32().ok()?;
    let _max_elements = r.u32().ok()?;
    let _max_commands_per_element = r.u32().ok()?;
    let _max_text_bytes = r.u32().ok()?;
    let _max_image_bytes = r.u32().ok()?;
    let _max_images = r.u32().ok()?;
    let supported_image_encodings = r.u8().ok()?;
    let _max_nesting_depth = r.u8().ok()?;
    Some(VgeProbeData {
        protocol_version,
        cell_pixel_width,
        cell_pixel_height,
        scale_factor,
        supported_image_encodings,
    })
}

fn parse_prt_probe_payload(payload: &[u8]) -> Option<PrtProbeData> {
    use prt_protocol::codec::Reader;
    use prt_protocol::frame::RSP_PROBE;
    let mut r = Reader::new(payload);
    let _version = r.u8().ok()?;
    let _payload_len = r.u32().ok()?;
    let frame_type = r.u8().ok()?;
    if frame_type != RSP_PROBE {
        return None;
    }
    let _req_id = r.u32().ok()?;
    let _body_len = r.u32().ok()?;
    let protocol_version = r.u16().ok()?;
    let max_portals = r.u32().ok()?;
    let max_portal_cells_w = r.u32().ok()?;
    let max_portal_cells_h = r.u32().ok()?;
    let max_scrollback_lines = r.u32().ok()?;
    let max_write_bytes = r.u32().ok()?;
    let features = r.u8().ok()?;
    let max_nesting_depth = r.u8().ok()?;
    Some(PrtProbeData {
        protocol_version,
        max_portals,
        max_portal_cells_w,
        max_portal_cells_h,
        max_scrollback_lines,
        max_write_bytes,
        features,
        max_nesting_depth,
    })
}

/// Read `TIOCGWINSZ` on the given fd. Returns `None` if the ioctl
/// fails or returns a zeroed winsize (which happens on non-tty fds
/// and a few corner cases — we treat both as "size unknown, keep
/// the existing one").
pub fn read_winsize(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 {
        return None;
    }
    if ws.ws_row == 0 || ws.ws_col == 0 {
        return None;
    }
    Some((ws.ws_row, ws.ws_col))
}

/// `TIOCSWINSZ` on the inner PTY master so the child process sees a
/// `SIGWINCH` with the renderer's actual grid dimensions. Best effort —
/// a failure here is not fatal to the attach.
pub fn set_inner_winsize(master_fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
    }
}

/// Diagnostic: append a probe-phase stdin read to the same input.log
/// the splice loop uses, prefixed with `probe>` so it's
/// distinguishable from splice reads at correlation time.
fn log_probe_chunk(chunk: &[u8]) {
    use std::io::Write;
    let dir = crate::runtime::runtime_dir();
    let path = dir.join("input.log");
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut line = format!(
        "[{:>10}.{:03}] probe> {:3} bytes: ",
        ts.as_secs(),
        ts.subsec_millis(),
        chunk.len()
    );
    for &b in chunk {
        line.push_str(&format!("{:02X} ", b));
    }
    line.push('|');
    for &b in chunk {
        line.push(if b.is_ascii_graphic() || b == b' ' {
            b as char
        } else {
            '.'
        });
    }
    line.push_str("|\n");
    let _ = file.write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vge_probe_response() {
        // Build a probe response with known values, wrap it, feed it
        // through `parse_vge_probe_payload` via ApcStream.
        use vge_protocol::apc::ApcStream;
        use vge_protocol::envelope::{append_frame, wrap_t2c_envelope, ProbeBody};
        use vge_protocol::frame::{MARKER_T2C, RSP_PROBE};

        let body = ProbeBody {
            protocol_version: 1,
            cell_pixel_width: 12,
            cell_pixel_height: 24,
            scale_factor: 2.0,
            max_elements: 100,
            max_commands_per_element: 100,
            max_text_bytes: 4096,
            max_image_bytes: 1 << 20,
            max_images: 64,
            supported_image_encodings: 0b0111,
            max_nesting_depth: 8,
        };
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_PROBE, 1, &body.encode());
        let env = wrap_t2c_envelope(&frames);

        let mut apc = ApcStream::with_marker(*MARKER_T2C);
        let out = apc.feed(&env);
        assert_eq!(out.payloads.len(), 1);
        let parsed = parse_vge_probe_payload(&out.payloads[0]).unwrap();
        assert_eq!(parsed.cell_pixel_width, 12);
        assert_eq!(parsed.cell_pixel_height, 24);
        assert!((parsed.scale_factor - 2.0).abs() < 1e-6);
        assert_eq!(parsed.supported_image_encodings, 0b0111);
    }

    #[test]
    fn parses_prt_probe_response() {
        use prt_protocol::apc::ApcStream;
        use prt_protocol::envelope::{append_frame, wrap_t2c_envelope, ProbeBody};
        use prt_protocol::frame::{MARKER_T2C, RSP_PROBE};

        let body = ProbeBody {
            protocol_version: 1,
            max_portals: 64,
            max_portal_cells_w: 1024,
            max_portal_cells_h: 512,
            max_scrollback_lines: 100_000,
            max_write_bytes: 1 << 20,
            features: 0xFF,
            max_nesting_depth: 8,
            vge_features: None,
            accent_rgba: None,
        };
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_PROBE, 1, &body.encode());
        let env = wrap_t2c_envelope(&frames);

        let mut apc = ApcStream::with_marker(*MARKER_T2C);
        let out = apc.feed(&env);
        assert_eq!(out.payloads.len(), 1);
        let parsed = parse_prt_probe_payload(&out.payloads[0]).unwrap();
        assert_eq!(parsed.max_portals, 64);
        assert_eq!(parsed.max_write_bytes, 1 << 20);
        assert_eq!(parsed.features, 0xFF);
        assert_eq!(parsed.max_nesting_depth, 8);
    }

    #[test]
    fn vge_probe_envelope_uses_uppercase_marker() {
        // Sanity: the envelope we build goes out on the C2T marker so
        // the renderer recognizes it as a command.
        let env = build_vge_probe();
        // ESC _ V G E ... ESC \
        assert_eq!(env[0], 0x1B);
        assert_eq!(env[1], b'_');
        assert_eq!(&env[2..5], b"VGE");
    }

    #[test]
    fn prt_probe_envelope_uses_uppercase_marker() {
        let env = build_prt_probe();
        assert_eq!(env[0], 0x1B);
        assert_eq!(env[1], b'_');
        assert_eq!(&env[2..5], b"PRT");
    }
}
