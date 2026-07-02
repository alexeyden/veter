# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

The project is **Veter** (Russian: ветер, "wind") — a GUI terminal emulator built around a family of APC-framed protocols that ride a single PTY: PRT (portals / multiplexing), VGE (vector graphics in the grid), VFT (file transfer), plus a session layer (SES + VSS) that keeps sessions alive across renderer disconnects. The terminal binary itself is `veter`; the supporting tools and library crates keep their names (`vmux`, `vcat`, `vplay`, `vsend`, `vrecv`, `vsd`, `vssh`, `vge-cli`, `prt-cli`, and the `*-protocol` wire crates).

## Build & run

Cargo workspace; edition 2024 (the vendored `vt100` fork stays on 2021).

- Build everything: `cargo build` (release: `cargo build --release`)
- Build one crate: `cargo build -p veter` (or `veter-host`, `vmux`, `vcat`, `vplay`, `vsend`, `vrecv`, `vsd`, `vssh`, `vge-cli`, `prt-cli`, `vge-protocol`, `prt-protocol`, `vft-protocol`, `ses-protocol`, `vss-protocol`, `breakout`)
- Run the GUI terminal: `cargo run -p veter`
- Install `veter` plus the tool set (`vcat`, `vplay`, `vmux`, `vsend`, `vrecv`, `vsd`, `vssh`) to `$PREFIX/bin` (default `~/.local`) plus a desktop entry: `make install` (override `PREFIX=...` to retarget; `make uninstall` removes them). `make install-remote-<arch>` cross-compiles a musl build and scp-installs it to `$REMOTE`.

## Tests

Most tests are inline `#[cfg(test)]` modules — there is no separate test harness layout to learn.

- Run the whole suite: `cargo test`
- One crate: `cargo test -p prt-protocol`
- One test by name substring: `cargo test -p prt-protocol envelope_roundtrip`

The only integration test directory is `vge-cli/tests/cli_roundtrip.rs`.

## Repository layout

The protocols live under `doc/` and drive the entire codebase. Read the relevant one before making non-trivial changes:

- `doc/portal-extension.md` — PRT, an APC-framed protocol (`ESC _ PRT … ESC \`) for carving the host grid into per-portal sub-terminals (multiplexer panes, PiP log views, scrollback-anchored snapshots).
- `doc/vector-graphics-extension.md` — VGE, an APC-framed protocol (`ESC _ VGE … ESC \`) for vector/raster graphics inside the grid.
- `doc/file-transfer-extension.md` — VFT (`ESC _ VFT … ESC \`), a bidirectional file-transfer channel: a CLI inside the terminal hands the host a local file (`vsend`) or pulls a host-side file back (`vrecv`). **WIP, v0** — wire format may change; clients and host ship in lockstep.
- `doc/session-manager.md` — `vsd`, the persistent session daemon, and **VSS** (`ESC _ VSS … ESC \`, in `doc/session-manager.md` §4), the binary engine-snapshot protocol it uses to ship state to an attaching renderer.
- `doc/session-extension.md` — SES (`ESC _ SES … ESC \`), the small `vmux` ↔ `vsd` control channel (session-name probe, detach command).

Host-side engine state (the vt100 grids and all five engines) lives in **`veter-host`**, GUI-free, so the same code backs both the `veter` GUI binary and the headless `vsd` daemon. The `veter` crate keeps only the GUI and the render/glue side of each engine (`src/prt/render.rs`, `src/vge/render.rs`, `src/vft/`, `src/ses.rs`, `src/vss.rs`).

| Crate | Role |
|---|---|
| `vge-protocol`, `prt-protocol`, `vft-protocol`, `ses-protocol`, `vss-protocol` | Pure wire format only: APC stream parser, primitive codec, command/response/event framing, encoders. No state, no rendering. Host and clients both depend on these. |
| `vt100` | Local fork of the vt100 parser (adds `clear_scrollback`, xterm-style push/pull vertical resize, `binary_snapshot`/`restore_from_binary_snapshot` for VSS, and the `origin_shift` / `scroll_committed` counters the engines' line trackers consume). The screen model the host and every portal use. |
| `veter-host` | GUI-free host engines: the host vt100 plus the PRT (`src/prt/`), VGE (`src/vge/`), VFT (`src/vft/`), SES (`src/ses/`), and VSS (`src/vss/`) engines and the shared line tracker. `gui` feature pulls desktop-only deps VFT needs on a real renderer. Consumed by both `veter` and `vsd`. |
| `veter` | The GUI terminal (winit + glutin + femtovg + parley + swash). Owns the `veter-host` engines and their rendering. |
| `vge-render` | Shared client-side helpers for rendering images to a VGE-aware terminal (used by `vcat`, `vplay`). |
| `vge-cli`, `prt-cli` | Emit raw envelopes for manual protocol testing. |
| `tools/vmux` | Terminal multiplexer that runs *inside* veter, using PRT for panes and VGE for chrome (outlines, titles). Default prefix `Ctrl+Space`. |
| `tools/vcat` | Display images inside a VGE-aware terminal. |
| `tools/vplay` | Interactive image and video viewer for VGE-aware terminals. |
| `tools/vsend`, `tools/vrecv` | VFT clients: upload a local file to / pull a host-side file back from a VFT-aware terminal. `tools/vft-client` is their shared client library (raw-TTY guard, host-side frame stream, probe/cursor helpers, progress UI). |
| `tools/vsd` | Persistent veter session daemon — holds host vt100 / PRT / VGE state across renderer disconnects. |
| `tools/vssh` | SSH wrapper that keeps the veter tools fresh on remote hosts. |
| `tools/breakout`, `tools/spinner` | VGE demos. |

## Host-side byte pipeline (veter)

`veter/src/main.rs::App::process_pty_output` is the load-bearing path. Output from the child PTY is fed through, in order:

1. **PRT engine** — extracts `ESC _ PRT …` envelopes, dispatches portal commands, observes RIS / DECSTR / `2J` / `3J` for portal scope cleanup, and returns the leftover bytes as `passthrough`.
2. **VGE engine** — extracts `ESC _ VGE …` envelopes from PRT's passthrough.
3. **VFT engine** — extracts `ESC _ VFT …` envelopes from VGE's passthrough.
4. **VSS engine** — extracts `ESC _ VSS …` snapshot frames; a completed host-level snapshot replaces the host's vt100 / VGE / PRT engines wholesale (the common case is per-portal snapshots handled recursively inside `prt::WritePortal`).
5. **SES engine** — consumed by the immediate host; the local renderer is not a session, so it just answers a `vmux` probe with "no session". Envelopes never reach the host vt100.
6. **Host vt100 parser** — receives whatever all engines passed through.

Each engine's APC parser passes the *other* extensions' markers through verbatim, so the pipeline order is correctness-independent. After the host vt100 runs, the engines' `after_vt100_process` hooks observe the resulting screen state (scroll position, alt-screen swaps, scrollback eviction). Engine-generated responses/events are written back to the PTY master.

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
