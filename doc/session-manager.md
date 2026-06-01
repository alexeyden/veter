# Veter Session Manager (vsd)

> **Status: WIP.** The architecture (§§1–3, 5–7), the **v2
> binary-snapshot protocol** (§4 VSS), and the **v3 per-session
> process model** (§2, §8.3) are implemented. Tracked in
> `CONTEXT.md`. The companion `doc/session-extension.md` (SES — the
> vmux ↔ vsd control channel) is now implemented.

`vsd` is a persistent host-side session manager. Its role is to
hold the state of a veter session (vt100 grids, scrollback, VGE
element tables, image tables, PRT portal trees, inner PTYs) across
disconnections of the rendering client. The motivating use case is
SSH: a user works inside a tab whose contents are owned by a `vsd`
running on a remote machine, drops the SSH connection (or just walks
away and closes their laptop), and on next attach sees the same
screen, scrollback, and running programs — without the local
renderer having to know anything about session continuity.

Persistent local sessions (close-the-window-reopen-the-window) fall
out of the same design for free, but they are not the driving
requirement.

## 1. Architecture

### 1.1 Picture

```
                                  hostA                            hostB
       ┌────────┐    ssh     ┌──────────────┐         ┌────────┐    ssh    ┌──────────────┐
local: │ veter  │ ─────────▶ │ ssh server   │         │ veter  │ ─────────▶ │ ssh server   │
       │ + vmux │            └──────────────┘         │ + vmux │            └──────────────┘
       │ tab A  │                    │                │ tab B  │                    │
       │ tab B  │                    ▼                │        │                    ▼
       └────────┘            ┌───────────────┐        └────────┘            ┌───────────────┐
            ▲                │ vsd attach │              ▲               │ vsd attach │
            │                │ cool (CLI)    │              │               │ another (CLI) │
            │                └───────┬───────┘              │               └───────┬───────┘
            │                        │ SCM_RIGHTS           │                       │
            │                        ▼                      │                       ▼
            │            ┌──────────────────────┐           │           ┌──────────────────────┐
            │            │ vsd --session     │           │           │ vsd --session     │
            │            │   cool               │           │           │   another            │
            │            │ ─ host vt100         │           │           │ ─ host vt100         │
            │            │ ─ PRT engine         │           │           │ ─ VGE engine         │
            │            │ ─ inner PTY (bash)   │           │           │ ─ inner PTY (bash)   │
            │            │ ─ <cool>.sock        │           │           │ ─ <another>.sock     │
            │            └──────────────────────┘           │           └──────────────────────┘
            │                                               │
            └──────────── PRT / VGE / VSS byte stream ──────┘
                          (rides the existing SSH PTY;
                           each session process emits
                           envelopes inside its tab's portal)
```

One process per session. Multiple sessions on the same machine =
multiple `vsd --session` processes, each with its own socket.
The CLI front-end (`new` / `attach` / `list` / `kill`) is short-
lived and talks to per-session sockets — there is no central
daemon.

### 1.2 Roles

| Component | Where it runs | What it owns |
|---|---|---|
| `veter` | local, GUI | winit/glutin window, paint loop, host engines for *local* state (top-level vt100, PRT engine, VGE engine). Same as today. |
| `vmux` | local, inside `veter` | Tabs and panes. Same as today. Each pane is a PRT portal in local `veter`. |
| ssh client | local, inside a vmux pane | Unmodified. Transports stdio bytes to the remote host. |
| `vsd` CLI (`new` / `attach` / `list` / `kill`) | remote, inside the SSH PTY | Thin, short-lived. `attach` hands its stdio over to the session process and stays blocked until detach. The others do single-shot IPC and exit. |
| `vsd --session NAME` | remote, background | One process per session. Owns the inner PTY + host engines + a per-session socket at `$XDG_RUNTIME_DIR/vsd/<NAME>.sock`. Spawned by `vsd new` via double-fork-and-exec. |
| inner program | remote, inside a session | The user's shell, vim, htop, vmux-on-remote, anything. |

