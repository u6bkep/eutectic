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

/// An empty overlay contributes no `El` (nothing selected, no measure in progress) —
/// so the static-layer cache is the only thing drawn. A populated overlay yields an
/// El keyed `overlay:dynamic`, stacked on top without touching the cached layers.
#[test]
fn empty_overlay_is_none_populated_is_some() {
    use super::Overlay;
    use ecad_core::coord::Point;
    use ecad_core::geom::Shape2D;

    let d = board_domain();
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let canvas = Canvas::new(doc, &d.lib).unwrap();

    // Empty overlay → nothing drawn.
    assert!(canvas.overlay_el(&Overlay::default()).is_none());

    // A highlighted shape → one overlay El keyed `overlay:dynamic`.
    let overlay = Overlay {
        highlights: vec![(
            Shape2D::trace(
                vec![
                    Point {
                        x: 3_000_000,
                        y: 7_000_000,
                    },
                    Point {
                        x: 17_000_000,
                        y: 7_000_000,
                    },
                ],
                500_000,
            ),
            false,
        )],
        measure: None,
        findings: Vec::new(),
        ghost: Vec::new(),
        ratsnest: Vec::new(),
    };
    let el = canvas
        .overlay_el(&overlay)
        .expect("populated overlay draws");
    assert_eq!(el.key.as_deref(), Some("overlay:dynamic"));
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

/// The bounds walk excludes `Role::Datum` fab geometry (and every non-copper /
/// non-silk role), matching `svg.rs`'s in-view set. Regression guard for the
/// framing finding: on the poc board the F.Fab datum text ran ~35 mm wider than the
/// board, which used to blow out the viewBox. With the role filter the canvas
/// viewBox is within a small copper-half-width / silk-extent slack of `svg.rs`'s —
/// same roles in view, same margin, not a huge fab gutter. (It is *not* byte-
/// identical: the canvas frames from inflated `world_features` copper, `svg.rs` from
/// pad centres — see `content_bounds`.)
#[test]
fn bounds_exclude_fab_datum_and_track_svg() {
    for name in ["board", "poc"] {
        let d = if name == "board" {
            crate::fixtures::board_domain()
        } else {
            crate::fixtures::poc_board_domain()
        };
        let doc = d.doc.as_ref().unwrap();
        let canvas = Canvas::new(doc, &d.lib).unwrap();
        let [vx, vy, vw, vh] = canvas.view_box();

        // Parse svg.rs's viewBox="x y w h".
        let svg = ecad_core::export::svg(doc, &d.lib).unwrap();
        let vbline = svg.lines().find(|l| l.contains("viewBox")).unwrap();
        let inner = vbline.split("viewBox=\"").nth(1).unwrap();
        let inner = inner.split('"').next().unwrap();
        let n: Vec<f32> = inner
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        let (sx, sy, sw, sh) = (n[0], n[1], n[2], n[3]);

        // Same framing to within a few mm (copper half-widths / silk extents), NOT the
        // old ~35 mm datum blowout. Each edge agrees within 6 mm.
        assert!((vx - sx).abs() < 6.0, "{name}: viewBox x {vx} vs svg {sx}");
        assert!((vy - sy).abs() < 6.0, "{name}: viewBox y {vy} vs svg {sy}");
        assert!((vw - sw).abs() < 6.0, "{name}: viewBox w {vw} vs svg {sw}");
        assert!((vh - sh).abs() < 6.0, "{name}: viewBox h {vh} vs svg {sh}");
    }

    // Directly assert Datum points are excluded: injecting fab-datum extent must not
    // be able to widen the bounds. We prove this structurally — the bounds walk only
    // admits Conductor | Marking — by checking a doc whose datum runs far outside the
    // board still frames to the board (poc, whose F.Fab text is ~35 mm wide).
    let d = crate::fixtures::poc_board_domain();
    let doc = d.doc.as_ref().unwrap();
    let canvas = Canvas::new(doc, &d.lib).unwrap();
    let [_, _, vw, _] = canvas.view_box();
    // The poc board copper is ~55 mm wide; +4 mm margin ⇒ ~59 mm. If datum leaked in
    // the width would be ~90 mm. Guard the regression.
    assert!(
        vw < 70.0,
        "poc viewBox width {vw} — fab datum leaked into bounds?"
    );
}

/// The **full** screen → board composition the status bar uses:
/// `ViewportView::unproject` then [`Canvas::content_px_to_board_mm`]. Unlike
/// `coordinate_mapping_spot_checks` (which feeds viewBox-mm straight in, bypassing
/// unproject), this exercises the real path end to end and would catch the dropped
/// viewBox-min offset and the non-square rect/viewBox scaling. We forward-project a
/// known board point to a screen coordinate through the exact same maps the renderer
/// uses, then invert and require it round-trips.
#[test]
fn screen_to_board_roundtrip_full_composition() {
    use damascene_core::viewport::ViewportView;

    let d = crate::fixtures::board_domain();
    let doc = d.doc.as_ref().unwrap();
    let canvas = Canvas::new(doc, &d.lib).unwrap();
    let [vx, vy, vw, vh] = canvas.view_box();

    // A deliberately NON-square El rect and NON-trivial pan/zoom, so the aspect-ratio
    // scale and viewBox min both matter (the two corrections the old path dropped).
    let rect = (30.0_f32, 12.0_f32, 800.0_f32, 300.0_f32); // (x, y, w, h)
    let vv = ViewportView {
        pan: (17.0, -9.0),
        zoom: 1.7,
    };
    let origin = (rect.0, rect.1);

    // Forward maps, mirroring the renderer:
    //  board mm --flip--> viewBox mm --(vx,vy)+scale--> content px --project--> screen.
    let board = (7.5_f32, 4.25_f32); // mm
    // flip: view_y_mm = flip_sum - board_y. Recover flip_sum from the canvas's own
    // inverse: view_to_board_mm((_, 0.0)).1 == flip_sum.
    let flip_sum = canvas.view_to_board_mm((0.0, 0.0)).1;
    let view_mm = (board.0, flip_sum - board.1);
    let sx = rect.2 / vw;
    let sy = rect.3 / vh;
    let content_px = (
        rect.0 + (view_mm.0 - vx) * sx,
        rect.1 + (view_mm.1 - vy) * sy,
    );
    let screen = vv.project(content_px, origin);

    // Inverse (the app's path):
    let back_px = vv.unproject(screen, origin);
    let back = canvas
        .content_px_to_board_mm(back_px, (rect.0, rect.1, rect.2, rect.3))
        .expect("non-degenerate rect");
    assert!(
        (back.0 - board.0).abs() < 1e-2 && (back.1 - board.1).abs() < 1e-2,
        "round-trip board ({},{}) -> screen {:?} -> board ({},{})",
        board.0,
        board.1,
        screen,
        back.0,
        back.1
    );

    // A degenerate rect returns None (the renderer draws nothing there).
    assert!(
        canvas
            .content_px_to_board_mm((5.0, 5.0), (0.0, 0.0, 0.0, 100.0))
            .is_none()
    );
}
