//! Per-session host-engine state and the PTY-reader worker thread.
//!
//! Each [`Session`](crate::session::Session) owns an `Arc<Mutex<EngineState>>`
//! and spawns one of these workers when the session is created. The
//! worker reads from a dup of the inner PTY master, runs the bytes
//! through PRT → VGE → vt100 in the same order as
//! `veter/src/main.rs::App::process_pty_output`, and writes any
//! engine-generated responses back to the PTY master.
//!
//! Sessions don't attach yet — the externally visible effect of this
//! module is that PTY output is parsed and accumulated in engine state,
//! ready for the attach path (task #6) to serialize and replay.
//!
//! ## Grid sizing
//!
//! v1 starts every session at the conventional `24×80` grid with a
//! generous default scrollback. The attach path will resize the parser
//! once it learns the renderer's actual grid via the VGE/PRT probes.
//! VGE/PRT engine metrics (cell pixel dims, scale factor) default to
//! 8×16 px / 1.0×; the attach path resets them from the probe response.
//!
//! ## Thread layout
//!
//! - The daemon's accept loop (main thread) constructs the session,
//!   inserts it into the session table, and holds an `OwnedFd` to the
//!   master for the future inbound-input splice path.
//! - The worker thread receives its own `OwnedFd` (a `dup(2)` of the
//!   master) and blocks on reads. It writes engine responses through
//!   that same fd so the inner program sees DSR/VGE/PRT replies.
//! - Both threads share `Arc<Mutex<EngineState>>`. The worker releases
//!   the lock around every `read(2)` so the attach path (later) can
//!   serialize the state without waiting on PTY traffic.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use veter::prt::PrtEngine;
use veter::vge::VgeEngine;

/// Default grid size used until the renderer attaches and reports its
/// actual cell count. Mirrors `veter/src/main.rs`'s startup defaults.
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Default scrollback depth. Matches the host binary's
/// `vt100::Parser::new(..., 10_000)` allocation; the attach path
/// inherits it.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Placeholder cell pixel dimensions used until the renderer's VGE
/// probe response arrives. Pixel-space layout decisions stored in VGE
/// state are anchor-based, so this default does not affect correctness
/// — it only sizes pre-attach `cell_px` reports to inner programs.
pub const DEFAULT_CELL_PX: (u16, u16) = (8, 16);
pub const DEFAULT_SCALE: f32 = 1.0;

/// Host-side state machinery shared between the daemon and the
/// per-session worker thread. See module docs for the threading model.
pub struct EngineState {
    pub parser: vt100::Parser,
    pub vge: VgeEngine,
    pub prt: PrtEngine,
    /// Write side of the renderer's stdout while a renderer is
    /// attached. The worker forwards every PTY-master chunk it reads
    /// into this fd verbatim (without the engine transforms) so the
    /// renderer paints exactly what the inner program produces. The
    /// attach handler installs this on attach and clears it on detach
    /// or write error.
    pub renderer_stdout: Option<OwnedFd>,
}

impl EngineState {
    pub fn new() -> Self {
        Self {
            parser: vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLS, DEFAULT_SCROLLBACK),
            vge: VgeEngine::new(DEFAULT_CELL_PX, DEFAULT_SCALE),
            // No-op VFT wakeup: the daemon has no event loop to nudge.
            // Per-portal VFT workers still tick, but the host loop polls
            // them every chunk anyway via `drive_and_flush_vft`.
            prt: PrtEngine::with_metrics_and_wakeup(
                DEFAULT_CELL_PX,
                DEFAULT_SCALE,
                Arc::new(|| {}),
            ),
            renderer_stdout: None,
        }
    }
}

impl Default for EngineState {
    fn default() -> Self {
        Self::new()
    }
}

/// Dup the master fd twice (one read handle, one write handle) and
/// spawn the per-session worker. Returns the shared engine handle so
/// the attach path can lock it to serialize a snapshot or to forward
/// live output.
pub fn spawn_worker(master: &OwnedFd) -> Result<Arc<Mutex<EngineState>>> {
    let reader_fd = dup_owned(master).context("dup(master) for worker reader")?;
    let writer_fd = dup_owned(master).context("dup(master) for worker writer")?;
    let engines = Arc::new(Mutex::new(EngineState::new()));
    let engines_for_worker = Arc::clone(&engines);
    std::thread::Builder::new()
        .name("veterd-worker".into())
        .spawn(move || worker_main(reader_fd, writer_fd, engines_for_worker))
        .context("spawn worker thread")?;
    Ok(engines)
}

