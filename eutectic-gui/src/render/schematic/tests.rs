//! Schematic-producer lowering tests (CPU tier): plane split, provenance →
//! semantic keys, polyline/polygon/text lowering, the bin-divider dash, and
//! determinism — all against the same `schematic_features` stream the SVG
//! oracle serializes.

use super::*;
use crate::app::DomainState;
use eutectic_core::coord::MM;
use eutectic_core::id::{EntityId, NetId};
use eutectic_core::part::part_library;
use eutectic_core::schematic::{STUB_LEN, SYMBOL_STROKE, symbol_extent};

/// Elaborate a document from source text (panics on failure — fixtures).
fn build(src: &str) -> (Doc, PartLib) {
    let d = DomainState::from_source(src.to_string(), None);
    (d.doc.expect("test source elaborates"), d.lib)
}

fn scene_of(src: &str) -> Scene {
    let (doc, lib) = build(src);
    schematic_scene(&doc, &lib).expect("non-empty doc produces a scene")
}

/// A scene's plane by key (must be enumerated).
fn plane<'a>(s: &'a Scene, key: &PlaneKey) -> &'a Plane {
    s.plane(key).unwrap_or_else(|| panic!("plane {key:?}"))
}

/// The source with a rotated MCU, an nc pin, net tags, a drawn wire, and an
/// unplaced bin — every stream feature kind in one doc.
const FULL_SRC: &str = "\
inst U1 MCU
inst C1 Cap
inst C2 Cap
net VDD U1.VDD C1.p1
nc C1.p2
schematic {
  row gap=8mm {
    sym C1
    sym U1 rot=90
    wire C1.p1 U1.VDD
  }
}
";

#[test]
fn empty_doc_produces_no_scene() {
    let (doc, lib) = build("board (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)\n");
    assert!(schematic_scene(&doc, &lib).is_none());
}

#[test]
fn scene_is_deterministic_and_bounds_match_the_stream() {
    let (doc, lib) = build(FULL_SRC);
    let a = schematic_scene(&doc, &lib).unwrap();
    let b = schematic_scene(&doc, &lib).unwrap();
    assert_eq!(a, b, "equal docs produce equal scenes");
    let fs = schematic_features(&doc, &lib);
    assert_eq!(
        a.bounds,
        (fs.bounds.x0, fs.bounds.y0, fs.bounds.x1, fs.bounds.y1),
        "scene bounds are the stream's shared bounds, verbatim"
    );
    assert_eq!(
        a.anchor,
        Point {
            x: (a.bounds.0 + a.bounds.2) / 2,
            y: (a.bounds.1 + a.bounds.3) / 2,
        }
    );
}

/// The four schematic planes enumerate in back-to-front order (wires under
/// symbols under tags under chrome), and every feature landed on one.
#[test]
fn plane_split_is_wire_ink_tag_chrome() {
    let s = scene_of(FULL_SRC);
    let keys: Vec<&PlaneKey> = s.planes.iter().map(|p| &p.key).collect();
    assert_eq!(
        keys,
        vec![
            &PlaneKey::SchematicWire,
            &PlaneKey::SchematicInk,
            &PlaneKey::SchematicTag,
            &PlaneKey::SchematicChrome,
        ]
    );
    let (doc, lib) = build(FULL_SRC);
    let fs = schematic_features(&doc, &lib);
    // Feature shapes conserve: every polyline edge / polygon edge / text run
    // lowered to something (count text runs exactly — one prim per run).
    let stream_runs = fs
        .features
        .iter()
        .filter(|f| matches!(f.shape, Shape::Text(_)))
        .count();
    let scene_runs = s
        .planes
        .iter()
        .flat_map(|p| &p.prims)
        .filter(|p| matches!(p.shape, PrimShape::TextRun { .. }))
        .count();
    assert_eq!(scene_runs, stream_runs, "one TextRun prim per stream run");
    assert!(!plane(&s, &PlaneKey::SchematicWire).prims.is_empty());
    assert!(!plane(&s, &PlaneKey::SchematicInk).prims.is_empty());
    assert!(!plane(&s, &PlaneKey::SchematicTag).prims.is_empty());
}

