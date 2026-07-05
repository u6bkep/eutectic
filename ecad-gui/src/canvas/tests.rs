//! Canvas projection tests: layer enumeration, pour-with-hole structure, and the
//! coordinate mapping — the proof the viewer renders correctly, since the windowed
//! binary can't be run in review.

use super::*;
use crate::fixtures::board_domain;
use damascene_core::prelude::VectorSegment;

/// Count the `MoveTo` commands in a path — one per subpath. A filled region with
/// holes has more than one (outer ring + hole rings).
fn subpath_count(path: &VectorPath) -> usize {
    path.segments
        .iter()
        .filter(|s| matches!(s, VectorSegment::MoveTo(_)))
        .count()
}

fn layer_named<'a>(layers: &'a [BoardLayer], name: &str) -> &'a BoardLayer {
    layers
        .iter()
        .find(|l| l.name == name)
        .unwrap_or_else(|| panic!("no layer named `{name}` in {:?}", names(layers)))
}

fn names(layers: &[BoardLayer]) -> Vec<String> {
    layers.iter().map(|l| l.name.clone()).collect()
}

/// The board fixture enumerates the expected layer set: the derived outline, every
/// stackup slab (the default 2-layer stack: B/F silk, mask, copper, core), and the
/// synthetic drills layer. Order is draw order (bottom-first).
#[test]
fn enumerates_board_layers() {
    let d = board_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let canvas = Canvas::new(doc, &d.lib).unwrap();
    let layers = canvas.build_layers(doc, &d.lib).unwrap();

    let names = names(&layers);
    // The outline is first (painted under everything).
    assert_eq!(layers.first().unwrap().id, LayerId::Outline);
    // Every default-stackup slab is enumerated, plus the derived drills layer.
    for expected in [
        "Board outline",
        "B.SilkS",
        "B.Mask",
        "B.Cu",
        "core",
        "F.Cu",
        "F.Mask",
        "F.SilkS",
        "Drills",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing layer `{expected}` in {names:?}"
        );
    }

    // The copper layers carry the warm-top / cool-bottom palette (a forward stackup
    // query, like svg.rs's layer_color).
    assert_eq!(
        layer_named(&layers, "F.Cu").color,
        super::copper_color_top()
    );
    assert_eq!(
        layer_named(&layers, "B.Cu").color,
        super::copper_color_bottom()
    );
}

/// The pour on F.Cu projects to a filled path whose knockouts are *actual holes* —
/// distinct subpaths beyond the outer ring — under the even-odd fill rule (matching
/// svg.rs). The fixture routes a trace and drops a via through the GND pour, so the
/// pour fill knocks out around both: the pour path must have ≥ 2 subpaths (outer +
/// at least one hole) and use even-odd fill.
#[test]
fn pour_has_real_holes() {
    let d = board_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let canvas = Canvas::new(doc, &d.lib).unwrap();
    let layers = canvas.build_layers(doc, &d.lib).unwrap();

    let f_cu = layer_named(&layers, "F.Cu");
    // The pour is the F.Cu fill with the most subpaths (the trace and via discs are
    // single-ring fills; the pour is the outline minus knockouts).
    let pour = f_cu
        .asset
        .paths
        .iter()
        .max_by_key(|p| subpath_count(p))
        .expect("F.Cu has at least one path");

    assert!(
        subpath_count(pour) >= 2,
        "pour must have a hole (outer ring + ≥1 knockout); got {} subpaths",
        subpath_count(pour)
    );
    assert_eq!(
        pour.fill.map(|f| f.rule),
        Some(VectorFillRule::EvenOdd),
        "pour fill must be even-odd so knockouts read as voids"
    );
    // A knockout implies each ring (outer + hole) closes.
    assert!(
        pour.segments
            .iter()
            .filter(|s| matches!(s, VectorSegment::Close))
            .count()
            >= 2,
        "each of the outer + hole rings must close"
    );
}

