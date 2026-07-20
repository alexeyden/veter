//! Undo / redo.
//!
//! Snapshot-based rather than inverse-op based: a checkpoint clones the
//! element list. Diagrams are small (tens of elements) and edits are
//! human-paced, so the memory cost is irrelevant — and unlike an
//! inverse-op log, a snapshot cannot drift out of sync with the model.
//!
//! Restoring re-sends the whole document, which is why undo is not on
//! the hot path the way pan and drag are.

use crate::doc::{Document, Element};

/// Checkpoints kept before the oldest is dropped.
const MAX_DEPTH: usize = 100;

#[derive(Default)]
pub struct History {
    undo: Vec<Vec<Element>>,
    redo: Vec<Vec<Element>>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the state *before* a mutation. Call once per gesture — on
    /// mouse-down for a drag, not per motion event — so one undo step
    /// corresponds to one user action.
    pub fn checkpoint(&mut self, doc: &Document) {
        self.undo.push(doc.elements.clone());
        if self.undo.len() > MAX_DEPTH {
            self.undo.remove(0);
        }
        // A fresh edit invalidates anything that was undone.
        self.redo.clear();
    }

    /// Returns true if the document changed.
    pub fn undo(&mut self, doc: &mut Document) -> bool {
        match self.undo.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut doc.elements, prev));
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self, doc: &mut Document) -> bool {
        match self.redo.pop() {
            Some(next) => {
                self.undo.push(std::mem::replace(&mut doc.elements, next));
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Shape;

    fn doc_with(n: usize) -> Document {
        let mut d = Document::default();
        d.elements = (0..n)
            .map(|i| Element::new(format!("e{i}"), Shape::Rectangle, 0.0, 0.0, 10.0, 10.0))
            .collect();
        d
    }

    #[test]
    fn undo_restores_the_previous_state() {
        let mut d = doc_with(1);
        let mut h = History::new();
        h.checkpoint(&d);
        d.elements.push(Element::new("new", Shape::Rectangle, 0.0, 0.0, 1.0, 1.0));
        assert_eq!(d.elements.len(), 2);
        assert!(h.undo(&mut d));
        assert_eq!(d.elements.len(), 1);
    }

    #[test]
    fn redo_reapplies_what_undo_removed() {
        let mut d = doc_with(1);
        let mut h = History::new();
        h.checkpoint(&d);
        d.elements.push(Element::new("new", Shape::Rectangle, 0.0, 0.0, 1.0, 1.0));
        h.undo(&mut d);
        assert!(h.redo(&mut d));
        assert_eq!(d.elements.len(), 2);
        assert_eq!(d.elements[1].id, "new");
    }

    #[test]
    fn undo_and_redo_are_no_ops_when_empty() {
        let mut d = doc_with(1);
        let mut h = History::new();
        assert!(!h.undo(&mut d));
        assert!(!h.redo(&mut d));
        assert_eq!(d.elements.len(), 1);
    }

    #[test]
    fn a_new_edit_discards_the_redo_stack() {
        let mut d = doc_with(1);
        let mut h = History::new();
        h.checkpoint(&d);
        d.elements.push(Element::new("a", Shape::Rectangle, 0.0, 0.0, 1.0, 1.0));
        h.undo(&mut d);
        // Branching off the undone state must not leave "a" redoable.
        h.checkpoint(&d);
        d.elements.push(Element::new("b", Shape::Rectangle, 0.0, 0.0, 1.0, 1.0));
        assert!(!h.redo(&mut d));
        assert_eq!(d.elements.last().map(|e| e.id.as_str()), Some("b"));
    }

    #[test]
    fn repeated_undo_walks_back_through_history() {
        let mut d = doc_with(0);
        let mut h = History::new();
        for i in 0..3 {
            h.checkpoint(&d);
            d.elements
                .push(Element::new(format!("e{i}"), Shape::Rectangle, 0.0, 0.0, 1.0, 1.0));
        }
        assert_eq!(d.elements.len(), 3);
        for expected in [2, 1, 0] {
            assert!(h.undo(&mut d));
            assert_eq!(d.elements.len(), expected);
        }
        assert!(!h.undo(&mut d));
    }

    #[test]
    fn depth_is_capped_without_losing_the_newest() {
        let mut d = doc_with(0);
        let mut h = History::new();
        for _ in 0..MAX_DEPTH + 10 {
            h.checkpoint(&d);
            d.elements
                .push(Element::new("x", Shape::Rectangle, 0.0, 0.0, 1.0, 1.0));
        }
        let mut steps = 0;
        while h.undo(&mut d) {
            steps += 1;
        }
        assert_eq!(steps, MAX_DEPTH, "oldest checkpoints should be dropped");
    }
}
