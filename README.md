# Veter

Veter (Russian: *ветер*, "wind") is a GUI terminal emulator built around two custom protocols:

- **PRT** — Portal Extension. Carves the terminal grid into recursive sub-terminals (multiplexer panes, PiP log views, scrollback-anchored snapshots). See [`doc/portal-extension.md`](doc/portal-extension.md).
- **VGE** — Vector Graphics Extension. Vector primitives, text, and images rendered directly inside the terminal grid. See [`doc/vector-graphics-extension.md`](doc/vector-graphics-extension.md).

Both protocols are framed as APC envelopes (`ESC _ … ESC \`) so they pass cleanly through pipes, `script(1)`, and other terminal tooling.

## Tools

| Crate | Purpose |
|---|---|
| `vterm` | The GUI terminal (winit + glutin + femtovg + parley + swash). Owns the host-side PRT and VGE engines. |
| `vmux` | Terminal multiplexer that runs *inside* `vterm`, using PRT for panes and VGE for chrome. Default prefix `Ctrl+Space`. |
| `vcat` | Display images inside a VGE-aware terminal. |
| `vge-cli`, `prt-cli` | Emit raw protocol envelopes for manual testing. |
| `vge-protocol`, `prt-protocol` | Pure wire-format crates — APC parser, codec, encoders. No state, no rendering. |
| `vt100` | Vendored fork of the `vt100` parser (adds `clear_scrollback`). |
| `breakout` | VGE demo. |

## Build

Cargo workspace, edition 2024 (the vendored `vt100` fork stays on 2021).

```sh
cargo build --release
cargo run -p vterm
```

## Install

```sh
make install      # binaries to $PREFIX/bin (default ~/.local) plus a desktop entry
make uninstall
```

Override `PREFIX=...` to retarget.

## Tests

```sh
cargo test                  # whole workspace
cargo test -p prt-protocol  # one crate
```