/// Provenance → SemanticKey: symbol bodies carry Part, stubs/nc marks Pin
/// (stored pin id), net tags AND net-carrying wires Net (the cross-view
/// currency), chrome the sentinel.
#[test]
fn provenance_maps_to_shared_semantic_keys() {
    let s = scene_of(FULL_SRC);
    let key_of = |prim: &Prim| s.semantics[prim.sem as usize].clone();

    let ink = plane(&s, &PlaneKey::SchematicInk);
    assert!(
        ink.prims
            .iter()
            .any(|p| key_of(p) == SemanticKey::Part(EntityId::new("C1"))),
        "C1's body outline carries Part"
    );
    let vdd_id = eutectic_core::schematic::symbol_body(&part_library()["MCU"])
        .pins
        .into_iter()
        .find(|p| p.name == "VDD")
        .expect("MCU has a VDD pin")
        .id;
    assert!(
        ink.prims.iter().any(|p| {
            key_of(p)
                == SemanticKey::Pin {
                    comp: EntityId::new("U1"),
                    pad: vdd_id.clone(),
                }
        }),
        "U1.VDD's stub/name carries Pin with the stored pin id"
    );
    // The nc mark is Pin-keyed text ("✕") on the ink plane.
    assert!(
        ink.prims.iter().any(|p| {
            matches!(&p.shape, PrimShape::TextRun { content, .. } if content == "✕")
                && key_of(p)
                    == SemanticKey::Pin {
                        comp: EntityId::new("C1"),
                        pad: "p2".into(),
                    }
        }),
        "the nc mark is a Pin-keyed ✕ run"
    );
    let tag = plane(&s, &PlaneKey::SchematicTag);
    assert!(
        tag.prims
            .iter()
            .all(|p| key_of(p) == SemanticKey::Net(NetId::new("VDD"))),
        "net tags key on their net"
    );
    let wire = plane(&s, &PlaneKey::SchematicWire);
    assert!(
        wire.prims
            .iter()
            .any(|p| key_of(p) == SemanticKey::Net(NetId::new("VDD"))),
        "a net-carrying wire keys on its net"
    );
    // Chrome is pinned at id 0.
    assert_eq!(s.semantics[0], SemanticKey::Chrome);
}

/// A netless wire (endpoints on no net) lowers with the chrome sentinel —
/// it has no selectable identity, matching the pick (no candidate).
#[test]
fn netless_wire_is_chrome_keyed() {
    let s = scene_of(
        "inst C1 Cap\ninst C2 Cap\nschematic {\n  row gap=8mm {\n    sym C1\n    sym C2\n    wire C1.p1 C2.p1\n  }\n}\n",
    );
    let wire = plane(&s, &PlaneKey::SchematicWire);
    assert!(!wire.prims.is_empty());
    assert!(
        wire.prims
            .iter()
            .all(|p| p.sem == crate::render::scene::SEM_CHROME),
        "a netless wire never highlights"
    );
}

