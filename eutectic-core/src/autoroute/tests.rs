use super::*;
use crate::command::{Command, Transaction};
use crate::doc::Point;
use crate::elaborate::{GenDirective as G, Source, board_rect};
use crate::history::History;
use crate::id::TraceId;
use crate::part::part_library;
use crate::query::{Engine, Key};
use crate::route::Violation;

/// Elaborate a source into a routed `History` head (default part library).
fn doc_of(src: Source) -> History {
    doc_of_lib(src, &part_library())
}

/// Elaborate a source into a `History` head against a caller-supplied library (for
/// scenes using footprints that the default library lacks, e.g. real SMD pads).
fn doc_of_lib(src: Source, lib: &crate::part::PartLib) -> History {
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), lib, "src")
        .unwrap();
    h
}

/// Issue 0003: a proposed trace that clashes a different-net pad is dropped by the
/// self-verify (the construction invariant is not trusted), and the net is moved
/// to `unrouted` — `routed` never includes a net whose copper actually violates.
#[test]
fn verify_prunes_a_net_whose_trace_clashes_a_pad() {
    use crate::geom::Shape2D;
    use crate::part::{PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole};
    let pin = PinDef {
        name: "1".into(),
        number: "1".into(),
        role: PinRole::Passive,
        offset: Point { x: 0, y: 0 },
        pad: Some(PadGeo {
            copper: vec![PadCopper {
                shape: Shape2D::rect(Point { x: 0, y: 0 }, 500_000, 500_000),
                layers: PadLayers::Top,
            }],
            drill: None,
        }),
    };
    let mut lib = crate::part::PartLib::new();
    lib.insert(
        "PAD".into(),
        PartDef {
            name: "PAD".into(),
            pins: vec![pin],
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        },
    );
    let src = vec![
        G::Instance {
            path: "b".into(),
            part: "PAD".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Fix {
            path: "b".into(),
            pos: Point { x: 0, y: 0 },
        },
        G::ConnectPins {
            net: "B".into(),
            pins: vec![("b".into(), "1".into())],
        },
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "src")
        .unwrap();

    let mut result = AutorouteResult {
        commands: vec![Command::AddTrace(
            TraceId(1),
            Trace {
                net: NetId::new("A"),
                layer: "F.Cu".into(),
                path: vec![Point::mm(-2, 0), Point::mm(2, 0)],
                width: 200_000,
                prov: Provenance::Free,
            },
        )],
        routed: vec![NetId::new("A")],
        unrouted: vec![],
        ..Default::default()
    };
    verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
    assert!(
        result.commands.is_empty(),
        "trace through a different-net pad must be pruned"
    );
    assert!(
        result.routed.is_empty(),
        "the clashing net must leave `routed`"
    );
    assert!(
        result.unrouted.contains(&NetId::new("A")),
        "and be reported unrouted"
    );
}

/// Apply a proposed transaction's commands to the history head (default library).
fn apply_all(h: &mut History, cmds: Vec<Command>) {
    apply_all_lib(h, cmds, &part_library());
}

/// Apply commands against a caller-supplied library.
fn apply_all_lib(h: &mut History, cmds: Vec<Command>, lib: &crate::part::PartLib) {
    h.commit(Transaction(cmds), lib, "autoroute").unwrap();
}

/// DRC violation set at the current head (default library).
fn drc(h: &History) -> Vec<Violation> {
    drc_lib(h, &part_library())
}

/// DRC violation set against a caller-supplied library.
fn drc_lib(h: &History, lib: &crate::part::PartLib) -> Vec<Violation> {
    let mut eng = Engine::new();
    eng.query(h.doc(), lib, Key::Drc).as_drc().to_vec()
}

fn has_clearance_or_width(v: &[Violation]) -> bool {
    v.iter()
        .any(|x| matches!(x, Violation::Clearance { .. } | Violation::MinWidth { .. }))
}

/// A two-net board on an explicit outline: VBUS (reg.VOUT↔dec.p1) and GND
/// (reg.GND↔dec.p2). reg(LDO)@(0,0), dec(Cap)@(12,0).
fn two_net_board() -> Source {
    vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "dec".into(),
            pos: Point::mm(12, 0),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("reg".into(), "GND".into()), ("dec".into(), "p2".into())],
        },
    ]
}

/// F1 (pour-only connection): a pad tied to its net ONLY through its own plane (no
/// trace/via — an SMD pad on the plane's own slab) is also a no-op on rerun. Its node
/// is a plane seed (in the tree), so the tree-membership skip fires even though
/// pad_on_own_copper (traces/vias only) does not. Two In1.Cu SMD pads over an In1.Cu
/// GND plane, deliberately placed OFF-grid so a naive stub would re-emit each pass.
#[test]
fn rerouting_a_pour_connected_pad_is_no_op() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("In1.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 10),
        Point::mm(0, 10),
    ]);
    let mut src = four_layer_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "g1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        // Off-grid centres (the +70µm shifts) so pad ≠ its nearest grid node.
        G::Place {
            path: "g1".into(),
            pos: Point {
                x: 5 * MM + 70_000,
                y: 5 * MM + 70_000,
            },
        },
        G::Place {
            path: "g2".into(),
            pos: Point {
                x: 15 * MM + 70_000,
                y: 5 * MM + 70_000,
            },
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = doc_of_lib(src, &lib);
    let r1 = autoroute(h.doc(), &lib, &DesignRules::default());
    apply_all_lib(&mut h, r1.commands, &lib);
    let (t, v) = (h.doc().traces.len(), h.doc().vias.len());
    let r2 = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r2.commands.is_empty(),
        "a pour-connected pad must be a no-op on rerun, got {:?}",
        r2.commands
    );
    apply_all_lib(&mut h, r2.commands, &lib);
    assert_eq!(
        (h.doc().traces.len(), h.doc().vias.len()),
        (t, v),
        "no duplicate copper on rerun of a pour-connected net"
    );
}

