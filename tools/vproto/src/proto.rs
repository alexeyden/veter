//! Which protocol a run speaks, and the two directions of translation:
//! JSON command array → one client→terminal envelope, and a
//! terminal→client envelope → JSON.
//!
//! Each protocol keeps its own APC marker, so a single envelope carries
//! exactly one protocol's frames — which is why `--proto` selects one
//! for the whole invocation rather than tagging each command. Ordering
//! *between* protocols carries no meaning anyway: the host pipeline's
//! non-terminal stages are order-insensitive byte filters, and the one
//! stage that is ordered (VGE against the text parser) orders against
//! screen output, which this tool never emits.

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Proto {
    /// Vector graphics (`doc/vector-graphics-extension.md`).
    Vge,
    /// Portals / multiplexing (`doc/portal-extension.md`).
    Prt,
    /// Session control (`doc/session-extension.md`).
    Ses,
}

impl Proto {
    pub fn as_str(self) -> &'static str {
        match self {
            Proto::Vge => "vge",
            Proto::Prt => "prt",
            Proto::Ses => "ses",
        }
    }

    /// The terminal→client APC marker to listen for.
    pub fn response_marker(self) -> [u8; 3] {
        match self {
            Proto::Vge => *vge_protocol::frame::MARKER_T2C,
            Proto::Prt => *prt_protocol::frame::MARKER_T2C,
            Proto::Ses => *ses_protocol::frame::MARKER_H2C,
        }
    }
}

/// Deserialize `commands` for this protocol and bundle them into one
/// envelope. `ids` supplies the request id per command, already
/// resolved (auto-assigned, explicit, or the no-response sentinel).
pub fn encode(proto: Proto, commands: &[Value], ids: &[u32]) -> Result<Vec<u8>> {
    debug_assert_eq!(commands.len(), ids.len());
    // Each command is deserialized on its own so a failure names the
    // index the caller wrote, not an offset into a re-serialized blob.
    let at = |i: usize, e: serde_json::Error| {
        anyhow!("command[{i}]: {e}\n  (try `vproto schema --proto {}` for the shape)", proto.as_str())
    };
    Ok(match proto {
        Proto::Vge => {
            let mut pairs = Vec::with_capacity(commands.len());
            for (i, (v, id)) in commands.iter().zip(ids).enumerate() {
                let c: vge_protocol::command::Command =
                    serde_json::from_value(v.clone()).map_err(|e| at(i, e))?;
                pairs.push((c, *id));
            }
            vge_protocol::encode::build_envelope(&pairs)
        }
        Proto::Prt => {
            let mut pairs = Vec::with_capacity(commands.len());
            for (i, (v, id)) in commands.iter().zip(ids).enumerate() {
                let c: prt_protocol::command::Command =
                    serde_json::from_value(v.clone()).map_err(|e| at(i, e))?;
                pairs.push((c, *id));
            }
            prt_protocol::encode::build_envelope(&pairs)
        }
        Proto::Ses => {
            let mut frames = Vec::new();
            for (i, (v, id)) in commands.iter().zip(ids).enumerate() {
                let c: ses_protocol::Command =
                    serde_json::from_value(v.clone()).map_err(|e| at(i, e))?;
                ses_protocol::envelope::append_command(&mut frames, &c, *id);
            }
            ses_protocol::envelope::wrap_c2h_envelope(&frames)
        }
    })
}

/// One decoded response or event frame.
pub struct Frame {
    pub kind: u8,
    pub request_id: u32,
    pub body: Vec<u8>,
}

