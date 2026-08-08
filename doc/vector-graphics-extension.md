# Vector Graphics Extension (VGE)

> **Status: unstable WIP — v0.** The wire format may change in
> incompatible ways without notice. Clients and host implementations
> ship from this repo in lockstep. The version byte in every envelope
> is `0` and the probe response advertises `protocol_version = 0`;
> both bump to `1` once the format is declared stable.

This extension lets a TUI client draw vector and raster graphics inside the
terminal grid. The shape opcode set is inspired by
[TinyVG](https://tinyvg.tech/download/specification.txt) but the wire
format below is self-contained — no part of TinyVG is required to
implement this protocol. It is designed for local PTYs and 8-bit-clean
SSH sessions; tmux/screen-style multiplexers are out of scope.

The protocol is binary, command-batched, and carries no graphical event
stream — input handling stays with the client TUI, using the existing VT100
keyboard/mouse reporting. All graphical state lives in the terminal and is
manipulated by addressable commands.

## 1. Wire format

### 1.1 Envelope

Every protocol message — both directions — rides inside an APC sequence:

```
client → terminal:   ESC _ V G E <payload> ESC \
terminal → client:   ESC _ v g e <payload> ESC \
```

- `0x1B 0x5F` (`ESC _`) opens APC.
- The 3-byte marker `VGE` (uppercase) means *command from client to
  terminal*. The marker `vge` (lowercase) means *response from terminal to
  client*. The case difference lets either side parse without a direction
  flag.
- `0x1B 0x5C` (`ESC \`) closes APC.

A terminal that implements VGE MUST forward APC envelopes whose marker
is not `VGE`/`vge` (PRT, VFT, iTerm-style `ESC _ G …`, or anything
else) verbatim to its downstream layer. This pass-through rule is what
lets a stack of nested hosts — for example a remote `vsd`
consuming PRT + VGE while a `vsend` running inside its session emits
VFT bytes that must reach the local user's terminal — layer cleanly
without each level having to understand every extension. See
`doc/session-manager.md` for the driving use case.

### 1.2 Payload framing

The payload is a single binary blob with byte stuffing applied (§1.3) before
being placed in the envelope, and unstuffed after extraction.

The unstuffed payload begins with:

```
u8   protocol_version       // 0 (this document — unstable WIP)
u32  payload_length          // little-endian, length of the rest, in bytes
```

After that header, the payload is a tightly packed sequence of one or more
*frames*. A frame is:

```
u8   frame_type              // command code (§3) or response code (§4)
u32  request_id              // little-endian; client-assigned, opaque to terminal
u32  body_length             // little-endian
u8   body[body_length]       // frame_type-specific body
```

Multiple frames may share a single envelope. The terminal MUST process
frames in order, and emit one response frame per command frame, in the same
order, in one or more response envelopes.

`request_id` is opaque to the terminal. A client that does not need to
correlate responses MAY set it to 0 for every command. The terminal echoes
the value verbatim in the corresponding response.

One value is reserved: `request_id == 0xFFFFFFFF` (`REQ_ID_NO_RESPONSE`) is
a "state-push" sentinel. The terminal MUST apply the command's effect on
state but MUST NOT emit any response (including error responses) for it.
This is intended for stateful middlemen (e.g. a session manager that
replays a snapshot to a freshly attached renderer): without the
suppression, the renderer's acks would round-trip through the chain and
get re-interpreted as input by the inner program's tty. Clients that need
acknowledgement MUST use any other value.

### 1.3 Byte stuffing

All bytes of the payload (after computing `payload_length`, before placing
in the envelope) are scanned and the following are replaced:

| Payload byte    | On-wire encoding | Reason                         |
|-----------------|------------------|--------------------------------|
| `0x1B` ESC      | `0x1B 0x1B`      | escape introducer / ST framing |
| `0x7E` `~`      | `0x1B 0x54` (`ESC T`) | ssh escape character      |
| `0x11` DC1      | `0x1B 0x51` (`ESC Q`) | XON (flow control)        |
| `0x13` DC3      | `0x1B 0x53` (`ESC S`) | XOFF (flow control)       |
| `0x09` HT       | `0x1B 0x48` (`ESC H`) | TAB (expanded by `TABDLY=XTABS`) |
| `0x0A` NL       | `0x1B 0x4E` (`ESC N`) | LF (rewritten by `ONLCR`) |
| `0x0D` CR       | `0x1B 0x52` (`ESC R`) | CR (rewritten by `OCRNL` / `ONLRET` / `ONOCR`) |

Decoding reverses each case; any other byte after a body `0x1B` (other
than these marks or the `0x5C` ST close) is malformed. All other bytes
pass through.

A receiver MUST bound how much of a single envelope it buffers. Nothing
in the framing obliges a sender to ever emit the closing `ESC \`, so an
unbounded receiver can be made to allocate without limit by a malformed
or hostile stream. On exceeding its cap the receiver MUST discard the
partial body and resynchronise at the envelope's end — byte-stuffing
guarantees the only bare `ESC \` inside an envelope is its terminator,
so this resync is exact — and MUST NOT pass the partial body through to
the text parser.

This cap is a memory backstop, not a policy limit. It is distinct from,
and sits above, the advertised caps of §11: a body that merely exceeds
`max_image_bytes` gets a normal error response carrying its
`request_id`, whereas an over-cap envelope can only be dropped
silently, since by then there is no `request_id` left to answer with.

The `~` / XON / XOFF rules exist because a VGE envelope can be relayed to
an inner program through its **input** channel — e.g. a portal's
`RawReply` forwarded into an `ssh` client. Such relays interpret these
bytes instead of forwarding them: `~` is ssh's escape character (a `\n~.`
in the stream tears the session down) and DC1/DC3 are software flow
control. Stuffing them guarantees the on-wire envelope never contains a
literal `~` (so `~` can never follow a newline), DC1 or DC3, making the
stream safe to relay 8-bit-clean. The mark bytes (`T`/`Q`/`S`) are
themselves transport-clean and distinct from `0x1B`/`0x5C`.

The TAB / LF / CR rules exist because an envelope may also be written to
a tty whose **output post-processing** is on — the normal state of a
pane, and one an out-of-band client cannot change, since it does not own
that pane's termios. `OPOST` with `ONLCR` (the default) rewrites every
LF to CRLF; `OCRNL`, `ONLRET` and `ONOCR` rewrite CR; `TABDLY=XTABS`
expands TAB into spaces. Any of these silently corrupts an envelope, and
the corruption is invisible to the sender. Stuffing them makes the body
safe to write to a cooked tty. The mark bytes (`H`/`N`/`R`) are
themselves transport-clean and distinct from `0x1B`/`0x5C` and from the
other marks.

`payload_length` is computed on the *unstuffed* payload, so the receiver
knows how much data to expect after unstuffing.

### 1.4 Encoding primitives

Used throughout the rest of the spec.

| Type     | Encoding                                            |
|----------|-----------------------------------------------------|
| `u8`     | 1 byte                                              |
| `u16`    | 2 bytes, little-endian                              |
| `u32`    | 4 bytes, little-endian                              |
| `i32`    | 4 bytes, little-endian, two's complement            |
| `f32`    | 4 bytes, IEEE 754 little-endian                     |
| `varu`   | LEB128 unsigned varint                              |
| `point`  | `f32 x, f32 y` — see §5 for units                   |
| `rect`   | `f32 x, f32 y, f32 w, f32 h`                        |
| `string` | `varu length` followed by `length` UTF-8 bytes      |
| `bytes`  | `varu length` followed by `length` raw bytes        |
| `transform` | `f32 a, b, c, d, e, f` — affine matrix, see §9.11 |

Strings are not NUL-terminated. Empty strings encode as a single `0x00`.

## 2. Probe and capability discovery

### 2.1 Probe (frame_type 0x01)

Sent by the client first thing after enabling the extension. Body is empty.

Terminal responds with `ProbeResponse` (§4):

```
u16  protocol_version          // highest version the terminal speaks
u16  cell_pixel_width
u16  cell_pixel_height
f32  scale_factor              // device pixels per logical pixel (HiDPI)
u32  max_elements              // soft cap; over-limit creates fail
u32  max_commands_per_element
u32  max_text_bytes            // per DrawText / UpdateText
u32  max_image_bytes           // per UploadImage (full image, not chunk)
u32  max_images                // concurrent uploaded images; includes
                               // in-progress chunked uploads (§8.2)
u8   supported_image_encodings // bitmask: bit0=Raw, bit1=WebP
u8   max_nesting_depth         // parent-child nesting cap (§9); 0
                               // means parenting not supported
```

If the terminal does not support the extension, no response is emitted; the
client SHOULD time out (e.g. 250 ms) and fall back to text-only mode.

A client MUST NOT send any other command before receiving the probe
response. If a higher protocol version exists in future, the terminal
returns its highest known version and the client picks `min(client, term)`.

The body length is the source of truth for which fields are present. A
client reading a shorter body MUST treat missing trailing fields as
zero (e.g. `max_nesting_depth = 0`, meaning no parenting). A terminal
emitting a longer body than this client knows about MUST be tolerated
by skipping unknown trailing bytes.

## 3. Commands (client → terminal)

All commands' frame_type values are listed here. Bodies are described in
later sections.

| Code | Command            | Body section |
|------|--------------------|--------------|
| 0x01 | Probe              | §2           |
| 0x02 | SetGlobalStyle     | §7.3         |
| 0x03 | CreateElement      | §6.1         |
| 0x04 | DeleteElement      | §6.2         |
| 0x05 | UpdateCommands     | §6.3         |
| 0x06 | UpdateCommand      | §6.3         |
| 0x07 | UpdateText         | §6.4         |
| 0x08 | UpdateImage        | §6.5         |
| 0x09 | UpdateOrigin       | §6.6         |
| 0x0A | UpdateVisibility   | §6.6         |
| 0x0B | UpdateDrawOrder    | §6.6         |
| 0x0C | UploadImage        | §8.2         |
| 0x0D | DropImage          | §8.2         |
| 0x0F | UpdateSize         | §9.5         |
| 0x10 | UpdateTransform    | §9.12        |

`0x0E` is **retired**. It was `ClearAll`, whose job is now done by
`DeleteElement` with an empty prefix (§6.2, §6.7).

All other frame_type values are reserved and MUST be rejected with
`err_unknown_command`.

## 4. Responses (terminal → client)

Every command produces exactly one response frame. Response frame layout
matches §1.2 (frame_type, request_id, body_length, body).

| Code | Response       | Body                                                     |
|------|----------------|----------------------------------------------------------|
| 0x01 | Ok             | command-specific (often empty)                           |
| 0x02 | Err            | `u16 error_code, string message`                         |
| 0x03 | ProbeResponse  | as in §2.1                                               |
| 0x04 | ChunkAck       | `string image_id, u32 bytes_received` (§8.2)             |

`error_code` values:

| Code   | Name                    | Meaning                                          |
|--------|-------------------------|--------------------------------------------------|
| 0x0001 | err_unknown_command     | Unknown frame_type                               |
| 0x0002 | err_bad_payload         | Frame body could not be parsed                   |
| 0x0003 | err_unsupported_version | protocol_version too new                         |
| 0x0010 | err_unknown_element     | element ID does not resolve                      |
| 0x0011 | err_duplicate_id        | string ID already in use (CreateElement)         |
| 0x0012 | err_too_many_elements   | Element budget exhausted                         |
| 0x0013 | err_command_index       | UpdateCommand index out of range                 |
| 0x0014 | err_text_range          | UpdateText byte range invalid or non-UTF-8       |
| 0x0020 | err_unknown_style       | StyleRef does not resolve                        |
| 0x0021 | err_reserved_style_id   | SetGlobalStyle id in the host-owned `host.*` namespace (§7.3) |
| 0x0030 | err_unknown_image       | image ID does not resolve                        |
| 0x0031 | err_image_too_large     | Image exceeds max_image_bytes                    |
| 0x0032 | err_image_decode        | Image bytes failed to decode (e.g. bad WebP)     |
| 0x0033 | err_duplicate_image_id  | image ID already in use (UploadImage)            |
| 0x0034 | err_too_many_images     | Image budget exhausted                           |
| 0x0040 | err_max_nesting_depth   | parenting would exceed advertised cap            |
| 0x00FF | err_internal            | Terminal-side failure                            |

After an `Err` response, the terminal's state is unchanged: failed commands
are atomic, no partial side effects.

Ok-response bodies, where non-empty:

- All others: empty.

## 5. Coordinate system, units, scrollback, screens

### 5.1 Cell coordinates

All draw-command coordinates are in **cell units**:

- `x` is measured in cell *widths* from the left edge of the terminal grid.
- `y` is measured in cell *heights*.
- Origin is top-left, +x rightward, +y downward.
- `1.0` on each axis equals one cell on that axis. Because cells are
  generally not square (e.g. 9×20 px), the unit is anisotropic. A path that
  needs to be visually circular must compensate using cell pixel dimensions
  from the probe response.

Coordinates are `f32` and may carry sub-cell offsets. They are not snapped
to the cell grid by the terminal.

### 5.2 Element origins and scrollback anchoring

Scrollback anchoring applies only to **top-level** elements (those with
no parent — see §9.1). For child elements the origin is interpreted in
the parent's interior coordinates instead, and lifecycle is governed
by the parent.

Element origins are in cell units, but the `y` component is interpreted as
**viewport-relative at command-processing time**, where "viewport" means
the live screen — i.e. the bottom of any scrollback, regardless of where the
user has scrolled to. The terminal converts this to an absolute scrollback
line index at the moment the command is processed:

```
anchor_line = top_of_live_screen + floor(origin.y)
sub_row     = origin.y - floor(origin.y)
```

`anchor_line` is then permanent for that element until `UpdateOrigin` is
issued. As the screen scrolls, the element travels with the line it is
anchored to. Once `anchor_line` falls off the top of scrollback (evicted),
the element is silently destroyed and its ID becomes available for reuse.

`UpdateOrigin` re-pins the element using the same rule applied at the time
of the update.

Origin `x` is plain horizontal cell offset; it does not interact with
scrolling.

**"At command-processing time" is normative about ordering.** A client
routinely writes text and a command in one `write(2)`, and the terminal
reads them in one chunk. `top_of_live_screen` MUST reflect every byte
that preceded the command *in the stream*, not the state before the
chunk began. A terminal that extracts all of a chunk's envelopes before
handing any of that chunk's text to its text parser resolves anchors
against the pre-chunk screen and is off by exactly the text that
arrived alongside the command.

The reference implementation satisfies this by making VGE the last
stage before the text parser and having it consume the stream as
ordered segments, interleaving text with command application
(`veter_host::vge::drive_terminal_stage`). Extensions whose commands
are cursor- or grid-dependent belong in that same stage; a stage placed
after it cannot see the ordering.

### 5.3 Visibility versus the visible viewport

An element with `is_visible = true` is still hidden if its `anchor_line`
sits outside the user's currently visible scrollback window. Rendering
clipping is automatic and not exposed as protocol state.

### 5.4 Alternate screen buffer

When the terminal switches to the alternate screen (DECSET 1047 / 1049),
the current element set is suspended and replaced with an empty set. On
return to the main screen, the alternate set is dropped and the main set
restored. The image table (§8) is shared across screens — uploads
survive the switch.

### 5.5 Resize

When the terminal is resized, element origins, drawing commands, and
anchors are not modified. Elements whose drawing now extends beyond the
grid are simply clipped at render time.

The text grid itself resizes xterm-style on the main screen (no scroll
region active): a **vertical shrink** pushes as many top rows into
scrollback as needed to keep the cursor row on screen (instead of
truncating the bottom of the grid), and a **vertical grow** pulls rows
back out of scrollback. Each pushed/pulled row moves the live screen
relative to scrollback, exactly like a scroll, and anchored elements
travel with their lines per §5.2 — so an element placed above the
prompt (e.g. a `vcat` image) keeps its distance to the prompt through
a shrink/grow cycle without the client doing anything. Width changes
never move the screen origin; rows are truncated or padded in place
(no reflow).

A live client TUI may still catch SIGWINCH (or its winit equivalent)
and reissue `UpdateOrigin` / `UpdateCommands` when it wants layout
beyond what anchoring preserves (e.g. re-fitting drawing to the new
width).

### 5.6 Reset

A full reset (RIS / `ESC c`) and soft reset (DECSTR / `ESC [ ! p`) both
clear the entire VGE state: all elements (both screens), the global
style table, and the image table. The client must re-probe and re-upload
afterwards.

### 5.7 Erase Display

`ESC [ 2 J` (erase visible screen) and `ESC [ 3 J` (xterm "Erase Saved
Lines" — erase scrollback) wipe the text grid in place. vt-style
terminals don't push the cleared cells into scrollback, so VGE
elements anchored to those rows would otherwise stay rendered on top
of now-blank text.

- `ESC [ 2 J` drops every top-level element whose `anchor_line` is
  in the live region (`anchor_line >= top_of_live_screen`).
- `ESC [ 3 J` drops every top-level element whose `anchor_line` is
  in the scrollback region (`anchor_line < top_of_live_screen`).

`clear(1)` (ncurses ≥ 6.0) emits `ESC [ H ESC [ 2 J ESC [ 3 J`, so the
two events together wipe every top-level element on the current
screen. The terminal's underlying text grid + scrollback go with them
(the host terminal must implement `3J` for the text side; see the
notes for veter's vendored vt100 fork in `vendored/vt100/src/screen.rs`).

The style table is not affected; only the element table. Images are not
erased *directly* either, but an erase releases the references the
dropped elements held, so an image nothing else draws is collected as a
consequence (§8.0). Partial erases (`ESC [ J` / `ESC [ 0 J` /
`ESC [ 1 J`) are cursor-relative and do not trigger this cleanup.

## 6. Elements

### 6.1 CreateElement (0x03)

Body:

```
string        id                ; empty string = anonymous, see below
varu          n_commands
DrawCommand[] commands          ; n_commands of them, §7
point         origin
u8            is_visible        ; 0 or 1
i32           draw_order
; optional trailing block — see §9.4 for the full layout. If the body
; ends here, the element is top-level with no parent and no clip.
[u8           extra_flags]
[...          parent_id / size fields, see §9.4]
```

Behavior:

- If `id` is empty, the element is anonymous: it renders normally but
  cannot be the target of any subsequent update or delete. Because it has
  no ID, no non-empty prefix can ever name it either: it is cleaned up
  only by scrollback eviction (§5.2), an empty-prefix `DeleteElement`
  (§6.2), or reset (§5.6).
- If `id` is non-empty and already in use, the entire command fails with
  `err_duplicate_id`. (Client-side replace = explicit `DeleteElement`
  followed by `CreateElement`.)
- For top-level elements (no parent), origin is interpreted per §5.2
  to derive `anchor_line` and `sub_row`. For child elements (§9.1),
  origin is in the parent's interior coordinates and there is no
  scrollback anchoring.
- Draw order ties broken by creation order: among elements with equal
  `draw_order`, later-created elements draw on top. Draw order is
  scoped per parent: only siblings under the same parent are compared
  to each other.
- Response: empty Ok.

Because IDs are picked client-side, the client can pipeline a
`CreateElement` and any number of follow-up updates targeting the same
ID in a single envelope without waiting for the create's response.

### 6.2 DeleteElement (0x04)

```
u8      flags        ; bit0 = by_prefix
                     ; bits 1..7 reserved, must be 0
string  id           ; exact ID, or an ID prefix when bit0 is set
```

Response: empty Ok. A reserved bit set → `err_bad_payload`.

**Exact form (`flags = 0`).** `id` MUST be non-empty (§6.8). Unknown ID →
`err_unknown_element`.

**Prefix form (`bit0` set).** Deletes every element on the current screen
whose ID starts with `id`, compared bytewise. `id` MAY be empty, in which
case it matches *every* element on the screen, anonymous ones included
(§6.1) — this is how a client wipes the screen wholesale, and it is what
retired `ClearAll` (§6.7). A non-empty prefix never matches an anonymous
element, which has no ID to compare.

Matching nothing is **not** an error: the prefix form always answers `Ok`.
That asymmetry with the exact form is deliberate — naming a single ID that
does not exist is a client bug worth reporting, whereas prefix cleanup is
meant to be idempotent, so a client can send it on exit, on startup, or
twice, without having to track what it actually created.

In both forms, deleting an element deletes its descendants (§9) too — the
response is `Ok` regardless of how many were destroyed. In the prefix form
this means a **matching parent takes non-matching children with it**: a
client that parents its content to one element it owns can name that
parent and reach a whole subtree of IDs it never chose (a document
renderer whose shape IDs come from the file it loaded, say).

Only the current screen's element table is touched (§5.4). Compare
`DropImage`'s prefix form (§8.2), which reaches a table shared by both
screens.

Prefix deletion is the reason to namespace IDs; see §6.8.

### 6.3 UpdateCommands (0x05) / UpdateCommand (0x06)

`UpdateCommands` body:

```
string        id
varu          n_commands
DrawCommand[] commands
```

Replaces the element's entire draw command list.

`UpdateCommand` body:

```
string      id
varu        index
DrawCommand command
```

Replaces a single draw command at the given index. Out-of-range index →
`err_command_index`. Index equal to current length is *not* permitted (use
`UpdateCommands` to grow).

### 6.4 UpdateText (0x07)

Targets a specific `DrawText` command within an element.

Body:

```
string     id
varu       command_index        // index into element.commands
u8         mode                 // 0 = whole text, 1 = byte range
// if mode == 1:
varu       byte_start
varu       byte_end
// always:
string     replacement
```

In range mode (`mode = 1`), `byte_start` and `byte_end` are byte offsets
into the existing text's UTF-8 representation; `byte_end` is exclusive. The
range must:

- Satisfy `byte_start ≤ byte_end ≤ current_length`.
- Land on UTF-8 character boundaries (both ends).

Otherwise → `err_text_range`. The replacement bytes are inserted between
the two offsets; replacement text itself must be valid UTF-8.

If `command_index` does not point to a `DrawText` command → `err_bad_payload`.

### 6.5 UpdateImage (0x08)

Patches a `DrawImage` command in place. Any combination of
`image_id`, source ROI, and `target_rect` can be replaced atomically;
unset fields keep their previous values. Intended both for animation
(swap `image_id` between pre-uploaded frames, or advance a sprite-atlas
ROI on a fixed image) and for dynamic zoom/pan (move/scale source and
target rects without re-uploading pixels).

Body:

```
string id                       ; element ID
varu   command_index            ; index into element.commands
u8     update_flags             ; bit0 = set_image_id
                                ; bit1 = set_source_rect
                                ; bit2 = set_target_rect
                                ; bits 3..7 reserved (must be 0)
; if bit0 (set_image_id):
string new_image_id             ; must reference an uploaded image (§8.2)
; if bit1 (set_source_rect):
u8     source_mode              ; 0 = clear ROI (sample full image)
                                ; 1 = explicit pixel rect
; if bit1 AND source_mode == 1:
rect   new_source_rect_px       ; f32 x,y,w,h in source image pixels (§7.5)
; if bit2 (set_target_rect):
rect   new_target_rect          ; cell units, relative to element.origin
```

Validation is atomic across all fields:

- If `command_index` does not point to a `DrawImage` command →
  `err_bad_payload`.
- `update_flags == 0` (no-op) or any reserved bit set →
  `err_bad_payload`.
- If bit0 set and `new_image_id` is not a known image →
  `err_unknown_image`.
- If bit1 set, `new_source_rect_px` (when present) is validated per
  §7.5 against the image that will be in effect after this update
  (the new image if bit0 is also set, otherwise the current one).
  Out-of-bounds, non-finite, or negative w/h → `err_bad_payload`.
- If bit2 set, `new_target_rect` must be finite. There is no further
  cell-space bound (drawing off-grid is silently clipped — §5.5).

On any error the underlying `DrawImage` is unchanged.

### 6.6 UpdateOrigin (0x09) / UpdateVisibility (0x0A) / UpdateDrawOrder (0x0B)

```
UpdateOrigin:     string     id, point new_origin
                  [u8        extra_flags]   ; anchor bits only, see below
                  [string    marker]        ; if bit4
UpdateVisibility: string     id, u8 is_visible
UpdateDrawOrder:  string     id, i32 draw_order
```

`UpdateOrigin` re-anchors per §5.2, and takes the same optional trailing
flags byte `CreateElement` does — presence decided strictly by body
length, so a body ending after `new_origin` is the base layout and means
viewport-relative exactly as before.

Only the anchor bits (`bit3` cursor-relative, `bit4` marker-anchored;
§9.4) are meaningful here. `parent`, `size` and `transform` are not
re-pinnable through this command, so `bit0`–`bit2` and the reserved bits
are `err_bad_payload`, as is setting both anchor bits.

### 6.7 ClearAll — retired (was 0x0E)

Removed. `DeleteElement` (§6.2) with `by_prefix` set and an empty prefix
does the same job — every element on the current screen, anonymous ones
included, and still nothing to the image or global style tables.

It went because it was the *only* cleanup primitive and it was unscoped: a
client that wanted to remove its own elements had either to enumerate every
ID it had created or to wipe the screen, a neighbour's elements included.
A prefix names exactly one client's worth of state, and the empty prefix
keeps the wholesale case available for anyone who genuinely wants it.

Section number retained so §6.8 and later cross-references keep their
numbering.

### 6.8 Element IDs

A string ID:

- Is at most 64 bytes of UTF-8.
- In `CreateElement`: MAY be empty, meaning "anonymous, not addressable
  later" (§6.1).
- In a prefix-matching command (`DeleteElement` §6.2, `DropImage` §8.2):
  MAY be empty, meaning "match everything".
- In every other command: MUST be non-empty; an empty ID is a parse error
  (`err_bad_payload`).
- Is opaque to the terminal beyond byte equality and, for the prefix forms,
  bytewise `starts_with`. The terminal ascribes no structure to an ID: any
  separator convention is the client's own, and a prefix may end mid-word.

There is no rename command. Reusing an ID requires `DeleteElement`
followed by `CreateElement`.

**Namespacing.** Because the element and image tables outlive any one
client — a session's tables persist across client runs, and several clients
may share a screen — a client SHOULD prefix every ID it creates with a name
it owns, e.g. `myapp.thing`. That single convention buys three things: it
can clean up after itself with one command per table (§6.2, §8.2), on
startup as well as exit, so a run is not confused by a previous run's
leftovers; it cannot collide with another client's IDs; and it never
destroys state it does not own. Note that a prefix is a plain byte
comparison, so pick a separator and keep it: `myapp.` matches `myapp.a` but
also `myapp-old`, and `img` matches `images.big`.

## 7. Draw commands

### 7.1 DrawCommand encoding

A draw command is:

```
u8 op
<op-specific body>
```

Opcodes:

| Op   | Name                  | Notes                            |
|------|-----------------------|----------------------------------|
| 0x01 | FillPolygon           |                                  |
| 0x02 | FillRectangles        |                                  |
| 0x03 | FillPath              |                                  |
| 0x04 | DrawLines             | independent line segments        |
| 0x05 | DrawLineLoop          |                                  |
| 0x06 | DrawLineStrip         |                                  |
| 0x07 | DrawLinePath          |                                  |
| 0x08 | OutlineFillPolygon    | fill + stroke                    |
| 0x09 | OutlineFillRectangles |                                  |
| 0x0A | OutlineFillPath       |                                  |
| 0x20 | DrawText              | §7.4                             |
| 0x21 | DrawImage             | §7.5                             |

Every shape op in 0x01–0x0A uses cell-unit coordinates (§5.1). The body
formats below are self-contained — no separate scale or coordinate-range
field exists; clients send raw `f32` cell-units and the terminal renders
them directly.

### 7.2 Shape command bodies

Each shape command's body:

```
FillPolygon:
  Style fill_style
  varu  n_points     ; n ≥ 3
  point points[n]

FillRectangles:
  Style fill_style
  varu  n_rects
  rect  rects[n]

FillPath:
  Style fill_style
  varu  n_segments
  PathSegment segments[n]

DrawLines:
  Style line_style
  f32   line_width
  varu  n_lines
  (point a, point b)[n]

DrawLineLoop / DrawLineStrip:
  Style line_style
  f32   line_width
  varu  n_points    ; ≥ 2
  point points[n]

DrawLinePath:
  Style line_style
  f32   line_width
  varu  n_segments
  PathSegment segments[n]

OutlineFillPolygon / OutlineFillRectangles / OutlineFillPath:
  Style fill_style
  Style line_style
  f32   line_width
  <body of corresponding fill command, minus the leading style>
```

A `PathSegment` is a single subpath: a starting point followed by a
sequence of nodes. Each segment is fully self-describing so the wire
format can be parsed in a single forward pass.

```
PathSegment:
  point start
  varu  n_nodes
  PathNode nodes[n_nodes]
```

A `PathNode` is one byte of `kind` followed by a kind-specific body:

```
u8 kind
body[kind]:
  0 LineTo:               point dst
  1 HorizontalLineTo:     f32 x        ; current y unchanged
  2 VerticalLineTo:       f32 y        ; current x unchanged
  3 CubicBezierTo:        point c0, point c1, point dst
  4 ArcEllipseTo:         u8 flags, f32 rx, f32 ry, f32 rotation, point dst
                          ; flags: bit0 = large_arc, bit1 = sweep
                          ; rotation in radians
  5 ClosePath:            (no body)
  6 QuadraticBezierTo:    point c, point dst
```

`kind` values outside 0–6 are reserved and MUST be rejected with
`err_bad_payload`. In particular, a `kind` byte with bit 7 set is
reserved (it had a meaning in earlier drafts and is now invalid).

Arc semantics for kind 4 follow SVG path arcs: an arc connects the
previous current-point to `dst`, sweeping around an implied center
such that the arc has the given `rx`/`ry` and rotation, with the
`large_arc` and `sweep` flags selecting which of the four candidate
arcs to use. `rotation` is in radians and applies to the ellipse's
x-axis. Degenerate inputs follow SVG: zero radius collapses to a
`LineTo`, and out-of-range radii are uniformly scaled up to just
reach `dst`.

There is intentionally no "circular arc" form (single-radius). Cells
are anisotropic — a single-radius arc is rarely visually circular —
so the protocol expects clients to compute compensated `rx`/`ry`
themselves using the cell pixel dimensions from the probe response
when they want a true visual circle.

Coordinates, control points, arc radii, and `line_width` are all `f32`
cell-units (anisotropic — §5.1).

### 7.3 Style encoding and the global style table

```
Style:
  u8 kind
  // kind == 0x01  Flat:
  Color color
  // kind == 0x02  LinearGradient:
  point p0, p1
  Color c0, c1
  // kind == 0x03  RadialGradient:
  point center, outer
  Color c_inner, c_outer
  // kind == 0xFF  StyleRef:
  string id

Color:
  u8 format               // 0x01 = RGBA8888, 0x02 = RGB565
  // 0x01: u8 r, u8 g, u8 b, u8 a   (straight alpha, not premultiplied)
  // 0x02: u16 packed              (5-6-5, alpha implicitly 0xFF)
```

`StyleRef` resolves against the global style table at *render time*, not
command-processing time. This is what makes the table useful for
theme-style updates: a `SetGlobalStyle` repaints every element that
referenced the ID.

`SetGlobalStyle` body:

```
string id
Style  style       // must not itself be a StyleRef
```

Setting a style with kind `0xFF` (StyleRef) → `err_bad_payload`. Styles
can be upserted; there is no delete (clients can effectively shadow with
a transparent flat color if needed). Keys are at most 64 UTF-8 bytes.

If a `StyleRef` is encountered at render time and the ID is unknown, the
element renders with a 100%-magenta flat color (a deliberate eye-catcher)
and the terminal logs (but does not respond with) an error. Render-time
errors do not produce response frames, since rendering is decoupled from
command processing.

#### Reserved `host.*` style ids

Style ids beginning with `host.` are **host-owned**. They let a host
publish its own theme (accent colors, etc.) into the style table so
clients can reference it by `StyleRef` instead of hardcoding colors.

- A client `SetGlobalStyle` whose id starts with `host.` MUST be rejected
  with `err_reserved_style_id`; the table is left unchanged.
- A host MAY pre-populate any `host.*` id. It SHOULD do so when the
  client begins a session (so the very first `StyleRef` resolves) and
  MUST re-inject its `host.*` entries after any RIS/DECSTR that clears
  the table (§5.4) — otherwise a client's surviving elements would render
  magenta after a reset.
- Whether a host populates these is advertised out-of-band. When the
  Portal Extension is also implemented, the host signals it with the
  `host_themed_styles` capability bit (portal-extension.md §10); a client
  that does not see the bit MUST assume the ids are absent and fall back
  to its own colors.

Reserved ids defined in v0:

| Id              | Meaning                                                       |
|-----------------|---------------------------------------------------------------|
| `host.accent`   | Contextual accent. In a per-portal VGE engine the host keys this on the portal's nesting depth, so nested clients get distinct accents. |
| `host.accent.1` | Explicit accent slot 1 (does not rotate with depth).          |
| `host.accent.2` | Explicit accent slot 2.                                       |
| `host.accent.3` | Explicit accent slot 3.                                       |

A host with fewer configured accents than slots populates only the slots
it has; `host.accent` always resolves (it wraps around the available
slots).

### 7.4 DrawText (0x20)

```
point     origin           ; relative to element.origin
u8        align            ; 0 = Left, 1 = Center, 2 = Right
Style     fill_style
u8        font_style       ; bitmask
string    text             ; UTF-8, single-line
```

`font_style` bits: 0x01 Bold, 0x02 Italic, 0x04 Underline, 0x08
Strikethrough. Multiple bits may be combined.

The text is rendered in the terminal's primary font at the same size used
for the cell grid. Multi-line text is not supported; embedded `\n` is
treated as a literal character (typically rendered as a tofu glyph).

`align` controls horizontal anchoring relative to `origin`:

- Left   → text starts at `origin.x`
- Center → text is centered on `origin.x`
- Right  → text ends at `origin.x`

Vertical alignment: the text baseline sits at `origin.y` (interpreted in
cell-height units, then converted to the font's pixel baseline using the
ascent of the primary font).

### 7.5 DrawImage (0x21)

```
rect    target_rect           ; cell units, relative to element.origin
string  image_id              ; references an uploaded image (§8.2)
u8      flags                 ; bit0 = has_source_rect
                              ; bits 1..7 reserved (must be 0)
; if bit0 (has_source_rect):
rect    source_rect_px        ; f32 x,y,w,h in source image pixels
```

The image must have been fully uploaded with `UploadImage` (§8.2) —
the final chunk (`is_last = true`) must already have been processed
when this command runs. An in-progress chunked upload is not yet
visible; referencing its id → `err_unknown_image`. Unknown ID →
`err_unknown_image` and the enclosing `CreateElement` /
`UpdateCommands` / `UpdateCommand` fails atomically.

`flags` is mandatory. Any reserved bit set → `err_bad_payload`.

If `has_source_rect` is unset (`flags == 0`), the whole image is
sampled. If set, `source_rect_px` selects a sub-region of the source
image in its native pixel coordinates (top-left origin, +x rightward,
+y downward). The selected region is stretched to fit `target_rect`
exactly as in the no-ROI case — only the *source* sampling changes.

`source_rect_px` validation, at command-processing time:

- Components must be finite.
- `w >= 0` and `h >= 0`. A region with `w == 0` or `h == 0` is legal
  and renders nothing (matches the "collapse without delete" pattern
  in §9.4).
- The region must fall fully within `[0, image.width] × [0, image.height]`.

Any violation → `err_bad_payload`, atomic.

Common patterns:

- **Sprite-sheet animation**: upload the atlas once; advance frames
  with tight `UpdateImage` calls that set only `source_rect_px`
  (§6.5).
- **Dynamic zoom/pan**: keep `image_id` fixed and update
  `source_rect_px` (zoom = scale, pan = translate) and/or
  `target_rect`.

If the referenced image is later dropped (`DropImage`) while the
element remains live, rendering of the affected `DrawImage` falls
back to a magenta debug fill (same treatment as missing styles, §7.3)
regardless of any ROI. The element itself stays — only its image
rendering is degraded — and a fresh `UpdateImage` to a valid ID
restores normal rendering.

The selected source region is stretched to fit `target_rect`.
Interpolation is implementation-defined (the femtovg-based renderer
in this repo will use linear filtering).

## 8. Image table

Images are uploaded once and addressed by client-supplied string ID, the
same way elements work. The image table is **session-scoped**: it lives
for the lifetime of one terminal process, is shared across both screen
buffers, and is wiped by full or soft reset (§5.6) and by terminal close.
There is no persistent or cross-process cache in v1.

This separation between upload and draw exists for two reasons: clients
can hold large images once and reference them cheaply, and animations
can cycle through pre-uploaded frames via `UpdateImage` without
re-transmitting pixel data.

`UploadImage` (§8.2) is chunk-aware: every upload carries
`total_bytes`, `chunk_offset`, and an `is_last` flag, so a small
image fits in a single chunk (offset=0, is_last=true) while a large
image streams across many envelopes. The terminal answers each chunk
with a `ChunkAck` carrying `bytes_received` so the sender can surface
byte-level progress to the user (e.g. over SSH).

### 8.0 Lifetime

Every uploaded image carries a **retention** policy (§8.2), chosen by
the uploader, that decides how it is reclaimed.

**Auto (the default) — reference-counted.** The image is reference-counted
against the `DrawImage` commands naming it. The count spans both screens'
element tables, since the image table is shared and the element tables
are not (§5.4). When the count falls to zero the terminal drops the image
and frees its memory. Two rules make this safe for the ordinary
upload-then-draw sequence:

- An image that has **never** been referenced is never collected. A
  fresh upload sits at zero references, often across several envelopes,
  until the element that draws it is created.
- A count only reaches zero by losing a reference it once had. Deleting
  an element, replacing its commands, re-pointing a `DrawImage` with
  `UpdateImage`, an erase (§5.7), scrollback eviction (§5.2) and leaving
  the alternate screen all release references. Deleting by prefix (§6.2)
  releases the references of every element it removes, descendants
  included.

This is what a one-shot display wants: draw it, scroll it away, and the
slot is reclaimed with no bookkeeping. But it makes an image unusable as
a *cache* — a client that references an image intermittently (a
thumbnail grid paging images on and off screen, say) would have it
collected the instant it leaves view, and re-referencing it then fails
with `err_unknown_image` (§7.5, resolution is atomic at command time).

**Manual — client-managed.** The terminal never auto-collects the image:
it survives any number of periods with no `DrawImage` referencing it, and
is removed only by an explicit `DropImage` (or a whole-table reset —
`RIS`). Reference counting still runs (so the count is correct if the
image is later re-uploaded as Auto, and for diagnostics), but a count of
zero does not trigger collection. This is the policy for a caching
client; the cost is that bounding the table is now the client's job —
it must `DropImage` what it no longer wants, and an unmanaged flood of
Manual uploads will hit `max_images` and fail further uploads with
`err_too_many_images` rather than being reclaimed for it.

`DropImage` (§8.2) means "remove now, whatever is referencing it" under
both policies — elements holding the id keep rendering their fallback.

Neither policy reclaims an image under table pressure: there is no
LRU/eviction. `max_images` is a hard ceiling; when it is reached the
next upload fails with `err_too_many_images`, and freeing space is the
sender's responsibility (let an Auto image go unreferenced, or
`DropImage` a Manual one).

### 8.1 ImageData encoding

```
u8 encoding              ; 0x01 = Raw RGBA8, 0x02 = WebP
u32 width
u32 height
bytes pixel_or_file_data ; for Raw: width*height*4 bytes RGBA8 (straight alpha)
                         ; for WebP: a complete WebP file
```

For WebP, `width` and `height` MUST match what the WebP file decodes to;
mismatch → `err_image_decode`. (The duplication lets the terminal reject
oversized images before invoking the WebP decoder.)

### 8.2 UploadImage (0x0C) / DropImage (0x0D)

```
UploadImage:
  string id                ; non-empty, ≤ 64 UTF-8 bytes
  u8     encoding          ; 0x01 = Raw RGBA8, 0x02 = WebP (§8.1)
  u32    width
  u32    height
  u32    total_bytes       ; size of the full payload (sum of all
                           ; chunks). Repeated in every chunk.
  u32    chunk_offset      ; byte offset of `data` inside the full
                           ; payload. First chunk MUST set to 0.
  bool   is_last           ; true on the final chunk; the terminal
                           ; runs encoding-specific validation
                           ; (§8.1) and registers the image only on
                           ; this transition.
  u8     retention         ; 0 = Auto (reference-counted, default),
                           ; 1 = Manual (client-managed) — see §8.0.
                           ; Repeated in every chunk; applied on the
                           ; last. Unknown values → err_bad_payload.
  bytes  data              ; chunk bytes (varu length prefix); NOT
                           ; the full image — see `total_bytes`.

DropImage:
  u8     flags             ; bit0 = by_prefix
                           ; bits 1..7 reserved, must be 0
  string id                ; exact ID, or an ID prefix when bit0 is set
```

Single-shot uploads (small images) set `chunk_offset = 0`,
`is_last = true`, and put the entire payload in `data` — equivalent
to a pre-chunked-upload `UploadImage` plus a `total_bytes` field.

The terminal responds to every chunk (single-shot included) with
`ChunkAck`:

```
ChunkAck body:
  string image_id
  u32    bytes_received    ; cumulative bytes absorbed for this id;
                           ; equals `total_bytes` on the final chunk.
```

Lifecycle:

1. **First chunk** (`chunk_offset == 0`): the terminal validates id,
   encoding, and size:
   - `id` already in use (live or in-progress) →
     `err_duplicate_image_id`.
   - `encoding` not in advertised `supported_image_encodings` →
     `err_bad_payload`.
   - `total_bytes > max_image_bytes` → `err_image_too_large`.
   - Image table full (live + in-progress) → `err_too_many_images`.

   On success it allocates a `total_bytes`-sized buffer, copies
   `data` at offset 0, and replies `ChunkAck { id, bytes_received =
   data.len() }`. The slot counts against `max_images` from this
   point on (so concurrent in-progress uploads cannot exceed the
   budget).

2. **Subsequent chunks** (`chunk_offset > 0`) MUST match the
   in-progress slot:
   - No slot with this `id` → `err_bad_payload`.
   - Any of `encoding`, `width`, `height`, `total_bytes` differs
     from the first chunk → `err_bad_payload`, slot dropped.
   - `chunk_offset != bytes_received` (out of order or gap) →
     `err_bad_payload`, slot dropped.
   - `chunk_offset + data.len() > total_bytes` → `err_bad_payload`,
     slot dropped.

   On success the chunk is appended at `chunk_offset`,
   `bytes_received` advances, and a `ChunkAck` is emitted.

3. The chunk with `is_last == true` triggers **finalize**:
   `bytes_received` must equal `total_bytes` (else
   `err_bad_payload`); then the encoding-specific validation from
   §8.1 runs (Raw size must equal `width * height * 4`; WebP is
   decoded and decoded dimensions must match). On success the image
   moves from in-progress to finalized and the final `ChunkAck`
   carries `bytes_received == total_bytes`. On failure the slot is
   dropped, `id` is released, and the response is `err_image_decode`
   (decode failure) or `err_bad_payload` (length mismatch).

`DropImage` removes a finalized entry; on an in-progress `id` it
also aborts the upload. It means "remove now, whatever is referencing
it", under either retention policy (§8.0). Live `DrawImage` references
to a dropped ID degrade to magenta debug fills per §7.5; the elements
themselves are not modified. Drawing against an in-progress `id` yields
`err_unknown_image` until finalize — the image is not addressable for
rendering before that point. Response: empty Ok. A reserved bit set →
`err_bad_payload`.

**Exact form (`flags = 0`).** `id` MUST be non-empty. Unknown ID →
`err_unknown_image`.

**Prefix form (`bit0` set).** Removes every image whose ID starts with
`id`, compared bytewise, finalized entries and in-progress uploads alike;
each in-progress match is aborted as the exact form would abort it.
Matching nothing answers `Ok`, not `err_unknown_image` — same reasoning as
§6.2's prefix form: cleanup should be idempotent so a client can run it on
startup and on exit without tracking what it uploaded. This is the intended
way to bound a `Manual`-retention cache (§8.0), which the terminal never
reclaims on the client's behalf.

`id` MAY be empty, matching every image in the table, but **think before
using it**: unlike the element table, the image table is shared by both
screen buffers (§5.4, §8.0). A full-screen client on the alternate screen
that drops every image also destroys images another client left inline on
the main screen. Use a prefix you own (§6.8); the empty prefix is for a
client that is certain it owns the whole table.

Multiple chunked uploads can be in progress concurrently against
distinct `id`s, bounded only by `max_images`. Clients interleaving
chunks for different ids are responsible for ordering them within
each id; the terminal does not buffer or reorder.

Image IDs share the same namespace rules as element IDs (§6.8) but live
in a separate table — an element ID and an image ID with the same string
do not collide.

Image data is held verbatim by the terminal (Raw stays Raw, WebP stays
WebP); decoding to a renderable representation is implementation-defined
and may be lazy or eager.

Reset (`RIS` / `DECSTR`, §5.6) wipes both finalized images and
in-progress slots; the screen-buffer switch (§5.4) does not — chunked
uploads continue across an alt-screen swap exactly like finalized
images survive it.

## 9. Element parenting and clipping

This section gives elements two related capabilities. **Parenting**
groups elements into a tree, with shared lifecycle and a parent-relative
coordinate space for children. **Clipping** is an optional rectangular
mask attached to any element — anything that would render outside it
(the element's own commands or any descendants) is clipped away.

Together they let a client build scrollable widgets (chat panes, logs,
lists) without re-uploading content on every scroll tick. The
"viewport" pattern is just a clip element whose children move via
`UpdateOrigin` to give the appearance of scrolling — see §9.9.

### 9.1 Parenting

Every element optionally has a **parent** (another element, by ID).
Parenting is fixed at create time. There is no `Reparent` operation;
the client recreates if it needs to change parents.

- **Top-level elements** (no parent) anchor their origin to a scrollback
  line per §5.2.
- **Child elements** (with a parent) have origins in the parent's
  coordinate space (parent's origin = (0, 0) for the child). They do
  not anchor to scrollback; their lifecycle is governed by the parent.
  When the parent is deleted, evicted, or destroyed by reset, the
  descendants go with it.

Any element can be a parent — it doesn't need a clip rect or any
special flag. A parent without a clip just acts as a group: a single
draw_order slot, shared lifecycle, parent-relative children.

Cycles are impossible: `parent_id` MUST already exist in the element
table when its child is created.

### 9.2 Clipping

An element has a **clip rect** if and only if it was created with the
`size` field (§9.4) or has had one set via `UpdateSize` (§9.5).

When the clip rect is set, it is the axis-aligned rect
`(origin.x, origin.y, size.x, size.y)` in the element's coordinate
space (parent's space, or the screen for a top-level element). At
render time:

1. The renderer pushes a scissor for that rect.
2. Render the element's descendants in `(draw_order, creation_order)`
   order.
3. Render the element's own `commands` (its `DrawCmd[]`) **on top of
   the descendants**, still inside the same clip.
4. Pop the scissor.

If the element has no clip rect, steps 1 and 4 are skipped — its
children and own commands draw freely (and whatever ancestor clip is
on the GPU stack still applies).

**Element commands render after children.** This is deliberate. It
matches the typical decoration use case: borders, edge fades, frames,
overlays, and scroll indicators all want to draw on top of their
contents. Backgrounds, when needed, are added as a low-`draw_order`
child rather than as a parent command.

### 9.3 Coordinate system

For a top-level element, `origin` is interpreted per §5.2 (y is
scrollback-relative at command-processing time).

For a child element, `origin` is the offset within the parent's
coordinate space: a child at `(5, 3)` whose parent has effective
on-screen position `(P_x, P_y)` renders at `(P_x + 5, P_y + 3)`.
Origins are `f32` and may be fractional.

There is no separate "scroll offset" field. To scroll content within a
clipped parent, the client moves the child(ren) via `UpdateOrigin` — see
the cookbook in §9.9.

### 9.4 CreateElement extension

`CreateElement` (§6.1) gains a single optional trailing block, gated by
a flags byte. The base layout (no trailing bytes) is still valid and
produces a top-level element with no parent and no clip.

```
CreateElement (with parent / clip):
  string        id
  varu          n_commands
  DrawCommand[] commands
  point         origin
  u8            is_visible
  i32           draw_order
  u8            extra_flags          ; bit0 = has_parent
                                     ; bit1 = has_size
                                     ; bit2 = has_transform
                                     ; bit3 = cursor-relative origin
                                     ; bit4 = marker-anchored origin
                                     ; bits 5..7 reserved (must be 0)
  ; if bit0 (has_parent):
  string        parent_id
  ; if bit1 (has_size):
  point         size                 ; clip rect width, height in cell
                                     ; units; clip rect is
                                     ; (origin.x, origin.y, w, h) in
                                     ; parent's coords (or the screen
                                     ; for top-level)
  ; if bit2 (has_transform):
  transform     transform            ; 6×f32, see §9.11
  ; if bit4 (marker-anchored):
  string        marker               ; substring to search for
```

#### Anchor modes (bits 3 and 4)

By default `origin.y` is viewport-relative (§5.2), which requires the
client to know where the viewport is. A client that cannot read a DSR
cursor report — anything that is not its pane's foreground program —
cannot, so these two bits let it name a position the terminal resolves
on its behalf. Both are ignored for child elements, whose origin is
parent-relative.

- **`bit3` — cursor-relative.** `anchor_line = cursor_row +
  floor(origin.y)`, where `cursor_row` is the cursor's row within the
  live screen at command-processing time. Negative `origin.y` is
  permitted and reaches the rows above the cursor. Simple, but the
  caller still has to know how far the cursor has drifted from the rows
  it means.
- **`bit4` — marker-anchored.** The trailing `marker` string is searched
  for in the live screen's rows and in whatever scrollback the terminal
  still holds; `anchor_line` is derived from the **bottom-most** row
  whose text contains it, so an application that reprints its token each
  frame anchors to the latest one. The live screen is searched first and
  wins outright: a token on screen is never passed over for an older
  copy in history. This is the better fit for the driving case: the
  application prints a token on the first row it reserved, and the
  terminal — which owns the grid — resolves it. No cursor arithmetic and
  no assumption about the application's live-region height.

  A match in scrollback resolves to an `anchor_line` **above** the live
  screen, and that is not an error. `anchor_line` is absolute (§5.2), so
  the marker names the same text line whether or not it is on screen —
  and it often will not be, because the message that reserved the space
  is frequently taller than the screen. A client that reserves two
  regions in one message has already scrolled the first marker away by
  the time it draws into it. Searching only the live screen would lose
  that first marker, fall back to the viewport, and place the element
  over whatever the scroll left at the top of the screen.

  The search domain is bounded by the terminal's scrollback, so a marker
  whose row has aged out of it matches nothing and takes the fallback
  below. Terminals SHOULD NOT bound it any more tightly than that.

The sub-row fraction (§5.2) is unchanged in both modes, so half-cell
placement still works.

Setting both bits is `err_bad_payload`: they name the same thing two
ways. If the marker matches no row — never printed, misspelled, or
scrolled out of scrollback entirely — or the terminal has no live screen
to resolve against, the origin falls back to viewport-relative rather
than failing: the element lands where a default-anchored one would,
which is visibly wrong rather than silently displaced.

Neither mode is affected by where the *user* has scrolled the view.
Scroll position is a property of the view; the anchor is a property of
the text, and the two must not be able to change each other.

**Space is reserved in-band, by the application.** These modes only let
a client *name* a location; they do not create room for it. A client
that emits newlines to reserve vertical space desynchronises the
foreground program's renderer, which dead-reckons its frame position —
scrolling underneath it strands the old frame and misplaces the next.
The application must emit the blank rows as part of its own output, so
its renderer accounts for them; the client then anchors into that space.

Resolution happens at command-processing time, which per §5.2 means
after every byte that preceded the command in the stream has reached the
text parser.

Validation:

- If `bit0` is set, `parent_id` MUST be non-empty and MUST already
  resolve to an existing element, else `err_unknown_element`, atomic.
- If `bit1` is set, `size`'s components MUST be finite and `>= 0`,
  else `err_bad_payload`. A `size` of `(0, 0)` is permitted and clips
  every descendant pixel; clients use it for "collapse without
  delete".
- If `bit2` is set, all six `transform` components MUST be finite,
  else `err_bad_payload`. Singular matrices (determinant 0) are
  permitted and render degenerate (collapsed) geometry.
- If the resulting tree depth would exceed advertised
  `max_nesting_depth` (§9.7), → `err_max_nesting_depth`, atomic.
- Any reserved bit (5..7) set in `extra_flags` → `err_bad_payload`.
- Both `bit3` and `bit4` set → `err_bad_payload`.
- If `bit4` is set, `marker` MUST be present and non-empty, else
  `err_bad_payload`.
- Trailing-byte presence is decided strictly by body length. If the
  body ends after `draw_order` there is no `extra_flags`. Trailing
  bytes that don't form a complete optional block are
  `err_bad_payload`.

### 9.5 UpdateSize (0x0F)

Body:

```
string id
point  new_size
```

Sets the named element's clip rect size to `new_size`. If the element
had no clip rect before, it now does. To remove clipping, recreate the
element.

Errors:

- Empty id → `err_bad_payload`.
- Unknown id → `err_unknown_element`.
- `new_size` components non-finite or negative → `err_bad_payload`.

Response: empty Ok.

### 9.6 Lifecycle and cascading

- `DeleteElement` (§6.2) on any element deletes its entire subtree.
- `DeleteElement` by prefix (§6.2) deletes the subtree of every match, so a
  matching parent takes non-matching descendants with it. An empty prefix
  wipes every element on the current screen, parents and descendants alike.
- Scrollback eviction (§5.2) of a top-level element cascades to its
  subtree.
- Reset (§5.6) wipes everything.
- `UpdateOrigin` on a parent moves its whole subtree (the descendants'
  origins are parent-relative and unchanged; their *screen* positions
  move with the parent).
- `UpdateVisibility` on a parent with `is_visible = false` skips
  rendering of the entire subtree. Descendants' own `is_visible` is
  preserved.
- `UpdateSize` only affects the element's own clip rect; it does not
  cascade.
- `UpdateTransform` (§9.12) replaces only the element's own matrix;
  descendants keep theirs (their on-screen rendering is composed
  through the ancestor's matrix, §9.11).

### 9.7 Nesting limits

The probe response advertises `max_nesting_depth` (§2.1, recommended
**16**). A `CreateElement` whose `parent_id` resolves to an element
already at depth `max_nesting_depth − 1` fails atomically with
`err_max_nesting_depth`. `max_nesting_depth = 0` means parenting is
unsupported on this terminal; clients should fall back to flat
top-level elements (and client-side scrolling).

The total-element budget (`max_elements`) still applies and counts
every element regardless of position in the tree.

### 9.8 Rendering details (informational)

The reference renderer (femtovg) implements clipping via
`canvas.scissor(...)` and the parent-relative translation via
`canvas.translate(...)`, paired with `canvas.save()` /
`canvas.restore()` to maintain a stack across nested clips.
Pixel-level filtering / anti-aliasing at the clip boundary is
implementation-defined.

Femtovg's scissor is axis-aligned rectangular only. Non-rectangular
clip shapes are out of scope for v1. The reference renderer sets an
element's scissor *before* applying that element's own transform
(§9.11), so the clip rect is exact and never rotates with the element.
Under a rotating *ancestor* transform, femtovg intersects scissors via
an axis-aligned approximation in the ancestor-transformed space —
nested clips inside rotated subtrees are approximate.

Render order at each level:

1. Push the parent's translate (and scissor if it has a clip rect),
   then the parent's transform (§9.11) if it has one.
2. Render each child recursively, sorted by
   `(child.draw_order, child.creation_order)` ascending.
3. Render the parent's own `commands` (its `DrawCmd[]`) on top.
4. Pop the transform / scissor / translate.

Across different parents, the parents' own draw orders take
precedence — the entire subtree of a lower-draw-order parent renders
before the entire subtree of a higher-draw-order parent.

### 9.9 Cookbook: scrollable viewport

The recommended pattern for a scrollable widget is two elements:

```
clip-element        (has size = pane bounds; parent_id = whatever)
└── content-group   (no size; parent_id = clip-element)
    ├── line-1, line-2, line-3, … line-N    (parent_id = content-group)
```

To **set up**: create the clip-element, then the content-group as its
child, then all the line elements as children of the content-group.
The lines lay out at their natural positions inside the content-group's
coordinate space.

To **scroll** by Δ cells: send one `UpdateOrigin` on the
content-group with the new origin. All grandchildren visually shift
together; lines outside the clip-element's bounds disappear. One
~30-byte envelope per scroll tick, regardless of how many lines are in
the widget.

To **draw a frame, edge fade, or scroll indicator** that should not
scroll: put it in the clip-element's own `commands` (which render on
top of children, §9.2) or as a sibling of the clip-element (above it
in draw_order).

To **draw a background** that does scroll with the content: a child of
the content-group with low draw_order. To **draw a background** that
does not scroll: a low-draw-order child of the clip-element (sibling
of the content-group).

### 9.10 Interaction with input

VGE itself delivers no mouse events. The client TUI receives wheel /
click events from the terminal via existing VT100 mouse reporting,
hit-tests against its own model of which clip rect is where, and sends
`UpdateOrigin` on the appropriate content-group accordingly. This
keeps the protocol stateless on input and lets the client own all
interaction policy (scroll acceleration, kinetic scrolling, focus,
etc.).

### 9.11 Element transforms

Every element carries an optional affine transform, default identity.
On the wire it is the `transform` primitive (§1.4): six `f32`s
`a, b, c, d, e, f` in the SVG / Canvas2D `matrix(a,b,c,d,e,f)`
convention — conceptually the 3×3 matrix

```
      | a  c  e |
M  =  | b  d  f |        x' = a·x + c·y + e
      | 0  0  1 |        y' = b·x + d·y + f
```

with the bottom row implied (no perspective). It is set at create time
via `extra_flags` bit2 (§9.4) or replaced later with `UpdateTransform`
(§9.12).

**Semantics.** The transform applies to the element's own draw
commands *and its entire descendant subtree*, pivoting about the
element's origin. Because cells are not square, the two parts of the
matrix act in different spaces, chosen so that a rotation matrix
produces a visually true (circular) rotation:

- the linear part `L = [[a, c], [b, d]]` acts on the element's
  *rendered pixel geometry* relative to its origin;
- the translation `t = (e, f)` is in *cell units*, like every other
  coordinate in this spec.

Formally, with `S = diag(cell_pixel_width, cell_pixel_height)`, `O`
the element's effective on-screen origin in pixels, and `p` a point in
element-local cell coordinates:

```
pixel = O + L·(S·p) + S·t
```

Consequences of this split: axis-aligned scales and pure translations
behave identically to the naive cell-space interpretation (`L·S` and
`S·diag(sx, sy)` commute), but `rotate(θ)` spins a shape rigidly on
screen instead of shearing it through the cell aspect ratio.

**Composition.** Transforms nest multiplicatively down the element
tree (a matrix stack). A child's transform pivots about the child's
own origin *in the parent's untransformed space*; the parent's
transform then applies to the combined result. There is no way to
opt a child out of an ancestor's transform.

**Interactions.**

- `UpdateOrigin` moves the pivot: the transform is independent state
  and `O` is evaluated at render time.
- Scrollback anchoring, eviction (§5.2) and renderer culling use the
  *untransformed* anchor. Translating an element far off-screen via
  `f` does not re-anchor it, and the reference renderer may mis-cull
  elements translated by more than ~1024 rows. Use `UpdateOrigin` for
  large moves.
- **Clip rects do not transform.** The element's own clip rect (§9.2)
  stays axis-aligned at `(origin, origin + size)` in the element's
  *untransformed* coordinate space; transformed content is still
  clipped by it, but the rect itself never rotates or scales with the
  element's matrix. (See §9.8 for the nested-ancestor caveat.)
- Strokes, gradients, images and text all transform with the geometry:
  a stroke under a 2× scale renders twice as wide, text rotates with
  its element.
- `draw_order` comparisons are unaffected.

There is no pivot field; clients bake pivots into the matrix (§9.13).
"Clearing" a transform = sending the identity `(1, 0, 0, 1, 0, 0)`.

### 9.12 UpdateTransform (0x10)

Body:

```
string    id
transform transform        ; 6×f32 a, b, c, d, e, f
```

Replaces the named element's transform unconditionally.

Errors:

- Empty id → `err_bad_payload`.
- Unknown id → `err_unknown_element`.
- Any non-finite component → `err_bad_payload`.

Response: empty Ok. No new error codes are introduced; the §4 table is
unchanged.

### 9.13 Cookbook: rotate-about-center spinner

The motivating use case: create a complex shape once, then animate it
with one small envelope per frame instead of re-sending geometry.

To rotate visually by `θ` about the element-local cell point
`(cx, cy)` on a terminal whose probe reported cell pixel size
`(cw, ch)`:

```
a = cos θ      c = −sin θ
b = sin θ      d =  cos θ
e = cx·(1 − cos θ) + cy·sin θ·(ch/cw)
f = cy·(1 − cos θ) − cx·sin θ·(cw/ch)
```

(Derivation: `t = S⁻¹·(I − R)·S·c` — the translation must round-trip
through pixel space because `L` acts there while `t` is in cell
units.)

Tip: build the geometry centered on the element origin so that
`cx = cy = 0` and the matrix is a pure rotation (`e = f = 0`) — then
the per-frame update needs no cell-size math at all. One
`UpdateTransform` envelope is ~40 bytes regardless of how complex the
shape is.

### 9.14 Future work (deferred)

Possible additions for later versions:

- `Reparent` / `MoveElement` — change an element's parent. Deliberately
  excluded today: parenting at create time gives a much simpler tree
  invariant, and the client can always recreate.
- Removing a clip rect post-create. Today the only way to "unclip" is
  to recreate the element. A flag on `UpdateSize` could clear it; not
  worth the byte yet.
- Non-rectangular clip shapes. Requires a different renderer
  technique (offscreen targets or stencil) and is out of scope.

## 10. Rendering semantics

- The terminal's text layer always renders below all VGE elements. There
  is no protocol for placing graphics below text.
- Cell backgrounds (from text attributes) render before glyphs and before
  VGE elements, so a colored cell background is visible through any
  transparent regions of overlaid graphics.
- Within VGE, top-level elements render sorted by `(draw_order,
  creation_order)` ascending; later in this ordering = on top. With
  parenting (§9), draw-order comparison is scoped per parent, and a
  parent's own commands render after its children — see §9.2 / §9.8.
- Anti-aliasing and stroke caps/joins are implementation-defined; this
  spec does not require pixel-identical rendering across implementations.
- Premultiplication: colors on the wire are straight (not premultiplied).
  Premultiplication for blending is the renderer's concern.

## 11. Limits and budgeting

The terminal advertises hard limits via the probe response. The client is
responsible for staying within them. Over-limit operations fail atomically
with the relevant error code. A non-exhaustive list:

- `max_elements`: per screen buffer.
- `max_commands_per_element`: applies to both `CreateElement` and
  `UpdateCommands`.
- `max_text_bytes`: per `DrawText` text field after any `UpdateText`.
- `max_image_bytes`: byte size of the full `UploadImage` payload — i.e.
  `total_bytes` from §8.2, summed across all chunks. Not a per-chunk
  cap.
- `max_images`: number of concurrently-allocated entries in the
  session image table (§8). In-progress chunked uploads (§8.2) count
  toward this budget exactly like finalized images.
- `max_nesting_depth`: parent-child tree depth (§9.7). 0 means
  parenting unsupported.

The reference implementation in this repo SHOULD start with: 4096
elements, 4096 commands per element, 1 MiB text per command, 256 MiB per
image, 1024 concurrent images, 16 levels of parent nesting. These
numbers can be tuned without breaking the protocol.

### 11.1 Discovery without a round-trip

A client that is not the foreground program of its pane cannot read the
probe response: the reply lands in the pane's *input* queue, where the
foreground program is blocked in `read()`, and whoever the kernel wakes
gets the bytes. `REQ_ID_NO_RESPONSE` (§1.2) lets such a client push
state silently, but it can never *ask* the terminal anything. Two
non-round-trip sources exist for what it needs.

**Live values — `TIOCGWINSZ`.** The terminal MUST populate `ws_xpixel`
and `ws_ypixel` with the grid's size in **device** pixels, so that
`ws_xpixel / ws_col` and `ws_ypixel / ws_row` equal the
`cell_pixel_width` / `cell_pixel_height` this section's probe response
advertises. The two sources disagreeing would give clients a silent 2×
error on HiDPI. A multiplexer that owns sub-grids MUST multiply the
cell size back out per pane rather than forwarding the host's own
values. Cell dimensions are deliberately *only* here: they change with
font size and DPI, and an environment variable would go stale silently.

**Static values — the environment.** The terminal SHOULD export:

| variable       | meaning                                                |
|----------------|--------------------------------------------------------|
| `VETER`        | terminal version; its presence means "running under veter" |
| `VETER_LIMITS` | the caps below, as comma-separated `key=value` pairs   |

`VETER_LIMITS` keys: `mib` = `max_image_bytes`, `mi` = `max_images`,
`enc` = `supported_image_encodings`, `nest` = `max_nesting_depth`,
`mwb` = PRT `max_write_bytes`. Readers MUST ignore unrecognised keys,
so caps can be added without a format break, and MUST fall back to the
recommended defaults above when the variable is absent or a value does
not parse — never to zero, which would fail every upload.

A multiplexer SHOULD additionally export `VMUX_PANE` (the pane's id)
and `VMUX_PANE_TTY` (its pts path), so a client learns which pane it
occupies without walking `ps` ancestry — a heuristic that breaks under
nested sessions or with several clients in one window.

`VETER` is **not** proof that the terminal reachable on stdout speaks
this protocol: it is inherited by every descendant, including one
behind an intermediary that does not relay APC envelopes. A client that
*can* read replies SHOULD still probe.

## 12. Interaction with existing terminal state

- A bell, scroll, or any normal text output does not affect VGE state.
- Cursor position is independent of element origins.
- Grid selection, search, and scrollback navigation operate on the text
  layer; they do not visually mask VGE elements unless explicitly
  rendered as a selection rectangle on top of them. A grid selection
  spanning a region that contains graphics yields the underlying cells
  only.
- A terminal MAY additionally let the user select and copy the contents
  of VGE elements — see §14. That is a terminal-local affordance, not
  protocol state: it generates no frames in either direction and needs
  no client participation.
- VGE issues no DA/DA2/DA3 changes; clients detect support solely via §2.

## 13. Open issues / future work

These are intentionally deferred and are not part of v1:

- Sub-cell rendering hints (text hinting, fractional-cell snap modes).
- Audio/video streams.
- Multi-line / wrapped text.
- A graphics-below-text layer.
- A query-element-existence command (clients track lifetimes themselves).
- Compression on the wire (the byte-stuffed APC envelope is already
  binary; image-level compression via WebP is what we have for now).
- Partial image *writes* — overwriting a region of an uploaded image
  in place without re-uploading the whole thing. Useful for video /
  streaming workloads. (Read-side ROI sampling on `DrawImage` is in
  v1; see §7.5.)
- Cross-session / persistent image cache (browser-style content-addressed
  store shared across terminal restarts). Removed from v1 due to identity
  / partitioning questions that were not resolvable without protocol-level
  client identity.
- Element-level animation slots (pre-register N images on an element,
  advance by index). May beat per-frame `UpdateImage` if profiling
  reveals it matters; deferred until that data exists.

## 14. Host-side selection and copy (informational)

Nothing in this section is protocol. It describes an affordance a
terminal MAY offer over content a client has already drawn, and exists
so clients know what a user can do with their `DrawText` and what that
implies for how they use it.

A terminal knows the string behind every `DrawText` and the geometry it
painted it at, so it can let the user select a range of that text with
the pointer and copy it — the same gesture that selects cells in the
text grid. This is **terminal-local**: no frame is sent in either
direction, no element state changes, and the client is not told. There
is nothing to opt into and nothing to implement.

Consequences worth knowing when writing a client:

- **`DrawText` content is user-copyable.** Use it for text the user
  might reasonably want — labels, filenames, titles, messages. Text
  that is really decoration (an icon assembled from glyphs, a spinner
  frame, a box-drawing rule) is better drawn as paths, or the user will
  eventually copy it and get nonsense.
- **Shaping is the terminal's.** A run is measured with the terminal's
  own font metrics (§7.4), which is why a client cannot predict where a
  character boundary falls; `libs/vge-ui`'s `unicode-width` estimate is
  an approximation for layout, not a mapping the terminal shares.
- **It does not compete with a mouse-driven client.** VGE delivers no
  mouse events (§9.10), and a client that has enabled VT100 mouse
  reporting keeps receiving every event it did before. A terminal
  offering this affordance is expected to put it behind a gesture that
  is already reserved for the terminal itself — in the reference
  implementation, holding Shift, which is what suppresses mouse
  reporting there.
- **Selection is per-run.** A run is the unit: elements float free of
  the grid and of each other, so there is no defined reading order
  between two runs, or between a run and the cells beneath one.
- **A selection does not pin anything.** It is anchored to an element
  that can be deleted, re-drawn, evicted from scrollback (§5.2) or
  wiped by a reset (§5.6) at any time; the terminal drops it when that
  happens. Clients need do nothing to make that safe.

The same reasoning extends to `DrawImage`, for a terminal that keeps
the decoded pixels around. Two differences from text:

- **The unit is the whole drawable.** There is no sub-rect selection: a
  `DrawImage` is one thing, and narrowing it further is what
  `source_rect_px` is for.
- **What is copied is what is sampled** — the `source_rect_px` region
  (§7.5), not the atlas behind it. A client animating a sprite sheet by
  advancing `source_rect_px` (§6.5) therefore hands the user the current
  frame, which is what they pointed at.

Retention (§8.0) does not enter into it. The copy is taken from the
image as it stands when the user asks; a later `DropImage`, or an Auto
image falling to zero references, affects nothing already on the
clipboard.
