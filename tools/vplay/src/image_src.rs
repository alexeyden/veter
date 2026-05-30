//! A decoded source frame held in memory (RGBA8), used both as the
//! upload source and for the cursor colour readout.

use anyhow::{Context, Result};

#[derive(Clone)]
pub struct Frame {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn new(w: u32, h: u32, rgba: Vec<u8>) -> Self {
        Self { w, h, rgba }
    }

    /// RGBA of the pixel at `(x, y)`, clamped into bounds.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let x = x.min(self.w.saturating_sub(1));
        let y = y.min(self.h.saturating_sub(1));
        let i = ((y as usize) * (self.w as usize) + x as usize) * 4;
        if i + 4 <= self.rgba.len() {
            [
                self.rgba[i],
                self.rgba[i + 1],
                self.rgba[i + 2],
                self.rgba[i + 3],
            ]
        } else {
            [0, 0, 0, 0]
        }
    }
}

/// Decode a still image (PNG/JPEG/WebP) to an RGBA8 [`Frame`].
pub fn load_image(path: &std::path::Path) -> Result<Frame> {
    let dyn_img = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("inspecting {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        anyhow::bail!("image has zero extent");
    }
    Ok(Frame::new(w, h, rgba.into_raw()))
}
