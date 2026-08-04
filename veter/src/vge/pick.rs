// Pointer hit-testing index for VGE content.
//
// VGE elements carry no extent on the wire — a `DrawText` is a string
// and an origin (§7.4), not a box — and where one lands on screen is
// the product of scrollback anchoring (§5.2), an optional per-element
// affine transform (§9.11), the ancestor chain of clip rects (§9.2)
// and, inside a portal, that portal's own origin and clip. A separate
// hit-test walk would have to re-derive all of it and keep the copy in
// step with the renderer forever; for text it would additionally have
// to re-shape the run to find which character the pointer is over.
//
// So the index is a *by-product of drawing*. `vge::render` pushes one
// [`PickItem`] per `DrawText` / `DrawImage` it paints and takes the
// device matrix straight off the canvas: femtovg's `set_transform`
// premultiplies, so `Canvas::transform()` at draw time already carries
// the portal translation and every ancestor transform. The geometry is
// correct by construction.
//
// Only text and images are indexed. Every other shape stays
// transparent to the pointer, so a client painting a full-screen
// background (vfm's tile grid) doesn't swallow ordinary grid selection
// underneath it.
//
// The list is rebuilt each frame and read on the next pointer event.
// It can only go stale if something moved, and anything that moves
// also asks for a redraw.

use femtovg::Transform2D;

use veter_host::vge::state::{Element, VgeState};
use vge_protocol::command::DrawCmd;

/// An axis-aligned rectangle in some pixel space. Kept local to this
/// module rather than borrowed from femtovg (whose `Rect` isn't
/// re-exported) so the pick geometry has one definition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PickRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PickRect {
    /// A rect large enough to stand in for "no clip". Finite, so it
    /// survives intersection arithmetic without producing NaNs.
    pub const UNBOUNDED: Self = Self {
        x: -1.0e9,
        y: -1.0e9,
        w: 2.0e9,
        h: 2.0e9,
    };

    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Build from a possibly-inverted rect (negative width or height),
    /// normalising it to non-negative extents.
    pub fn normalized(x: f32, y: f32, w: f32, h: f32) -> Self {
        let (x, w) = if w < 0.0 { (x + w, -w) } else { (x, w) };
        let (y, h) = if h < 0.0 { (y + h, -h) } else { (y, h) };
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    pub fn intersect(&self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let w = (self.x + self.w).min(other.x + other.w) - x;
        let h = (self.y + self.h).min(other.y + other.h) - y;
        Self {
            x,
            y,
            w: w.max(0.0),
            h: h.max(0.0),
        }
    }

    /// The axis-aligned bounding box of this rect once mapped through
    /// `t`. Under rotation this is an over-approximation — the same one
    /// femtovg makes when it intersects a rotated scissor (see
    /// `Canvas::intersect_scissor`), and the same one §9.8 warns about
    /// for nested clips.
    pub fn transformed_bounds(&self, t: &Transform2D) -> Self {
        let corners = [
            t.transform_point(self.x, self.y),
            t.transform_point(self.x + self.w, self.y),
            t.transform_point(self.x, self.y + self.h),
            t.transform_point(self.x + self.w, self.y + self.h),
        ];
        let min_x = corners.iter().fold(f32::INFINITY, |m, c| m.min(c.0));
        let min_y = corners.iter().fold(f32::INFINITY, |m, c| m.min(c.1));
        let max_x = corners.iter().fold(f32::NEG_INFINITY, |m, c| m.max(c.0));
        let max_y = corners.iter().fold(f32::NEG_INFINITY, |m, c| m.max(c.1));
        Self {
            x: min_x,
            y: min_y,
            w: max_x - min_x,
            h: max_y - min_y,
        }
    }
}

/// What kind of drawable an item indexes, plus just enough payload to
/// notice that the underlying command changed between the frame that
/// recorded the item and the pointer event that consults it. The
/// command itself is re-read from live state through
/// [`PickItem::resolve`], so nothing here is a copy of client data.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PickKind {
    /// A `DrawText` run. `byte_len` is the length of the string as
    /// drawn; `scale` is the element's composed on-screen scale
    /// (§9.11), which set the rasterisation size and so has to be fed
    /// back in to reproduce the same shaping when mapping a pointer
    /// position to a character.
    Text { byte_len: u32, scale: f32 },
    /// A `DrawImage` target rect.
    Image,
}

