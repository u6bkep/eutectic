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
//! 3. a candidate walk over the **doc** (not `world_features`), because
//!    `world_features` carries only a net annotation, never the owning entity id
//!    (see the module note on provenance below). Each candidate is one
//!    [`Candidate`] pairing a [`SemanticId`] with the world-space [`Shape2D`] to test
//!    and the [`LayerId`] it lives on;
//! 4. containment: a candidate hits when the query point is inside its shape inflated
//!    by the tolerance, tested through the same [`shape_to_region`] kernel the SVG
//!    backend and DRC use — so a filled pad/pour uses exact area containment and a
//!    zero-area trace/via disc uses its honest inflated copper extent;
//! 5. resolution: among hits on **visible** layers, the most *specific* wins
//!    ([`PickPriority`]): pad/pin ▸ trace ▸ via ▸ pour ▸ board outline. Ties within a
//!    priority break by top-most layer (highest z) first.
//!
//! # Provenance / granularity (the honest story)
//!
//! `ecad-core`'s [`world_features`](ecad_core::route::world_features) stream is the
//! render producer, and each [`NetFeature`](ecad_core::geom::NetFeature) carries only
//! `Option<NetId>` — **not** the trace / via / component / pin it came from
//! (`NetFeature { net, feature }`, geometry-model Decision 12: the net is an
//! annotation, identity lives elsewhere). So mapping a rendered feature back to a
//! selectable entity id is *not* possible from that stream.
//!
//! Rather than pick against `world_features` and lose identity, this module rebuilds
//! the pickable geometry directly from the doc, where the ids live: `doc.traces`
//! (→ [`TraceId`]), `doc.vias` (→ [`ViaId`]), `doc.components` + their `PartDef` pins
//! (→ refdes [`EntityId`] and per-pin [`PinRef`]), and `elaborate::regions` for pour
//! outlines (→ net + layer). This recovers **full granularity** — trace, via, pin,
//! and pour identity are all resolvable GUI-side without touching `ecad-core`. The
//! geometry is rebuilt with the same public constructors the engine uses
//! ([`Shape2D::trace`], [`Shape2D::disc`], [`PinDef::pad_features`]), so a pick tests
//! the identical copper extent the canvas draws. See the report's
//! `selection_granularity` field.

use super::LayerId;
use damascene_core::viewport::ViewportView;
use ecad_core::coord::{MM, Nm, Point};
use ecad_core::doc::Doc;
use ecad_core::geom::kernel::{DEFAULT_CIRCLE_SEGS, shape_to_region};
use ecad_core::geom::{Extent, Role, Shape2D, Stackup};
use ecad_core::id::{EntityId, NetId, TraceId, ViaId};
use ecad_core::part::PartLib;

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
    /// pin name (`PinRef`'s two parts, spelled out so the id is `Hash + Ord`).
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

/// One thing the pointer could land on: a semantic id, the world-space shape to test
/// containment against, and the visual layer it lives on (so the pick can skip hidden
/// layers). Built by [`candidates`]; never stored in the selection model.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The id this candidate selects when it wins the pick.
    pub id: SemanticId,
    /// The world-frame (nm, y-up) copper/area shape to test the query point against.
    pub shape: Shape2D,
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

