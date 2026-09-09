//! Picture decoding, off the event loop, and the VGE image table
//! behind it.
//!
//! Two separate questions, answered at two different times. **How big
//! is it** has to be answered while the document is being laid out, for
//! every picture in the file, before anything is on screen — so it is
//! `image_dimensions`, which reads the header and stops. **What does it
//! look like** is only asked of the pictures that come into view, and
//! is a full decode, so it happens on a worker thread and the page
//! draws a plate until the answer arrives.
//!
//! Decodes are capped at [`MAX_DECODE_PX`] on the long edge rather than
//! sized to the drawn rect. The host stretches a `DrawImage` into its
//! target with linear filtering (VGE §7.5), so one decode serves every
//! zoom step — which is what keeps `+`/`-` from re-decoding the whole
//! document.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Longest edge a decoded picture keeps. Generous enough to stay sharp
/// when a HiDPI cell grid draws it across a wide pane, small enough
/// that a dozen of them are not a memory problem.
const MAX_DECODE_PX: u32 = 1600;

/// A file *version*: a picture is stale once its size or mtime moves.
type Stamp = (u64, u64);

/// Where one picture has got to.
#[derive(Debug, Clone)]
pub enum Slot {
    Pending,
    Ready {
        image_id: String,
    },
    /// Decoding failed, and is not retried — a corrupt file would
    /// otherwise be re-read every frame it is on screen.
    Failed,
}

/// A decoded picture on its way to the terminal.
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
}

struct Done {
    path: PathBuf,
    stamp: Stamp,
    pixels: Option<(u32, u32, Vec<u8>)>,
}

pub struct Images {
    /// Header-only dimensions, memoised. `None` is a remembered
    /// failure, which is why the value is an `Option` rather than the
    /// entry simply being absent.
    sizes: RefCell<HashMap<PathBuf, Option<(u32, u32)>>>,
    jobs: Sender<Job>,
    done: Receiver<Done>,
    slots: HashMap<PathBuf, (Stamp, Slot)>,
    /// Uploaded pictures in touch order, oldest first — the eviction
    /// queue that keeps us inside the host's image budget.
    lru: VecDeque<PathBuf>,
    max_live: usize,
    in_flight: usize,
    max_in_flight: usize,
    next_id: u64,
}

impl Images {
    /// Start `workers` decoder threads, keeping at most `max_live`
    /// pictures uploaded at once.
    pub fn new(workers: usize, max_live: usize) -> Self {
        let (jobs_tx, jobs_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        let jobs_rx = Arc::new(Mutex::new(jobs_rx));
        for _ in 0..workers.max(1) {
            let jobs = Arc::clone(&jobs_rx);
            let done = done_tx.clone();
            std::thread::spawn(move || {
                loop {
                    let job = match jobs.lock() {
                        Ok(rx) => rx.recv(),
                        Err(_) => return,
                    };
                    let Ok(job) = job else { return };
                    let pixels = decode(&job.path);
                    if done
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
        Images {
            sizes: RefCell::new(HashMap::new()),
            jobs: jobs_tx,
            done: done_rx,
            slots: HashMap::new(),
            lru: VecDeque::new(),
            max_live,
            in_flight: 0,
            max_in_flight: workers.max(1) * 2,
            next_id: 0,
        }
    }

    /// Ask for `path` to be decoded, if it isn't already in hand.
    pub fn request(&mut self, path: &Path) {
        let Some(stamp) = stamp_of(path) else { return };
        match self.slots.get(path) {
            Some((have, _)) if *have == stamp => return,
            _ => {}
        }
        if self.in_flight >= self.max_in_flight {
            return;
        }
        self.slots
            .insert(path.to_path_buf(), (stamp, Slot::Pending));
        self.in_flight += 1;
        if self
            .jobs
            .send(Job {
                path: path.to_path_buf(),
                stamp,
            })
            .is_err()
        {
            self.in_flight -= 1;
            self.slots.insert(path.to_path_buf(), (stamp, Slot::Failed));
        }
    }

    /// Collect whatever the workers finished, as pictures to upload.
    /// A failed decode is recorded here and never reported.
    pub fn drain(&mut self) -> Vec<Decoded> {
        let mut out = Vec::new();
        while let Ok(done) = self.done.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            match done.pixels {
                Some((w, h, rgba)) => out.push(Decoded {
                    path: done.path,
                    stamp: done.stamp,
                    w,
                    h,
                    rgba,
                }),
                None => {
                    self.slots.insert(done.path, (done.stamp, Slot::Failed));
                }
            }
        }
        out
    }

    /// The next free VGE image id. Namespaced under the crate prefix so
    /// one `DropImage` sweep reclaims every one of them (§8.2).
    pub fn next_image_id(&mut self) -> String {
        self.next_id += 1;
        format!("{}i{}", crate::ID_PREFIX, self.next_id)
    }

    /// Record an uploaded picture against its file.
    pub fn ready(&mut self, path: PathBuf, stamp: Stamp, image_id: String) {
        self.lru.retain(|p| p != &path);
        self.lru.push_back(path.clone());
        self.slots.insert(path, (stamp, Slot::Ready { image_id }));
    }

    /// Ids to drop: the coldest pictures past `max_live`, never one in
    /// `keep` (which is what the page is about to draw).
    pub fn evict(&mut self, keep: &[PathBuf]) -> Vec<String> {
        let mut out = Vec::new();
        while self.lru.len() > self.max_live {
            let Some(cold) = self
                .lru
                .iter()
                .position(|p| !keep.contains(p))
                .and_then(|i| self.lru.remove(i))
            else {
                break;
            };
            if let Some((_, Slot::Ready { image_id })) = self.slots.remove(&cold) {
                out.push(image_id);
            }
        }
        out
    }

    pub fn slot(&self, path: &Path) -> Option<&Slot> {
        self.slots.get(path).map(|(_, s)| s)
    }
}

impl crate::layout::ImageSizes for Images {
    fn size(&self, path: &Path) -> Option<(u32, u32)> {
        if let Some(known) = self.sizes.borrow().get(path) {
            return *known;
        }
        let measured = image::image_dimensions(path).ok();
        self.sizes.borrow_mut().insert(path.to_path_buf(), measured);
        measured
    }
}

impl crate::render::Pictures for Images {
    fn picture(&self, image: &crate::layout::ImageBox) -> crate::render::Picture {
        use crate::render::Picture;
        let Some(path) = image.path.as_deref() else {
            return Picture::Missing;
        };
        match self.slot(path) {
            Some(Slot::Ready { image_id }) => Picture::Ready(image_id.clone()),
            Some(Slot::Failed) => Picture::Missing,
            _ => Picture::Pending,
        }
    }
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((meta.len(), mtime))
}

/// Decode and downscale one file to RGBA8. Runs on a worker thread.
fn decode(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::open(path).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let long = w.max(h);
    let img = if long > MAX_DECODE_PX {
        let k = MAX_DECODE_PX as f32 / long as f32;
        img.resize(
            ((w as f32 * k) as u32).max(1),
            ((h as f32 * k) as u32).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}
