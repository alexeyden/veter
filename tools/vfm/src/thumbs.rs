//! Thumbnail decoding, off the event loop.
//!
//! Decoding a directory full of JPEGs takes far longer than a frame, so
//! it happens on a small pool of worker threads: the grid asks for the
//! thumbnails it can see, workers decode and downscale them to RGBA, and
//! the event loop drains finished work each tick and uploads it as a VGE
//! image. Until one arrives the tile draws its file-type icon, so the
//! grid is never blocked on pixels.
//!
//! Video posters come from `ffmpeg` when it is on `$PATH` — one frame,
//! seeked a little way in so the poster isn't a black lead-in.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};

/// Identifies a file *version*: a thumbnail is stale once the file's
/// size or mtime changes.
pub type Stamp = (u64, u64);

/// How a tile's picture is doing.
#[derive(Debug, Clone)]
pub enum Slot {
    /// A worker is on it.
    Pending,
    /// Uploaded and drawable.
    Ready {
        image_id: String,
        w: u32,
        h: u32,
    },
    /// Decoding failed (corrupt file, no ffmpeg, unsupported codec).
    /// Remembered so we don't retry it every frame.
    Failed,
}

/// A decoded thumbnail on its way to the terminal.
pub struct Decoded {
    pub path: PathBuf,
    pub stamp: Stamp,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

struct Job {
    path: PathBuf,
    stamp: Stamp,
    video: bool,
    target_px: u32,
}

struct Done {
    path: PathBuf,
    stamp: Stamp,
    pixels: Option<(u32, u32, Vec<u8>)>,
}

pub struct Thumbs {
    target_px: u32,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    slots: HashMap<PathBuf, (Stamp, Slot)>,
    /// Ready thumbnails in touch order, oldest first — the eviction
    /// queue that keeps us under the host's image budget.
    lru: VecDeque<PathBuf>,
    max_live: usize,
    in_flight: usize,
    max_in_flight: usize,
    next_id: u64,
    have_ffmpeg: bool,
}

impl Thumbs {
    /// Start `workers` decoder threads producing thumbnails whose long
    /// edge is at most `target_px`, keeping at most `max_live` uploaded
    /// at once.
    pub fn new(target_px: u32, workers: usize, max_live: usize) -> Self {
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        let shared = Arc::new(Mutex::new(jobs_rx));
        for _ in 0..workers.max(1) {
            let rx = Arc::clone(&shared);
            let tx = done_tx.clone();
            std::thread::spawn(move || {
                loop {
                    // Hold the lock only long enough to take one job, so
                    // the other workers keep pulling while this one
                    // decodes.
                    let job = {
                        let guard = match rx.lock() {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                        match guard.recv() {
                            Ok(j) => j,
                            Err(_) => return, // sender dropped: shut down
                        }
                    };
                    let pixels = decode(&job.path, job.video, job.target_px);
                    if tx
                        .send(Done {
                            path: job.path,
                            stamp: job.stamp,
                            pixels,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        Thumbs {
            target_px,
            jobs: jobs_tx,
            done: done_rx,
            slots: HashMap::new(),
            lru: VecDeque::new(),
            max_live,
            in_flight: 0,
            max_in_flight: workers.max(1) * 4,
            next_id: 0,
            have_ffmpeg: which("ffmpeg"),
        }
    }

    /// The slot for `path`, if its thumbnail is current for `stamp`.
    pub fn slot(&self, path: &Path, stamp: Stamp) -> Option<&Slot> {
        match self.slots.get(path) {
            Some((s, slot)) if *s == stamp => Some(slot),
            _ => None,
        }
    }

    /// Ask for a thumbnail of `path`. Cheap to call every frame for
    /// every visible tile: already-known files are ignored, and requests
    /// stop once the workers' queue is deep enough that more would just
    /// pile up behind a scroll that has already moved on.
    pub fn request(&mut self, path: &Path, stamp: Stamp, video: bool) {
        if video && !self.have_ffmpeg {
            self.slots
                .insert(path.to_path_buf(), (stamp, Slot::Failed));
            return;
        }
        if self.slot(path, stamp).is_some() {
            return;
        }
        if self.in_flight >= self.max_in_flight {
            return;
        }
        self.slots
            .insert(path.to_path_buf(), (stamp, Slot::Pending));
        self.in_flight += 1;
        let _ = self.jobs.send(Job {
            path: path.to_path_buf(),
            stamp,
            video,
            target_px: self.target_px,
        });
    }

    /// Collect everything the workers finished since the last call. The
    /// caller uploads each [`Decoded`] and then calls [`Thumbs::ready`]
    /// with the image id it used.
    pub fn drain(&mut self) -> Vec<Decoded> {
        let mut out = Vec::new();
        loop {
            match self.done.try_recv() {
                Ok(d) => {
                    self.in_flight = self.in_flight.saturating_sub(1);
                    match d.pixels {
                        Some((w, h, rgba)) => out.push(Decoded {
                            path: d.path,
                            stamp: d.stamp,
                            w,
                            h,
                            rgba,
                        }),
                        None => {
                            self.slots.insert(d.path, (d.stamp, Slot::Failed));
                        }
                    }
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }

    /// Mint the VGE image id a decoded thumbnail will be uploaded under.
    pub fn next_image_id(&mut self) -> String {
        self.next_id += 1;
        format!("vfm.t{}", self.next_id)
    }

    /// Record that `path` is uploaded and drawable.
    pub fn ready(&mut self, path: PathBuf, stamp: Stamp, image_id: String, w: u32, h: u32) {
        self.lru.retain(|p| p != &path);
        self.lru.push_back(path.clone());
        self.slots
            .insert(path, (stamp, Slot::Ready { image_id, w, h }));
    }

    /// Image ids to `DropImage` because the cache is over budget. The
    /// thumbnails are uploaded with `Retention::Manual`, so the host
    /// keeps them across navigation/scroll and never GCs them — which is
    /// what makes paging them on and off screen work, but also means
    /// their lifetime is ours to manage: this LRU frees the coldest once
    /// past `max_live`. Tiles currently on screen (`keep`) are never
    /// evicted, so scrolling back and forth over a small directory never
    /// thrashes; a thumbnail dropped here is simply re-decoded and
    /// re-uploaded if the user revisits it.
    pub fn evict(&mut self, keep: &[PathBuf]) -> Vec<String> {
        let mut drops = Vec::new();
        let live = self
            .slots
            .values()
            .filter(|(_, s)| matches!(s, Slot::Ready { .. }))
            .count();
        let mut over = live.saturating_sub(self.max_live);
        while over > 0 {
            let Some(path) = self.lru.pop_front() else {
                break;
            };
            if keep.contains(&path) {
                self.lru.push_back(path); // still on screen — skip it
                // Everything left may be on screen too; bail rather than
                // spin.
                if self.lru.len() <= keep.len() {
                    break;
                }
                continue;
            }
            if let Some((_, Slot::Ready { image_id, .. })) = self.slots.remove(&path) {
                drops.push(image_id);
                over -= 1;
            }
        }
        drops
    }
}

/// Decode `path` (or one frame of it, for video) and downscale it so its
/// long edge is `target_px`. Returns `None` on any failure — a bad file
/// just keeps its icon.
fn decode(path: &Path, video: bool, target_px: u32) -> Option<(u32, u32, Vec<u8>)> {
    let img = if video {
        let png = ffmpeg_poster(path)?;
        image::load_from_memory(&png).ok()?
    } else {
        image::ImageReader::open(path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()?
    };
    let (w, h) = (img.width().max(1), img.height().max(1));
    let scale = (target_px as f32 / w.max(h) as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    );
    let rgba = img.thumbnail(tw, th).to_rgba8();
    let (rw, rh) = rgba.dimensions();
    Some((rw, rh, rgba.into_raw()))
}

/// One PNG-encoded frame, seeked a couple of seconds in so the poster
/// isn't the black frame most videos open on. Falls back to the very
/// first frame for clips shorter than the seek.
fn ffmpeg_poster(path: &Path) -> Option<Vec<u8>> {
    for seek in ["00:00:02", "00:00:00"] {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-ss", seek, "-i"])
            .arg(path)
            .args([
                "-frames:v",
                "1",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "-",
            ])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if out.status.success() && !out.stdout.is_empty() {
            return Some(out.stdout);
        }
    }
    None
}

/// Whether `bin` is on `$PATH`.
fn which(bin: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

/// The size/mtime pair a thumbnail is keyed on.
pub fn stamp_of(size: u64, mtime: std::time::SystemTime) -> Stamp {
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (size, secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn decoding_downscales_to_the_target_and_keeps_aspect() {
        let dir = std::env::temp_dir().join(format!("vfm-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("wide.png");
        std::fs::write(&p, png_bytes(400, 200)).unwrap();

        let (w, h, rgba) = decode(&p, false, 100).expect("decodes");
        assert_eq!((w, h), (100, 50));
        assert_eq!(rgba.len(), (w * h * 4) as usize);

        // Smaller than the target: left alone rather than upscaled.
        let small = dir.join("small.png");
        std::fs::write(&small, png_bytes(20, 10)).unwrap();
        assert_eq!(decode(&small, false, 100).map(|t| (t.0, t.1)), Some((20, 10)));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_file_fails_rather_than_panicking() {
        let dir = std::env::temp_dir().join(format!("vfm-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("not-really.png");
        std::fs::write(&p, b"nonsense").unwrap();
        assert!(decode(&p, false, 64).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_stamp_invalidates_the_slot() {
        let mut t = Thumbs::new(64, 1, 8);
        let p = PathBuf::from("/tmp/x.png");
        t.ready(p.clone(), (10, 100), "vfm.t1".into(), 8, 8);
        assert!(matches!(t.slot(&p, (10, 100)), Some(Slot::Ready { .. })));
        assert!(t.slot(&p, (11, 100)).is_none(), "size changed");
        assert!(t.slot(&p, (10, 101)).is_none(), "mtime changed");
    }

    #[test]
    fn eviction_drops_the_oldest_but_spares_what_is_on_screen() {
        let mut t = Thumbs::new(64, 1, 2);
        for i in 0..4 {
            t.ready(
                PathBuf::from(format!("/tmp/{i}.png")),
                (0, 0),
                format!("vfm.t{i}"),
                8,
                8,
            );
        }
        let keep = vec![PathBuf::from("/tmp/0.png")];
        let drops = t.evict(&keep);
        assert_eq!(drops.len(), 2);
        assert!(!drops.contains(&"vfm.t0".to_string()), "on screen, spared");
        assert!(drops.contains(&"vfm.t1".to_string()), "oldest evictable");
    }

    #[test]
    fn image_ids_are_unique() {
        let mut t = Thumbs::new(64, 1, 8);
        let a = t.next_image_id();
        let b = t.next_image_id();
        assert_ne!(a, b);
    }
}
