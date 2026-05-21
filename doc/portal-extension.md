# Portal Extension (PRT)

> **Status: unstable WIP — v0.** The wire format may change in
> incompatible ways without notice. Clients and host implementations
> ship from this repo in lockstep. The version byte in every envelope
> is `0` and the probe response advertises `protocol_version = 0`;
> both bump to `1` once the format is declared stable.

This extension lets a client carve out rectangular regions of the
terminal grid — *portals* — and pipe arbitrary terminal byte streams
into them. The host parses those streams with its full VT100 stack and
renders the result inside the portal's cells, exactly as if each
portal were the top-level terminal.

Portals exist to enable terminal multiplexers (tmux, screen, zellij,
floating panes, picture-in-picture log views) without an in-band
escape-sequence translation layer. Anything a real terminal can do
inside the host — colours, alt screen, mouse modes, OSC 52 paste,
sixel — works inside a portal because it is a real (sub-)terminal.

The protocol is binary, command-batched, and bidirectional. Commands
flow client → host. Host → client carries two things on one wire:
**responses** (one per command) and **events** (unsolicited, e.g. an
inner program writing a DSR reply that must be relayed to that
program's stdin).

This extension is self-contained: a host can implement it without
implementing any other terminal extension. If the host *also*
implements the Vector Graphics Extension, §10 spells out how the
two interact.

## 1. Wire format

### 1.1 Envelope

Every protocol message — both directions — rides inside an APC
sequence:

```
client → host:   ESC _ P R T <payload> ESC \
host   → client: ESC _ p r t <payload> ESC \
```

- `0x1B 0x5F` (`ESC _`) opens APC.
- The 3-byte marker `PRT` (uppercase) means *command from client to
  host*. The marker `prt` (lowercase) means *response or event from
  host to client*.
- `0x1B 0x5C` (`ESC \`) closes APC.

The case-difference between the two markers lets either side parse
without a direction flag, and the 3-byte marker as a whole lets a
host-side APC parser route PRT envelopes to this extension while
passing other APC sequences (iTerm-style `ESC _G`, other
extensions) through to whatever else handles them.

A host that implements PRT MUST forward APC envelopes whose marker
is not `PRT`/`prt` verbatim to its downstream layer. This pass-through
rule is what lets a stack of nested hosts — for example a remote
`veterd` consuming PRT + VGE while a `vsend` running inside its
session emits VFT bytes that must reach the local user's terminal —
layer cleanly without each level having to understand every
extension. See `doc/session-manager.md` for the driving use case.

### 1.2 Payload framing

The payload is one binary blob with byte stuffing applied (§1.3) before
it goes into the envelope, and unstuffed after extraction.

The unstuffed payload begins with:

```
u8   protocol_version       // 0 (this document — unstable WIP)
u32  payload_length         // little-endian, length of the rest, in bytes
```

After that header the payload is a tightly packed sequence of one or
more *frames*. A frame is:

```
u8   frame_type             // command code (§3), response code (§4),
                            //   or event code (§4)
u32  request_id             // little-endian; client-assigned, opaque to host
u32  body_length            // little-endian
u8   body[body_length]      // frame_type-specific body
```

Multiple frames may share one envelope. The host MUST process command
frames in order and emit one response frame per command frame, in the
same order, in one or more response envelopes. Event frames may be
interleaved with response frames in any order; the only guarantee is
that events generated *while* processing a command appear after that
command's response.

`request_id` is opaque to the host and echoed verbatim in the
matching response. For event frames (host-originated, unsolicited)
`request_id` MUST be `0` and the client MUST ignore it.

### 1.3 ESC byte stuffing

All bytes of the payload (after computing `payload_length`, before
placing in the envelope) are scanned. Any byte equal to `0x1B` is
replaced with the two-byte sequence `0x1B 0x1B`. Decoding reverses
this. This is the only escape rule.

`payload_length` is computed on the *unstuffed* payload, so the
receiver knows how much data to expect after unstuffing.

The same stuffing applies to the byte streams carried as `bytes`
fields *inside* PRT frames (§7.1, §7.2): they are part of the payload
like any other field, so they get stuffed once at envelope-encode
time. There is no double-stuffing.

### 1.4 Encoding primitives

| Type     | Encoding                                            |
|----------|-----------------------------------------------------|
| `u8`     | 1 byte                                              |
| `u16`    | 2 bytes, little-endian                              |
| `u32`    | 4 bytes, little-endian                              |
| `i32`    | 4 bytes, little-endian, two's complement            |
| `varu`   | LEB128 unsigned varint                              |
| `string` | `varu length` followed by `length` UTF-8 bytes      |
| `bytes`  | `varu length` followed by `length` raw bytes        |

Strings are not NUL-terminated. Empty strings encode as a single
`0x00`.

All coordinate and size fields in this spec are integer cells; the
extension does not use floating-point types and does not address
sub-cell positions (§5.1).

## 2. Probe and capability discovery

### 2.1 Probe (frame_type 0x01)

Sent by the client first thing after enabling the extension. Body is
empty.

Host responds with `ProbeResponse` (§4):

```
u16  protocol_version          // highest version the host speaks
u32  max_portals               // soft cap; over-limit creates fail
u32  max_portal_cells_w
u32  max_portal_cells_h        // per-portal grid caps, in cells
u32  max_scrollback_lines      // per-portal scrollback ring cap
u32  max_write_bytes           // largest single WritePortal body
u8   features                  // bitmask, see below
u8   max_nesting_depth         // sub-portal tree depth (§5.5);
                               // 0 means nested portals unsupported
```

`features` bitmask:

```
bit 0  alt_screen_in_portal    // honours DECSET 1047/1049 inside
bit 1  emit_title_events       // OSC 0/2 → TitleChange event
bit 2  emit_icon_events        // OSC 1   → IconNameChange event
bit 3  emit_cwd_events         // OSC 7   → WorkingDirChange event
bit 4  emit_clipboard_events   // OSC 52  → ClipboardOp event
bit 5  emit_bell_events        // BEL     → Bell event
bit 6  emit_mouse_mode_events  // DECSET 9/1000/1002/1003/1005/1006/1015
                               //         → MouseModeChange event
bit 7  emit_activity_events    // meaningful scroll → PortalActivity event
```

Hosts that also implement other terminal extensions (e.g. vector
graphics) MAY advertise additional capability bits in extra trailing
fields after `max_nesting_depth`; see §10 for the bits this
implementation defines.

If the host does not support the extension, no response is emitted;
the client SHOULD time out (e.g. 250 ms) and fall back to non-portal
operation.

A client MUST NOT send any other PRT command before receiving the
probe response. If a higher protocol version exists in future, the
host returns its highest known version and the client picks
`min(client, host)`.

The body length is the source of truth for which fields are present.
A client reading a shorter body MUST treat missing trailing fields as
zero. A host emitting a longer body than this client knows about MUST
be tolerated by skipping unknown trailing bytes.

## 3. Commands (client → host)

| Code | Command            | Body section |
|------|--------------------|--------------|
| 0x01 | Probe              | §2           |
| 0x02 | CreatePortal       | §6.1         |
| 0x03 | DeletePortal       | §6.2         |
| 0x04 | UpdateSize         | §6.3         |
| 0x05 | UpdateOrigin       | §6.4         |
| 0x06 | UpdateVisibility   | §6.5         |
| 0x07 | UpdateDrawOrder    | §6.6         |
| 0x08 | ClearAll           | §6.7         |
| 0x09 | WritePortal        | §7.1         |
| 0x0A | SetFocus           | §9.1         |
| 0x0B | SetCursorStyle     | §9.2         |
| 0x0C | SetPortalScrollback| §9.3         |

All other frame_type values in the command range (0x01..0x7F) are
reserved and MUST be rejected with `err_unknown_command`. Frame types
in 0x80..0xFF are events (§4) and MUST NOT appear in client → host
envelopes; if they do, the host MUST reject with
`err_unknown_command`.

## 4. Frames (host → client)

Two kinds of host → client frames share one frame_type space.

### 4.1 Response frames (0x01..0x7F)

Every command produces exactly one response frame. The `request_id`
in the response equals the `request_id` of the originating command.

| Code | Response       | Body                                         |
|------|----------------|----------------------------------------------|
| 0x01 | Ok             | command-specific (typically empty)           |
| 0x02 | Err            | `u16 error_code, string message`             |
| 0x03 | ProbeResponse  | as in §2.1                                   |

`error_code` values:

| Code   | Name                    | Meaning                                          |
|--------|-------------------------|--------------------------------------------------|
| 0x0001 | err_unknown_command     | Unknown frame_type                               |
| 0x0002 | err_bad_payload         | Frame body could not be parsed                   |
| 0x0003 | err_unsupported_version | protocol_version too new                         |
| 0x0010 | err_unknown_portal      | portal ID does not resolve                       |
| 0x0011 | err_duplicate_id        | string ID already in use (CreatePortal)          |
| 0x0012 | err_too_many_portals    | Portal budget exhausted                          |
| 0x0013 | err_size_out_of_range   | Portal size exceeds advertised cap or is zero    |
| 0x0014 | err_write_too_large     | WritePortal body exceeds max_write_bytes         |
| 0x0040 | err_max_nesting_depth   | sub-portal would exceed advertised depth         |
| 0x00FF | err_internal            | Host-side failure                                |

After an `Err` response the host's state is unchanged: failed commands
are atomic, with no partial side effects. In particular, a
`WritePortal` that fails atomically does not consume any bytes — the
inner parser sees nothing.

### 4.2 Event frames (0x80..0xFF)

Events are unsolicited, host-originated frames. `request_id` is `0`
and ignored. Event ordering is preserved per portal; ordering across
portals is not guaranteed.

| Code | Event                    | Body section |
|------|--------------------------|--------------|
| 0x80 | RawReply                 | §7.2         |
| 0x81 | Bell                     | §8.1         |
| 0x82 | TitleChange              | §8.2         |
| 0x83 | IconNameChange           | §8.2         |
| 0x84 | WorkingDirChange         | §8.3         |
| 0x85 | ClipboardOp              | §8.4         |
| 0x86 | CursorVisibilityChange   | §8.5         |
| 0x87 | BufferModeChange         | §8.6         |
| 0x88 | PortalEvicted            | §8.7         |
| 0x89 | ResizeNotify             | §8.8         |
| 0x8A | MouseModeChange          | §8.9         |
| 0x8B | PortalActivity           | §8.10        |

Events for capabilities a client did not request via the `features`
bitmask in §2.1 MUST NOT be emitted (the host learns the client's
preferences through the bits set in the *probe* — at v1, the bits in
the response are advisory only; see §13).

Unknown event codes received by a client MUST be ignored without
error so the protocol can grow.

## 5. Coordinate system, screens, anchoring, resize, reset

### 5.1 Cell coordinates

All portal coordinates and sizes are **integer cell units** of the
host grid:

- `x` is measured in cells from the host's left edge.
- `y` is measured in cells from the top.
- Origin is top-left, +x right, +y down.

Origins, sizes, and any other position field are `i32` (origins) or
`u32` (sizes). The extension intentionally exposes no sub-cell
positioning, no fractional offsets, and no pixel-level coordinates.
A portal's bounds always fall on cell boundaries, and a renderer
that draws cells on an integer pixel grid never needs to draw a
portal anywhere else.

Inside a portal, the inner grid uses the host's cell metrics
unchanged: a portal cell is the same size and shape as a host cell.
Inner-portal text is glyph-for-glyph identical to host text in the
same cell area, just clipped to the portal's bounds.

Origins are signed (`i32`) so a portal whose top-left has scrolled
above the live region (Scrollback anchor mode, §5.2) or has been
moved partly off the host grid can still be addressed
unambiguously. The host clips at render time.

### 5.2 Anchoring modes

A portal has an **anchor mode**, set at creation:

- `Live` (default) — `origin.y` is interpreted relative to the top of
  the live screen at every render. As the user scrolls the host, the
  portal stays put on screen. This is the multiplexer feel: the
  portal is part of the live-region UI.
- `Scrollback` — `origin.y` is interpreted relative to the top of the
  live screen at command-processing time. The host converts that into
  an absolute scrollback line:

  ```
  anchor_line = top_of_live_screen + origin.y
  ```

  `anchor_line` is then permanent for that portal until `UpdateOrigin`
  is issued. As the screen scrolls, the portal travels with the line
  it is anchored to. Once `anchor_line` falls off the top of
  scrollback (evicted), the portal is silently destroyed and a
  `PortalEvicted` event (§8.7) is emitted. `UpdateOrigin` re-pins the
  portal using the same rule applied at the time of the update.

`origin.x` is plain horizontal cell offset in both modes; it does not
interact with scrolling.

The two modes are mutually exclusive and there is no mode-swap
command. A client wanting to convert a `Live` portal into a
`Scrollback` snapshot deletes and recreates.

### 5.3 Visibility versus the visible viewport

A portal with `is_visible = true` and anchor mode `Scrollback` is
still hidden if its `anchor_line` sits outside the user's visible
scrollback window. Rendering clipping is automatic and not exposed
as protocol state.

`Live` portals are always within the live region by definition; if
their bounds extend past the live region (e.g. a tall portal pushed
near the bottom), the rendered inner grid is clipped at the host's
screen edge. The portal's own size does NOT shrink — only its
rendering is masked.

### 5.4 Host alternate screen

When the host switches to its alternate screen (DECSET 1047 / 1049),
the current portal set is suspended and replaced with an empty set.
On return to the main screen the alt portal set is dropped and the
main set restored. Each host screen has its own portal table; they
do not share state.

A portal's *own* alt-screen state — set by the inner program writing
`ESC [ ? 1049 h` into that portal — is per-portal and orthogonal
to the host's. It is honoured if `features.alt_screen_in_portal` is
set (it always is in this implementation; the bit exists for future
profiles).

### 5.5 Sub-portals and nesting

Portals are nested by *recursion*, not by an explicit `parent_id`.
The inner program inside a portal speaks the protocol over its own
PTY exactly as a top-level client does, and the host's per-portal
parser handles its `PRT` envelopes. A portal created from within
another portal lives in the inner portal's element table and is
scoped to its lifetime.

`max_nesting_depth` (§2.1) caps the total tree depth. A `CreatePortal`
issued at depth `max_nesting_depth − 1` fails atomically with
`err_max_nesting_depth`. Depth `0` means the host does not support
nested portals; clients receiving PRT envelopes from inside an
unsupported-depth portal SHOULD see those bytes pass through to the
inner vt100 as APC-other (which the inner vt100 will discard).

There is no flat global namespace for portal IDs. IDs are scoped to
their containing scope (the host or a parent portal); a `WritePortal`
addressing portal `"foo"` always means *the `"foo"` directly in this
scope*.

### 5.6 Resize

When the **host** is resized, portal origins, sizes, and scrollback
anchors are not modified. Portals whose bounds now extend beyond the
host grid are clipped at render time. The client is responsible for
deciding whether to issue `UpdateOrigin` / `UpdateSize` calls in
response.

Resizing a **portal** via `UpdateSize` (§6.3) is plumbed through to
the portal's inner vt100 parser (rows = `size_h`, cols = `size_w`).
The host emits a `ResizeNotify` event (§8.8) once the inner grid has
been resized, so the client can decide when to deliver the
SIGWINCH-equivalent to the program owning the portal's PTY.

### 5.7 Reset

A full reset (RIS / `ESC c`) and soft reset (DECSTR / `ESC [ ! p`) on
the **host** clear the entire portal state of the host's current
screen: every portal, every sub-portal, every per-portal vt100. The
client must re-create portals afterwards.

The same sequences received *inside* a portal are scoped to that
portal: they reset the portal's own vt100 and destroy any sub-portals
of that portal, but do not touch sibling portals or the host.

### 5.8 Erase Display

`ESC [ 2 J` (erase visible screen) and `ESC [ 3 J` (xterm "Erase
Saved Lines" — erase scrollback) wipe the host's text grid in place.
The host doesn't push the cleared cells into scrollback, so portals
anchored to those rows would otherwise stay rendered on top of
now-blank text. The host therefore drops portals alongside:

- Host `ESC [ 2 J` drops every portal whose effective anchor lies in
  the live region. For `Live` portals, this is always; for
  `Scrollback` portals, it means `anchor_line >= top_of_live_screen`.
- Host `ESC [ 3 J` drops every `Scrollback` portal whose `anchor_line`
  is in the scrollback region (`anchor_line < top_of_live_screen`).
  `Live` portals are unaffected.

`clear(1)` (ncurses ≥ 6.0) emits `ESC [ H ESC [ 2 J ESC [ 3 J`, so
the two events together wipe every portal in the current host
screen.

Partial erases (`ESC [ J` / `ESC [ 0 J` / `ESC [ 1 J`) are
cursor-relative and do not trigger this cleanup.

`ESC [ 2 J` / `ESC [ 3 J` received *inside* a portal are scoped to
that portal — they wipe the portal's own grid and drop the portal's
own sub-portals, but do not touch host-level portals.

## 6. Portal lifecycle

### 6.1 CreatePortal (0x02)

Body:

```
string  id                 ; non-empty, ≤ 64 UTF-8 bytes; unique in this scope
u32     size_w             ; cells, ≥ 1
u32     size_h             ; cells, ≥ 1
i32     origin_x           ; cells from the host's left edge
i32     origin_y           ; cells; interpretation depends on anchor_mode
u8      anchor_mode        ; 0 = Live, 1 = Scrollback
u8      is_visible         ; 0 or 1
i32     draw_order
u8      flags              ; reserved; must be 0 (rejected with err_bad_payload)
u32     scrollback_lines   ; requested scrollback ring size; clamped at
                           ;   max_scrollback_lines
```

Behavior:

- `id` MUST be non-empty (anonymous portals are not supported — there
  is no use case where a client doesn't want to address its own portal
  later). Empty `id` → `err_bad_payload`.
- Duplicate `id` in the same scope → `err_duplicate_id`. Replace =
  explicit `DeletePortal` then `CreatePortal`.
- `size_w` or `size_h` of 0 → `err_size_out_of_range`. Sizes above the
  advertised caps → same error.
- For `Live` portals, `origin_y` is the cell offset from the top of
  the live region; rendering re-evaluates this every frame.
- For `Scrollback` portals, the host derives `anchor_line` per §5.2
  and pins the portal to that line.
- `draw_order`: ties broken by creation order among portals (later =
  on top).

The portal starts with an empty inner grid (a fresh vt100 instance)
and its own scrollback ring of
`min(scrollback_lines, max_scrollback_lines)` rows.

Response: empty Ok.

A client MAY pipeline `CreatePortal` and a follow-up `WritePortal` in
one envelope without waiting for the create's response, since IDs are
client-picked.

### 6.2 DeletePortal (0x03)

Body: `string id`. Response: empty Ok.

Unknown ID → `err_unknown_portal`. Deletion is recursive: any
sub-portals (and their sub-portals…) are torn down with their parent.
The PTY-side concerns of the inner program are *not* the host's
problem — the client owns those FDs and is responsible for hangup.

### 6.3 UpdateSize (0x04)

Body:

```
string  id
u32     new_w              ; cells, ≥ 1
u32     new_h              ; cells, ≥ 1
```

Resizes the portal. The inner vt100 is reconfigured (`set_size(rows,
cols)`); existing scrollback lines longer than the new width follow
the inner vt100's reflow rules (this implementation does not reflow —
lines are clipped or padded).

Sizes outside `[1, max_portal_cells_*]` → `err_size_out_of_range`.
Unknown ID → `err_unknown_portal`.

Response: empty Ok. A `ResizeNotify` event (§8.8) follows once the
inner grid is materially resized.

### 6.4 UpdateOrigin (0x05)

Body:

```
string id
i32    new_origin_x
i32    new_origin_y
u8     anchor_mode        ; 0 = Live, 1 = Scrollback (must match portal's
                          ;   current mode; mismatch → err_bad_payload)
```

Re-positions the portal. For `Scrollback` portals, the host re-pins
using the same rule applied at create time; for `Live`, it just
overwrites the live-relative origin.

`anchor_mode` is repeated in the body so that the client and host
remain in agreement on the portal's mode without the client needing
to remember it. Mode-swap is not allowed here — the only way to
change mode is delete + recreate.

### 6.5 UpdateVisibility (0x06) / 6.6 UpdateDrawOrder (0x07)

```
UpdateVisibility:  string id, u8 is_visible
UpdateDrawOrder:   string id, i32 draw_order
```

Self-explanatory. Hiding a portal does *not* pause its inner parsing —
bytes still flow through `WritePortal` and update inner state; only
the visual rendering is suppressed. This matters for log-tail panes
that should keep tailing while collapsed.

If a client wants to actually pause an inner program, it should
withhold writes (or signal the program directly via its PTY).

### 6.7 ClearAll (0x08)

Body: empty. Removes every portal from the host's *current* screen
buffer. Same scoping behaviour as RIS but without resetting the
host's text grid. Useful for "shutdown" by a client without issuing
a full terminal reset.

### 6.8 Portal IDs

A portal ID:

- Is at most 64 bytes of UTF-8.
- MUST be non-empty in every command that references one (anonymous
  portals are not supported). An empty ID is `err_bad_payload`.
- Is opaque to the host beyond byte equality.
- Lives in a per-scope namespace: each containing scope (the host or
  a parent portal) has its own table, and an ID resolves only within
  the scope of the command that uses it (§5.5).

There is no rename command. Reusing an ID requires `DeletePortal`
followed by `CreatePortal`.

## 7. Portal byte stream

### 7.1 WritePortal (0x09)

Body:

```
string id
bytes  data           ; raw bytes destined for the portal's inner vt100
```

The host appends `data` to the portal's input queue and parses
synchronously: a single `WritePortal` call drains entirely into the
inner parser before the response is generated. There is no per-portal
input buffer; flow control is end-to-end (the client should not
out-pace the host; if it does, the response latency degrades but
nothing is lost).

The inner parser is *fully recursive*: nested PRT envelopes, OSC
sequences, alt-screen toggles, mouse modes, sixel — and any other
extension the host implements at top level — are honoured inside
the portal too. Any side effects emit events (§8) on the *host* PRT
channel, tagged with the portal ID, so the client sees them without
having to re-parse the byte stream.

If `data.len()` exceeds `max_write_bytes` → `err_write_too_large` and
no bytes are processed. The client MUST split such writes into
smaller chunks. (In this implementation, `max_write_bytes` defaults to
1 MiB; a single OSC 52 paste is the only common payload near that
size, and it is always splittable on UTF-8 boundaries.)

If `id` is unknown → `err_unknown_portal`, atomic.

Response: empty Ok.

### 7.2 RawReply event (0x80)

Body:

```
string id
bytes  data
```

`data` is the bytes the portal's inner vt100 would have written back
to its TTY in response to whatever the inner program asked for: DSR
replies, DA / DA2 / DA3 replies, OSC 52 paste replies, DECSCUSR
queries, mouse-mode confirmations, etc.

The client MUST forward `data` to the inner program's stdin (typically
its PTY master) verbatim. This is the only mechanism by which inner
programs can complete their query/reply protocols.

`data` is delivered in the order the inner vt100 produced it.
Multiple `RawReply` events for one portal preserve relative ordering.
The host MAY coalesce multiple inner-vt100 writes into a single
`RawReply` event but MUST NOT reorder bytes.

### 7.3 Buffering and flow control

Host → client backpressure is plain TCP-style: the host writes events
and responses to the PTY master, and `write(2)` blocks if the OS-side
buffer is full. There is no application-layer flow control on the PRT
channel.

Client → host: the client decides its own pacing. Excessive write
rates manifest as host CPU saturation (parsing) but do not lose data.

A client that wants to throttle inner output can do so before calling
`WritePortal` — e.g. by reading less from the inner PTY when a portal
is hidden. The host has no portal-level rate-limit knob in v1.

## 8. Portal events (host → client)

All events carry a `string id` first, identifying the source portal.
Event bodies are described below.

### 8.1 Bell (0x81)

```
string id
```

The portal's inner vt100 saw a `BEL` (`0x07`) outside an OSC. The
client decides what to do (window-flash, audible bell, badge, ignore).
Suppressed if `features.emit_bell_events` is not advertised.

### 8.2 TitleChange (0x82) / IconNameChange (0x83)

```
string id
string title       ; UTF-8; OSC payload after stripping the OSC envelope
```

OSC 0 fires both `TitleChange` and `IconNameChange` (with the same
payload). OSC 1 fires `IconNameChange` only. OSC 2 fires
`TitleChange` only.

The host applies no length limit beyond the host's own input parser
limits. The client is responsible for clamping if it puts the title
in a tab UI.

Suppressed if the relevant feature bit is not advertised.

### 8.3 WorkingDirChange (0x84)

```
string id
string uri         ; OSC 7 payload, typically file://host/path
```

The OSC 7 payload is forwarded verbatim. The host does not parse the
URI; that is the client's job.

### 8.4 ClipboardOp (0x85)

```
string id
u8     selection       ; ASCII byte: 'c' clipboard, 'p' primary, 's' selection,
                       ;   etc. — see xterm's OSC 52 doc
u8     op              ; 0 = set (base64-decoded into `data`),
                       ; 1 = query  (data is empty; client will issue a paste
                       ;             reply via WritePortal which the host
                       ;             relays via RawReply)
bytes  data            ; for op = set: the decoded clipboard content
```

The client implements the actual clipboard policy. `op = 1` (query)
is just a notification that the inner program *requested* the
clipboard; the client decides whether and how to respond by feeding
the OSC 52 reply bytes via `WritePortal`. The reply then surfaces as
a `RawReply` to the inner program — same path any other reverse-
channel byte takes.

### 8.5 CursorVisibilityChange (0x86)

```
string id
u8     visible       ; 0 or 1
```

Fired when the portal's inner vt100 toggles DECTCEM (`ESC [ ? 25 h/l`).
The client uses this together with focus state (§9.1) to decide
whether to render the cursor at all.

### 8.6 BufferModeChange (0x87)

```
string id
u8     on_alt        ; 0 = main screen, 1 = alt screen
```

Fired when the portal's inner vt100 enters or leaves alt-screen mode
(DECSET 1047 / 1049). Useful for clients that want to swap UI chrome
based on whether a full-screen TUI is running inside a portal.

### 8.7 PortalEvicted (0x88)

```
string id
u8     reason         ; 0 = scrollback eviction
                      ; 1 = host erase-display / erase-scrollback (§5.8)
                      ; 2 = host alt-screen swap-out (§5.4)
```

Notifies the client that a portal it created is gone for reasons
other than its own `DeletePortal`. The portal's ID is now free for
re-use.

### 8.8 ResizeNotify (0x89)

```
string id
u32    rows
u32    cols
```

Confirms an `UpdateSize` was applied to the inner grid. The client
SHOULD use this as the trigger to deliver SIGWINCH (or its
equivalent) to the inner program, ensuring the program always sees
the same `(rows, cols)` the host now expects.

### 8.9 MouseModeChange (0x8A)

```
string id
u8     protocol       ; 0 off, 1 X10 (DECSET 9), 2 normal (1000),
                      ; 3 button (1002), 4 any-event (1003)
u8     encoding       ; 0 default (legacy), 1 UTF-8 (1005),
                      ; 2 SGR (1006), 3 urxvt (1015)
u8     focus_events   ; 0 off, 1 on (DECSET 1004)
```

Fired whenever the portal's inner vt100 changes one of the DEC mouse
modes. The host coalesces back-to-back changes into a single event
carrying the current resolved state.

This event exists because mouse-mode tracking is exactly the kind of
parser-state work the extension is meant to spare clients from. It
matters most for **nested multiplexers** (§13.5): the parent
multiplexer needs to know "does any of my descendants currently want
mouse events?" so it can enable the matching mode on its own input
source. Without this event, the parent would have to maintain its
own mouse-mode parser on every child's display stream.

A multi-pane client coalesces across panes itself: it tracks the
union of `protocol != 0` across all of its panes and writes the
appropriate DECSET sequence to its own input source whenever the
union changes.

### 8.10 PortalActivity (0x8B)

```
string id
```

Fired when the portal produced **meaningful output** — a tmux-style
"activity in a background window" signal. A multiplexer cannot
compute this itself: it forwards raw bytes and has no vt100 to tell a
log line scrolling past from a spinner redrawing one cell in place.
The host runs the portal's vt100 and so can.

The heuristic: the event fires when, during a `WritePortal`, the
portal's *main* (non-alternate) grid committed at least one new line
— either its content scrolled, or the cursor advanced to a lower row
(output filling a screen that is not yet full). In-place updates
(spinners, progress bars, clocks) rewrite a line with a carriage
return, leaving the cursor row unchanged; full-screen TUIs run on the
alternate screen. Neither triggers the event. It is **edge-triggered**:
at most one per `WritePortal` regardless of how many lines scrolled;
the client keeps its own sticky per-portal flag and clears it when
the user next views that portal.

The host SHOULD suppress the event for the currently focused portal
(per `SetFocus`, §9.1) — the client is already looking at it, and
this also cuts event volume for the pane most output lands in.
Correctness does not depend on this; a client that receives activity
for a portal it considers "in view" simply ignores it.

The heuristic is host-internal and MAY be refined (e.g. a damage
burst rule) without a protocol change — the wire contract is only
"PortalActivity fired".

Gated by `emit_activity_events` (features bit 7, §2.1).

## 9. Focus and cursor rendering

### 9.1 SetFocus (0x0A)

Body:

```
u8     mode             ; 0 = host, 1 = portal
string id               ; portal ID, only present if mode == 1
```

Tells the host where keyboard focus *currently* sits. The host uses
this for two rendering decisions only:

- The host's own text-grid cursor renders only when `mode = 0`.
- The targeted portal's inner cursor renders as "focused" (typically
  blinking, solid block); unfocused portals render their cursors
  hollow (or hidden, per `SetCursorStyle`).

`SetFocus` does **not** affect input routing. Input never crosses
the PRT wire: the client receives keyboard/mouse via the host's
normal VT100 input reporting on the host's own PTY, and writes the
encoded bytes directly to the **inner program's PTY master FD** —
the same FD it reads display bytes from. The kernel hands those
bytes to the inner program through the slave end. `WritePortal` is
the *display* direction only; using it for input would feed
keystrokes into the host's parser, where they have no effect on
the inner program.

Errors:

- `mode = 1` with empty or unknown `id` → `err_unknown_portal`.

Response: empty Ok.

### 9.2 SetCursorStyle (0x0B)

Body:

```
u8     unfocused_style       ; 0 = hidden, 1 = hollow, 2 = dim
```

Configures how portals that don't currently have focus render their
cursors. Default is `1` (hollow). Per-portal override is not
supported in v1; this is a host-wide policy.

Response: empty Ok.

### 9.3 SetPortalScrollback (0x0C)

Body:

```
string id
u32    lines       ; offset, in rows, from the top of the live screen
                   ; into scrollback. 0 = live region (no offset).
```

Drives the portal's vt100 scrollback offset. While `lines > 0` the host
renders that portion of the portal's history instead of the live
region; new bytes still flow into the inner vt100 normally and accrue
in scrollback. The value is silently capped at the portal's current
history depth — the response carries the post-clamp offset so the
client can show the actual scroll position even when its request
exceeded the available history.

`lines = 0` returns the portal to live view. Clients SHOULD send this
when the user exits scroll/copy mode.

Errors:

- Unknown ID → `err_unknown_portal`.

Response: Ok with body

```
u32 applied_lines  ; the offset actually in effect after clamping
u32 history_depth  ; rows currently held in the portal's scrollback ring
                   ; (grows with inner-program output, capped at
                   ;  scrollback_lines from CreatePortal)
```

The body length is the source of truth, as in §2.1 — clients reading a
shorter body MUST treat missing trailing fields as zero, so a host that
only echoes `applied_lines` is still spec-compliant.

Multiplexer-style clients typically issue `SetPortalScrollback` while
in a "copy mode" UI driven by arrow keys / PgUp / PgDn / vim-style
`j`/`k`. The applied-offset echo lets the indicator stop incrementing
once the user reaches the top of the captured history;
`history_depth` lets the client draw a scrollbar thumb sized to the
actual history available.

## 10. Integration with VGE

This section is **optional**: a host that does not implement the
Vector Graphics Extension can ignore it entirely, and a client that
does not care about VGE can ignore the events and bits described
here. PRT is fully functional without VGE.

When both extensions are present, hosts SHOULD advertise the
following extra capability bit in a trailing byte of the probe
response (§2.1):

```
bit 0  vge_in_portal           // host runs a per-portal VGE engine
```

Clients that read a probe response shorter than the field offset
treat the bit as 0 (VGE-in-portal not supported).

**VGE inside a portal.** When `vge_in_portal` is set, every portal
owns a private VGE engine that operates in the portal's cell
coordinate space:

- `VGE` envelopes inside the portal byte stream are extracted by
  the per-portal APC parser and routed to the per-portal VGE
  engine, exactly as the host does at top level.
- VGE responses generated inside a portal join that portal's
  `RawReply` stream — they look like ordinary bytes to the inner
  program.
- Coordinates are in *portal* cells, so the inner program does not
  need to know its position on the host screen.
- All VGE rules (anchoring, scrollback eviction, erase-display
  cleanup, alt-screen swap, parenting, image table) apply per-portal.
- A program running inside a portal cannot address a different
  portal's VGE state. Cross-portal addressing is deferred.

**Layering with host-level VGE.** Host VGE elements and host
portals share the same `i32 draw_order` namespace. The composite
rendering order at the host level is:

1. Host text grid (always at the bottom).
2. For each top-level visible item — host VGE element OR host
   portal — in `(draw_order, creation_seq)` ascending:
   - If a VGE element: render it.
   - If a portal: render its text grid, then its per-portal VGE
     elements (if any), then recurse into its sub-portals using
     their per-scope draw orders.

A clip rectangle equal to the portal's bounds is pushed before
rendering the portal's contents and popped after, so portal contents
never bleed outside the portal.

Portals are **not** VGE elements: they cannot be the `parent_id` of
a VGE element, and a VGE element cannot be the parent of a portal.
Cross-extension parenting is deferred (§14).

## 11. Mouse and selection

The host does not forward mouse events to portals automatically. The
client TUI receives mouse events via the host's VT100 mouse reporting,
hit-tests against its own model of where each portal sits, translates
the mouse coordinates into the portal's cell space, encodes the
appropriate VT100 mouse sequence (per the portal's currently active
mouse encoding — see `MouseModeChange`, §8.9), and writes those bytes
to the **inner program's PTY master FD**. As with keyboard input,
mouse bytes never cross the PRT wire — they go through the same
PTY chain the inner program is already attached to.

This split keeps host-side state minimal, lets the client own all
interaction policy (focus follows mouse vs. click-to-focus, scroll
wheel routes to portal vs. host scrollback, selection model), and
makes mouse mode mismatches between portal and host trivially
diagnosable client-side.

The client also has to enable mouse reporting on **its own** input
source (the host's PTY, or — for a nested multiplexer — its parent
portal's PTY). It does so by writing the appropriate DECSET sequence
upstream whenever the union of its descendants' mouse modes
changes. `MouseModeChange` events make that union cheap to track.

Selection of inner-portal text is similarly client-side. The client
hit-tests against its own portal layout and renders any selection
chrome it wants on its side. Copying selected inner-portal text is
the client's job — the host has no API for "give me the cells in
this rect of portal X" in v1.

## 12. Limits and budgeting

The host advertises hard caps via the probe response. Over-limit ops
fail atomically. A non-exhaustive list:

- `max_portals`: per host screen buffer (main and alt are independent).
- `max_portal_cells_w` / `max_portal_cells_h`: per portal grid.
- `max_scrollback_lines`: per portal scrollback ring.
- `max_write_bytes`: per `WritePortal` body (clients chunk bigger
  payloads).
- `max_nesting_depth`: portal tree depth.

The reference implementation in this repo SHOULD start with: 64
top-level portals, 1024×512 cells max per portal, 100 000 scrollback
lines, 1 MiB per `WritePortal`, 8 levels of portal nesting. These
numbers can be tuned without breaking the protocol.

Memory cost is dominated by per-portal scrollback rings, so clients
that allocate many portals should request a smaller
`scrollback_lines` per portal at create time.

## 13. Cookbook

### 13.1 tmux-style multiplexer with two side-by-side panes

```
host cell grid: 200 cols × 60 rows.
client wants: two equal panes, 100 cols × 60 rows each.

CreatePortal id="left",  size=(100,60), origin=(0,0),   anchor=Live
CreatePortal id="right", size=(100,60), origin=(100,0), anchor=Live
```

For each pane the client opens a normal PTY for a shell, then loops.
The display direction crosses the PRT wire; the input direction does
not.

Display direction (host renders pane contents):

- Read from inner-shell PTY master → `WritePortal` into the matching
  portal.

Input direction (kernel routes keystrokes to the right shell):

- Keyboard input from the host's VT100 input reporting → write
  directly to whichever inner-shell PTY master corresponds to the
  pane that has client-side focus.
- `SetFocus` is sent in parallel so the host renders the right
  cursor — but it does not move bytes.

Reverse channel and notifications:

- Any `RawReply` event → write `data` to the matching inner-shell
  PTY master, just like keyboard input. From the shell's stdin
  perspective, "user keystrokes" and "DSR replies my own queries
  triggered" are the same byte stream.
- Any `TitleChange` event → update the client's tab/status UI.
- Any `MouseModeChange` event → update the client's per-pane mouse
  state; if the union over panes changed, write the matching DECSET
  sequence to the host's own input source so the host starts (or
  stops) reporting mouse events to this client.

To **swap** panes, the client emits two `UpdateOrigin` calls in one
envelope. To **resize** (e.g. drag a divider), `UpdateSize` on each
pane and forward `ResizeNotify` to the inner PTYs as SIGWINCH.

### 13.2 Inline-scrollback log preview

A long-running build wants a 20-row × 80-col preview embedded in the
host scrollback at the moment a unit-test fails. The client creates
the portal in `Scrollback` mode at that scrollback row, then writes a
captured replay of the test output via `WritePortal`. The portal
travels with that scrollback line and survives as long as the
scrollback line does. When it scrolls off the top, a `PortalEvicted`
event fires.

```
CreatePortal id="test-#123", size=(80,20),
             origin=(0, current_live_row), anchor=Scrollback,
             scrollback_lines=0
WritePortal  id="test-#123", data=<replay bytes>
```

### 13.3 Picture-in-picture log tail with a header strip

A 40-cell-wide log tail in the bottom-right, with a 1-row header
strip above it that the user can click to close the tail.

```
CreatePortal id="tail-hdr",  size=(40,1),  origin=(160,49),
             anchor=Live, draw_order=10
CreatePortal id="tail-body", size=(40,10), origin=(160,50),
             anchor=Live, draw_order=10
WritePortal  id="tail-hdr",  data="\x1b[7m   tail of build.log    [x]\x1b[0m"
```

Both panes are portals; the header is just a one-row pane the
client renders with reverse video. When the user clicks the `[x]`
cell, the client hit-tests against its own model and issues
`DeletePortal id="tail-hdr"` and `DeletePortal id="tail-body"`.

### 13.4 Routing inner DSR queries

The shell inside `id="left"` emits `\x1b[6n` (cursor-position query).
The portal's inner vt100 captures it, computes the inner cursor
position, and writes `\x1b[<row>;<col>R` back. The host packages those
4–8 bytes into `RawReply { id="left", data=<bytes> }`. The client
forwards the bytes to the left-pane PTY's master FD. The shell sees
its reply and proceeds.

The client never has to know which sequences need replies; it just
treats `RawReply` as opaque relay traffic.

### 13.5 Nested multiplexers

A nested setup looks like `host → outer-mux (M2) → inner-mux (M3)
→ leaf shell`. M2 runs against the host's PTY and creates portal
`A` on the host. Inside `A`, M3 runs against `A`'s PTY (which M2
owns) and creates portal `X` scoped under `A`. The leaf shell runs
inside `X`, with its PTY owned by M3.

Three independent kernel PTYs in series, plus three layers of the
PRT scope tree. The portal extension does not need any new
operations to handle this — both directions follow naturally from
the single-level model.

**Display path** (leaf → host):

```
leaf writes to its slave
  → M3 reads from leaf's PTY master
  → M3 writes those bytes to its own stdout (which is A's PTY slave)
  → M2 reads from A's PTY master
  → M2 calls WritePortal(A, bytes) on the host
  → host's per-portal-A APC parser pulls out any nested PRT envelopes
    M3 wrote (e.g. WritePortal(X, ...)) and applies them in A's
    sub-portal scope
  → host's vt100 for portal A/X parses + renders the leaf's bytes
```

PRT envelopes M3 emits are just bytes inside its own stdout. M2
forwards them blindly via `WritePortal(A, ...)`; the host's
per-portal APC parser at scope `A` peels them off and routes them
to A's children. M2 doesn't need to know they exist.

**Input path** (key → leaf):

```
key pressed in host window
  → host emits VT100 input bytes on the host's PTY (M2's stdin)
  → M2 reads, decides focus is on portal A
  → M2 writes the bytes to A's PTY master (M3's stdin)
  → M3 reads, decides focus is on its sub-pane X
  → M3 writes the bytes to X's PTY master (the leaf's stdin)
  → the leaf reads
```

No PRT op carries input. Each layer makes one routing decision on
its own input bytes and delegates to the kernel.

**Focus chain**:

- M2 sends `SetFocus mode=portal id="A"` to the host (over the host
  PTY).
- M3 sends `SetFocus mode=portal id="X"` — but to *its* host, which
  is portal A. The bytes ride M3's stdout → M2's `WritePortal(A,
  ...)` → host's per-portal-A parser → recorded as "A's focus is on
  X".
- At render time the host walks the chain top-down (host → A → X),
  finds X is the leaf, and draws X's cursor focused. A's own cursor
  (which is M3's TUI cursor) draws per `SetCursorStyle.unfocused_style`
  — usually hidden, since it is meaningless when the focus has been
  passed through.

**Mouse-mode cascade**:

The leaf writes `\x1b[?1000h` to enable mouse reporting. M3 forwards
the bytes via its stdout; the host's portal-X vt100 records the mode
and emits `MouseModeChange { id="X", protocol=2, ... }` upward
through `A`'s `RawReply` stream — i.e. the host writes those bytes
to A's PTY for M3 to read. M3 reads its own `MouseModeChange` event
(it is itself a PRT client of its host), updates the union over its
panes, and — if the union just flipped from off to on — writes
`\x1b[?1000h` to its own stdout. The host's portal-A vt100 records
the mode and emits `MouseModeChange { id="A", ... }` to M2. M2
unions across its own panes and, if needed, writes the DECSET
upstream to the host's PTY. The mode is now enabled at every level
that sits between the user's mouse and the leaf, and mouse bytes
generated at the host can flow down the input path described above.

The cascade requires no new wire ops — every layer is just an
ordinary PRT client that happens to also be an inner program of
the layer above.

## 14. Open issues / future work

These are intentionally deferred and are not part of v1:

- **Reparenting / moving a portal between scopes.** The
  parent/scope is fixed at create time. A `MovePortal` op would need
  to deal with re-keying every descendant portal.
- **Server-side mouse routing.** A `mouse_routing = auto` mode where
  the host hit-tests pointer events and forwards them to the portal
  underneath. Clients would still get the host events for non-portal
  cells. Not in v1 because it conflicts with focus-follows-mouse vs.
  click-to-focus policy choices the client should own.
- **Server-side selection / copy.** A `ReadCells(portal_id, rect)`
  op for clients that want to copy text rendered inside a portal
  without keeping a parallel client-side mirror.
- **Per-portal cursor style override.** The host cursor-style policy
  is global (§9.2). Useful but easy to add later without breaking the
  wire.
- **Compression on the wire.** Large `WritePortal` bodies (alt-screen
  redraws, sixel images) compress well, but the byte-stuffed APC
  envelope is already binary and compression adds CPU and complexity
  on both ends. Deferred until profiling justifies it.
- **PTY ownership inside the host.** Today the client owns inner
  PTYs and shuttles bytes. A future profile could let the host
  spawn a child process and feed its stdout straight into a portal,
  eliminating one round trip per chunk. Out of scope for v1 because
  it raises trust / sandboxing questions.
- **Feature negotiation.** v1 advertises `features` from the host
  but does not let the client *select* which events it wants. A
  future `Configure` command could let clients opt out of noisy
  events (e.g. cursor-visibility changes during animations).
- **Cross-extension parenting and addressing.** When the Vector
  Graphics Extension is also implemented (§10), today a portal
  cannot be a VGE parent and a VGE element cannot be a portal's
  parent. Lifting either restriction needs careful scoping rules
  around the image and style tables.
