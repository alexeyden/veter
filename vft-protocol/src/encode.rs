// Client-side command body encoders (§3, §6, §7, §8).
//
// Mirrors the decoders in `command.rs` so commands can be round-tripped
// in tests and emitted by clients (vsend, vrecv, future test CLIs).

use crate::codec::Writer;
use crate::command::{
    BeginDownloadBody, BeginUploadBody, CancelTransferBody, Command, EndUploadBody,
    ReportDownloadAckBody, RequestAckBody, UploadChunkBody,
};
use crate::envelope::{append_frame, wrap_c2h_envelope};
use crate::frame::*;

pub fn probe_body() -> Vec<u8> {
    Vec::new()
}

pub fn begin_upload_body(b: &BeginUploadBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(
        1 + b.transfer_id.len()
            + 1 + b.host_path.len()
            + 1 + b.basename.len()
            + 8 // total_bytes
            + 1 // flags
            + 4 // mode
            + 8, // mtime
    );
    w.str(&b.transfer_id);
    w.str(&b.host_path);
    w.str(&b.basename);
    w.u64(b.total_bytes);
    w.u8(b.flags);
    w.u32(b.mode);
    w.i64(b.mtime);
    w.buf
}

pub fn upload_chunk_body(b: &UploadChunkBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.transfer_id.len() + 8 + 5 + b.data.len());
    w.str(&b.transfer_id);
    w.u64(b.offset);
    w.bytes(&b.data);
    w.buf
}

pub fn end_upload_body(b: &EndUploadBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.transfer_id.len());
    w.str(&b.transfer_id);
    w.buf
}

pub fn begin_download_body(b: &BeginDownloadBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.transfer_id.len() + 1 + b.host_path.len() + 4);
    w.str(&b.transfer_id);
    w.str(&b.host_path);
    w.u32(b.chunk_size_hint);
    w.buf
}

pub fn report_download_ack_body(b: &ReportDownloadAckBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.transfer_id.len() + 8);
    w.str(&b.transfer_id);
    w.u64(b.bytes_confirmed);
    w.buf
}

pub fn request_ack_body(b: &RequestAckBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.transfer_id.len());
    w.str(&b.transfer_id);
    w.buf
}

pub fn cancel_transfer_body(b: &CancelTransferBody) -> Vec<u8> {
    let mut w = Writer::with_capacity(1 + b.transfer_id.len());
    w.str(&b.transfer_id);
    w.buf
}

/// Discriminate the wire frame type of a `Command` (§3 table).
pub fn frame_type_for(cmd: &Command) -> u8 {
    match cmd {
        Command::Probe => CMD_PROBE,
        Command::BeginUpload(_) => CMD_BEGIN_UPLOAD,
        Command::UploadChunk(_) => CMD_UPLOAD_CHUNK,
        Command::EndUpload(_) => CMD_END_UPLOAD,
        Command::BeginDownload(_) => CMD_BEGIN_DOWNLOAD,
        Command::ReportDownloadAck(_) => CMD_REPORT_DOWNLOAD_ACK,
        Command::RequestAck(_) => CMD_REQUEST_ACK,
        Command::CancelTransfer(_) => CMD_CANCEL_TRANSFER,
    }
}

/// Encode a `Command`'s body bytes (§6, §7, §8).
pub fn encode_command(cmd: &Command) -> Vec<u8> {
    match cmd {
        Command::Probe => probe_body(),
        Command::BeginUpload(b) => begin_upload_body(b),
        Command::UploadChunk(b) => upload_chunk_body(b),
        Command::EndUpload(b) => end_upload_body(b),
        Command::BeginDownload(b) => begin_download_body(b),
        Command::ReportDownloadAck(b) => report_download_ack_body(b),
        Command::RequestAck(b) => request_ack_body(b),
        Command::CancelTransfer(b) => cancel_transfer_body(b),
    }
}

