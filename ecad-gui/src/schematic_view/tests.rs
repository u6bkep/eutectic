//! Schematic-view projection + pick tests.

use super::*;
use crate::fixtures::schematic_domain;
use ecad_core::id::EntityId;

/// The schematic fixture's (doc, lib, view).
fn fixture() -> (ecad_core::doc::Doc, ecad_core::part::PartLib, SchematicView) {
    let d = schematic_domain();
    let doc = d
        .doc
        .as_ref()
        .expect("schematic fixture elaborates")
        .clone();
    let view = SchematicView::build(&doc, &d.lib).expect("schematic projects");
    (doc, d.lib, view)
}

/// The placement centre of a component in schematic space.
fn center_of(doc: &ecad_core::doc::Doc, lib: &ecad_core::part::PartLib, path: &str) -> Point {
    let placements = doc.reflow_schematic(lib);
    placements
        .get(&EntityId::new(path))
        .expect("component placed")
        .center
}

/// Clicking the centre of a symbol body (clear of its pins) selects that part.
#[test]
fn click_symbol_body_selects_part() {
    let (doc, lib, view) = fixture();
    let c = center_of(&doc, &lib, "C1");
    let id = view.resolve(c, 0).expect("body hit");
    assert_eq!(id, SemanticId::Part(EntityId::new("C1")), "got {id:?}");
}

/// Clicking a pin stub tip selects that pin (by pad number), beating the body underneath.
#[test]
fn click_pin_selects_pin() {
    let (doc, lib, view) = fixture();
    let center = center_of(&doc, &lib, "U1");
    let def = lib.get("MCU").unwrap();
    let unrot_hw = symbol_extent(def).w / 2;
    let slot = pin_slots(def)
        .into_iter()
        .find(|s| s.name == "VDD")
        .expect("MCU has a VDD pin");
    let g = stub_geometry(slot.side, unrot_hw, slot.dy, Orient::IDENTITY);
    let tip = offset(center, g.tip);
    let id = view.resolve(tip, 0).expect("pin hit");
    match id {
        SemanticId::Pin { comp, pin } => {
            assert_eq!(comp, EntityId::new("U1"));
            assert_eq!(
                pin, slot.id,
                "pin id must be the pad NUMBER (the PinRef join key)"
            );
        }
        other => panic!("expected a pin, got {other:?}"),
    }
}

/// Clicking a wire segment selects its net (the cross-view currency for wires). The fixture
/// draws `wire C1.p1 U1.VDD` on net VDD.
#[test]
fn click_wire_selects_net() {
    let (doc, lib, view) = fixture();
    let c1 = center_of(&doc, &lib, "C1");
    let u1 = center_of(&doc, &lib, "U1");
    let cap = lib.get("Cap").unwrap();
    let mcu = lib.get("MCU").unwrap();
    let cap_slot = pin_slots(cap).into_iter().find(|s| s.id == "p1").unwrap();
    let mcu_slot = pin_slots(mcu)
        .into_iter()
        .find(|s| s.name == "VDD")
        .unwrap();
    let a = offset(
        c1,
        stub_geometry(
            cap_slot.side,
            symbol_extent(cap).w / 2,
            cap_slot.dy,
            Orient::IDENTITY,
        )
        .tip,
    );
    let b = offset(
        u1,
        stub_geometry(
            mcu_slot.side,
            symbol_extent(mcu).w / 2,
            mcu_slot.dy,
            Orient::IDENTITY,
        )
        .tip,
    );
    let mid = Point {
        x: (a.x + b.x) / 2,
        y: (a.y + b.y) / 2,
    };
    let id = view.resolve(mid, 100_000).expect("wire hit");
    assert_eq!(id, SemanticId::Net(NetId::new("VDD")), "got {id:?}");
}

/// A click far outside every feature picks nothing.
#[test]
fn empty_spot_picks_nothing() {
    let (_doc, _lib, view) = fixture();
    let far = Point {
        x: -1_000 * MM,
        y: -1_000 * MM,
    };
    assert!(view.resolve(far, 0).is_none());
}

/// The overlay projects a selected net into wire + pin highlights (the schematic side of
/// cross-view highlighting): selecting VDD produces a non-empty schematic overlay.
#[test]
fn overlay_lights_net_wires_and_pins() {
    let (_doc, _lib, view) = fixture();
    let mut ids = std::collections::BTreeSet::new();
    ids.insert(SemanticId::Net(NetId::new("VDD")));
    let el = view.overlay_el(&ids, "overlay:test");
    assert!(
        el.is_some(),
        "selecting VDD must produce a non-empty schematic overlay"
    );
}

/// The poc smoke: the real 44-symbol schematic projects non-empty symbols/wires without
/// panic (spec E: poc/out/board.ecad's schematic).
#[test]
fn poc_schematic_projects_non_empty() {
    let d = crate::fixtures::poc_board_domain();
    let doc = d.doc.as_ref().expect("poc board elaborates");
    let view = SchematicView::build(doc, &d.lib).expect("poc schematic projects");
    let bodies = view
        .candidates()
        .iter()
        .filter(|c| matches!(c.id, SemanticId::Part(_)))
        .count();
    assert!(
        bodies >= 44,
        "poc schematic must project all 44 placed symbols, got {bodies}"
    );
    let wires = view.candidates().iter().filter(|c| c.priority == 1).count();
    assert!(wires >= 1, "poc schematic must project its authored wires");
}
