use super::*;
use crate::doc::{Nm, Orient};
use crate::id::EntityId;
use crate::part::part_library;
use std::collections::{BTreeMap, BTreeSet};

fn sym(path: &str) -> LayoutNode {
    LayoutNode::Symbol(Symbol {
        path: path.into(),
        rot: Orient::IDENTITY,
        dx: 0,
        dy: 0,
    })
}

fn row(children: Vec<LayoutNode>) -> LayoutNode {
    LayoutNode::Container(Container {
        dir: Direction::Row,
        name: None,
        gap: 0,
        align: Align::Start,
        children,
    })
}

fn column(children: Vec<LayoutNode>) -> LayoutNode {
    LayoutNode::Container(Container {
        dir: Direction::Column,
        name: None,
        gap: 0,
        align: Align::Start,
        children,
    })
}

/// A component universe (path -> part) from `(path, part)` pairs.
fn universe(pairs: &[(&str, &str)]) -> BTreeMap<EntityId, String> {
    pairs
        .iter()
        .map(|(p, part)| (EntityId::new(*p), part.to_string()))
        .collect()
}

fn ids(pairs: &[(&str, &str)]) -> BTreeSet<EntityId> {
    pairs.iter().map(|(p, _)| EntityId::new(*p)).collect()
}

/// A DNP-dropped path set from string slices.
fn dnp(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| p.to_string()).collect()
}

// --- sizing -------------------------------------------------------------

#[test]
fn symbol_extent_grows_with_pin_count() {
    let lib = part_library();
    let cap = symbol_extent(&lib["Cap"]); // 2 pins
    let ldo = symbol_extent(&lib["LDO"]); // 3 pins
    // More pins on a side => taller box (3 pins: 2 left, 1 right => 2-high side).
    assert!(ldo.h >= cap.h);
    // Every box is at least the minimum.
    assert!(cap.w >= MIN_BOX_W && cap.h >= MIN_BOX_H);
}

#[test]
fn pin_slots_split_by_parity_and_fit_the_box() {
    let lib = part_library();
    let def = &lib["MCU"]; // 2 discrete pins + uart(tx,rx) = 4 edge pins.
    let slots = pin_slots(def);
    assert_eq!(slots.len(), 4);
    // Parity split: even indices left, odd right (2 each).
    assert_eq!(slots.iter().filter(|s| s.side == PinSide::Left).count(), 2);
    assert_eq!(slots.iter().filter(|s| s.side == PinSide::Right).count(), 2);
    // Every stub sits within the box the sizer produced (|dy| ≤ half-height).
    let e = symbol_extent(def);
    for s in &slots {
        assert!(s.dy.abs() <= e.h / 2, "stub {s:?} outside box h={}", e.h);
    }
}

#[test]
fn interface_signals_count_as_edge_pins() {
    let lib = part_library();
    // MCU: 2 discrete pins + a uart interface (tx, rx) = 4 edge pins.
    assert_eq!(edge_pins(&lib["MCU"]).len(), 4);
}

// --- packing ------------------------------------------------------------

#[test]
fn row_advances_along_x_column_along_neg_y() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);

    let r = SchematicLayout {
        roots: vec![row(vec![sym("C1"), sym("C2")])],
    };
    let pr = reflow(&r, &u, &lib, &BTreeMap::new());
    // In a row, C2 sits to the right of C1 (greater x), same y.
    assert!(pr[&EntityId::new("C2")].center.x > pr[&EntityId::new("C1")].center.x);
    assert_eq!(
        pr[&EntityId::new("C1")].center.y,
        pr[&EntityId::new("C2")].center.y
    );

    let c = SchematicLayout {
        roots: vec![column(vec![sym("C1"), sym("C2")])],
    };
    let pc = reflow(&c, &u, &lib, &BTreeMap::new());
    // In a column, C2 sits below C1 (lesser y), same x.
    assert!(pc[&EntityId::new("C2")].center.y < pc[&EntityId::new("C1")].center.y);
    assert_eq!(
        pc[&EntityId::new("C1")].center.x,
        pc[&EntityId::new("C2")].center.x
    );
}

