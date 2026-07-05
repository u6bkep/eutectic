//! Board hit-testing: the pure pointer-to-entity pick path (milestone 3).
//!
//! This is the "hit-testing is ours" half of the canvas strategy
//! (`docs/gui-architecture.md`, "Canvas strategy"): damascene hit-tests chrome,
//! but the canvas *interior* is one keyed viewport `El`, so mapping a pointer to a
//! board entity is our job. The composition is:
//!
//! 1. pointer logical-px → board mm via the m2 coordinate machinery
//!    ([`Canvas::content_px_to_board_mm`](super::Canvas::content_px_to_board_mm) —
//!    undoes pan/zoom, the viewBox min offset, the (possibly non-square) rect scale,
//!    and the y-flip), then mm → nm for the exact-integer geometry kernel;
//! 2. a **screen-px pick tolerance** converted to a board distance through the
//!    current zoom, so a 6-px grab radius stays 6 px on screen at every zoom (picking
//!    does not get harder as you zoom out — the tolerance grows in board space);
//! 3. a candidate walk over the **same `world_features` stream the canvas renders**
//!    (issue 0031): each derived [`NetFeature`] now carries a
//!    [`FeatureOrigin`](ecad_core::geom::FeatureOrigin) naming the source entity it
//!    was lowered from, so a rendered feature maps straight back to a selectable id
//!    with no second walk over the doc. Each candidate is one [`Candidate`] pairing a
//!    [`SemanticId`] with the world-space [`Shape2D`] to test and the [`LayerId`] it
//!    lives on;
//! 4. containment: a candidate hits when the query point's distance to its copper region
//!    is within the tolerance. That region — a filled pad/pour's exact area, a zero-area
//!    trace/via's honest inflated copper extent — is tessellated **once** at candidate
//!    build time through the same [`shape_to_region`] kernel the SVG backend and DRC use;
//!    the per-event pick then does an integer AABB reject and a point-to-region distance
//!    test on that cached region (no per-event offset/re-tessellation);
//! 5. resolution: among hits on **visible** layers, the most *specific* wins
//!    ([`PickPriority`]): pad/pin ▸ trace ▸ via ▸ pour ▸ board outline. Ties within a
//!    priority break by top-most layer (highest z) first.
//!
//! # Provenance: one stream, no double-walk (issue 0031)
//!
//! `ecad-core`'s [`world_features`](ecad_core::route::world_features) stream is *the*
//! render producer, and each [`NetFeature`](ecad_core::geom::NetFeature) now carries a
//! [`FeatureOrigin`](ecad_core::geom::FeatureOrigin) naming the source entity it was
//! derived from — the trace / via / component-pad / pour / board / silk it belongs to
//! — populated at derivation where that entity is in hand. Mapping a rendered feature
//! back to a selectable entity id is a pure `FeatureOrigin → SemanticId` fold.
//!
//! So the picker walks **the same stream the canvas renders** (there is no second walk
//! over the doc). [`candidates`] filters that stream to the copper it can attribute
//! (pad ▸ trace ▸ via ▸ pour) and maps each origin to a [`SemanticId`], reusing the
//! feature's own [`Shape2D`] and z — the identical copper extent the canvas draws.
//! Origins that name no selectable board entity (board substrate/mask, drill/mask
//! `Void`s, silk/text markings, [`Unattributed`](ecad_core::geom::FeatureOrigin::Unattributed))
//! contribute no candidate. This deletes the former doc-rebuild walk: render and pick
//! can no longer silently diverge because they consume one producer.

use super::LayerId;
use damascene_core::viewport::ViewportView;
use ecad_core::coord::{MM, Nm, Point};
use ecad_core::doc::Doc;
use ecad_core::geom::kernel::{DEFAULT_CIRCLE_SEGS, Region, shape_to_region};
use ecad_core::geom::{Extent, FeatureOrigin, NetFeature, Role, Shape2D, Stackup};
use ecad_core::id::{EntityId, NetId, TraceId, ViaId};
use ecad_core::part::PartLib;
use ecad_core::route::{DesignRules, world_features};

