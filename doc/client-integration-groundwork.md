# Client Integration Groundwork

> **Status: proposal — not normative.** Planning notes, unlike the
> protocol specs alongside them. Each item here is a small, independent
> change that makes veter easier to drive from a client that is *not*
> the foreground program of a pane. They were found while prototyping
> an out-of-band image renderer, but none of them depend on that
> prototype shipping, and several are prerequisites for
> `doc/kitty-graphics-support.md`.

## 0. Where this list came from

The driving experiment: a headless helper process (no controlling
terminal) tried to display an image in a pane whose foreground program
was a full-screen TUI it did not control. It worked — bytes written to
the pane's pts slave travel up through `vmux` as a PRT `WritePortal`
and reach veter's per-portal VGE engine — but every rough edge it hit
is a gap that any external tool would hit too.

Three findings shaped the list:

1. **Responses are unusable out-of-band.** Anything the terminal sends
   back (`ProbeResponse`, `ChunkAck`, a DSR cursor report) lands in the
   pane's *input* queue, where the foreground program is blocked in
   `read()`. Whoever the kernel wakes gets the bytes. VGE's
   `REQ_ID_NO_RESPONSE` sentinel (§1.2) makes silent state-push
   possible, but it means an out-of-band client can never *ask* the
   terminal anything. Everything such a client needs must therefore be
   available without a round-trip — items 1, 3 and 7.
2. **Envelopes are not safe through a cooked tty.** A pane's termios
   normally has `OPOST`/`ONLCR` on; a 431 KB raw-RGBA upload measured
   in testing contained 42 `0x0A` bytes, every one of which would have
   been rewritten to `0x0D 0x0A`. Item 2.