#[test]
fn gap_widens_spacing() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
    let mk = |gap: Nm| SchematicLayout {
        roots: vec![LayoutNode::Container(Container {
            dir: Direction::Row,
            name: None,
            gap,
            align: Align::Start,
            children: vec![sym("C1"), sym("C2")],
        })],
    };
    let close = reflow(&mk(0), &u, &lib, &BTreeMap::new());
    let far = reflow(&mk(10 * 1_000_000), &u, &lib, &BTreeMap::new());
    let dx0 = close[&EntityId::new("C2")].center.x - close[&EntityId::new("C1")].center.x;
    let dx1 = far[&EntityId::new("C2")].center.x - far[&EntityId::new("C1")].center.x;
    assert_eq!(dx1 - dx0, 10 * 1_000_000);
}

#[test]
fn align_shifts_cross_axis() {
    let lib = part_library();
    // A row with a tall MCU and a short Cap: alignment moves the Cap's cross (y) pos.
    let u = universe(&[("U1", "MCU"), ("C1", "Cap")]);
    let mk = |align: Align| SchematicLayout {
        roots: vec![LayoutNode::Container(Container {
            dir: Direction::Row,
            name: None,
            gap: 0,
            align,
            children: vec![sym("U1"), sym("C1")],
        })],
    };
    let start = reflow(&mk(Align::Start), &u, &lib, &BTreeMap::new());
    let center = reflow(&mk(Align::Center), &u, &lib, &BTreeMap::new());
    let end = reflow(&mk(Align::End), &u, &lib, &BTreeMap::new());
    let cap_y = |m: &BTreeMap<EntityId, Placement>| m[&EntityId::new("C1")].center.y;
    // Start puts the short box at the top; End at the bottom; Center between.
    assert!(cap_y(&start) > cap_y(&center));
    assert!(cap_y(&center) > cap_y(&end));
}

#[test]
fn nested_containers_size_to_content() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap"), ("C3", "Cap")]);
    // A column whose first row holds C1,C2 and second row holds C3. All three placed.
    let layout = SchematicLayout {
        roots: vec![column(vec![
            row(vec![sym("C1"), sym("C2")]),
            row(vec![sym("C3")]),
        ])],
    };
    let p = reflow(&layout, &u, &lib, &BTreeMap::new());
    assert_eq!(p.len(), 3);
    // The second row (C3) sits below the first (C1/C2).
    assert!(p[&EntityId::new("C3")].center.y < p[&EntityId::new("C1")].center.y);
}

#[test]
fn pinned_offset_shifts_symbol() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap")]);
    let base = SchematicLayout {
        roots: vec![row(vec![sym("C1")])],
    };
    let shifted = SchematicLayout {
        roots: vec![row(vec![LayoutNode::Symbol(Symbol {
            path: "C1".into(),
            rot: Orient::IDENTITY,
            dx: 3_000_000,
            dy: -2_000_000,
        })])],
    };
    let pb = reflow(&base, &u, &lib, &BTreeMap::new());
    let ps = reflow(&shifted, &u, &lib, &BTreeMap::new());
    let b = pb[&EntityId::new("C1")].center;
    let s = ps[&EntityId::new("C1")].center;
    // dx/dy applied on top of the (unchanged, centered) flow position.
    assert_eq!(s.x - b.x, 3_000_000);
    assert_eq!(s.y - b.y, -2_000_000);
}

#[test]
fn rot_swaps_extent() {
    let lib = part_library();
    let u = universe(&[("U1", "MCU")]);
    let upright = symbol_extent(&lib["MCU"]);
    let layout = SchematicLayout {
        roots: vec![row(vec![LayoutNode::Symbol(Symbol {
            path: "U1".into(),
            rot: Orient::from_deg(90).unwrap(),
            dx: 0,
            dy: 0,
        })])],
    };
    let p = reflow(&layout, &u, &lib, &BTreeMap::new());
    let e = p[&EntityId::new("U1")].extent;
    assert_eq!(e.w, upright.h);
    assert_eq!(e.h, upright.w);
}

// --- unplaced bin -------------------------------------------------------