/// A stable, geometry-free semantic identity — the currency of the selection model
/// (structural commitment 2). Every variant is an id (or a small id tuple), **never**
/// a rect, point, or layer index, so the model survives re-elaboration and projects
/// into any pane's overlay. See [`crate::selection::SelectionModel`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SemanticId {
    /// A placed component, by its stable entity id (the refdes is a derived label).
    Part(EntityId),
    /// A whole net, by id. Reached by picking a member trace / via / pin / pour and
    /// (milestone 4) by the net explorer.
    Net(NetId),
    /// A routed trace, by id.
    Trace(TraceId),
    /// A via, by id.
    Via(ViaId),
    /// A copper pour, identified by its net + copper-layer name (a pour has no id of
    /// its own; net+layer is its stable authored identity).
    Pour { net: NetId, layer: String },
    /// A single pin/pad of a placed component, identified by the owning component +
    /// **pad number** — `PinRef`'s two parts, spelled out so the id is `Hash + Ord`.
    /// The `pin` field is the pad *number* (the stable symbol↔footprint join key that
    /// `PinRef` and net membership key on), **not** the functional pin name; the
    /// inspector derives the display name from the number. See `docs/gui-architecture.md`
    /// and `PinRef`'s contract (`ecad_core::doc::PinRef`).
    Pin { comp: EntityId, pin: String },
}

/// Pick priority: **lower wins**. Smaller / more-specific features beat larger ones so
/// a pad on a trace on a pour resolves to the pad, matching the documented ordering
/// (pad/pin ▸ trace ▸ via ▸ pour ▸ board outline).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PickPriority {
    /// A pad / pin — the most specific copper.
    Pad = 0,
    /// A routed trace.
    Trace = 1,
    /// A via.
    Via = 2,
    /// A copper pour (large area).
    Pour = 3,
    /// The board outline (the whole board — last resort).
    Outline = 4,
}

/// One thing the pointer could land on: a semantic id, the visual layer it lives on (so
/// the pick can skip hidden layers), and the **pre-derived** hit-test geometry. Built by
/// [`candidates`]; never stored in the selection model.
///
/// # Derived-state discipline (the perf contract)
///
/// All geometry-kernel work is hoisted here, at candidate build time (per doc revision,
/// cached in `DerivedCaches`): [`region`](Candidate::region) is the shape's copper extent
/// tessellated **once**, and [`aabb`](Candidate::aabb) is that region's integer bounding
/// box. [`resolve`] is then a pure per-event lookup — an integer AABB reject followed by a
/// point-to-region distance test on the *cached* region — with **no** per-event offset or
/// re-tessellation. The zoom-dependent tolerance is applied as a distance threshold at
/// query time, so it is never an input to any cached state.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The id this candidate selects when it wins the pick.
    pub id: SemanticId,
    /// The world-frame (nm, y-up) copper/area shape — retained for the overlay highlight
    /// geometry (`app::panels`) and the halo location (`findings`). **Not** used on the
    /// per-event hit-test path; [`region`](Candidate::region) is what [`resolve`] tests.
    pub shape: Shape2D,
    /// The shape's copper extent tessellated to a [`Region`] **once** at build time (the
    /// same [`shape_to_region`] realisation the old per-event `hits` produced, but at the
    /// shape's own radius, tolerance-free). [`resolve`]'s narrow phase distance-tests the
    /// query point against this immutable region — no per-event inflate/tessellate.
    pub region: Region,
    /// The `region`'s integer axis-aligned bounding box `(min, max)` in nm — the broad
    /// phase reject. A pointer whose position, minus the tolerance, falls outside this box
    /// (saturating) cannot be within tolerance of the region, so it is skipped before the
    /// distance test. A degenerate (empty) region yields a zero-extent box at the origin;
    /// such a candidate never hits, matching the old empty-region containment.
    pub aabb: (Point, Point),
    /// The visual layer this candidate sits on — matched against the visibility
    /// predicate so hidden layers are not pickable.
    pub layer: LayerId,
    /// This candidate's pick priority (lower wins).
    pub priority: PickPriority,
    /// The candidate's top z in nm, for the top-most-layer tie-break within a priority.
    pub z_top: Nm,
}

