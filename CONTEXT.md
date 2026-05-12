# Session-manager (veterd) implementation — handoff

This file captures the state of the session-manager work so another
session can pick it up cold. Architecture lives in
`doc/session-manager.md`; this is the implementation log.

## Where things stand

Working tree: clean. Branch: `master`. The build is green and the
full workspace test suite passes (`cargo test`). End-to-end smoke-tested: `new` / `list` /
`kill` / `kill-server` over the Unix socket, plus `attach <name>`
with `SCM_RIGHTS` stdio handover that:

- probes the renderer for cell metrics + reads `TIOCGWINSZ` for
  the actual grid, resizes the per-session engines and inner PTY
  accordingly;
- replays a vt100 + VGE + PRT snapshot;
- live-forwards subsequent inner-program output to the renderer;
- detaches cleanly on the `Ctrl+\` + `d` hotkey, leaving the
  session running for a later re-attach;
- handles mid-attach window resizes via a per-attach
  `WinsizeWatcher` thread that polls `TIOCGWINSZ` every 250 ms and
  re-applies the size to engines + inner PTY on change.

## What's done

Commits on `master`, oldest first, after the WIP-banner cleanup:

| Commit | Subject |
|---|---|
| `0faa4fb` | chore: prt+vge+vft: mark protocols as unstable WIP v0 |
| `4015d2f` | doc: session-manager: persistent veterd architecture sketch |
| `ee89c77` | feat: vt100: full_contents_formatted snapshot serializer |
| `1f8d874` | feat: vge: VgeEngine::serialize_state snapshot serializer |
| `858472e` | feat: prt: PrtEngine::serialize_state snapshot serializer |
| `ee9d02a` | refactor: vge+prt: decouple host state from femtovg::ImageId |
| `6781832` | refactor: veter: expose host engines as a library face |
| `8cb3c1a` | feat: veterd: persistent session manager skeleton |
| `f831104` | doc: prt+vge+vft: spell out pass-through contract for foreign markers |
| `a212101` | chore: make: add veterd to install target |
| `9855a75` | feat: veterd: per-session host engines + PTY worker thread |
| `0564ee8` | feat: veterd: attach with snapshot replay over SCM_RIGHTS |
| `cf34bbf` | doc: CONTEXT.md: log Commit A and B |
| `a832110` | feat: veterd: detach hotkey Ctrl+\\ then d |
| `6c7dad7` | doc: CONTEXT.md: mark Commit C as landed |
| `840adcf` | feat: veterd: upstream VGE/PRT probe + winsize at attach time |
| `fe9e657` | doc: CONTEXT.md: mark Commit D as landed |
| `dc35a2a` | feat: veterd: mid-attach SIGWINCH via TIOCGWINSZ poll |

### Snapshot serializers (the core)

The three building blocks of attach-time replay all exist with
round-trip tests:

- `vt100::Screen::full_contents_formatted()` (in
  `vt100/src/screen.rs`) — scrollback + visible grid + cursor + input
  modes. Internally uses a new `Row::write_contents_inline`
  (`vt100/src/row.rs`) and `Grid::scrollback_rows` (`vt100/src/grid.rs`).
- `veter::vge::VgeEngine::serialize_state()` (in
  `veter/src/vge/state.rs`) — image table + style table + currently-
  active element set, parents before children, top-level origin
  rebased against `top_of_live_screen`.
- `veter::prt::PrtEngine::serialize_state()` (in
  `veter/src/prt/state.rs`) — portal tree, each portal followed by a
  `WritePortal` whose payload concatenates the portal's vt100 redraw,
  its nested PRT subtree, and its VGE state.

v1 limitations called out in the docstrings:

- vt100 serializer drops alt-screen-buffer state, DECSC saved cursor,
  scrolling region, and the active G0/G1 charset.
- VGE serializer ships images as Raw RGBA8 regardless of original
  encoding (engine doesn't retain WebP source bytes).
- PRT serializer skips per-portal VFT engines (in-flight transfers
  are abandoned by reattach) and engine-level focus/cursor-style/
  polled-cache state.
- Only the currently-active screen's elements/portals are emitted;
  the suspended alt/main set is dropped.

### GpuImageId decoupling

Host engine state used to hold `femtovg::ImageId` directly. Replaced
with an opaque `pub struct GpuImageId(pub u64)` defined in
`veter/src/vge/state.rs` and re-exported as `veter::vge::GpuImageId`.
The renderer (`veter/src/renderer.rs`) maintains the
`HashMap<GpuImageId, femtovg::ImageId>` and gained three helpers:
`register_gpu_image`, `lookup_gpu_image`, `release_gpu_image`. The
host engines are now type-level free of femtovg (doc comments still
mention it for readers).

### veter as lib+bin

`veter/Cargo.toml` grew a `[lib]` section. `veter/src/lib.rs` declares
`pub mod` for every previously-internal module (`clipboard`, `prt`,
`pty`, `renderer`, `vft`, `vge`). `veter/src/main.rs` now starts with
`use veter::{...}` instead of declaring those modules itself. This
unblocks veterd at the cost of the lib pulling in the GUI deps —
veterd inherits femtovg/winit/glutin/parley/fontconfig as link-time
freight. A proper veter-host crate split is the path to a clean
remote-side binary; see "Known limitations" below.

### Pass-through contract

`doc/portal-extension.md`, `doc/vector-graphics-extension.md`, and
`doc/file-transfer-extension.md` all now contain a MUST-language
paragraph in §1.1 about forwarding APC envelopes whose marker doesn't
belong to the extension this host consumes. The PRT/VGE/VFT APC
parsers already implement this behaviour; the contract just makes it
official.

### veterd skeleton

New crate at `tools/veterd`. Layout:

```
tools/veterd/
├── Cargo.toml      — deps on veter, vt100, prt/vge protocols, clap, nix
└── src/
    ├── main.rs     — clap CLI; --foreground runs the daemon, otherwise
    │                 connects to the socket and routes a single Request
    ├── ipc.rs      — tiny length-prefixed binary protocol (Request +
    │                 Response with round-trip tests for every variant)
    ├── daemon.rs   — UnixListener accept loop + dispatch + session table
    └── session.rs  — Session struct; forkpty + execvp the child shell
