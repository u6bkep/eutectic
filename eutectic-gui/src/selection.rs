//! The semantic selection model (structural commitment 2, `docs/gui-architecture.md`).
//!
//! Selection and hover are **sets of semantic ids** — never stored geometry, rects,
//! or layer indices. The model lives in [`DomainState`](crate::app::DomainState) so
//! every pane (milestone 4's schematic pane included) projects the *same* selection
//! into its own overlay; cross-view highlighting is then free.
//!
//! # Geometry-free by construction
//!
//! [`SelectionModel`] holds only `BTreeSet<SemanticId>`, and
//! [`SemanticId`](crate::canvas::pick::SemanticId) is an enum of ids / id-tuples with
//! no `Point`, `Rect`, `Shape2D`, or layer-index field anywhere in its definition.
//! This is a compile-time property: there is no way to store geometry in the model.
//! A pick produces an id; a pane *re-derives* the geometry to highlight from the doc
//! each frame (see [`crate::canvas::pick::candidates`]). Re-elaboration that changes
//! geometry never invalidates the selection.
//!
//! Single-select on click is enough for milestone 3, but the type is a set so
//! multi-select (marquee, shift-click) drops in without a model change.

use crate::canvas::pick::SemanticId;
use std::collections::BTreeSet;

/// The shared selection + hover state: two id sets. Empty ⇒ nothing selected (the
/// inspector shows its empty state). Cloned cheaply; folded by the app in `on_event`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionModel {
    /// The committed selection (click-select). One element in m3, but a set so
    /// marquee / shift-click extend it later with no type change.
    selected: BTreeSet<SemanticId>,
    /// The hover set — feature(s) the pointer is over. Sparse on damascene 0.4.5
    /// (free hover emits no event; only enter/drag/down positions arrive), so this is
    /// usually empty or single. Rendered as a dimmer pre-select cue.
    hovered: BTreeSet<SemanticId>,
}

impl SelectionModel {
    /// A fresh empty model.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the selection with a single id (click-select). Clears any prior
    /// selection first — single-select semantics for m3.
    pub fn select_only(&mut self, id: SemanticId) {
        self.selected.clear();
        self.selected.insert(id);
    }

    /// Clear the whole selection (click on empty canvas / Esc).
    pub fn clear(&mut self) {
        self.selected.clear();
    }

    /// Add an id to the selection WITHOUT clearing (multi-select). Used by the findings
    /// panel to select both nets of a clearance violation at once. Idempotent.
    pub fn add(&mut self, id: SemanticId) {
        self.selected.insert(id);
    }

    /// Prune both the selection and hover sets to the ids `keep` accepts — the reload
    /// contract's "drop ids that no longer resolve" step (m5). An id that fails `keep`
    /// is removed silently; a selection that fully drops out becomes empty (the
    /// inspector then shows its empty state). Never panics.
    pub fn retain(&mut self, keep: impl Fn(&SemanticId) -> bool) {
        self.selected.retain(&keep);
        self.hovered.retain(&keep);
    }

    /// True when nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// The selected ids, in stable (`BTreeSet`) order.
    pub fn selected(&self) -> impl Iterator<Item = &SemanticId> {
        self.selected.iter()
    }

    /// The single selected id, if exactly one is selected — the common m3 read for the
    /// inspector.
    pub fn single(&self) -> Option<&SemanticId> {
        if self.selected.len() == 1 {
            self.selected.iter().next()
        } else {
            None
        }
    }

    /// Is `id` selected?
    pub fn is_selected(&self, id: &SemanticId) -> bool {
        self.selected.contains(id)
    }

    /// Set the hover to a single id (from a pointer-enter/drag pick), replacing any
    /// prior hover.
    pub fn hover_only(&mut self, id: SemanticId) {
        self.hovered.clear();
        self.hovered.insert(id);
    }

    /// Clear the hover set (pointer left, or nothing under the cursor).
    pub fn clear_hover(&mut self) {
        self.hovered.clear();
    }

    /// The hovered ids, in stable order.
    pub fn hovered(&self) -> impl Iterator<Item = &SemanticId> {
        self.hovered.iter()
    }

    /// Is `id` hovered (and not already selected — a selected feature reads as
    /// selected, not hovered)?
    pub fn is_hovered(&self, id: &SemanticId) -> bool {
        self.hovered.contains(id) && !self.selected.contains(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eutectic_core::id::TraceId;

    /// Single-select semantics: select_only replaces the prior selection.
    #[test]
    fn select_only_replaces() {
        let mut m = SelectionModel::new();
        m.select_only(SemanticId::Trace(TraceId(1)));
        assert_eq!(m.single(), Some(&SemanticId::Trace(TraceId(1))));
        m.select_only(SemanticId::Trace(TraceId(2)));
        assert_eq!(m.single(), Some(&SemanticId::Trace(TraceId(2))));
        assert_eq!(m.selected().count(), 1);
    }

    /// Clearing empties the selection; hover is tracked independently and a selected
    /// id never reads as hovered.
    #[test]
    fn clear_and_hover_independence() {
        let mut m = SelectionModel::new();
        let id = SemanticId::Trace(TraceId(7));
        m.select_only(id.clone());
        m.hover_only(id.clone());
        // A selected id is not "hovered" for highlight purposes.
        assert!(m.is_selected(&id));
        assert!(!m.is_hovered(&id));
        m.clear();
        assert!(m.is_empty());
        // With the selection cleared, the same id now reads as hovered.
        assert!(m.is_hovered(&id));
        m.clear_hover();
        assert_eq!(m.hovered().count(), 0);
    }

    /// Geometry-free by construction (structural commitment 2): the model stores only
    /// `SemanticId`s, which are ids — no `Point`/`Rect`/`Shape2D`/layer-index field.
    /// This is a *compile-time* property. `SemanticId` is `Hash + Ord` (a pure id
    /// key), and the whole model is `size_of`-bounded by two `BTreeSet` handles — it
    /// cannot hold a geometry buffer. The assertion below documents the intent; the
    /// real guarantee is that `SemanticId`'s definition (see `canvas::pick`) has no
    /// geometry variant/field, which the type system enforces.
    #[test]
    fn selection_model_holds_ids_only() {
        // Two BTreeSets — a fixed small handle size, independent of how much is
        // selected (the ids live on the heap as `SemanticId`, never geometry).
        assert_eq!(
            std::mem::size_of::<SelectionModel>(),
            2 * std::mem::size_of::<std::collections::BTreeSet<SemanticId>>()
        );
    }
}
