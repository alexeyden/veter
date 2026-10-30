//! The flat engine: plain rounded fills and text, every colour taken
//! from the host theme ([`crate::theme`]) — the look the clients had
//! before there were engines. Each client family's parts live in a
//! child module ([`mux`], [`doc`], [`browser`]); the modals are [`crate::modal`]'s builders.

mod browser;
mod doc;
mod host;
mod icons;
mod mux;

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{Align, Color, CreateElementBody, DrawCmd};

use super::{
    Dialog, DocChrome, DocFrame, DocRole, Engine, Figure, FileKind, IconBox, ListInk, Metrics,
    PaneChrome, SearchPanel, StatusLeft, StatusPlacement, TabBar, TabBarParts, TabbarMetrics,
};
use crate::modal::{self, ModalIds, ScrollModal};
use crate::paint::Px;
use crate::picker::PickerView;

/// The flat engine.
pub struct Flat;

const METRICS: Metrics = Metrics {
    // One row, tabs abutting, a space either side of a label.
    tabbar: TabbarMetrics {
        rows: 1,
        margin: 0.0,
        gap: 0.0,
        name_pad: 2.0,
        status: StatusPlacement::BarEnd,
    },
    page_inset: 0.0,
};

impl Engine for Flat {
    fn name(&self) -> &'static str {
        "flat"
    }

    fn metrics(&self) -> &Metrics {
        &METRICS
    }

    fn tab_bar(&self, px: Px, bar: &TabBar<'_>) -> TabBarParts {
        mux::tab_bar(px, bar)
    }

    fn pane_chrome(&self, cmds: &mut Vec<DrawCmd>, px: Px, pane: &PaneChrome<'_>) {
        mux::pane_chrome(cmds, px, pane);
    }

    fn separators(&self, cmds: &mut Vec<DrawCmd>, lines: Vec<(Point, Point)>) {
        mux::separators(cmds, lines);
    }

    fn doc_ground(&self, cmds: &mut Vec<DrawCmd>, px: Px, frame: &DocFrame) {
        // The terminal's background is the paper.
        let _ = (cmds, px, frame);
    }

    fn doc_chrome(&self, cmds: &mut Vec<DrawCmd>, px: Px, frame: &DocFrame, c: &DocChrome<'_>) {
        doc::chrome(cmds, px, frame, c);
    }

    fn doc_ink(&self, role: DocRole) -> Color {
        doc::ink(role)
    }

    fn code_plate(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x0: f32,
        x1: f32,
        y: f32,
        h: f32,
        first: bool,
        last: bool,
        zoom: f32,
    ) {
        let _ = zoom;
        doc::code_plate(cmds, px, x0, x1, y, h, first, last);
    }

    fn table_band(&self, cmds: &mut Vec<DrawCmd>, r: Rect, head: bool) {
        doc::table_band(cmds, r, head);
    }

    fn quote_bar(&self, cmds: &mut Vec<DrawCmd>, r: Rect, warning: bool) {
        doc::quote_bar(cmds, r, warning);
    }

    fn rule(&self, cmds: &mut Vec<DrawCmd>, r: Rect, weight: f32) {
        doc::rule(cmds, r, weight);
    }

    fn figure(&self, cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, content: Figure<'_>, zoom: f32) {
        doc::figure(cmds, px, r, content, zoom);
    }

    fn code_chip(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x0: f32,
        x1: f32,
        y: f32,
        h: f32,
        zoom: f32,
    ) {
        doc::code_chip(cmds, px, x0, x1, y, h, zoom);
    }

    fn heading_mark(
        &self,
        cmds: &mut Vec<DrawCmd>,
        level: u8,
        x: f32,
        w: f32,
        y: f32,
        h: f32,
        zoom: f32,
    ) {
        // Ink and size are mark enough.
        let _ = (cmds, level, x, w, y, h, zoom);
    }

    fn search_hit(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        w: f32,
        y: f32,
        h: f32,
        zoom: f32,
        current: bool,
    ) {
        let _ = px;
        doc::search_hit(cmds, x, w, y, h, zoom, current);
    }

    fn browser_backdrop(&self, cmds: &mut Vec<DrawCmd>, px: Px, w: f32, h: f32) {
        let _ = px;
        browser::backdrop(cmds, w, h);
    }

    fn column_pane(&self, cmds: &mut Vec<DrawCmd>, px: Px, area: Rect) {
        browser::column_pane(cmds, px, area);
    }

    fn column_row(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        y: f32,
        label: &str,
        is_dir: bool,
        selected: bool,
    ) {
        browser::column_row(cmds, px, area, y, label, is_dir, selected);
    }

    fn browser_note(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        align: Align,
        text: &str,
        warn: bool,
    ) {
        let _ = px;
        browser::note(cmds, x, y, align, text, warn);
    }

    fn grid_pane(&self, cmds: &mut Vec<DrawCmd>, px: Px, area: Rect) {
        // The backdrop is the panel.
        let _ = (cmds, px, area);
    }

    fn browser_scrollbar(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        track_h: f32,
        thumb_h: f32,
        frac: f32,
    ) {
        let _ = px;
        browser::scrollbar(cmds, area, track_h, thumb_h, frac);
    }

    fn tile_plate(&self, cmds: &mut Vec<DrawCmd>, px: Px, tile: Rect, img_h: f32, selected: bool) {
        browser::tile_plate(cmds, px, tile, img_h, selected);
    }

    fn tile_mark(&self, cmds: &mut Vec<DrawCmd>, px: Px, tile: Rect) {
        browser::tile_mark(cmds, px, tile);
    }

    fn tile_label(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        label: &str,
        is_dir: bool,
        selected: bool,
    ) {
        // A tile tells a directory by its icon alone.
        let _ = (px, is_dir);
        browser::tile_label(cmds, x, y, label, selected);
    }

    fn list_row_plate(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        row: Rect,
        odd: bool,
        selected: bool,
    ) {
        browser::list_row_plate(cmds, px, row, odd, selected);
    }

    fn list_row_mark(&self, cmds: &mut Vec<DrawCmd>, px: Px, row: Rect) {
        let _ = px;
        browser::list_row_mark(cmds, row);
    }

    fn list_text(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        x: f32,
        y: f32,
        align: Align,
        text: &str,
        what: ListInk,
        selected: bool,
    ) {
        let _ = px;
        browser::list_text(cmds, x, y, align, text, what, selected);
    }

    fn thumbnail(&self, cmds: &mut Vec<DrawCmd>, px: Px, r: Rect, image_id: String) {
        let _ = px;
        cmds.push(DrawCmd::DrawImage {
            target_rect: r,
            image_id,
            source_rect: None,
        });
    }

    fn file_icon(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        kind: FileKind,
        b: IconBox,
        is_link: bool,
        broken: bool,
    ) {
        let _ = px;
        cmds.extend(icons::draw(kind, b, is_link, broken));
    }

    fn status_bar(
        &self,
        cmds: &mut Vec<DrawCmd>,
        px: Px,
        area: Rect,
        left: &str,
        left_kind: StatusLeft,
        right: &str,
    ) {
        browser::status_bar(cmds, px, area, left, left_kind, right);
    }

    fn dialog(&self, cmds: &mut Vec<DrawCmd>, px: Px, dialog: &Dialog<'_>) {
        host::dialog(cmds, px, dialog);
    }

    fn search_panel(&self, cmds: &mut Vec<DrawCmd>, px: Px, panel: &SearchPanel<'_>) {
        host::search_panel(cmds, px, panel);
    }

    fn jump_label(
        &self,
        cmds: &mut Vec<DrawCmd>,
        _px: Px,
        x: f32,
        y: f32,
        label: &str,
        fill: Color,
        ink: Color,
    ) {
        host::jump_label(cmds, x, y, label, fill, ink);
    }

    fn scroll_thumb(&self, cmds: &mut Vec<DrawCmd>, px: Px, thumb: Rect) {
        host::scroll_thumb(cmds, px, thumb);
    }

    fn prompt(
        &self,
        id: &str,
        host_w: u32,
        host_h: u32,
        title: &str,
        line: &str,
        px: Px,
        caret: Option<usize>,
        draw_order: i32,
    ) -> CreateElementBody {
        modal::prompt_element(
            id, host_w, host_h, title, line, px.w, px.h, caret, draw_order,
        )
    }

    fn picker(
        &self,
        id: &str,
        host_w: u32,
        host_h: u32,
        picker: &dyn PickerView,
        px: Px,
        draw_order: i32,
    ) -> Vec<CreateElementBody> {
        modal::picker_element(id, host_w, host_h, picker, px.w, px.h, draw_order)
    }

    fn scroll_modal(
        &self,
        modal: &ScrollModal,
        ids: ModalIds<'_>,
        host_w: u32,
        host_h: u32,
        offset: u32,
        px: Px,
        draw_order: i32,
    ) -> Vec<CreateElementBody> {
        modal.elements(ids, host_w, host_h, offset, px.w, px.h, draw_order)
    }
}
