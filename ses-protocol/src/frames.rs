// Typed frame bodies for the SES extension. Two enums — one per
// direction — keep parse and encode together for each frame.

use super::codec::{Reader, Writer};
use super::frame::*;

/// Client → host commands (marker `SES`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// "Are you a session, and what is your name?" Empty body.
    Probe,
    /// "Detach this session." Empty body.
    Detach,
}

impl Command {
    pub fn frame_type(&self) -> u8 {
        match self {
            Command::Probe => CMD_PROBE,
            Command::Detach => CMD_DETACH,
        }
    }

    pub fn encode_body(&self) -> Vec<u8> {
        // Both commands have empty bodies in v0.
        Vec::new()
    }

    pub fn parse(frame_type: u8, body: &[u8]) -> Result<Self, u16> {
        let cmd = match frame_type {
            CMD_PROBE => Command::Probe,
            CMD_DETACH => Command::Detach,
            _ => return Err(ERR_UNKNOWN_COMMAND),
        };
        if !body.is_empty() {
            return Err(ERR_BAD_PAYLOAD);
        }
        Ok(cmd)
    }
}

/// Host → client responses (marker `ses`). Every command yields
/// exactly one response, with the command's `request_id` echoed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostFrame {
    /// Command succeeded.
    Ok,
    /// Command failed.
    Err { code: u16, msg: String },
    /// Answer to `Probe`. `in_session` is false for a plain `veter`
    /// host (and for any per-portal scope); true for a `vsd`
    /// session, in which case `name` is the session name. `features`
    /// is a reserved capability bitmask, `0` in v0.
    ProbeResponse {
        protocol_version: u8,
        features: u8,
        in_session: bool,
        name: String,
    },
}

impl HostFrame {
    pub fn frame_type(&self) -> u8 {
        match self {
            HostFrame::Ok => RSP_OK,
            HostFrame::Err { .. } => RSP_ERR,
            HostFrame::ProbeResponse { .. } => RSP_PROBE,
        }
    }

    pub fn encode_body(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            HostFrame::Ok => {}
            HostFrame::Err { code, msg } => {
                w.u16(*code);
                w.str(msg);
            }
            HostFrame::ProbeResponse {
                protocol_version,
                features,
                in_session,
                name,
            } => {
                w.u8(*protocol_version);
                w.u8(*features);
                w.u8(u8::from(*in_session));
                w.str(name);
            }
        }
        w.buf
    }

    pub fn parse(frame_type: u8, body: &[u8]) -> Result<Self, u16> {
        let mut r = Reader::new(body);
        let f = match frame_type {
            RSP_OK => HostFrame::Ok,
            RSP_ERR => HostFrame::Err {
                code: r.u16()?,
                msg: r.string()?.to_string(),
            },
            RSP_PROBE => HostFrame::ProbeResponse {
                protocol_version: r.u8()?,
                features: r.u8()?,
                in_session: r.u8()? != 0,
                name: r.string()?.to_string(),
            },
            _ => return Err(ERR_UNKNOWN_FRAME),
        };
        if !r.at_end() {
            return Err(ERR_BAD_PAYLOAD);
        }
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trip() {
        for c in &[Command::Probe, Command::Detach] {
            let body = c.encode_body();
            let parsed = Command::parse(c.frame_type(), &body).unwrap();
            assert_eq!(&parsed, c);
        }
    }

    #[test]
    fn unknown_command_errors() {
        assert_eq!(Command::parse(0x7F, &[]).unwrap_err(), ERR_UNKNOWN_COMMAND);
    }

    #[test]
    fn command_with_trailing_bytes_errors() {
        assert_eq!(
            Command::parse(CMD_PROBE, &[0xFF]).unwrap_err(),
            ERR_BAD_PAYLOAD
        );
    }

    #[test]
    fn ok_round_trip() {
        let f = HostFrame::Ok;
        let body = f.encode_body();
        assert!(body.is_empty());
        assert_eq!(HostFrame::parse(f.frame_type(), &body).unwrap(), f);
    }

    #[test]
    fn err_round_trip() {
        let f = HostFrame::Err {
            code: ERR_NOT_IN_SESSION,
            msg: "not a session".to_string(),
        };
        let body = f.encode_body();
        assert_eq!(HostFrame::parse(f.frame_type(), &body).unwrap(), f);
    }

    #[test]
    fn probe_response_round_trip() {
        for f in &[
            HostFrame::ProbeResponse {
                protocol_version: PROTOCOL_VERSION,
                features: 0,
                in_session: true,
                name: "cool".to_string(),
            },
            HostFrame::ProbeResponse {
                protocol_version: PROTOCOL_VERSION,
                features: 0,
                in_session: false,
                name: String::new(),
            },
        ] {
            let body = f.encode_body();
            let parsed = HostFrame::parse(f.frame_type(), &body).unwrap();
            assert_eq!(&parsed, f);
        }
    }

    #[test]
    fn unknown_host_frame_errors() {
        assert_eq!(HostFrame::parse(0x99, &[]).unwrap_err(), ERR_UNKNOWN_FRAME);
    }

    #[test]
    fn truncated_probe_response_errors() {
        assert_eq!(
            HostFrame::parse(RSP_PROBE, &[0x00]).unwrap_err(),
            ERR_BAD_PAYLOAD
        );
    }
}