```

Socket path: `$XDG_RUNTIME_DIR/veterd/sock` (mode 0700 dir), falling
back to `/tmp/veterd-<uid>/veterd/sock` if `$XDG_RUNTIME_DIR` is
unset. CLI surface: `veterd --foreground`, `veterd new <name>
[cmd...]`, `veterd list`, `veterd kill <name>`, `veterd kill-server`.

### Commit A — per-session engines + worker thread (committed)

`tools/veterd/src/engines.rs` defines `EngineState` (vt100 parser,
VgeEngine, PrtEngine, plus an optional `renderer_stdout: OwnedFd`)
and `spawn_worker(&master)`. Each `Session::spawn` dups the inner
PTY master and spawns a `veterd-worker` thread that reads in a
blocking loop, runs the same PRT → VGE → vt100 pipeline as
`veter/src/main.rs::App::process_pty_output`, calls
`handle_terminal_events`, `after_vt100_process`,
`flush_pending_events`, `drive_and_flush_vft`, and writes engine
responses back to the master.

VFT is intentionally not instantiated host-side: the daemon forwards
VFT envelopes verbatim per `doc/file-transfer-extension.md` §1.1.
Per-portal VFT engines still tick via PRT.

### Commit B — attach + snapshot replay (committed)

Files added/changed:

- `tools/veterd/src/ipc.rs` — new `Request::Attach { name }` variant
  with a round-trip test. Daemon's `handle_connection` no longer
  wraps the stream in `BufReader` (which would have stripped the
  `SCM_RIGHTS` cmsg from any bytes it read ahead).
- `tools/veterd/src/fdpass.rs` — `send_stdio` / `recv_stdio`
  helpers around nix's `sendmsg`/`recvmsg` with
  `ControlMessage::ScmRights`. Sends one filler byte (`b'F'`) so
  the kernel accepts the cmsg. Includes a `socketpair` + pipe
  round-trip test.
- `tools/veterd/src/attach.rs` — `start(stream, sessions, name)`
  runs on the accept loop: receives the fds, validates the
  session, flips `Session.attached`, spawns a `veterd-attach`
  thread, returns. The handler thread:
    1. Locks `EngineState`, serializes vt100 + VGE + PRT, writes
       the concatenation to the renderer's stdout, installs the
       stdout fd on `EngineState.renderer_stdout` while still
       holding the lock so the worker's first live byte cannot
       race ahead of the snapshot.
    2. Splices renderer stdin → inner PTY master verbatim (input
       never crosses the engines per PRT spec).
    3. On EOF / error: clears `renderer_stdout`, drops its copy of
       the stdout fd, and the deferred closure flips
       `Session.attached` back to false.
- `tools/veterd/src/session.rs` — `attached: bool` → `attached:
  Arc<AtomicBool>` so the handler thread can mutate it from outside
  the accept loop. New `is_attached()` accessor.
- `tools/veterd/src/engines.rs` — `EngineState.renderer_stdout:
  Option<OwnedFd>`. When set, the worker forwards every PTY-master
  chunk to it verbatim (raw bytes — the renderer parses
  PRT/VGE/VFT natively). Writes happen outside the engines lock;
  on write error the worker clears the fd so a closed pipe doesn't
  block subsequent chunks.
- `tools/veterd/Cargo.toml` — `nix` gains the `uio` feature for
  `sendmsg`/`recvmsg`.

What's intentionally not in Commit B (some now done in later commits):

- ~~No upstream probe.~~ Done in Commit D (see below).
- **No detach hotkey.** Disconnect is detected only via fd EOF on
  the renderer's stdin. See Commit C below.
- ~~No resize handling.~~ Folded into Commit D — at attach time we
  read `TIOCGWINSZ` on stdin, resize the parser, and TIOCSWINSZ
  the inner PTY. Mid-attach SIGWINCH (the user resizing their
  window after attach) is still not wired up.

### Commit C — detach hotkey (committed)

Hardcoded `Ctrl+\` (`0x1C`) + `d` (`0x64`). Lives in
`attach::DetachScanner` (state machine) and is plumbed into the
existing `splice_input` loop. State machine:

- *Pass* — bytes flow verbatim. The prefix byte itself is not
  forwarded yet; it transitions to *Pending*.
- *Pending* — the next byte decides:
    - `d` → detach (return cleanly, both prefix bytes consumed).
    - prefix again → flush the buffered prefix, stay *Pending*.
    - anything else → flush the buffered prefix, then the new byte,
      return to *Pass*.
- On stdin EOF with a buffered prefix → flush it.

Eight unit tests in `attach::tests` cover the table:
plain-text passthrough, lone prefix-then-letter, detach-in-one-chunk,
detach-split-across-chunks, prefix-prefix-letter, prefix-prefix-d,
EOF-flushes-prefix, prefix-followed-by-detach.

End-to-end smoke test verified: attach with stdin ending in
`\x1cd` detaches cleanly, session stays alive, re-attach succeeds
and snapshot replay still contains the session's prior output.

Configurability via env var is deferred — the architecture doc
called this out as fine for v1.

### Commit D — upstream VGE/PRT probe + winsize handover (committed)

Files added/changed:

- `tools/veterd/src/probe.rs` (new) — `probe::run(stdin, stdout, timeout)`
  writes VGE + PRT `Probe` envelopes to stdout, reads stdin with
  `nix::poll`, parses `ProbeResponse` payloads via the two protocol
  crates' `ApcStream`s (one for VGE T2C `vge`, one for PRT T2C
  `prt`), and returns the parsed `VgeProbeData` / `PrtProbeData`
  along with any non-probe bytes (typeahead) that arrived during
  the probe phase. Also reads `TIOCGWINSZ` on stdin for the
  renderer's actual `(rows, cols)`. Tests cover both parsers plus
  the encoder marker bytes.
- `tools/veterd/src/attach.rs` — `handler_main` now opens with a
  500 ms probe round. The outcome is applied to the per-session
  engines (`Screen::set_size`, `VgeEngine::set_dimensions`,
  `PrtEngine::set_metrics`) and forwarded to the inner PTY via
  `TIOCSWINSZ`. Typeahead is pushed to the inner PTY before the
  snapshot is serialized so the user's keystrokes during attach
  aren't dropped.
- `veter/src/prt/state.rs` — new `PrtEngine::set_metrics(cell_px,
  scale_factor)` setter mirroring `VgeEngine::set_dimensions`.
  Future per-portal sub-engines inherit the new metrics; existing
  portals keep their construction-time values (mid-flight reflow
  unsupported).

Non-VGE / non-PRT renderers still attach cleanly: the probe times
out, defaults are kept, the snapshot is still emitted (the renderer
just ignores the embedded VGE/PRT envelopes).

End-to-end smoke tests verified:

- Non-VGE renderer (plain pipe): probe times out at 500 ms, attach
  proceeds with defaults, vt100 snapshot reaches stdout.
- Typeahead-during-probe: keystrokes piped to attach's stdin are
  forwarded to the inner PTY after the probe phase ends; an
  echoing bash session sees them and replies as usual.

### Commit E — mid-attach SIGWINCH via TIOCGWINSZ poll (committed)

The daemon doesn't share a controlling tty with the renderer's PTY
slave, so the kernel never delivers `SIGWINCH` to us. Instead,
`attach::handler_main` spawns a per-attach `WinsizeWatcher` thread
right after the probe. The watcher:

- Dups the renderer-stdin and inner-PTY-master fds so its lifetime
  is independent of the splice loop's fds (clean drop on detach).
- Sleeps for `WINSIZE_POLL_INTERVAL` (250 ms) in a loop, then
  calls `probe::read_winsize` on stdin.
- On change: locks `EngineState`, calls
  `Screen::set_size(rows, cols)`, then `TIOCSWINSZ`s the inner PTY
  master so the child program `SIGWINCH`es and redraws.
- Self-terminates via a shared `AtomicBool`; the `Drop` impl flips
  the flag and joins the thread when the attach handler returns.

The polling is purely local kernel state — `TIOCGWINSZ` reads the
slave's winsize that the SSH server already wrote when the renderer
sent a `window-change` packet. No additional SSH traffic. Cost is
~4 ioctls/sec per active attach, in microseconds.

End-to-end smoke tested with a Python PTY harness: attach to a
session whose bash has a `WINCH` trap, resize the harness's PTY
master to 50×200, and confirm `SIZE 50x200` is forwarded back via
the renderer-stdout within one polling interval.

## What's left

### #7 — Detach hotkey

Done (hardcoded `Ctrl+\` + `d` — see Commit C above). Making it
configurable is the obvious follow-up; the architecture doc names a
daemon-side env var (e.g. `VETERD_DETACH_PREFIX`).

### #8 — aarch64-musl packaging for veterd

Currently deferred. veterd depends on `veter` the lib which
transitively pulls parley → fontconfig and glutin/winit. None of
those musl-cross without a sysroot. The cross-build fails on
`yeslogic-fontconfig-sys`'s build script ("pkg-config has not been
configured to support cross-compilation"). The proper fix is the
"full veter-host extraction" — splitting the host engines into a
GUI-free `veter-host` crate. See "Known limitations" below.

For local installs `veterd` is already in `make install`'s
`PACKAGES` list and lands at `$(BINDIR)/veterd`.

### #1 (lingering work) — full veter-host extraction

The lib+bin split in 6781832 was the minimal unblock. It does NOT
get us a GUI-free face for the host engines: the lib drags every
GUI dep along. The proper end-state is:

- A new `veter-host` workspace crate that contains
  `vge/state.rs`, `prt/{state,portal}.rs`, and the parts of
  `vft/{state,worker}.rs` that don't need rfd / opener.
- `vge/render.rs`, `prt/render.rs`, `renderer.rs` stay in the GUI
  binary. The render path imports state types from `veter_host::*`.
- `veterd` depends on `veter-host` only — no femtovg / winit /
  parley / fontconfig in the dep tree.

Gotchas the partial work surfaced:

- `vge/render.rs` references `crate::renderer::TerminalRenderer`.
  TerminalRenderer is GUI-only and shouldn't live in veter-host.
  Either move TerminalRenderer's GpuImageId map onto a separate
  small struct that lives in veter-host (and is `&mut`-passed
  into render functions), or accept a render-only sibling module in
  the binary.
- `Portal` (in `prt/portal.rs`) owns a `VftEngine`. If VftEngine
  doesn't move to veter-host, Portal can't either. Options:
  - Move VftEngine too. Worker.rs's `rfd`/`opener` deps need to
    be feature-flagged; the picker / open-after handlers become
    no-ops when the "gui" feature is off, and the GUI binary
    activates the feature.
  - Make `Portal::vft` an `Option`/generic so headless hosts
    don't carry one. Bigger API churn.
- Tests inside `veter/src/{vge,prt}/state.rs` will need their
  scaffolding (`build_envelope`, `unwrap_t2c_envelope`, etc.) to
  move with them.

### Companion `doc/session-extension.md` (deferred per architecture doc)

The vmux ↔ veterd integration protocol (detach command, session-
name display in pane title) was intentionally deferred. The
proposed shape: a new APC marker pair (`VSE` / `vse`) ridden over
the pane's input/output streams between vmux and veterd, parallel
to PRT/VGE/VFT framing. vmux extracts VSE envelopes from the pane
output stream before forwarding the rest as PRT `WritePortal` data,
so local veter never sees VSE. Commands worth landing first: Probe,
Detach, Status (request-side); Ok, Err, ProbeResponse,
SessionAttached, SessionDetached, SessionRenamed, Status events
(response/event-side). v1 of the daemon works without this; it's
the second shoe to drop.

## Known limitations & gotchas

- **veter's lib face drags GUI deps.** The fix is the veter-host
  extraction (see above). Symptom you'll hit: `cargo build -p veterd
  --target aarch64-unknown-linux-musl` fails on fontconfig sys
  crate.
- **veter's lib also added a doctest pass.** When the lib was
  introduced, cargo started running doctests on lib doc comments.
  One pre-existing comment block in `prt::state::cmd_set_portal_scrollback`
  had to be re-fenced from ` ``` ` to ` ```text ` (handled in
  6781832 / the subsequent fix commit). If you add prose code-blocks
  to private fns in the lib face, use `text` or `ignore` to keep
  cargo test green.
- **Doctest for new lib doc comments:** as above.
- **VGE serializer ships decoded pixels** for images. A 50 KiB WebP
  inflates to width×height×4 bytes on every reattach. Future:
  retain original encoded bytes alongside decoded pixels.
- **PRT serializer doesn't carry VFT state.** Reattach drops
  in-flight `vsend`/`vrecv` transfers. The pass-through contract
  means VFT envelopes ride through veterd verbatim to local veter,
  but mid-transfer state in the daemon's per-portal VFT engines is
  lost.
- **Sessions are in-memory only.** Daemon dying = sessions gone.
  This matches the spec; persistence is out of scope.
- **Single-attach.** Multiple renderers attaching to the same
  session is not implemented; the IPC doesn't even have an attach
  variant yet.
- **No auth.** Socket is mode 0700 in `$XDG_RUNTIME_DIR`. Per-user
  isolation only; cross-user attach is unsupported.

## Useful files at a glance

| File | Why you'd read it |
|---|---|
| `doc/session-manager.md` | Architecture, staging plan, open questions |
| `doc/portal-extension.md` §1.1 | Pass-through contract for PRT |
| `doc/vector-graphics-extension.md` §1.1 | Pass-through contract for VGE |
| `doc/file-transfer-extension.md` §1.1 | Pass-through contract for VFT |
| `vt100/src/screen.rs::full_contents_formatted` | The vt100 redraw serializer |
| `veter/src/vge/state.rs::VgeEngine::serialize_state` | VGE serializer |
| `veter/src/prt/state.rs::PrtEngine::serialize_state` | PRT serializer |
| `veter/src/lib.rs` | The host engine library face |
| `veter/src/renderer.rs::TerminalRenderer::register_gpu_image` | GpuImageId↔femtovg::ImageId bridge |
| `tools/veterd/src/daemon.rs` | Accept loop + dispatch |
| `tools/veterd/src/session.rs` | Session struct + forkpty |
| `tools/veterd/src/ipc.rs` | Wire protocol between CLI and daemon |
| `tools/veterd/src/main.rs` | CLI entry point |

## Quick smoke test

```
make install                                       # installs veterd into ~/.local/bin
veterd --foreground &                              # start daemon
veterd new alpha bash                              # spawn a session
veterd list                                        # see it
veterd kill alpha                                  # tear it down
veterd kill-server                                 # stop the daemon
```

`veterd` reads `$XDG_RUNTIME_DIR` from the environment — pass the
same value to the daemon and the CLI invocations.
