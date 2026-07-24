# Kitty Graphics Protocol Support (minimal)

> **Status: research / proposal — not normative.** A plan for teaching
> veter enough of the kitty graphics protocol to display raster images
> emitted by third-party programs. Kitty's protocol is specified
> externally (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>);
> this document covers only the subset veter would implement and how it
> maps onto machinery veter already has. Companion:
> `doc/client-integration-groundwork.md`.

## 1. Why

VGE is veter's own graphics protocol and every veter-aware tool speaks
it. Programs that are *not* veter-aware — but are kitty-aware — are a
large, existing population: `icat`, `ranger`, `mpv`, `timg`,
matplotlib's kitty backend, and Claude Code. Supporting a small kitty
subset makes all of them display images in veter with no per-tool work.

It also solves a problem VGE cannot solve from outside. As
`doc/client-integration-groundwork.md` §0 records, vertical space for an
image can only be reserved *in-band*, by the program that owns the
screen. A kitty-protocol emitter is exactly that program, so the
placement problem disappears rather than being worked around.

## 2. What a real emitter sends

Taken from the Claude Code binary (version 2.1.218), by enumerating
every literal `ESC _ G` string in the executable. There are exactly
three:

```
ESC _ G i=31,s=1,v=1,a=q,t=d,f=24 ; AAAA        ESC \    capability probe
ESC _ G a=T,t=d,f=100,q=2         ; <base64 png> ESC \   transmit + display, direct
ESC _ G a=T,t=f,f=100,q=2         ; <base64 path> ESC \  transmit + display, by path
```

Also present nearby in the same (Rust) string table: a `ghostty` /
`wezterm` terminal allowlist, an `image/png` media-type check, and a
text fallback that renders `[img] … (image)` wrapped in an OSC 8
hyperlink.

Notable *absences*, which shrink the required subset considerably:

- no `m=` chunking literal — whole PNGs go out in a single escape;
- no `c=` / `r=` — the terminal derives the cell footprint from the
  image's natural pixel dimensions;
- no `C=1` — the emitter relies on the terminal advancing the cursor;
- no `a=d` — it never deletes.

Two caveats. This is one emitter at one version, and the wider
population (notably `icat`) *does* chunk, *does* size explicitly and
*does* delete, so a useful implementation cannot stop at the three
literals above. And whether Claude Code's path is live in the CLI could
not be determined statically — see §7.

## 3. Protocol subset