/// The body outline (a stream Polygon) lowers as a CLOSED capsule chain —
/// the closing edge included, path length accumulating around the ring —
/// and a rotated part's chain spans the swapped extents.
#[test]
fn polygon_closes_and_rotation_reaches_the_scene() {
    let s = scene_of(FULL_SRC);
    let ink = plane(&s, &PlaneKey::SchematicInk);
    let u1_caps: Vec<&Prim> = ink
        .prims
        .iter()
        .filter(|p| {
            s.semantics[p.sem as usize] == SemanticKey::Part(EntityId::new("U1"))
                && matches!(p.shape, PrimShape::Capsule { .. })
        })
        .collect();
    assert_eq!(u1_caps.len(), 4, "a box outline is 4 capsules (closed)");
    // len0 accumulates around the ring: strictly increasing, starting at 0.
    let lens: Vec<f64> = u1_caps.iter().map(|p| p.len0).collect();
    assert_eq!(lens[0], 0.0);
    lens.windows(2).for_each(|w| assert!(w[1] > w[0]));
    // rot=90 swaps the drawn extents (the stream pins this; the producer
    // must not undo it): the capsule chain's bbox is h×w of the base extent.
    let lib = part_library();
    let base = symbol_extent(&lib["MCU"]);
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
    for p in &u1_caps {
        if let PrimShape::Capsule { a, b, .. } = &p.shape {
            for q in [a, b] {
                min_x = min_x.min(q.x);
                max_x = max_x.max(q.x);
                min_y = min_y.min(q.y);
                max_y = max_y.max(q.y);
            }
        }
    }
    assert_eq!(
        (max_x - min_x, max_y - min_y),
        (base.h, base.w),
        "rot=90 swapped extents survive the lowering"
    );
    // Stub capsules render at the symbol stroke's half-width.
    assert!(
        ink.prims
            .iter()
            .any(|p| matches!(&p.shape, PrimShape::Capsule { r, .. } if *r == SYMBOL_STROKE / 2)),
        "stubs/outlines stroke at half the stream width"
    );
    let _ = STUB_LEN; // (unit imported for the stub reach in other tests)
}

/// Text runs lower verbatim: baseline anchor, height, and the Start→Left /
/// End→Right justify mapping.
#[test]
fn text_runs_lower_with_baseline_and_justify() {
    let (doc, lib) = build(FULL_SRC);
    let fs = schematic_features(&doc, &lib);
    let s = schematic_scene(&doc, &lib).unwrap();
    let scene_runs: Vec<(&Point, &Nm, &Justify, &String)> = s
        .planes
        .iter()
        .flat_map(|p| &p.prims)
        .filter_map(|p| match &p.shape {
            PrimShape::TextRun {
                pos,
                height,
                justify,
                content,
            } => Some((pos, height, justify, content)),
            _ => None,
        })
        .collect();
    let mut matched = 0;
    for f in &fs.features {
        if let Shape::Text(run) = &f.shape {
            let want_justify = match run.justify {
                TextJustify::Start => Justify::Left,
                TextJustify::End => Justify::Right,
            };
            assert!(
                scene_runs.iter().any(|(pos, h, j, c)| **pos == run.at
                    && **h == run.height
                    && **j == want_justify
                    && **c == run.text),
                "stream run {run:?} must lower verbatim"
            );
            matched += 1;
        }
    }
    assert!(matched > 0);
    // Both justifies occur (left- and right-side pins).
    assert!(scene_runs.iter().any(|(_, _, j, _)| **j == Justify::Left));
    assert!(scene_runs.iter().any(|(_, _, j, _)| **j == Justify::Right));
}

/// The unplaced-bin divider exercises the dash machinery for real: a
/// Dash(DASH_BIN) capsule on the chrome plane, plus the Fill "unplaced"
/// label run; a fully-placed drawing has an empty chrome plane.
#[test]
fn bin_divider_is_dashed_chrome() {
    let s = scene_of("inst C1 Cap\ninst C2 Cap\nschematic {\n  row {\n    sym C1\n  }\n}\n");
    let chrome = plane(&s, &PlaneKey::SchematicChrome);
    let dashes: Vec<&Prim> = chrome
        .prims
        .iter()
        .filter(|p| p.class == StyleClass::Dash(DASH_BIN))
        .collect();
    assert_eq!(dashes.len(), 1, "one dashed divider capsule");
    assert!(matches!(dashes[0].shape, PrimShape::Capsule { .. }));
    assert_eq!(dashes[0].sem, crate::render::scene::SEM_CHROME);
    assert!(
        chrome.prims.iter().any(|p| matches!(
            &p.shape,
            PrimShape::TextRun { content, .. } if content == "unplaced"
        ) && p.class == StyleClass::Fill),
        "the bin label rides the chrome plane as plain text"
    );

    // All placed ⇒ chrome plane enumerated but empty.
    let s = scene_of("inst C1 Cap\nschematic {\n  row {\n    sym C1\n  }\n}\n");
    assert!(plane(&s, &PlaneKey::SchematicChrome).prims.is_empty());
    let _ = MM;
}
