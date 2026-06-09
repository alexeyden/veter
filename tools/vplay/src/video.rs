//! Video backend: `ffprobe` for metadata and `ffmpeg` for raw RGBA
//! frame decoding. No Rust media crate — both are spawned as external
//! processes. Audio is ignored.

use std::io::Read;
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, ChildStdout, Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct VideoMeta {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub nb_frames: Option<u64>,
    pub duration_s: Option<f64>,
}

impl VideoMeta {
    /// Best available total duration in seconds.
    pub fn duration(&self) -> f64 {
        self.duration_s
            .or_else(|| self.nb_frames.map(|n| n as f64 / self.fps.max(1.0)))
            .unwrap_or(0.0)
            .max(0.0)
    }

    /// Best estimate of the total frame count, kept consistent with the
    /// seek domain. The largest frame index reachable by seeking is
    /// `round(duration * fps)`, so the count can never be smaller than
    /// that — otherwise the status line would show a current frame past
    /// the total. `nb_frames` from the container is frequently a stale or
    /// low estimate (and is often missing), so it only wins when it isn't
    /// smaller than the duration-derived bound. Returns `None` only when
    /// nothing usable is known.
    pub fn total_frames(&self) -> Option<u64> {
        let by_duration = (self.duration() * self.fps).round() as u64;
        match (self.nb_frames, by_duration) {
            (Some(n), b) => Some(n.max(b)),
            (None, 0) => None,
            (None, b) => Some(b),
        }
    }
}

/// Run `ffprobe` and extract the first video stream's geometry/timing.
pub fn probe_video(path: &str) -> Result<VideoMeta> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate,avg_frame_rate,nb_frames,duration",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
            path,
        ])
        .output()
        .context("spawning ffprobe (is ffmpeg installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed for {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("parsing ffprobe json")?;
    let stream = v["streams"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("no video stream found in {path}"))?;
    let width = stream["width"].as_u64().unwrap_or(0) as u32;
    let height = stream["height"].as_u64().unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        bail!("video stream has zero extent");
    }
    // Prefer `avg_frame_rate` (total frames / duration — the real average
    // rate). `r_frame_rate` is the *base* rate: the lowest framerate that
    // can represent every timestamp exactly (an LCM of frame durations),
    // and for streams with any timing jitter it is often a 2-4x multiple of
    // the real rate. Using it makes a `1/fps` step land inside the same
    // frame several times in a row and inflates the frame index past
    // `nb_frames`. Fall back to `r_frame_rate` only when `avg` is missing.
    let fps = parse_fps(stream["avg_frame_rate"].as_str())
        .filter(|f| *f > 0.0)
        .or_else(|| parse_fps(stream["r_frame_rate"].as_str()))
        .filter(|f| *f > 0.0)
        .unwrap_or(25.0);
    let mut nb_frames = stream["nb_frames"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok());
    let duration_s = stream["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            v["format"]["duration"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
        });
    if nb_frames.is_none() {
        nb_frames = duration_s.map(|d| (d * fps).round() as u64);
    }
    Ok(VideoMeta {
        width,
        height,
        fps,
        nb_frames,
        duration_s,
    })
}

/// Probe the presentation timestamp (in seconds, display order) of every
/// video frame. Reads *packet* timestamps only — no pixel decoding — so it
/// stays cheap even on long files. Packets are demuxed in decode (DTS)
/// order, so the presentation times must be sorted to recover display
/// order. Returns an empty vec when nothing usable is produced (e.g. a
/// pipe/stream with no index, or packets without timestamps); callers then
/// fall back to the `index / fps` grid.
pub fn probe_frame_times(path: &str) -> Vec<f64> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
            path,
        ])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let mut times: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<f64>().ok())
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times
}

fn parse_fps(s: Option<&str>) -> Option<f64> {
    let s = s?;
    if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.parse().ok()?;
        let d: f64 = d.parse().ok()?;
        if d != 0.0 { Some(n / d) } else { None }
    } else {
        s.parse().ok()
    }
}

/// Decode exactly one frame at `time` seconds (accurate seek). vplay is
/// scrub-only: every displayed frame comes from one of these calls.
pub fn grab_one_frame(path: &str, w: u32, h: u32, time: f64) -> Result<Option<Vec<u8>>> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner").args(["-loglevel", "error"]);
    if time > 0.0 {
        cmd.args(["-ss", &format!("{time}")]);
    }
    cmd.args(["-i", path]).args([
        "-frames:v",
        "1",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-an",
        "-sn",
        "-",
    ]);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().context("spawning ffmpeg")?;
    let mut so = child.stdout.take().expect("piped stdout");
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let r = so.read_exact(&mut buf);
    let _ = child.wait();
    match r {
        Ok(()) => Ok(Some(buf)),
        Err(_) => Ok(None),
    }
}

/// An ffmpeg single-frame decode running in the background. The event loop
/// drains it incrementally with [`Decode::poll`] so it can animate a
/// spinner (and stay responsive to input / newer seeks) while ffmpeg seeks
/// and decodes. Dropping the handle kills ffmpeg — that is how a superseded
/// seek (e.g. mid-drag) cancels the decode it replaced.
pub struct Decode {
    child: Child,
    stdout: ChildStdout,
    buf: Vec<u8>,
    filled: usize,
}