fn dup_owned(fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    let raw = nix::unistd::dup(fd.as_raw_fd()).map_err(std::io::Error::other)?;
    // SAFETY: dup(2) returned a fresh fd we now solely own.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Worker thread entry point. Mirrors the host pipeline in
/// `veter/src/main.rs::App::process_pty_output`: PRT extracts its
/// envelopes (and observes RIS/DECSTR/2J/3J), VGE extracts its
/// envelopes from the PRT passthrough, and the host vt100 parses
/// whatever remains. After every chunk we run both engines'
/// `after_vt100_process` hooks and write back any pending responses.
///
/// VFT is intentionally **not** instantiated host-side in veterd: per
/// the architecture sketch in `doc/session-manager.md`, VFT envelopes
/// ride through the daemon verbatim (the pass-through contract in
/// `doc/file-transfer-extension.md` §1.1 makes this normative). The
/// per-portal VFT engines inside the PRT tree still tick via
/// `drive_and_flush_vft`.
fn worker_main(reader_fd: OwnedFd, writer_fd: OwnedFd, engines: Arc<Mutex<EngineState>>) {
    let mut reader = std::fs::File::from(reader_fd);
    let mut writer = std::fs::File::from(writer_fd);
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                // EIO on Linux means the slave was closed — treat as EOF.
                if e.raw_os_error() == Some(libc::EIO) {
                    break;
                }
                eprintln!("veterd: worker read error: {e}");
                break;
            }
        };

        let (to_write, forward_to_renderer) = {
            let mut guard = match engines.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let EngineState {
                parser,
                vge,
                prt,
                renderer_stdout,
            } = &mut *guard;

            let prt_chunk = prt.process_pty_chunk_full(&buf[..n]);
            let vge_passthrough = vge.process_pty_chunk(&prt_chunk.passthrough);
            if !vge_passthrough.is_empty() {
                parser.process(&vge_passthrough);
            }
            prt.handle_terminal_events(&prt_chunk.terminal_events);
            prt.after_vt100_process(parser);
            prt.flush_pending_events();
            vge.after_vt100_process(parser);
            prt.drive_and_flush_vft();

            let mut out = prt.take_responses();
            out.extend_from_slice(&vge.take_responses());

            // Forward verbatim to the renderer while attached. The
            // renderer parses PRT/VGE/VFT envelopes natively, so we
            // ship the raw chunk we just received from the inner PTY
            // (not the engine-transformed view).
            let forward = renderer_stdout.is_some();
            (out, forward)
        };

        if !to_write.is_empty() {
            // Best effort: a failed write back to the inner program
            // means it has gone away; the next read will EOF and we'll
            // exit the loop.
            if let Err(e) = writer.write_all(&to_write) {
                eprintln!("veterd: worker write error: {e}");
                break;
            }
        }

        if forward_to_renderer {
            // Write outside the engines lock so a slow renderer
            // doesn't stall the engines. We dup the fd briefly to
            // avoid holding the lock during the write; on write error
            // we clear `renderer_stdout` so the next chunk doesn't
            // retry into a closed pipe.
            let raw = {
                let guard = match engines.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard
                    .renderer_stdout
                    .as_ref()
                    .map(|fd| fd.as_raw_fd())
            };
            if let Some(raw) = raw {
                let mut wrote_ok = true;
                let mut off = 0;
                while off < n {
                    match nix::unistd::write(
                        // SAFETY: `raw` is borrowed from the OwnedFd
                        // held by engines.renderer_stdout; we don't
                        // close it. write(2) takes a BorrowedFd.
                        unsafe {
                            std::os::fd::BorrowedFd::borrow_raw(raw)
                        },
                        &buf[off..n],
                    ) {
                        Ok(0) => {
                            wrote_ok = false;
                            break;
                        }
                        Ok(k) => off += k,
                        Err(nix::errno::Errno::EINTR) => continue,
                        Err(_) => {
                            wrote_ok = false;
                            break;
                        }
                    }
                }
                if !wrote_ok {
                    let mut guard = match engines.lock() {
                        Ok(g) => g,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    guard.renderer_stdout = None;
                }
            }
        }
    }
}