/// Bundle a slice of `(command, request_id)` pairs into a single
/// client-to-host APC envelope ready to write to a PTY (§1.2).
pub fn build_envelope(commands: &[(Command, u32)]) -> Vec<u8> {
    let mut frames = Vec::new();
    for (cmd, req_id) in commands {
        let body = encode_command(cmd);
        append_frame(&mut frames, frame_type_for(cmd), *req_id, &body);
    }
    wrap_c2h_envelope(&frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::parse;

    #[test]
    fn begin_upload_round_trips() {
        let b = BeginUploadBody {
            transfer_id: "vsend-1".into(),
            host_path: "/home/user/foo.pdf".into(),
            basename: "".into(),
            total_bytes: 1_000_000,
            flags: FLAG_OVERWRITE,
            mode: 0o644,
            mtime: 1_700_000_000,
        };
        let body = begin_upload_body(&b);
        let cmd = parse(CMD_BEGIN_UPLOAD, &body).unwrap();
        let Command::BeginUpload(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn upload_chunk_round_trips_with_esc_bytes() {
        let b = UploadChunkBody {
            transfer_id: "t1".into(),
            offset: 0,
            data: vec![0x1B, b'[', b'H', 0x07, 0x1B],
        };
        let body = upload_chunk_body(&b);
        let cmd = parse(CMD_UPLOAD_CHUNK, &body).unwrap();
        let Command::UploadChunk(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn end_upload_round_trips() {
        let b = EndUploadBody {
            transfer_id: "t1".into(),
        };
        let body = end_upload_body(&b);
        let cmd = parse(CMD_END_UPLOAD, &body).unwrap();
        let Command::EndUpload(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn begin_download_round_trips() {
        let b = BeginDownloadBody {
            transfer_id: "vrecv-1".into(),
            host_path: "/var/log/syslog".into(),
            chunk_size_hint: 262_144,
        };
        let body = begin_download_body(&b);
        let cmd = parse(CMD_BEGIN_DOWNLOAD, &body).unwrap();
        let Command::BeginDownload(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn report_download_ack_round_trips() {
        let b = ReportDownloadAckBody {
            transfer_id: "vrecv-1".into(),
            bytes_confirmed: 1 << 40,
        };
        let body = report_download_ack_body(&b);
        let cmd = parse(CMD_REPORT_DOWNLOAD_ACK, &body).unwrap();
        let Command::ReportDownloadAck(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn request_ack_round_trips() {
        let b = RequestAckBody {
            transfer_id: "t1".into(),
        };
        let body = request_ack_body(&b);
        let cmd = parse(CMD_REQUEST_ACK, &body).unwrap();
        let Command::RequestAck(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn cancel_transfer_round_trips() {
        let b = CancelTransferBody {
            transfer_id: "t1".into(),
        };
        let body = cancel_transfer_body(&b);
        let cmd = parse(CMD_CANCEL_TRANSFER, &body).unwrap();
        let Command::CancelTransfer(parsed) = cmd else {
            panic!()
        };
        assert_eq!(parsed, b);
    }

    #[test]
    fn build_envelope_round_trips_through_apc_stream() {
        use crate::apc::ApcStream;
        use crate::codec::Reader;
        use crate::frame::{MARKER_C2H, PROTOCOL_VERSION};

        let probe = Command::Probe;
        let begin = Command::BeginUpload(BeginUploadBody {
            transfer_id: "t1".into(),
            host_path: "/tmp/x".into(),
            basename: "".into(),
            total_bytes: 1024,
            flags: 0,
            mode: 0,
            mtime: 0,
        });
        let env = build_envelope(&[(probe, 1), (begin, 2)]);

        let mut s = ApcStream::with_marker(*MARKER_C2H);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);
        let mut r = Reader::new(&out.payloads[0]);
        assert_eq!(r.u8().unwrap(), PROTOCOL_VERSION);
        let _payload_len = r.u32().unwrap();
        let mut request_ids = Vec::new();
        while !r.at_end() {
            let _ft = r.u8().unwrap();
            let rid = r.u32().unwrap();
            let body_len = r.u32().unwrap() as usize;
            r.take(body_len).unwrap();
            request_ids.push(rid);
        }
        assert_eq!(request_ids, vec![1, 2]);
    }

    #[test]
    fn build_envelope_with_chunk_carrying_esc_bytes() {
        use crate::apc::ApcStream;
        use crate::codec::Reader;
        use crate::frame::MARKER_C2H;

        let chunk = Command::UploadChunk(UploadChunkBody {
            transfer_id: "t1".into(),
            offset: 0,
            data: vec![0x1B; 8],
        });
        let env = build_envelope(&[(chunk, 1)]);

        let mut s = ApcStream::with_marker(*MARKER_C2H);
        let out = s.feed(&env);
        assert_eq!(out.payloads.len(), 1);

        let payload = &out.payloads[0];
        let mut r = Reader::new(payload);
        let _v = r.u8().unwrap();
        let _len = r.u32().unwrap();
        let frame_type = r.u8().unwrap();
        let _rid = r.u32().unwrap();
        let body_len = r.u32().unwrap() as usize;
        let body = r.take(body_len).unwrap();
        let cmd = parse(frame_type, body).unwrap();
        let Command::UploadChunk(b) = cmd else {
            panic!()
        };
        assert_eq!(b.data, vec![0x1B; 8]);
    }
}
