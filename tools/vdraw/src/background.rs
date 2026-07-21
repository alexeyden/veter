//! Optional `--background IMAGE_PATH` reference image.
//!
//! The image is uploaded once at startup and drawn by a single element
//! parented to the `canvas` (like every document element), so it pans
//! and zooms with the drawing rather than staying pinned to the screen.
//! It is fitted to the pane on open and never re-fitted on resize — its
//! geometry lives in canvas cells, so the camera is the only thing that
//! moves it afterwards.
//!
//! The element sits below every shape (`BACKGROUND_ORDER`), is not part
//! of `document.elements`, and so is never hit-tested, selected, saved
//! or undone. `ClearAll` (used by `full_render` on undo/redo) wipes the
//! element but leaves the image table untouched (§6.7), so the picture
//! only needs uploading once — later frames just re-create the element.

use std::path::Path;

use anyhow::{Context, Result};
use image::ImageReader;
use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Command, CreateElementBody, DrawCmd, UploadImageBody};
use vge_protocol::frame::REQ_ID_NO_RESPONSE;
use vge_render::is_ssh_session;
use vge_render::probe::ProbeData;
use vge_render::upload::{choose_encoding, encode_payload};

use crate::camera::Camera;
use crate::render::CANVAS_ID;

pub const BACKGROUND_ID: &str = "canvas.background";
/// Below every shape (document elements start at draw order 1) but above
/// the canvas anchor itself (order 0, which carries no geometry).
pub const BACKGROUND_ORDER: i32 = -1;

/// Never upload more pixels than this on the longest side. Zoom tops out
/// at `MAX_ZOOM`, so beyond a few thousand pixels extra detail is never
/// visible; the cap keeps the startup upload bounded on large photos.
const MAX_UPLOAD_DIM: u32 = 4096;
/// Fallback raw-byte budget when the probe advertises none (0). Kept
/// well under the spec's recommended 32 MiB host default so a large
/// screenshot isn't rejected outright by a host we can't interrogate.
const DEFAULT_MAX_IMAGE_BYTES: u32 = 16 * 1024 * 1024;
/// WebP quality used when uploading over SSH, matching vcat's default.
const SSH_WEBP_QUALITY: f32 = 75.0;
/// Payload chunk size over SSH (§8.1); local runs send a single chunk.
const SSH_CHUNK_BYTES: u32 = 32 * 1024;

/// A loaded, fitted background image, ready to (re)draw as an element.
pub struct Background {
    element: CreateElementBody,
}

impl Background {
    /// Decode `path`, fit it into the `cols × rows` pane preserving its
    /// visual aspect ratio, and return the background plus the image
    /// upload commands. The upload commands MUST be sent before the
    /// element (and before the first `full_render`); the image then
    /// survives every later `ClearAll`.
    pub fn load(
        path: &Path,
        cam: &Camera,
        cols: u16,
        rows: u16,
        probe: &ProbeData,
    ) -> Result<(Self, Vec<(Command, u32)>)> {
        let rgba = ImageReader::open(path)
            .with_context(|| format!("opening {}", path.display()))?
            .with_guessed_format()
            .with_context(|| format!("reading {}", path.display()))?
            .decode()
            .with_context(|| format!("decoding {}", path.display()))?
            .to_rgba8();
        let (w_px, h_px) = (rgba.width(), rgba.height());
        if w_px == 0 || h_px == 0 {
            anyhow::bail!("{} has zero size", path.display());
        }

        // Fit the image into the pane preserving its *visual* aspect on
        // the anisotropic cell grid. At open, zoom=1 / pan=0, so screen
        // cells equal canvas cells; the element is parented to the canvas
        // and thus scales with any later zoom/pan.
        let pane_px_w = cols as f32 * cam.cell_w;
        let pane_px_h = rows as f32 * cam.cell_h;
        let scale = (pane_px_w / w_px as f32).min(pane_px_h / h_px as f32);
        let w_cells = w_px as f32 * scale / cam.cell_w;
        let h_cells = h_px as f32 * scale / cam.cell_h;
        // Centre it in the pane.
        let target_rect = Rect {
            x: (cols as f32 - w_cells) / 2.0,
            y: (rows as f32 - h_cells) / 2.0,
            w: w_cells,
            h: h_cells,
        };

        // Upload at native resolution, bounded by the terminal's image
        // byte budget and a hard dimension cap so a huge photo can't
        // stall startup with a multi-megabyte payload.
        let (up_w, up_h) = upload_dims(w_px, h_px, probe);
        let resized = if (up_w, up_h) != (w_px, h_px) {
            image::imageops::resize(&rgba, up_w, up_h, image::imageops::FilterType::Lanczos3)
        } else {
            rgba
        };

        let enc = choose_encoding(
            probe.supported_image_encodings,
            is_ssh_session(),
            SSH_WEBP_QUALITY,
        );
        let (encoding, payload) = encode_payload(resized.into_raw(), up_w, up_h, enc)?;

        // The image table lives in the terminal (a persistent vmux portal
        // outlives any one vdraw), so a fixed id collides with a stale
        // upload from an earlier run — the host answers a re-upload with
        // `err_duplicate_image_id` and keeps the *old* picture. Drop the
        // id first so every run replaces it. A miss is a swallowed no-op
        // (the frame is quiet), so this is safe on the first run too.
        let mut uploads = vec![(
            Command::DropImage {
                id: BACKGROUND_ID.into(),
            },
            REQ_ID_NO_RESPONSE,
        )];
        uploads.extend(chunk_uploads(BACKGROUND_ID, encoding, up_w, up_h, payload));

        let element = CreateElementBody {
            id: BACKGROUND_ID.into(),
            commands: vec![DrawCmd::DrawImage {
                target_rect,
                image_id: BACKGROUND_ID.into(),
                source_rect: None,
            }],
            origin: Point { x: 0.0, y: 0.0 },
            is_visible: true,
            draw_order: BACKGROUND_ORDER,
            parent: Some(CANVAS_ID.into()),
            size: None,
            transform: None,
        };

        Ok((Self { element }, uploads))
    }

