//! vrecv — pull a host-side file back to the local filesystem from a
//! VFT-aware terminal.
//!
//! Usage:
//!   $ vrecv :/var/log/syslog ./syslog.txt
//!   $ vrecv ./from-host.bin                # picker form: the host
//!                                          #   pops a native file
//!                                          #   dialog; user cancel
//!                                          #   surfaces as
//!                                          #   err_cancelled
//!
//! Pipeline:
//!   1. Resolve the host path (prefix `:`) and local destination from
//!      the CLI args.
//!   2. Switch to raw mode and probe VFT (mandatory) + VGE
//!      (optional, drives the progress bar).
//!   3. Spawn a stdin reader thread that decodes both VGE and VFT
//!      host envelopes onto a typed channel.
//!   4. Send BeginDownload, wait for the Ok carrying total_bytes /
//!      mode / mtime.
//!   5. Drain DownloadChunk events into the local file, advancing the
//!      progress UI on each chunk. On DownloadEnd, finalise; on
//!      TransferAborted, bail and remove the partial file.
//!   6. Tear down the progress UI and print the destination path on
//!      stdout.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use vft_client::cancel::{cancel_and_drain, CancelGuard};
use vft_client::probe::{read_cursor_row, run_vft_probe, run_vge_probe};
use vft_client::progress::{AsciiProgress, DelayedProgress, ProgressUI, VgeProgress};
use vft_client::stream::{HostFrame, ResponseStream};
use vft_client::tty::{drain_stale_stdin, winsize_cols, RawTty};

use vft_protocol::codec::Reader;
use vft_protocol::command::{BeginDownloadBody, Command, ReportDownloadAckBody};
use vft_protocol::encode::build_envelope;
use vft_protocol::frame::*;

#[derive(Parser, Debug)]
#[command(version, about = "Download a host-side file to the local filesystem.")]
struct Cli {
    /// First positional argument. Either `:host_path` (use the second
    /// argument as the local destination) or a local destination
    /// (then the host shows a file picker).
    arg1: String,

    /// Local destination when the first argument is `:host_path`.
    arg2: Option<String>,

    /// Disable the progress display entirely.
    #[arg(long)]
    no_progress: bool,

    /// Defer the progress display by this many milliseconds. Quick
    /// transfers (localhost VM, fast LAN, small files) finish before
    /// the threshold and never spawn a bar; only longer-running ones
    /// reveal it. `0` shows the bar immediately.
    #[arg(long, default_value_t = 2000)]
    progress_delay_ms: u64,

    /// Preferred DownloadChunk size hint sent to the host. `0` lets
    /// the host pick. Kept modest (64 KiB) so the progress bar advances
    /// smoothly on slow links — a chunk is the finest progress
    /// granularity, so large chunks make the bar jump and look stalled.
    #[arg(long, default_value_t = 64 * 1024)]
    chunk_size_hint: u32,

