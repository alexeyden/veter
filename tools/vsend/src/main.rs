//! vsend — upload a local file to a VFT-aware terminal.
//!
//! Usage:
//!   $ vsend ./report.pdf :~/Documents/report.pdf
//!   $ vsend ./screenshot.png            # deferred form: host saves
//!                                       #   under $TMPDIR
//!
//! Pipeline:
//!   1. Stat the local file for its size.
//!   2. Switch to raw mode and probe VFT (mandatory) + VGE
//!      (optional, drives the progress bar).
//!   3. Spawn a stdin reader thread that decodes both VGE and VFT
//!      host envelopes onto a typed channel.
//!   4. Send BeginUpload, wait for the Ok carrying the resolved
//!      destination path.
//!   5. Stream UploadChunk envelopes while updating the progress UI.
//!      Drain any pending Oks between chunks so the channel doesn't
//!      grow unbounded across the transfer.
//!   6. Send EndUpload, wait for its (deferred) Ok carrying the
//!      final path.
//!   7. Tear down the progress UI and print the resolved path on
//!      stdout.

use std::fs::File;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use vft_client::probe::{read_cursor_row, run_vft_probe, run_vge_probe};
use vft_client::progress::{AsciiProgress, DelayedProgress, ProgressUI, VgeProgress};
use vft_client::stream::{HostFrame, ResponseStream};
use vft_client::tty::{drain_stale_stdin, winsize_cols, RawTty};

use vft_protocol::codec::Reader;
use vft_protocol::command::{BeginUploadBody, Command, EndUploadBody, UploadChunkBody};
use vft_protocol::encode::build_envelope;
use vft_protocol::frame::*;

#[derive(Parser, Debug)]
#[command(version, about = "Upload a local file to a VFT-aware terminal.")]
struct Cli {
    /// Local file to upload.
    local: PathBuf,

    /// Host destination, with a leading `:` (e.g. `:~/Documents/foo.pdf`).
    /// Omit to let the host pick a destination under `$TMPDIR`.
    #[arg(value_parser = parse_host_target)]
    host_target: Option<String>,

    /// Disable the progress display entirely.
    #[arg(long)]
    no_progress: bool,

    /// Defer the progress display by this many milliseconds. Quick
    /// transfers (localhost VM, fast LAN, small files) finish before
    /// the threshold and never spawn a bar; only longer-running ones
    /// reveal it. `0` shows the bar immediately.
    #[arg(long, default_value_t = 2000)]
    progress_delay_ms: u64,

    /// Bytes per UploadChunk frame. Default 256 KiB.
    #[arg(long, default_value_t = 256 * 1024)]
    chunk_size: usize,

    /// Probe timeout, milliseconds.
    #[arg(long, default_value_t = 500)]
    timeout_ms: u64,

    /// Permit overwriting an existing destination on the host.
    #[arg(long)]
    overwrite: bool,
}

fn parse_host_target(s: &str) -> Result<String, String> {
    s.strip_prefix(':')
        .map(String::from)
        .ok_or_else(|| format!("host path must start with ':' (got {s:?})"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vsend must run with stdin and stdout connected to a terminal");
    }

    let mut local = File::open(&cli.local)
        .with_context(|| format!("opening {}", cli.local.display()))?;
    let total_bytes = local
        .metadata()
        .with_context(|| format!("stat {}", cli.local.display()))?
        .len();
    let basename = cli
        .local
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("upload")
        .to_string();

    let host_path = cli.host_target.unwrap_or_default();

    let _guard = RawTty::enable()?;
    drain_stale_stdin();

    let timeout = Duration::from_millis(cli.timeout_ms);
    let vft_probe = run_vft_probe(timeout)?
        .ok_or_else(|| anyhow!("VFT probe timed out — terminal does not support VFT"))?;
    if vft_probe.features & FEAT_UPLOAD == 0 {
        bail!("host does not advertise upload support");
    }
    if cli.chunk_size as u32 > vft_probe.max_chunk_bytes {
        bail!(
            "chunk_size={} exceeds host's max_chunk_bytes={}",
            cli.chunk_size,
            vft_probe.max_chunk_bytes
        );
    }

    let vge_probe = run_vge_probe(timeout)?;
    let cursor_row = read_cursor_row(timeout)?.unwrap_or(1);
    let term_cols = winsize_cols().unwrap_or(80) as u32;

    let stream = ResponseStream::spawn();

    let transfer_id = format!("vsend-{}", std::process::id());
    let mut next_req: u32 = 1;

    // BeginUpload
    let begin = Command::BeginUpload(BeginUploadBody {
        transfer_id: transfer_id.clone(),
        host_path: host_path.clone(),
        basename: basename.clone(),
        total_bytes,
        flags: if cli.overwrite { FLAG_OVERWRITE } else { 0 },
        mode: 0,
        mtime: 0,
    });
    let begin_rid = next_req;
    next_req += 1;
    write_envelope(&build_envelope(&[(begin, begin_rid)]))?;
    let resolved = wait_for_vft_ok(&stream, begin_rid, |body| {
        let mut r = Reader::new(body);
        Ok(r.string()
            .map_err(|_| anyhow!("BeginUpload Ok: missing resolved_path"))?
            .to_owned())
    })?;

    // Progress UI
    let delay = Duration::from_millis(cli.progress_delay_ms);
    let mut ui: Box<dyn ProgressUI> = if cli.no_progress {
        Box::new(NoopProgress)
    } else if vge_probe.is_some() {
        Box::new(DelayedProgress::new(
            VgeProgress::new(
                format!("vsend-progress-{}", std::process::id()),
                format!("vsend: {basename}"),
                cursor_row,
                term_cols,
            ),
            delay,
        ))
    } else {
        Box::new(DelayedProgress::new(
            AsciiProgress::new(format!("vsend: {basename}"), term_cols),
            delay,
        ))
    };
    ui.start()?;
    let _ = ui.update(0, total_bytes, 0.0);

    // Streaming loop
    let started = Instant::now();
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; cli.chunk_size];
    loop {
        let n = local.read(&mut buf).context("reading local file")?;
        if n == 0 {
            break;
        }
        let chunk = buf[..n].to_vec();
        let cmd = Command::UploadChunk(UploadChunkBody {
            transfer_id: transfer_id.clone(),
            offset,
            data: chunk,
        });
        let rid = next_req;
        next_req += 1;
        write_envelope(&build_envelope(&[(cmd, rid)]))?;
        offset += n as u64;
        drain_responses(&stream)?;
        let rate = bytes_per_sec(offset, started);
        let _ = ui.update(offset, total_bytes, rate);
    }

    // EndUpload (deferred Ok)
    let end = Command::EndUpload(EndUploadBody {
        transfer_id: transfer_id.clone(),
    });
    let end_rid = next_req;
    write_envelope(&build_envelope(&[(end, end_rid)]))?;
    let final_path = wait_for_vft_ok(&stream, end_rid, |body| {
        let mut r = Reader::new(body);
        let path = r
            .string()
            .map_err(|_| anyhow!("EndUpload Ok: missing final_path"))?
            .to_owned();
        let _bytes = r
            .u64()
            .map_err(|_| anyhow!("EndUpload Ok: missing bytes_written"))?;
        Ok(path)
    })?;

    let _ = ui.update(total_bytes, total_bytes, bytes_per_sec(offset, started));
    ui.finish(&format!("uploaded -> {final_path}"))?;
    // VGE responses for the trailing UpdateCommand / DeleteElement
    // envelopes the progress UI emitted are still in flight; if
    // they land on the shell's stdin after we exit, zsh's zle
    // interprets `ESC _` as `insert-last-word` and pastes our argv
    // back onto the next prompt. Round-trip a VGE Probe to flush
    // them deterministically (VGE spec §1.2: one response per
    // command, in order).
    if !cli.no_progress && vge_probe.is_some() {
        stream.vge_barrier(next_req, Duration::from_secs(2));
    }
    let _ = resolved; // silence warning when --no-progress / non-vge
    Ok(())
}