/// The result of a successful pick: the winning id plus which layer it was on. The
/// caller folds `id` into the [`SelectionModel`](crate::selection::SelectionModel) and
/// uses `layer` only for the overlay accent (it is *not* stored in the model).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    /// The selected entity.
    pub id: SemanticId,
    /// The layer the winning candidate was on.
    pub layer: LayerId,
}

/// Convert a screen-px pick tolerance to a board-space distance in nm through the
/// current viewport zoom. At `zoom == 1.0`, one logical px is one mm (the m2 viewBox
/// convention), so `tol_mm = tol_px / zoom` and `tol_nm = tol_mm * MM`. As you zoom
/// **out** (`zoom < 1`) the board-space tolerance *grows*, keeping the on-screen grab
/// radius constant. Guards a non-positive/NaN zoom to a 1.0 fallback.
pub fn tolerance_nm(tol_px: f32, zoom: f32) -> Nm {
    let z = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };
    let tol_mm = tol_px / z;
    (tol_mm * MM as f32).round() as Nm
}

/// Map a viewport pointer (logical px, window-relative) to a board point in **nm**
/// (y-up), composing the m2 screen→board machinery: unproject through the viewport
/// (pan/zoom removed) then [`Canvas::content_px_to_board_mm`] (viewBox offset + rect
/// scale + y-flip), then mm→nm. `None` for a degenerate rect (matches the renderer,
/// which draws nothing there).
///
/// [`Canvas::content_px_to_board_mm`]: super::Canvas::content_px_to_board_mm
pub fn pointer_to_board_nm(
    canvas: &super::Canvas,
    pointer_px: (f32, f32),
    el_rect: (f32, f32, f32, f32),
    vv: ViewportView,
) -> Option<Point> {
    let (rx, ry, ..) = el_rect;
    let content_px = vv.unproject(pointer_px, (rx, ry));
    let (mx, my) = canvas.content_px_to_board_mm(content_px, el_rect)?;
    Some(Point {
        x: (mx * MM as f32).round() as Nm,
        y: (my * MM as f32).round() as Nm,
    })
}

