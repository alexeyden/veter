// APC envelope wrapping (§1.1–1.2) for both directions, plus
// per-response body builders (ProbeResponse, Err).

use crate::codec::{stuff, Writer};
use crate::frame::*;

/// Build the body for a ProbeResponse (§2.1).
pub struct ProbeBody {
    pub protocol_version: u16,
    pub cell_pixel_width: u16,
    pub cell_pixel_height: u16,
    pub scale_factor: f32,
    pub max_elements: u32,
    pub max_commands_per_element: u32,
    pub max_text_bytes: u32,
    pub max_image_bytes: u32,
    pub max_images: u32,
    pub supported_image_encodings: u8,
}

impl ProbeBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(32);
        w.u16(self.protocol_version);
        w.u16(self.cell_pixel_width);
        w.u16(self.cell_pixel_height);
        w.f32(self.scale_factor);
        w.u32(self.max_elements);
        w.u32(self.max_commands_per_element);
        w.u32(self.max_text_bytes);
        w.u32(self.max_image_bytes);
        w.u32(self.max_images);
        w.u8(self.supported_image_encodings);
        w.buf
    }
}

/// Build the body for an Err response (§4).
pub fn err_body(error_code: u16, message: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + 1 + message.len());
    w.u16(error_code);
    w.str(message);
    w.buf
}

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
    // §1.2: unstuffed payload = u8 protocol_version, u32 payload_length,
    // frames. payload_length is "length of the rest, in bytes" — i.e.
    // just the frames region.
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

/// Wrap a frame buffer as a terminal→client envelope (lowercase `vge`
/// marker). This is what the terminal emits in response to commands.
pub fn wrap_t2c_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_T2C)
}

/// Wrap a frame buffer as a client→terminal envelope (uppercase `VGE`
/// marker). Used by the test CLI and any client that wants to feed
/// commands into a vterm session.
pub fn wrap_c2t_envelope(frames_buf: &[u8]) -> Vec<u8> {
    wrap(frames_buf, MARKER_C2T)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apc::ApcStream;
    use crate::codec::Reader;

    #[test]
    fn probe_body_encoded_size() {
        let pb = ProbeBody {
            protocol_version: 1,
            cell_pixel_width: 9,
            cell_pixel_height: 20,
            scale_factor: 1.0,
            max_elements: 4096,
            max_commands_per_element: 4096,
            max_text_bytes: 1_048_576,
            max_image_bytes: 0,
            max_images: 0,
            supported_image_encodings: 0,
        };
        assert_eq!(pb.encode().len(), 31);
    }

    #[test]
    fn t2c_envelope_passes_through_apc_stream() {
        // ApcStream only recognizes the C2T marker; a T2C envelope must
        // come back as plain passthrough.
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_OK, 42, &[]);
        let env = wrap_t2c_envelope(&frames);

        let mut s = ApcStream::new();
        let out = s.feed(&env);
        assert!(out.payloads.is_empty());
        assert_eq!(out.passthrough, env);
    }

    #[test]
    fn c2t_envelope_round_trips_with_stuffing() {
        // Build a C2T envelope whose body has embedded ESCs, parse it
        // back via ApcStream, confirm we recover the original frames.
        let mut frames = Vec::new();
        append_frame(&mut frames, RSP_OK, 0xDEAD_BEEF, &[0x1B, 0x00, 0x1B]);
        let env = wrap_c2t_envelope(&frames);

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
}
