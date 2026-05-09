// APC envelope wrapping (§1.1–1.2) for both directions, plus host-side
// body builders for ProbeResponse, Err, the per-command Ok bodies that
// carry data (BeginUpload/BeginDownload/EndUpload, §6.1/§7.1/§6.3), and
// every event body in §4.2.

use crate::codec::{stuff, Writer};
use crate::frame::*;

/// Build the body for a ProbeResponse (§2.1).
pub struct ProbeBody {
    pub protocol_version: u16,
    pub max_concurrent_transfers: u32,
    pub max_chunk_bytes: u32,
    pub max_path_bytes: u32,
    /// `0` means no host-side limit.
    pub max_file_bytes: u64,
    pub features: u8,
}

impl ProbeBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(2 + 4 * 3 + 8 + 1);
        w.u16(self.protocol_version);
        w.u32(self.max_concurrent_transfers);
        w.u32(self.max_chunk_bytes);
        w.u32(self.max_path_bytes);
        w.u64(self.max_file_bytes);
        w.u8(self.features);
        w.buf
    }
}

/// Build the body for an Err response (§4.1).
pub fn err_body(error_code: u16, message: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + 1 + message.len());
    w.u16(error_code);
    w.str(message);
    w.buf
}

// ---- per-command Ok bodies --------------------------------------------

/// §6.1 — Ok body for BeginUpload.
pub fn ok_begin_upload_body(resolved_path: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + resolved_path.len());
    w.str(resolved_path);
    w.buf
}

/// §6.3 — Ok body for EndUpload.
pub fn ok_end_upload_body(final_path: &str, bytes_written: u64) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + final_path.len() + 8);
    w.str(final_path);
    w.u64(bytes_written);
    w.buf
}

/// §7.1 — Ok body for BeginDownload.
pub fn ok_begin_download_body(
    resolved_path: &str,
    total_bytes: u64,
    mode: u32,
    mtime: i64,
) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + resolved_path.len() + 8 + 4 + 8);
    w.str(resolved_path);
    w.u64(total_bytes);
    w.u32(mode);
    w.i64(mtime);
    w.buf
}

// ---- §4.2 event bodies ------------------------------------------------

pub fn download_chunk_body(id: &str, offset: u64, data: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 8 + 5 + data.len());
    w.str(id);
    w.u64(offset);
    w.bytes(data);
    w.buf
}

pub fn download_end_body(id: &str, bytes_sent: u64) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 8);
    w.str(id);
    w.u64(bytes_sent);
    w.buf
}

pub fn upload_ack_body(id: &str, bytes_received: u64, bytes_processed: u64) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 16);
    w.str(id);
    w.u64(bytes_received);
    w.u64(bytes_processed);
    w.buf
}

pub fn transfer_aborted_body(id: &str, reason: u8, message: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + id.len() + 1 + 1 + message.len());
    w.str(id);
    w.u8(reason);
    w.str(message);
    w.buf
}

// ---- frame + envelope wrapping ----------------------------------------

/// Append a single frame to an unstuffed payload buffer.
/// Frame layout (§1.2): u8 frame_type, u32 request_id, u32 body_length,
/// body[body_length].
pub fn append_frame(buf: &mut Vec<u8>, frame_type: u8, request_id: u32, body: &[u8]) {
    buf.push(frame_type);
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body);
}

fn wrap(frames_buf: &[u8], marker: &[u8; 3]) -> Vec<u8> {
    // §1.2 unstuffed payload = u8 protocol_version, u32 payload_length,
    // frames. payload_length is "length of the rest" — i.e. just the
    // frames region.
    let mut unstuffed = Vec::with_capacity(5 + frames_buf.len());
    unstuffed.push(PROTOCOL_VERSION);
    unstuffed.extend_from_slice(&(frames_buf.len() as u32).to_le_bytes());
    unstuffed.extend_from_slice(frames_buf);

    let mut env = Vec::with_capacity(7 + unstuffed.len());
    env.push(ESC);
    env.push(APC_OPEN);
    env.extend_from_slice(marker);
    stuff(&unstuffed, &mut env);
    env.push(ESC);
    env.push(ST_CLOSE);
    env
}

/// Wrap a frame buffer as a host→client envelope (lowercase `vft`
/// marker). This is what the host emits as responses and events.
pub fn wrap_h2c_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_H2C)
}