/// Build every pickable [`Candidate`] for a doc by folding the **same
/// [`world_features`] stream the canvas renders** (issue 0031): each derived feature's
/// [`FeatureOrigin`] maps to a [`SemanticId`], and the feature's own [`Shape2D`] + z is
/// the pick geometry. Only copper the origin can attribute to a selectable board entity
/// yields a candidate — pad ▸ trace ▸ via ▸ pour; substrate/mask/void/silk features
/// (and any [`Unattributed`](FeatureOrigin::Unattributed)) contribute none. Pure over
/// the doc + library + stackup; the render cache is untouched. Deterministic
/// (`world_features` emits in a stable source order).
///
/// This replaced the former doc-rebuild walk (`doc.traces`/`vias`/`components` +
/// `elaborate::regions`): render and pick now consume one producer and cannot silently
/// diverge. `panic`-free — a committed doc never fails lowering (the commit-time slab
/// gate); an unexpected lowering error degrades to *no* candidates (the whole board
/// simply becomes unpickable) rather than crashing the UI.
pub fn candidates(doc: &Doc, lib: &PartLib, su: &Stackup) -> Vec<Candidate> {
    let Ok(features) = doc_world_features(doc, lib, su) else {
        return Vec::new();
    };
    let mut out: Vec<Candidate> = Vec::new();

    for nf in &features {
        // Map the source-entity provenance to a selectable id + pick priority. Only the
        // four attributable copper kinds are pickable; everything else (board body,
        // mask, drill/mask voids, silk/text, Unattributed) names no board-entity id.
        let (id, priority) = match &nf.origin {
            FeatureOrigin::Pad { comp, pad } => (
                SemanticId::Pin {
                    comp: comp.clone(),
                    // The pad NUMBER — the `PinRef`/net-membership join key — flows
                    // straight through from `FeatureOrigin::Pad` (the engine tags it
                    // with `pin.number`), never the functional name. This preserves the
                    // m3 pin-identity contract by construction.
                    pin: pad.clone(),
                },
                PickPriority::Pad,
            ),
            FeatureOrigin::Trace(tid) => (SemanticId::Trace(*tid), PickPriority::Trace),
            FeatureOrigin::Via(vid) => (SemanticId::Via(*vid), PickPriority::Via),
            FeatureOrigin::Region {
                net: Some(net),
                layer,
            } => (
                SemanticId::Pour {
                    net: net.clone(),
                    layer: layer.clone(),
                },
                PickPriority::Pour,
            ),
            // Netless region (keep-out), board body / mask, board / footprint markings,
            // and Unattributed name no selectable board entity — not pickable.
            FeatureOrigin::Region { net: None, .. }
            | FeatureOrigin::ComponentMarking(_)
            | FeatureOrigin::Board
            | FeatureOrigin::BoardText
            | FeatureOrigin::Unattributed => continue,
        };

        // Only copper is pickable: the pad/via *drill* `Void`s share their entity's
        // origin (Pad / Via) but are not selectable copper — filter by role so a via's
        // barrel copper is a candidate while its plated-drill Void is not.
        if nf.feature.role != Role::Conductor {
            continue;
        }

        let Extent::Prism { shape, z } = &nf.feature.extent;
        // The visual layer this copper sits on — the slab whose z it matches (a via
        // barrel fans to one Conductor feature per spanned copper slab, so a via yields
        // one candidate per slab, keeping it pickable on any visible layer). A pour's
        // z is its copper slab; a trace/pad's z is its slab.
        let Some(slab) = super::slab_of_z(su, z) else {
            continue;
        };
        // Hoist ALL geometry-kernel work here (per-revision, cached in DerivedCaches):
        // tessellate the copper extent to a Region ONCE (the same realisation the old
        // per-event `hits` produced via `shape_to_region`, but at the shape's own radius —
        // tolerance is applied as a distance threshold at query time, not baked in), and
        // take its integer AABB for the broad-phase reject. An `Area` shape short-circuits
        // to a region clone inside `shape_to_region` (no tessellation).
        let region = shape_to_region(shape, DEFAULT_CIRCLE_SEGS);
        // The region's own bbox is the honest broad-phase box: `resolve` distance-tests
        // this exact region, and every ring vertex lies within it. An empty region (no
        // rings ≥ 3 verts) has no bbox and can never hit; a zero-extent box at origin is a
        // safe placeholder (the narrow-phase `point_within` still returns false).
        let aabb = region
            .bbox()
            .unwrap_or((Point { x: 0, y: 0 }, Point { x: 0, y: 0 }));
        out.push(Candidate {
            id,
            shape: shape.clone(),
            region,
            aabb,
            layer: LayerId::Slab(slab.name.clone()),
            priority,
            z_top: z.hi,
        });
    }

    // Board outline: intentionally not emitted. A click on bare board must *clear* the
    // selection (per the m3 spec); an always-present outline candidate would make
    // "click empty canvas clears" impossible. A future board-properties selection would
    // add it here (the `FeatureOrigin::Board` features already carry the identity).

    out
}

/// Build the `world_features` stream for a doc with default design rules — the one
/// producer the canvas renders and the picker folds, so render and pick always agree
/// (issue 0031). The GUI-side twin of `canvas::doc_world_features`.
fn doc_world_features(doc: &Doc, lib: &PartLib, su: &Stackup) -> Result<Vec<NetFeature>, String> {
    world_features(
        doc,
        lib,
        &super::doc_netlist(doc),
        &DesignRules::default(),
        su,
    )
}