/// Split a response payload into frames. The header is the same shape
/// in all three protocols: `u8 version`, `u32 payload_len`, then
/// `(u8 type, u32 request_id, u32 body_len, body)` repeated.
pub fn parse_payload(proto: Proto, payload: &[u8]) -> Result<Vec<Frame>> {
    let mut r = vge_protocol::codec::Reader::new(payload);
    let version = r.u8().map_err(|_| anyhow!("response truncated at version byte"))?;
    let max = match proto {
        Proto::Vge => vge_protocol::frame::PROTOCOL_VERSION,
        Proto::Prt => prt_protocol::frame::PROTOCOL_VERSION,
        Proto::Ses => ses_protocol::frame::PROTOCOL_VERSION,
    };
    if version > max {
        bail!("response declares unsupported protocol_version {version} (this build speaks {max})");
    }
    let _len = r.u32().map_err(|_| anyhow!("response truncated at length field"))?;

    let mut frames = Vec::new();
    while !r.at_end() {
        let kind = r.u8().map_err(|_| anyhow!("frame truncated at type"))?;
        let request_id = r.u32().map_err(|_| anyhow!("frame truncated at request_id"))?;
        let body_len = r.u32().map_err(|_| anyhow!("frame truncated at body_len"))? as usize;
        let body = r
            .take(body_len)
            .map_err(|_| anyhow!("frame body truncated"))?
            .to_vec();
        frames.push(Frame { kind, request_id, body });
    }
    Ok(frames)
}

/// Render a frame as JSON.
///
/// `Ok`, `Err` and `Probe` are decoded structurally because those are
/// what a script branches on. Everything else — per-protocol events,
/// chunk acks, anything a future version adds — is reported with its
/// numeric type and the body verbatim in base64 rather than guessed at:
/// a testing tool that invents a shape for bytes it does not understand
/// is worse than one that hands them over.
pub fn frame_to_json(proto: Proto, f: &Frame) -> Value {
    let (rsp_ok, rsp_err, rsp_probe) = match proto {
        Proto::Vge => (
            vge_protocol::frame::RSP_OK,
            vge_protocol::frame::RSP_ERR,
            vge_protocol::frame::RSP_PROBE,
        ),
        Proto::Prt => (
            prt_protocol::frame::RSP_OK,
            prt_protocol::frame::RSP_ERR,
            prt_protocol::frame::RSP_PROBE,
        ),
        Proto::Ses => (
            ses_protocol::frame::RSP_OK,
            ses_protocol::frame::RSP_ERR,
            ses_protocol::frame::RSP_PROBE,
        ),
    };

    let mut out = json!({ "request_id": f.request_id });
    let obj = out.as_object_mut().expect("just built an object");

    if f.kind == rsp_ok {
        obj.insert("ok".into(), Value::Null);
    } else if f.kind == rsp_err {
        let mut r = vge_protocol::codec::Reader::new(&f.body);
        let code = r.u16().unwrap_or(0);
        let message = r.string().unwrap_or_default().to_owned();
        obj.insert("error".into(), json!({ "code": code, "message": message }));
    } else if f.kind == rsp_probe {
        obj.insert("probe".into(), probe_to_json(proto, &f.body));
    } else {
        obj.insert("frame_type".into(), json!(f.kind));
        obj.insert("body_base64".into(), json!(b64(&f.body)));
    }
    out
}

fn probe_to_json(proto: Proto, body: &[u8]) -> Value {
    match proto {
        Proto::Vge => match vge_protocol::envelope::ProbeBody::decode(body) {
            Ok(p) => serde_json::to_value(p).unwrap_or(Value::Null),
            Err(_) => json!({ "undecodable_base64": b64(body) }),
        },
        Proto::Prt => match prt_protocol::envelope::ProbeBody::decode(body) {
            Ok(p) => serde_json::to_value(p).unwrap_or(Value::Null),
            Err(_) => json!({ "undecodable_base64": b64(body) }),
        },
        // SES's probe body is a bare session-name string (§4).
        Proto::Ses => {
            let mut r = ses_protocol::codec::Reader::new(body);
            match r.string() {
                Ok(s) => json!({ "session": s }),
                Err(_) => json!({ "undecodable_base64": b64(body) }),
            }
        }
    }
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| anyhow!("data_base64 is not valid base64: {e}"))
}
