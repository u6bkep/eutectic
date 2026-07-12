//! The schematic producer: `schematic_features` → [`Scene`] (renderer-spec
//! §2/§12 WP3) — the second producer on the one ingest contract.
//!
//! Consumes the same [`schematic_features`] stream the SVG exporter
//! serializes (Decision 23 — the SVG stays the semantic oracle), so drawing
//! conventions (stub lengths, text heights, the bounds math) live in core
//! exactly once. Everything here is pure lowering: stream shapes → renderer
//! primitives, stream provenance → semantic keys, stream classes → planes.
//!
//! # Plane split
//!
//! The schematic drawing is line art in four color roles, so the plane split
//! is **by color role**, not by shape kind (a plane's appearance is one
//! composite uniform — renderer-spec §4):
//!
//! 1. [`PlaneKey::SchematicWire`] — drawn wires, composited first so wires
//!    read *under* symbols (the stream's §20d order, preserved as plane
//!    order rather than per-primitive z).
//! 2. [`PlaneKey::SchematicInk`] — symbol outlines + pin stubs **and** the
//!    same-color annotation text (headers, pin names, nc marks).
//! 3. [`PlaneKey::SchematicTag`] — net tags (the tag accent).
//! 4. [`PlaneKey::SchematicChrome`] — the unplaced-bin divider + label.
//!
//! There is no translucency and — beyond wires-under-symbols — no meaningful
//! overdraw between roles, so this compositing order is exactly the stream's
//! draw order. Within a plane, primitives keep stream order (deterministic).
//!
//! # Style classes exercised
//!
//! The bin divider lowers as [`StyleClass::Dash`] pattern [`DASH_BIN`]
//! (1 mm on / 1 mm off — the SVG oracle draws it `stroke-dasharray="1,1"`),
//! with the dash phase accumulated along the polyline so the pattern flows
//! through corners. Everything else is [`StyleClass::Fill`].
//!
//! # Semantic keys
//!
//! Provenance → [`SemanticKey`] mirrors the pick vocabulary
//! ([`crate::schematic_pick`]) so state-buffer highlight and pick share ids:
//! `Component → Part`, `Pin → Pin` (stored pin id — the `PinRef` join key),
//! `NetTag → Net`, `Wire-with-net → Net` (the cross-view currency),
//! netless wires and `Chrome → Chrome` (never highlightable).

use super::scene::{
    Justify, Plane, PlaneKey, Prim, PrimShape, Scene, SemanticInterner, SemanticKey, StyleClass,
};
use eutectic_core::coord::{Nm, Point};
use eutectic_core::doc::Doc;
use eutectic_core::part::PartLib;
use eutectic_core::schematic::{
    Provenance, Shape, StyleClass as SchClass, TextJustify, schematic_features,
};
use std::collections::BTreeMap;

/// The dash pattern id of the unplaced-bin divider (see
/// [`StyleTables::board_defaults`](super::style::StyleTables::board_defaults):
/// 1 mm on / 1 mm off — the SVG oracle's `stroke-dasharray="1,1"`).
pub const DASH_BIN: u8 = 1;