#[test]
fn unplaced_components_land_in_the_bin() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap"), ("C3", "Cap")]);
    // Only C1 is placed; C2 and C3 fall to the bin.
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("C1")])],
    };
    let p = reflow(&layout, &u, &lib, &BTreeMap::new());
    assert_eq!(p.len(), 3); // totality: every component has a coordinate.
    // The bin sits below the placed content (negative y region well under C1).
    assert!(p[&EntityId::new("C2")].center.y < p[&EntityId::new("C1")].center.y);
    assert!(p[&EntityId::new("C3")].center.y < p[&EntityId::new("C1")].center.y);
}

#[test]
fn empty_layout_puts_everything_in_the_bin() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
    let p = reflow(&SchematicLayout::default(), &u, &lib, &BTreeMap::new());
    assert_eq!(p.len(), 2);
}

#[test]
fn missing_part_still_gets_a_placement() {
    let lib = part_library();
    // A component whose part is not in the lib: the view stays total (min box).
    let u = universe(&[("X1", "NoSuchPart")]);
    let p = reflow(&SchematicLayout::default(), &u, &lib, &BTreeMap::new());
    assert_eq!(p[&EntityId::new("X1")].extent, MIN_EXTENT);
}

// --- determinism --------------------------------------------------------

#[test]
fn reflow_is_deterministic() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("U1", "MCU"), ("L1", "LDO"), ("C2", "Cap")]);
    let layout = SchematicLayout {
        roots: vec![column(vec![
            row(vec![sym("U1"), sym("L1")]),
            sym("C1"),
            // C2 unplaced -> bin.
        ])],
    };
    // Two runs must be byte-equal. BTreeMap iteration is deterministic, so a
    // Debug-rendered dump is a faithful byte-level proxy for the placement set.
    let dump = |m: &BTreeMap<EntityId, Placement>| format!("{m:?}");
    let a = reflow(&layout, &u, &lib, &BTreeMap::new());
    let b = reflow(&layout, &u, &lib, &BTreeMap::new());
    assert_eq!(dump(&a), dump(&b));
    assert_eq!(a, b);
}

// --- validation ---------------------------------------------------------

#[test]
fn unknown_sym_path_is_an_error() {
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("C1"), sym("NOPE")])],
    };
    let (errors, _, _) = validate(&layout, &ids(&[("C1", "Cap")]), &dnp(&[]), &BTreeMap::new());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, "E_SCHEMATIC");
}

#[test]
fn duplicate_sym_is_an_error() {
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("C1"), sym("C1")])],
    };
    let (errors, _, _) = validate(&layout, &ids(&[("C1", "Cap")]), &dnp(&[]), &BTreeMap::new());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("more than one"));
}

#[test]
fn duplicate_sibling_name_is_an_error() {
    let named = |name: &str| {
        LayoutNode::Container(Container {
            dir: Direction::Row,
            name: Some(name.into()),
            gap: 0,
            align: Align::Start,
            children: vec![],
        })
    };
    let layout = SchematicLayout {
        roots: vec![named("power"), named("power")],
    };
    let (errors, _, _) = validate(&layout, &ids(&[]), &dnp(&[]), &BTreeMap::new());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("duplicate sibling"));
}

#[test]
fn same_name_in_different_scopes_is_ok() {
    // Two containers named "col" but in different parents: not siblings, so allowed.
    let inner = |name: &str| {
        LayoutNode::Container(Container {
            dir: Direction::Column,
            name: Some(name.into()),
            gap: 0,
            align: Align::Start,
            children: vec![],
        })
    };
    let layout = SchematicLayout {
        roots: vec![row(vec![inner("col")]), row(vec![inner("col")])],
    };
    let (errors, _, _) = validate(&layout, &ids(&[]), &dnp(&[]), &BTreeMap::new());
    assert!(errors.is_empty());
}

#[test]
fn unplaced_reported_as_warning_set() {
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("C1")])],
    };
    let (errors, unplaced, _) = validate(
        &layout,
        &ids(&[("C1", "Cap"), ("C2", "Cap")]),
        &dnp(&[]),
        &BTreeMap::new(),
    );
    assert!(errors.is_empty());
    assert_eq!(unplaced, vec![EntityId::new("C2")]);
}

