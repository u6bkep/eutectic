//! Cross-view highlight projection (the payoff of structural commitment 2).
//!
//! Selection stays **semantic and shared** (one [`SelectionModel`] in domain state). Each
//! pane projects that selection into its own overlay. But a raw selected id does not map
//! one-to-one onto what a pane highlights: selecting a *net* must light up every copper
//! feature of the net on the board *and* every wire/tag of the net on the schematic;
//! selecting a *trace* (a board-only id with no schematic geometry) must light up ITS NET's
//! wires on the schematic. Nets are the **cross-view currency** for board-only ids.
//!
//! This module owns that mapping table, so both the board and schematic overlays expand
//! the same way. The projection is: given the selected [`SemanticId`]s (+ the doc, to
//! resolve net membership), produce the set of ids each view should test its candidates
//! against.
//!
//! # The mapping table (documented, and reported in `cross_highlight_semantics`)
//!
//! | Selected id            | Board overlay lights                         | Schematic overlay lights                    |
//! |------------------------|----------------------------------------------|---------------------------------------------|
//! | `Part(refdes)`         | the part's footprint copper (its pins)       | the part's symbol body + its pins           |
//! | `Pin{comp,pad}`        | that pad's copper                            | that pin's stub                             |
//! | `Net(n)`               | all copper of net `n` (traces/vias/pours/pins)| all wires + tagged pins of net `n`          |
//! | `Trace(t)`             | the trace's copper (direct)                  | the trace's NET's wires + tagged pins       |
//! | `Via(v)`               | the via's copper (direct)                    | the via's NET's wires + tagged pins         |
//! | `Pour{net,layer}`      | the pour's copper (direct)                   | the pour's NET's wires + tagged pins        |
//!
//! Each view then intersects the expanded set with its own candidates: the board tests its
//! `world_features` candidates, the schematic its reflow candidates. An id with no geometry
//! in a view simply matches nothing there (a `Trace` has no schematic candidate; its *net*
//! does).

use crate::canvas::pick::SemanticId;
use ecad_core::doc::{Doc, PinRef};
use ecad_core::id::NetId;
use std::collections::BTreeSet;

/// The expanded id sets a frame highlights, one per view. Built once per frame from the
/// shared selection; each view's overlay tests its candidates' ids against the matching
/// set. Separating the two keeps the *view-specific* expansion in one place (a net expands
/// to copper-feature ids for the board but to the net id itself for the schematic, whose
/// wire candidates are keyed by net).
#[derive(Clone, Debug, Default)]
pub struct HighlightSets {
    /// Ids the board overlay tests against (copper features + pins + the raw ids).
    pub board: BTreeSet<SemanticId>,
    /// Ids the schematic overlay tests against (parts + pins + net ids).
    pub schematic: BTreeSet<SemanticId>,
    /// The set of nets any selected id resolves to (for status-bar / net-explorer cues and
    /// for expanding board copper by net).
    pub nets: BTreeSet<NetId>,
}

impl HighlightSets {
    /// Project a set of selected ids into the per-view highlight sets, resolving nets from
    /// the doc. Pure over `(selected, doc)`.
    pub fn project<'a>(selected: impl Iterator<Item = &'a SemanticId>, doc: &Doc) -> HighlightSets {
        let mut sets = HighlightSets::default();
        for id in selected {
            match id {
                SemanticId::Part(_) => {
                    // Board: the part's pins are separate copper candidates; the schematic
                    // has a Part body candidate + pin candidates. Add the raw Part id (both
                    // views have a matching candidate — board pins share the comp; schematic
                    // has the body) and every pin of the part.
                    sets.board.insert(id.clone());
                    sets.schematic.insert(id.clone());
                    for pr in part_pins(doc, id) {
                        let pin = SemanticId::Pin {
                            comp: pr.comp.clone(),
                            pin: pr.pin.clone(),
                        };
                        sets.board.insert(pin.clone());
                        sets.schematic.insert(pin);
                    }
                }
                SemanticId::Pin { .. } => {
                    sets.board.insert(id.clone());
                    sets.schematic.insert(id.clone());
                    if let Some(net) = pin_net_of(doc, id) {
                        sets.nets.insert(net);
                    }
                }
                SemanticId::Net(net) => {
                    sets.nets.insert(net.clone());
                }
                SemanticId::Trace(_) | SemanticId::Via(_) | SemanticId::Pour { .. } => {
                    // Direct board copper: the board has a matching candidate.
                    sets.board.insert(id.clone());
                    if let Some(net) = raw_net_of(doc, id) {
                        sets.nets.insert(net);
                    }
                }
            }
        }
        // Nets expand into: board copper of the net (via each member pin + the net-keyed
        // pour/trace/via candidates all carry the net — but board candidates key on their
        // own id, so we express "net copper" by adding the Net id AND every member pin id,
        // and the board overlay additionally matches any candidate whose net is selected —
        // see `board_matches`). The schematic wire/tag candidates ARE keyed by Net, so the
        // net id itself is the schematic match.
        for net in sets.nets.clone() {
            sets.schematic.insert(SemanticId::Net(net.clone()));
            // Board: member pins are concrete copper candidates.
            if let Some(n) = doc.nets.get(&net) {
                for pr in &n.members {
                    sets.board.insert(SemanticId::Pin {
                        comp: pr.comp.clone(),
                        pin: pr.pin.clone(),
                    });
                }
            }
        }
        sets
    }

    /// Does the board overlay highlight a candidate with id `id` on net `net`? A candidate
    /// matches when its id is in the board set, OR its net is a selected net (so pours /
    /// traces / vias of a selected net all light up without enumerating their ids).
    pub fn board_matches(&self, id: &SemanticId, net: Option<&NetId>) -> bool {
        if self.board.contains(id) {
            return true;
        }
        matches!(net, Some(n) if self.nets.contains(n))
    }

    /// The schematic overlay set (its candidates key on Part / Pin / Net directly).
    pub fn schematic_ids(&self) -> &BTreeSet<SemanticId> {
        &self.schematic
    }
}

/// Every pin of a `Part` id, as `PinRef`s (comp + pad number), from the net membership —
/// the pins that actually exist as copper/schematic candidates.
fn part_pins(doc: &Doc, part: &SemanticId) -> Vec<PinRef> {
    let SemanticId::Part(eid) = part else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for net in doc.nets.values() {
        for pr in &net.members {
            if &pr.comp == eid {
                out.push(pr.clone());
            }
        }
    }
    out
}

/// The net a `Pin` id belongs to, if any.
fn pin_net_of(doc: &Doc, pin: &SemanticId) -> Option<NetId> {
    let SemanticId::Pin { comp, pin } = pin else {
        return None;
    };
    let pr = PinRef::new(comp, pin);
    doc.nets
        .iter()
        .find(|(_, n)| n.members.contains(&pr))
        .map(|(nid, _)| nid.clone())
}

/// The net a board-only id (trace / via / pour) belongs to.
fn raw_net_of(doc: &Doc, id: &SemanticId) -> Option<NetId> {
    match id {
        SemanticId::Trace(t) => doc.traces.get(t).map(|t| t.net.clone()),
        SemanticId::Via(v) => doc.vias.get(v).map(|v| v.net.clone()),
        SemanticId::Pour { net, .. } => Some(net.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