/// Lower an elaborated document's schematic drawing to a [`Scene`] over the
/// [`schematic_features`] stream. `None` when the doc has no components (an
/// empty schematic — the caller shows a placeholder pane), matching the old
/// `SchematicView::build` degrade. Deterministic: equal documents produce
/// equal scenes.
pub fn schematic_scene(doc: &Doc, lib: &PartLib) -> Option<Scene> {
    if doc.components.is_empty() {
        return None;
    }
    let fs = schematic_features(doc, lib);
    let bounds = (fs.bounds.x0, fs.bounds.y0, fs.bounds.x1, fs.bounds.y1);
    let anchor = Point {
        x: (bounds.0 + bounds.2) / 2,
        y: (bounds.1 + bounds.3) / 2,
    };

    let mut sems = SemanticInterner::new();
    let mut buckets: BTreeMap<PlaneKey, Vec<Prim>> = BTreeMap::new();

    for f in &fs.features {
        let sem = sems.intern(sem_key(&f.provenance));
        let (plane, class) = plane_of(f.class);
        let out = buckets.entry(plane).or_default();
        match &f.shape {
            Shape::Polyline { pts, width } => {
                polyline_prims(out, pts, *width, sem, class, false);
            }
            Shape::Polygon { pts, width } => {
                // A stream polygon is a *closed stroked outline* (today every
                // polygon is line art — Decision 23); the closing edge joins
                // the chain so the dash phase, if any, runs around the ring.
                polyline_prims(out, pts, *width, sem, class, true);
            }
            Shape::Disc { center, radius } => out.push(Prim {
                sem,
                class,
                len0: 0.0,
                shape: PrimShape::Disc {
                    c: *center,
                    r: *radius,
                },
            }),
            Shape::Text(run) => out.push(Prim {
                sem,
                class: StyleClass::Fill, // text has no dash rendering
                len0: 0.0,
                shape: PrimShape::TextRun {
                    pos: run.at,
                    height: run.height,
                    justify: match run.justify {
                        TextJustify::Start => Justify::Left,
                        TextJustify::End => Justify::Right,
                    },
                    content: run.text.clone(),
                },
            }),
        }
    }

    // Planes in back-to-front composite order (wires under symbols under
    // tags under chrome — the stream's draw order as plane order). Empty
    // planes are still enumerated (stable indices for the style tables).
    let mut planes: Vec<Plane> = Vec::new();
    for key in [
        PlaneKey::SchematicWire,
        PlaneKey::SchematicInk,
        PlaneKey::SchematicTag,
        PlaneKey::SchematicChrome,
    ] {
        let prims = buckets.remove(&key).unwrap_or_default();
        planes.push(Plane { key, prims });
    }
    debug_assert!(buckets.is_empty(), "every stream class maps to a plane");

    Some(Scene {
        anchor,
        bounds,
        planes,
        semantics: sems.into_table(),
    })
}

/// Stream provenance → the semantic key hover/selection and pick share.
fn sem_key(p: &Provenance) -> SemanticKey {
    match p {
        Provenance::Component(id) => SemanticKey::Part(id.clone()),
        Provenance::Pin { comp, pin } => SemanticKey::Pin {
            comp: comp.clone(),
            pad: pin.clone(),
        },
        Provenance::NetTag { net, .. } => SemanticKey::Net(net.clone()),
        Provenance::Wire { net: Some(net), .. } => SemanticKey::Net(net.clone()),
        // A netless wire has no selectable identity (the pick emits no
        // candidate for it either); chrome never highlights.
        Provenance::Wire { net: None, .. } | Provenance::Chrome => SemanticKey::Chrome,
    }
}

/// Stream style class → (plane, geometry style). The bin divider is the one
/// dashed stroke in the drawing (the SVG oracle's `stroke-dasharray="1,1"`).
fn plane_of(class: SchClass) -> (PlaneKey, StyleClass) {
    match class {
        SchClass::Wire => (PlaneKey::SchematicWire, StyleClass::Fill),
        SchClass::SymbolOutline
        | SchClass::PinStub
        | SchClass::Header
        | SchClass::PinName
        | SchClass::NcMark => (PlaneKey::SchematicInk, StyleClass::Fill),
        SchClass::NetTag => (PlaneKey::SchematicTag, StyleClass::Fill),
        SchClass::BinDivider => (PlaneKey::SchematicChrome, StyleClass::Dash(DASH_BIN)),
        SchClass::BinLabel => (PlaneKey::SchematicChrome, StyleClass::Fill),
    }
}

/// Lower a stroked polyline (open or closed) to a capsule chain at half the
/// stroke width, accumulating path length so dash patterns flow continuously
/// through corners (renderer-spec §2). Consecutive capsules share endpoints;
/// round joins come free from coverage max-blend. A single point degrades to
/// a disc.
fn polyline_prims(
    out: &mut Vec<Prim>,
    pts: &[Point],
    width: Nm,
    sem: u32,
    class: StyleClass,
    closed: bool,
) {
    let r = (width / 2).max(1);
    if pts.is_empty() {
        return;
    }
    if pts.len() == 1 {
        out.push(Prim {
            sem,
            class,
            len0: 0.0,
            shape: PrimShape::Disc { c: pts[0], r },
        });
        return;
    }
    let mut len = 0.0_f64;
    let n = pts.len();
    let edges = if closed { n } else { n - 1 };
    for k in 0..edges {
        let (a, b) = (pts[k], pts[(k + 1) % n]);
        if a == b {
            continue;
        }
        out.push(Prim {
            sem,
            class,
            len0: len,
            shape: PrimShape::Capsule { a, b, r },
        });
        len += ((b.x - a.x) as f64).hypot((b.y - a.y) as f64);
    }
}

#[cfg(test)]
mod tests;