#[test]
fn dnp_dropped_sym_degrades_to_unplaced_not_error() {
    // A `sym` pointing at a component the source declared but a false `if=`
    // depopulated must NOT be an E_SCHEMATIC abort (Decision 20c × 21b): it degrades to
    // the unplaced warning, like a never-placed part. Only a truly unknown path aborts.
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("C1"), sym("C2")])],
    };
    // C1 is populated; C2 was dropped by `if=false`. No component universe entry for C2.
    let (errors, unplaced, _) = validate(
        &layout,
        &ids(&[("C1", "Cap")]),
        &dnp(&["C2"]),
        &BTreeMap::new(),
    );
    assert!(errors.is_empty(), "DNP-dropped placed sym must not error");
    // C2 surfaces as unplaced (so it warns), and is absent from the placed set.
    assert_eq!(unplaced, vec![EntityId::new("C2")]);
}

// --- wire validation ----------------------------------------------------

fn wire(a: (&str, &str), b: (&str, &str)) -> LayoutNode {
    LayoutNode::Wire(Wire {
        a: WireEnd {
            comp: a.0.into(),
            pin: a.1.into(),
        },
        b: WireEnd {
            comp: b.0.into(),
            pin: b.1.into(),
        },
        waypoints: vec![],
    })
}

/// A pin→net lookup from `(comp, pin, net)` triples.
fn nets(triples: &[(&str, &str, &str)]) -> impl Fn(&crate::doc::PinRef) -> Option<String> {
    let map: BTreeMap<(String, String), String> = triples
        .iter()
        .map(|(c, p, n)| ((c.to_string(), p.to_string()), n.to_string()))
        .collect();
    move |pr: &crate::doc::PinRef| map.get(&(pr.comp.to_string(), pr.pin.clone())).cloned()
}

#[test]
fn wire_to_real_pins_on_same_net_is_silent() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
    let layout = SchematicLayout {
        roots: vec![wire(("C1", "p1"), ("C2", "p1"))],
    };
    let net = nets(&[("C1", "p1", "N1"), ("C2", "p1", "N1")]);
    let (errors, warnings) = validate_wires(&layout, &u, &lib, &dnp(&[]), &net);
    assert!(errors.is_empty());
    assert!(warnings.is_empty(), "same-net wire is honest: {warnings:?}");
}

#[test]
fn wire_across_two_nets_warns_not_errors() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
    let layout = SchematicLayout {
        roots: vec![wire(("C1", "p1"), ("C2", "p1"))],
    };
    // The two pins are on *different* nets: legal but honest disagreement (§20d).
    let net = nets(&[("C1", "p1", "N1"), ("C2", "p1", "N2")]);
    let (errors, warnings) = validate_wires(&layout, &u, &lib, &dnp(&[]), &net);
    assert!(errors.is_empty(), "cross-net is a warning, not an error");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W_SCHEMATIC_WIRE");
    assert!(warnings[0].message.contains("different nets"));
}

#[test]
fn wire_unknown_comp_or_pin_is_an_error() {
    let lib = part_library();
    let u = universe(&[("C1", "Cap")]);
    // Unknown component `NOPE`, and a real component with a bogus pin.
    let layout = SchematicLayout {
        roots: vec![
            wire(("C1", "p1"), ("NOPE", "p1")),
            wire(("C1", "bogus"), ("C1", "p2")),
        ],
    };
    let (errors, _) = validate_wires(&layout, &u, &lib, &dnp(&[]), &nets(&[]));
    assert_eq!(errors.len(), 2);
    assert!(errors.iter().all(|e| e.code == "E_SCHEMATIC"));
}

#[test]
fn wire_to_interface_signal_fails_loud_not_silent() {
    // v1 limitation (documented on parse_wire_header): a `port.signal` endpoint like
    // `U1.uart.tx` last-dot-splits to comp `U1.uart` + pin `tx`. There is no component
    // `U1.uart`, so it is a hard E_SCHEMATIC — loud, never silently mis-wired. The
    // workaround is to wire the bound pad number. This pins the clean failure so the
    // behaviour can never silently degrade into wiring the wrong node.
    let lib = part_library();
    let u = universe(&[("U1", "MCU")]);
    let layout = SchematicLayout {
        roots: vec![wire(("U1.uart", "tx"), ("U1", "VDD"))],
    };
    let (errors, _) = validate_wires(&layout, &u, &lib, &dnp(&[]), &nets(&[]));
    assert_eq!(errors.len(), 1, "the interface-signal endpoint must error");
    assert_eq!(errors[0].code, "E_SCHEMATIC");
    assert!(errors[0].message.contains("U1.uart"));
}