3. **Out-of-band writes must have zero cursor side effects.** Writes
   that only carry APC envelopes are invisible to the vt100 and caused
   no disturbance whatsoever. Writes that emitted newlines to reserve
   vertical space desynchronised the foreground TUI's renderer
   immediately: it dead-reckons its frame position ("my live region
   starts K rows above the cursor"), so scrolling underneath it strands
   the old frame on screen and misplaces the next one. **Space can only
   be reserved in-band, by the application itself.** The client's job
   is then reduced to naming a location, which is item 4.

## 1. Populate `ws_xpixel` / `ws_ypixel`

**Problem.** Cell pixel dimensions are currently obtainable only from a
VGE probe, i.e. only by a client that can read the terminal's reply.
`TIOCGWINSZ` reports zeros. xterm has reported real pixel dimensions
there for decades, and most third-party image clients (kitty's `icat`
among them) size their output from those fields.

**Sites.** `veter/src/pty.rs:62` (initial `forkpty` winsize) and `:155`
(resize); `tools/vmux/src/main.rs:754`; `tools/vmux/src/main.rs:3804`
(`get_host_winsize` returns only rows/cols and needs to carry pixels);
pane spawn/resize at `tools/vmux/src/main.rs:693` and `:750`; `vsd`'s
inner PTYs.

**Work.** `Pty::resize(rows, cols)` has to take cell dimensions; the App
already computes them at `veter/src/main.rs:2774`. vmux must derive cell
size from its own winsize and multiply back out *per pane* — panes are
not the host grid.

**Units.** Device pixels, matching what the probe already advertises in
`cell_pixel_width` / `cell_pixel_height` (`veter-host/src/vge/state.rs:861`)
alongside `scale_factor`. Letting the two sources disagree would give
clients a silent 2× error on HiDPI, which is worse than reporting
nothing.

**Caveats.** This introduces a resize trigger that does not exist today:
a font-size/DPI change alters cell pixels without changing rows/cols and
must still push `TIOCSWINSZ`. vmux already has scar tissue about
redundant winsize writes waking shells into spurious `PS1` redraws
(`tools/vmux/src/main.rs:2131`, `:2203`); the same guard applies.

**Optional follow-up.** Teach `vge-render/src/probe.rs` to try the ioctl
first and fall back to the probe, so `vcat`/`vplay`/`vfm`/`vdraw` start
without a round-trip.

## 2. Make envelopes survive a cooked tty

**Problem.** §1.3 stuffs `ESC`, `~`, DC1 and DC3 because envelopes get
relayed through channels that reinterpret them. A tty with output
post-processing is the same class of hazard and is not covered.

**Scope — wider than LF.** `ONLCR` (LF → CRLF) is only the default case.
`OCRNL`, `ONLRET` and `ONOCR` rewrite CR, and `TABDLY=XTABS` expands TAB
into spaces. The stuffing set should be `0x09`, `0x0A`, `0x0D` — three
new marks alongside the existing `T`/`Q`/`S`
(`vge-protocol/src/frame.rs:113-115`), each transport-clean and distinct
from `0x5C`.

**Sites.** Five implementations, not one: `vge`, `prt`, `vft`, `ses` and
`vss` each carry their own `codec.rs` stuff/unstuff plus an `apc.rs`
unstuffing state machine, and five copies of §1.3 in `doc/`. They must
move together — PRT `WritePortal` carries VGE bytes, VSS carries whole
snapshots. `vge-protocol/src/codec.rs:405`
(`stuffed_output_is_transport_clean`) is the test to extend in each
crate; a property test over random payloads is the right shape.

**Compatibility.** This changes bytes on the wire. An old receiver sees a
malformed envelope and drops it silently. `make install` keeps local
binaries in lockstep, but two cases do not: hosts refreshed by
`vssh`/`make install-remote-<arch>`, and — worse — a running `vsd`
daemon on an old binary that a new client attaches to. VSS versions its
*snapshots* (`snapshot_version`, `SnapshotRejected` reason 1, see
`doc/session-manager.md` §4), but a stuffing change breaks the envelope
layer *underneath* that, so the failure is a silent drop rather than a
clean rejection. Either bump `protocol_version` (still 0, explicitly
WIP) with a loud version check at attach, or advertise a capability bit
in the probe response and let new clients emit old-style stuffing until
they have seen it.

## 3. Terminal and pane discovery

**Problem.** `veter/src/pty.rs:71` sets only `TERM=xterm-256color` and
`COLORTERM=truecolor`. Nothing tells a process that it is running under
veter, and nothing tells it which pane it occupies. The prototype had to
walk `ps` ancestry to find its pane's pts, which breaks with nested
sessions or several clients in one window.

**Work.** Export `VETER=<version>` in veter's child environment, and
`VMUX_PANE=<id>` (optionally `VMUX_PANE_TTY`) when vmux spawns a pane
(`tools/vmux/src/main.rs:693`).

**Why it matters beyond convenience.** Combined with item 1, it gives an
out-of-band client everything it needs to decide *whether* to draw and
*where* to write, with no round-trip and no heuristics.

## 4. Anchoring without a round-trip

**Problem.** Element origins are viewport-relative at command-processing
time (§5.2). A client that cannot read a DSR reply has no way to place
anything relative to the text it cares about.

Finding 3 above constrains the design: the client must not create the
space itself. The workable division of labour is that the *application*
emits the blank rows as part of its own output — its renderer then
accounts for them and stays consistent — and the client only names
where they are.

**Two candidate mechanisms**, both fitting in the reserved bits of the
§9.4 `extra_flags` byte (bits 3..7 are free, so neither is a
compatibility break):

- `bit3` — **cursor-relative origin**: `anchor_line = cursor_row +
  floor(origin.y)`, negative `y` permitted. Simple, but the caller still
  has to know how far the cursor has drifted from the rows it means.
- `bit4` + `string marker` — **marker-anchored origin**: anchor to the
  most recent line of the live screen containing the given substring.
  The application prints a token on the first reserved row; the terminal,
  which owns the grid, resolves it. No cursor arithmetic and no
  assumptions about the application's live-region height. This is the
  better fit for the driving use case.

`UpdateOrigin` (§6.6) can take the same optional trailing flags byte,
decided by body length, exactly as `CreateElement` does today.

**Resolution timing.** Both need item 5. Precedent for the mechanism
already exists: DSR cursor queries are counted during a chunk and
answered from `after_vt100_process`, because the cursor is only correct
post-process (`veter-host/src/vge/state.rs:672`, called at `:708`).

## 5. Stream ordering: a segment-aware final engine

**Problem.** Every engine consumes a whole chunk's envelopes before any
of that chunk's text reaches the vt100
(`veter/src/main.rs:2581-2591`). Anything cursor-dependent therefore
resolves against the *pre-chunk* cursor, off by exactly the text that
arrived alongside it.

**Cheap fix.** Defer resolution to `after_vt100_process`, as DSR does.
Correct as long as nothing follows the command in the same read chunk —
and read boundaries are veter's, not the client's, so it is best-effort.

**Exact fix.** The engine positioned **last, immediately before the
vt100**, returns segments — `[Text, Command, Text]` — and the caller
feeds the vt100 up to the command, resolves, then continues. Only the
last engine needs this; the rest keep their current shape. The same
wiring repeats once in the per-portal `WritePortal` path.

Item 4 can ship with the cheap fix. `doc/kitty-graphics-support.md`
requires the exact one, since kitty placement is inherently
cursor-anchored and must also advance the cursor mid-stream.

## 6. Image-table lifetime versus element eviction

**Problem.** Elements are destroyed when their anchor line falls out of
scrollback (§5.2), and `ESC[2J`/`ESC[3J` drop them (§5.7) — but §5.7 is
explicit that the shared image and style tables are *not* affected. A
client that uploads one image per element and lets elements scroll away
leaks image-table slots until `max_images` is exhausted and uploads
start failing with `err_too_many_images`.

**Options.** Refcount images against the elements referencing them and
drop at zero; or add an explicit "drop when unreferenced" flag on
`UploadImage`; or leave the semantics alone and document that cleanup is
the client's responsibility, which in practice means content-addressed
image IDs so re-display reuses one slot.

## 7. Limits discovery for clients that cannot read replies

`max_image_bytes`, `max_images`, `supported_image_encodings` and
`max_nesting_depth` are advertised only in the probe response. Item 1
solves cell dimensions, but these have no non-round-trip source. Either
export them alongside item 3's environment variables, or state
explicitly that blind clients must assume the recommended defaults in
§11. Worth deciding deliberately rather than discovering at 4 MB.

## 8. `REQ_ID_NO_RESPONSE` beyond VGE

The state-push sentinel (§1.2) has no equivalent in the PRT, VFT or SES
specs. Image display works without one, but the first out-of-band client
that needs a PRT command will need it ported.

## 9. APC payload caps

The APC parsers carry no maximum-payload guard, so a malformed or
hostile stream can make the terminal buffer without bound. This is a
latent robustness hole today; it becomes more exposed under
`doc/kitty-graphics-support.md`, where clients legitimately send
multi-megabyte payloads and Claude Code sends them unchunked.

## Suggested order

1. Items 1 and 3 — small, independently useful, and unblock everything
   else.
2. Item 5 — shared prerequisite for items 4 and kitty support.
3. Item 6 and item 9 — correctness/robustness, cheap.
4. Item 4 — only if inline placement by an out-of-band client is still
   wanted after kitty support lands, since that may subsume the use case.
5. Item 2 last: it is the only change that breaks the wire.

## Appendix: experiment log

Run against veter + vmux, with the helper process having no controlling
terminal (`/dev/tty` → `ENXIO`) and writing directly to its pane's pts
slave.

| Experiment | Result |
|---|---|
| Upload + `CreateElement` with `request_id = 0xFFFFFFFF`, `OPOST` cleared for the write | Image rendered correctly. No disturbance to the foreground TUI. |
| Same, then `DeleteElement` + `DropImage` | Clean removal, no artefacts. Confirms envelope-only writes are inert to the vt100. |
| Reserve space by writing `2 × rows` newlines, then place at the top of the blanked screen | Image landed correctly, but the entire visible screen was scrolled into scrollback. |
| Reserve `H + composer` rows by writing newlines, place above the composer | **Failed.** Foreground TUI stranded its previous frame at the top of the screen and repainted the next one through the reserved rows; text was drawn into the gap and partly hidden behind the image, since VGE renders above the text layer. |

The last row is the finding that motivates item 4's marker design and
the in-band division of labour described there.
