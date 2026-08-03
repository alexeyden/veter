//! Decoding and encoding, off the event loop.
//!
//! Both are blocking and unmeasurable: the image crate hands back a
//! decoded picture or nothing, and the WebP encoder the same. Run on the
//! loop they froze vplay for their whole duration — no input, no
//! repaint, and a progress panel stuck on whatever it last said. One
//! worker thread runs them instead, so the loop keeps drawing (the
//! panel's bar sweeps) and still answers a key or a newer request.
//!
//! Jobs are superseded, not cancelled: neither step can be interrupted,
//! so each job carries a sequence number, the worker skips any job that
//! is already stale when it picks it up, and [`Worker::poll`] drops the
//! results a later request has overtaken.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

use vge_render::upload::encode_payload;

use crate::image_src::{Frame, load_image};
use crate::pick_encoding;

/// What the worker is doing, for the progress panel to name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Idle,
    Decoding,
    Encoding,
}

const PHASE_IDLE: u8 = 0;
const PHASE_DECODING: u8 = 1;
const PHASE_ENCODING: u8 = 2;

/// The host's advertised limits, which decide the wire encoding.
#[derive(Clone, Copy)]
pub struct EncodeParams {
    pub supported: u8,
    pub ssh: bool,
    pub max_image_bytes: u32,
}

/// A picture the worker has decoded (if it started from a file) and
/// encoded, ready for the caller to stream.
pub struct Ready {
    pub seq: u64,
    /// The file it came from — `Some` for a still, which also names the
    /// status bar; `None` for a video frame the worker only encoded.
    pub path: Option<PathBuf>,
    pub frame: Frame,
    pub encoding: u8,
    pub payload: Vec<u8>,
}

pub enum Done {
    Ready(Ready),
    /// The file would not decode, or its payload busts the host's
    /// limits. What to do about it is the caller's call — the playlist
    /// walks on to the next entry, a lone file is fatal.
    Failed {
        seq: u64,
        path: Option<PathBuf>,
        error: String,
    },
}

enum Job {
    Load {
        seq: u64,
        path: PathBuf,
        params: EncodeParams,
    },
    Encode {
        seq: u64,
        frame: Frame,
        params: EncodeParams,
    },
}

/// Handle to the worker thread. Dropping it closes the job channel,
/// which ends the thread once its current job finishes.
pub struct Worker {
    tx: Sender<Job>,
    rx: Receiver<Done>,
    phase: Arc<AtomicU8>,
    /// Newest sequence number submitted, shared so the thread can skip
    /// a job that went stale while it sat in the queue.
    latest: Arc<AtomicU64>,
    next_seq: u64,
    /// Sequence number the loop is waiting on; `None` when none is owed.
    inflight: Option<u64>,
}

impl Worker {
    pub fn spawn() -> Self {
        let (job_tx, job_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();
        let phase = Arc::new(AtomicU8::new(PHASE_IDLE));
        let latest = Arc::new(AtomicU64::new(0));
        let thread_phase = phase.clone();
        let thread_latest = latest.clone();

        std::thread::spawn(move || {
            for job in job_rx {
                let seq = match &job {
                    Job::Load { seq, .. } | Job::Encode { seq, .. } => *seq,
                };
                // Overtaken while queued: the caller would discard the
                // result anyway, so don't spend the CPU on it.
                if seq < thread_latest.load(Ordering::Acquire) {
                    continue;
                }
                let done = run_job(job, &thread_phase);
                thread_phase.store(PHASE_IDLE, Ordering::Release);
                if done_tx.send(done).is_err() {
                    break;
                }
            }
        });

        Self {
            tx: job_tx,
            rx: done_rx,
            phase,
            latest,
            next_seq: 1,
            inflight: None,
        }
    }

    /// Decode `path`, then encode it. Supersedes anything in flight.
    pub fn load(&mut self, path: PathBuf, params: EncodeParams) {
        let seq = self.submit();
        let _ = self.tx.send(Job::Load { seq, path, params });
    }

    /// Encode an already-decoded frame — the ffmpeg path, where the
    /// pixels arrive from elsewhere. Supersedes anything in flight.
    pub fn encode(&mut self, frame: Frame, params: EncodeParams) {
        let seq = self.submit();
        let _ = self.tx.send(Job::Encode { seq, frame, params });
    }

    fn submit(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.latest.store(seq, Ordering::Release);
        self.inflight = Some(seq);
        seq
    }

    /// Take the current job's outcome, discarding any superseded one.
    /// `None` while the worker is still on it, or when nothing is owed.
    pub fn poll(&mut self) -> Option<Done> {
        let want = self.inflight?;
        loop {
            match self.rx.try_recv() {
                Ok(done) => {
                    let seq = match &done {
                        Done::Ready(r) => r.seq,
                        Done::Failed { seq, .. } => *seq,
                    };
                    if seq == want {
                        self.inflight = None;
                        return Some(done);
                    }
                    // A superseded job that had already started; its
                    // replacement is still coming.
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// True while a submitted job has yet to be collected.
    pub fn is_busy(&self) -> bool {
        self.inflight.is_some()
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Acquire) {
            PHASE_DECODING => Phase::Decoding,
            PHASE_ENCODING => Phase::Encoding,
            _ => Phase::Idle,
        }
    }
}

fn run_job(job: Job, phase: &AtomicU8) -> Done {
    match job {
        Job::Load { seq, path, params } => {
            phase.store(PHASE_DECODING, Ordering::Release);
            let frame = match load_image(&path) {
                Ok(f) => f,
                Err(e) => {
                    return Done::Failed {
                        seq,
                        path: Some(path),
                        error: e.to_string(),
                    };
                }
            };
            phase.store(PHASE_ENCODING, Ordering::Release);
            match encode(&frame, params) {
                Ok((encoding, payload)) => Done::Ready(Ready {
                    seq,
                    path: Some(path),
                    frame,
                    encoding,
                    payload,
                }),
                Err(error) => Done::Failed {
                    seq,
                    path: Some(path),
                    error,
                },
            }
        }
        Job::Encode { seq, frame, params } => {
            phase.store(PHASE_ENCODING, Ordering::Release);
            match encode(&frame, params) {
                Ok((encoding, payload)) => Done::Ready(Ready {
                    seq,
                    path: None,
                    frame,
                    encoding,
                    payload,
                }),
                Err(error) => Done::Failed {
                    seq,
                    path: None,
                    error,
                },
            }
        }
    }
}

/// Raw locally, WebP when a full-resolution picture would exceed the
/// host's advertised limit. The frame is kept (the cursor readout reads
/// its pixels), so the payload is a copy of it rather than a move.
fn encode(frame: &Frame, params: EncodeParams) -> Result<(u8, Vec<u8>), String> {
    let enc = pick_encoding(
        frame.w,
        frame.h,
        params.supported,
        params.ssh,
        params.max_image_bytes,
    )
    .map_err(|e| e.to_string())?;
    let (encoding, payload) =
        encode_payload(frame.rgba.clone(), frame.w, frame.h, enc).map_err(|e| e.to_string())?;
    if params.max_image_bytes > 0 && payload.len() > params.max_image_bytes as usize {
        return Err(format!(
            "encoded {}x{} picture ({} bytes) exceeds the host limit of {} bytes",
            frame.w,
            frame.h,
            payload.len(),
            params.max_image_bytes
        ));
    }
    Ok((encoding, payload))
}