The whole chain between local `veter` and a remote `vsd --session`
is the plain SSH stdin/stdout pair. **No TCP socket, no port forwarding, no
custom transport.** The bytes on the wire are exactly the existing
PRT / VGE / VFT envelopes. Local `veter`'s portal parser already
treats every portal as a recursive host, so a remote `vsd`
emitting envelopes inside an SSH-mediated pane is structurally
identical to any other inner program emitting envelopes.

### 1.3 Why "split the host" instead of "split vmux"

An earlier draft proposed a `vmuxd` daemon with snapshot/replay
handled at the multiplexer layer. That approach was rejected:

- VGE elements are addressable and stateful. Daemon-side replay
  would need to track every element ever created — effectively
  reimplementing the host VGE engine inside the daemon.
- Replaying raw bytes through a daemon-side vt100 would duplicate
  the host's vt100 parser. Two parallel terminal stacks is a tax
  on every change.

The host already owns this state authoritatively. Moving the
persistent boundary to *the host process itself* (`vsd`) means:

- Engines live in exactly one place.
- The renderer (local `veter`) stays as-is — it has no notion of
  session continuity, just paints whatever envelopes arrive.
- `vmux` stays as a single process and is unchanged.

The cost is a snapshot serializer inside the host engines (§4); it
is the only genuinely new piece of protocol-adjacent code in the
plan.

## 2. vsd

### 2.1 Process model

`vsd` runs **one process per session**, not a single daemon
hosting many. Each session is a long-lived background process
listening on its own Unix socket at
`$XDG_RUNTIME_DIR/vsd/<NAME>.sock` (mode `0600`, owner-only).
The runtime dir itself is `0700`. The CLI front-end is short-lived:
it talks to per-session sockets and never owns engine state.

Each session process owns:

- An inner PTY pair, with a child process (default `$SHELL`, or
  whatever was passed to `vsd new`) running on its slave side.
- Host engine instances:
  - vt100 (the vendored `vt100` fork) for the top-level grid.
  - PRT engine (recursive — children for any nested portals the
    inner program spawns).
  - VGE engine (element table, style table, image table).
- Its own accept loop on `<NAME>.sock`, a worker thread reading the
  inner PTY master, and (during an attach) a handler thread plus a
  winsize-watcher thread.

Sessions are identified by string name (≤ 64 bytes, the same rules
as element IDs in §6.8 of the portal spec). The runtime directory
acts as the name registry: a name exists iff `<NAME>.sock` is
connectable. Stale sockets from crashed session processes are
auto-unlinked on the next probe.

State is in-memory only. A session process exiting (intentionally
or via crash / signal) drops that session's state; other sessions
are unaffected.

### 2.2 Lifecycle

A session process:

- Starts when `vsd new NAME [argv...]` re-execs the binary into
  `--session NAME [argv...]` inside a double-forked detached child
  (stdio redirected to `<NAME>.log`).
- Exits when:
  - the inner PTY child exits (worker observes EOF, main loop
    breaks),
  - a `Kill` IPC arrives,
  - SIGTERM is delivered.
- On exit, a `SocketGuard` Drop impl unlinks `<NAME>.sock` so the
  runtime dir stays clean.

There is no central server, no `kill-server`, no auto-spawn-on-
attach. `vsd attach NAME` on a nonexistent session errors out.

### 2.3 CLI surface

```text
vsd new [-a] NAME [argv ...]   # spawn a new session; -a attaches afterwards
vsd attach NAME                # attach the calling terminal
vsd list                       # enumerate live sessions
vsd kill NAME                  # tear down NAME
```

`new` (without `-a`) returns once the session's socket is responding
(3 s deadline). `attach` is the only command that takes over the
caller's stdio. `list` is a CLI-side directory scan + per-socket
`Status` round-trip — no central state to query. `kill` is a single
`Request::Kill` round-trip on the session's socket.

Internal modes invoked by `new`:

```text
vsd --session NAME [argv ...]              # detached session backend
vsd --foreground-session NAME [argv ...]   # non-detached, for debugging
```

These are hidden from `--help`; users only invoke the four
subcommands above.

### 2.4 Stdio handover

When `vsd attach NAME` is invoked, the CLI:

1. Connects to `<NAME>.sock`.
2. Sends `Request::Attach`.
3. Passes its stdin and stdout fds to the session via `SCM_RIGHTS`
   over the same socket.
4. Blocks on `read` until the session closes the connection.

