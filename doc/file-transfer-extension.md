# File Transfer Extension (VFT)

> **Status: unstable WIP — v0.** The wire format may change in
> incompatible ways without notice. Clients and host implementations
> ship from this repo in lockstep. The version byte in every envelope
> is `0` and the probe response advertises `protocol_version = 0`;
> both bump to `1` once the format is declared stable.

This extension lets a client move file bytes between its own filesystem
and the host's: a CLI tool running inside the terminal can hand the host
a local file (`vsend`) or pull a host-side file back to the client side
(`vrecv`). Both directions ride the same APC-framed control channel as
PRT and VGE, so a single PTY carries display, input, and file traffic
without any extra connection.

The protocol is binary, command-batched, and bidirectional. Commands
flow client → host. Host → client carries two things on one wire:
**responses** (one per command) and **events** (unsolicited, e.g. the
chunks of a download streaming back to the requester).

The protocol is intentionally simplistic. It assumes a reliable,
in-order transport (TCP, local PTY, SSH channel) — there are no CRCs,
no compression, no resume. Files travel as a sequence of opaque
chunks, and acknowledgments are sent only when the client asks for
them. Sophisticated transfer features (rsync-style deltas, integrity
hashing, recursive trees) are out of scope for v1; see §13.

This extension is self-contained: a host can implement it without
implementing PRT or VGE. If the host *also* implements PRT, §10 spells
out how the two interact.

## 1. Wire format

### 1.1 Envelope

Every protocol message — both directions — rides inside an APC
sequence:

```
client → host:   ESC _ V F T <payload> ESC \
host   → client: ESC _ v f t <payload> ESC \
```

- `0x1B 0x5F` (`ESC _`) opens APC.
- The 3-byte marker `VFT` (uppercase) means *command from client to
  host*. The marker `vft` (lowercase) means *response or event from
  host to client*.