// -------- helpers ------------------------------------------------------

struct NoopProgress;
impl ProgressUI for NoopProgress {
    fn start(&mut self) -> Result<()> {
        Ok(())
    }
    fn update(&mut self, _: u64, _: u64, _: f64) -> Result<()> {
        Ok(())
    }
    fn finish(&mut self, line: &str) -> Result<()> {
        let mut out = std::io::stdout().lock();
        write!(out, "{line}\r\n")?;
        out.flush()?;
        Ok(())
    }
}

fn write_envelope(env: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(env)?;
    out.flush()?;
    Ok(())
}

fn bytes_per_sec(offset: u64, started: Instant) -> f64 {
    let secs = started.elapsed().as_secs_f64();
    if secs <= 0.0 {
        0.0
    } else {
        offset as f64 / secs
    }
}

/// Block until a VFT response with the given `request_id` arrives.
/// On RSP_OK, decode the body via `decode`; on RSP_ERR or
/// TransferAborted, bail.
fn wait_for_vft_ok<R>(
    stream: &ResponseStream,
    request_id: u32,
    decode: impl FnOnce(&[u8]) -> Result<R>,
) -> Result<R> {
    loop {
        let frame = stream
            .recv_timeout(Duration::from_secs(60))
            .ok_or_else(|| anyhow!("timed out waiting for response to req={request_id}"))?;
        match frame {
            HostFrame::Vft {
                frame_type,
                request_id: rid,
                body,
            } => {
                if rid == request_id && frame_type == RSP_OK {
                    return decode(&body);
                }
                if rid == request_id && frame_type == RSP_ERR {
                    return Err(decode_err(&body));
                }
                if frame_type == EVT_TRANSFER_ABORTED {
                    return Err(decode_aborted(&body));
                }
                // Otherwise: an Ok or Err for an earlier request, or
                // an event we don't care about; ignore.
            }
            HostFrame::Vge { .. } => {
                // VGE responses are acks for our progress-bar
                // updates; we don't need to inspect them.
            }
        }
    }
}

/// Drain any pending host frames without blocking. Stops on the first
/// fatal frame (Err for an outstanding request, or TransferAborted)
/// and bails.
fn drain_responses(stream: &ResponseStream) -> Result<()> {
    while let Some(frame) = stream.try_recv() {
        if let HostFrame::Vft {
            frame_type, body, ..
        } = frame
        {
            match frame_type {
                RSP_OK => {}
                RSP_ERR => return Err(decode_err(&body)),
                EVT_TRANSFER_ABORTED => return Err(decode_aborted(&body)),
                _ => {}
            }
        }
    }
    Ok(())
}

fn decode_err(body: &[u8]) -> anyhow::Error {
    let mut r = Reader::new(body);
    let code = r.u16().unwrap_or(0);
    let msg = r.string().unwrap_or("").to_owned();
    anyhow!("host returned VFT error 0x{code:04X}: {msg}")
}

fn decode_aborted(body: &[u8]) -> anyhow::Error {
    let mut r = Reader::new(body);
    let id = r.string().unwrap_or("").to_owned();
    let reason = r.u8().unwrap_or(0);
    let msg = r.string().unwrap_or("").to_owned();
    anyhow!("transfer {id} aborted (reason={reason}): {msg}")
}