The session process now owns those fds directly. From that point
on, the SSH PTY is glued to the session process (not to the original
CLI process), which means:

- No long-lived `vsd-attach` middleman is consuming a PTY slot.
- Detaching is a matter of the session closing its end of the fds
  and going silent; ssh-side stdio falls back to whatever shell
  the user was in before the attach.

If a session is already attached when a second `attach` request
arrives, the session refuses with `Err("session already attached")`.
Multi-attach is not in v1 (§7).

## 3. veter (the local renderer)

The local `veter` does not change in user-visible behaviour. It is
still a GUI process that:

- Owns the top-level vt100, PRT engine, VGE engine.
- Spawns its inner PTY for the user's shell (which is `vmux` by
  default; see `veter/src/pty.rs`).
- Parses PRT / VGE / VFT envelopes recursively per portal.
- Renders.

The only refactor that touches `veter` is an internal one: the host
engine modules (`veter/src/prt`, `veter/src/vge`, plus the vendored
`vt100`) become a new `veter-host` library crate that both `veter`
and `vsd` link against. No public-API change; no protocol
change. See §8.

## 4. The snapshot protocol (VSS)

On every `attach`, `vsd` has to bring a fresh renderer into the
session's exact current state. The chosen format is a **versioned
binary state dump** that the renderer decodes and writes directly
into its engine structs — no parsing of replayed commands, no side
effects re-firing.

### 4.0 Why not the replay serializer?

The v1 of this section described a *replay-style* serializer (emit
ordinary PRT / VGE envelopes that, processed by a fresh renderer,
reconstruct state). Commits `ee89c77` / `1f8d874` / `858472e` landed
that design and it works for the common case, but it structurally
leaks state:

- vt100: alternate-screen buffer, DECSC saved cursor, scrolling
  region (DECSTBM), origin mode, current G0/G1 charset, saved
  attributes — all dropped.
- VGE: only the currently-active screen's element set is emitted.
- PRT: per-portal VFT engines, engine-level focus and cursor style,
  per-portal `PolledStateCache`, suspended-screen portals.

Closing each gap in the replay world means inventing the right
command sequence to emit, in the right order, with no side effects
re-firing on the receiver — fiddly per-field work, every gap costs
its own week. A direct binary dump captures every field once and
sidesteps the side-effect problem entirely (no parsing on the
receive path, no event callbacks to silence).

### 4.1 Wire format

A new APC-framed extension, **VSS** (Veter State Snapshot), parallel
to PRT / VGE / VFT and routed by the same per-portal pipeline:

- Engine → renderer envelope: `ESC _ V S S <payload> ESC \`
- Renderer → engine envelope: `ESC _ v s s <payload> ESC \`
- Payload framing matches PRT §1.1–1.4: `u8 protocol_version = 0`,
  `u32 payload_length`, then a sequence of
  `(u8 frame_type, u32 request_id, u32 body_length, body[body_length])`
  frames. ESC byte-stuffing applied to the payload.
- Primitive encoding (`u8`, `u16`, `u32`, `i32`, `varu`, `string`,
  `bytes`) per PRT §1.4.

#### Frame types (engine → renderer, marker `VSS`)

| Code | Frame | Body |
|---|---|---|
| 0x01 | `SnapshotBegin` | `u32 snapshot_version`, `u16 rows`, `u16 cols`, `u32 sequence_id` |
| 0x02 | `VtFragment`    | `varu index`, `varu total`, `bytes payload` |
| 0x03 | `VgeFragment`   | `varu index`, `varu total`, `bytes payload` |
| 0x04 | `PrtFragment`   | `varu index`, `varu total`, `bytes payload` |
| 0x05 | `SnapshotEnd`   | `u32 sequence_id` |

#### Frame types (renderer → engine, marker `vss`)

| Code | Frame | Body |
|---|---|---|
| 0x01 | `SnapshotAccepted` | `u32 sequence_id` |
| 0x02 | `SnapshotRejected` | `u32 sequence_id`, `u8 reason` (1 = version, 2 = malformed, 3 = capacity) |

Fragmenting lets a multi-megabyte snapshot (images!) span several
envelopes without busting any single APC budget. Reassembly is by
`(frame_type, total)` count; a complete `Vt+Vge+Prt` set within one
`Begin … End` window applies atomically.

### 4.2 Version policy

`snapshot_version` is a single monotonic `u32` baked into both
binaries at build time. Bump on every breaking change to any
sub-snapshot layout. **Strict match.** On mismatch the renderer
emits `SnapshotRejected { reason = 1 }`; `vsd` writes a plain-text
banner to the renderer's alt-screen view, holds for ~2 s, and tears
the attach down via the existing `ATTACH_LEAVE`. No replay fallback.
The operational expectation is that `vsd` and `veter` ship in
lockstep.

### 4.3 Snapshot payload

Three independent sub-snapshots, each carrying its own
`u16 kind_version` so subsystems can rev independently of the
envelope version. Exact field lists live in the implementation;
the doc-level inventory:

**VtFragment** — vt100 state. The full `vt100::Screen`: visible grid,
**alternate grid**, cursor and **saved cursor (DECSC)**, **scroll
region (DECSTBM)** + saved scroll region, **origin mode** + saved
origin mode, current and **saved SGR attributes**, current and
**saved G0/G1 charset**, all input modes (application keypad / cursor,
hide cursor, bracketed paste, alt-screen flag, mouse protocol mode
+ encoding), title, icon name, the scrollback ring with current
scrollback offset. Lives in the vt100 fork as
`Screen::binary_snapshot()` / `restore_from_binary_snapshot()`.
Bolded items are gaps the v1 replay serializer drops.

**VgeFragment** — VGE state. `shared.{styles, images}` (images keep
`source_encoding` + `source_data` — WebP stays WebP, never
re-encoded), **both `main` and `alt` element sets** (the v1 replay
shipped only the active one), the `on_alt` flag, and engine-level
scalars (`cell_px`, `scale_factor`). GPU image handles are *not* on
the wire; the renderer re-creates them lazily through the existing
`Renderer::register_gpu_image` path on first paint. Lives in
`veter-host` as `VgeEngine::binary_snapshot()` /
`restore_from_binary_snapshot()`.

**PrtFragment** — PRT state, recursive. `main` and **`alt`** portal
sets, `on_alt`, **engine-level `FocusKind` and `CursorStyle`**, and
per portal: id, geometry, anchor, visibility, draw order, creation
sequence, scrollback length, plus three nested binary blobs — that
portal's vt100 snapshot, its `children` PrtEngine snapshot
(recursive), and its VgeEngine snapshot — plus its
**`PolledStateCache`** and **`pending_cursor_queries`** counter.
VFT engines are deliberately *not* serialized: in-flight transfers
are abandoned on reattach (same policy as the v1 replay). Lives in
`veter-host` as `PrtEngine::binary_snapshot()` /
`restore_from_binary_snapshot()`.

### 4.4 Engine-side composition (vsd)

`tools/vsd/src/attach.rs::handler_main` replaces the v1 replay
composition (lines ~237–245). Under the engines lock:

1. Grab `vt`, `vge`, `prt` binary snapshots.
2. Wrap them into
   `SnapshotBegin → VtFragment* → VgeFragment* → PrtFragment* → SnapshotEnd`
   envelopes via `vss-protocol::encode_snapshot`.
3. Write `ATTACH_ENTER` (`CSI ?1049 h`) + envelopes to the
   renderer's stdout.
4. Read upstream for `SnapshotAccepted` / `SnapshotRejected` with a
   ~1 s timeout (matches the existing `PROBE_TIMEOUT`).
5. On accept: install the renderer-stdout fd on the engines so the
   worker forwards live PTY bytes; release the lock; splice input
   and run the winsize watcher exactly as today.
6. On reject: print a plain-text mismatch banner to the alt-screen
   view; `ATTACH_LEAVE`; tear the attach down.

The per-session worker thread (`tools/vsd/src/engines.rs`) is
**unchanged**: it keeps forwarding inner-PTY bytes verbatim once
the snapshot is acknowledged. Pass-through after attach is exactly
what it is today, modulo the new VSS marker that PRT / VGE / VFT
forward verbatim per §1.1 of each spec.

### 4.5 Renderer-side application

The renderer's byte pipeline
(`veter/src/main.rs::App::process_pty_output`,
`PRT → VGE → VFT → vt100`) and the equivalent per-portal pipeline in
`veter-host/src/prt/state.rs::WritePortal` gain a fourth stage:
`VssEngine`, sitting between VFT and vt100.

`VssEngine` (new module, `veter-host/src/vss/state.rs`) is an APC
parser and fragment reassembler with the same shape as `VftEngine`.
On a complete snapshot (`SnapshotBegin` … `SnapshotEnd` matched and
validated):

1. Check `snapshot_version` against the compile-time constant.
   Mismatch → emit `SnapshotRejected { reason = 1 }`, drop the
   payload.
2. Decode the three sub-snapshots and call
   `restore_from_binary_snapshot()` on the **owning context's**
   engines — host-level vt100 / PRT / VGE for the top-level
   `VssEngine`, `portal.{vt, children, vge}` for a per-portal
   `VssEngine`.
3. Emit `SnapshotAccepted { sequence_id }` upstream via the
   engine's `pending_response_bytes` queue (the same path PRT and
   VGE already use for upstream responses).

Restore is **side-effect-free by construction**: binary fields are
assigned directly, so engine callbacks
(`HostCallbacks::on_title_change`, bell, mouse-mode-change, …) are
not invoked. A "post-restore reconcile" pass on the renderer pushes
title / cursor / mouse-mode to the GUI by reading the new field
values directly, without going through the callback path.

### 4.6 Bandwidth

A 200 × 80 grid with 4 K scrollback lines, binary cell encoding
(content + attrs + width, no run-coalescing) lands in the 300–600
KiB range, comparable to the replay-with-coalescing estimate. Images
dominate, exactly as before: WebP `source_data` is reshipped on
every attach. The content-hash image cache previously listed as a
deferred optimization remains a v1.1 lever — its design is
unchanged by the VSS switch.

## 5. SSH and the wire

The session manager is intentionally **SSH-first** and uses no other
transport:

- The local user's `ssh user@hostX` runs inside a vmux pane on
  local `veter`. The pane is a PRT portal owned by local `veter`.
- The SSH client transports stdio bytes verbatim between local
  `veter` (via vmux) and the remote login shell.
- On the remote, `vsd attach foo` hijacks the SSH PTY's stdio
  and starts emitting PRT / VGE envelopes upward through SSH.
- Local `veter`'s portal parser sees those envelopes as if they
  came from any other inner program (recursive portal hosting).

VFT (file transfer, `vsend` / `vrecv`) running *inside* a vsd
session must reach the *local* host, not be consumed by `vsd`.
The portal-recursive parsing already implements
"pass-through markers I don't own" for PRT and VGE; the same rule
applies to `vsd` for VFT. `vsd`'s host engines must declare
which extensions they consume and forward every other extension's
APC envelopes verbatim. This is a per-host-implementation rule,
not a protocol change.

No port forwarding, no SSH config changes, no agent integration
required.

## 6. Detach ergonomics

The detach trigger is owned by `vsd`, not by local `vmux` or
local `veter`. The reason is that detach is a session-control event
and only `vsd` can correctly act on it (cleanly close stdio,
keep the inner PTY).

For v1, the trigger is a configurable byte sequence on `vsd`'s
input stream — something rare enough that the user is unlikely to
press it accidentally, distinct from vmux's prefix
(default `Ctrl+Space`). A reasonable default is `Ctrl+\` followed
by `d` (analogous to tmux's `prefix-d`, but with a different
prefix so it never collides with a local-vmux prefix that may
itself be running inside the session). `vsd` reads this off
its input stream before forwarding the rest to the inner PTY.

A protocol-level detach command (so local `vmux` can offer a
"prefix-D" that talks directly to `vsd`) is defined by the
companion document `doc/session-extension.md` (SES) and is
implemented — `vmux`'s `prefix-D` sends a SES `Detach`, which the
session process turns into this same teardown via its
`attach_shutdown` self-pipe.

## 7. Limitations and non-goals for v1

- **No shared sessions.** Exactly one renderer attached at a time
  per session. A second attach kicks the first. Multi-renderer
  (mirroring, observer mode) is a future direction.
- **No state persistence across `vsd` restarts.** Sessions are
  in-memory only. Daemon dying = sessions gone.
- **No cross-host session migration.** A session is bound to the
  machine its `vsd` runs on.
- **No automatic auth.** The Unix socket is mode-0700 in
  `$XDG_RUNTIME_DIR`; only the same UID can attach. Cross-user
  session sharing is out.
- **No GUI for `vsd`.** The daemon is headless and exposes
  itself only through the CLI and the upstream SSH PTY.
- ~~**No vmux integration.**~~ *Landed.* vmux now learns its
  session name (shown as a tab-bar segment) and offers `prefix-D`
  detach, via the SES control channel — `doc/session-extension.md`.
  Was a v1 non-goal; the "second shoe" has dropped.

## 8. Staging

### 8.1 v1 (replay-style attach) — landed

Tracked in `CONTEXT.md`. Summarised:

1. Extract `veter-host` crate (mechanical refactor).
2. Pass-through rule for unknown extensions (PRT / VGE / VFT spec
   §1.1 contract).
3. vt100 redraw serializer (`Screen::full_contents_formatted`).
4. PRT / VGE state serializers (`PrtEngine::serialize_state`,
   `VgeEngine::serialize_state`).
5. `vsd` skeleton (`new` / `list` / `kill` / `kill-server`).
6. `attach` with replay snapshot over `SCM_RIGHTS` stdio handover.
7. Detach hotkey (`Ctrl+\ d`).
8. Packaging (cross-compile to aarch64-musl, included in
   `dist-aarch64-deb`).

These are end-to-end smoke-tested. The replay path is the current
production code path on the `vsd` branch.

### 8.2 v2 (VSS binary snapshot) — next track

The replay-style serializers stay in tree while VSS lands so the
two can be A/B-tested. The `vsd` switchover (step 7 below) is
the cutover moment; step 8 garbage-collects the replay path.

1. **`vss-protocol` crate.** Wire-format only (envelope codec, frame
   types, primitive helpers), mirroring `prt-protocol` /
   `vge-protocol` / `vft-protocol`. Inline roundtrip tests.
2. **vt100 binary snapshot.** `Screen::binary_snapshot()` /
   `restore_from_binary_snapshot()` in the vt100 fork, covering all
   the fields the replay serializer drops (alt-screen, DECSC, scroll
   region, charset, …). Round-trip test asserts encode → decode →
   encode is byte-equal across alt-screen-active, DECSC-saved,
   scroll-region-set, charset-shifted, scrollback-populated cases.
3. **VGE binary snapshot.** `VgeEngine::binary_snapshot()` /
   `restore_from_binary_snapshot()` covering main + alt element
   sets, image table (with WebP `source_data` preserved), style
   table, element ordering. Round-trip + GPU-image-id reattachment
   test.
4. **PRT binary snapshot.** `PrtEngine::binary_snapshot()` /
   `restore_from_binary_snapshot()`, recursive over the portal
   tree. Round-trip test with a two-level nested portal carrying
   VGE elements.
5. **`VssEngine` in `veter-host`.** APC parser, fragment
   reassembler, restore dispatcher. Per-portal field on `Portal`,
   plus the host-level engine. Inline tests for fragment reordering,
   malformed frames, version mismatch.
6. **Renderer pipeline wire-up.** Add the VSS stage to
   `veter/src/main.rs::App::process_pty_output` and to the
   per-portal pipeline in `veter-host/src/prt/state.rs::WritePortal`.
   Manual end-to-end test by hand-rolling a VSS envelope inside a
   vmux pane.
7. **`vsd` switches to VSS.** `attach.rs` composition rewritten;
   accept / reject upstream wired. End-to-end smoke (see §4.4 and
   §4.5). Replay serializers still in tree but unused.
8. **Remove replay serializers.** Delete
   `Screen::full_contents_formatted`, `VgeEngine::serialize_state`,
   `PrtEngine::serialize_state`, and any helpers
   (`Row::write_contents_inline`, `Grid::scrollback_rows`) that no
   longer have callers. Update `CONTEXT.md`.

Steps 1–4 are pure additive work — they don't change runtime
behaviour and can land independently and out of order. Step 5
introduces the `VssEngine` type but is inert without step 6 wiring.
Step 7 is the user-visible switchover; step 8 is cleanup.

### 8.3 v3 (per-session process) — landed

The v1 daemon-of-many-sessions design (a single `vsd` process
holding a `HashMap<String, Session>`) was replaced with one process
per session. CLI shape changed:

```text
vsd new [-a] NAME [argv ...]
vsd attach NAME
vsd list
vsd kill NAME
```

`start`, `kill-server`, and `--foreground` (daemon mode) were
removed. Hidden internal flags `--session NAME [argv...]` and
`--foreground-session NAME [argv...]` are what `new` re-execs into
(detached vs. foreground). §2 documents the new model.

What got simpler:

- No central `HashMap<String, Session>`, no daemon-wide accept loop,
  no auto-spawn-the-daemon-on-first-CLI-call.
- `Arc<Mutex<EngineState>>` still exists, but its scope is now one
  process. Crash in one session can't take down others.
- `vsd list` is a CLI-side directory scan + per-session `Status`
  round-trip; stale sockets from crashed sessions auto-unlink on
  probe.

What got swapped in:

- A small `runtime.rs` module for the per-session socket-path
  helpers, runtime-dir setup, and stale-socket probe/unlink.
- `session.rs` absorbed the accept loop (was `daemon.rs`); each
  session process binds `<NAME>.sock` and accepts `Attach` / `Kill`
  / `Status`.
- IPC trimmed: `New` / `List` / `KillServer` gone; `Status` added.

## 9. Open questions

Resolved by the VSS design:

- ~~**Where does the vt100 serializer live?**~~ Fork. The binary
  snapshot needs every private field; a public-API serializer was
  never going to work.

Still open:

- **`vsd new` versus auto-create on attach.** Should
  `vsd attach foo` create `foo` if it doesn't exist (tmux-
  style) or require an explicit `new`? tmux-style is friendlier;
  explicit-`new` makes typos visible. Probably tmux-style with a
  `--no-create` flag for scripts.
- **Inner-PTY exit policy.** When the session's foreground
  process exits, does the session end immediately, or hang
  around as a "dead" session the user can still attach to (to
  read final scrollback)? tmux ends it; the alternative has
  ergonomic value but complicates the lifecycle. Default to
  ending, with `vsd new --linger` as an opt-in later.
- **Image table digest probe.** Mentioned in §4.6 as a deferred
  optimisation. The minimum viable version is a one-frame VGE
  capability bit ("renderer remembers image digests across
  attaches") plus a probe response field for the current digest
  set. Defer until profiling justifies it.

VSS-specific:

- **`SnapshotAccepted` timeout.** Tentative 1 s, matching the
  existing `PROBE_TIMEOUT` (see
  `tools/vsd/src/attach.rs`). Revisit if WAN attaches feel
  sluggish.
- **VFT in-flight transfer survival.** Current decision: abandon
  on reattach (matches v1). A small "live-transfer state"
  snapshot is a v2 design idea; not blocking VSS.
- **Reply suppression on the renderer side during an attach.**
  The renderer's per-portal engines see forwarded inner-program
  bytes and may generate CPR / DSR / title responses that
  `vsd` has already answered locally. Correctness is fine —
  `vsd` discards the stray PRT responses — but it's wasted
  upstream bandwidth. Defer to v1.1.
- **Multi-attach.** Out of scope for v1. The
  `session.attached` flag stays as "exactly one renderer at a
  time."
- **`engine_build_id` alongside `snapshot_version`.** Cheap to
  add (a 7-byte git short hash in `SnapshotBegin`) and catches
  the silent failure mode of "someone bumped a field but forgot
  to bump the version constant." Recommend adding in step 8.1
  of §8.2.

## 10. Companion documents

- `doc/portal-extension.md` — PRT spec. Unchanged.
- `doc/vector-graphics-extension.md` — VGE spec. Unchanged.
- `doc/file-transfer-extension.md` — VFT spec. Unchanged.
- `doc/session-extension.md` — SES. *Implemented.* The
  client-to-host control channel (vmux ↔ vsd): a session-name
  probe and a detach command. Note that the related per-portal
  *activity* signal lives in PRT (`PortalActivity`, portal
  extension §8.10), not in SES — activity is per-portal and must
  also work under a local `veter` host with no session.