/// Where a picked drawable lives in host state. Elements are addressed
/// by `creation_seq` rather than by storage key: it is unique within an
/// `ElementSet`, never reused, and — unlike the key — is the same for
/// named and anonymous elements (§6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PickLocator {
    /// Index into [`PickList::path`]. `0` is always the host scope.
    pub path_id: u16,
    /// Which screen's element set the item was drawn from (§5.4). A
    /// swap invalidates the locator: the two sets number their
    /// elements independently.
    pub on_alt: bool,
    pub creation_seq: u64,
    pub cmd_index: u32,
}

/// One drawable recorded during a render pass.
#[derive(Clone, Copy, Debug)]
pub struct PickItem {
    pub loc: PickLocator,
    pub kind: PickKind,
    /// The drawable's box in the coordinate space the draw call used,
    /// before `to_device` is applied.
    pub local: PickRect,
    /// Local space → device pixels, as read off the canvas at draw
    /// time.
    pub to_device: Transform2D,
    /// Every clip in force at draw time, intersected, in device pixels.
    pub clip: PickRect,
}

impl PickItem {
    /// Map a device-pixel point into this item's local space. `None`
    /// when the point is outside the item's clip, or when `to_device`
    /// is singular (a degenerate transform — the element renders to
    /// nothing, so there is nothing to pick).
    pub fn local_point(&self, px: f32, py: f32) -> Option<(f32, f32)> {
        if !self.clip.contains(px, py) {
            return None;
        }
        let Transform2D([a, b, c, d, ..]) = self.to_device;
        // `Transform2D::inverse` silently yields the identity on a
        // singular matrix, which would hand back a bogus hit.
        if (a * d - c * b).abs() < 1.0e-6 {
            return None;
        }
        Some(self.to_device.inverse().transform_point(px, py))
    }

    /// Look the item's draw command back up in live state. Returns
    /// `None` if the element is gone, the screen swapped, the command
    /// index no longer exists, or the command is no longer the kind
    /// that was recorded — each of which means the pick is stale and
    /// the caller should drop whatever it was anchoring there.
    pub fn resolve<'a>(&self, state: &'a VgeState) -> Option<&'a DrawCmd> {
        if state.on_alt() != self.loc.on_alt {
            return None;
        }
        let el = element_by_seq(state, self.loc.creation_seq)?;
        let cmd = el.commands.get(self.loc.cmd_index as usize)?;
        match (self.kind, cmd) {
            (PickKind::Text { byte_len, .. }, DrawCmd::DrawText { text, .. })
                if text.len() == byte_len as usize =>
            {
                Some(cmd)
            }
            (PickKind::Image, DrawCmd::DrawImage { .. }) => Some(cmd),
            _ => None,
        }
    }

    /// Like [`Self::local_point`] but ignoring the clip. A drag that
    /// wanders outside the clip still has to move the selection head,
    /// the same way a grid drag past the viewport edge keeps
    /// extending.
    pub fn local_point_unclipped(&self, px: f32, py: f32) -> Option<(f32, f32)> {
        let Transform2D([a, b, c, d, ..]) = self.to_device;
        if (a * d - c * b).abs() < 1.0e-6 {
            return None;
        }
        Some(self.to_device.inverse().transform_point(px, py))
    }
}

/// What a VGE selection covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VgeSelKind {
    /// A range of characters inside one `DrawText` run. `anchor` is
    /// where the drag started and does not move; `head` follows the
    /// pointer, so `head < anchor` for a right-to-left drag. Both are
    /// byte offsets into the run's text.
    Text {
        anchor: usize,
        head: usize,
        /// True while the button is still down.
        dragging: bool,
    },
    /// A whole `DrawImage`. There is nothing finer to select: the
    /// target rect is one drawable, and cropping it further is the
    /// client's job through `source_rect_px` (§7.5).
    Image,
}