/// Autoroute makes the previously-unrouted nets pass the ratsnest, and introduces
/// no clearance/width violations (verified through the real DRC query).
#[test]
fn autoroute_two_nets_clean_via_drc() {
    let lib = part_library();
    let mut h = doc_of(two_net_board());

    let before = drc(&h);
    assert!(
        before
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { .. })),
        "expected unrouted nets before routing: {before:?}"
    );

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(r.unrouted, Vec::<NetId>::new(), "both nets should route");
    assert_eq!(r.routed.len(), 2);
    assert!(!r.commands.is_empty());
    // Stats are populated (the pre-verify capability signal, issue 0008): a clean scene
    // that survives verify has pre_verify_routed == final routed and matching commands.
    assert_eq!(
        r.stats.pre_verify_routed, 2,
        "both nets connected pre-verify"
    );
    assert_eq!(
        r.stats.pre_verify_commands,
        r.commands.len(),
        "no clash pruning on a clean 2-net board"
    );

    apply_all(&mut h, r.commands);
    let after = drc(&h);
    assert!(
        after.is_empty(),
        "routed board must be DRC clean, got {after:?}"
    );
}

/// F1 (idempotent rerun): routing, applying, then routing AGAIN emits **zero** commands
/// for the already-connected nets — the router ingests its own committed copper as
/// pre-connected tree membership (own_copper_cells), so every pad is already reachable
/// and nothing new is laid. Without this a second pass silently duplicates clean nets'
/// copper (same-net overlap is invisible to verify AND DRC). Both nets are fully routed
/// after pass 1, so pass 2 is a no-op.
#[test]
fn rerouting_a_connected_net_emits_no_duplicate_copper() {
    let lib = part_library();
    let mut h = doc_of(two_net_board());

    let r1 = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(r1.routed.len(), 2, "both nets route on pass 1");
    let (t1, v1) = (h.doc().traces.len(), h.doc().vias.len());
    apply_all(&mut h, r1.commands);
    let (t1b, v1b) = (h.doc().traces.len(), h.doc().vias.len());
    assert!(t1b > t1 || v1b > v1, "pass 1 laid some copper");

    // Pass 2 on the routed board: the nets are connected, so nothing is emitted.
    let r2 = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r2.commands.is_empty(),
        "rerouting connected nets must emit no copper (no silent duplication), got {:?}",
        r2.commands
    );
    // And applying that empty transaction leaves the geometry byte-identical.
    apply_all(&mut h, r2.commands);
    assert_eq!(
        (h.doc().traces.len(), h.doc().vias.len()),
        (t1b, v1b),
        "no duplicate traces/vias after a second routing pass"
    );
    assert!(drc(&h).is_empty(), "still DRC clean after the rerun");
}

/// F1 (partial extension): a net with only *some* of its copper committed EXTENDS
/// toward the unconnected pad rather than re-laying the existing copper. A 3-pin net is
/// routed, one pin's copper removed to make it partial, then re-routed: the rerun adds
/// copper (reconnecting the orphaned pin) and the net ends up connected, DRC clean.
#[test]
fn rerouting_a_partial_net_extends_without_duplicating() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(-6, -12), Point::mm(30, 12)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c0".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "c0".into(),
            pos: Point::mm(12, 6),
        },
        G::Place {
            path: "c1".into(),
            pos: Point::mm(20, -6),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("reg".into(), "VOUT".into()),
                ("c0".into(), "p1".into()),
                ("c1".into(), "p1".into()),
            ],
        },
    ];
    let mut h = doc_of(src);
    let r1 = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r1.routed.contains(&NetId::new("VBUS")),
        "pass 1 routes VBUS"
    );
    let pass1_cmds = r1.commands.len();
    apply_all(&mut h, r1.commands);
    assert!(drc(&h).is_empty(), "connected + clean after pass 1");

    // Amputate: drop the last VBUS trace so the net is partially routed (an island
    // splits off). This models a stale/edited board the router is re-fired at.
    let victim = *h
        .doc()
        .traces
        .iter()
        .filter(|(_, t)| t.net == NetId::new("VBUS"))
        .map(|(id, _)| id)
        .max_by_key(|id| id.0)
        .expect("VBUS has traces");
    h.commit(
        Transaction::one(Command::RemoveTrace(victim)),
        &lib,
        "amputate",
    )
    .unwrap();
    // Snapshot the surviving VBUS copper (id → path). F1 must leave these UNTOUCHED and
    // build on them — this is what distinguishes an extension from a wholesale re-route
    // (which the vacuous earlier version could not tell apart: both add copper, both
    // reconnect, both end clean). The two discriminators below fail if F1 is off.
    let survivors: std::collections::BTreeMap<u64, Vec<Point>> = h
        .doc()
        .traces
        .iter()
        .filter(|(_, t)| t.net == NetId::new("VBUS"))
        .map(|(id, t)| (id.0, t.path.clone()))
        .collect();
    let before_traces = h.doc().traces.len();
    assert!(
        drc(&h)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
        "VBUS is partial after amputation"
    );

    // Re-fire: the rerun should EXTEND (add copper) to reconnect, not re-route wholesale.
    let r2 = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        !r2.commands.is_empty(),
        "a partial net must be extended on rerun"
    );
    // Discriminator 1: an extension reconnects the ONE orphaned pin, so it emits fewer
    // commands than routing the whole net from scratch did (pass 1). With F1 off the net
    // re-routes wholesale (pin-0 MST over all pins) and this fails.
    assert!(
        r2.commands.len() < pass1_cmds,
        "extension must be cheaper than the from-scratch route ({} vs {}): F1 is \
             re-routing wholesale, not building on committed copper",
        r2.commands.len(),
        pass1_cmds
    );
    apply_all(&mut h, r2.commands);
    assert!(
        h.doc().traces.len() > before_traces,
        "rerun added copper (extension), not a no-op"
    );
    // Discriminator 2: every surviving trace is still present VERBATIM (same id + path)
    // — F1 built on them; it did not rip up and re-lay the net.
    for (id, path) in &survivors {
        let t = h
            .doc()
            .traces
            .get(&crate::id::TraceId(*id))
            .expect("a surviving VBUS trace vanished on rerun (wholesale re-route)");
        assert_eq!(
            &t.path, path,
            "a surviving VBUS trace's geometry changed on rerun (not a clean extension)"
        );
    }
    assert!(
        !drc(&h)
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
        "VBUS reconnected after the extending rerun: {:?}",
        drc(&h)
    );
    assert!(
        drc(&h).is_empty(),
        "and the board is DRC clean: {:?}",
        drc(&h)
    );
}

