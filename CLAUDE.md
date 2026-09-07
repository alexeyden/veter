# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

The project is **Veter** (Russian: ветер, "wind") — a GUI terminal emulator built around a family of APC-framed protocols that ride a single PTY: PRT (portals / multiplexing), VGE (vector graphics in the grid), VFT (file transfer), plus a session layer (SES + VSS) that keeps sessions alive across renderer disconnects. The terminal binary itself is `veter`; the supporting tools and library crates keep their names (`vmux`, `vcat`, `vplay`, `vdraw`, `vfm`, `vsend`, `vrecv`, `vsd`, `vssh`, `vproto`, and the `*-protocol` wire crates).

## Build & run

Cargo workspace; edition 2024 (the vendored `vt100` fork stays on 2021).

- Build everything: `cargo build` (release: `cargo build --release`)
- Build one crate: `cargo build -p veter` (or `veter-host`, `vmux`, `vcat`, `vplay`, `vdraw`, `vfm`, `vsend`, `vrecv`, `vsd`, `vssh`, `vproto`, `vge-protocol`, `prt-protocol`, `vft-protocol`, `ses-protocol`, `vss-protocol`, `vge-ui`, `breakout`)
- Run the GUI terminal: `cargo run -p veter`
- Install `veter` plus the tool set (`vcat`, `vplay`, `vdraw`, `vfm`, `vmux`, `vproto`, `vsend`, `vrecv`, `vsd`, `vssh`), the `vplace` script and the Claude Code hook to `$PREFIX/bin` (default `~/.local`) plus a desktop entry: `make install` (override `PREFIX=...` to retarget; `make uninstall` removes them). `make install-remote-<arch>` cross-compiles a musl build and scp-installs it to `$REMOTE`.

## Tests

Most tests are inline `#[cfg(test)]` modules — there is no separate test harness layout to learn.

- Run the whole suite: `cargo test`
- One crate: `cargo test -p prt-protocol`
- One test by name substring: `cargo test -p prt-protocol envelope_roundtrip`

