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

### 1.3 ESC byte stuffing

All bytes of the payload (after computing `payload_length`, before placing
in the envelope) are scanned. Any byte equal to `0x1B` is replaced with the
two-byte sequence `0x1B 0x1B`. Decoding reverses this.

This is the only escape rule. All other bytes pass through. `payload_length`
is computed on the *unstuffed* payload, so the receiver knows how much data
to expect after unstuffing.

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
u32  max_image_bytes           // per UploadImage / UploadImageBegin
u32  max_images                // concurrent uploaded images; includes
                               // in-progress chunked uploads (§8.3)
u8   supported_image_encodings // bitmask: bit0=Raw, bit1=WebP
u8   max_nesting_depth         // parent-child nesting cap (§9); 0
                               // means parenting not supported
u32  max_chunk_bytes           // upper bound on a single
                               // UploadImageChunk payload (§8.3)
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
| 0x0E | ClearAll           | §6.7         |
| 0x0F | UpdateSize         | §9.5         |
| 0x10 | UploadImageBegin   | §8.3         |
| 0x11 | UploadImageChunk   | §8.3         |

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
| 0x0030 | err_unknown_image       | image ID does not resolve                        |
| 0x0031 | err_image_too_large     | Image exceeds max_image_bytes                    |
| 0x0032 | err_image_decode        | Image bytes failed to decode (e.g. bad WebP)     |
| 0x0033 | err_duplicate_image_id  | image ID already in use (UploadImage)            |
| 0x0034 | err_too_many_images     | Image budget exhausted                           |
| 0x0035 | err_chunk_overflow      | UploadImageChunk would push cumulative > total_bytes |
| 0x0036 | err_chunk_no_upload     | UploadImageChunk targets an id with no in-progress upload |
| 0x0037 | err_chunk_too_large     | Single chunk exceeds advertised max_chunk_bytes  |
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
grid are simply clipped at render time. The client TUI is responsible for
catching SIGWINCH (or its winit equivalent inside the client) and reissuing
appropriate `UpdateOrigin` / `UpdateCommands` calls.

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
notes for veter's vendored vt100 fork in `vt100/src/screen.rs`).

The shared image and style tables are not affected; only the element
table. Partial erases (`ESC [ J` / `ESC [ 0 J` / `ESC [ 1 J`) are
cursor-relative and do not trigger this cleanup.

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
  cannot be the target of any subsequent update or delete. It will be
  cleaned up only by scrollback eviction (§5.2), `ClearAll` (§6.7), or
  reset (§5.6).
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

Body: `string id`. Response: empty Ok.

Unknown ID → `err_unknown_element`. If the deleted element has
descendants (§9), they are deleted too — the response is `Ok`
regardless of how many were destroyed.

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
UpdateVisibility: string     id, u8 is_visible
UpdateDrawOrder:  string     id, i32 draw_order
```

`UpdateOrigin` re-anchors per §5.2.

### 6.7 ClearAll (0x0E)

Body: empty. Removes every element from the *current* screen buffer. Does
not touch the image table or global style table. Useful for "shutdown" by
the client without issuing a full terminal reset.

### 6.8 Element IDs

A string ID:

- Is at most 64 bytes of UTF-8.
- In `CreateElement`: MAY be empty, meaning "anonymous, not addressable
  later" (§6.1).
- In every other command: MUST be non-empty; an empty ID is a parse error
  (`err_bad_payload`).
- Is opaque to the terminal beyond byte equality.

There is no rename command. Reusing an ID requires `DeleteElement`
followed by `CreateElement`.

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
string  image_id              ; references an uploaded image (§8.2 / §8.3)
u8      flags                 ; bit0 = has_source_rect
                              ; bits 1..7 reserved (must be 0)
; if bit0 (has_source_rect):
rect    source_rect_px        ; f32 x,y,w,h in source image pixels
```

The image must have been fully uploaded with `UploadImage` (§8.2) or
finalized via `UploadImageBegin` / `UploadImageChunk` (§8.3) prior to
the command being processed. An in-progress chunked upload is not yet
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

Small images are uploaded in a single envelope (§8.2). Large images
may be split across many envelopes using the chunked upload commands
(§8.3) so the sender can pace its writes, surface byte-level progress
to the user (e.g. over SSH), and stay within `max_chunk_bytes`.

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
  string    id              ; non-empty, ≤ 64 UTF-8 bytes
  ImageData data

DropImage:
  string id
```

`UploadImage` registers the image under `id`. If `id` is already in use
→ `err_duplicate_image_id`. If the image table is at capacity
(`max_images`) → `err_too_many_images`. If the encoded data exceeds
`max_image_bytes` → `err_image_too_large`. If WebP decoding fails →
`err_image_decode`. Response: empty Ok.

`DropImage` removes the entry if present; unknown ID → `err_unknown_image`.
Live `DrawImage` references to the dropped ID degrade to magenta debug
fills per §7.5, but the elements themselves are not modified. Response:
empty Ok.

Image IDs share the same namespace rules as element IDs (§6.8) but live
in a separate table — an element ID and an image ID with the same string
do not collide.

Image data is held verbatim by the terminal (Raw stays Raw, WebP stays
WebP); decoding to a renderable representation is implementation-defined
and may be lazy or eager.

### 8.3 Chunked upload: UploadImageBegin (0x10) / UploadImageChunk (0x11)

A chunked upload streams a single image across many frames. The
intended use cases are oversized images that would not fit in one
envelope comfortably, and progress reporting on the sender side (the
sender knows how many bytes it has flushed of `total_bytes`).

```
UploadImageBegin:
  string id                ; non-empty, ≤ 64 UTF-8 bytes
  u8     encoding          ; 0x01 = Raw RGBA8, 0x02 = WebP (§8.1)
  u32    width
  u32    height
  u32    total_bytes       ; size of the pixel/file payload that will
                           ; arrive across subsequent UploadImageChunk
                           ; frames, summed; matches the `data` field
                           ; of the equivalent single-shot UploadImage

