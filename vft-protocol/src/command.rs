// Typed command parsing for VFT frames (§3, §6, §7, §8).

use super::codec::{DecodeError, DecodeResult, Reader};
use super::frame::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeginUploadBody {
    pub transfer_id: String,
    /// "" = deferred form (host chooses destination); see §6.1.
    pub host_path: String,
    /// Filename hint used in the deferred form; ignored when
    /// `host_path` is non-empty.
    pub basename: String,
    /// Declared total size; 0 means unknown.
    pub total_bytes: u64,
    /// `FLAG_OVERWRITE` (bit 0). Reserved bits must be 0; the decoder
    /// rejects non-zero reserved bits with `err_bad_payload`.
    pub flags: u8,
    /// POSIX permission bits the client wants; 0 = host default.
    pub mode: u32,
    /// Modification time stamp, seconds since the UNIX epoch; 0 = host
    /// default (typically wall-clock time at finalisation).
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadChunkBody {
    pub transfer_id: String,
    pub offset: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndUploadBody {
    pub transfer_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeginDownloadBody {
    pub transfer_id: String,
    /// "" = deferred form (host shows file picker); see §7.1.
    pub host_path: String,
    /// Preferred `DownloadChunk` body size; 0 = host chooses.
    pub chunk_size_hint: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportDownloadAckBody {
    pub transfer_id: String,
    pub bytes_confirmed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestAckBody {
    pub transfer_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelTransferBody {
    pub transfer_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Probe,
    BeginUpload(BeginUploadBody),
    UploadChunk(UploadChunkBody),
    EndUpload(EndUploadBody),
    BeginDownload(BeginDownloadBody),
    ReportDownloadAck(ReportDownloadAckBody),
    RequestAck(RequestAckBody),
    CancelTransfer(CancelTransferBody),
}

fn read_id(r: &mut Reader<'_>) -> DecodeResult<String> {
    let s = r.string()?;
    // §5.2 — transfer IDs are non-empty in every command that
    // references one.
    if s.is_empty() {
        return Err(DecodeError::bad_payload());
    }
    if s.len() > MAX_ID_BYTES {
        return Err(DecodeError::bad_payload());
    }
    Ok(s.to_owned())
}

fn read_path(r: &mut Reader<'_>) -> DecodeResult<String> {
    // Paths may be empty (deferred form, §5.3); just decode UTF-8.
    Ok(r.string()?.to_owned())
}

/// Parse one frame body given its `frame_type`. Returns the decoded
/// command on success, or an `error_code` (one of the `ERR_*` constants
/// in §4.1) on failure.
///
/// The caller is responsible for surfacing `request_id` to the client
/// in the matching error response — we don't see it here.
pub fn parse(frame_type: u8, body: &[u8]) -> Result<Command, u16> {
    let mut r = Reader::new(body);
    match frame_type {
        CMD_PROBE => {
            // Body is empty (§2.1). Strict on trailing bytes.
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::Probe)
        }
        CMD_BEGIN_UPLOAD => {
            let transfer_id = read_id(&mut r)?;
            let host_path = read_path(&mut r)?;
            let basename = read_path(&mut r)?;
            let total_bytes = r.u64()?;
            let flags = r.u8()?;
            // §6.1 — reserved bits must be zero.
            if flags & !FLAG_OVERWRITE != 0 {
                return Err(ERR_BAD_PAYLOAD);
            }
            let mode = r.u32()?;
            let mtime = r.i64()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::BeginUpload(BeginUploadBody {
                transfer_id,
                host_path,
                basename,
                total_bytes,
                flags,
                mode,
                mtime,
            }))
        }
        CMD_UPLOAD_CHUNK => {
            let transfer_id = read_id(&mut r)?;
            let offset = r.u64()?;
            let data = r.bytes()?.to_vec();
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::UploadChunk(UploadChunkBody {
                transfer_id,
                offset,
                data,
            }))
        }
        CMD_END_UPLOAD => {
            let transfer_id = read_id(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::EndUpload(EndUploadBody { transfer_id }))
        }
        CMD_BEGIN_DOWNLOAD => {
            let transfer_id = read_id(&mut r)?;
            let host_path = read_path(&mut r)?;
            let chunk_size_hint = r.u32()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::BeginDownload(BeginDownloadBody {
                transfer_id,
                host_path,
                chunk_size_hint,
            }))
        }
        CMD_REPORT_DOWNLOAD_ACK => {
            let transfer_id = read_id(&mut r)?;
            let bytes_confirmed = r.u64()?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::ReportDownloadAck(ReportDownloadAckBody {
                transfer_id,
                bytes_confirmed,
            }))
        }
        CMD_REQUEST_ACK => {
            let transfer_id = read_id(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::RequestAck(RequestAckBody { transfer_id }))
        }
        CMD_CANCEL_TRANSFER => {
            let transfer_id = read_id(&mut r)?;
            if !r.at_end() {
                return Err(ERR_BAD_PAYLOAD);
            }
            Ok(Command::CancelTransfer(CancelTransferBody { transfer_id }))
        }
        // §3 — frame types in 0x80..=0xFF are events and MUST NOT
        // appear in client-to-host envelopes. Treat as unknown command.
        _ => Err(ERR_UNKNOWN_COMMAND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Writer;

    fn parse_one(frame_type: u8, w: Writer) -> Result<Command, u16> {
        parse(frame_type, &w.buf)
    }

    #[test]
    fn probe_empty_body() {
        let cmd = parse(CMD_PROBE, &[]).unwrap();
        assert!(matches!(cmd, Command::Probe));
    }

    #[test]
    fn probe_rejects_trailing_bytes() {
        assert_eq!(parse(CMD_PROBE, &[0x00]).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn begin_upload_roundtrip_explicit_form() {
        let mut w = Writer::new();
        w.str("vsend-1");
        w.str("/home/user/foo.pdf");
        w.str("");
        w.u64(1_000_000);
        w.u8(FLAG_OVERWRITE);
        w.u32(0o644);
        w.i64(1_700_000_000);
        let cmd = parse_one(CMD_BEGIN_UPLOAD, w).unwrap();
        let Command::BeginUpload(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "vsend-1");
        assert_eq!(body.host_path, "/home/user/foo.pdf");
        assert_eq!(body.basename, "");
        assert_eq!(body.total_bytes, 1_000_000);
        assert_eq!(body.flags, FLAG_OVERWRITE);
        assert_eq!(body.mode, 0o644);
        assert_eq!(body.mtime, 1_700_000_000);
    }

    #[test]
    fn begin_upload_roundtrip_deferred_form() {
        // Empty host_path is the deferred form (§6.1).
        let mut w = Writer::new();
        w.str("vsend-1");
        w.str("");
        w.str("screenshot.png");
        w.u64(0); // unknown size
        w.u8(0);
        w.u32(0);
        w.i64(0);
        let cmd = parse_one(CMD_BEGIN_UPLOAD, w).unwrap();
        let Command::BeginUpload(body) = cmd else {
            panic!()
        };
        assert_eq!(body.host_path, "");
        assert_eq!(body.basename, "screenshot.png");
        assert_eq!(body.total_bytes, 0);
        assert_eq!(body.flags, 0);
        assert_eq!(body.mode, 0);
        assert_eq!(body.mtime, 0);
    }

    #[test]
    fn begin_upload_rejects_reserved_flag_bits() {
        let mut w = Writer::new();
        w.str("t");
        w.str("");
        w.str("");
        w.u64(0);
        w.u8(0x02); // bit 1 reserved
        w.u32(0);
        w.i64(0);
        assert_eq!(parse_one(CMD_BEGIN_UPLOAD, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn upload_chunk_roundtrip() {
        let mut w = Writer::new();
        w.str("t1");
        w.u64(4096);
        w.bytes(&[0x1B, 0x00, 0xFF]);
        let cmd = parse_one(CMD_UPLOAD_CHUNK, w).unwrap();
        let Command::UploadChunk(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "t1");
        assert_eq!(body.offset, 4096);
        assert_eq!(body.data, vec![0x1B, 0x00, 0xFF]);
    }

    #[test]
    fn end_upload_roundtrip() {
        let mut w = Writer::new();
        w.str("t1");
        let cmd = parse_one(CMD_END_UPLOAD, w).unwrap();
        let Command::EndUpload(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "t1");
    }

    #[test]
    fn begin_download_roundtrip_explicit_form() {
        let mut w = Writer::new();
        w.str("vrecv-1");
        w.str("/var/log/syslog");
        w.u32(262_144);
        let cmd = parse_one(CMD_BEGIN_DOWNLOAD, w).unwrap();
        let Command::BeginDownload(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "vrecv-1");
        assert_eq!(body.host_path, "/var/log/syslog");
        assert_eq!(body.chunk_size_hint, 262_144);
    }

    #[test]
    fn begin_download_roundtrip_deferred_form() {
        let mut w = Writer::new();
        w.str("vrecv-1");
        w.str("");
        w.u32(0);
        let cmd = parse_one(CMD_BEGIN_DOWNLOAD, w).unwrap();
        let Command::BeginDownload(body) = cmd else {
            panic!()
        };
        assert_eq!(body.host_path, "");
        assert_eq!(body.chunk_size_hint, 0);
    }

    #[test]
    fn report_download_ack_roundtrip() {
        let mut w = Writer::new();
        w.str("vrecv-1");
        w.u64(8_388_608);
        let cmd = parse_one(CMD_REPORT_DOWNLOAD_ACK, w).unwrap();
        let Command::ReportDownloadAck(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "vrecv-1");
        assert_eq!(body.bytes_confirmed, 8_388_608);
    }

    #[test]
    fn request_ack_roundtrip() {
        let mut w = Writer::new();
        w.str("t1");
        let cmd = parse_one(CMD_REQUEST_ACK, w).unwrap();
        let Command::RequestAck(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "t1");
    }

    #[test]
    fn cancel_transfer_roundtrip() {
        let mut w = Writer::new();
        w.str("t1");
        let cmd = parse_one(CMD_CANCEL_TRANSFER, w).unwrap();
        let Command::CancelTransfer(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id, "t1");
    }

    #[test]
    fn empty_id_is_bad_payload() {
        let mut w = Writer::new();
        w.str("");
        assert_eq!(parse_one(CMD_END_UPLOAD, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn id_too_long_is_bad_payload() {
        let mut w = Writer::new();
        w.str(&"x".repeat(MAX_ID_BYTES + 1));
        assert_eq!(parse_one(CMD_END_UPLOAD, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn id_at_cap_is_accepted() {
        let mut w = Writer::new();
        w.str(&"x".repeat(MAX_ID_BYTES));
        let cmd = parse_one(CMD_END_UPLOAD, w).unwrap();
        let Command::EndUpload(body) = cmd else {
            panic!()
        };
        assert_eq!(body.transfer_id.len(), MAX_ID_BYTES);
    }

    #[test]
    fn unknown_frame_type_is_unknown_command() {
        // Reserved client-to-host range.
        assert_eq!(parse(0x7F, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
        // Event range — never legal client→host (§3).
        assert_eq!(parse(0x80, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
        assert_eq!(parse(0xFF, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
    }

    #[test]
    fn truncated_body_is_bad_payload() {
        // BeginUpload truncated mid-body.
        let mut w = Writer::new();
        w.str("t");
        w.str("");
        w.str("");
        w.u64(0);
        // ... missing flags+mode+mtime
        assert_eq!(parse_one(CMD_BEGIN_UPLOAD, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }

    #[test]
    fn trailing_bytes_after_complete_body_is_bad_payload() {
        // EndUpload + extra byte.
        let mut w = Writer::new();
        w.str("t");
        w.u8(0xAA);
        assert_eq!(parse_one(CMD_END_UPLOAD, w).unwrap_err(), ERR_BAD_PAYLOAD);
    }
}
