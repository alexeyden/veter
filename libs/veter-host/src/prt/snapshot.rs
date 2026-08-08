//! Binary state snapshot for [`super::state::PrtEngine`] (VSS
//! extension's `PrtFragment` payload). Recursive: each portal's
//! payload bundles its own vt100, per-portal VGE, and sub-engine PRT
//! snapshots, plus its `PolledStateCache` and pending DSR counter.
//!
//! VFT engines are deliberately not serialized — in-flight transfers
//! are abandoned on reattach. Engine policy state (`Limits`, the VFT
//! wakeup closure, the `cell_px` / `scale_factor` metrics needed to
//! spin up portal sub-engines) is inherited from the receiving
//! engine, not from the snapshot.
//!
//! See `doc/session-manager.md` §4.3 for the protocol-level role.

use vge_protocol::codec::{Reader, Writer};

use super::portal::{
    PolledStateCache, Portal, PortalAnchor, PortalContent, PortalSet,
};
use super::state::{FocusKind, PrtEngine, PrtState};
use prt_protocol::command::CursorStyle;

/// Bumped on any breaking change to the binary layout below. Strict
/// match — see [`PrtEngine::restore_from_binary_snapshot`].
///
/// History:
/// - v4: split each portal into a view and a shared `PortalContent`.
///   Buffers are serialized once per set and views carry a content id,
///   so a forked pair (§6.9) comes back still sharing one grid instead
///   of silently unsharing into two divergent ones.
/// - v3: drop `top_of_live_screen` — the line origin
///   `PortalAnchor::Scrollback { anchor_line }` values reference now
///   lives in (and is restored by) the vt100 snapshot that always
///   accompanies this fragment.
/// - v2: include `top_of_live_screen` so portal
///   `PortalAnchor::Scrollback { anchor_line }` values stay aligned
///   with the receiving engine's line tracker.
/// - v1: initial layout.
pub(crate) const SNAPSHOT_KIND_VERSION: u16 = 4;