    /// Probe timeout, milliseconds.
    #[arg(long, default_value_t = 500)]
    timeout_ms: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let (host_path, local_path) = if let Some(rest) = cli.arg1.strip_prefix(':') {
        let local = cli
            .arg2
            .ok_or_else(|| anyhow!("missing local destination"))?;
        (rest.to_string(), PathBuf::from(local))
    } else {
        if cli.arg2.is_some() {
            bail!("unexpected second argument; use `:host_path local_path` for explicit form");
        }
        (String::new(), PathBuf::from(&cli.arg1))
    };

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("vrecv must run with stdin and stdout connected to a terminal");
    }

    // Open the local destination eagerly so a missing parent dir or
    // a permission failure surfaces before we touch the wire.
    let mut local = File::create(&local_path)
        .with_context(|| format!("creating {}", local_path.display()))?;

    let _guard = RawTty::enable()?;
    drain_stale_stdin();

    let timeout = Duration::from_millis(cli.timeout_ms);
    let vft_probe = run_vft_probe(timeout)?
        .ok_or_else(|| anyhow!("VFT probe timed out — terminal does not support VFT"))?;
    if vft_probe.features & FEAT_DOWNLOAD == 0 {
        bail!("host does not advertise download support");
    }
    let vge_probe = run_vge_probe(timeout)?;
    let cursor_row = read_cursor_row(timeout)?.unwrap_or(1);
    let term_cols = winsize_cols().unwrap_or(80) as u32;

    let stream = ResponseStream::spawn();

    let transfer_id = format!("vrecv-{}", std::process::id());
    let begin_rid: u32 = 1;
    let begin = Command::BeginDownload(BeginDownloadBody {
        transfer_id: transfer_id.clone(),
        host_path,
        chunk_size_hint: cli.chunk_size_hint,
    });
    write_envelope(&build_envelope(&[(begin, begin_rid)]))?;

    // The transfer is now active on the host, so arm the cancel-on-exit
    // net before we block on its response: a Ctrl+C or error during the
    // wait below then still emits a CancelTransfer via the guard's Drop.
    let mut cancel = CancelGuard::new(&transfer_id);

    // Wait for the BeginDownload Ok carrying metadata.
    let (resolved_path, total_bytes) = wait_begin_ok(&stream, begin_rid)?;

    let local_label = local_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download")
        .to_string();
    let delay = Duration::from_millis(cli.progress_delay_ms);
    let mut ui: Box<dyn ProgressUI> = if cli.no_progress {
        Box::new(NoopProgress)
    } else if let Some(vge) = vge_probe {
        Box::new(DelayedProgress::new(
            VgeProgress::new(
                format!("vrecv-progress-{}", std::process::id()),
                format!("vrecv: {local_label}"),
                cursor_row,
                term_cols,
                (vge.cell_pixel_width, vge.cell_pixel_height),
            ),
            delay,
        ))
    } else {
        Box::new(DelayedProgress::new(
            AsciiProgress::new(format!("vrecv: {local_label}"), term_cols),
            delay,
        ))
    };
    ui.start()?;
    let _ = ui.update(0, total_bytes, 0.0);

    let started = Instant::now();
    let received = match stream_download(
        &stream,
        &mut local,
        &local_path,
        &transfer_id,
        total_bytes,
        ui.as_mut(),
        started,
    ) {
        Ok(n) => n,
        Err(e) => {
            // Graceful error / Ctrl+C: tell the host to stop and swallow
            // the chunks already in flight so they don't land on the shell
            // once we exit. The download window is small (host caps it at
            // ~128 KiB), so this drains in a few seconds even on a slow
            // link; the 30 s cap is only a safety net for a host that
            // never confirms the abort. Note it, since draining can take a
            // moment and should not look like a hang.
            let _ = ui.finish(&format!("cancelling: {e}"));
            cancel_and_drain(&stream, &transfer_id, Duration::from_secs(30));
            cancel.disarm();
            let _ = std::fs::remove_file(&local_path);
            // Flush the progress bar's trailing VGE responses too, so they
            // don't leak onto the shell after we exit (same rationale as
            // the success path below).
            if !cli.no_progress && vge_probe.is_some() {
                stream.vge_barrier(begin_rid.wrapping_add(1), Duration::from_secs(2));
            }
            return Err(e);
        }
    };
    cancel.disarm();

    local.flush().context("flush local destination")?;
    drop(local);

    let _ = ui.update(received, total_bytes, bytes_per_sec(received, started));
    ui.finish(&format!(
        "downloaded {} -> {}",
        resolved_path,
        local_path.display()
    ))?;
    // VGE responses for the trailing UpdateCommand / DeleteElement
    // envelopes the progress UI emitted are still in flight; if
    // they land on the shell's stdin after we exit, zsh's zle
    // interprets `ESC _` as `insert-last-word` and pastes our argv
    // back onto the next prompt. Round-trip a VGE Probe to flush
    // them deterministically (VGE spec §1.2: one response per
    // command, in order).
    if !cli.no_progress && vge_probe.is_some() {
        stream.vge_barrier(begin_rid.wrapping_add(1), Duration::from_secs(2));
    }
    Ok(())
}

// -------- helpers ------------------------------------------------------