Framing is `ESC _ G <control data> ; <payload> ESC \`, where control
data is comma-separated `key=value` pairs and the payload is base64.
Unlike VGE envelopes, the payload is **not** byte-stuffed.

| Key | Values needed | Meaning |
|---|---|---|
| `a` | `T`, `q`, `d` | transmit+display, query, delete |
| `t` | `d`, `f` | direct base64 payload, or base64-encoded file path |
| `f` | `100`, `24`, `32` | PNG, RGB, RGBA (`24`/`32` require `s=`/`v=`) |
| `s`, `v` | u32 | source pixel width/height, for raw formats |
| `m` | `0`, `1` | chunk continuation; `1` on all but the last chunk |
| `i` | u32 | image id, echoed in responses |
| `q` | `1`, `2` | suppress OK responses / suppress errors |
| `c`, `r` | u32 | explicit cell footprint; if one is given the other follows the aspect ratio |

Chunks are at most 4096 bytes, and every chunk except the last must have
a length that is a multiple of 4. Continuation chunks carry only `m` and
optionally `q`.

Placement semantics: the image is drawn at the cursor, and afterwards
the cursor moves right by the placement's column count and down by its
row count, scrolling the screen if that runs past the bottom, unless
`C=1` suppresses the movement.

Responses are `ESC _ G i=<id> ; OK ESC \` or `ESC _ G i=<id> ;
E<CODE>:<message> ESC \`, subject to `q`. The query action `a=q`
transmits a dummy image and expects that response and nothing else —
this is how clients detect support, so answering it is what makes veter
visible to them.

## 4. What veter already provides

The reason this is a small change:

- **PNG decoding is already a dependency.** `veter-host/Cargo.toml:30`
  pulls `image` with the `png` feature for VGE uploads.
- **The placement model already exists.** A kitty image is an image-table
  entry plus a cell rectangle anchored to a grid line that scrolls with
  content, is evicted with scrollback, and is suspended on the alternate
  screen. That is precisely `VgeState`. Expressing kitty images as VGE
  elements inherits anchoring (§5.2), eviction, alt-screen handling
  (§5.4), `vsd` snapshotting — and the renderer, since
  `veter/src/vge/render.rs` already draws image elements. Kitty's default
  `z=0` (above text) matches VGE's draw order.
- **Foreign APC already passes through.** The parser buffers three marker
  bytes and dispatches on them (`vge-protocol/src/apc.rs:200`); `Ga=`
  does not match `VGE`, so kitty escapes already reach passthrough
  untouched. A new engine slots into the chain without disturbing the
  others.
- **Response plumbing exists.** `queue_envelope` /
  `pending_response_bytes` (`veter-host/src/vge/state.rs:1440`),
  including the per-portal reply route — so `a=q` answers correctly
  inside a vmux pane, not just at the host level.
- **Portals come free.** Per-portal engines are constructed generically
  (`veter-host/src/prt/snapshot.rs:327`), so each pane gets its own
  kitty state exactly as it gets its own VGE state, and nested cases
  work by the same recursion as everything else.

## 5. What has to be written

1. **`ESC _ G` framing and control parser** (~150 lines): APC extraction,
   `key=value` splitting, unknown-key tolerance, and `m=1`/`m=0` chunk
   reassembly keyed by image id.
2. **base64 decoding** — not currently a dependency anywhere in the
   workspace. A small crate or ~40 lines.
3. **Command mapping**: `a=q` → `OK` response; `a=T` with `t=d`/`t=f`
   and `f=100`/`24`/`32` → decode → image-table entry plus an element at
   the cursor; `a=d` → element deletion. Cell footprint from
   `ceil(px / cell_px)` with `c=`/`r=` as override. Kitty images should
   live in a reserved element-id namespace, following the `host.*`
   precedent in VGE §7.3, so they cannot collide with client-created IDs.
4. **Cursor advance — the one structural piece.** Placement is at the
   cursor and the cursor must then move, which means the engine needs
   the cursor *at that point in the byte stream*. This requires the
   segment-aware final engine described in
   `doc/client-integration-groundwork.md` §5: the kitty engine sits last,
   immediately before the vt100, and returns `[Text, Image, Text]`
   segments so the caller can feed the vt100 up to the escape, place,
   then continue. The advance itself can be synthesised as `\n` × rows
   plus `ESC [ <cols> C`, which yields kitty's scroll-at-the-bottom
   behaviour for free. No global pipeline refactor; the wiring repeats
   once in the per-portal path.
5. **`t=f` file reading** with a size cap and a regular-file check.
   Note the exposure: a hostile stream can make the terminal read any
   file the user can read. Kitty carries the same exposure, but it should
   be a deliberate decision, and `t=f` is meaningless across SSH.

## 6. Non-goals for a first cut

Unicode placeholder placement (`U+10EEEE`), `z=` z-indexing, animation
frames, `t=s` shared-memory transport, `t=t` temp-file transport with
deletion, `a=p` separate placements of a transmitted image, virtual
placements, and `o=z` zlib-compressed payloads. `o=z` is worth
revisiting early if `icat` compatibility matters, since `icat`
compresses by default.

## 7. Open questions

**What gates the emitter.** Claude Code contains both an `a=q` probe and
a `ghostty`/`wezterm` name allowlist. If the probe is the gate, veter
answering it is sufficient. If the allowlist is, veter would have to
identify as one of those terminals for images to appear, which is
unattractive and worth knowing before any code is written. This could
not be settled by reading strings.

**Whether the emitter's own layout copes.** Claude Code sends no `c=`/`r=`
and no `C=1`, so it cannot know the image's row count — the terminal
decides. That is fine for static output, but a TUI that dead-reckons its
frame position cannot account for rows it does not know about; this is
the same exposure that broke the reservation experiment in
`doc/client-integration-groundwork.md`. A correct kitty implementation
may still yield images that the emitter's next repaint paints over.
Nothing veter does can fix that from its side.

**Whether the path is live at all.** The renderer takes an options
struct whose field names include `kittyGraphics`, and that literal
occurs exactly once in the whole binary — inside the Rust string table,
never on the JavaScript side. So either the native code enables it from
its own detection, or the path is dormant in the CLI and present for
another consumer. The only user-facing string about inline images points
at Claude Code Desktop.

## 8. Validation before implementation

All three questions in §7 are answerable in one experiment, with no
veter changes: spawn `claude -p` on a PTY we own with `TERM=xterm-kitty`,
answer the `a=q` probe with `ESC _ G i=31 ; OK ESC \`, ask for output
containing a markdown image, and record whether `ESC _ G` appears in the
output — and if it does, whether the emitter also emits newlines to
reserve the rows. Repeat with `TERM_PROGRAM=ghostty` if the first run is
silent, to separate the probe gate from the allowlist gate.

This is worth doing first. It is cheap, and a negative result redirects
the effort to `doc/client-integration-groundwork.md` §4 instead.

## 9. Effort and sequencing

Roughly two days for a working minimal implementation — probe response,
`t=d`/`t=f` PNG transmission, chunk reassembly, placement, cursor
advance, deletion — with most of the time in the segment-aware plumbing
and its tests rather than in protocol parsing.

Dependencies and synergies with `doc/client-integration-groundwork.md`:

- §5 (segment-aware final engine) is a **hard prerequisite**.
- §9 (APC payload caps) becomes materially more important, since kitty
  payloads are large and Claude Code sends them unchunked.
- §1 (winsize pixel dimensions) is **not** needed by Claude Code, which
  sends raw PNGs and lets the terminal do the arithmetic — but it *is*
  needed by `icat` and most other kitty clients, which size their output
  from `TIOCGWINSZ`. Without it, third-party kitty support is
  substantially less useful.