#[test]
fn wire_on_dnp_dropped_comp_degrades_to_warning() {
    let lib = part_library();
    // C2 is DNP-dropped (declared, then `if=false`). A wire onto it must not error — it
    // degrades like a `sym` (§20c × 21b), a non-blocking W_SCHEMATIC_WIRE.
    let u = universe(&[("C1", "Cap")]);
    let layout = SchematicLayout {
        roots: vec![wire(("C1", "p1"), ("C2", "p1"))],
    };
    let (errors, warnings) = validate_wires(&layout, &u, &lib, &dnp(&["C2"]), &nets(&[]));
    assert!(
        errors.is_empty(),
        "DNP-dropped wire endpoint must not error"
    );
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code, "W_SCHEMATIC_WIRE");
    assert!(warnings[0].message.contains("depopulated"));
}

// --- def-embedded layout stamping (Decision 20 embedded in a def) ------

/// Build a stamped fragment table `{ ipath -> layout }` where each layout is a
/// prefixed `column` of `sym`s at the given internal paths (already instance-prefixed by
/// the caller — mirroring what `elaborate::prefix_fragment` produces).
fn frag_column(ipath: &str, prefixed_paths: &[&str]) -> (String, SchematicLayout) {
    let children = prefixed_paths.iter().map(|p| sym(p)).collect();
    (
        ipath.to_string(),
        SchematicLayout {
            roots: vec![column(children)],
        },
    )
}

#[test]
fn def_instance_sym_expands_to_the_stamped_fragment() {
    let lib = part_library();
    // One def instance `sense[0]` with internal R1/C1; the doc tree places the instance.
    let u = universe(&[("sense[0].R1", "R"), ("sense[0].C1", "Cap")]);
    let fragments: BTreeMap<String, SchematicLayout> =
        [frag_column("sense[0]", &["sense[0].R1", "sense[0].C1"])]
            .into_iter()
            .collect();
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("sense[0]")])],
    };
    let p = reflow(&layout, &u, &lib, &fragments);
    // Totality: both internal components got a coordinate, as a group (not the bin).
    assert_eq!(p.len(), 2);
    // The fragment is a column: C1 sits below R1 (lesser y), same x.
    assert!(p[&EntityId::new("sense[0].C1")].center.y < p[&EntityId::new("sense[0].R1")].center.y);
}

#[test]
fn two_instances_render_with_identical_relative_geometry() {
    let lib = part_library();
    // Two instances of the same def, placed side by side in a row.
    let u = universe(&[
        ("a.R1", "R"),
        ("a.C1", "Cap"),
        ("b.R1", "R"),
        ("b.C1", "Cap"),
    ]);
    let fragments: BTreeMap<String, SchematicLayout> = [
        frag_column("a", &["a.R1", "a.C1"]),
        frag_column("b", &["b.R1", "b.C1"]),
    ]
    .into_iter()
    .collect();
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("a"), sym("b")])],
    };
    let p = reflow(&layout, &u, &lib, &fragments);
    assert_eq!(p.len(), 4);
    // Internal relative geometry: (C1 - R1) offset must be identical across instances —
    // the "renders identically everywhere" guarantee.
    let off = |inst: &str| {
        let r = p[&EntityId::new(format!("{inst}.R1"))].center;
        let c = p[&EntityId::new(format!("{inst}.C1"))].center;
        (c.x - r.x, c.y - r.y)
    };
    assert_eq!(off("a"), off("b"));
}

