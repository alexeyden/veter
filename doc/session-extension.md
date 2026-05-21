# Veter Session Extension (SES)

> **Status: WIP.** Implemented. The companion to `doc/session-manager.md`
> (`veterd`): the small control channel a multiplexer client (`vmux`)
> uses to discover the session it lives in and to detach it. Wire-format
> crate `ses-protocol`; host engine `veter-host`'s `SesEngine`.

SES is an APC-framed protocol, a sibling of PRT (`doc/portal-extension.md`),
VGE (`doc/vector-graphics-extension.md`), VFT (`doc/file-transfer-extension.md`)
and VSS (`doc/session-manager.md` §4). It carries two things between a
multiplexer client and its **immediate host**:

- the **name of the `veterd` session** the client is running inside, so
  the client can show it (e.g. in a tab bar), and
- a **detach** command, so the client can offer a key binding that
  detaches the session — the protocol-level equivalent of `veterd`'s
  `Ctrl+\ d` hotkey (`doc/session-manager.md` §6).

## 1. Why a separate extension

`doc/session-manager.md` §10 earmarked this as a deferred companion
rather than folding it into PRT. Session identity and detach are
*session-control*, not *portal-control*: a portal is a sub-grid, and
"detach the whole session" has nothing to do with any one portal. A
separate marker keeps PRT's surface about portals and lets session
control evolve independently.

The split also gives a clean answer for a non-session host: local
`veter` answers a SES probe with `in_session = false` (a fast,
definitive "no session"), so a client never has to distinguish "host
does not speak SES" from "host is not a session" via a timeout.

## 2. Wire format

Identical to PRT §1.1–1.4 (envelope, ESC byte-stuffing, payload
framing, encoding primitives), with these markers:

- client → host: `ESC _ S E S <payload> ESC \`
- host → client: `ESC _ s e s <payload> ESC \`

Payload framing (PRT §1.2): `u8 protocol_version`, `u32 payload_length`,
then frames of `u8 frame_type`, `u32 request_id`, `u32 body_length`,
`body[body_length]`. `protocol_version` is `0` (unstable WIP).

Unlike VSS, SES **uses `request_id`**: a command carries a
client-chosen id and its response echoes it, so pipelined commands can
be correlated. Events do not exist in SES v0 (see §6).

A host that does not implement SES emits no response; the client
SHOULD time out (250–500 ms, shared with its PRT/VGE probe window) and
treat the host as "no session".

## 3. Probe and capability discovery

### Probe (frame_type 0x01)

Empty body. The client sends this once, alongside its PRT/VGE probes.

### ProbeResponse (frame_type 0x03)

```
u8     protocol_version
u8     features          ; reserved capability bitmask, 0 in v0
u8     in_session        ; 0 = not a session, 1 = a veterd session
string name              ; session name when in_session, else empty
```

`in_session = 1` only for the top-level engine of a `veterd --session`
process; local `veter` and every per-portal scope (§5) answer `0` with
an empty `name`. The body length is the source of truth: a client
reading a shorter body treats missing trailing fields as zero.

## 4. Commands (client → host)

| Code | Command | Body  | Response          |
|------|---------|-------|-------------------|
| 0x01 | Probe   | empty | ProbeResponse / — |
| 0x02 | Detach  | empty | Ok / Err          |

An unknown command code yields `Err { ERR_UNKNOWN_COMMAND }`.

## 5. Responses (host → client)

| Code | Response      | Body                                              |
|------|---------------|---------------------------------------------------|
| 0x01 | Ok            | empty                                             |
| 0x02 | Err           | `u16 code`, `string msg`                          |
| 0x03 | ProbeResponse | see §3                                            |

Error codes:

| Code   | Name                | Meaning                              |
|--------|---------------------|--------------------------------------|
| 0x0001 | ERR_UNKNOWN_COMMAND | command frame_type not recognised    |
| 0x0002 | ERR_BAD_PAYLOAD     | malformed command body               |
| 0x0010 | ERR_NOT_IN_SESSION  | `Detach` sent to a non-session host  |
| 0x00FF | ERR_INTERNAL        | host-side failure                    |

Every command produces exactly one response, with the command's
`request_id` echoed.

## 6. Detach semantics

`Detach` asks the host to end the current attach. On a session host it
triggers the **same teardown** as `veterd`'s `Ctrl+\ d` hotkey
(`doc/session-manager.md` §6): the renderer's stdio is released, the
inner PTY and all engine state are kept, and the session keeps running
detached. The host replies `Ok`.

`Detach` on a non-session host (local `veter`, or a per-portal scope)
is refused with `Err { ERR_NOT_IN_SESSION }`. A well-behaved client
only sends `Detach` after a `ProbeResponse` with `in_session = 1`, so
this is a backstop, not a normal path.

Detach is **fire-and-forget**: the client need not wait for `Ok`
before considering the request issued. Detaching while no renderer is
attached is a no-op.

There are no unsolicited SES events in v0. The session name is fixed
for the lifetime of the client process (renaming a live session is out
of scope), so a single probe at startup suffices; if a future revision
adds live session renaming it will introduce an event for it, and
unknown event codes (0x80..0xFF) MUST be ignored by clients so that
revision does not break older ones.

## 7. Pass-through rule

As in PRT §1.1: a SES engine consumes only `SES` / `ses` envelopes and
passes every other byte — plain text, CSI sequences, and foreign APC
envelopes (`PRT`, `VGE`, `VFT`, `VSS`, …) — through verbatim.

## 8. Recursion and nesting

SES is consumed by the client's **immediate host** and is **never
forwarded** upward (unlike VFT, which must reach the local machine).
Each host scope runs its own `SesEngine`:

- A `veterd --session` process runs a session engine at its top level
  (the inner program — typically `vmux` — is its SES client).
- Local `veter` runs a non-session engine at its top level.
- Every PRT portal owns a per-portal engine. A portal is an inner
  program of *this* host and is not itself session-named, so per-portal
  engines always answer `in_session = false`. They exist only so that a
  nested client's SES envelopes are consumed (and answered) at the
  right scope rather than leaking to a vt100.

Consequently a `vmux` running as the top-level inner program of a
`veterd` session sees the session name; a `vmux` running inside a pane
of another `vmux` sees `in_session = false`.

## 9. Relationship to the other extensions

- `doc/portal-extension.md` (PRT) — unchanged. SES does not carry
  display direction; PRT remains the per-portal channel. The companion
  per-portal *activity* signal (`PortalActivity`, PRT §8.10) is a PRT
  event, not a SES one, because activity is per-portal and must work in
  a local `veter` host that has no session.
- `doc/vector-graphics-extension.md` (VGE) — unchanged.
- `doc/session-manager.md` (`veterd`, VSS) — SES is its vmux-facing
  control channel; VSS remains the renderer-facing snapshot channel.