/// Determinism: the same document autoroutes to byte-identical commands.
#[test]
fn autoroute_is_deterministic() {
    let lib = part_library();
    let h = doc_of(two_net_board());
    let r1 = autoroute(h.doc(), &lib, &DesignRules::default());
    let r2 = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(format!("{:?}", r1.commands), format!("{:?}", r2.commands));
    assert_eq!(r1.routed, r2.routed);
    assert_eq!(r1.unrouted, r2.unrouted);
}

/// A `Pinned` obstacle trace of *another* net, walling the direct path on Top,
/// is avoided: the net still routes (dropping to Bottom through vias) and DRC is
/// clean — clearance-clean *is* the proof it was avoided.
#[test]
fn pinned_obstacle_is_avoided() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "dec".into(),
            pos: Point::mm(12, 0),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
        },
        G::ConnectPins {
            net: "WALL".into(),
            pins: vec![("reg".into(), "VIN".into())],
        },
    ];
    let mut h = doc_of(src);
    let wall = Trace {
        net: NetId::new("WALL"),
        layer: "F.Cu".into(),
        path: vec![Point::mm(6, -10), Point::mm(6, 10)],
        width: 200_000,
        prov: Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), wall)),
        &lib,
        "wall",
    )
    .unwrap();

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r.unrouted.is_empty(),
        "VBUS should route around/under the wall"
    );
    assert!(
        r.commands.iter().any(|c| matches!(c, Command::AddVia(..))),
        "crossing a full-height Top wall should drop to Bottom via a via"
    );

    apply_all(&mut h, r.commands);
    let after = drc(&h);
    assert!(
        !has_clearance_or_width(&after),
        "routing around a Pinned obstacle must stay clearance-clean: {after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
        "VBUS must be fully routed: {after:?}"
    );
}

/// An intentionally impossible net (walled off on *both* layers) is reported as
/// unrouted rather than producing bad copper.
#[test]
fn impossible_net_is_reported_not_botched() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "dec".into(),
            pos: Point::mm(12, 0),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
        },
        G::ConnectPins {
            net: "WALL".into(),
            pins: vec![("reg".into(), "VIN".into())],
        },
    ];
    let mut h = doc_of(src);
    for (id, layer) in [(TraceId(1), "F.Cu"), (TraceId(2), "B.Cu")] {
        let wall = Trace {
            net: NetId::new("WALL"),
            layer: layer.to_string(),
            path: vec![Point::mm(6, -12), Point::mm(6, 12)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        h.commit(Transaction::one(Command::AddTrace(id, wall)), &lib, "wall")
            .unwrap();
    }

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(
        r.unrouted,
        vec![NetId::new("VBUS")],
        "VBUS is walled off both layers"
    );
    assert!(
        r.commands.is_empty(),
        "a failed net must emit no copper, got {:?}",
        r.commands
    );

    let after = drc(&h);
    assert!(
        !has_clearance_or_width(&after),
        "no spurious clearance/width: {after:?}"
    );
    assert!(
        after
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
        "VBUS should remain flagged unrouted: {after:?}"
    );
}

/// A multi-pin (3-pin) net connects all pins (MST-style) and passes the ratsnest.
#[test]
fn autoroute_three_pin_net() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(-6, -12), Point::mm(30, 12)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c0".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "c0".into(),
            pos: Point::mm(12, 6),
        },
        G::Place {
            path: "c1".into(),
            pos: Point::mm(20, -6),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("reg".into(), "VOUT".into()),
                ("c0".into(), "p1".into()),
                ("c1".into(), "p1".into()),
            ],
        },
    ];
    let mut h = doc_of(src);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(r.unrouted.is_empty(), "3-pin net should fully route");
    apply_all(&mut h, r.commands);
    let after = drc(&h);
    assert!(
        after.is_empty(),
        "3-pin routed net must be DRC clean: {after:?}"
    );
}

// ------------------------------------------------------------------------
// N-layer grid, honest masking, pours, keep-outs, pad extents, pitch split.
// ------------------------------------------------------------------------

use crate::doc::MM;
use crate::elaborate::RegionDecl;
use crate::geom::{KeepoutKind, Material, Role, Shape2D, Slab, ZRange};