/// Error returned when a PRT binary snapshot cannot be decoded.
#[derive(Debug, Clone)]
pub enum SnapshotError {
    /// The leading `u16 kind_version` did not match
    /// [`SNAPSHOT_KIND_VERSION`].
    KindVersionMismatch { got: u16, want: u16 },
    /// A typed primitive could not be decoded (truncated, malformed).
    BadPayload,
    /// All structured frames decoded but extra bytes remain at the
    /// payload tail.
    TrailingBytes,
    /// A nested vt100 sub-snapshot failed to decode.
    Vt100(vt100::SnapshotError),
    /// A nested VGE sub-snapshot failed to decode.
    Vge(crate::vge::SnapshotError),
    /// Snapshot disagrees with itself (e.g. `on_alt = true` but no
    /// alt portal set).
    Inconsistent,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KindVersionMismatch { got, want } => {
                write!(f, "PRT snapshot kind version mismatch: got {got}, want {want}")
            }
            Self::BadPayload => f.write_str("PRT snapshot payload malformed or truncated"),
            Self::TrailingBytes => f.write_str("PRT snapshot has trailing bytes"),
            Self::Vt100(e) => write!(f, "PRT snapshot: vt100 sub-payload: {e}"),
            Self::Vge(e) => write!(f, "PRT snapshot: VGE sub-payload: {e}"),
            Self::Inconsistent => f.write_str("PRT snapshot is internally inconsistent"),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<vge_protocol::codec::DecodeError> for SnapshotError {
    fn from(_: vge_protocol::codec::DecodeError) -> Self {
        SnapshotError::BadPayload
    }
}

impl From<vt100::SnapshotError> for SnapshotError {
    fn from(e: vt100::SnapshotError) -> Self {
        SnapshotError::Vt100(e)
    }
}

impl From<crate::vge::SnapshotError> for SnapshotError {
    fn from(e: crate::vge::SnapshotError) -> Self {
        SnapshotError::Vge(e)
    }
}

// ---- top-level encode -------------------------------------------------

pub(crate) fn encode_engine(engine: &PrtEngine, out: &mut Vec<u8>) {
    let mut w = Writer::new();
    w.u16(SNAPSHOT_KIND_VERSION);
    // See the VGE snapshot for the rationale: the line origin a
    // `Scrollback` portal's `anchor_line` references is vt100's, and
    // rides along in the vt100 fragment of the same screen.

    encode_focus(&engine.state.focus, &mut w);
    encode_cursor_style(engine.state.cursor_style, &mut w);
    w.bool(engine.state.on_alt);
    encode_portal_set(&engine.state.main, &mut w);
    if let Some(alt) = &engine.state.alt {
        w.bool(true);
        encode_portal_set(alt, &mut w);
    } else {
        w.bool(false);
    }

    out.extend_from_slice(&w.buf);
}

pub(crate) fn decode_engine_into(
    engine: &mut PrtEngine,
    bytes: &[u8],
) -> Result<(), SnapshotError> {
    let mut r = Reader::new(bytes);
    let kind = r.u16()?;
    if kind != SNAPSHOT_KIND_VERSION {
        return Err(SnapshotError::KindVersionMismatch {
            got: kind,
            want: SNAPSHOT_KIND_VERSION,
        });
    }
    let focus = decode_focus(&mut r)?;
    let cursor_style = decode_cursor_style(&mut r)?;
    let on_alt = r.bool()?;
    let main = decode_portal_set(&mut r, engine)?;
    let has_alt = r.bool()?;
    let alt = if has_alt {
        Some(decode_portal_set(&mut r, engine)?)
    } else {
        None
    };
    if on_alt && alt.is_none() {
        return Err(SnapshotError::Inconsistent);
    }
    if !r.at_end() {
        return Err(SnapshotError::TrailingBytes);
    }

    engine.install_state_from_snapshot(PrtState::from_raw_parts(
        main,
        alt,
        on_alt,
        focus,
        cursor_style,
    ));
    Ok(())
}

// ---- FocusKind --------------------------------------------------------

const FOCUS_HOST: u8 = 0;
const FOCUS_PORTAL: u8 = 1;

fn encode_focus(focus: &FocusKind, w: &mut Writer) {
    match focus {
        FocusKind::Host => w.u8(FOCUS_HOST),
        FocusKind::Portal(id) => {
            w.u8(FOCUS_PORTAL);
            w.str(id);
        }
    }
}

fn decode_focus(r: &mut Reader) -> Result<FocusKind, SnapshotError> {
    Ok(match r.u8()? {
        FOCUS_HOST => FocusKind::Host,
        FOCUS_PORTAL => FocusKind::Portal(r.string()?.to_owned()),
        _ => return Err(SnapshotError::BadPayload),
    })
}

// ---- CursorStyle ------------------------------------------------------

fn encode_cursor_style(cs: CursorStyle, w: &mut Writer) {
    w.u8(match cs {
        CursorStyle::Hidden => 0,
        CursorStyle::Hollow => 1,
        CursorStyle::Dim => 2,
    });
}

fn decode_cursor_style(r: &mut Reader) -> Result<CursorStyle, SnapshotError> {
    Ok(match r.u8()? {
        0 => CursorStyle::Hidden,
        1 => CursorStyle::Hollow,
        2 => CursorStyle::Dim,
        _ => return Err(SnapshotError::BadPayload),
    })
}

// ---- PolledStateCache -------------------------------------------------

fn encode_polled(c: &PolledStateCache, w: &mut Writer) {
    w.bool(c.on_alt);
    w.bool(c.cursor_visible);
    w.u8(c.mouse_protocol);
    w.u8(c.mouse_encoding);
    w.u8(c.focus_events);
}

fn decode_polled(r: &mut Reader) -> Result<PolledStateCache, SnapshotError> {
    Ok(PolledStateCache {
        on_alt: r.bool()?,
        cursor_visible: r.bool()?,
        mouse_protocol: r.u8()?,
        mouse_encoding: r.u8()?,
        focus_events: r.u8()?,
    })
}

// ---- PortalAnchor -----------------------------------------------------

const ANCHOR_LIVE: u8 = 0;
const ANCHOR_SCROLLBACK: u8 = 1;

fn encode_anchor(a: PortalAnchor, w: &mut Writer) {
    match a {
        PortalAnchor::Live { origin_y } => {
            w.u8(ANCHOR_LIVE);
            w.i32(origin_y);
        }
        PortalAnchor::Scrollback { anchor_line } => {
            w.u8(ANCHOR_SCROLLBACK);
            w.i64(anchor_line);
        }
    }
}

fn decode_anchor(r: &mut Reader) -> Result<PortalAnchor, SnapshotError> {
    Ok(match r.u8()? {
        ANCHOR_LIVE => PortalAnchor::Live { origin_y: r.i32()? },
        ANCHOR_SCROLLBACK => PortalAnchor::Scrollback {
            anchor_line: r.i64()?,
        },
        _ => return Err(SnapshotError::BadPayload),
    })
}

// ---- PortalSet --------------------------------------------------------

/// Buffers first, then the views that reference them.
///
/// The sharing graph has to survive the wire: serializing each view's
/// buffer inline would silently unshare a forked pair on reattach,
/// turning two views of one shell into two independent (and instantly
/// divergent) grids.
fn encode_portal_set(set: &PortalSet, w: &mut Writer) {
    w.varu(set.contents.len() as u64);
    for (cid, content) in &set.contents {
        w.u64(*cid);
        encode_content(content, w);
    }
    w.varu(set.portals.len() as u64);
    for (key, portal) in &set.portals {
        w.str(key);
        encode_portal(portal, w);
    }
    w.u64(set.creation_counter);
    w.u64(set.content_counter());
}

fn decode_portal_set(
    r: &mut Reader,
    parent_engine: &PrtEngine,
) -> Result<PortalSet, SnapshotError> {
    let n = r.varu()? as usize;
    let mut contents = std::collections::HashMap::with_capacity(n);
    for _ in 0..n {
        let cid = r.u64()?;
        let content = decode_content(r, parent_engine)?;
        contents.insert(cid, content);
    }

    let n = r.varu()? as usize;
    let mut portals = std::collections::HashMap::with_capacity(n);
    for _ in 0..n {
        let key = r.string()?.to_owned();
        let portal = decode_portal(r)?;
        portals.insert(key, portal);
    }
    let creation_counter = r.u64()?;
    let content_counter = r.u64()?;

    // Rebuild the refcounts from the views rather than trusting a
    // number on the wire — they cannot then disagree with each other.
    // A view naming a missing buffer, or a buffer nothing references,
    // means the two halves of the snapshot don't match.
    for content in contents.values_mut() {
        content.views = 0;
    }
    for portal in portals.values() {
        let Some(content) = contents.get_mut(&portal.content) else {
            return Err(SnapshotError::Inconsistent);
        };
        content.views += 1;
    }
    if contents.values().any(|c| c.views == 0) {
        return Err(SnapshotError::Inconsistent);
    }

    Ok(PortalSet::from_raw_parts(
        portals,
        contents,
        creation_counter,
        content_counter,
    ))
}

// ---- Portal (view) ----------------------------------------------------

fn encode_portal(p: &Portal, w: &mut Writer) {
    w.str(&p.id);
    w.u64(p.content);
    w.i32(p.origin_x);
    encode_anchor(p.anchor, w);
    w.bool(p.is_visible);
    w.i32(p.draw_order);
    w.u64(p.creation_seq);
    w.u32(p.view_offset);
}

fn decode_portal(r: &mut Reader) -> Result<Portal, SnapshotError> {
    Ok(Portal {
        id: r.string()?.to_owned(),
        content: r.u64()?,
        origin_x: r.i32()?,
        anchor: decode_anchor(r)?,
        is_visible: r.bool()?,
        draw_order: r.i32()?,
        creation_seq: r.u64()?,
        view_offset: r.u32()?,
    })
}

// ---- PortalContent (shared buffer) ------------------------------------

fn encode_content(c: &PortalContent, w: &mut Writer) {
    w.u32(c.size_w);
    w.u32(c.size_h);
    w.u32(c.scrollback_lines);
    // `views` is not written: the decoder recounts it from the views,
    // so the refcount cannot disagree with the graph it describes.

    // Nested sub-snapshots as length-prefixed byte blobs so the
    // decoder can hand each one to the right deserializer without
    // needing intricate framing.
    let vt_bytes = c.vt.screen().binary_snapshot();
    w.bytes(&vt_bytes);
    let vge_bytes = c.vge.binary_snapshot();
    w.bytes(&vge_bytes);
    // Recursive PRT snapshot for the buffer's children sub-engine.
    let mut children_bytes = Vec::new();
    encode_engine(&c.children, &mut children_bytes);
    w.bytes(&children_bytes);

    encode_polled(&c.state_cache, w);
    w.u32(c.pending_cursor_queries);
}

fn decode_content(
    r: &mut Reader,
    parent_engine: &PrtEngine,
) -> Result<PortalContent, SnapshotError> {
    let size_w = r.u32()?;
    let size_h = r.u32()?;
    let scrollback_lines = r.u32()?;

    let vt_bytes = r.bytes()?;
    let vge_bytes = r.bytes()?;
    let children_bytes = r.bytes()?;

    let state_cache = decode_polled(r)?;
    let pending_cursor_queries = r.u32()?;

    // Build a fresh buffer scaffold the way `cmd_create_portal` does.
    let rows = size_h as u16;
    let cols = size_w as u16;
    let mut vt = vt100::Parser::new_with_callbacks(
        rows,
        cols,
        scrollback_lines as usize,
        super::portal::PortalCallbacks::default(),
    );
    vt.screen_mut().restore_from_binary_snapshot(vt_bytes)?;

    let (cell_px, scale_factor) = parent_engine.metrics_for_children();
    let mut vge = crate::vge::VgeEngine::new(cell_px, scale_factor);
    vge.set_auto_reply_dsr(false);
    vge.restore_from_binary_snapshot(vge_bytes)?;

    let mut children = parent_engine.child_engine_scaffold();
    decode_engine_into(&mut children, children_bytes)?;

    // Both sub-engines anchor to this portal's vt100, restored just
    // above with the line origin their `anchor_line` values were
    // recorded against.
    vge.sync_top_of_live_screen(&vt);
    children.sync_top_of_live_screen(&vt);

    let vft = parent_engine.spawn_portal_vft();
    // A restored portal starts with a fresh VssEngine: any in-flight
    // snapshot transfer inside the previous attach has by definition
    // already completed (it produced *this* snapshot) or been
    // abandoned with the old renderer. Same for `pre_attach_backup` —
    // a snapshot loaded from outside doesn't carry an "even older"
    // state to roll back to.
    let vss = crate::vss::VssEngine::new();
    // SES holds no persistent state — a restored portal starts with a
    // fresh, non-session engine, same as VSS/VFT.
    let ses = crate::ses::SesEngine::new();

    Ok(PortalContent {
        size_w,
        size_h,
        scrollback_lines,
        // Recomputed by `decode_portal_set` once the views are in.
        views: 0,
        vt,
        children,
        vge,
        vft,
        vss,
        ses,
        pre_attach_backup: None,
        state_cache,
        // Not carried on the wire: the §8.10 damage baseline is a
        // local diffing aid, and the first quiet write after the
        // attach re-establishes it against the restored grid. Its
        // rate-limit stamp goes with it, so the restored portal is
        // free to evaluate on the very next write.
        damage_baseline: None,
        last_damage_eval: None,
        pending_cursor_queries,
    })
}