/// Result of draining a [`Decode`] without blocking.
pub enum DecodeState {
    /// ffmpeg is still working / the frame is only partly transferred.
    Pending,
    /// The full RGBA frame is ready.
    Done(Vec<u8>),
    /// ffmpeg exited before producing a frame (e.g. seek past the end) or
    /// the pipe errored.
    Failed,
}

impl Decode {
    /// The stdout pipe fd, for inclusion in the event loop's poll set so a
    /// finished decode wakes the loop promptly.
    pub fn fd(&self) -> RawFd {
        self.stdout.as_raw_fd()
    }

    /// Drain whatever ffmpeg has written so far without blocking. The pipe
    /// is non-blocking, so a read returns `EAGAIN` when momentarily empty.
    pub fn poll(&mut self) -> DecodeState {
        loop {
            if self.filled == self.buf.len() {
                let _ = self.child.wait();
                return DecodeState::Done(std::mem::take(&mut self.buf));
            }
            match nix::unistd::read(self.stdout.as_raw_fd(), &mut self.buf[self.filled..]) {
                Ok(0) => {
                    let _ = self.child.wait();
                    return DecodeState::Failed;
                }
                Ok(n) => self.filled += n,
                Err(nix::errno::Errno::EAGAIN) => return DecodeState::Pending,
                Err(_) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return DecodeState::Failed;
                }
            }
        }
    }
}

impl Drop for Decode {
    fn drop(&mut self) {
        // Superseded or abandoned: stop ffmpeg rather than leak it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Like [`grab_one_frame`], but non-blocking: spawn the ffmpeg decode of the
/// frame at `time` and return a [`Decode`] the event loop can poll.
pub fn start_decode(path: &str, w: u32, h: u32, time: f64) -> Result<Decode> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner").args(["-loglevel", "error"]);
    if time > 0.0 {
        cmd.args(["-ss", &format!("{time}")]);
    }
    cmd.args(["-i", path]).args([
        "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-an", "-sn", "-",
    ]);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().context("spawning ffmpeg")?;
    let stdout = child.stdout.take().expect("piped stdout");
    set_nonblocking(stdout.as_raw_fd());
    Ok(Decode {
        child,
        stdout,
        buf: vec![0u8; (w as usize) * (h as usize) * 4],
        filled: 0,
    })
}

/// Flip a fd to non-blocking. Best-effort: on the off chance fcntl fails the
/// decode still works, it just blocks the loop briefly instead of spinning.
fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(fps: f64, nb_frames: Option<u64>, duration_s: Option<f64>) -> VideoMeta {
        VideoMeta {
            width: 1920,
            height: 1080,
            fps,
            nb_frames,
            duration_s,
        }
    }

    #[test]
    fn total_frames_never_below_seek_domain() {
        // nb_frames is a stale/low container tag; duration is accurate.
        // The end-of-seek index is round(36.0 * 25.0) = 900, so the total
        // must not be the smaller reported 898.
        let m = meta(25.0, Some(898), Some(36.0));
        assert_eq!(m.total_frames(), Some(900));
    }

    #[test]
    fn total_frames_prefers_larger_nb_frames() {
        // duration slightly short of the real frame count.
        let m = meta(25.0, Some(900), Some(35.9));
        assert_eq!(m.total_frames(), Some(900));
    }

    #[test]
    fn total_frames_fallbacks() {
        // Only nb_frames known.
        assert_eq!(meta(25.0, Some(100), None).total_frames(), Some(100));
        // Only duration known.
        assert_eq!(meta(25.0, None, Some(4.0)).total_frames(), Some(100));
        // Nothing usable.
        assert_eq!(meta(25.0, None, None).total_frames(), None);
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffprobe")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            && Command::new("ffmpeg")
                .arg("-version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    }

    #[test]
    fn frame_times_are_sorted_and_complete() {
        if !have_ffmpeg() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        }
        // Synthesize a 25 fps, 2 s clip (50 frames). Packets demux in DTS
        // order, so this exercises the sort in `probe_frame_times`.
        let path = std::env::temp_dir().join(format!("vplay_pts_{}.mp4", std::process::id()));
        let p = path.to_str().unwrap();
        let ok = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(["-f", "lavfi", "-i", "testsrc=duration=2:size=160x120:rate=25"])
            .args(["-pix_fmt", "yuv420p", p])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "ffmpeg failed to synthesize test clip");

        let times = probe_frame_times(p);
        let _ = std::fs::remove_file(&path);

        assert_eq!(times.len(), 50, "expected 50 frames");
        assert!(
            times.windows(2).all(|w| w[0] <= w[1]),
            "frame times must be sorted ascending: {times:?}"
        );
        assert!(times[0].abs() < 1e-6, "first frame should be at t=0");
    }

    #[test]
    fn fps_fraction() {
        assert!((parse_fps(Some("30000/1001")).unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parse_fps(Some("25/1")), Some(25.0));
        assert_eq!(parse_fps(Some("0/0")), None);
    }
}