/// A 4-copper stackup: F.Cu / In1.Cu / In2.Cu / B.Cu with the masks the two outer
/// sides need (so the board stays fully masked / clean), z descending F→B.
fn four_layer_slabs() -> Vec<G> {
    let cu = |name: &str, lo: Nm, hi: Nm| {
        G::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role: Role::Conductor,
            material: Some(Material::named("copper")),
        })
    };
    let other = |name: &str, lo: Nm, hi: Nm, role: Role| {
        G::Slab(Slab {
            name: name.into(),
            z: ZRange::new(lo, hi),
            role,
            material: None,
        })
    };
    // z from bottom (0) up: B.Mask, B.Cu, core, In2, core, In1, core, F.Cu, F.Mask.
    vec![
        other("B.Mask", -25_000, 0, Role::Mask),
        cu("B.Cu", 0, 35_000),
        other("core3", 35_000, 500_000, Role::Substrate),
        cu("In2.Cu", 500_000, 535_000),
        other("core2", 535_000, 1_000_000, Role::Substrate),
        cu("In1.Cu", 1_000_000, 1_035_000),
        other("core1", 1_035_000, 1_565_000, Role::Substrate),
        cu("F.Cu", 1_565_000, 1_600_000),
        other("F.Mask", 1_600_000, 1_625_000, Role::Mask),
    ]
}

/// N-layer routing: on a 4-copper board with both *outer* layers walled off by
/// foreign pinned copper across the whole span, the net still routes — it must use an
/// inner layer — and stays DRC clean. Proves the grid is genuinely N-layer, not 2.
#[test]
fn four_layer_uses_inner_when_outers_blocked() {
    let lib = part_library();
    let mut src = four_layer_slabs();
    src.extend(vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "dec".into(),
            pos: Point::mm(12, 0),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
        },
        G::ConnectPins {
            net: "WALL".into(),
            pins: vec![("reg".into(), "VIN".into())],
        },
    ]);
    let mut h = doc_of(src);
    // Walls on both OUTER copper layers, full board height: no crossing on F/B.
    for (id, layer) in [(TraceId(1), "F.Cu"), (TraceId(2), "B.Cu")] {
        let wall = Trace {
            net: NetId::new("WALL"),
            layer: layer.to_string(),
            path: vec![Point::mm(6, -12), Point::mm(6, 12)],
            width: 200_000,
            prov: Provenance::Pinned,
        };
        h.commit(Transaction::one(Command::AddTrace(id, wall)), &lib, "wall")
            .unwrap();
    }

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r.unrouted.is_empty(),
        "VBUS should route on an inner layer, got unrouted {:?}",
        r.unrouted
    );
    // At least one trace on an inner copper layer proves inner-layer routing.
    let on_inner = r.commands.iter().any(
        |c| matches!(c, Command::AddTrace(_, t) if t.layer == "In1.Cu" || t.layer == "In2.Cu"),
    );
    assert!(
        on_inner,
        "expected a trace on an inner layer: {:?}",
        r.commands
    );
    apply_all(&mut h, r.commands);
    let after = drc(&h);
    assert!(
        !has_clearance_or_width(&after),
        "inner-layer route must stay clearance-clean: {after:?}"
    );
    assert!(
        !after
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("VBUS"))),
        "VBUS must be fully routed: {after:?}"
    );
}

/// A through via blocks its own site on *every* copper layer. Build a 4-layer board,
/// route a net that must change layers (outer walls force a via), and assert the
/// emitted via is a through via (`span: None`) and DRC (which fans it out to all
/// spanned slabs) stays clean — a foreign net cannot occupy the via site on any layer.
#[test]
fn through_via_blocks_all_four_layers() {
    let lib = part_library();
    let mut src = four_layer_slabs();
    src.extend(vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "dec".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "dec".into(),
            pos: Point::mm(12, 0),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
        },
        G::ConnectPins {
            net: "WALL".into(),
            pins: vec![("reg".into(), "VIN".into())],
        },
    ]);
    let mut h = doc_of(src);
    let wall = Trace {
        net: NetId::new("WALL"),
        layer: "F.Cu".into(),
        path: vec![Point::mm(6, -10), Point::mm(6, 10)],
        width: 200_000,
        prov: Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), wall)),
        &lib,
        "wall",
    )
    .unwrap();

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(r.unrouted.is_empty(), "VBUS should route around the wall");
    let via = r
        .commands
        .iter()
        .find_map(|c| match c {
            Command::AddVia(_, v) => Some(v),
            _ => None,
        })
        .expect("crossing a full-height wall drops layers via a via");
    assert_eq!(via.span, None, "vias are through (full copper extent)");
    apply_all(&mut h, r.commands);
    let after = drc(&h);
    assert!(
        !has_clearance_or_width(&after),
        "through-via route must stay clearance-clean on all layers: {after:?}"
    );
}

/// A one-pad SMD footprint on `layer`, 0.4mm square copper at the instance origin.
fn smd_pad(layer: &str) -> crate::part::PartDef {
    crate::kicad::import_footprint(&format!(
        r#"(footprint "SP" (pad "1" smd rect (at 0 0) (size 0.4 0.4) (layers "{layer}")))"#
    ))
    .unwrap()
}

/// Masking: a route cannot cross a board cutout, cannot leave the outline, and honours
/// the edge clearance. Two pads on opposite sides of a central slot cutout that spans
/// the full board height, forcing any route between them through the cutout — which is
/// masked — so the net cannot route.
#[test]
fn route_cannot_cross_a_cutout() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        // A full-height slot cutout down the middle (x 9..11), splitting the board.
        G::Cutout {
            shape: Shape2D::rect(Point::mm(10, 5), 2 * MM, 12 * MM),
        },
        G::Instance {
            path: "l".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "r".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "l".into(),
            pos: Point::mm(4, 5),
        },
        G::Place {
            path: "r".into(),
            pos: Point::mm(16, 5),
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("l".into(), "1".into()), ("r".into(), "1".into())],
        },
    ];
    let h = doc_of_lib(src, &lib);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(
        r.unrouted,
        vec![NetId::new("SIG")],
        "the cutout splits the board — SIG cannot route across it"
    );
    assert!(r.commands.is_empty(), "a failed net emits no copper");
}

