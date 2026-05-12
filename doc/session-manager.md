# Veter Session Manager (veterd)

> **Status: design sketch — WIP.** This document captures the
> architecture for persistent veter sessions. Nothing here is
> implemented yet. The companion `doc/session-extension.md` (vmux ↔
> veterd integration protocol) is deferred and will be written
> separately.

`veterd` is a persistent host-side session manager. Its role is to
hold the state of a veter session (vt100 grids, scrollback, VGE
element tables, image tables, PRT portal trees, inner PTYs) across
disconnections of the rendering client. The motivating use case is
SSH: a user works inside a tab whose contents are owned by a `veterd`
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
            ▲                │ veterd attach │              ▲               │ veterd attach │
            │                │ cool          │              │               │ another       │
            │                └───────┬───────┘              │               └───────┬───────┘
            │                        │ SCM_RIGHTS           │                       │
            │                        ▼                      │                       ▼
            │                ┌───────────────┐              │               ┌───────────────┐
            │                │ veterd daemon │              │               │ veterd daemon │
            │                │ ─ host vt100  │              │               │ ─ host vt100  │
            │                │ ─ PRT engine  │              │               │ ─ VGE engine  │
            │                │ ─ inner PTY   │              │               │ ─ inner PTY   │
            │                │   (bash, …)   │              │               │   (bash, …)   │
            │                └───────────────┘              │               └───────────────┘
            │                                               │
            └──────────── PRT / VGE / VFT byte stream ──────┘
                          (rides the existing SSH PTY;
                           each veterd emits envelopes
                           inside its tab's portal)
```

### 1.2 Roles

| Component | Where it runs | What it owns |
|---|---|---|
| `veter` | local, GUI | winit/glutin window, paint loop, host engines for *local* state (top-level vt100, PRT engine, VGE engine). Same as today. |
| `vmux` | local, inside `veter` | Tabs and panes. Same as today. Each pane is a PRT portal in local `veter`. |
| ssh client | local, inside a vmux pane | Unmodified. Transports stdio bytes to the remote host. |
| `veterd attach <name>` | remote, inside the SSH PTY | Thin CLI. Hands its stdio over to the long-lived daemon and exits. |
| `veterd` daemon | remote, background | Persistent. Owns one or more sessions. Each session = (inner PTY, host engines, accumulated state). |
| inner program | remote, inside a session | The user's shell, vim, htop, vmux-on-remote, anything. |

The whole chain between local `veter` and a remote `veterd` is the
plain SSH stdin/stdout pair. **No TCP socket, no port forwarding, no
custom transport.** The bytes on the wire are exactly the existing
PRT / VGE / VFT envelopes. Local `veter`'s portal parser already
treats every portal as a recursive host, so a remote `veterd`
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
persistent boundary to *the host process itself* (`veterd`) means:

- Engines live in exactly one place.
- The renderer (local `veter`) stays as-is — it has no notion of
  session continuity, just paints whatever envelopes arrive.
- `vmux` stays as a single process and is unchanged.

The cost is a snapshot serializer inside the host engines (§4); it
is the only genuinely new piece of protocol-adjacent code in the
plan.

## 2. veterd

### 2.1 Process model

`veterd` is a single long-lived background process per remote user.
On first invocation it daemonizes itself (double-fork, detach from
the controlling terminal) and creates a Unix socket at
`$XDG_RUNTIME_DIR/veterd/sock` (mode `0700`). All subsequent CLI
calls (`veterd new`, `veterd attach`, …) connect to that socket.

Each *session* inside the daemon owns:

- An inner PTY pair, with a child process (default `$SHELL`, or
  whatever was passed to `veterd new`) running on its slave side.
- Host engine instances:
  - vt100 (the vendored `vt100` fork) for the top-level grid.
  - PRT engine (recursive — children for any nested portals the
    inner program spawns).
  - VGE engine (element table, style table, image table).
- Session metadata: name, creation time, current row × column
  dimensions, last-attached-by, last-attached-at.

Sessions are identified by string name (≤ 64 bytes, the same rules
as element IDs in §6.8 of the portal spec). Names are globally
unique within one `veterd` instance.

State is in-memory only. `veterd` exiting (intentionally or
otherwise) drops every session.

### 2.2 Daemon lifecycle

- Started lazily by the first `veterd new` / `veterd attach` call
  if no daemon is running.
- Stays alive as long as at least one session exists. The last
  session ending (its inner PTY exits) starts a short grace timer
  (default 30 s) before the daemon exits, so the user can quickly
  `veterd new` again without a respawn race.
- `veterd kill-server` shuts everything down immediately.

### 2.3 CLI surface

```
veterd new <name> [cmd ...]      # create a session, default cmd = $SHELL,
                                 # then attach the current stdio to it
veterd attach <name>             # resume a named session (alias: `restore`)
veterd list                      # one line per session: name, age, w×h, attached?
veterd detach                    # called from inside an attached session via
                                 # a hotkey (configurable; see §6)
veterd kill <name>               # terminate a specific session
veterd kill-server               # stop the daemon and every session
```

`new` and `attach`/`restore` are the only commands that take over the
caller's stdio. The others are short-lived control RPCs.

### 2.4 Stdio handover

When `veterd attach foo` is invoked, the CLI:

1. Connects to the daemon socket.
2. Sends `Attach { name: "foo" }`.
3. Passes its stdin and stdout fds to the daemon via `SCM_RIGHTS`
   over the same socket.
4. Exits.

The daemon now owns those fds directly. From that point on, the
SSH PTY is glued to `veterd` (not to the original CLI process),
which means:

- No long-lived `veterd-attach` middleman process is consuming a
  PTY slot.
- Detaching is a matter of the daemon closing its end of the fds
  and going silent; ssh-side stdio falls back to whatever shell
  the user was in before the attach.

If a session is already attached when a second `attach` request
arrives, the daemon kicks the prior attach (closes its fds with a
warning event written first) and accepts the new one. Multi-attach
is not in v1 (§7).

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
and `veterd` link against. No public-API change; no protocol
change. See §8.

## 4. The snapshot serializer

On every `attach`, `veterd` has to re-paint the session for a fresh
renderer that knows nothing about it. Because the renderer parses
PRT / VGE / VFT envelopes natively, the serializer's job is to
walk the engines' internal state and emit **a stream of ordinary
PRT / VGE envelopes** that, applied to a fresh host, reconstructs
the same state.

Replay is then *just normal command processing on the receiving
side*. No new wire format, no decoder asymmetry.

### 4.1 What the stream contains

In order, per session:

1. **Image table.** For every uploaded image, one `UploadImage` (or
   `UploadImageBegin` + chunks, if the image is large) per entry,
   so any subsequent `DrawImage` references resolve.
2. **Global style table.** One `SetGlobalStyle` per registered
   style, so any `StyleRef` in element commands resolves.
3. **Top-level VGE elements.** Walk the element tree in
   `(draw_order, creation_order)` and emit `CreateElement` for
   each. Children are emitted after their parents so `parent_id`
   resolves at processing time. `is_visible` and `draw_order` go
   in the create body.
4. **PRT portals.** For each portal (recursively):
   - `CreatePortal` with the portal's recorded geometry.
   - A stream of `WritePortal` envelopes carrying *vt100 escape
     sequences* that bring a fresh portal vt100 to the daemon's
     current grid + scrollback state. (§4.2)
   - Per-portal VGE elements (same shape as step 3, but scoped to
     this portal — using the existing portal-recursive VGE state
     model).
   - Recursively, any nested portals.

### 4.2 The vt100 redraw stream

This is the only genuinely new bit of engineering. The serializer
walks each vt100's grid + scrollback and emits:

- For each line of scrollback (oldest first): SGR setup
  + cursor positioning + the line's cells + `CR LF` to push it
  into history.
- For the visible screen: SGR setup + cursor positioning + cells
  + the final cursor position, scrolling region, alt-screen flag,
  saved cursor (`DECSC`/`DECRC` state), character set state.

Coalesce identical-attribute runs into a single SGR + text span so
the byte cost stays close to "grid area × bytes per visible char".

This serializer lives next to the vt100 it reads (likely as a new
method on the vendored `vt100::Screen`), so it has access to the
private grid representation without exposing it. **Replay flicker**
during attach is unavoidable but bounded: the renderer paints
intermediate states as the stream is consumed. Mitigations (e.g.
"render only the final state per attach") are a v2 optimization.

### 4.3 Bandwidth

A 200 × 80 grid with 4 K scrollback lines and tight attribute
coalescing comes out to roughly 200–600 KiB. Images push that up
fast — a single 1 MiB WebP avatar re-shipped on every attach is the
biggest variable cost.

Mitigations (deferred):

- Image table digest. `veterd` keeps the SHA-256 of every uploaded
  image; the renderer sends its current set of known image
  digests on attach via a small new probe extension, and `veterd`
  skips re-uploading matches. Saves bandwidth on tight reattach
  cycles where the renderer is reused.
- Delta snapshots. `veterd` keeps a serialized hash of its last
  shipped state and emits only diffs since then. Substantially
  more complex; only worth it if profiling shows it's needed.

v1 ships the full state on every attach. The simpler model first.

## 5. SSH and the wire

The session manager is intentionally **SSH-first** and uses no other
transport:

- The local user's `ssh user@hostX` runs inside a vmux pane on
  local `veter`. The pane is a PRT portal owned by local `veter`.
- The SSH client transports stdio bytes verbatim between local
  `veter` (via vmux) and the remote login shell.
- On the remote, `veterd attach foo` hijacks the SSH PTY's stdio
  and starts emitting PRT / VGE envelopes upward through SSH.
- Local `veter`'s portal parser sees those envelopes as if they
  came from any other inner program (recursive portal hosting).

VFT (file transfer, `vsend` / `vrecv`) running *inside* a veterd
session must reach the *local* host, not be consumed by `veterd`.
The portal-recursive parsing already implements
"pass-through markers I don't own" for PRT and VGE; the same rule
applies to `veterd` for VFT. `veterd`'s host engines must declare
which extensions they consume and forward every other extension's
APC envelopes verbatim. This is a per-host-implementation rule,
not a protocol change.

No port forwarding, no SSH config changes, no agent integration
required.

## 6. Detach ergonomics

The detach trigger is owned by `veterd`, not by local `vmux` or
local `veter`. The reason is that detach is a session-control event
and only `veterd` can correctly act on it (cleanly close stdio,
keep the inner PTY).

For v1, the trigger is a configurable byte sequence on `veterd`'s
input stream — something rare enough that the user is unlikely to
press it accidentally, distinct from vmux's prefix
(default `Ctrl+Space`). A reasonable default is `Ctrl+\` followed
by `d` (analogous to tmux's `prefix-d`, but with a different
prefix so it never collides with a local-vmux prefix that may
itself be running inside the session). `veterd` reads this off
its input stream before forwarding the rest to the inner PTY.

A protocol-level detach command (so local `vmux` could offer a
"prefix-D" that talks directly to `veterd`) is the subject of the
deferred companion document, `doc/session-extension.md`.

## 7. Limitations and non-goals for v1

- **No shared sessions.** Exactly one renderer attached at a time
  per session. A second attach kicks the first. Multi-renderer
  (mirroring, observer mode) is a future direction.
- **No state persistence across `veterd` restarts.** Sessions are
  in-memory only. Daemon dying = sessions gone.
- **No cross-host session migration.** A session is bound to the
  machine its `veterd` runs on.
- **No automatic auth.** The Unix socket is mode-0700 in
  `$XDG_RUNTIME_DIR`; only the same UID can attach. Cross-user
  session sharing is out.
- **No GUI for `veterd`.** The daemon is headless and exposes
  itself only through the CLI and the upstream SSH PTY.
- **No vmux integration.** vmux does not yet know it is talking to
  a `veterd` and offers no "prefix-D" or session-name title
  decoration. That integration rides on the deferred session
  extension protocol; see §10.

## 8. Staging

A staged landing, smallest reviewable steps first:

1. **Extract `veter-host` crate.** Move `veter/src/prt`,
   `veter/src/vge`, and re-export the vendored `vt100` through a
   single host-engine façade. `veter` keeps existing behaviour;
   no protocol change. Mechanical refactor.
2. **Pass-through rule for unknown extensions.** Audit the PRT
   and VGE host engines so a daemon that consumes one extension
   but not another forwards the foreign envelopes verbatim
   (already true for PRT/VGE crossing each other; needs to be
   spelled out as a contract).
3. **vt100 redraw serializer.** Add `Screen::serialize_state() ->
   Vec<u8>` to the vendored vt100 fork; test by round-tripping
   through a fresh `Screen`.
4. **PRT / VGE state serializers.** In `veter-host`, walk the
   engines' tables and emit equivalent `CreateElement` /
   `CreatePortal` / `UploadImage` / etc. streams; round-trip
   tested against a fresh engine pair.
5. **`veterd` binary skeleton.** Daemon process, socket, session
   table, `new` / `list` / `kill` / `kill-server`. No `attach`
   yet; sessions can be created but the only way to observe them
   is `list`.
6. **`attach` with snapshot replay.** Hook the serializers up to
   the attach path, including `SCM_RIGHTS` stdio handover.
   First end-to-end use.
7. **Detach hotkey.** Configurable, default `Ctrl+\ d`.
8. **Packaging.** Cross-compile `veterd` to static aarch64-musl
   alongside the existing client tools (`vmux`, `vcat`, `vsend`,
   `vrecv`) and add it to the `dist-aarch64-deb` target.

Steps 1–4 are pure refactor + serializer work — they don't change
any user-facing behaviour and can land independently. Step 5
starts being externally observable. Step 6 is the first user-
useful state.

## 9. Open questions

- **Where does the vt100 serializer live?** Inside the vendored
  `vt100` fork (cleanest abstraction, but fork grows) or in
  `veter-host` using `vt100`'s public read API (no fork change,
  but probably less efficient). Leaning toward the fork — the
  serializer wants the private grid representation.
- **`veterd new` versus auto-create on attach.** Should
  `veterd attach foo` create `foo` if it doesn't exist (tmux-
  style) or require an explicit `new`? tmux-style is friendlier;
  explicit-`new` makes typos visible. Probably tmux-style with a
  `--no-create` flag for scripts.
- **Inner-PTY exit policy.** When the session's foreground
  process exits, does the session end immediately, or hang
  around as a "dead" session the user can still attach to (to
  read final scrollback)? tmux ends it; the alternative has
  ergonomic value but complicates the lifecycle. Default to
  ending, with `veterd new --linger` as an opt-in later.
- **Image table digest probe.** Mentioned in §4.3 as a deferred
  optimization. The minimum viable version is a one-frame VGE
  capability bit ("renderer remembers image digests across
  attaches") plus a probe response field for the current digest
  set. Defer until profiling justifies it.

## 10. Companion documents

- `doc/portal-extension.md` — PRT spec. Unchanged.
- `doc/vector-graphics-extension.md` — VGE spec. Unchanged.
- `doc/file-transfer-extension.md` — VFT spec. Unchanged.
- `doc/session-extension.md` — *deferred.* Will define the
  client-to-host control channel (vmux ↔ veterd: detach command,
  session-name display, status events). v1 of the session
  manager works without it; vmux integration is the second
  shoe to drop.
