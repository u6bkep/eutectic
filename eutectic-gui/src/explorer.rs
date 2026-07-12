//! The explorer panel (mockup `NetExplorer` anatomy, minimal): collapsible
//! **Components** and **Nets** sections in the right sidebar, below the inspector.
//!
//! Each row is a click-to-select entry into the shared [`SelectionModel`], so selecting a
//! component or net here cross-highlights in every pane (the semantic selection model made
//! visible — structural commitment 2). The selected row gets the mockup's selected cue
//! (the `current` treatment on `sidebar_menu_button`). Sheets are omitted (single-sheet
//! model today).
//!
//! This is a pure projection `Doc → Vec<Row>` plus a route-key ↔ [`SemanticId`] mapping;
//! the El rendering folds over it, and the app's `on_event` maps a clicked row key back to
//! the id to select. Keeping the mapping here (not scattered in `app.rs`) means the click
//! target and the rendered row can never drift.

use crate::pick::SemanticId;
use eutectic_core::annotate;
use eutectic_core::doc::Doc;
use eutectic_core::part::PartLib;

/// One explorer row: its display parts and the semantic id it selects. `key` is the
/// event-route key of the row's button; `count` is the badge (pin count for a part, member
/// count for a net).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerRow {
    /// The route key of the row's button (`explorer:comp:<refdes>` / `explorer:net:<name>`).
    pub key: String,
    /// The primary label (refdes / net name).
    pub label: String,
    /// The secondary label (part name for a component; empty for a net).
    pub secondary: String,
    /// The count badge (pins / members).
    pub count: usize,
    /// The id this row selects.
    pub id: SemanticId,
}

/// The projected explorer contents: the two sections, each a list of rows. Built from the
/// doc; the El builder folds over it and `on_event` looks a clicked key up in [`lookup`].
#[derive(Clone, Debug, Default)]
pub struct Explorer {
    /// Components, in entity-id order (stable).
    pub components: Vec<ExplorerRow>,
    /// Nets, in net-id order (stable).
    pub nets: Vec<ExplorerRow>,
}

/// The route-key prefix for a component row.
const COMP_PREFIX: &str = "explorer:comp:";
/// The route-key prefix for a net row.
const NET_PREFIX: &str = "explorer:net:";

impl Explorer {
    /// Project the doc + library into explorer rows. Components show refdes + part + pin
    /// count; nets show name + member count. Pure and deterministic (`BTreeMap` order).
    pub fn project(doc: &Doc, lib: &PartLib) -> Explorer {
        let refdes = annotate::refdes(doc, lib, &annotate::registry(&doc.source));
        let components = doc
            .components
            .iter()
            .map(|(eid, comp)| {
                let designator = refdes.get(eid).cloned().unwrap_or_else(|| eid.0.clone());
                let pins = lib.get(&comp.part).map(|d| d.pins.len()).unwrap_or(0);
                ExplorerRow {
                    key: format!("{COMP_PREFIX}{}", eid.0),
                    label: designator,
                    secondary: comp.part.clone(),
                    count: pins,
                    id: SemanticId::Part(eid.clone()),
                }
            })
            .collect();
        let nets = doc
            .nets
            .iter()
            .map(|(nid, net)| ExplorerRow {
                key: format!("{NET_PREFIX}{}", nid.0),
                label: nid.0.clone(),
                secondary: String::new(),
                count: net.members.len(),
                id: SemanticId::Net(nid.clone()),
            })
            .collect();
        Explorer { components, nets }
    }

    /// The id a row route-key selects, if it is one of ours. Used by `on_event` to fold a
    /// clicked explorer row into the selection. Looks the key up against the projected rows
    /// (so it stays honest if a row's id derivation changes).
    pub fn lookup(&self, key: &str) -> Option<SemanticId> {
        self.components
            .iter()
            .chain(&self.nets)
            .find(|r| r.key == key)
            .map(|r| r.id.clone())
    }
}
