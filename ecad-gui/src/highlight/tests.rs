//! Cross-view highlight projection tests (the mapping table).

use super::*;
use crate::fixtures::{board_domain, schematic_domain};
use ecad_core::id::{EntityId, NetId};

/// Selecting a trace projects (board) the trace itself and (both) its NET — the schematic
/// has no trace geometry, so a board-only trace id must reach the schematic via its net.
#[test]
fn trace_projects_to_its_net_for_the_schematic() {
    let d = board_domain();
    let doc = d.doc.as_ref().expect("board fixture elaborates");
    let tid = *doc.traces.keys().next().expect("fixture has a trace");
    let net = doc.traces[&tid].net.clone();

    let sel = [SemanticId::Trace(tid)];
    let sets = HighlightSets::project(sel.iter(), doc, &d.lib);

    // The trace's net is resolved and carried.
    assert!(sets.nets.contains(&net), "trace's net must be resolved");
    // The schematic set carries the NET id (the schematic wire/tag candidates key on Net) —
    // NOT the trace id (which the schematic can't render).
    assert!(
        sets.schematic.contains(&SemanticId::Net(net.clone())),
        "schematic must light the trace's net"
    );
    // The board matches the trace by net expansion even without enumerating its id: any
    // candidate on that net (the trace / pour / via) lights up.
    assert!(
        sets.board_matches(&SemanticId::Trace(tid), Some(&net)),
        "board must light the trace"
    );
    assert!(
        sets.board_matches(
            &SemanticId::Pour {
                net: net.clone(),
                layer: "F.Cu".into()
            },
            Some(&net)
        ),
        "board must light other copper of the same net"
    );
}

/// Selecting a NET lights all copper of the net (board) and all wires/tagged pins (schematic
/// — via the Net id in the schematic set), and every member pin appears in the board set.
#[test]
fn net_projects_to_copper_and_wires() {
    let d = schematic_domain();
    let doc = d.doc.as_ref().expect("schematic fixture elaborates");
    let net = NetId::new("VDD");

    let sel = [SemanticId::Net(net.clone())];
    let sets = HighlightSets::project(sel.iter(), doc, &d.lib);

    assert!(sets.nets.contains(&net));
    // Schematic side: the net id.
    assert!(sets.schematic.contains(&SemanticId::Net(net.clone())));
    // Board side: every member pin of VDD is in the board set (concrete copper candidates),
    // and any candidate on the net matches.
    let members = &doc.nets[&net].members;
    assert!(!members.is_empty());
    for pr in members {
        assert!(
            sets.board.contains(&SemanticId::Pin {
                comp: pr.comp.clone(),
                pin: pr.pin.clone(),
            }),
            "board must light member pin {pr:?}"
        );
    }
    // A pour on VDD lights up by net even though its id was never enumerated.
    assert!(sets.board_matches(
        &SemanticId::Pour {
            net: net.clone(),
            layer: "F.Cu".into()
        },
        Some(&net)
    ));
}

/// Selecting a refdes (Part) lights halos in BOTH views: the board (its pins) and the
/// schematic (its body + pins).
#[test]
fn part_projects_to_both_views() {
    let d = schematic_domain();
    let doc = d.doc.as_ref().expect("schematic fixture elaborates");
    let part = SemanticId::Part(EntityId::new("U1"));

    let sets = HighlightSets::project(std::iter::once(&part), doc, &d.lib);

    // Both views carry the Part id itself (board has pin candidates sharing the comp;
    // schematic has the body candidate keyed on Part).
    assert!(sets.board.contains(&part), "board must light the part");
    assert!(
        sets.schematic.contains(&part),
        "schematic must light the symbol body"
    );
    // U1's pins (VDD/GND are net members) appear in both sets.
    let vdd_pin = SemanticId::Pin {
        comp: EntityId::new("U1"),
        pin: "VDD".into(),
    };
    assert!(sets.board.contains(&vdd_pin) && sets.schematic.contains(&vdd_pin));
}

/// Selecting a part lights **every** pad on the board — including pads on no net. Board
/// pick candidates key every pad by `Pin{comp,pad}` regardless of net, so a part-derived
/// highlight must enumerate pins from the part *definition*, not from net membership.
/// In `SCHEMATIC_ECAD`, `C2` (a `Cap` with pads p1/p2) has only `C2.p1` on a net (GND);
/// `C2.p2` is unconnected. Selecting `C2` must still light p2's copper on the board.
#[test]
fn part_lights_unconnected_pads_on_the_board() {
    let d = schematic_domain();
    let doc = d.doc.as_ref().expect("schematic fixture elaborates");
    let part = SemanticId::Part(EntityId::new("C2"));

    let sets = HighlightSets::project(std::iter::once(&part), doc, &d.lib);

    let p1 = SemanticId::Pin {
        comp: EntityId::new("C2"),
        pin: "p1".into(),
    };
    let p2 = SemanticId::Pin {
        comp: EntityId::new("C2"),
        pin: "p2".into(),
    };
    // Precondition: C2.p2 is on no net at all.
    assert!(
        doc.nets.values().all(|n| !n
            .members
            .contains(&ecad_core::doc::PinRef::new(&EntityId::new("C2"), "p2"))),
        "precondition: C2.p2 must be unconnected"
    );
    // Both pads' copper must light on the board (board candidates key on Pin id, net or not).
    assert!(
        sets.board.contains(&p1),
        "board must light netted pad C2.p1"
    );
    assert!(
        sets.board.contains(&p2),
        "board must light UNCONNECTED pad C2.p2 (was the bug: net-only derivation omitted it)"
    );
    // Both also light on the schematic (a pin candidate per pin_slot).
    assert!(sets.schematic.contains(&p1) && sets.schematic.contains(&p2));
}

/// Selecting a pin resolves its net (so the status bar / net cues follow) and lights the
/// pin in both views.
#[test]
fn pin_projects_to_both_and_resolves_net() {
    let d = schematic_domain();
    let doc = d.doc.as_ref().expect("schematic fixture elaborates");
    let pin = SemanticId::Pin {
        comp: EntityId::new("U1"),
        pin: "VDD".into(),
    };
    let sets = HighlightSets::project(std::iter::once(&pin), doc, &d.lib);
    assert!(sets.board.contains(&pin) && sets.schematic.contains(&pin));
    assert!(
        sets.nets.contains(&NetId::new("VDD")),
        "the pin's net must be resolved for the net cue"
    );
}
