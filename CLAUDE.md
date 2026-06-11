# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

The project is **Veter** (Russian: ветер, "wind") — a GUI terminal emulator built around two custom protocols: PRT (portals / multiplexing) and VGE (vector graphics in the grid). The terminal binary itself is `veter`; the supporting tools and library crates keep their `v`-prefix names (`vmux`, `vcat`, `vge-cli`, `prt-cli`, `vge-protocol`, `prt-protocol`).

## Build & run

Cargo workspace; edition 2024 (the vendored `vt100` fork stays on 2021).

- Build everything: `cargo build` (release: `cargo build --release`)
- Build one crate: `cargo build -p veter` (or `vmux`, `vcat`, `vge-cli`, `prt-cli`, `vge-protocol`, `prt-protocol`, `breakout`)
- Run the GUI terminal: `cargo run -p veter`
- Install `veter`/`vcat`/`vmux` to `$PREFIX/bin` (default `~/.local`) plus a desktop entry: `make install` (override `PREFIX=...` to retarget; `make uninstall` removes them)

## Tests

Most tests are inline `#[cfg(test)]` modules — there is no separate test harness layout to learn.

- Run the whole suite: `cargo test`
- One crate: `cargo test -p prt-protocol`
- One test by name substring: `cargo test -p prt-protocol envelope_roundtrip`

The only integration test directory is `vge-cli/tests/cli_roundtrip.rs`.

## Repository layout

Two custom terminal protocols live under `doc/` and drive the entire codebase. Read these before making non-trivial changes:

- `doc/portal-extension.md` — PRT, an APC-framed protocol (`ESC _ PRT … ESC \`) for carving the host grid into per-portal sub-terminals (multiplexer panes, PiP log views, scrollback-anchored snapshots).
- `doc/vector-graphics-extension.md` — VGE, an APC-framed protocol (`ESC _ VGE … ESC \`) for vector/raster graphics inside the grid.

Each extension is split into a wire-format crate and host-side state lives only in `veter`:

| Crate | Role |
|---|---|
| `vge-protocol`, `prt-protocol` | Pure wire format only: APC stream parser, primitive codec, command/response/event framing, encoders. No state, no rendering. Both host and clients depend on these. |
| `vt100` | Local fork of the vt100 parser (adds `clear_scrollback`, xterm-style push/pull vertical resize, and the `origin_shift` / `scroll_committed` counters the engines' line trackers consume). The screen model the host and every portal use. |
| `veter` | The GUI terminal (winit + glutin + femtovg + parley + swash). Owns the host vt100, the host-side PRT engine (`src/prt/`), and the host-side VGE engine (`src/vge/`). |
| `vge-cli`, `prt-cli` | Emit raw envelopes for manual protocol testing. |
| `tools/vmux` | Terminal multiplexer that runs *inside* veter, using PRT for panes and VGE for chrome (outlines, titles). Default prefix `Ctrl+Space`. |
| `tools/vcat` | Display images inside a VGE-aware terminal. |
| `tools/breakout` | VGE demo. |

## Host-side byte pipeline (veter)

`veter/src/main.rs::App::process_pty_output` is the load-bearing path. Output from the child PTY is fed through, in order:

1. **PRT engine** (`src/prt/state.rs`) — extracts `ESC _ PRT …` envelopes, dispatches portal commands, observes RIS / DECSTR / `2J` / `3J` for portal scope cleanup, and returns the leftover bytes as `passthrough`.
2. **VGE engine** (`src/vge/state.rs`) — extracts `ESC _ VGE …` envelopes from PRT's passthrough.
3. **Host vt100 parser** — receives whatever both engines passed through.

Each engine's APC parser passes the *other* extension's marker through verbatim, so the order in step 1/2 is correctness-independent. After the host vt100 runs, both engines' `after_vt100_process` hooks observe the resulting screen state (scroll position, alt-screen swaps, scrollback eviction). Engine-generated responses/events are written back to the PTY master.

## Portals are recursive

A portal owns a private vt100 instance and its own PRT/VGE state. Portals nest by recursion — the inner program speaks the same protocol over its own PTY, and the host's per-portal APC parser handles its envelopes (`max_nesting_depth` defaults to 8). When working inside `prt::PrtEngine` / `prt::Portal`, remember that almost everything the host engine does (scope reset, erase-display cleanup, scrollback eviction, alt-screen swap) must also be implemented per-portal.

## Input never crosses PRT

PRT carries display direction only. Keystrokes/mouse go from the host's PTY straight to the inner program's PTY master FD — `WritePortal` is not an input channel. `SetFocus` is purely a rendering hint. This is the contract every multiplexer client (including `vmux`) is built on; do not invent input-over-PRT shortcuts.

## veter spawns vmux by default

`veter/src/pty.rs` execs `vmux` (first the binary next to `veter`, then `$PATH`) before falling back to `$SHELL` / `/bin/sh`. So launching `veter` normally drops you into `vmux` — bypass with e.g. `SHELL=/bin/bash` and a `vmux`-free `PATH`, or run a different binary. Tests and headless work should run individual crates with `cargo run -p …` rather than going through `veter`.

## Conventions

- Specs in `doc/` are normative. If code disagrees with them, the spec wins; if the spec is wrong, update both. Section numbers (e.g. `§5.2`, `§9.1`) referenced in code comments map to those documents.
- The `*-protocol` crates must stay pure wire format — no rendering, no terminal state, no I/O. Anything else belongs in the consuming crate.
- Limits (`max_portals`, `max_portal_cells_*`, `max_write_bytes`, `max_nesting_depth`, …) are advertised in the probe response; the recommended defaults from `portal-extension.md` §12 live in `prt::Limits::default`.