/// Wrap a frame buffer as a client→host envelope (uppercase `VFT`
/// marker). Used by clients to feed commands.
pub fn wrap_c2h_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_C2H)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apc::ApcStream;
    use crate::codec::Reader;

    #[test]
    fn probe_body_encoded_size() {
        // 2 + 4 + 4 + 4 + 8 + 1 = 23 bytes.
        let pb = ProbeBody {
            protocol_version: 1,
            max_concurrent_transfers: 8,
            max_chunk_bytes: 4 << 20,
            max_path_bytes: 4096,
            max_file_bytes: 0,
            features: FEAT_UPLOAD | FEAT_DOWNLOAD,
        };
        assert_eq!(pb.encode().len(), 23);
    }

    #[test]
    fn probe_body_round_trip() {
        let pb = ProbeBody {
            protocol_version: 1,
            max_concurrent_transfers: 8,
            max_chunk_bytes: 4 << 20,
            max_path_bytes: 4096,
            max_file_bytes: 1u64 << 40,
            features: FEAT_UPLOAD | FEAT_DOWNLOAD,
        };
        let body = pb.encode();
        let mut r = Reader::new(&body);
        assert_eq!(r.u16().unwrap(), 1);
        assert_eq!(r.u32().unwrap(), 8);
        assert_eq!(r.u32().unwrap(), 4 << 20);
        assert_eq!(r.u32().unwrap(), 4096);
        assert_eq!(r.u64().unwrap(), 1u64 << 40);
        assert_eq!(r.u8().unwrap(), FEAT_UPLOAD | FEAT_DOWNLOAD);
        assert!(r.at_end());
    }

    #[test]
    fn ok_begin_upload_body_round_trip() {
        let body = ok_begin_upload_body("/tmp/foo.png");
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "/tmp/foo.png");
        assert!(r.at_end());
    }

    #[test]
    fn ok_end_upload_body_round_trip() {
        let body = ok_end_upload_body("/tmp/foo.png", 91234);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "/tmp/foo.png");
        assert_eq!(r.u64().unwrap(), 91234);
        assert!(r.at_end());
    }

    #[test]
    fn ok_begin_download_body_round_trip() {
        let body = ok_begin_download_body("/var/log/syslog", 8_421_376, 0o640, 1_700_000_000);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "/var/log/syslog");
        assert_eq!(r.u64().unwrap(), 8_421_376);
        assert_eq!(r.u32().unwrap(), 0o640);
        assert_eq!(r.i64().unwrap(), 1_700_000_000);
        assert!(r.at_end());
    }

    #[test]
    fn err_body_round_trip() {
        let body = err_body(ERR_PATH_DENIED, "policy refused");
        let mut r = Reader::new(&body);
        assert_eq!(r.u16().unwrap(), ERR_PATH_DENIED);
        assert_eq!(r.string().unwrap(), "policy refused");
        assert!(r.at_end());
    }

    #[test]
    fn download_chunk_body_round_trip() {
        let body = download_chunk_body("vrecv-1", 262_144, &[0x1B, b'[', b'H']);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "vrecv-1");
        assert_eq!(r.u64().unwrap(), 262_144);
        assert_eq!(r.bytes().unwrap(), &[0x1B, b'[', b'H']);
        assert!(r.at_end());
    }

    #[test]
    fn download_end_body_round_trip() {
        let body = download_end_body("vrecv-1", 8_421_376);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "vrecv-1");
        assert_eq!(r.u64().unwrap(), 8_421_376);
        assert!(r.at_end());
    }

    #[test]
    fn upload_ack_body_round_trip() {
        let body = upload_ack_body("vsend-1", 33_554_432, 29_360_128);
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "vsend-1");
        assert_eq!(r.u64().unwrap(), 33_554_432);
        assert_eq!(r.u64().unwrap(), 29_360_128);
        assert!(r.at_end());
    }

    #[test]
    fn transfer_aborted_body_round_trip() {
        let body = transfer_aborted_body("t1", ABORT_DISK_FULL, "out of space");
        let mut r = Reader::new(&body);
        assert_eq!(r.string().unwrap(), "t1");
        assert_eq!(r.u8().unwrap(), ABORT_DISK_FULL);
        assert_eq!(r.string().unwrap(), "out of space");
        assert!(r.at_end());
    }

    #[test]
    fn h2c_envelope_passes_through_default_apc_stream() {
        // ApcStream defaults to MARKER_C2H; an H2C envelope must come
        // back as plain passthrough.
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_OK, 42, &[]);
        let env = wrap_h2c_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.payloads.is_empty());
        assert_eq!(out.passthrough, env);
    }

    #[test]
    fn c2h_envelope_round_trips_with_stuffing() {
        let mut frames = Vec::new();
        // Body deliberately contains ESC bytes to exercise stuffing.
        append_frame(&mut frames, RSP_OK, 0xDEAD_BEEF, &[0x1B, 0x00, 0x1B]);
        let env = wrap_c2h_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.passthrough.is_empty());
        assert_eq!(out.payloads.len(), 1);

        let mut r = Reader::new(&out.payloads[0]);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let payload_len = r.u32().unwrap();
        assert_eq!(payload_len as usize, frames.len());
        assert_eq!(r.u8().unwrap(), RSP_OK);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u32().unwrap(), 3);
    }

    #[test]
    fn upload_chunk_body_in_envelope_unstuffs_correctly() {
        // Build a c2h envelope carrying CMD_UPLOAD_CHUNK with an ESC in
        // the inner data, run through ApcStream, decode, and verify.
        use crate::command::{parse, Command};

        let mut body = Writer::new();
        body.str("vsend-1");
        body.u64(0);
        body.bytes(&[0x1B, b'[', b'H']);

        let mut frames = Vec::new();
        append_frame(&mut frames, CMD_UPLOAD_CHUNK, 7, &body.buf);
        let env = wrap_c2h_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _ = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let request_id = r.u32().unwrap();
        let body_len = r.u32().unwrap();
        let body_bytes = r.take(body_len as usize).unwrap();
        assert_eq!(frame_type, CMD_UPLOAD_CHUNK);
        assert_eq!(request_id, 7);

        let cmd = parse(frame_type, body_bytes).unwrap();
        let Command::UploadChunk(b) = cmd else {
            panic!();
        };
        assert_eq!(b.transfer_id, "vsend-1");
        assert_eq!(b.offset, 0);
        assert_eq!(b.data, vec![0x1B, b'[', b'H']);
    }
}