/// The coordinate mapping: 1 viewBox unit == 1 mm, y flipped so the board reads
/// upright. A known board point round-trips through the flip, and a feature at a
/// known mm position lands at the expected viewBox coordinate.
#[test]
fn coordinate_mapping_spot_checks() {
    let d = board_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let canvas = Canvas::new(doc, &d.lib).unwrap();

    // The fixture board is (0,0)..(20,15) mm, so with the 2 mm margin the content
    // bounds are (-2,-2)..(22,17) mm and the viewBox is [-2, -2, 24, 19].
    let vb = canvas.view_box();
    assert_eq!(vb, [-2.0, -2.0, 24.0, 19.0]);

    // board_to_view flips y within [y0, y1] = [-2, 17] mm, so flip_sum = 15 mm.
    // A board point at y = 0 mm maps to view y = 15 mm; y = 15 mm maps to 0 mm.
    let flip_sum = 15 * MM;
    let (vx, vy) = super::board_to_view(Point { x: 5 * MM, y: 0 }, flip_sum);
    assert_eq!((vx, vy), (5.0, 15.0));
    let (_, vy_top) = super::board_to_view(Point { x: 0, y: 15 * MM }, flip_sum);
    assert_eq!(vy_top, 0.0);

    // view_to_board_mm is the inverse the status bar uses: a cursor at view
    // (5, 15) mm reads as board (5, 0) mm.
    let (bx, by) = canvas.view_to_board_mm((5.0, 15.0));
    assert!(
        (bx - 5.0).abs() < 1e-4 && (by - 0.0).abs() < 1e-4,
        "got ({bx}, {by})"
    );
    // And a full round-trip: board 12.34 / 5.67 mm → view → board.
    let (rx, ry) = canvas.view_to_board_mm(super::board_to_view(
        Point {
            x: 12_340_000,
            y: 5_670_000,
        },
        flip_sum,
    ));
    assert!(
        (rx - 12.34).abs() < 1e-3 && (ry - 5.67).abs() < 1e-3,
        "got ({rx}, {ry})"
    );
}

/// Visibility filtering never re-tessellates: `layer_els` on the cached layers
/// yields one `El` per visible non-empty layer, and hiding a layer just drops its
/// `El` — the cached assets are untouched.
#[test]
fn visibility_toggles_include_exclude_els() {
    let d = board_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let canvas = Canvas::new(doc, &d.lib).unwrap();
    let layers = canvas.build_layers(doc, &d.lib).unwrap();

    let all = canvas.layer_els(&layers, |_| true);
    let none = canvas.layer_els(&layers, |_| false);
    assert!(none.is_empty(), "hiding every layer yields no Els");

    // Every non-empty layer contributes exactly one El when visible.
    let nonempty = layers.iter().filter(|l| !l.asset.paths.is_empty()).count();
    assert_eq!(all.len(), nonempty);

    // Hiding just F.Cu drops exactly one El (F.Cu is non-empty in the fixture).
    let without_fcu = canvas.layer_els(&layers, |id| id != &LayerId::Slab("F.Cu".to_string()));
    assert_eq!(without_fcu.len(), all.len() - 1);
}

/// The dynamic-overlay seam is empty in milestone 2 (the layered-canvas commitment
/// reserves it for m3+ selection / DRC / tools).
#[test]
fn overlay_is_empty_in_m2() {
    let d = board_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let canvas = Canvas::new(doc, &d.lib).unwrap();
    assert!(canvas.overlay_el().is_none());
}

/// The real 4-layer multiprobe board (poc/out/board.ecad) parses, elaborates, and
/// projects to non-empty assets for the copper layers without panicking — the
/// end-to-end smoke test over a genuine board. Reads the poc file at test time via
/// a path relative to the crate manifest.
#[test]
fn poc_multiprobe_board_projects() {
    let d = crate::fixtures::poc_board_domain();
    let doc = d
        .doc
        .as_ref()
        .expect("poc/out/board.ecad elaborates with the poc library");
    let canvas = Canvas::new(doc, &d.lib).expect("canvas builds for the poc board");
    let layers = canvas.build_layers(doc, &d.lib).expect("layers build");

    // The 4-layer stack has four copper slabs, all present.
    for cu in ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"] {
        assert!(
            layers.iter().any(|l| l.name == cu),
            "missing copper layer `{cu}`"
        );
    }
    // At least one copper layer carries geometry (pads / pours / routed copper).
    let copper_nonempty = layers
        .iter()
        .filter(|l| ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"].contains(&l.name.as_str()))
        .any(|l| !l.asset.paths.is_empty());
    assert!(
        copper_nonempty,
        "no copper geometry projected for the poc board"
    );
    // The inner pours (In1.Cu GND, In2.Cu +3V3) are non-empty fills.
    assert!(
        !layer_named(&layers, "In1.Cu").asset.paths.is_empty(),
        "In1.Cu pour projected empty"
    );
}
