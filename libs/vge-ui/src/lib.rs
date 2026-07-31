//! vge-ui — the widget layer every VGE client draws its chrome with.
//!
//! Extracted from `vmux`, which grew all of this inline: the accent
//! theme that follows the host's `host.accent` ([`theme`]), the rounded
//! chrome paths ([`shape`]), the readline-flavored single-line editor
//! ([`edit`]), the filterable list picker behind the command palette
//! ([`picker`]), the modal builders those two render through
//! ([`modal`]), the cell-width rule they lay out with ([`measure`]),
//! and a key + SGR-mouse input parser ([`input`]).
//!
//! Like `vge-render`, this crate is a pure consumer of `vge-protocol`:
//! it builds `CreateElementBody` / `DrawCmd` values and parses input
//! bytes. It owns no terminal state and performs no I/O — the consuming
//! binary decides when to send what.

pub mod edit;
pub mod input;
pub mod measure;
pub mod modal;
pub mod picker;
pub mod shape;
pub mod theme;

pub use edit::{EditOutcome, LineEditor};
pub use input::{Button, Dir, Event, InputParser};
pub use measure::{prefix_cells, text_cells};
pub use modal::{ModalIds, ScrollModal, picker_element, prompt_element};
pub use picker::{FilterMode, Picker, PickerItem, PickerOutcome};
pub use shape::{chrome_corner_radii, rounded_rect_path, rounded_rect_path_corners};