UploadImageChunk:
  string id
  bytes  chunk             ; raw payload bytes (varu length prefix)
```

Lifecycle:

1. `UploadImageBegin` reserves `id` in the image table as an
   **in-progress** slot, locks in the metadata, and counts against
   `max_images` exactly like a finalized image. Pre-flight checks:
   - `id` already in use (in-progress or finalized) →
     `err_duplicate_image_id`.
   - `encoding` bit not set in advertised `supported_image_encodings`
     → `err_bad_payload`.
   - `total_bytes > max_image_bytes` → `err_image_too_large`.
   - Image table full → `err_too_many_images`.

   On any error the slot is never created and `id` stays free.
   Response: empty Ok.

2. Each `UploadImageChunk` appends `chunk` bytes to the slot in
   arrival order. Chunks within an envelope are processed
   sequentially, and the spec defines no offset / out-of-order
   semantics. Checks per chunk:
   - No in-progress slot with this `id` (never begun, already
     finalized, or aborted) → `err_chunk_no_upload`.
   - `chunk.length > max_chunk_bytes` → `err_chunk_too_large`.
   - `bytes_received + chunk.length > total_bytes` →
     `err_chunk_overflow`.

   On any error the slot is dropped and the `id` released; the
   client must restart from `UploadImageBegin` to retry. Successful
   chunks respond empty Ok unless the chunk triggers finalize (next
   step).

3. The chunk whose arrival brings `bytes_received` up to
   `total_bytes` triggers **implicit finalize**: the encoding-specific
   validation from §8.1 runs (Raw size must equal
   `width * height * 4`; WebP file is decoded and the decoded
   dimensions must match `width`/`height`). If finalize succeeds, the
   image moves from in-progress to finalized and that chunk's
   response is empty Ok. If finalize fails (Raw size mismatch, WebP
   decode error, decoded WebP dimensions disagree) the slot is
   dropped, `id` is released, and the chunk's response carries
   `err_image_decode`.

`DropImage` on an in-progress `id` aborts the upload: the slot is
dropped and `id` is released. Drawing against an in-progress `id`
yields `err_unknown_image` until finalize — the image is not
addressable for rendering before that point.

Multiple chunked uploads can be in progress concurrently against
distinct `id`s, bounded only by `max_images`. Clients interleaving
chunks for different ids are responsible for ordering them
correctly; the terminal does not buffer or reorder.

Reset (`RIS` / `DECSTR`, §5.6) wipes the image table and so drops
every in-progress slot too; the screen-buffer switch (§5.4) does
not — chunked uploads continue across an alt-screen swap exactly
like finalized images survive it.

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
                                     ; bits 2..7 reserved (must be 0)
  ; if bit0 (has_parent):
  string        parent_id
  ; if bit1 (has_size):
  point         size                 ; clip rect width, height in cell
                                     ; units; clip rect is
                                     ; (origin.x, origin.y, w, h) in
                                     ; parent's coords (or the screen
                                     ; for top-level)
```

Validation:

- If `bit0` is set, `parent_id` MUST be non-empty and MUST already
  resolve to an existing element, else `err_unknown_element`, atomic.
- If `bit1` is set, `size`'s components MUST be finite and `>= 0`,
  else `err_bad_payload`. A `size` of `(0, 0)` is permitted and clips
  every descendant pixel; clients use it for "collapse without
  delete".
- If the resulting tree depth would exceed advertised
  `max_nesting_depth` (§9.7), → `err_max_nesting_depth`, atomic.
- Any reserved bit set in `extra_flags` → `err_bad_payload`.
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
- `ClearAll` (§6.7) wipes every element on the current screen including
  parents and descendants.
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

Femtovg's scissor is axis-aligned rectangular only (potentially
rotated by the current transform stack). Non-rectangular clip shapes
are out of scope for v1.

Render order at each level:

1. Push the parent's translate (and scissor if it has a clip rect).
2. Render each child recursively, sorted by
   `(child.draw_order, child.creation_order)` ascending.
3. Render the parent's own `commands` (its `DrawCmd[]`) on top.
4. Pop the scissor / translate.

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

### 9.11 Future work (deferred)

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
- `max_image_bytes`: byte size of a single `ImageData` payload at
  `UploadImage`, or `total_bytes` at `UploadImageBegin`.
- `max_images`: number of concurrently-allocated entries in the
  session image table (§8). In-progress chunked uploads (§8.3) count
  toward this budget exactly like finalized images.
- `max_chunk_bytes`: byte size of a single `UploadImageChunk` payload
  (§8.3).
- `max_nesting_depth`: parent-child tree depth (§9.7). 0 means
  parenting unsupported.

The reference implementation in this repo SHOULD start with: 4096
elements, 4096 commands per element, 1 MiB text per command, 32 MiB per
image, 1024 concurrent images, 64 KiB per chunk, 16 levels of parent
nesting. These numbers can be tuned without breaking the protocol.

## 12. Interaction with existing terminal state

- A bell, scroll, or any normal text output does not affect VGE state.
- Cursor position is independent of element origins.
- Selection, search, and scrollback navigation operate on the text layer;
  they do not visually mask VGE elements unless explicitly rendered as a
  selection rectangle on top of them. Selecting a region that contains
  graphics yields the underlying text only (graphics are not
  copy/pasteable in any form by this protocol).
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
