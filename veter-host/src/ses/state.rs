// SES host engine: an APC envelope extractor + command dispatcher
// that sits in the per-portal (and host-level) byte pipeline. It
// extracts uppercase `SES` command envelopes a multiplexer client
// emits, answers each with a lowercase `ses` response, and surfaces
// `Detach` requests to the caller.
//
// One `SesEngine` lives at the host level and one per portal — same
// shape. Only the host-level engine of a `veterd` session carries a
// session name; `veter` and every per-portal scope use `new()` and
// report "not in a session".

use ses_protocol::{
    ERR_NOT_IN_SESSION, PROTOCOL_VERSION, apc::ApcStream, encode_host_frame, for_each_frame,
    frames::{Command, HostFrame},
};

/// Per-context SES engine.
pub struct SesEngine {
    apc: ApcStream,
    /// `Some(name)` for the host-level engine of a `veterd` session;
    /// `None` for `veter` and for every per-portal scope.
    session: Option<String>,
    /// `ses` response envelopes produced for commands seen so far.
    /// Drained by the caller and written back toward the client.
    pending_response_bytes: Vec<u8>,
    /// Count of `Detach` commands accepted since the last drain. Only
    /// ever non-zero on a session host.
    detach_requests: usize,
}

impl Default for SesEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SesEngine {
    /// A non-session engine: `Probe` reports `in_session = false` and
    /// `Detach` is refused. Used by `veter` and every per-portal scope.
    pub fn new() -> Self {
        Self {
            apc: ApcStream::new(),
            session: None,
            pending_response_bytes: Vec::new(),
            detach_requests: 0,
        }
    }

    /// A session engine: `Probe` reports `in_session = true` with
    /// `name`, and `Detach` is accepted. Used by `veterd` for the
    /// top-level engine of a session process.
    pub fn with_session(name: impl Into<String>) -> Self {
        Self {
            session: Some(name.into()),
            ..Self::new()
        }
    }

    /// Feed raw PTY bytes through the SES layer. Returns whatever bytes
    /// did not belong to a SES envelope so the caller can forward them
    /// to the next layer.
    pub fn process_pty_chunk(&mut self, input: &[u8]) -> Vec<u8> {
        let out = self.apc.feed(input);
        for payload in out.payloads {
            let mut frames: Vec<(u8, u32, Vec<u8>)> = Vec::new();
            let parsed = for_each_frame(&payload, |ft, rid, body| {
                frames.push((ft, rid, body.to_vec()));
                Ok::<(), u16>(())
            });
            if parsed.is_err() {
                // Malformed envelope payload — ignore it, matching the
                // permissive style of the VSS engine.
                continue;
            }
            for (ft, rid, body) in frames {
                self.dispatch(ft, rid, &body);
            }
        }
        out.passthrough
    }