/// Build every pickable [`Candidate`] for a doc: pour outlines, pad copper (per pin),
/// traces, and vias (per spanned copper slab), plus the board outline as the
/// last-resort candidate. Pure over the doc + library + stackup; the render cache is
/// untouched. Emission is deterministic (doc iteration order is `BTreeMap`-stable).
pub fn candidates(doc: &Doc, lib: &PartLib, su: &Stackup) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let cu = su.copper_slabs();

    // Pours: the authored conductor-region outline (not the knocked-out fill — a click
    // anywhere inside the outline should select the pour; the knockouts are visual).
    // Identity is net + layer. Priority Pour (large area, loses to everything on top).
    for r in ecad_core::elaborate::regions(&doc.source) {
        if r.role != Role::Conductor {
            continue;
        }
        let Some(net) = &r.net else { continue };
        let Some(slab) = cu.iter().find(|s| s.name == r.layer) else {
            continue;
        };
        out.push(Candidate {
            id: SemanticId::Pour {
                net: NetId::new(net.clone()),
                layer: r.layer.clone(),
            },
            shape: r.shape.clone(),
            layer: LayerId::Slab(r.layer.clone()),
            priority: PickPriority::Pour,
            z_top: slab.z.hi,
        });
    }

    // Pads / pins: each pin's copper features, rebuilt with the engine's own lowering
    // (`pad_features`). One candidate per copper feature (a through pad fans to several
    // slabs); identity is the pin ref. Priority Pad (most specific).
    for c in doc.components.values() {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for pin in &def.pins {
            for f in pin.pad_features(c, su) {
                if f.role != Role::Conductor {
                    continue; // the drill / mask-opening Void is not pickable copper
                }
                let Extent::Prism { shape, z } = &f.extent;
                let Some(slab) = cu.iter().find(|s| s.z == *z) else {
                    continue;
                };
                out.push(Candidate {
                    id: SemanticId::Pin {
                        comp: c.id.clone(),
                        pin: pin.name.clone(),
                    },
                    shape: shape.clone(),
                    layer: LayerId::Slab(slab.name.clone()),
                    priority: PickPriority::Pad,
                    z_top: z.hi,
                });
            }
        }
    }

    // Traces: the honest capsule/polyline copper extent on the trace's named slab.
    for (tid, t) in &doc.traces {
        let Some(slab) = cu.iter().find(|s| s.name == t.layer) else {
            continue;
        };
        out.push(Candidate {
            id: SemanticId::Trace(*tid),
            shape: Shape2D::trace(t.path.clone(), t.width),
            layer: LayerId::Slab(t.layer.clone()),
            priority: PickPriority::Trace,
            z_top: slab.z.hi,
        });
    }

    // Vias: the pad disc on every copper slab the via spans (so a hidden top layer
    // still leaves the via pickable on a visible inner/bottom layer).
    for (vid, v) in &doc.vias {
        for slab in v.spanned_slabs(&cu) {
            out.push(Candidate {
                id: SemanticId::Via(*vid),
                shape: Shape2D::disc(v.at, v.pad / 2),
                layer: LayerId::Slab(slab.name.clone()),
                priority: PickPriority::Via,
                z_top: slab.z.hi,
            });
        }
    }

    // Board outline: the whole board region, the last-resort candidate so a click on
    // bare substrate still lands *somewhere* meaningful. Selecting it currently maps to
    // nothing selectable (no board-entity id), so it is emitted but the resolver drops
    // an outline-only hit to "no selection" — clicking empty board clears, per the
    // spec. Kept as a documented seam for a future board-properties selection.
    // (Intentionally not emitted: it would make "click empty canvas clears" impossible.)

    out
}

/// Resolve a board-space query point (nm) to the winning [`Pick`], honoring the
/// visibility predicate and the priority ordering. Pure and unit-testable.
///
/// `visible(&LayerId) -> bool` is the canvas's own visibility test (a hidden layer is
/// not pickable). `tol_nm` is the board-space grab radius from [`tolerance_nm`]. A
/// candidate hits when `p` is inside its shape inflated by `tol_nm`, via the region
/// kernel. Among hits the winner is the lowest [`PickPriority`], breaking ties by
/// highest `z_top` (top-most layer). `None` when nothing (visible) is within tolerance.
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
        if !hits(&c.shape, p, tol_nm) {
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

/// Does `shape`, inflated by `tol_nm`, contain point `p`? Realised through the same
/// [`shape_to_region`] kernel the SVG backend uses: a filled `Polygon`/`Area` tests
/// exact area containment, and a zero-area `Stroke` (trace / disc / capsule) gets its
/// honest inflated copper region — so a hairline trace is pickable within `radius +
/// tol`. `tol_nm` floors at 0 (a negative tolerance never shrinks below the true
/// extent).
fn hits(shape: &Shape2D, p: Point, tol_nm: Nm) -> bool {
    let inflated = shape.inflated(tol_nm.max(0));
    let region = match &inflated {
        Shape2D::Area { region } => region.clone(),
        _ => shape_to_region(&inflated, DEFAULT_CIRCLE_SEGS),
    };
    region.contains_point(p)
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
}