/// The user's selection inside VGE content — at most one, of one kind,
/// in one draw command.
///
/// Text selection is confined to a single run: VGE text floats free of
/// the grid, so there is no defined reading order between two runs, nor
/// between a run and the grid rows beneath it. One run is a filename, a
/// label, a pane title — which is what there is to copy.
///
/// The command is addressed the same way a [`PickItem`] is, except the
/// portal path is owned: this outlives the frame that created it, and
/// the interned path ids do not.
#[derive(Clone, Debug)]
pub struct VgeSelection {
    /// Portal path from the host root down to the scope the command was
    /// drawn in. Empty = the host scope.
    pub path: Vec<String>,
    pub on_alt: bool,
    pub creation_seq: u64,
    pub cmd_index: u32,
    pub kind: VgeSelKind,
}

impl VgeSelection {
    /// The selected byte range of a text selection, low end first.
    pub fn text_range(&self) -> Option<(usize, usize)> {
        match self.kind {
            VgeSelKind::Text { anchor, head, .. } => Some(if anchor <= head {
                (anchor, head)
            } else {
                (head, anchor)
            }),
            VgeSelKind::Image => None,
        }
    }

    /// True for a text selection that covers no characters — a drag
    /// that hasn't moved yet. Images are never empty.
    pub fn is_empty(&self) -> bool {
        matches!(self.kind, VgeSelKind::Text { anchor, head, .. } if anchor == head)
    }

    pub fn is_dragging(&self) -> bool {
        matches!(self.kind, VgeSelKind::Text { dragging: true, .. })
    }

    pub fn is_image(&self) -> bool {
        matches!(self.kind, VgeSelKind::Image)
    }

    /// The [`PickKind`] this selection can legally live in.
    fn wants(&self) -> fn(&PickKind) -> bool {
        match self.kind {
            VgeSelKind::Text { .. } => |k| matches!(k, PickKind::Text { .. }),
            VgeSelKind::Image => |k| matches!(k, PickKind::Image),
        }
    }

    /// Does this selection live in the command identified by these
    /// coordinates? Used by the render walk to decide whether the
    /// command it is about to paint carries the highlight.
    pub fn targets(
        &self,
        path: &[String],
        on_alt: bool,
        creation_seq: u64,
        cmd_index: u32,
    ) -> bool {
        self.on_alt == on_alt
            && self.creation_seq == creation_seq
            && self.cmd_index == cmd_index
            && self.path == path
    }
}

/// Find an element on the current screen by its creation sequence.
/// Linear, but only ever walked on a pointer event.
pub fn element_by_seq(state: &VgeState, creation_seq: u64) -> Option<&Element> {
    state
        .elements()
        .values()
        .find(|e| e.creation_seq == creation_seq)
}

/// A hit, with the pointer already mapped into the item's local space
/// so callers don't invert the matrix twice.
#[derive(Clone, Copy, Debug)]
pub struct PickHit<'a> {
    pub item: &'a PickItem,
    pub local_x: f32,
    pub local_y: f32,
}

/// The per-frame index. Items are appended in paint order, so the last
/// one containing a point is the visually topmost.
#[derive(Default)]
pub struct PickList {
    /// Portal paths, interned so an item stays a fixed-size POD.
    /// Index 0 is the host scope (empty path) and is never removed.
    paths: Vec<Vec<String>>,
    items: Vec<PickItem>,
}

impl PickList {
    pub fn new() -> Self {
        Self {
            paths: vec![Vec::new()],
            items: Vec::new(),
        }
    }

    /// Drop last frame's contents, keeping the allocations and the
    /// host scope at index 0.
    pub fn clear(&mut self) {
        self.paths.truncate(1);
        self.items.clear();
    }

