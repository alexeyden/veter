//! Image wire-encoding selection and payload encoding for VGE
//! `UploadImage`. The chunking/progress loop stays in the consuming
//! binary (vcat's progress bar, vplay's frame pump); this module just
//! turns RGBA8 into the `(encoding_byte, payload)` pair.

use anyhow::{Result, anyhow, bail};

/// A chosen wire encoding for an uploaded image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Encoding {
    /// Straight RGBA8 bytes — fastest to encode, biggest payload.
    Raw,
    /// Lossless WebP via the pure-Rust `zenwebp` encoder.
    WebpLossless,
    /// Lossy WebP at the given quality (0..=100).
    WebpLossy(f32),
}

impl Encoding {
    /// The protocol encoding byte (§8.2): 0x01 Raw, 0x02 WebP.
    pub fn wire_byte(self) -> u8 {
        match self {
            Encoding::Raw => 0x01,
            Encoding::WebpLossless | Encoding::WebpLossy(_) => 0x02,
        }
    }
}

/// Pick a default encoding when the caller hasn't forced one: WebP-lossy
/// over SSH (small payload matters on the wire), Raw locally (sub-ms
/// round-trip makes encode time the only cost). `supported` is the
/// probe's `supported_image_encodings` bitmask; if WebP isn't supported
/// we fall back to Raw regardless.
pub fn choose_encoding(supported: u8, ssh: bool, default_quality: f32) -> Encoding {
    let webp_ok = supported & 0x02 != 0;
    if ssh && webp_ok {
        Encoding::WebpLossy(default_quality)
    } else {
        Encoding::Raw
    }
}

/// Encode an RGBA8 buffer (`w*h*4` bytes) into `(wire_byte, payload)`
/// for `UploadImage`.
pub fn encode_payload(rgba: Vec<u8>, w: u32, h: u32, enc: Encoding) -> Result<(u8, Vec<u8>)> {
    match enc {
        Encoding::Raw => Ok((0x01, rgba)),
        Encoding::WebpLossless => {
            let cfg = zenwebp::LosslessConfig::new();
            let out =
                zenwebp::EncodeRequest::lossless(&cfg, &rgba, zenwebp::PixelLayout::Rgba8, w, h)
                    .encode()
                    .map_err(|e| anyhow!("webp lossless encode: {e}"))?;
            Ok((0x02, out))
        }
        Encoding::WebpLossy(quality) => {
            if !quality.is_finite() || !(0.0..=100.0).contains(&quality) {
                bail!("quality must be in 0..=100, got {}", quality);
            }
            let cfg = zenwebp::LossyConfig::new().with_quality(quality);
            let out = zenwebp::EncodeRequest::lossy(&cfg, &rgba, zenwebp::PixelLayout::Rgba8, w, h)
                .encode()
                .map_err(|e| anyhow!("webp lossy encode: {e}"))?;
            Ok((0x02, out))
        }
    }
}
