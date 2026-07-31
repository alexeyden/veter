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
libs/       veter-host vge-render vge-ui   — shared implementation crates
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
| `vendored/vt100` | Local fork of the vt100 parser (adds `clear_scrollback`, xterm-style push/pull vertical resize, `binary_snapshot`/`restore_from_binary_snapshot` for VSS, the `scroll_committed` counter the PRT activity heuristic watches, and `top_of_live_screen` — the absolute scrollback line index VGE elements and Scrollback portals anchor to, maintained by the grid itself and carried in its snapshot). The screen model the host and every portal use. |
| `libs/veter-host` | GUI-free host engines: the host vt100 plus the PRT (`src/prt/`), VGE (`src/vge/`), VFT (`src/vft/`), SES (`src/ses/`), and VSS (`src/vss/`) engines. Links no GUI toolkit at all — the two desktop affordances VFT needs (native file picker, open-after-finalize) are the `vft::DesktopHooks` trait, which `veter` implements and `vsd` leaves at its `HeadlessHooks` default. Consumed by both `veter` and `vsd`. |
| `veter` | The GUI terminal (winit + glutin + femtovg + parley + swash). Owns the `veter-host` engines and their rendering. |
| `libs/vge-render` | Shared client-side helpers for rendering images to a VGE-aware terminal, plus the shared raw-TTY / poll / probe helpers every VGE client uses (`vcat`, `vplay`, `vdraw`, `vfm`, `spinner`, `breakout`). |
| `libs/vge-ui` | Shared client-side widget toolkit, extracted from `vmux`: accent theme (`theme`), rounded chrome paths (`shape`), the readline-style `LineEditor` (`edit`), the filterable `Picker` (`picker`), the prompt/picker/scrolling modal builders (`modal`), and the key + SGR-mouse `InputParser` (`input`). Pure `vge-protocol` consumer — builds draw commands and parses input, owns no state and does no I/O. Used by `vmux` and `vfm`. |
| `tools/vproto` | Speak VGE/PRT/SES from a script: a JSON array of commands on stdin becomes one envelope, and the terminal's reply comes back as JSON. Deserializes straight into the protocol crates' own types, so its surface *is* the wire format — the hand-written `vge-cli`/`prt-cli` it replaced reached 11 of VGE's 15 commands and could not name a cursor or marker anchor at all. `send` / `emit` / `measure` / `caps` / `schema`; `emit` writes envelope bytes instead of sending, which is how a VGE envelope becomes the `data_file` of a PRT `WritePortal`. |
| `tools/vplace` | Not a crate — a python script (plus the Claude Code `Stop` hook beside it) that places an image into a pane whose foreground program is something else. It can write to the pane but never read from it, so every command goes out with no request id and the cell metrics come from `vproto caps`. Space is reserved *in-band* by the application (a marker line plus a fenced gap); the script anchors to the marker and the image lands one row below it, leaving that line readable as a caption. All the arithmetic is `vproto measure`; what is left here is one renderer's conventions. |
| `tools/vmux` | Terminal multiplexer that runs *inside* veter, using PRT for panes and VGE for chrome (outlines, titles). Default prefix `Ctrl+Space`. |
| `tools/vcat` | Display images inside a VGE-aware terminal. |
| `tools/vplay` | Interactive image and video viewer for VGE-aware terminals. Left/right arrows seek in video mode and, in image mode, cycle the opened file's sibling stills (`src/playlist.rs`, lexicographical, directory scanned once at startup); `hjkl` always pans, since the arrows are taken. Every texture (the still and the two ping-ponged video frame slots) is uploaded with `Retention::Manual` — see `RETENTION` in `src/main.rs`: an `Auto` image is refcount-collected by the resize path's element wipe, and in the still's case by its own same-id swap's element retarget. vplay therefore releases each id by hand (each swap drops the id it supersedes; `TermExit` sweeps the `vplay-` prefix). |
| `tools/vdraw` | Interactive block-diagram editor. Draws with VGE, stores documents in Excalidraw's `.excalidraw` JSON schema. Geometry snaps to the cell grid, since SGR mouse reports are only cell-accurate. |
| `tools/vfm` | File browser with picture previews. ranger-style navigation (ancestor columns on the left, `h`/`l` to move up/in, per-directory cursor memory) with the current directory drawn as a Dolphin-style icon grid or, on `Tab`, as a detail list (`layout::View`; a list is the same geometry one tile wide, so scrolling and hit-testing have one implementation, and `+`/`-` sizes tiles in the grid and row heights in the list). Everything is VGE, including the filenames — the text layer renders below VGE, so a selection bar behind a name needs the name to be a `DrawText`. Thumbnails decode on worker threads (`src/thumbs.rs`, ffmpeg for video posters); copy/move/delete run on a file-operation worker (`src/ops.rs`). Enter opens a file with its configured program (`src/config.rs`, TOML at `~/.config/vfm/config.toml`; `src/open.rs` spawns it — in-terminal for editors/vplay, which suspends the VGE UI in `main` and restores it, or detached for GUI/xdg-open); `i` is the in-app preview. Two clipboards that never mix: `y`/`d`/`p` are vfm's own, while `Y` (paths, via OSC 52) and `Ctrl+Y` (the files, as a `text/uri-list` selection) drive the system one (`src/clip.rs`). Since an X11/Wayland selection dies with the process that owns it, `Ctrl+Y` re-execs the binary as a detached `--clipboard-serve` helper that holds it; the `system-clipboard` feature (default on, off for the musl dist build) gates that half. |
| `tools/vsend`, `tools/vrecv` | VFT clients: upload a local file to / pull a host-side file back from a VFT-aware terminal. `tools/vft-client` is their shared client library (raw-TTY guard, host-side frame stream, probe/cursor helpers, progress UI). |
| `tools/vsd` | Persistent veter session daemon — holds host vt100 / PRT / VGE state across renderer disconnects. |
| `tools/vssh` | SSH wrapper that keeps the veter tools fresh on remote hosts. |
| `tools/breakout`, `tools/spinner` | VGE demos. |