#[test]
fn nested_def_instance_expands_recursively() {
    let lib = part_library();
    // `outer` is a def instance whose fragment contains ANOTHER def instance `outer.in`.
    // Both are fragment keys; the inner one expands recursively.
    let u = universe(&[("outer.in.R1", "R"), ("outer.C1", "Cap")]);
    let fragments: BTreeMap<String, SchematicLayout> = [
        // `outer`'s fragment places its own C1 and a nested def-instance sym `outer.in`.
        (
            "outer".to_string(),
            SchematicLayout {
                roots: vec![column(vec![sym("outer.C1"), sym("outer.in")])],
            },
        ),
        frag_column("outer.in", &["outer.in.R1"]),
    ]
    .into_iter()
    .collect();
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("outer")])],
    };
    let p = reflow(&layout, &u, &lib, &fragments);
    // Both the outer C1 and the nested R1 land — the nested instance expanded.
    assert_eq!(p.len(), 2);
    assert!(p.contains_key(&EntityId::new("outer.in.R1")));
    assert!(p.contains_key(&EntityId::new("outer.C1")));
}

#[test]
fn doc_level_sym_overrides_fragment_placement() {
    let lib = part_library();
    let u = universe(&[("sense[0].R1", "R"), ("sense[0].C1", "Cap")]);
    let fragments: BTreeMap<String, SchematicLayout> =
        [frag_column("sense[0]", &["sense[0].R1", "sense[0].C1"])]
            .into_iter()
            .collect();
    // The doc places the instance AND overrides `sense[0].R1` with an explicit pinned
    // offset. The doc-level placement must win (the fragment's R1 copy is dropped).
    let layout = SchematicLayout {
        roots: vec![row(vec![
            sym("sense[0]"),
            LayoutNode::Symbol(Symbol {
                path: "sense[0].R1".into(),
                rot: Orient::IDENTITY,
                dx: 7_000_000,
                dy: 0,
            }),
        ])],
    };
    let p = reflow(&layout, &u, &lib, &fragments);
    assert_eq!(p.len(), 2);

    // The doc-level R1 wins: its placement carries the authored dx (+7mm). The fragment
    // would have placed R1 in a column with no dx, so a +7mm-shifted x proves the
    // doc-level sym (a row child with dx) is what reflow kept — the fragment copy was
    // dropped. Reflow the same tree with the doc-level R1 at dx=0 to get the un-shifted
    // baseline for R1's slot, and assert the +7mm delta.
    let base_layout = SchematicLayout {
        roots: vec![row(vec![
            sym("sense[0]"),
            LayoutNode::Symbol(Symbol {
                path: "sense[0].R1".into(),
                rot: Orient::IDENTITY,
                dx: 0,
                dy: 0,
            }),
        ])],
    };
    let pb = reflow(&base_layout, &u, &lib, &fragments);
    assert_eq!(
        p[&EntityId::new("sense[0].R1")].center.x - pb[&EntityId::new("sense[0].R1")].center.x,
        7_000_000,
        "R1 carries the doc-level authored dx (fragment copy was dropped)"
    );

    // And `validate` surfaces the override as a single non-blocking W_SCHEMATIC warning.
    let ids: BTreeSet<EntityId> = u.keys().cloned().collect();
    let (errors, _unplaced, warnings) = validate(&layout, &ids, &dnp(&[]), &fragments);
    assert!(errors.is_empty(), "override is not an error: {errors:?}");
    assert_eq!(warnings.len(), 1, "one override warning: {warnings:?}");
    assert_eq!(warnings[0].code, "W_SCHEMATIC");
    assert!(warnings[0].message.contains("overrides"));
}

#[test]
fn rot_dx_dy_on_def_instance_sym_warns_ignored() {
    // F7: a def-instance sym expands to a group, so an authored rot/dx/dy on it is
    // dropped by reflow — validate must surface that as a non-blocking W_SCHEMATIC so the
    // drop is never invisible. A plain (identity, zero-offset) def-instance sym is silent.
    let u = universe(&[("sense[0].R1", "R")]);
    let ids: BTreeSet<EntityId> = u.keys().cloned().collect();
    let fragments: BTreeMap<String, SchematicLayout> = [frag_column("sense[0]", &["sense[0].R1"])]
        .into_iter()
        .collect();
    // Identity + zero offset: no warning.
    let plain = SchematicLayout {
        roots: vec![row(vec![sym("sense[0]")])],
    };
    let (_e, _u, w) = validate(&plain, &ids, &dnp(&[]), &fragments);
    assert!(w.is_empty(), "plain def-instance sym is silent: {w:?}");
    // A pinned offset on the def-instance sym: one ignored-attr warning.
    let shifted = SchematicLayout {
        roots: vec![row(vec![LayoutNode::Symbol(Symbol {
            path: "sense[0]".into(),
            rot: Orient::IDENTITY,
            dx: 3_000_000,
            dy: 0,
        })])],
    };
    let (errors, _unplaced, warnings) = validate(&shifted, &ids, &dnp(&[]), &fragments);
    assert!(
        errors.is_empty(),
        "ignored attr is not an error: {errors:?}"
    );
    assert_eq!(warnings.len(), 1, "one ignored-attr warning: {warnings:?}");
    assert_eq!(warnings[0].code, "W_SCHEMATIC");
    assert!(warnings[0].message.contains("is ignored"));
}