The only integration test directory is `tools/vproto/tests/` — `roundtrip.rs` (JSON → envelope → the host's own parser → JSON) and `placement.rs` (the `vplace` script against a real pty, replayed through the host engine).

## Repository layout

Crates are grouped by what they are, not by name. A crate's directory
name drops the redundant suffix its parent already supplies
(`protocol/vge` is the `vge-protocol` crate); **crate names are
unchanged**, so `cargo -p <name>` and every `use` path work exactly as
before.

```
protocol/   vge prt vft vss ses   — pure wire format, no state, no I/O
libs/       veter-host veter-version vge-render vge-ui   — shared implementation crates
tools/      the CLI/TUI clients, plus vproto and the vplace script
vendored/   vt100 femtovg   — third-party forks
veter/      the GUI terminal
doc/        the normative protocol specs
```

The protocols live under `doc/` and drive the entire codebase. Read the relevant one before making non-trivial changes:

- `doc/portal-extension.md` — PRT, an APC-framed protocol (`ESC _ PRT … ESC \`) for carving the host grid into per-portal sub-terminals (multiplexer panes, PiP log views, scrollback-anchored snapshots).
- `doc/vector-graphics-extension.md` — VGE, an APC-framed protocol (`ESC _ VGE … ESC \`) for vector/raster graphics inside the grid. Cleanup is prefix-based: `DeleteElement` (§6.2) and `DropImage` (§8.2) each lead with a flags byte whose `bit0` reinterprets the id as an id prefix, so a client namespaces its ids (`myapp.`, §6.8) and sweeps one command per table — on startup as well as exit, since the tables outlive any one client run. An empty prefix matches everything, which is what retired `ClearAll` (§6.7).
- `doc/file-transfer-extension.md` — VFT (`ESC _ VFT … ESC \`), a bidirectional file-transfer channel: a CLI inside the terminal hands the host a local file (`vsend`) or pulls a host-side file back (`vrecv`). **WIP, v0** — wire format may change; clients and host ship in lockstep.
- `doc/session-manager.md` — `vsd`, the persistent session daemon, and **VSS** (`ESC _ VSS … ESC \`, in `doc/session-manager.md` §4), the binary engine-snapshot protocol it uses to ship state to an attaching renderer.
- `doc/session-extension.md` — SES (`ESC _ SES … ESC \`), the small `vmux` ↔ `vsd` control channel (session-name probe, detach command).

Host-side engine state (the vt100 grids and all five engines) lives in **`libs/veter-host`**, GUI-free, so the same code backs both the `veter` GUI binary and the headless `vsd` daemon. The `veter` crate keeps only the GUI and the render/glue side of each engine (`src/prt/render.rs`, `src/vge/render.rs`, `src/vft/`, `src/ses.rs`, `src/vss.rs`).

| Crate | Role |
|---|---|
| `protocol/*` — `vge-protocol`, `prt-protocol`, `vft-protocol`, `ses-protocol`, `vss-protocol` | Pure wire format only: APC stream parser, primitive codec, command/response/event framing, encoders. No state, no rendering. Host and clients both depend on these. VGE/PRT/SES carry optional default-off `serde` and `schemars` features so `vproto` can read the same types as JSON and generate their schema; nothing else enables them. |
| `vendored/vt100` | Local fork of the vt100 parser (adds `clear_scrollback`, xterm-style push/pull vertical resize, `binary_snapshot`/`restore_from_binary_snapshot` for VSS, the `scroll_committed` counter the PRT activity heuristic watches, `top_of_live_screen` — the absolute scrollback line index VGE elements and Scrollback portals anchor to, maintained by the grid itself and carried in its snapshot — and the SGR-Pixels mouse encoding, DECSET 1016). The screen model the host and every portal use. The sequence coverage is measured against `infocmp xterm-256color` — terminfo is the set of sequences real programs emit — so anything in that entry should either be implemented or be a deliberate omission with a comment saying so. |
| `libs/veter-host` | GUI-free host engines: the host vt100 plus the PRT (`src/prt/`), VGE (`src/vge/`), VFT (`src/vft/`), SES (`src/ses/`), and VSS (`src/vss/`) engines. Links no GUI toolkit at all — the two desktop affordances VFT needs (native file picker, open-after-finalize) are the `vft::DesktopHooks` trait, which `veter` implements and `vsd` leaves at its `HeadlessHooks` default. Consumed by both `veter` and `vsd`. |
| `veter` | The GUI terminal (winit + glutin + femtovg + parley + swash). Owns the `veter-host` engines and their rendering. |
| `libs/veter-version` | The commit every binary was built from. A build script resolves the short sha and commit date at compile time and re-runs when `HEAD` moves; `long_version()` formats the `--version` line each binary prints. Exists because every crate here is `0.1.0` and stays `0.1.0`, so the crate version cannot answer "are these two machines running the same build?" — which is the question that comes up when a bug reproduces on one end of an SSH hop and not the other. No `.git` (a tarball build) reports `unknown` rather than failing. |
| `libs/vge-render` | Shared client-side helpers for rendering images to a VGE-aware terminal, plus the shared raw-TTY / poll / probe helpers every VGE client uses (`vcat`, `vplay`, `vdraw`, `vfm`, `spinner`, `breakout`). |
| `libs/vge-ui` | Shared client-side widget toolkit, extracted from `vmux`: accent theme (`theme`), rounded chrome paths (`shape`), the readline-style `LineEditor` (`edit`), the filterable `Picker` (`picker`), the prompt/picker/scrolling modal builders (`modal`), and the key + SGR-mouse `InputParser` (`input`). Pure `vge-protocol` consumer — builds draw commands and parses input, owns no state and does no I/O. Used by `vmux` and `vfm`. |
| `tools/vproto` | Speak VGE/PRT/SES from a script: a JSON array of commands on stdin becomes one envelope, and the terminal's reply comes back as JSON. Deserializes straight into the protocol crates' own types, so its surface *is* the wire format — the hand-written `vge-cli`/`prt-cli` it replaced reached 11 of VGE's 15 commands and could not name a cursor or marker anchor at all. `send` / `emit` / `measure` / `caps` / `schema`; `emit` writes envelope bytes instead of sending, which is how a VGE envelope becomes the `data_file` of a PRT `WritePortal`. |
| `tools/vplace` | Not a crate — a python script (plus the Claude Code `Stop` hook beside it) that places an image into a pane whose foreground program is something else. It can write to the pane but never read from it, so every command goes out with no request id and the cell metrics come from `vproto caps`. Space is reserved *in-band* by the application (a marker line plus a fenced gap); the script anchors to the marker and the image lands one row below it, leaving that line readable as a caption. All the arithmetic is `vproto measure`; what is left here is one renderer's conventions. |
| `tools/vmux` | Terminal multiplexer that runs *inside* veter, using PRT for panes and VGE for chrome (outlines, titles). Default prefix `Ctrl+Space`. |
| `tools/vcat` | Display images inside a VGE-aware terminal. |
| `tools/vplay` | Interactive image and video viewer for VGE-aware terminals. Left/right arrows seek in video mode and, in image mode, cycle the opened file's sibling stills (`src/playlist.rs`, lexicographical, directory scanned once at startup); `hjkl` always pans, since the arrows are taken. Every texture (the still and the two ping-ponged video frame slots) is uploaded with `Retention::Manual` — see `RETENTION` in `src/main.rs`: an `Auto` image is refcount-collected by the resize path's element wipe, and in the still's case by its own same-id swap's element retarget. vplay therefore releases each id by hand (each swap drops the id it supersedes; `TermExit` sweeps the `vplay-` prefix). |
| `tools/vdraw` | Interactive block-diagram editor. Draws with VGE, stores documents in Excalidraw's `.excalidraw` JSON schema. Takes mouse input in SGR-Pixels (DECSET 1016, enabled once the VGE probe answers and the cell size is known), so the pointer has sub-cell resolution — `input::Pos` carries the cell *and* the fractional position, and falls back to the cell's centre under a cell-only encoding. Geometry still snaps to a one-cell grid, whose points sit at cell *centres* (`drag::snap`). |
| `tools/vfm` | File browser with picture previews. ranger-style navigation (ancestor columns on the left, `h`/`l` to move up/in, per-directory cursor memory) with the current directory drawn as a Dolphin-style icon grid or, on `Tab`, as a detail list (`layout::View`; a list is the same geometry one tile wide, so scrolling and hit-testing have one implementation, and `+`/`-` sizes tiles in the grid and row heights in the list). Everything is VGE, including the filenames — the text layer renders below VGE, so a selection bar behind a name needs the name to be a `DrawText`. Thumbnails decode on worker threads (`src/thumbs.rs`, ffmpeg for video posters); copy/move/delete run on a file-operation worker (`src/ops.rs`). Enter opens a file with its configured program (`src/config.rs`, TOML at `~/.config/vfm/config.toml`; `src/open.rs` spawns it — in-terminal for editors/vplay, which suspends the VGE UI in `main` and restores it, or detached for GUI/xdg-open); `i` is the in-app preview. Two clipboards that never mix: `y`/`d`/`p` are vfm's own, while `Y` (paths, via OSC 52) and `Ctrl+Y` (the files, as a `text/uri-list` selection) drive the system one (`src/clip.rs`). Since an X11/Wayland selection dies with the process that owns it, `Ctrl+Y` re-execs the binary as a detached `--clipboard-serve` helper that holds it; the `system-clipboard` feature (default on, off for the musl dist build) gates that half. |
| `tools/vsend`, `tools/vrecv` | VFT clients: upload a local file to / pull a host-side file back from a VFT-aware terminal. `tools/vft-client` is their shared client library (raw-TTY guard, host-side frame stream, probe/cursor helpers, progress UI). |
| `tools/vsd` | Persistent veter session daemon — holds host vt100 / PRT / VGE state across renderer disconnects. |
| `tools/vssh` | SSH wrapper that keeps the veter tools fresh on remote hosts. |
| `tools/breakout`, `tools/spinner` | VGE demos. |

## Host-side byte pipeline (veter)

**One implementation**: `veter_host::pipeline::drive_chunk`, called by all three places the engines run — `veter/src/main.rs::App::process_pty_output` for the host grid, `prt::PrtEngine::cmd_write_portal` for every portal, and `tools/vsd/src/engines.rs::EngineState::process_chunk` for the daemon's mirror. It used to be written out separately, which is how the same ordering bug came to live in two of them with two different symptoms.

The pipeline is **SES, then a splitting stage, then a prefix of order-insensitive byte filters, then exactly one segment-aware terminal stage**. Output from the child PTY is fed through, in order:

0. **SES engine** — run by the caller, *outside* `drive_chunk`, because `vsd` forwards its passthrough to the renderer and needs it as one contiguous piece rather than per segment. SES carries no screen state, so where its envelopes sat in the stream doesn't matter to anything downstream. The immediate host consumes them: a local renderer is not a session and answers a `vmux` probe with "no session"; the daemon *is* one and answers with its name. Envelopes never reach the vt100.
1. **VSS engine** — extracts `ESC _ VSS …` snapshot frames and **splits the chunk at each one** (`VssEngine::process_pty_chunk_segments`). Everything below runs once per run of bytes between snapshots, with each restore applied in between. VSS cannot be just another filter: a completed snapshot doesn't *extract* state, it replaces the receiving context's vt100 / VGE / PRT engines wholesale, so a chunk carrying `[text][snapshot][command]` has text belonging to the screen being replaced and a command belonging to the engines that replaced it. Over SSH the daemon's snapshot and the inner multiplexer's redraw coalesce routinely, so that shape is an ordinary attach, not a corner case.
2. **PRT engine** — extracts `ESC _ PRT …` envelopes, dispatches portal commands, observes RIS / DECSTR / `2J` / `3J` / the alt-screen swaps (`?47`, `?1049`) for portal scope cleanup, and returns the leftover bytes as `passthrough`. Commands and those observations are applied **interleaved, in stream order** (`prt_protocol::apc::Output` is one ordered list for exactly this reason): a chunk can carry a screen swap and the `CreatePortal` that belongs to the screen it swapped *to*, which is what a multiplexer's startup looks like once a network hop coalesces its writes.
3. **VFT engine** — extracts `ESC _ VFT …` envelopes from PRT's passthrough.
4. **VGE engine + host vt100 parser** — the terminal stage, driven together by `veter_host::vge::drive_terminal_stage`. The VGE engine is also the sole responder to the terminal identification and mode queries — DA1, DA2, XTVERSION, DECRQM (`veter_host::query`) — at the host level *and* inside every portal: the vt100 fork parses them and answers none, and a program that sends DA1 blocks on the reply. DSR is the exception, answered by PRT inside a portal and by VGE at the host level, which is why the engine carries two separate auto-reply switches.

Stages 0, 2 and 3 are pure byte filters, and each one's APC parser passes the *other* extensions' markers through verbatim (a nested envelope inside a `WritePortal` payload is byte-stuffed, so no parser can steal another's bytes), so **their relative order is free** — which is what lets SES be hoisted out in front. Stage 1 must lead the walk, for the reason above. Stage 4 is not interchangeable with them: VGE element origins are viewport-relative *at command-processing time* (`doc/vector-graphics-extension.md` §5.2), so a command must be applied against the screen the sender saw. `drive_terminal_stage` therefore consumes ordered `Segment`s (`vge_protocol::apc::feed_segments`) rather than a whole chunk's payloads at once, feeding the vt100 the text that preceded each command before applying it. **Nothing may be inserted between VGE and the parser**, and anything else that becomes cursor- or grid-dependent (e.g. kitty graphics) belongs *in* that stage rather than after it.

After the chunk, the byte filters' `after_vt100_process` hooks observe the resulting screen state (scroll position, alt-screen swaps, scrollback eviction); VGE's already ran inside the terminal stage. Engine-generated responses/events are written back to the PTY master.

`vsd` runs the same walk, and what it forwards to an attached renderer is its SES passthrough — everything byte for byte except the SES envelopes, which only the daemon can answer (it is the process that knows the session name) and which would otherwise reach a renderer whose own per-portal SES engine would answer "not in a session". It holds a `VssEngine` too, despite being a snapshot *sender*: a `vsd attach` to a second session, run from a shell inside this one, writes `ESC _ VSS …` straight onto this session's pty, and the daemon has to apply it for the same reason the renderer does — the mirror must match the screen it is a copy of, or the next attach ships a snapshot of a session that has moved on. The "answered exactly once" switch suppresses the daemon's VGE, DSR, PRT and VSS replies while a renderer is attached; that renderer runs the same commands off the forwarded chunk and answers them itself.

## What answers a query, and where

Three layers reply to the child, and which one owns a given sequence is
not obvious from the sequence:

- **The vt100 fork** applies everything that only changes screen state
  — SGR, the mode families (SM/RM and DECSET/DECRST), DECSTR, DECSCUSR
  — and routes what it cannot answer alone to a `vt100::Callbacks`
  method.
- **`veter-host::query`** owns the *format* of every reply that names
  the terminal: DA1, DA2, XTVERSION, DECRQM, the XTWINOPS size reports
  and the OSC colour reports. Two engines emit them (VGE at host level,
  PRT inside a portal) and they must not disagree about what terminal
  this is.
- **The renderer** answers what only it knows — cell pixel metrics, the
  palette, the default fore/background — through `HostCallbacks`, which
  queues a `TerminalRequest` for `App::drain_terminal_requests` rather
  than replying in place. Inside a portal the same callbacks re-emit as
  PRT `Osc` events, because there the client owns the answer.

A query that goes unanswered is not a cosmetic bug: the sender blocks on
it. That is what an unanswered DA1 cost every vim/tmux launch, and what
an unanswered `OSC 11 ; ?` costs them now.

## Portals are recursive

A portal owns a private vt100 instance and its own PRT/VGE state. Portals nest by recursion — the inner program speaks the same protocol over its own PTY, and the host's per-portal APC parser handles its envelopes (`max_nesting_depth` defaults to 8). When working inside `prt::PrtEngine` / `prt::Portal`, remember that almost everything the host engine does (scope reset, erase-display cleanup, scrollback eviction, alt-screen swap, VSS snapshot restore) must also be implemented per-portal.

## VGE hit-testing rides the render pass

A VGE element has no extent on the wire, and where it lands on screen is the product of scrollback anchoring (§5.2), its affine transform (§9.11), the ancestor chain of clip rects (§9.2) and — inside a portal — that portal's origin and clip. So there is no second geometry walk: `vge::render` pushes a `vge::pick::PickItem` for every `DrawText` / `DrawImage` it paints, taking the device matrix straight off the canvas (`Canvas::transform()` — femtovg's `set_transform` premultiplies, so it already carries the portal translation and every ancestor transform). `TerminalRenderer::pick` holds the result, rebuilt each frame and read on the next pointer event. **Do not add a parallel hit-test walk** — extend the pass instead. Only text and images are indexed, so a client's full-screen background stays transparent to the pointer and doesn't swallow grid selection under it. Text geometry has one source of truth too: `TerminalRenderer::layout_vge_text` shapes a run for both drawing and hit-testing, so a click can't resolve to a character other than the one painted there — the run's character boundaries are interned into the pick list as it is drawn.

That index also answers VGE's `QueryHit` (`doc/vector-graphics-extension.md` §15), the one thing a client can't work out for itself: the engine takes a `vge::state::HitTester` (implemented by `veter`'s `PickTester`, left `None` by `vsd`, which holds the state but does not paint it), and each PRT recursion level wraps it in a `ScopedHitTester` so the portal path assembles itself on the way back out. The answer describes the last painted frame; VGE carries no unsolicited frames and input still never crosses the protocol.

That index backs host-side selection of VGE content (`doc/vector-graphics-extension.md` §14): `App::vge_selection` is one `VgeSelection` — either a byte range inside a `DrawText` run or a whole `DrawImage` — mutually exclusive with the grid `selection`, and its highlight (reverse-video span, or an outline) is painted by the selected command's own draw call, so it shares that command's transform, clip and layout. Copying an image reads `UploadedImage.pixels`, cropped to the `DrawImage`'s `source_rect`. All of it is terminal-local; no protocol frames are involved.

## Input never crosses PRT

PRT carries display direction only. Keystrokes/mouse go from the host's PTY straight to the inner program's PTY master FD — `WritePortal` is not an input channel. `SetFocus` is purely a rendering hint. This is the contract every multiplexer client (including `vmux`) is built on; do not invent input-over-PRT shortcuts.

## Sessions (vsd)

`vsd` is a persistent host-side session manager that holds a session's state (vt100 grids, scrollback, VGE/PRT/image tables, inner PTYs) across disconnections of the rendering client — the motivating case is SSH survivability. On attach it ships that state to the renderer as a **VSS** binary snapshot; **SES** is the sidecar control channel a `vmux` client uses to learn its session name and to detach (`Ctrl+\ d`). Because the host engines are factored into `veter-host`, `vsd` and the `veter` GUI run the same engine code. See `doc/session-manager.md` and `doc/session-extension.md`.

## veter spawns vmux by default

`veter/src/pty.rs` execs `vmux` (first the binary next to `veter`, then `$PATH`) before falling back to `$SHELL` / `/bin/sh`. So launching `veter` normally drops you into `vmux` — bypass with e.g. `SHELL=/bin/bash` and a `vmux`-free `PATH`, or run a different binary. Tests and headless work should run individual crates with `cargo run -p …` rather than going through `veter`.

## Conventions

- Specs in `doc/` are normative. If code disagrees with them, the spec wins; if the spec is wrong, update both. Section numbers (e.g. `§5.2`, `§9.1`) referenced in code comments map to those documents.
- The `*-protocol` crates must stay pure wire format — no rendering, no terminal state, no I/O. Anything else belongs in the consuming crate (`veter-host` for host state, `veter` for GUI, the tools for clients).
- Limits (`max_portals`, `max_portal_cells_*`, `max_write_bytes`, `max_nesting_depth`, …) are advertised in the probe response; the recommended defaults from `portal-extension.md` §12 live in `prt::Limits::default`.