## Host-side byte pipeline (veter)

`veter/src/main.rs::App::process_pty_output` is the load-bearing path. The pipeline is **a prefix of order-insensitive byte filters, then exactly one segment-aware terminal stage**. Output from the child PTY is fed through, in order:

1. **PRT engine** — extracts `ESC _ PRT …` envelopes, dispatches portal commands, observes RIS / DECSTR / `2J` / `3J` for portal scope cleanup, and returns the leftover bytes as `passthrough`.
2. **VFT engine** — extracts `ESC _ VFT …` envelopes from PRT's passthrough.
3. **VSS engine** — extracts `ESC _ VSS …` snapshot frames; a completed host-level snapshot replaces the host's vt100 / VGE / PRT engines wholesale (the common case is per-portal snapshots handled recursively inside `prt::WritePortal`).
4. **SES engine** — consumed by the immediate host; the local renderer is not a session, so it just answers a `vmux` probe with "no session". Envelopes never reach the host vt100.
5. **VGE engine + host vt100 parser** — the terminal stage, driven together by `veter_host::vge::drive_terminal_stage`.

Stages 1–4 are pure byte filters, and each one's APC parser passes the *other* extensions' markers through verbatim, so **their relative order is free**. Stage 5 is not interchangeable with them: VGE element origins are viewport-relative *at command-processing time* (`doc/vector-graphics-extension.md` §5.2), so a command must be applied against the screen the sender saw. `drive_terminal_stage` therefore consumes ordered `Segment`s (`vge_protocol::apc::feed_segments`) rather than a whole chunk's payloads at once, feeding the vt100 the text that preceded each command before applying it. **Nothing may be inserted between VGE and the parser**, and anything else that becomes cursor- or grid-dependent (e.g. kitty graphics) belongs *in* that stage rather than after it.

After the chunk, the byte filters' `after_vt100_process` hooks observe the resulting screen state (scroll position, alt-screen swaps, scrollback eviction); VGE's already ran inside the terminal stage. Engine-generated responses/events are written back to the PTY master.

The same shape repeats in the two other places the engines run: the per-portal path in `prt::PrtEngine::cmd_write_portal`, and `vsd`'s worker loop in `tools/vsd/src/engines.rs`.

## Portals are recursive

A portal owns a private vt100 instance and its own PRT/VGE state. Portals nest by recursion — the inner program speaks the same protocol over its own PTY, and the host's per-portal APC parser handles its envelopes (`max_nesting_depth` defaults to 8). When working inside `prt::PrtEngine` / `prt::Portal`, remember that almost everything the host engine does (scope reset, erase-display cleanup, scrollback eviction, alt-screen swap, VSS snapshot restore) must also be implemented per-portal.

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