/// Resolve a board-space query point (nm) to the winning [`Pick`], honoring the
/// visibility predicate and the priority ordering. Pure and unit-testable, and — by the
/// derived-state contract on [`Candidate`] — a **pure per-event lookup**: no geometry
/// derivation runs here, only integer compares and a distance test against each
/// candidate's pre-built region.
///
/// `visible(&LayerId) -> bool` is the canvas's own visibility test (a hidden layer is
/// not pickable). `tol_nm` is the board-space grab radius from [`tolerance_nm`].
///
/// Two phases per candidate:
/// - **broad phase** — [`aabb_admits`]: reject any candidate whose AABB, grown by
///   `tol_nm`, excludes `p` (saturating integer compares; overflow-safe near the
///   coordinate extremes). This lands ~100× on the poc board.
/// - **narrow phase** — [`Region::point_within`]: on the few survivors, the point is a
///   hit when its distance to the cached region is `≤ tol_nm` (inside counts as 0). This
///   is the offset-free equivalent of the old "inside shape inflated by tol" test.
///
/// Among hits the winner is the lowest [`PickPriority`], breaking ties by highest `z_top`
/// (top-most layer). `None` when nothing (visible) is within tolerance.
pub fn resolve<'a>(
    cands: &'a [Candidate],
    p: Point,
    tol_nm: Nm,
    visible: impl Fn(&LayerId) -> bool,
) -> Option<Pick> {
    let mut best: Option<&'a Candidate> = None;
    for c in cands {
        if !visible(&c.layer) {
            continue;
        }
        // Broad phase: integer AABB reject before touching the region.
        if !aabb_admits(c.aabb, p, tol_nm) {
            continue;
        }
        // Narrow phase: exact point-to-region distance against the cached region.
        if !c.region.point_within(p, tol_nm) {
            continue;
        }
        best = Some(match best {
            None => c,
            Some(b) => {
                // Lower priority wins; tie → higher z_top (top-most layer) wins.
                if (c.priority, std::cmp::Reverse(c.z_top))
                    < (b.priority, std::cmp::Reverse(b.z_top))
                {
                    c
                } else {
                    b
                }
            }
        });
    }
    best.map(|c| Pick {
        id: c.id.clone(),
        layer: c.layer.clone(),
    })
}