    /// Intern a portal path, returning its id. Called once per portal
    /// per frame, so the linear scan is over a handful of entries.
    pub fn intern_path(&mut self, path: &[String]) -> u16 {
        if let Some(i) = self.paths.iter().position(|p| p == path) {
            return i as u16;
        }
        // u16 is plenty (max_portals is two orders of magnitude below
        // it); saturate rather than wrap if a pathological tree ever
        // gets there — a shared id costs a mis-scoped pick, not memory
        // unsafety.
        if self.paths.len() >= u16::MAX as usize {
            return 0;
        }
        self.paths.push(path.to_vec());
        (self.paths.len() - 1) as u16
    }

    /// The portal path an item was drawn under. Empty = host scope.
    pub fn path(&self, path_id: u16) -> &[String] {
        self.paths.get(path_id as usize).map_or(&[], |p| p.as_slice())
    }

    pub fn push(&mut self, item: PickItem) {
        if item.local.is_empty() || item.clip.is_empty() {
            return;
        }
        self.items.push(item);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The topmost item under a device-pixel point.
    pub fn hit(&self, px: f32, py: f32) -> Option<PickHit<'_>> {
        self.hit_matching(px, py, |_| true)
    }

    /// Find the item a live selection is anchored to in *this* frame's
    /// index. The run may have moved (scrolled, re-laid-out) since the
    /// selection was made, so a drag re-locates it rather than reusing
    /// stale geometry. `None` once the run stops being drawn.
    pub fn find(&self, sel: &VgeSelection) -> Option<&PickItem> {
        let wants = sel.wants();
        self.items.iter().find(|item| {
            wants(&item.kind)
                && sel.targets(
                    self.path(item.loc.path_id),
                    item.loc.on_alt,
                    item.loc.creation_seq,
                    item.loc.cmd_index,
                )
        })
    }