    /// Drain pending `ses` response envelopes.
    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response_bytes)
    }

    /// Drain the count of `Detach` commands accepted since the last
    /// call. Non-zero only on a session host; the caller turns each
    /// into the session's detach teardown.
    pub fn take_detach_requests(&mut self) -> usize {
        std::mem::take(&mut self.detach_requests)
    }

    /// Whether this engine represents a named session.
    pub fn in_session(&self) -> bool {
        self.session.is_some()
    }

    fn dispatch(&mut self, frame_type: u8, request_id: u32, body: &[u8]) {
        let resp = match Command::parse(frame_type, body) {
            Ok(Command::Probe) => HostFrame::ProbeResponse {
                protocol_version: PROTOCOL_VERSION,
                features: 0,
                in_session: self.session.is_some(),
                name: self.session.clone().unwrap_or_default(),
            },
            Ok(Command::Detach) => {
                if self.session.is_some() {
                    self.detach_requests += 1;
                    HostFrame::Ok
                } else {
                    HostFrame::Err {
                        code: ERR_NOT_IN_SESSION,
                        msg: "host is not a session".to_string(),
                    }
                }
            }
            Err(code) => HostFrame::Err {
                code,
                msg: String::new(),
            },
        };
        let env = encode_host_frame(&resp, request_id);
        self.pending_response_bytes.extend_from_slice(&env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ses_protocol::{
        ERR_UNKNOWN_COMMAND, MARKER_H2C, apc::ApcStream as ClientApc, encode_command,
    };

    /// Decode every `ses` response envelope the engine produced.
    fn responses(engine: &mut SesEngine) -> Vec<HostFrame> {
        let bytes = engine.take_responses();
        let mut client = ClientApc::with_marker(*MARKER_H2C);
        let out = client.feed(&bytes);
        let mut frames = Vec::new();
        for payload in &out.payloads {
            for_each_frame(payload, |ft, _rid, body| {
                frames.push(HostFrame::parse(ft, body).map_err(|_| 0u16)?);
                Ok(())
            })
            .unwrap();
        }
        frames
    }

    #[test]
    fn probe_without_session() {
        let mut e = SesEngine::new();
        let pass = e.process_pty_chunk(&encode_command(&Command::Probe, 1));
        assert!(pass.is_empty());
        assert_eq!(
            responses(&mut e),
            vec![HostFrame::ProbeResponse {
                protocol_version: PROTOCOL_VERSION,
                features: 0,
                in_session: false,
                name: String::new(),
            }]
        );
    }

    #[test]
    fn probe_with_session_reports_name() {
        let mut e = SesEngine::with_session("cool");
        e.process_pty_chunk(&encode_command(&Command::Probe, 9));
        assert_eq!(
            responses(&mut e),
            vec![HostFrame::ProbeResponse {
                protocol_version: PROTOCOL_VERSION,
                features: 0,
                in_session: true,
                name: "cool".to_string(),
            }]
        );
    }

    #[test]
    fn detach_with_session_acks_and_counts() {
        let mut e = SesEngine::with_session("cool");
        e.process_pty_chunk(&encode_command(&Command::Detach, 2));
        assert_eq!(responses(&mut e), vec![HostFrame::Ok]);
        assert_eq!(e.take_detach_requests(), 1);
        assert_eq!(e.take_detach_requests(), 0);
    }

    #[test]
    fn detach_without_session_errors() {
        let mut e = SesEngine::new();
        e.process_pty_chunk(&encode_command(&Command::Detach, 3));
        assert_eq!(
            responses(&mut e),
            vec![HostFrame::Err {
                code: ERR_NOT_IN_SESSION,
                msg: "host is not a session".to_string(),
            }]
        );
        assert_eq!(e.take_detach_requests(), 0);
    }

    #[test]
    fn unknown_command_errors() {
        // A frame whose type is not a known command code.
        let mut frames = Vec::new();
        ses_protocol::append_frame(&mut frames, 0x7E, 4, &[]);
        let env = ses_protocol::wrap_c2h_envelope(&frames);
        let mut e = SesEngine::new();
        e.process_pty_chunk(&env);
        assert_eq!(
            responses(&mut e),
            vec![HostFrame::Err {
                code: ERR_UNKNOWN_COMMAND,
                msg: String::new(),
            }]
        );
    }

    #[test]
    fn foreign_apc_passes_through() {
        let prt = b"\x1b_PRTabc\x1b\\";
        let mut e = SesEngine::with_session("cool");
        assert_eq!(e.process_pty_chunk(prt), prt);
        assert!(e.take_responses().is_empty());
    }

    #[test]
    fn plain_text_passes_through() {
        let mut e = SesEngine::new();
        assert_eq!(e.process_pty_chunk(b"hello\x1b[2Jworld"), b"hello\x1b[2Jworld");
    }

    #[test]
    fn split_envelope_across_chunks() {
        let env = encode_command(&Command::Probe, 7);
        let mid = env.len() / 2;
        let mut e = SesEngine::with_session("s");
        assert!(e.process_pty_chunk(&env[..mid]).is_empty());
        assert!(e.process_pty_chunk(&env[mid..]).is_empty());
        assert_eq!(responses(&mut e).len(), 1);
    }
}