/// Broad-phase reject: could point `p` be within `tol_nm` of a region whose AABB is
/// `(min, max)`? True iff `p` lies in the box grown by `tol_nm` on every side. The grow
/// and the box edges are computed with **saturating** integer arithmetic so a coordinate
/// near the `Nm` (i64) extremes cannot overflow into a wrong verdict — the reject is
/// conservative (it never drops a candidate that the narrow phase would accept). A
/// negative `tol_nm` floors at 0 (never shrinks the box).
fn aabb_admits((min, max): (Point, Point), p: Point, tol_nm: Nm) -> bool {
    let tol = tol_nm.max(0);
    p.x >= min.x.saturating_sub(tol)
        && p.x <= max.x.saturating_add(tol)
        && p.y >= min.y.saturating_sub(tol)
        && p.y <= max.y.saturating_add(tol)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::board_domain;
    use ecad_core::coord::MM;

    /// A board point at `(x, y)` in mm.
    fn mm(x: f64, y: f64) -> Point {
        Point {
            x: (x * MM as f64) as Nm,
            y: (y * MM as f64) as Nm,
        }
    }

    /// The board fixture's (doc, candidates) — the pick input.
    fn fixture() -> (Vec<Candidate>, ecad_core::part::PartLib) {
        let d = board_domain();
        let doc = d.doc.as_ref().expect("board fixture elaborates").clone();
        let su = ecad_core::elaborate::stackup(&doc.source);
        (candidates(&doc, &d.lib, &su), d.lib)
    }

    /// Every layer visible (the default pick predicate).
    fn all_visible(_: &LayerId) -> bool {
        true
    }

    /// Clicking on the VBUS trace (which runs y=7mm from x=3 to x=17) selects that
    /// trace id. Point (10, 7) is squarely on the trace centreline.
    #[test]
    fn click_on_trace_selects_trace() {
        let (cands, _) = fixture();
        let pick = resolve(&cands, mm(10.0, 7.0), 0, all_visible).expect("trace hit");
        assert!(
            matches!(pick.id, SemanticId::Trace(_)),
            "expected a trace, got {:?}",
            pick.id
        );
        assert_eq!(pick.layer, LayerId::Slab("F.Cu".to_string()));
    }

    /// A point inside the GND pour (its outline is (1,1)-(19,14)) but clear of the
    /// trace/via selects the pour with the GND net.
    #[test]
    fn click_inside_pour_selects_pour() {
        let (cands, _) = fixture();
        // (5, 3) is inside the pour, well away from the trace (y=7) and via (15,10).
        let pick = resolve(&cands, mm(5.0, 3.0), 0, all_visible).expect("pour hit");
        match pick.id {
            SemanticId::Pour { net, layer } => {
                assert_eq!(net.to_string(), "GND");
                assert_eq!(layer, "F.Cu");
            }
            other => panic!("expected a pour, got {other:?}"),
        }
    }

    /// Overlapping trace-over-pour: a point on the trace, which is also inside the
    /// pour outline, resolves to the trace (priority — the more specific feature wins).
    #[test]
    fn trace_beats_pour_by_priority() {
        let (cands, _) = fixture();
        // (10, 7) is on the trace *and* inside the pour outline.
        let pick = resolve(&cands, mm(10.0, 7.0), 0, all_visible).expect("hit");
        assert!(
            matches!(pick.id, SemanticId::Trace(_)),
            "trace must beat the pour it lies on, got {:?}",
            pick.id
        );
    }

    /// The via at (15, 10) is pickable; with the pour beneath it, the via wins by
    /// priority (Via < Pour).
    #[test]
    fn click_on_via_selects_via() {
        let (cands, _) = fixture();
        let pick = resolve(&cands, mm(15.0, 10.0), 0, all_visible).expect("via hit");
        assert!(
            matches!(pick.id, SemanticId::Via(_)),
            "expected a via, got {:?}",
            pick.id
        );
    }

    /// A point clearly outside every feature (outside the pour outline entirely) picks
    /// nothing.
    #[test]
    fn empty_spot_picks_nothing() {
        let (cands, _) = fixture();
        // (0.2, 0.2) is inside the 2mm margin but outside the pour outline (1,1).
        assert!(resolve(&cands, mm(0.2, 0.2), 0, all_visible).is_none());
    }

    /// Tolerance scales with zoom: the same screen-px grab radius hits a thin trace at
    /// two different zooms. Aim just *off* the trace edge — at a low zoom the board-mm
    /// tolerance is larger, so the off-edge point still hits; the test asserts the
    /// converted tolerance grows as zoom shrinks and that both hit.
    #[test]
    fn tolerance_scales_with_zoom() {
        let (cands, _) = fixture();
        // The trace spans x=3..17 at y=7 (radius 0.25). Aim at (0.5, 7): 2.5mm off the
        // trace's near end AND outside the pour outline (x<1), so with zero tolerance
        // it misses everything; only a generous board-mm tolerance grabs the trace.
        let off = mm(0.5, 7.0);
        assert!(
            resolve(&cands, off, 0, all_visible).is_none(),
            "off-edge point must miss with zero tolerance"
        );
        // 6 screen px at zoom 1.0 → 6mm tolerance (plenty). At zoom 0.5 the board-mm
        // tolerance doubles.
        let tol_z1 = tolerance_nm(PICK_TOL_PX_TEST, 1.0);
        let tol_z_out = tolerance_nm(PICK_TOL_PX_TEST, 0.5);
        assert!(
            tol_z_out > tol_z1,
            "zooming out must grow the board-space tolerance ({tol_z_out} !> {tol_z1})"
        );
        // Both tolerances are enough to grab the 0.25mm-off point → both hit the trace.
        for tol in [tol_z1, tol_z_out] {
            let pick = resolve(&cands, off, tol, all_visible).expect("tolerant hit");
            assert!(matches!(pick.id, SemanticId::Trace(_)));
        }
    }

    /// A hidden layer is not pickable: hide F.Cu and the trace/pour/via on it vanish
    /// from the pick, so a point that hit the trace now picks nothing (the via also
    /// lives on B.Cu, but our query point (10,7) is not on the via).
    #[test]
    fn hidden_layer_not_pickable() {
        let (cands, _) = fixture();
        let hide_fcu = |id: &LayerId| *id != LayerId::Slab("F.Cu".to_string());
        assert!(
            resolve(&cands, mm(10.0, 7.0), 0, hide_fcu).is_none(),
            "hiding F.Cu must make the trace unpickable"
        );
    }

    /// The via spans both copper layers, so hiding F.Cu still leaves it pickable on
    /// B.Cu — a fan-out candidate on a visible layer keeps the via selectable.
    #[test]
    fn via_pickable_on_visible_span_layer() {
        let (cands, _) = fixture();
        let hide_fcu = |id: &LayerId| *id != LayerId::Slab("F.Cu".to_string());
        let pick = resolve(&cands, mm(15.0, 10.0), 0, hide_fcu).expect("via on B.Cu");
        assert!(matches!(pick.id, SemanticId::Via(_)));
        assert_eq!(pick.layer, LayerId::Slab("B.Cu".to_string()));
    }

    /// The pick tolerance in px used by the zoom test (mirrors the app constant).
    const PICK_TOL_PX_TEST: f32 = 6.0;

    /// Manual perf probe (`cargo test -p ecad-gui -- --ignored pick_resolve_perf
    /// --nocapture`): reproduces the profiler's measurement on the real poc board
    /// (192 candidates, 186 pads). Prints mean per-event `resolve` cost at four
    /// pointer positions (pad / trace / pour / empty). The regression this branch
    /// fixed had resolve running the full offset+tessellate kernel per candidate per
    /// event (~800 ms/event debug); after hoisting the region+AABB into candidate
    /// build time, resolve is a pure AABB-reject + cached-region distance lookup and
    /// must land well under 1 ms/event debug. Ignored so it never runs in CI (it reads
    /// the poc board off disk and is a timing measurement, not an assertion).
    #[test]
    #[ignore = "manual perf probe; run with --ignored --nocapture"]
    fn pick_resolve_perf() {
        use std::time::Instant;
        let d = crate::fixtures::poc_board_domain();
        let doc = d.doc.as_ref().expect("poc board elaborates").clone();
        let su = ecad_core::elaborate::stackup(&doc.source);
        let t_build = Instant::now();
        let cands = candidates(&doc, &d.lib, &su);
        let build_ms = t_build.elapsed().as_secs_f64() * 1e3;
        let pads = cands
            .iter()
            .filter(|c| matches!(c.id, SemanticId::Pin { .. }))
            .count();
        eprintln!(
            "poc board: {} candidates ({pads} pads); candidates() build = {build_ms:.2} ms",
            cands.len()
        );
        // A screen-px tolerance at zoom 1.0, as the app uses.
        let tol = tolerance_nm(PICK_TOL_PX_TEST, 1.0);
        // Four representative pointer positions. Exact hit targets are not important —
        // the profiler found the cost FLAT across position, which this probe verifies.
        let spots: [(&str, Point); 4] = [
            (
                "pad",
                cands.first().map(|c| c.aabb.0).unwrap_or(mm(0.0, 0.0)),
            ),
            ("trace", mm(10.0, 7.0)),
            ("pour", mm(5.0, 3.0)),
            ("empty", mm(-50.0, -50.0)),
        ];
        let iters = 200;
        for (label, p) in spots {
            let t = Instant::now();
            let mut sink = 0usize;
            for _ in 0..iters {
                if resolve(&cands, p, tol, all_visible).is_some() {
                    sink += 1;
                }
            }
            let per_ev_ms = t.elapsed().as_secs_f64() * 1e3 / iters as f64;
            eprintln!("resolve @ {label:>5}: {per_ev_ms:.4} ms/event (hits={sink}/{iters})");
        }
    }
}