    /// The `CreateElement` for the background, re-emitted by every
    /// `full_render` (the image itself is already resident on the host).
    pub fn element(&self) -> Command {
        Command::CreateElement(self.element.clone())
    }
}

/// Pixel dimensions to upload: native, clamped to `MAX_UPLOAD_DIM` on the
/// longest side and to the host's raw-byte budget when it advertises one.
fn upload_dims(w: u32, h: u32, probe: &ProbeData) -> (u32, u32) {
    let mut scale = 1.0f32;

    let longest = w.max(h) as f32;
    if longest > MAX_UPLOAD_DIM as f32 {
        scale = scale.min(MAX_UPLOAD_DIM as f32 / longest);
    }

    // Keep the raw RGBA footprint under the host's image-byte limit. The
    // host rejects `total_bytes > max_image_bytes` outright, and a local
    // (Raw) upload's `total_bytes` *is* the raw footprint, so shrink to
    // fit. The 0.95 margin keeps rounding from landing us back over the
    // strict limit.
    let budget = if probe.max_image_bytes > 0 {
        probe.max_image_bytes
    } else {
        DEFAULT_MAX_IMAGE_BYTES
    } as f32
        * 0.95;
    let raw = w as f32 * h as f32 * 4.0;
    if raw > budget {
        scale = scale.min((budget / raw).sqrt());
    }

    if scale >= 1.0 {
        return (w, h);
    }
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    (nw, nh)
}

/// Slice the payload into `UploadImage` chunks (§8.1): ~32 KB each over
/// SSH so a large image streams politely, a single chunk locally. Every
/// chunk is quiet (`REQ_ID_NO_RESPONSE`) so no ack frames land back on
/// vdraw's stdin, where the input parser would choke on them.
fn chunk_uploads(
    id: &str,
    encoding: u8,
    width: u32,
    height: u32,
    payload: Vec<u8>,
) -> Vec<(Command, u32)> {
    let total_bytes = payload.len() as u32;
    let chunk_size = if is_ssh_session() {
        SSH_CHUNK_BYTES.min(total_bytes.max(1))
    } else {
        total_bytes.max(1)
    };
    let num_chunks = total_bytes.div_ceil(chunk_size).max(1);

    (0..num_chunks)
        .map(|i| {
            let offset = i * chunk_size;
            let end = (offset + chunk_size).min(total_bytes);
            (
                Command::UploadImage(UploadImageBody {
                    id: id.to_string(),
                    encoding,
                    width,
                    height,
                    total_bytes,
                    chunk_offset: offset,
                    is_last: i == num_chunks - 1,
                    data: payload[offset as usize..end as usize].to_vec(),
                }),
                REQ_ID_NO_RESPONSE,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(max_image_bytes: u32) -> ProbeData {
        ProbeData {
            cell_pixel_width: 8,
            cell_pixel_height: 17,
            scale_factor: 1.0,
            max_image_bytes,
            max_images: 16,
            supported_image_encodings: 0x01,
            max_nesting_depth: 8,
        }
    }

    #[test]
    fn upload_dims_keep_small_images_native() {
        assert_eq!(upload_dims(640, 480, &probe(0)), (640, 480));
    }

    #[test]
    fn upload_dims_clamp_to_the_dimension_cap() {
        // A huge byte budget so only the dimension cap binds.
        let (w, h) = upload_dims(8000, 4000, &probe(u32::MAX));
        assert_eq!(w.max(h), MAX_UPLOAD_DIM);
        // Aspect ratio preserved.
        assert!((w as f32 / h as f32 - 2.0).abs() < 0.01);
    }

    #[test]
    fn upload_dims_apply_a_default_budget_when_probe_advertises_none() {
        // probe(0) now falls back to DEFAULT_MAX_IMAGE_BYTES rather than
        // uploading an unbounded raw footprint.
        let (w, h) = upload_dims(4000, 4000, &probe(0));
        assert!(w as f32 * h as f32 * 4.0 <= DEFAULT_MAX_IMAGE_BYTES as f32);
    }

    #[test]
    fn upload_dims_respect_the_byte_budget() {
        // 2000×2000×4 = 16 MB raw; budget of 4 MB halves each side.
        let (w, h) = upload_dims(2000, 2000, &probe(4 * 1024 * 1024));
        assert!(w as f32 * h as f32 * 4.0 <= 4.0 * 1024.0 * 1024.0 + 1.0);
        assert_eq!(w, h);
    }

    #[test]
    fn single_chunk_when_local() {
        // Outside SSH the whole payload goes in one chunk.
        if is_ssh_session() {
            return;
        }
        let ups = chunk_uploads(BACKGROUND_ID, 0x01, 2, 2, vec![0u8; 16]);
        assert_eq!(ups.len(), 1);
        let Command::UploadImage(b) = &ups[0].0 else {
            panic!("expected UploadImage");
        };
        assert!(b.is_last);
        assert_eq!(b.chunk_offset, 0);
        assert_eq!(b.total_bytes, 16);
    }
}
