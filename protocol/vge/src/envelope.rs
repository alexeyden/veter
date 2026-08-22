// APC envelope wrapping (§1.1–1.2) for both directions, plus
// per-response body builders (ProbeResponse, Err).

use crate::codec::{Reader, stuff, Writer};
use crate::frame::*;

/// What a `QueryHit` (§15) landed on. `None` for a miss — the point is
/// over nothing this scope painted.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct HitBody {
    pub hit: Option<Hit>,
}

/// One resolved hit: which draw command the point is over, and where
/// in it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct Hit {
    /// The element's id. Empty for an anonymous element (§6.1) — the
    /// client did not name it, so there is nothing to name back.
    pub element_id: String,
    /// Index of the draw command within that element (§6.3).
    pub command_index: u32,
    /// The point in the element's own coordinate space (§9.3), so it
    /// is directly comparable with the origins the client sent.
    pub local: crate::codec::Point,
    pub kind: HitKind,
}

/// The kind of drawable that was hit. Only `DrawText` and `DrawImage`
/// are hit-testable (§15); shapes are transparent to the pointer.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub enum HitKind {
    /// A `DrawText` run, with the character under the point given as a
    /// byte range into the run's text — the terminal shaped it, so
    /// this is the only place that range can come from.
    Text { byte_offset: u32, byte_len: u32 },
    Image,
}

const HIT_FLAG_HIT: u8 = 0b1;
const HIT_KIND_TEXT: u8 = 1;
const HIT_KIND_IMAGE: u8 = 2;

impl HitBody {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(32);
        let Some(h) = &self.hit else {
            w.u8(0);
            return w.buf;
        };
        w.u8(HIT_FLAG_HIT);
        w.str(&h.element_id);
        w.u32(h.command_index);
        w.f32(h.local.x);
        w.f32(h.local.y);
        match h.kind {
            HitKind::Text { byte_offset, byte_len } => {
                w.u8(HIT_KIND_TEXT);
                w.u32(byte_offset);
                w.u32(byte_len);
            }
            HitKind::Image => w.u8(HIT_KIND_IMAGE),
        }
        w.buf
    }

    pub fn decode(body: &[u8]) -> Result<Self, crate::codec::DecodeError> {
        let mut r = Reader::new(body);
        if r.u8()? & HIT_FLAG_HIT == 0 {
            return Ok(Self { hit: None });
        }
        let element_id = r.string()?.to_owned();
        let command_index = r.u32()?;
        let local = crate::codec::Point { x: r.f32()?, y: r.f32()? };
        let kind = match r.u8()? {
            HIT_KIND_TEXT => HitKind::Text {
                byte_offset: r.u32()?,
                byte_len: r.u32()?,
            },
            HIT_KIND_IMAGE => HitKind::Image,
            _ => return Err(crate::codec::DecodeError::bad_payload()),
        };
        Ok(Self {
            hit: Some(Hit {
                element_id,
                command_index,
                local,
                kind,
            }),
        })
    }
}

/// Build the body for a ProbeResponse (§2.1).
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
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
    /// Parent-child nesting cap (§9.7). 0 means parenting unsupported.
    pub max_nesting_depth: u8,
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
        w.u8(self.max_nesting_depth);
        w.buf
    }

    /// Inverse of [`Self::encode`]. Fields added after v0 are optional
    /// on the wire, so a short body from an older host decodes with
    /// those left at their defaults rather than failing.
    pub fn decode(body: &[u8]) -> Result<Self, crate::codec::DecodeError> {
        let mut r = Reader::new(body);
        Ok(Self {
            protocol_version: r.u16()?,
            cell_pixel_width: r.u16()?,
            cell_pixel_height: r.u16()?,
            scale_factor: r.f32()?,
            max_elements: r.u32()?,
            max_commands_per_element: r.u32()?,
            max_text_bytes: r.u32()?,
            max_image_bytes: r.u32()?,
            max_images: r.u32()?,
            supported_image_encodings: r.u8()?,
            max_nesting_depth: r.u8().unwrap_or(0),
        })
    }
}

/// Build the body for an Err response (§4).
pub fn err_body(error_code: u16, message: &str) -> Vec<u8> {
    let mut w = Writer::with_capacity(2 + 1 + message.len());
    w.u16(error_code);
    w.str(message);
    w.buf
}

/// Body for a ChunkAck response (§4). Emitted by the host after it
/// absorbs each `UploadImage` chunk, so the sender can show real
/// upload progress instead of guessing from the local stdout pipe.
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct ChunkAckBody<'a> {
    pub image_id: &'a str,
    /// Cumulative bytes received for this image id so far (this chunk
    /// included). When the host has seen the full payload, this equals
    /// `total_bytes` from the originating `UploadImageBody`.
    pub bytes_received: u32,
}

impl ChunkAckBody<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(1 + self.image_id.len() + 4);
        w.str(self.image_id);
        w.u32(self.bytes_received);
        w.buf
    }
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
/// commands into a veter session.
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
            protocol_version: 0,
            cell_pixel_width: 9,
            cell_pixel_height: 20,
            scale_factor: 1.0,
            max_elements: 4096,
            max_commands_per_element: 4096,
            max_text_bytes: 1_048_576,
            max_image_bytes: 0,
            max_images: 0,
            supported_image_encodings: 0,
            max_nesting_depth: 16,
        };
        assert_eq!(pb.encode().len(), 32);
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