/// Drain `DownloadChunk` events into `local` until `DownloadEnd`,
/// returning the total bytes received. Advances the §7.4 flow-control
/// window via `ReportDownloadAck` as it drains, and refreshes the
/// progress UI on a wall-clock cadence (not merely once per chunk) so
/// the delayed bar reveals promptly even while a slow chunk is still in
/// flight. A host abort/error, a malformed frame, a local write failure,
/// closed input, or a 60 s data stall each surface as `Err` so the
/// caller can cancel the transfer and clean up.
fn stream_download(
    stream: &ResponseStream,
    local: &mut File,
    local_path: &Path,
    transfer_id: &str,
    total_bytes: u64,
    ui: &mut dyn ProgressUI,
    started: Instant,
) -> Result<u64> {
    // Ack every ACK_INTERVAL bytes. Small so the first ack lands well
    // within the host's initial burst (INITIAL_DOWNLOAD_BURST_BYTES),
    // which is what opens the transfer up to the full window; still
    // coarse enough that the back-channel chatter stays negligible.
    const ACK_INTERVAL: u64 = 64 * 1024;
    // Abort if no chunk arrives for this long (the stall guard that used
    // to be the blocking recv timeout, now measured from the last chunk).
    const DATA_STALL: Duration = Duration::from_secs(60);
    // UI refresh / poll cadence: short enough that the delayed bar
    // reveals promptly after its threshold even mid-chunk.
    const POLL: Duration = Duration::from_millis(250);

    let mut received: u64 = 0;
    let mut last_acked: u64 = 0;
    let mut ack_rid: u32 = 1;
    let mut last_data = Instant::now();
    loop {
        // Ctrl+C: the reader flags a bare ETX keystroke. Bail here so the
        // caller cancels the transfer and cleans up the partial file.
        if stream.interrupted() {
            return Err(anyhow!("download cancelled (Ctrl+C)"));
        }
        match stream.recv_timeout(POLL) {
            Some(HostFrame::Vft {
                frame_type, body, ..
            }) => match frame_type {
                EVT_DOWNLOAD_CHUNK => {
                    let mut r = Reader::new(&body);
                    let id = r
                        .string()
                        .map_err(|_| anyhow!("DownloadChunk: missing id"))?
                        .to_owned();
                    let _offset = r
                        .u64()
                        .map_err(|_| anyhow!("DownloadChunk: missing offset"))?;
                    let data = r
                        .bytes()
                        .map_err(|_| anyhow!("DownloadChunk: missing data"))?;
                    if id != transfer_id {
                        continue;
                    }
                    local
                        .write_all(data)
                        .with_context(|| format!("writing {}", local_path.display()))?;
                    received += data.len() as u64;
                    last_data = Instant::now();
                    let _ = ui.update(received, total_bytes, bytes_per_sec(received, started));
                    if received - last_acked >= ACK_INTERVAL {
                        ack_rid = ack_rid.wrapping_add(1);
                        let ack = Command::ReportDownloadAck(ReportDownloadAckBody {
                            transfer_id: transfer_id.to_string(),
                            bytes_confirmed: received,
                        });
                        write_envelope(&build_envelope(&[(ack, ack_rid)]))?;
                        last_acked = received;
                    }
                }
                EVT_DOWNLOAD_END => return Ok(received),
                EVT_TRANSFER_ABORTED => return Err(decode_aborted(&body)),
                RSP_ERR => return Err(decode_err(&body)),
                _ => {
                    // Stray Ok for some other request, or an event we
                    // don't surface — ignore.
                }
            },
            Some(HostFrame::Vge { .. }) => {
                // VGE acks for our progress-bar updates; not relevant.
            }
            None => {
                // Poll timeout (or reader EOF): no frame this tick.
                // Refresh the UI so a delayed bar can reveal and its
                // elapsed / rate keep ticking, then enforce the deadlines.
                let _ = ui.update(received, total_bytes, bytes_per_sec(received, started));
                if stream.at_eof() {
                    return Err(anyhow!("terminal input closed mid-transfer"));
                }
                if last_data.elapsed() >= DATA_STALL {
                    return Err(anyhow!("timed out waiting for download data"));
                }
            }
        }
    }
}

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

/// Block until the BeginDownload Ok arrives. Returns
/// `(resolved_path, total_bytes)`.
fn wait_begin_ok(stream: &ResponseStream, request_id: u32) -> Result<(String, u64)> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if stream.interrupted() {
            return Err(anyhow!("download cancelled (Ctrl+C)"));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("timed out waiting for BeginDownload response"));
        }
        // Poll in short slices so Ctrl+C is noticed within ~250 ms even
        // while the host is slow to answer.
        let Some(frame) = stream.recv_timeout(remaining.min(Duration::from_millis(250))) else {
            if stream.at_eof() {
                return Err(anyhow!("terminal input closed before BeginDownload response"));
            }
            continue;
        };
        match frame {
            HostFrame::Vft {
                frame_type,
                request_id: rid,
                body,
            } => {
                if rid == request_id && frame_type == RSP_OK {
                    let mut r = Reader::new(&body);
                    let resolved = r
                        .string()
                        .map_err(|_| anyhow!("BeginDownload Ok: missing resolved_path"))?
                        .to_owned();
                    let total = r
                        .u64()
                        .map_err(|_| anyhow!("BeginDownload Ok: missing total_bytes"))?;
                    let _mode = r
                        .u32()
                        .map_err(|_| anyhow!("BeginDownload Ok: missing mode"))?;
                    let _mtime = r
                        .i64()
                        .map_err(|_| anyhow!("BeginDownload Ok: missing mtime"))?;
                    return Ok((resolved, total));
                }
                if rid == request_id && frame_type == RSP_ERR {
                    return Err(decode_err(&body));
                }
                if frame_type == EVT_TRANSFER_ABORTED {
                    return Err(decode_aborted(&body));
                }
            }
            HostFrame::Vge { .. } => {}
        }
    }
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