/// An authored through-hole (NPTH mounting hole) blocks routing over it on every layer
/// (issue 0025's routing side): the hole is a full-stackup `Role::Void`, invisible to
/// `board_region` (which only subtracts `Cutout`), so the router sees it only via the
/// obstacle stream's Void arm. A big central hole spanning the board height forces any
/// route between two flanking pads through the hole — which is blocked — so SIG fails.
#[test]
fn route_cannot_cross_a_mounting_hole() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        // A large central NPTH hole (12mm dia at (10,5)) — taller than the 10mm board,
        // so it fully spans the height and leaves no channel above or below it.
        G::Hole {
            center: Point::mm(10, 5),
            dia: 12 * MM,
        },
        G::Instance {
            path: "l".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "r".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "l".into(),
            pos: Point::mm(3, 5),
        },
        G::Place {
            path: "r".into(),
            pos: Point::mm(17, 5),
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("l".into(), "1".into()), ("r".into(), "1".into())],
        },
    ];
    let h = doc_of_lib(src, &lib);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(
        r.unrouted,
        vec![NetId::new("SIG")],
        "an 8mm hole spanning the board height blocks the only channel between the pads"
    );
    assert!(r.commands.is_empty(), "a blocked net emits no copper");
}

/// A copper pour of a *foreign* net blocks routing: a board-covering GND pour on F.Cu
/// leaves no F.Cu channel, so a two-pad SIG net (SMD, F.Cu only) cannot route (it has
/// no other layer to escape to on this 2-layer default... it can drop to B.Cu — so to
/// make the pour genuinely block, the pads are B.Cu and the pour is B.Cu too).
#[test]
fn foreign_pour_blocks_routing() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("B.Cu"));
    // A GND pad somewhere + a board-covering GND pour on B.Cu; two SIG pads on B.Cu
    // that the pour walls off (their own layer is flooded by a foreign net, and an SMD
    // pad seeds only on its own layer, so there is no escape).
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 10),
        Point::mm(0, 10),
    ]);
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "g".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "a".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "b".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g".into(),
            pos: Point::mm(1, 1),
        },
        G::Place {
            path: "a".into(),
            pos: Point::mm(5, 5),
        },
        G::Place {
            path: "b".into(),
            pos: Point::mm(15, 5),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g".into(), "1".into())],
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("a".into(), "1".into()), ("b".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "B.Cu".into(),
        }),
    ];
    let h = doc_of_lib(src, &lib);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(
        r.unrouted,
        vec![NetId::new("SIG")],
        "a board-covering foreign pour on the pads' only layer blocks SIG"
    );
}

/// A copper keep-out blocks routing on its layer: a two-pad net whose only straight
/// path is walled by a full-height `Role::Keepout(Copper)` region — and, on the
/// 2-layer default, the keep-out is placed on both copper layers so there is no escape.
#[test]
fn keepout_blocks_routing() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let mut src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 10)),
        G::Instance {
            path: "a".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "b".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "a".into(),
            pos: Point::mm(4, 5),
        },
        G::Place {
            path: "b".into(),
            pos: Point::mm(16, 5),
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("a".into(), "1".into()), ("b".into(), "1".into())],
        },
    ];
    // Full-height copper keep-out down the middle on BOTH copper layers.
    for layer in ["F.Cu", "B.Cu"] {
        src.push(G::Region(RegionDecl {
            shape: Shape2D::rect(Point::mm(10, 5), 2 * MM, 12 * MM),
            role: Role::Keepout(KeepoutKind::Copper),
            net: None,
            layer: layer.into(),
        }));
    }
    let h = doc_of_lib(src, &lib);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert_eq!(
        r.unrouted,
        vec![NetId::new("SIG")],
        "a full-height copper keep-out on both layers blocks SIG"
    );
    assert!(r.commands.is_empty(), "a blocked net emits no copper");
}

/// Pad extents (not points): two 0.4mm pads only 0.5mm apart on the *same* foreign
/// net. A route of a third net threading the 0.1mm gap between their copper is
/// impossible where the old point model (pads as zero-size points) would have let it
/// through. Here the extents block the channel, so the third net detours (or fails) —
/// we assert it does not lay copper *through* the gap by checking DRC stays clean.
#[test]
fn pad_extents_block_where_points_would_not() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    // Two GND pads 0.5mm apart (centres) — copper edges 0.1mm apart. A SIG net's two
    // pads sit above and below, so the straight route runs through the pad gap.
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(10, 10)),
        G::Instance {
            path: "g0".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "s0".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "s1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g0".into(),
            pos: Point {
                x: 5 * MM - 250_000,
                y: 5 * MM,
            },
        },
        G::Place {
            path: "g1".into(),
            pos: Point {
                x: 5 * MM + 250_000,
                y: 5 * MM,
            },
        },
        G::Place {
            path: "s0".into(),
            pos: Point::mm(5, 1),
        },
        G::Place {
            path: "s1".into(),
            pos: Point::mm(5, 9),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g0".into(), "1".into()), ("g1".into(), "1".into())],
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("s0".into(), "1".into()), ("s1".into(), "1".into())],
        },
    ];
    let mut h = doc_of_lib(src, &lib);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    // Whatever the router does, applying it must be DRC clean — the pad extents mean
    // it cannot thread the 0.1mm gap that a point model would have permitted.
    apply_all_lib(&mut h, r.commands, &lib);
    let after = drc_lib(&h, &lib);
    assert!(
        !has_clearance_or_width(&after),
        "routing must respect pad extents, not treat pads as points: {after:?}"
    );
}

