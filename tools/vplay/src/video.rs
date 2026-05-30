//! Video backend: `ffprobe` for metadata and `ffmpeg` for raw RGBA
//! frame decoding. No Rust media crate — both are spawned as external
//! processes. Audio is ignored.

use std::io::Read;
use std::process::{Command, Stdio};

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
    let fps = parse_fps(stream["r_frame_rate"].as_str())
        .or_else(|| parse_fps(stream["avg_frame_rate"].as_str()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_fraction() {
        assert!((parse_fps(Some("30000/1001")).unwrap() - 29.97).abs() < 0.01);
        assert_eq!(parse_fps(Some("25/1")), Some(25.0));
        assert_eq!(parse_fps(Some("0/0")), None);
    }
}