    /// The topmost item under a device-pixel point that also satisfies
    /// `want` — used to ask for "the topmost *image* here" without
    /// letting a text run painted over it shadow the answer.
    pub fn hit_matching(
        &self,
        px: f32,
        py: f32,
        want: impl Fn(&PickItem) -> bool,
    ) -> Option<PickHit<'_>> {
        for item in self.items.iter().rev() {
            if !want(item) {
                continue;
            }
            let Some((lx, ly)) = item.local_point(px, py) else {
                continue;
            };
            if item.local.contains(lx, ly) {
                return Some(PickHit {
                    item,
                    local_x: lx,
                    local_y: ly,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(local: PickRect, t: Transform2D, clip: PickRect, seq: u64) -> PickItem {
        PickItem {
            loc: PickLocator {
                path_id: 0,
                on_alt: false,
                creation_seq: seq,
                cmd_index: 0,
            },
            kind: PickKind::Image,
            local,
            to_device: t,
            clip,
        }
    }

    #[test]
    fn topmost_wins() {
        let mut list = PickList::new();
        let r = PickRect::new(0.0, 0.0, 10.0, 10.0);
        list.push(item(r, Transform2D::identity(), PickRect::UNBOUNDED, 1));
        list.push(item(r, Transform2D::identity(), PickRect::UNBOUNDED, 2));
        // Later push == painted later == on top.
        assert_eq!(list.hit(5.0, 5.0).unwrap().item.loc.creation_seq, 2);
        assert!(list.hit(11.0, 5.0).is_none());
    }

    #[test]
    fn translation_is_applied() {
        let mut list = PickList::new();
        list.push(item(
            PickRect::new(0.0, 0.0, 10.0, 10.0),
            Transform2D::translation(100.0, 50.0),
            PickRect::UNBOUNDED,
            1,
        ));
        assert!(list.hit(5.0, 5.0).is_none());
        let hit = list.hit(105.0, 55.0).unwrap();
        assert!((hit.local_x - 5.0).abs() < 1e-4);
        assert!((hit.local_y - 5.0).abs() < 1e-4);
    }

    #[test]
    fn clip_rejects_outside() {
        let mut list = PickList::new();
        list.push(item(
            PickRect::new(0.0, 0.0, 100.0, 100.0),
            Transform2D::identity(),
            PickRect::new(0.0, 0.0, 20.0, 20.0),
            1,
        ));
        assert!(list.hit(10.0, 10.0).is_some());
        // Inside the drawable but scissored away.
        assert!(list.hit(50.0, 50.0).is_none());
    }

    #[test]
    fn rotation_maps_back_to_local() {
        let mut list = PickList::new();
        // Rotate 90° CCW about the origin: local (2, 0) → device (0, 2).
        let t = Transform2D::rotation(std::f32::consts::FRAC_PI_2);
        list.push(item(
            PickRect::new(0.0, 0.0, 10.0, 4.0),
            t,
            PickRect::UNBOUNDED,
            1,
        ));
        let hit = list.hit(-1.0, 2.0).expect("inside the rotated box");
        assert!((hit.local_x - 2.0).abs() < 1e-3, "lx = {}", hit.local_x);
        assert!((hit.local_y - 1.0).abs() < 1e-3, "ly = {}", hit.local_y);
        // A point that would be inside the *unrotated* box is now outside.
        assert!(list.hit(2.0, 1.0).is_none());
    }

    #[test]
    fn singular_transform_is_unpickable() {
        let mut list = PickList::new();
        list.push(item(
            PickRect::new(0.0, 0.0, 10.0, 10.0),
            Transform2D::scaling(0.0, 1.0),
            PickRect::UNBOUNDED,
            1,
        ));
        assert!(list.hit(0.0, 5.0).is_none());
    }

    #[test]
    fn empty_geometry_is_not_indexed() {
        let mut list = PickList::new();
        list.push(item(
            PickRect::new(0.0, 0.0, 0.0, 10.0),
            Transform2D::identity(),
            PickRect::UNBOUNDED,
            1,
        ));
        list.push(item(
            PickRect::new(0.0, 0.0, 10.0, 10.0),
            Transform2D::identity(),
            PickRect::new(5.0, 5.0, 0.0, 0.0),
            2,
        ));
        assert!(list.is_empty());
    }

    #[test]
    fn hit_matching_skips_unwanted_kinds() {
        let mut list = PickList::new();
        let r = PickRect::new(0.0, 0.0, 10.0, 10.0);
        list.push(item(r, Transform2D::identity(), PickRect::UNBOUNDED, 1));
        let mut text = item(r, Transform2D::identity(), PickRect::UNBOUNDED, 2);
        text.kind = PickKind::Text { byte_len: 4, scale: 1.0 };
        list.push(text);
        assert_eq!(list.hit(5.0, 5.0).unwrap().item.loc.creation_seq, 2);
        let img = list
            .hit_matching(5.0, 5.0, |i| matches!(i.kind, PickKind::Image))
            .unwrap();
        assert_eq!(img.item.loc.creation_seq, 1);
    }

    #[test]
    fn paths_intern_and_reuse() {
        let mut list = PickList::new();
        assert_eq!(list.path(0), &[] as &[String]);
        let a = list.intern_path(&["pane1".to_string()]);
        let b = list.intern_path(&["pane1".to_string(), "inner".to_string()]);
        assert_eq!(list.intern_path(&["pane1".to_string()]), a);
        assert_ne!(a, b);
        assert_eq!(list.path(b), &["pane1".to_string(), "inner".to_string()]);
        list.clear();
        assert_eq!(list.path(0), &[] as &[String]);
        // Interning restarts after a clear.
        assert_eq!(list.intern_path(&["other".to_string()]), 1);
    }

    fn selection(path: &[&str], seq: u64, anchor: usize, head: usize) -> VgeSelection {
        VgeSelection {
            path: path.iter().map(|s| s.to_string()).collect(),
            on_alt: false,
            creation_seq: seq,
            cmd_index: 0,
            kind: VgeSelKind::Text {
                anchor,
                head,
                dragging: false,
            },
        }
    }

    fn image_selection(path: &[&str], seq: u64) -> VgeSelection {
        VgeSelection {
            path: path.iter().map(|s| s.to_string()).collect(),
            on_alt: false,
            creation_seq: seq,
            cmd_index: 0,
            kind: VgeSelKind::Image,
        }
    }

    #[test]
    fn selection_range_is_direction_agnostic() {
        let forward = selection(&[], 1, 2, 7);
        let backward = selection(&[], 1, 7, 2);
        assert_eq!(forward.text_range(), Some((2, 7)));
        assert_eq!(backward.text_range(), Some((2, 7)));
        assert!(!forward.is_empty());
        assert!(selection(&[], 1, 3, 3).is_empty());
        // An image selection has no range and is never empty.
        let img = image_selection(&[], 1);
        assert_eq!(img.text_range(), None);
        assert!(!img.is_empty());
        assert!(img.is_image());
    }

    #[test]
    fn selection_targets_one_command_in_one_scope() {
        let sel = selection(&["pane1"], 4, 0, 3);
        let pane = ["pane1".to_string()];
        assert!(sel.targets(&pane, false, 4, 0));
        // Wrong screen, element, command index or scope: no match.
        assert!(!sel.targets(&pane, true, 4, 0));
        assert!(!sel.targets(&pane, false, 5, 0));
        assert!(!sel.targets(&pane, false, 4, 1));
        assert!(!sel.targets(&[], false, 4, 0));
        assert!(!sel.targets(&["pane2".to_string()], false, 4, 0));
    }

    #[test]
    fn find_locates_the_selected_run_in_this_frame() {
        let mut list = PickList::new();
        let r = PickRect::new(0.0, 0.0, 10.0, 10.0);
        let pane_id = list.intern_path(&["pane1".to_string()]);

        // Same creation_seq in two different scopes — the path is what
        // tells them apart.
        let mut host_text = item(r, Transform2D::identity(), PickRect::UNBOUNDED, 4);
        host_text.kind = PickKind::Text {
            byte_len: 3,
            scale: 1.0,
        };
        let mut pane_text = host_text;
        pane_text.loc.path_id = pane_id;
        list.push(host_text);
        list.push(pane_text);

        let found = list.find(&selection(&["pane1"], 4, 0, 3)).expect("in pane");
        assert_eq!(found.loc.path_id, pane_id);
        let found = list.find(&selection(&[], 4, 0, 3)).expect("on host");
        assert_eq!(found.loc.path_id, 0);
        assert!(list.find(&selection(&["pane2"], 4, 0, 3)).is_none());
        assert!(list.find(&selection(&[], 9, 0, 3)).is_none());
    }

    #[test]
    fn find_matches_the_selections_own_kind() {
        let mut list = PickList::new();
        // An image at the same coordinates as a text selection must not
        // be mistaken for the selected run, and vice versa.
        list.push(item(
            PickRect::new(0.0, 0.0, 10.0, 10.0),
            Transform2D::identity(),
            PickRect::UNBOUNDED,
            4,
        ));
        assert!(list.find(&selection(&[], 4, 0, 3)).is_none());
        assert!(list.find(&image_selection(&[], 4)).is_some());
        assert!(list.find(&image_selection(&[], 5)).is_none());
    }

    #[test]
    fn unclipped_mapping_survives_a_drag_off_the_run() {
        let it = item(
            PickRect::new(0.0, 0.0, 10.0, 10.0),
            Transform2D::translation(100.0, 50.0),
            PickRect::new(100.0, 50.0, 10.0, 10.0),
            1,
        );
        // Pointer dragged past the clip: clipped mapping refuses, the
        // unclipped one still reports where the head should go.
        assert!(it.local_point(200.0, 55.0).is_none());
        let (lx, _) = it.local_point_unclipped(200.0, 55.0).unwrap();
        assert!((lx - 100.0).abs() < 1e-4, "lx = {lx}");
    }

    #[test]
    fn transformed_bounds_covers_rotation() {
        let r = PickRect::new(0.0, 0.0, 10.0, 2.0);
        let t = Transform2D::rotation(std::f32::consts::FRAC_PI_2);
        let b = r.transformed_bounds(&t);
        assert!((b.w - 2.0).abs() < 1e-3, "w = {}", b.w);
        assert!((b.h - 10.0).abs() < 1e-3, "h = {}", b.h);
    }
}