/// The trace/via pitch split (the QFN fix, distilled): two adjacent fine-pitch (0.4mm)
/// pads are *individually reachable* — the grid resolves them — where the old
/// via-sized pitch (0.45mm > 0.4mm) could not place a node on each. Route two 2-pad
/// nets whose pads are 0.4mm apart and assert both route DRC-clean.
#[test]
fn fine_pitch_pads_are_individually_reachable() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    // Four pads in a 0.4mm-pitch row: A B A B (nets NA, NB interleaved). Each net's two
    // pads sit 0.8mm apart with the other net's pad between them.
    let mut src = vec![board_rect(Point::mm(-2, -3), Point::mm(3, 3))];
    let xs = [0, 400_000, 800_000, 1_200_000];
    for (k, x) in xs.iter().enumerate() {
        src.push(G::Instance {
            path: format!("p{k}"),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        src.push(G::Place {
            path: format!("p{k}"),
            pos: Point { x: *x, y: 0 },
        });
    }
    src.push(G::ConnectPins {
        net: "NA".into(),
        pins: vec![("p0".into(), "1".into()), ("p2".into(), "1".into())],
    });
    src.push(G::ConnectPins {
        net: "NB".into(),
        pins: vec![("p1".into(), "1".into()), ("p3".into(), "1".into())],
    });
    let mut h = doc_of_lib(src, &lib);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    // Both nets seed a distinct node on each fine-pitch pad; the fine grid resolves
    // them (a coarser via-sized pitch would collapse adjacent pads onto one node).
    // Routing may still need to detour up/around; the point is each pad is reachable
    // and the result is DRC clean.
    apply_all_lib(&mut h, r.commands, &lib);
    let after = drc_lib(&h, &lib);
    assert!(
        !has_clearance_or_width(&after),
        "fine-pitch routing must be clearance-clean: {after:?}"
    );
    // The grid must have seeded each net (nothing dropped for un-seedability): a net
    // with reachable pins is either routed or reported unrouted, never silently gone.
    assert_eq!(
        r.routed.len() + r.unrouted.len(),
        2,
        "both fine-pitch nets are accounted for"
    );
}

/// Via legality is stricter than trace legality (the pitch split): a via pad needs
/// `via_pad/2 + width/2 + clearance` (0.375 mm) of room from any *other* net's copper,
/// which is more than one grid `pitch` (0.30 mm) — so a via may not sit one node away
/// from a foreign trace even though a *trace* one node away is clearance-clean (exactly
/// `pitch − width = clearance`). This is the invariant the old via-sized grid papered
/// over; here we assert it directly at the A* boundary.
#[test]
fn via_legality_is_stricter_than_trace_at_one_pitch() {
    let rules = DesignRules::default();
    let pitch = rules.min_trace_width + rules.min_clearance; // 0.30 mm
    let via_pad = 2 * rules.min_trace_width;
    // via_pad/2 + width/2 + clr = 0.15 + 0.075 + 0.15 = 0.375 mm
    let via_clear = rules.min_clearance + via_pad / 2 + rules.min_trace_width / 2;
    // A trace one node (pitch) from a foreign centreline is clean: pitch − width/2 −
    // width/2 = clearance. A via one node away is not: it needs `via_clear` > pitch.
    assert!(
        pitch >= rules.min_clearance + rules.min_trace_width / 2 + rules.min_trace_width / 2,
        "a trace one pitch from a foreign trace meets clearance (pitch ≥ clr + w)"
    );
    assert!(
        via_clear > pitch,
        "a via needs more than one pitch of room from foreign copper (the pitch split): \
             via_clear={via_clear} > pitch={pitch}"
    );
}

/// End-to-end companion to the invariant above: a dense two-net scene whose greedy
/// solution puts a via near the other net's trunk only routes cleanly *because* via
/// legality forbade the too-close via and forced a detour. Both nets route and DRC is
/// clean — the same scene the Gerber export determinism test exercises, distilled.
#[test]
fn dense_scene_places_vias_clear_of_foreign_copper() {
    let lib = part_library();
    let src = vec![
        board_rect(Point::mm(-6, -10), Point::mm(18, 10)),
        G::Instance {
            path: "reg".into(),
            part: "LDO".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c0".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "c1".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "reg".into(),
            pos: Point::mm(0, 0),
        },
        G::Place {
            path: "c0".into(),
            pos: Point::mm(12, 5),
        },
        G::Place {
            path: "c1".into(),
            pos: Point::mm(12, -5),
        },
        G::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("reg".into(), "VOUT".into()),
                ("c0".into(), "p1".into()),
                ("c1".into(), "p1".into()),
            ],
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![
                ("reg".into(), "GND".into()),
                ("c0".into(), "p2".into()),
                ("c1".into(), "p2".into()),
            ],
        },
    ];
    let mut h = doc_of(src);
    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r.unrouted.is_empty(),
        "both nets route (via legality forced clean via placement): {:?}",
        r.unrouted
    );
    apply_all(&mut h, r.commands);
    let after = drc(&h);
    assert!(
        after.is_empty(),
        "dense routed board must be DRC clean: {after:?}"
    );
}

// ------------------------------------------------------------------------
// Decision 19a — via-permeable foreign pours.
// ------------------------------------------------------------------------

/// A full-board GND plane on In1.Cu, on a 4-copper board, plus a lone SIG pad.
/// The scene the 19a verify/grid tests share.
fn plane_scene(sig_via_at: Point) -> (History, crate::part::PartLib, AutorouteResult) {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(20, 0),
        Point::mm(20, 20),
        Point::mm(0, 20),
    ]);
    let mut src = four_layer_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "s".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        // A GND pad in a corner, well away from the plane centre — declares the net
        // the pour carries (a pour on an unconnected net is rejected at commit).
        G::Instance {
            path: "g".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "s".into(),
            pos: Point::mm(10, 10),
        },
        G::Place {
            path: "g".into(),
            pos: Point::mm(2, 2),
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("s".into(), "1".into())],
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let h = doc_of_lib(src, &lib);
    // A proposed SIG through via sitting inside the GND plane.
    let result = AutorouteResult {
        commands: vec![Command::AddVia(
            crate::id::ViaId(1),
            Via {
                net: NetId::new("SIG"),
                at: sig_via_at,
                span: None,
                drill: 300_000,
                pad: 600_000,
                prov: Provenance::Free,
            },
        )],
        routed: vec![NetId::new("SIG")],
        unrouted: vec![],
        ..Default::default()
    };
    (h, lib, result)
}