- `0x1B 0x5C` (`ESC \`) closes APC.

The case-difference between the two markers lets either side parse
without a direction flag, and the 3-byte marker as a whole lets a
host-side APC parser route VFT envelopes to this extension while
passing other APC sequences (PRT, VGE, iTerm-style `ESC _ G …`)
through to whatever else handles them.

A host that implements VFT MUST forward APC envelopes whose marker
is not `VFT`/`vft` verbatim to its downstream layer. This pass-through
rule is what lets a stack of nested hosts — for example a remote
`vsd` consuming PRT + VGE while a `vsend` running inside its
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
fields *inside* VFT frames (§6.2, §7.2): they are part of the payload
like any other field, so they get stuffed once at envelope-encode
time. There is no double-stuffing.

Worst-case overhead is 2× (a payload of all-`0x1B` bytes doubles
under stuffing). Real file content is rarely close to this; clients
that care about wire amplification SHOULD assume an average overhead
of a few percent and size their chunks accordingly.

### 1.4 Encoding primitives

| Type     | Encoding                                            |
|----------|-----------------------------------------------------|
| `u8`     | 1 byte                                              |
| `u16`    | 2 bytes, little-endian                              |
| `u32`    | 4 bytes, little-endian                              |
| `u64`    | 8 bytes, little-endian                              |
| `i64`    | 8 bytes, little-endian, two's complement            |
| `varu`   | LEB128 unsigned varint                              |
| `string` | `varu length` followed by `length` UTF-8 bytes      |
| `bytes`  | `varu length` followed by `length` raw bytes        |

Strings are not NUL-terminated. Empty strings encode as a single
`0x00`. File offsets and sizes are `u64` because file sizes routinely
exceed 4 GiB; everything else fits in `u32`.

Paths on the wire are UTF-8 strings. POSIX path bytes that are not
valid UTF-8 (rare in practice on user-facing systems) MUST be
escaped by the client into a representation the host can round-trip;
this spec does not define such an escape and treats the wire path as
opaque UTF-8 the host filesystem accepts. Hosts on platforms where
the native path encoding is not UTF-8 (e.g. Windows wide-chars,
classic POSIX latin-1) are responsible for converting at their
boundary.

## 2. Probe and capability discovery

### 2.1 Probe (frame_type 0x01)

Sent by the client first thing after enabling the extension. Body is
empty.

Host responds with `ProbeResponse` (§4):

```
u16  protocol_version          // highest version the host speaks
u32  max_concurrent_transfers  // soft cap; over-limit BeginX fails
u32  max_chunk_bytes           // largest single UploadChunk / DownloadChunk body
u32  max_path_bytes            // largest path string accepted in BeginUpload /
                               //   BeginDownload, in encoded UTF-8 bytes
u64  max_file_bytes            // largest single transfer the host will accept;
                               //   0 means no host-side limit
u8   features                  // bitmask, see below
```

`features` bitmask:

```
bit 0  upload                   // host accepts BeginUpload
bit 1  download                 // host accepts BeginDownload
bits 2..7  reserved (must be 0)
```

A host that advertises neither `upload` nor `download` is degenerate
and behaves like a host that does not implement the extension at
all; clients SHOULD treat it as such.

The deferred forms of upload and download (empty `host_path`, §6.1
and §7.1) are part of the base protocol; a host that cannot satisfy
a particular deferred request — typically because it is headless
and has no file picker, or has no notion of a default-handler
launcher — surfaces that at the moment the request arrives via
`err_picker_unavailable`. There is no separate feature bit for
either deferred form.

If the host does not support the extension, no response is emitted;
the client SHOULD time out (e.g. 250 ms) and abort the transfer with
a user-visible error.

A client MUST NOT send any other VFT command before receiving the
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
| 0x02 | BeginUpload        | §6.1         |
| 0x03 | UploadChunk        | §6.2         |
| 0x04 | EndUpload          | §6.3         |
| 0x05 | BeginDownload      | §7.1         |
| 0x06 | ReportDownloadAck  | §7.4         |
| 0x07 | RequestAck         | §8.1         |
| 0x08 | CancelTransfer     | §8.2         |

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
| 0x01 | Ok             | command-specific (often empty)               |
| 0x02 | Err            | `u16 error_code, string message`             |
| 0x03 | ProbeResponse  | as in §2.1                                   |

`error_code` values:

| Code   | Name                    | Meaning                                          |
|--------|-------------------------|--------------------------------------------------|
| 0x0001 | err_unknown_command     | Unknown frame_type                               |
| 0x0002 | err_bad_payload         | Frame body could not be parsed                   |
| 0x0003 | err_unsupported_version | protocol_version too new                         |
| 0x0010 | err_unknown_transfer    | transfer_id does not resolve                     |
| 0x0011 | err_duplicate_transfer  | transfer_id already in use                       |
| 0x0012 | err_too_many_transfers  | Concurrent-transfer budget exhausted             |
| 0x0013 | err_unsupported_dir     | Direction not advertised in features             |
| 0x0020 | err_chunk_too_large     | UploadChunk body exceeds max_chunk_bytes         |
| 0x0021 | err_chunk_offset        | UploadChunk offset does not match host's view    |
| 0x0022 | err_too_many_bytes      | Total bytes exceed declared total or limit       |
| 0x0030 | err_path_too_long       | host_path exceeds max_path_bytes                 |
| 0x0031 | err_path_invalid        | host_path syntactically invalid for this OS      |
| 0x0032 | err_path_denied         | Host policy refuses this path / direction        |
| 0x0033 | err_path_exists         | Upload target exists and overwrite = 0           |
| 0x0034 | err_path_missing        | Download source does not exist                   |
| 0x0040 | err_picker_unavailable  | Deferred form (empty path) cannot be satisfied   |
| 0x0041 | err_cancelled           | User dismissed picker / chose Cancel             |
| 0x0050 | err_io                  | Read or write failure on the host filesystem     |
| 0x0051 | err_disk_full           | Out of space partway through                     |
| 0x0052 | err_premature_end       | EndUpload before all declared bytes received     |
| 0x00FF | err_internal            | Host-side failure                                |

After an `Err` response the host's state is unchanged: failed commands
are atomic, with no partial side effects. In particular, a
`BeginUpload` that fails atomically reserves no transfer slot and does
not create a `transfer_id` table entry; an `UploadChunk` that fails
atomically does not advance the transfer's `bytes_received`.

The exception is mid-transfer I/O failure (`err_io`, `err_disk_full`,
`err_too_many_bytes`): once these are returned for an `UploadChunk` or
detected during a download, the host MUST also fire a
`TransferAborted` event (§8.3) for the affected `transfer_id` and
treat the transfer as ended. The client receives the per-command Err
*and* the abort event; both are necessary because subsequent chunks
already in flight need a clear signal to stop.

### 4.2 Event frames (0x80..0xFF)

Events are unsolicited, host-originated frames. `request_id` is `0`
and ignored. Event ordering is preserved per transfer; ordering across
transfers is not guaranteed.

| Code | Event              | Body section |
|------|--------------------|--------------|
| 0x80 | DownloadChunk      | §7.2         |
| 0x81 | DownloadEnd        | §7.3         |
| 0x82 | UploadAck          | §8.1         |
| 0x83 | TransferAborted    | §8.3         |

Unknown event codes received by a client MUST be ignored without
error so the protocol can grow.

## 5. Concepts

### 5.1 Transfer model

A **transfer** is one file moving in one direction. Its lifecycle:

1. **Open** — `BeginUpload` or `BeginDownload`. The host validates
   the path, allocates state, and either responds with metadata (Ok)
   or rejects (Err). On success the transfer is *active*.
2. **Stream** — bytes flow chunk by chunk. For uploads the client
   sends `UploadChunk` frames; for downloads the host emits
   `DownloadChunk` events.
3. **Close** — `EndUpload` (uploads) or `DownloadEnd` event
   (downloads) finalises and frees the `transfer_id`. Either side
   may cut the transfer short with `CancelTransfer` /
   `TransferAborted`.

Multiple transfers can be active concurrently within one session,
distinguished by `transfer_id` (§5.2). The host SHOULD interleave
chunks of different downloads fairly, but the spec does not require
any specific scheduling policy.

### 5.2 Transfer IDs

Every transfer carries a client-assigned `string transfer_id`.
Constraints:

- Non-empty; ≤ 64 bytes of UTF-8.
- Unique across all *currently active* transfers in this session.
  Once a transfer is closed (EndUpload acknowledged, DownloadEnd
  emitted, or TransferAborted fired), its ID is free for reuse.
- Opaque to the host beyond byte equality. Hosts MUST NOT parse
  structure out of a transfer_id.

`transfer_id` is scoped to one VFT engine instance: top-level
host-side transfers are independent of any per-portal VFT engine
running inside a PRT portal (§10).

There is no central registry of transfer IDs across sessions. Hosts
that survive the client's reconnect (e.g. a long-lived
multiplexer) MAY recycle IDs from before the disconnect.

### 5.3 Path semantics

Both `BeginUpload` and `BeginDownload` carry a `string host_path`.

- A non-empty `host_path` is the **explicit** form: the host MUST
  resolve and use that exact path (after standard tilde expansion
  and environment-variable substitution if the host so chooses; see
  below).
- An empty `host_path` is the **deferred** form: the host decides
  what to do, in a direction-specific way (§6.1, §7.1).

Tilde expansion (`~/`, `~user/`) and environment-variable
substitution (`$HOME`, `${VAR}`) are host policy. A host MAY apply
them, MAY pass paths through verbatim, or MAY refuse paths
containing such metacharacters with `err_path_invalid`. The
resolved absolute path is reported back to the client in the
matching `Ok` body (§6.1, §7.1) so the user can see exactly what
the host chose.

Relative paths are resolved against the host's current working
directory at the moment the command is processed. Clients that
want a stable base SHOULD pass absolute paths.

The client-side CLI tools (`vsend`, `vrecv`) use a `:` prefix to
distinguish a host-path argument from a local one; this is purely a
shell-UX convention. The wire protocol carries the path bytes
*after* stripping that prefix.

### 5.4 Host policy

The host enforces all filesystem policy. It MAY:

- Refuse uploads outside a configured "downloads" directory.
- Refuse downloads of paths outside the user's home directory.
- Refuse paths that contain `..` segments or symlinks pointing
  outside an allowlist.
- Cap total transfer bytes per session, per file, per minute, etc.
- Always pop a confirmation dialog before accepting a transfer.

Any policy violation surfaces as `err_path_denied` (path-driven) or
`err_too_many_bytes` (size-driven). The wire spec deliberately does
not enumerate policies; the reference implementation in this repo
SHOULD start with a permissive default (the user's full home
directory is reachable) and let the user tighten via host-side
configuration.

### 5.5 Acknowledgments

VFT does **not** ack on its own. By default:

- `UploadChunk` frames are fire-and-forget at the application
  layer. The host returns the standard per-command `Ok`/`Err`
  (§4.1) so the client knows the chunk parsed and was accepted into
  the transfer's byte stream, but it does **not** confirm that the
  bytes have been written to disk or fsync'd.
- `DownloadChunk` events are fire-and-forget too. The client
  receives them but is not required to acknowledge.

When the client wants confirmation, it sends `RequestAck` (§8.1).
The host replies with the current `bytes_received` and
`bytes_processed` for that transfer. "Processed" means the host has
durably handed the bytes to its filesystem (write returned; fsync is
implementation-defined but SHOULD be performed at `EndUpload`, not
per chunk).

For downloads, the client uses `ReportDownloadAck` (§7.4) to tell
the host how far receipt has progressed; this is informational
(typically for a host-side progress UI) and the host does not
otherwise need it. The client decides if and when to send these.

This design — no implicit acks, no ack window, polling on
demand — keeps the wire format minimal and matches what the
underlying transport (TCP / PTY) already provides for delivery.

### 5.6 Reset behavior

A full reset (RIS / `ESC c`) and soft reset (DECSTR / `ESC [ ! p`)
on the host abort every active transfer. For each one, the host
fires `TransferAborted { reason = host_reset }` (§8.3) on its way
down, releases all transfer state, and closes the destination /
source file descriptors. Clients SHOULD treat a reset as a
session-level event and not attempt to resume.

Switching the host's alternate screen (§5.4 in the PRT spec) does
**not** affect VFT state; transfers keep flowing across DECSET
1047/1049 toggles. File transfer is a session-level operation
orthogonal to the text-grid screen buffer.

`ESC [ 2 J` and `ESC [ 3 J` (erase display / erase scrollback) do
not affect VFT either. Unlike VGE elements and PRT portals, file
transfers carry no on-screen rendering, so the text-grid wipe rules
do not apply.

## 6. Upload (client → host)

### 6.1 BeginUpload (0x02)

Body:

```
string  transfer_id        ; non-empty, ≤ 64 UTF-8 bytes; unique among
                           ;   currently active transfers
string  host_path          ; "" = deferred (host chooses), see below
string  basename           ; filename hint used in deferred form;
                           ;   ignored when host_path is non-empty
u64     total_bytes        ; declared file size; 0 means unknown
u8      flags              ; bit 0 = overwrite (target may pre-exist)
                           ; bits 1..7 reserved (must be 0)
u32     mode               ; POSIX permission bits the client wants on
                           ;   the resulting file; 0 = host default
i64     mtime              ; modification time to stamp on the resulting
                           ;   file, in seconds since the UNIX epoch;
                           ;   0 = host default (typically "now")
```

Behavior:

- The host validates `transfer_id` (non-empty, not in use), the
  feature direction (§5.4), and `host_path` (length ≤
  `max_path_bytes`; passes host policy; resolves to a valid
  destination). Any failure → corresponding `err_*`, atomic.

- **Explicit form** (`host_path` non-empty). The host opens (or
  creates) the file at that path. If the path exists and `flags`
  bit 0 is `0` → `err_path_exists`. If the path exists and bit 0 is
  `1`, the host truncates it. The recommended pattern is to write
  to a sibling temporary (e.g. `<path>.vft-<random>`) and rename
  on `EndUpload`, so a partial transfer never replaces the
  destination. Whether to do so is host policy; the wire protocol
  does not mandate it.

- **Deferred form** (`host_path` empty). The host chooses a
  destination — typically a per-session directory under
  `${TMPDIR:-/tmp}` — and uses `basename` (or, if empty, an
  internally-generated name) as the file name. After `EndUpload`
  finalises the file, the host MAY trigger the user's default
  application for that file type (e.g. `xdg-open` on Linux,
  Launch Services on macOS). Whether the open happens, and which
  app is selected, is host policy. The deferred form lets clients
  hand a file to the user without knowing where to put it. A host
  that cannot satisfy the deferred form (headless, no writable
  scratch directory) → `err_picker_unavailable`.

- `total_bytes`: a hint and a contract. If non-zero, the host MAY
  pre-allocate disk space (`fallocate`, `SetEndOfFile`, or
  equivalent) and MUST reject any subsequent `UploadChunk` whose
  arrival would push the running total past `total_bytes` →
  `err_too_many_bytes`. `EndUpload` before reaching `total_bytes`
  → `err_premature_end`. If zero, neither check applies; the
  transfer is delimited solely by `EndUpload`.

- `mode`: the POSIX permission bits the client would like the
  resulting file to have, masked against the host's `umask` and
  policy. `0` means "use the host's default" (typically `0644`).
  Hosts MAY clamp or ignore the value to satisfy local policy;
  the spec does not require the resulting file to match `mode`
  exactly.

- `mtime`: the modification time to stamp on the resulting file,
  in seconds since the UNIX epoch. `0` means "use the host's
  default", which SHOULD be the wall-clock time at `EndUpload`.
  Hosts MAY clamp or ignore the value (e.g. on filesystems that
  do not support arbitrary mtimes).

Response: `Ok` with body

```
string  resolved_path      ; absolute path the host will write to
                           ;   (or has chosen for deferred form)
```

The body length is the source of truth; future revisions may add
trailing fields and clients MUST tolerate longer bodies (§2.1).

A client MAY pipeline `BeginUpload` and the first one or more
`UploadChunk` frames in the same envelope without waiting for the
response, since `transfer_id` is client-picked. If `BeginUpload`
fails, every queued `UploadChunk` for that ID also fails atomically
with `err_unknown_transfer`. The reverse (chunks arriving before
the begin frame in the same envelope) is a parse error since
frames within an envelope are processed in order.

### 6.2 UploadChunk (0x03)

Body:

```
string  transfer_id
u64     offset             ; absolute byte offset within the transferred file
bytes   data
```

The host appends `data` at `offset` to the destination file. The
host MUST reject:

- `data.len() > max_chunk_bytes` → `err_chunk_too_large`.
- `offset != bytes_received` (the host's running cursor for this
  transfer) → `err_chunk_offset`. Out-of-order or duplicate chunks
  are not permitted in v1; the offset field exists so that the
  client and host can sanity-check each other's view.
- `offset + data.len() > total_bytes` (when `total_bytes` was
  declared non-zero in `BeginUpload`) → `err_too_many_bytes`.
- An I/O failure on `write(2)` (or equivalent) → `err_io`, plus a
  `TransferAborted { reason = io_error }` event that closes the
  transfer.
- Out of disk → `err_disk_full`, plus the same abort.

On success the host advances `bytes_received` by `data.len()` and
returns an empty `Ok`. The Ok confirms the chunk parsed and was
accepted into the transfer's byte stream; durable-write
confirmation is available separately via `RequestAck` (§8.1).

Multiple `UploadChunk` frames may share an envelope; the host
processes them in order. A client that wants a periodic
durability checkpoint inserts a `RequestAck` between chunks at
whatever cadence it likes — that is the v1 mechanism for an
ack-window.

### 6.3 EndUpload (0x04)

Body:

```
string  transfer_id
```

Finalises the upload. The host:

1. Verifies `bytes_received == total_bytes` if `total_bytes` was
   declared. Otherwise → `err_premature_end`.
2. Flushes any buffered writes and (SHOULD) fsync's the
   destination.
3. If the host implemented temp-file + rename for the explicit
   form (§6.1), renames into place now.
4. Stamps `mode` and `mtime` from `BeginUpload` if applicable.
5. For deferred form, optionally launches the user's default
   handler for the file type.
6. Releases the `transfer_id`.

Response: `Ok` with body

```
string  final_path         ; absolute path of the resulting file
u64     bytes_written      ; should equal total_bytes (or bytes_received
                           ;   when total_bytes was unknown)
```

After `EndUpload`'s `Ok` response, the `transfer_id` is free; any
further command that references it → `err_unknown_transfer`.

If the host's finalisation fails (rename collision, fsync error,
chmod failure), it returns `Err` and fires `TransferAborted`. The
partially-written file is the host's mess to clean up; the client
SHOULD assume nothing about whether anything landed.

## 7. Download (host → client)

### 7.1 BeginDownload (0x05)

Body:

```
string  transfer_id        ; non-empty, ≤ 64 UTF-8 bytes; unique among
                           ;   currently active transfers
string  host_path          ; "" = host-side file picker, see below
u32     chunk_size_hint    ; preferred DownloadChunk body size, in bytes;
                           ;   0 = host chooses
```

Behavior:

- Validates `transfer_id`, the direction feature, the path, and
  host policy as for §6.1.

- **Explicit form** (`host_path` non-empty). The host opens the
  named file for reading. Missing file → `err_path_missing`.
  Permission denied → `err_path_denied`. Path validation failures
  → the relevant `err_path_*`.

- **Deferred form** (`host_path` empty). The host opens a file
  picker dialog, blocks the response on user input, and returns
  either the chosen file (Ok) or `err_cancelled` (user dismissed
  the dialog). The picker is host UX; this spec does not mandate
  modality, default directory, filename filtering, or styling.
  A host that cannot show a picker (headless, or running under a
  desktop session that has no file-chooser portal) →
  `err_picker_unavailable`.

- `chunk_size_hint`: a request, not a contract. The host SHOULD
  size its `DownloadChunk` events near this value but is free to
  use anything ≤ `max_chunk_bytes`. A `chunk_size_hint` of `0`
  means the client has no preference.

Response: `Ok` with body

```
string  resolved_path      ; absolute path the host will read from
                           ;   (the picked file in the deferred form)
u64     total_bytes        ; the file's size at open time
u32     mode               ; POSIX permission bits as the host sees them;
                           ;   0 if the host doesn't expose them
i64     mtime              ; modification time, seconds since UNIX epoch;
                           ;   0 if the host doesn't expose it
```

The body length is the source of truth (§2.1).

After the host returns `Ok`, it begins streaming `DownloadChunk`
events (§7.2) for that `transfer_id`. The first chunk MAY be in
the same response envelope as the `Ok` frame; clients MUST
tolerate that.

### 7.2 DownloadChunk event (0x80)

Body:

```
string  transfer_id
u64     offset             ; absolute byte offset within the source file
bytes   data
```

`data.len()` MUST be ≤ `max_chunk_bytes` and SHOULD be near
`chunk_size_hint`. `offset` increases monotonically per
transfer; v1 forbids out-of-order or duplicate chunks (§5.5).

### 7.3 DownloadEnd event (0x81)

Body:

```
string  transfer_id
u64     bytes_sent         ; total bytes emitted; should equal total_bytes
```

Emitted exactly once per download, after the last
`DownloadChunk`. Signals successful completion. After this event
the `transfer_id` is free; commands referencing it → `err_unknown_transfer`.

If the host fails partway through (read error, file truncated by
another writer, host policy revoked) it does **not** emit
`DownloadEnd`; instead it fires `TransferAborted` (§8.3) and
closes the transfer.

### 7.4 ReportDownloadAck (0x06)

Body:

```
string  transfer_id
u64     bytes_confirmed    ; cumulative bytes the client has received
                           ;   and (optionally) processed
```

A purely informational note from client to host. The host MAY use
it to drive a host-side progress UI ("downloading 42 MiB / 60
MiB"), or ignore it entirely. There is no flow-control implication;
the host does not pause sending if the client falls behind.
Standard transport-level backpressure (a full PTY buffer) is the
only mechanism that throttles the host.

`bytes_confirmed` SHOULD be monotonically non-decreasing within a
transfer. Hosts MAY clamp out-of-range values rather than erroring.

Response: empty `Ok`. Unknown transfer → `err_unknown_transfer`.

## 8. Acks, polling, cancellation

### 8.1 RequestAck (0x07) and the UploadAck event

`RequestAck` body:

```
string  transfer_id
```

Asks the host for the current state of an *upload* transfer. The
host responds with `Ok` *and* fires `UploadAck` once the response
has been queued. The split exists so that the per-command response
ordering (§1.2) stays consistent with all other commands while the
useful payload still rides an event:

`UploadAck` event body:

```
string  transfer_id
u64     bytes_received     ; bytes the host has parsed from UploadChunks
u64     bytes_processed    ; bytes the host has durably written to disk
```

`bytes_received >= bytes_processed`. After `EndUpload` succeeds,
both values equal the final file size; calling `RequestAck` on a
closed transfer yields `err_unknown_transfer`.

`UploadAck` is the **only** event the host emits for uploads, and
*only* in response to a `RequestAck` (§5.5).

`RequestAck` is meaningful only for uploads. Calling it on a
download → `err_bad_payload` (the client already knows what it
received; for host-side progress reporting, see
`ReportDownloadAck`, §7.4).

### 8.2 CancelTransfer (0x08)

Body:

```
string  transfer_id
```

Aborts the named transfer immediately. The host:

- For uploads: stops accepting further `UploadChunk` for this ID,
  closes (and ideally deletes) the partial destination file, and
  fires `TransferAborted { reason = client_cancel }`.
- For downloads: stops emitting `DownloadChunk` for this ID, closes
  the source file descriptor, and fires `TransferAborted { reason = client_cancel }`.

Response: empty `Ok`. Unknown transfer → `err_unknown_transfer`.

After the response, the `transfer_id` is free for reuse. Any
chunks for this transfer that were already in flight before the
client sent `CancelTransfer` are processed normally; the client
discards them on its end based on the ID being cancelled.

### 8.3 TransferAborted event (0x83)

Body:

```
string  transfer_id
u8      reason             ; 0 = client_cancel
                           ; 1 = host_cancel        (host-side policy / UI cancel)
                           ; 2 = io_error           (read or write failed)
                           ; 3 = disk_full
                           ; 4 = host_reset         (RIS / DECSTR)
                           ; 5 = path_revoked       (file unlinked or chmod'd
                           ;                          mid-transfer)
                           ; 6 = limit_exceeded     (byte / time cap hit)
string  message            ; UTF-8, may be empty; human-readable detail
```

Fired exactly once per transfer if the transfer ends abnormally
(i.e. without a successful `EndUpload` Ok or `DownloadEnd` event).
Releases the `transfer_id`.

`TransferAborted` is the host's authoritative "this transfer is
gone" signal. Clients SHOULD treat it as terminal: stop sending
further chunks, close any local destination file, surface the
`message` to the user.

## 9. Concurrency and ordering guarantees

- **Per transfer**: command ordering is FIFO. `UploadChunk` frames
  applied to one transfer arrive in send order; `DownloadChunk`
  events for one transfer arrive in send order. Out-of-order or
  duplicate chunks are forbidden in v1 (§5.5).
- **Across transfers**: no ordering guarantee. The host may
  interleave `DownloadChunk` events from two concurrent downloads
  in any order, and the client may interleave `UploadChunk` frames
  to two concurrent uploads.
- **Command vs event**: events generated *while* processing a
  command appear after that command's response (§1.2). Events
  generated *outside* command processing (e.g. a
  `TransferAborted` fired because the host filesystem returned
  an asynchronous I/O error between commands) may appear at any
  time relative to other commands' responses, subject only to the
  per-transfer FIFO rule above.

A host implementation processing chunks under flow control MUST
NOT block command parsing on filesystem I/O — otherwise a slow
disk would stall probe responses and unrelated transfers. The
recommended pattern is one I/O worker per active transfer; the
parser thread enqueues chunks and never waits.

## 10. Integration with PRT

This section is **optional**: a host that does not implement PRT
can ignore it entirely, and a client that does not care about
running file transfers from inside a portal can ignore the
behaviour described here. VFT is fully functional without PRT.

A host that runs a per-portal VFT engine lets an inner program
speak VFT inside its portal exactly as a top-level client does.
There is no separate capability bit in the PRT probe response —
the inner program discovers per-portal support by sending a
regular VFT `Probe` (§2.1) and either receiving a probe response
or timing out, the same handshake a top-level client uses. A
host that supports VFT only at top level simply lets the inner
probe time out: the probe envelope is swallowed by the
per-portal vt100 (it never reaches the host's top-level VFT
engine, because PRT already extracted the byte stream at the
portal scope) and the inner program falls back to non-VFT
operation.

**VFT inside a portal.** When per-portal VFT is implemented:

- `VFT` envelopes inside the portal byte stream are extracted by
  the per-portal APC parser and routed to the per-portal VFT
  engine, exactly as the host does at top level.
- VFT responses and events generated inside a portal join that
  portal's `RawReply` stream — they look like ordinary bytes to
  the inner program.
- The per-portal VFT engine has its own `transfer_id` table,
  scoped to that portal. A program inside the portal cannot
  address a transfer started by the host (or by a sibling
  portal).
- The per-portal engine acts on the **host's** filesystem, with
  the host's user permissions. There is no portal-level
  sandbox at the protocol layer; if the host wants to confine
  inner programs, it MUST do so via OS-level mechanisms
  (containers, user accounts, mandatory access control). VFT
  does not pretend to be a security boundary.
- Reset / erase-display rules from the PRT spec do not apply
  (§5.6). Portal scope reset (`ESC c` inside the portal) MUST
  abort every transfer in that portal's VFT engine via
  `TransferAborted { reason = host_reset }`.

**File picker / default-save in nested setups.** A deferred-form
`BeginUpload` or `BeginDownload` issued *inside* a portal
triggers the host's file picker / default-app handler — there is
one user, one desktop session, one set of dialogs. The picker is
not multiplexed across portals; if two portals open pickers at
the same time, the host serialises them in some
implementation-defined order.

VFT and PRT together let a multiplexer (e.g. `vmux`) host
multiple shells where each pane independently runs `vsend` /
`vrecv` against the host's filesystem without any extra
plumbing.

## 11. Limits and budgeting

The host advertises hard caps via the probe response. Over-limit
ops fail atomically. A non-exhaustive list:

- `max_concurrent_transfers`: per host (and per per-portal VFT
  engine separately, when the host runs them).
- `max_chunk_bytes`: per `UploadChunk` / `DownloadChunk` body.
- `max_path_bytes`: per `host_path` field.
- `max_file_bytes`: per single transfer; 0 = no host limit.

The reference implementation in this repo SHOULD start with: 8
concurrent transfers, 4 MiB per chunk, 4096 bytes per path, no
per-file size cap. These numbers can be tuned without breaking the
protocol.

Memory cost is dominated by per-chunk buffer space; clients with
many concurrent transfers SHOULD pick smaller chunks. A host that
buffers chunks before flushing to disk SHOULD apply transport-level
backpressure (stop reading from the PTY) once a configured high-water
mark is reached, rather than refusing chunks at the protocol level.

## 12. Cookbook

### 12.1 vsend with explicit host path

```
$ vsend ./report.pdf :~/Documents/report.pdf
```

```
client → host (one envelope, three frames):
  BeginUpload {
    transfer_id  = "vsend-1",
    host_path    = "~/Documents/report.pdf",
    basename     = "",
    total_bytes  = 134217,
    flags        = 0,
    mode         = 0o644,
    mtime        = 1746796800,
  }
  UploadChunk { transfer_id="vsend-1", offset=0,    data=<256 KiB> }
  UploadChunk { transfer_id="vsend-1", offset=2^18, data=<256 KiB> }
  …
  EndUpload   { transfer_id="vsend-1" }

host → client:
  Ok (BeginUpload) { resolved_path = "/home/user/Documents/report.pdf" }
  Ok (UploadChunk) … repeated …
  Ok (EndUpload)   { final_path    = "/home/user/Documents/report.pdf",
                     bytes_written = 134217 }
```

### 12.2 vsend without host path (auto-open)

```
$ vsend ./screenshot.png
```

The client uses the deferred form (empty `host_path`). After
`EndUpload` succeeds, the host opens the saved file with the
system's default image viewer.

```
BeginUpload {
  transfer_id = "vsend-1",
  host_path   = "",
  basename    = "screenshot.png",
  total_bytes = 91234,
  flags       = 0,
  mode        = 0,
  mtime       = 0,
}

Ok (BeginUpload) { resolved_path = "/tmp/veter-uploads-XXXX/screenshot.png" }
… chunks …
EndUpload   { transfer_id = "vsend-1" }
Ok (EndUpload) { final_path    = "/tmp/veter-uploads-XXXX/screenshot.png",
                 bytes_written = 91234 }
; host now spawns `xdg-open /tmp/veter-uploads-XXXX/screenshot.png`
```

The client prints the resolved path so the user knows where the
file landed and why their image viewer just popped up.

### 12.3 vrecv with explicit host path

```
$ vrecv :/var/log/syslog ./syslog.txt
```

```
BeginDownload { transfer_id="vrecv-1", host_path="/var/log/syslog",
                chunk_size_hint=262144 }

Ok (BeginDownload) { resolved_path="/var/log/syslog",
                     total_bytes=8421376,
                     mode=0o640,
                     mtime=1746796800 }
DownloadChunk { transfer_id="vrecv-1", offset=0,      data=<256 KiB> }
DownloadChunk { transfer_id="vrecv-1", offset=262144, data=<256 KiB> }
…
DownloadEnd   { transfer_id="vrecv-1", bytes_sent=8421376 }
```

The client writes each chunk to `./syslog.txt` at the chunk's
`offset` (since v1 forbids out-of-order chunks, sequential
appends are fine). When `DownloadEnd` arrives, the client
closes the file and exits.

### 12.4 vrecv with the host file picker

```
$ vrecv ./from-host.bin
```

```
BeginDownload { transfer_id="vrecv-1", host_path="", chunk_size_hint=0 }

; (host blocks on a GTK / Cocoa / Win32 file dialog)

Ok (BeginDownload) { resolved_path="/home/user/Pictures/foo.png",
                     total_bytes=2345678,
                     mode=0o644,
                     mtime=1746796800 }
DownloadChunk … DownloadEnd
```

If the user dismisses the dialog:

```
Err (BeginDownload) { error_code=0x0041 err_cancelled, message="user cancelled picker" }
```

`vrecv` exits with a non-zero status and an error message;
nothing is written to `./from-host.bin`.

### 12.5 Two concurrent transfers

```
$ vsend ./video.mp4 :~/big.mp4 &
$ vrecv :~/another.tar.gz ./tarball.tar.gz
```

Each tool picks a unique `transfer_id` (e.g. `"vsend-1"` and
`"vrecv-1"`). Their chunks interleave on the PTY in whatever order
the host happens to schedule them; the client tools demultiplex by
ID. Progress reporting on either side is independent.

### 12.6 Polling upload progress

A long upload's UI wants to show a progress bar even though no
chunk requested an ack:

```
; once per second:
RequestAck { transfer_id = "vsend-1" }

Ok (RequestAck)
UploadAck { transfer_id="vsend-1",
            bytes_received=33554432,
            bytes_processed=29360128 }
```

The host's response and event together let the UI advance two
counters: bytes accepted into the protocol stream vs. bytes
durably on disk. A refusal to fsync per chunk (§5.5) means the
two values normally differ; on `EndUpload` they converge.

### 12.7 Cancel mid-transfer

User hits `Ctrl-C` halfway through a large upload:

```
CancelTransfer { transfer_id = "vsend-1" }

Ok (CancelTransfer)
TransferAborted { transfer_id="vsend-1", reason=0 client_cancel, message="" }
```

The host deletes (or leaves, per host policy) the partial
destination file. Future `UploadChunk { transfer_id="vsend-1" }`
frames already in flight error with `err_unknown_transfer`; the
client discards them.

## 13. Open issues / future work

These are intentionally deferred and are not part of v1:

- **Resume / restart of an interrupted transfer.** The protocol
  has the offset field needed for it, but no `ResumeUpload` /
  `ResumeDownload` op and no notion of a transfer surviving a
  session disconnect. v1 transfers are session-scoped and
  one-shot.
- **Integrity hashes.** No CRC, no SHA, no Merkle tree. TCP /
  PTY are assumed reliable; if they aren't, the user finds out by
  the file being corrupt. A future `BeginUpload` extension could
  carry an optional whole-file hash that the host verifies at
  `EndUpload`, and a `TransferAborted { reason = hash_mismatch }`.
- **Compression.** The byte-stuffed APC envelope is already
  binary, and adding gzip / zstd costs CPU and complexity on both
  ends. Most file types worth transferring (images, archives,
  binaries) are already compressed. Deferred.
- **Out-of-order / parallel chunk delivery.** The PTY is a single
  ordered stream, so a v1 transfer is sequential. Splitting a
  transfer across multiple TCP connections — for higher throughput
  on long-fat networks — is out of scope; VFT is intended for
  local PTY and SSH-channel use.
- **Recursive directory transfers.** A `BeginUploadDir` /
  `BeginDownloadDir` would carry a tree of files. The shape of
  that op (how to enumerate children, how to handle name
  collisions, whether to preserve links / xattrs / acls) is large
  enough to deserve its own design pass.
- **Glob / wildcard patterns.** Today `host_path` is a single
  literal path. A future op could accept a glob and stream back
  all matches, but the security boundary (does the host expand
  globs the client supplied?) needs care.
- **Capability negotiation / per-transfer policy.** v1's host
  policy is host-global. A future profile could let the client
  request per-transfer policy hints ("save into Downloads, not
  /tmp", "do not auto-open"), with the host free to ignore.
- **Cross-extension addressing.** A VGE element could receive a
  rendered image straight from a download buffer ("here's the
  PNG, draw it inline") without the bytes ever touching disk.
  Useful for `vcat`-style clients but couples extensions in ways
  that need separate scoping rules.
- **Chunk-level out-of-band progress.** Today a host-side progress
  UI for a download relies on `ReportDownloadAck` from the client.
  A future event could let the host stream its own
  bytes-on-the-wire counter independently for transports where
  the host is the natural progress source.