#[test]
fn fragment_nesting_past_the_cap_is_an_error_not_a_silent_drop() {
    // A chain of def instances each nesting the next, longer than MAX_FRAGMENT_DEPTH.
    // reflow would truncate the over-deep tail silently; validate must reject it with a
    // hard E_SCHEMATIC instead (a silent drop is against the house rules).
    let mut fragments: BTreeMap<String, SchematicLayout> = BTreeMap::new();
    // link[k] is a def instance whose fragment holds the next instance sym link[k+1].
    let n = MAX_FRAGMENT_DEPTH + 2;
    for k in 0..n {
        let child = format!("link{}", k + 1);
        fragments.insert(
            format!("link{k}"),
            SchematicLayout {
                roots: vec![column(vec![sym(&child)])],
            },
        );
    }
    // The doc places the head of the chain.
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("link0")])],
    };
    // No populated components needed — the check is purely on fragment nesting depth.
    let ids: BTreeSet<EntityId> = BTreeSet::new();
    let (errors, _unplaced, _warnings) = validate(&layout, &ids, &dnp(&[]), &fragments);
    assert!(
        errors
            .iter()
            .any(|e| e.code == "E_SCHEMATIC" && e.message.contains("depth cap")),
        "over-deep fragment nesting must be a hard error, got: {errors:?}"
    );
}

#[test]
fn def_instance_with_no_fragment_lands_in_the_bin() {
    let lib = part_library();
    // `sense[0]` is a def instance but has NO fragment (its def declared no schematic
    // block). Its internal components are ordinary components in the universe; the
    // instance sym is neither a component nor a fragment key, so it prunes away and its
    // internals fall to the unplaced bin (unchanged totality behaviour).
    let u = universe(&[("sense[0].R1", "R"), ("sense[0].C1", "Cap")]);
    let fragments: BTreeMap<String, SchematicLayout> = BTreeMap::new();
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("sense[0]")])],
    };
    let p = reflow(&layout, &u, &lib, &fragments);
    // Totality: both internal components still get a coordinate (in the bin).
    assert_eq!(p.len(), 2);
}

#[test]
fn def_instance_sym_does_not_error_but_unknown_still_does() {
    // A def-instance sym path is legal (expands); an unknown-typo path still errors.
    let fragments: BTreeMap<String, SchematicLayout> = [frag_column("sense[0]", &["sense[0].R1"])]
        .into_iter()
        .collect();
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("sense[0]"), sym("NOPE")])],
    };
    let ids = ids(&[("sense[0].R1", "R")]);
    let (errors, _unplaced, _warnings) = validate(&layout, &ids, &dnp(&[]), &fragments);
    // Only `NOPE` errors — the def-instance sym `sense[0]` is legal.
    assert_eq!(errors.len(), 1, "only the typo errors: {errors:?}");
    assert!(errors[0].message.contains("NOPE"));
}

#[test]
fn unknown_path_still_aborts_even_with_dnp_set() {
    // A typo'd path (unknown to both the populated universe AND the DNP-dropped set)
    // stays a hard error, even when some other path is legitimately DNP-dropped.
    let layout = SchematicLayout {
        roots: vec![row(vec![sym("TYPO"), sym("C2")])],
    };
    let (errors, _, _) = validate(
        &layout,
        &ids(&[("C1", "Cap")]),
        &dnp(&["C2"]),
        &BTreeMap::new(),
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("TYPO"));
}