/// A via punched into a foreign derived pour verifies **clean** (Decision 19a) AND is
/// DRC-clean once committed — the two verdicts agree, which is what `routed` means.
/// `verify_and_prune` re-derives the world (pours retreat around the proposed via, the
/// automatic anti-pad) and skips pour-vs-solid exactly as `check_drc` does, so a via
/// in a plane is not pruned. Non-vacuous: the via centre is well inside the plane
/// outline, so a naive fill-inclusion test would flag it.
#[test]
fn via_inside_foreign_plane_verifies_clean() {
    let (mut h, lib, mut result) = plane_scene(Point::mm(10, 10));
    verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
    assert!(
        !result.commands.is_empty() && result.unrouted.is_empty(),
        "a via inside a foreign plane must survive verify (fill retreats on re-derive)"
    );
    // End-to-end: committing the via and running the real DRC must also be clean — the
    // committed doc's pours re-derive with the via's anti-pad, so no pour-vs-via short.
    apply_all_lib(&mut h, result.commands, &lib);
    let after = drc_lib(&h, &lib);
    assert!(
        !has_clearance_or_width(&after),
        "the committed via-in-plane must be DRC clean (re-derived anti-pad): {after:?}"
    );
}

/// The complement: a via too close to another net's **non-pour** copper still fails
/// verify. A GND SMD pad (solid copper) sits at (10,10); a SIG via placed right on top
/// of it clashes (solid-vs-solid is not exempt — only the pour retreats). Proves the
/// 19a exemption is scoped to pours, not a blanket "vias never clash".
#[test]
fn via_on_foreign_solid_copper_is_pruned() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let src = vec![
        board_rect(Point::mm(0, 0), Point::mm(20, 20)),
        G::Instance {
            path: "g".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g".into(),
            pos: Point::mm(10, 10),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g".into(), "1".into())],
        },
    ];
    let h = doc_of_lib(src, &lib);
    // A SIG via directly on the GND pad's F.Cu copper — a real short, must prune.
    let mut result = AutorouteResult {
        commands: vec![Command::AddVia(
            crate::id::ViaId(1),
            Via {
                net: NetId::new("SIG"),
                at: Point::mm(10, 10),
                span: None,
                drill: 300_000,
                pad: 600_000,
                prov: Provenance::Free,
            },
        )],
        routed: vec![NetId::new("SIG")],
        unrouted: vec![NetId::new("SIG")],
        ..Default::default()
    };
    // (SIG already unrouted for its lone pad; the point is the via command is dropped.)
    result.unrouted.clear();
    result.routed = vec![NetId::new("SIG")];
    verify_and_prune(h.doc(), &lib, &DesignRules::default(), &mut result);
    assert!(
        result.commands.is_empty(),
        "a via on a foreign net's SOLID pad copper must be pruned (only pours yield)"
    );
    assert!(result.unrouted.contains(&NetId::new("SIG")));
}

/// A foreign plane still blocks TRACE placement on its own slab (Decision 19a: planes
/// are not signal layers this round). Build the grid's per-net obstacle map with a
/// full-board GND plane on In1.Cu and assert the trace mask on In1.Cu is set inside the
/// plane, while the via mask at that same cell is NOT (the via is permeable there).
#[test]
fn foreign_plane_blocks_trace_but_not_via_in_blockmap() {
    let (h, lib, _r) = plane_scene(Point::mm(10, 10));
    let doc = h.doc();
    let rules = DesignRules::default();
    let su = stackup(&doc.source);
    let layers: Vec<Layer> = copper_layers_z(&su).into_iter().map(|(l, _)| l).collect();
    let nl = layers.len();
    let in1 = layers
        .iter()
        .position(|&l| layer_slab_name(&su, l).as_deref() == Some("In1.Cu"))
        .expect("In1.Cu present");
    let width = rules.min_trace_width;
    let via_pad = 2 * rules.min_trace_width;
    let pitch = rules.min_trace_width + rules.min_clearance;
    let area = crate::solve::Rect {
        min: Point::mm(0, 0),
        max: Point::mm(20, 20),
    };
    let grid = Grid::new(area, pitch, nl);
    let board_mask = BoardMask::build(doc, &grid, &rules, width);
    let netlist = doc_netlist(doc);
    // Per-net pads (needed for the bare-pin obstacle pass, empty here for SIG).
    let mut net_pads: BTreeMap<NetId, Vec<Pad>> = BTreeMap::new();
    net_pads.insert(NetId::new("SIG"), Vec::new());
    net_pads.insert(NetId::new("GND"), Vec::new());
    let block = BlockMap::build(
        &grid,
        &board_mask,
        doc,
        &lib,
        &rules,
        &su,
        &netlist,
        &net_pads,
        &NetId::new("SIG"),
        width,
        via_pad,
    );
    // A cell deep inside the plane, away from the board edge AND from any pad (the
    // SIG pad sits at (10,10), the GND pad at (2,2)).
    let (ci, cj) = (
        ((15 * MM - area.min.x) / pitch) as usize,
        ((15 * MM - area.min.y) / pitch) as usize,
    );
    let idx = grid.idx(ci, cj);
    assert!(
        block.trace[idx * nl + in1],
        "the foreign GND plane must block TRACE placement on In1.Cu"
    );
    assert!(
        !block.via[idx],
        "but a via may punch the plane — the via-site mask is clear (Decision 19a)"
    );
    assert!(
        !block.via_layer[idx * nl + in1],
        "and the via barrel's In1.Cu room test ignores the permeable pour"
    );
}

// ------------------------------------------------------------------------
// Decision 19b — same-net plane fills are stitching targets.
// ------------------------------------------------------------------------

/// A net with its own plane routes pad→plane with a stitching via. Two GND SMD pads on
/// F.Cu are separated by a full-height foreign wall on F.Cu (so no direct F.Cu path)
/// but share a GND plane on In1.Cu. The router seeds the tree with the plane cells and
/// drops a stitching via from each pad down to the plane, connecting GND — one island,
/// DRC clean. Without 19b (no plane seeding) the wall would leave GND unrouted.
#[test]
fn net_stitches_to_own_plane_via_one_via_each() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(30, 0),
        Point::mm(30, 20),
        Point::mm(0, 20),
    ]);
    let mut src = four_layer_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(30, 20)),
        G::Instance {
            path: "g1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        // A foreign net pad so WALL is a real net we can wall F.Cu with.
        G::Instance {
            path: "w".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 10),
        },
        G::Place {
            path: "g2".into(),
            pos: Point::mm(25, 10),
        },
        G::Place {
            path: "w".into(),
            pos: Point::mm(15, 1),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::ConnectPins {
            net: "WALL".into(),
            pins: vec![("w".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = doc_of_lib(src, &lib);
    // A full-height WALL trace on F.Cu between the two GND pads: no direct F.Cu route.
    let wall = Trace {
        net: NetId::new("WALL"),
        layer: "F.Cu".into(),
        path: vec![Point::mm(15, -2), Point::mm(15, 22)],
        width: 200_000,
        prov: Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), wall)),
        &lib,
        "wall",
    )
    .unwrap();

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    assert!(
        r.routed.contains(&NetId::new("GND")),
        "GND stitches to its own In1.Cu plane despite the F.Cu wall: routed={:?} unrouted={:?}",
        r.routed,
        r.unrouted
    );
    assert!(
        r.commands.iter().any(|c| matches!(c, Command::AddVia(..))),
        "connecting a pad to an inner plane needs a stitching via: {:?}",
        r.commands
    );
    apply_all_lib(&mut h, r.commands, &lib);
    let after = drc_lib(&h, &lib);
    assert!(
        !after
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "GND must be one island after stitching: {after:?}"
    );
    assert!(
        !has_clearance_or_width(&after),
        "stitched board must be clearance-clean: {after:?}"
    );
}

/// A fragmented own-plane leaves honest ratsnest islands (Decision 19b): seeding the
/// tree with all own-fill cells does NOT let the router claim a split plane is one node
/// — the ratsnest (layer-honest, Task A) is the judge. Here a foreign trace cuts the
/// GND In1.Cu plane in two, and each GND pad can only reach the island on its own side,
/// so GND stays unrouted (>1 island) even though tree completion might have merged them.
#[test]
fn fragmented_own_plane_stays_unrouted() {
    let mut lib = part_library();
    lib.insert("SP".into(), smd_pad("F.Cu"));
    let outline = Shape2D::polygon(vec![
        Point::mm(0, 0),
        Point::mm(30, 0),
        Point::mm(30, 20),
        Point::mm(0, 20),
    ]);
    let mut src = four_layer_slabs();
    src.extend(vec![
        board_rect(Point::mm(0, 0), Point::mm(30, 20)),
        G::Instance {
            path: "g1".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Instance {
            path: "g2".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        // A foreign net whose full-height In1.Cu trace splits the GND plane in two.
        G::Instance {
            path: "w".into(),
            part: "SP".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        },
        G::Place {
            path: "g1".into(),
            pos: Point::mm(5, 10),
        },
        G::Place {
            path: "g2".into(),
            pos: Point::mm(25, 10),
        },
        G::Place {
            path: "w".into(),
            pos: Point::mm(15, 1),
        },
        G::ConnectPins {
            net: "GND".into(),
            pins: vec![("g1".into(), "1".into()), ("g2".into(), "1".into())],
        },
        G::ConnectPins {
            net: "SIG".into(),
            pins: vec![("w".into(), "1".into())],
        },
        G::Region(RegionDecl {
            shape: outline,
            role: Role::Conductor,
            net: Some("GND".into()),
            layer: "In1.Cu".into(),
        }),
    ]);
    let mut h = doc_of_lib(src, &lib);
    // A full-height SIG trace ON THE PLANE'S SLAB (In1.Cu) cutting it into left/right
    // islands. The knockout carves a clearance channel through the GND fill.
    let cut = Trace {
        net: NetId::new("SIG"),
        layer: "In1.Cu".into(),
        path: vec![Point::mm(15, -2), Point::mm(15, 22)],
        width: 600_000,
        prov: Provenance::Pinned,
    };
    h.commit(
        Transaction::one(Command::AddTrace(TraceId(1), cut)),
        &lib,
        "cut",
    )
    .unwrap();

    let r = autoroute(h.doc(), &lib, &DesignRules::default());
    apply_all_lib(&mut h, r.commands, &lib);
    let after = drc_lib(&h, &lib);
    // GND's two pads land on opposite plane islands; stitching each to its own island
    // does not connect them (only a route crossing the cut would). Honest: unrouted.
    assert!(
        after
            .iter()
            .any(|v| matches!(v, Violation::Unrouted { net, .. } if *net == NetId::new("GND"))),
        "a fragmented own-plane must leave GND with >1 ratsnest island: {after:?}"
    );
    assert!(
        !r.routed.contains(&NetId::new("GND")),
        "the router must NOT claim GND routed when its plane is fragmented (reconciled \
             against the ratsnest): routed={:?}",
        r.routed
    );
}
