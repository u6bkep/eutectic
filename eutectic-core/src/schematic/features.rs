//! The schematic realized-geometry tier (Decision 23): [`schematic_features`], the one
//! place the schematic *drawing* is realized.
//!
//! The schematic model is healthy (authored tree → [`reflow`](crate::schematic::reflow) →
//! per-component [`Placement`]s), but everything that makes the drawing a drawing — pin
//! stubs, refdes headers, net tags, nc marks, wires meeting stub tips, the unplaced-bin
//! divider — used to be realized *inside the views, twice* (SVG strings in
//! [`crate::schematic_svg`], VectorAssets in the GUI), with constants copy-synced under
//! "kept in sync" comments. This module is the cure, the schematic twin of the board's
//! `route::world_features`: a flat, deterministically-ordered stream of typed primitives
//! in schematic space (integer nm, y-up) that every consumer — the SVG serializer, the
//! GUI projection + pick, the owned renderer to come — filters rather than re-derives.
//!
//! **The contract** (Decision 23, points 1–3):
//! - Vocabulary: stroked [`Shape::Polyline`]s and closed [`Shape::Polygon`] outlines
//!   (widths in nm — schematic space, never screen units), reserved [`Shape::Disc`]s
//!   (junction dots, gw-26), and [`TextRun`]s. **Text is a run, not glyphs** — position,
//!   height, justify, content; each consumer realizes it (SVG `<text>`, GUI stroke font,
//!   MSDF atlas later). Stroked-glyph realization is reserved for fab ink.
//! - Every primitive carries semantic [`Provenance`] (component / pin / net tag / wire /
//!   chrome) and a [`StyleClass`] (a semantic enum — **no colors, no view-toolkit types**).
//! - [`SchematicFeatures::bounds`] is the one home of the drawing's content-extent math —
//!   the SVG viewBox and the GUI content rect frame from the same numbers.
//! - Deterministic order (documented on [`schematic_features`]), like every producer in
//!   this codebase.
//!
//! **The symbol-artwork seam** (Decision 23, point 4): a symbol's body is realized by one
//! function, [`symbol_body`] — "body primitives + pin anchors for this part def" — whose
//! only implementation today is the derived box-with-pins
//! ([`symbol_extent`]/[`pin_slots`]). Authored artwork (a resistor squiggle with two pin
//! anchors and no box) later replaces the default *behind the seam*, with no contract
//! change.

use crate::doc::{Doc, MM, Nm, Orient, Point};
use crate::id::{EntityId, NetId};
use crate::part::{PartDef, PartLib};
use crate::schematic::{LayoutNode, PinSide, Placement, pin_slots, symbol_extent};
use std::collections::{BTreeMap, BTreeSet};

// ----------------------------------------------------------------------------
// The drawing conventions — one home (formerly copy-synced across the two views).
// ----------------------------------------------------------------------------

/// Length of a pin stub drawn out from the box edge (half a pin pitch), and the unit the
/// tag/name anchor offsets derive from.
pub const STUB_LEN: Nm = 1_270_000;
/// Text height for pin names, in nm.
pub const PIN_TEXT_H: Nm = 1_000_000;
/// Text height for the `refdes (Part)` component header and the bin label, in nm.
pub const HEADER_TEXT_H: Nm = 1_500_000;
/// Text height for net tags and nc marks, in nm.
pub const TAG_TEXT_H: Nm = 1_000_000;
/// Margin added around the content on all four sides of [`SchematicFeatures::bounds`].
pub const MARGIN: Nm = 2 * MM;
/// Horizontal bounds pad past each symbol's stub reach, so name/tag text stays in view
/// without measuring glyphs at layout time.
pub const LABEL_PAD: Nm = 20 * MM;
/// Vertical gap lifting the header baseline off the box top edge.
pub const HEADER_GAP: Nm = 500_000;
/// Stroke width (nm) of symbol outlines, pin stubs, and the bin divider.
pub const SYMBOL_STROKE: Nm = 100_000;
/// Stroke width (nm) of drawn wires.
pub const WIRE_STROKE: Nm = 150_000;

// ----------------------------------------------------------------------------
// The primitive vocabulary.
// ----------------------------------------------------------------------------

/// Horizontal justification of a [`TextRun`] about its anchor point — the SVG
/// `text-anchor` vocabulary. `Start` reads rightward from the anchor, `End` ends at it.
/// (A `Middle` joins when a consumer needs it; nothing emits one today.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextJustify {
    Start,
    End,
}

/// A text run in schematic space: **content, never glyph geometry** (Decision 23 point
/// 2). `at` is the *baseline* anchor point (y-up): the run's baseline sits at `at.y` and
/// it reads from / ends at `at.x` per `justify`. Runs that visually center on a stub line
/// already carry the baseline drop (height/3) in `at` — consumers apply no further
/// vertical convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRun {
    pub at: Point,
    pub height: Nm,
    pub justify: TextJustify,
    pub text: String,
}

/// One drawing primitive, in schematic space (integer nm, y-up).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// An open stroked polyline (stub, wire, divider). `width` is the stroke width in nm.
    Polyline { pts: Vec<Point>, width: Nm },
    /// A closed stroked outline (the symbol body box). The last point implicitly joins the
    /// first; `width` is the stroke width in nm. (Fill semantics, if artwork ever wants
    /// them, are a style-class question — today every polygon is line art.)
    Polygon { pts: Vec<Point>, width: Nm },
    /// A filled disc. Reserved for derived junction dots (gw-26); nothing emits one today.
    Disc { center: Point, radius: Nm },
    /// A text run — see [`TextRun`].
    Text(TextRun),
}

/// Semantic provenance: what document entity a primitive *is*, so pick candidates and
/// cross-view highlights derive from the stream instead of re-walking the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// A placed component's own drawing (body outline, header), by instance path.
    Component(EntityId),
    /// A pin's drawing (stub, name, nc mark). `pin` is the **stored pin identity** — a pad
    /// number, or `port.signal` — the `PinRef` join key the netlist and the GUI's
    /// `SemanticId::Pin` both use, *not* the display name.
    Pin { comp: EntityId, pin: String },
    /// A net tag at a connected pin: the pin it sits on and the net it names.
    NetTag {
        comp: EntityId,
        pin: String,
        net: NetId,
    },
    /// A drawn wire (§20d). `index` is the wire's stable position in the authored
    /// pre-order walk ([`SchematicLayout::wires`](crate::schematic::SchematicLayout::wires)
    /// order — indices of undrawable wires are skipped, not reassigned). `net` is the net
    /// of endpoint A's pin, falling back to B's — the cross-highlight currency; `None`
    /// when neither endpoint is on a net.
    Wire { index: usize, net: Option<NetId> },
    /// Non-semantic chrome (the unplaced-bin divider and its label).
    Chrome,
}

/// The style class — a *semantic* rendering role each consumer maps to its own styling
/// (SVG class + stroke attributes, GUI color tokens, per-plane tables in the owned
/// renderer). Never colors, never screen units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleClass {
    SymbolOutline,
    PinStub,
    Wire,
    Header,
    PinName,
    NetTag,
    NcMark,
    BinDivider,
    BinLabel,
}

/// One realized schematic feature: a primitive with its provenance and style class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchematicFeature {
    pub provenance: Provenance,
    pub class: StyleClass,
    pub shape: Shape,
}

/// The drawing's content bounds in schematic nm (y-up), **margin included** — the one
/// home of the extent math both views frame from (the SVG viewBox, the GUI content
/// rect). Gathered from every box corner (widened by stub reach + [`LABEL_PAD`] and the
/// header height) and every wire point, then padded by [`MARGIN`]; an empty drawing gets
/// a fixed 10 mm box so the frame is never degenerate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub x0: Nm,
    pub y0: Nm,
    pub x1: Nm,
    pub y1: Nm,
}

/// The realized schematic drawing: the deterministic feature stream + its content bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchematicFeatures {
    pub bounds: Bounds,
    pub features: Vec<SchematicFeature>,
}

// ----------------------------------------------------------------------------
// The symbol-artwork seam (Decision 23, point 4).
// ----------------------------------------------------------------------------

/// One pin's semantic anchors on a symbol body, in the part's **unrotated** box frame
/// (origin at the box center, y-up). `base`→`tip` is the stub centerline (base on the
/// body outline, tip pointing outward — the approach direction); wires meet `tip`,
/// tags/nc marks anchor at `tag_at` (past the tip), the pin name at `name_at` (inside
/// the body). The justifies make labels read outward from the body regardless of where
/// the pin sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinAnchor {
    /// Stored pin identity (pad number, or `port.signal`) — the `PinRef` join key.
    pub id: String,
    /// Human display name.
    pub name: String,
    pub base: Point,
    pub tip: Point,
    pub name_at: Point,
    pub tag_at: Point,
    pub name_justify: TextJustify,
    pub tag_justify: TextJustify,
}

/// A symbol's realized body: line-art primitives plus per-pin semantic anchors, in the
/// part's unrotated box frame. Produced by [`symbol_body`] — the artwork seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolBody {
    /// Body line art with its style classes (today: one [`StyleClass::SymbolOutline`]
    /// box polygon).
    pub body: Vec<(StyleClass, Shape)>,
    /// Pin anchors in the [`pin_slots`] enumeration order (deterministic).
    pub pins: Vec<PinAnchor>,
}

/// Body primitives + pin anchors for a part def — **the symbol-artwork seam**. The only
/// implementation today is the derived box-with-pins (Decision 20e, via
/// [`symbol_extent`]/[`pin_slots`]); authored artwork later replaces this default behind
/// the same signature, with no contract change for any consumer of
/// [`schematic_features`].
pub fn symbol_body(def: &PartDef) -> SymbolBody {
    let ext = symbol_extent(def);
    let (hw, hh) = (ext.w / 2, ext.h / 2);
    let outline = Shape::Polygon {
        pts: vec![
            Point { x: -hw, y: -hh },
            Point { x: hw, y: -hh },
            Point { x: hw, y: hh },
            Point { x: -hw, y: hh },
        ],
        width: SYMBOL_STROKE,
    };
    let pins = pin_slots(def)
        .into_iter()
        .map(|slot| {
            // Left stubs point out −x, right stubs out +x; anchors sit on the stub line.
            let sign: Nm = match slot.side {
                PinSide::Left => -1,
                PinSide::Right => 1,
            };
            let dy = slot.dy;
            let (name_justify, tag_justify) = match slot.side {
                PinSide::Left => (TextJustify::Start, TextJustify::End),
                PinSide::Right => (TextJustify::End, TextJustify::Start),
            };
            PinAnchor {
                id: slot.id,
                name: slot.name,
                base: Point {
                    x: sign * hw,
                    y: dy,
                },
                tip: Point {
                    x: sign * (hw + STUB_LEN),
                    y: dy,
                },
                name_at: Point {
                    x: sign * (hw - STUB_LEN / 4),
                    y: dy,
                },
                tag_at: Point {
                    x: sign * (hw + STUB_LEN + STUB_LEN / 2),
                    y: dy,
                },
                name_justify,
                tag_justify,
            }
        })
        .collect();
    SymbolBody {
        body: vec![(StyleClass::SymbolOutline, outline)],
        pins,
    }
}

// ----------------------------------------------------------------------------
// The query.
// ----------------------------------------------------------------------------

/// Realize the whole schematic drawing (Decision 23 point 1): reflow the layout and emit
/// every primitive the drawing consists of, each with provenance and style class, plus
/// the content [`Bounds`]. Pure and deterministic — same doc+lib, byte-identical stream
/// (BTreeMap iteration, pre-order walks, integer nm arithmetic only).
///
/// **Order contract** (also the draw order — wires render *under* symbols, §20d):
/// 1. every drawn wire, in the authored pre-order walk;
/// 2. per placed component in `EntityId` order: body outline(s), header, then per pin in
///    [`pin_slots`] order: stub, pin name, then its net tag *or* nc mark (if either);
/// 3. the unplaced-bin divider + label, when both a placed symbol and a bin symbol exist.
///
/// Totality (§20c): every component in the doc gets a body + header (bin parts too); a
/// component whose part is missing from the lib draws its min box with no stubs. A wire
/// is dropped only when an endpoint is genuinely unresolvable (component absent from the
/// placements, part missing, or a pin selector resolving to no stub).
pub fn schematic_features(doc: &Doc, lib: &PartLib) -> SchematicFeatures {
    let placements = doc.reflow_schematic(lib);
    // The derived reference designators (Decision 14): headers show `C3 (Cap)`, falling
    // back to the raw instance path for any id the query somehow omits.
    let refdes = crate::annotate::refdes(doc, lib, &crate::annotate::registry(&doc.source));
    let rots = symbol_rotations(doc);
    let placed = placed_paths(doc);
    // Pin identity -> net name, from the materialized netlist (the tag source). A pin
    // absent here joins no net and gets no tag (§20c: unconnected pins get nothing).
    let pin_net: BTreeMap<(String, String), String> = doc
        .nets
        .values()
        .flat_map(|net| {
            net.members
                .iter()
                .map(move |m| ((m.comp.to_string(), m.pin.clone()), net.name.clone()))
        })
        .collect();
    // No-connect marks (§20c): a pin the source declared `nc` gets a small ✕ tag instead.
    let nc: BTreeSet<(String, String)> = doc
        .no_connects
        .iter()
        .map(|p| (p.comp.to_string(), p.pin.clone()))
        .collect();

    let wires = wire_polylines(doc, &placements, lib, &rots);
    let bounds = content_bounds(&placements, &wires);

    let mut out: Vec<SchematicFeature> = Vec::new();

    // ---- 1. wires, under the symbols (§20d) -------------------------------------
    for w in &wires {
        out.push(SchematicFeature {
            provenance: Provenance::Wire {
                index: w.index,
                net: w.net.clone(),
            },
            class: StyleClass::Wire,
            shape: Shape::Polyline {
                pts: w.poly.clone(),
                width: WIRE_STROKE,
            },
        });
    }

    // ---- 2. symbols (BTreeMap order ⇒ deterministic) ----------------------------
    for (id, pl) in &placements {
        let comp = &doc.components[id];
        let rot = rots.get(id.as_str()).copied().unwrap_or(Orient::IDENTITY);
        let place = |p: Point| offset(pl.center, rot.apply(p));
        let (hw, hh) = (pl.extent.w / 2, pl.extent.h / 2);

        // Body, through the artwork seam (built in the part's unrotated frame — reflow
        // already swapped `pl.extent` for a 90/270 rot, so the seam's own extent is
        // rotated here, never double-swapped). A part missing from the lib degrades to
        // the min box reflow placed it with, and no pins (the view stays total).
        let body = lib.get(&comp.part).map(symbol_body);
        match &body {
            Some(b) => {
                for (class, shape) in &b.body {
                    out.push(SchematicFeature {
                        provenance: Provenance::Component(id.clone()),
                        class: *class,
                        shape: transform_shape(shape, &place),
                    });
                }
            }
            None => out.push(SchematicFeature {
                provenance: Provenance::Component(id.clone()),
                class: StyleClass::SymbolOutline,
                shape: Shape::Polygon {
                    pts: vec![
                        Point {
                            x: pl.center.x - hw,
                            y: pl.center.y - hh,
                        },
                        Point {
                            x: pl.center.x + hw,
                            y: pl.center.y - hh,
                        },
                        Point {
                            x: pl.center.x + hw,
                            y: pl.center.y + hh,
                        },
                        Point {
                            x: pl.center.x - hw,
                            y: pl.center.y + hh,
                        },
                    ],
                    width: SYMBOL_STROKE,
                },
            }),
        }

        // Header: `refdes (Part)`, baseline reading from the box's top-left corner,
        // lifted by HEADER_GAP. Keyed on the *rotated* extent so it hugs the drawn box.
        let designator = refdes
            .get(id)
            .map(String::as_str)
            .unwrap_or_else(|| id.as_str());
        out.push(SchematicFeature {
            provenance: Provenance::Component(id.clone()),
            class: StyleClass::Header,
            shape: Shape::Text(TextRun {
                at: Point {
                    x: pl.center.x - hw,
                    y: pl.center.y + hh + HEADER_GAP,
                },
                height: HEADER_TEXT_H,
                justify: TextJustify::Start,
                text: format!("{designator} ({})", comp.part),
            }),
        });

        // Pin stubs + names + net tags / nc marks.
        if let Some(b) = &body {
            for pin in &b.pins {
                let prov = Provenance::Pin {
                    comp: id.clone(),
                    pin: pin.id.clone(),
                };
                out.push(SchematicFeature {
                    provenance: prov.clone(),
                    class: StyleClass::PinStub,
                    shape: Shape::Polyline {
                        pts: vec![place(pin.base), place(pin.tip)],
                        width: SYMBOL_STROKE,
                    },
                });
                out.push(SchematicFeature {
                    provenance: prov.clone(),
                    class: StyleClass::PinName,
                    shape: Shape::Text(TextRun {
                        at: centered_baseline(place(pin.name_at), PIN_TEXT_H),
                        height: PIN_TEXT_H,
                        justify: pin.name_justify,
                        text: pin.name.clone(),
                    }),
                });
                // Net tag (§20c) at the stub tip, or a no-connect ✕, or nothing.
                let key = (id.to_string(), pin.id.clone());
                let tag_at = centered_baseline(place(pin.tag_at), TAG_TEXT_H);
                if let Some(net) = pin_net.get(&key) {
                    out.push(SchematicFeature {
                        provenance: Provenance::NetTag {
                            comp: id.clone(),
                            pin: pin.id.clone(),
                            net: NetId::new(net),
                        },
                        class: StyleClass::NetTag,
                        shape: Shape::Text(TextRun {
                            at: tag_at,
                            height: TAG_TEXT_H,
                            justify: pin.tag_justify,
                            text: net.clone(),
                        }),
                    });
                } else if nc.contains(&key) {
                    out.push(SchematicFeature {
                        provenance: prov,
                        class: StyleClass::NcMark,
                        shape: Shape::Text(TextRun {
                            at: tag_at,
                            height: TAG_TEXT_H,
                            justify: pin.tag_justify,
                            text: "✕".to_string(),
                        }),
                    });
                }
            }
        }
    }

    // ---- 3. unplaced-bin divider + label ----------------------------------------
    // The bin sits below the placed content; the divider spans the bounds (inset half a
    // margin) between the lowest placed box bottom and the highest bin box top.
    if let Some(div_y) = bin_divider_y(&placements, &placed) {
        out.push(SchematicFeature {
            provenance: Provenance::Chrome,
            class: StyleClass::BinDivider,
            shape: Shape::Polyline {
                pts: vec![
                    Point {
                        x: bounds.x0 + MARGIN / 2,
                        y: div_y,
                    },
                    Point {
                        x: bounds.x1 - MARGIN / 2,
                        y: div_y,
                    },
                ],
                width: SYMBOL_STROKE,
            },
        });
        out.push(SchematicFeature {
            provenance: Provenance::Chrome,
            class: StyleClass::BinLabel,
            shape: Shape::Text(TextRun {
                at: Point {
                    x: bounds.x0 + MARGIN / 2,
                    y: div_y - HEADER_TEXT_H,
                },
                height: HEADER_TEXT_H,
                justify: TextJustify::Start,
                text: "unplaced".to_string(),
            }),
        });
    }

    SchematicFeatures {
        bounds,
        features: out,
    }
}

// ----------------------------------------------------------------------------
// Internals.
// ----------------------------------------------------------------------------

/// A run that visually centers on a horizontal anchor line (a stub) drops its baseline a
/// third of the text height below the line — the one home of the old SVG `+H/3` idiom.
fn centered_baseline(anchor: Point, height: Nm) -> Point {
    Point {
        x: anchor.x,
        y: anchor.y - height / 3,
    }
}

/// Apply a point transform to every coordinate of a shape (rotate-then-translate for
/// symbol placement; text runs are placed by their anchor, height/justify unchanged).
fn transform_shape(shape: &Shape, f: &impl Fn(Point) -> Point) -> Shape {
    match shape {
        Shape::Polyline { pts, width } => Shape::Polyline {
            pts: pts.iter().copied().map(f).collect(),
            width: *width,
        },
        Shape::Polygon { pts, width } => Shape::Polygon {
            pts: pts.iter().copied().map(f).collect(),
            width: *width,
        },
        Shape::Disc { center, radius } => Shape::Disc {
            center: f(*center),
            radius: *radius,
        },
        Shape::Text(run) => Shape::Text(TextRun {
            at: f(run.at),
            ..run.clone()
        }),
    }
}

/// Add a box-frame offset to a component center → an absolute schematic point.
fn offset(center: Point, off: Point) -> Point {
    Point {
        x: center.x + off.x,
        y: center.y + off.y,
    }
}

/// Content bounds (see [`Bounds`]): every box corner (widened by stub reach + label pad,
/// header height above), every wire point; ±[`MARGIN`]; a 10 mm default when empty.
fn content_bounds(placements: &BTreeMap<EntityId, Placement>, wires: &[RealizedWire]) -> Bounds {
    let mut xs: Vec<Nm> = Vec::new();
    let mut ys: Vec<Nm> = Vec::new();
    for pl in placements.values() {
        let (hw, hh) = (pl.extent.w / 2, pl.extent.h / 2);
        xs.push(pl.center.x - hw - STUB_LEN - LABEL_PAD);
        ys.push(pl.center.y - hh);
        xs.push(pl.center.x + hw + STUB_LEN + LABEL_PAD);
        ys.push(pl.center.y + hh + HEADER_TEXT_H);
    }
    for w in wires {
        for p in &w.poly {
            xs.push(p.x);
            ys.push(p.y);
        }
    }
    let (mut x0, mut y0, mut x1, mut y1) = if xs.is_empty() {
        (0, 0, 10 * MM, 10 * MM)
    } else {
        (
            *xs.iter().min().unwrap(),
            *ys.iter().min().unwrap(),
            *xs.iter().max().unwrap(),
            *ys.iter().max().unwrap(),
        )
    };
    x0 -= MARGIN;
    y0 -= MARGIN;
    x1 += MARGIN;
    y1 += MARGIN;
    Bounds { x0, y0, x1, y1 }
}

/// Authored schematic rotation ([`Symbol::rot`](crate::schematic::Symbol), §20b) per
/// component path, from the layout tree. Deterministic pre-order walk; last placement of
/// a path wins (validation forbids duplicates, so this is unambiguous in a valid doc).
fn symbol_rotations(doc: &Doc) -> BTreeMap<String, Orient> {
    let mut out = BTreeMap::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    fn walk(nodes: &[LayoutNode], out: &mut BTreeMap<String, Orient>) {
        for n in nodes {
            match n {
                LayoutNode::Symbol(s) => {
                    out.insert(s.path.clone(), s.rot);
                }
                LayoutNode::Container(c) => walk(&c.children, out),
                _ => {}
            }
        }
    }
    walk(&layout.roots, &mut out);
    out
}

/// Component paths that are *placed* by the tree (named by a `sym` **and** populated).
/// The complement (within the placement set) is the unplaced bin — sites the divider.
fn placed_paths(doc: &Doc) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    for path in layout.symbol_paths() {
        if doc.components.contains_key(&EntityId::new(path)) {
            out.insert(path.to_string());
        }
    }
    out
}

/// The y (schematic space, nm) of the unplaced-bin divider: midway between the lowest
/// placed box bottom and the highest bin box top. `None` when there is nothing placed
/// *or* nothing in the bin (no divider needed — the drawing is all one or all the other).
fn bin_divider_y(
    placements: &BTreeMap<EntityId, Placement>,
    placed: &BTreeSet<String>,
) -> Option<Nm> {
    let mut lowest_placed: Option<Nm> = None;
    let mut highest_bin: Option<Nm> = None;
    for (id, pl) in placements {
        let bottom = pl.center.y - pl.extent.h / 2;
        let top = pl.center.y + pl.extent.h / 2;
        if placed.contains(id.as_str()) {
            lowest_placed = Some(lowest_placed.map_or(bottom, |v: Nm| v.min(bottom)));
        } else {
            highest_bin = Some(highest_bin.map_or(top, |v: Nm| v.max(top)));
        }
    }
    match (lowest_placed, highest_bin) {
        (Some(lo), Some(hi)) => Some((lo + hi) / 2),
        _ => None,
    }
}

/// One drawable wire: its stable authored index, its polyline (pin-A tip, waypoints,
/// pin-B tip), and the net it highlights.
struct RealizedWire {
    index: usize,
    poly: Vec<Point>,
    net: Option<NetId>,
}

/// Each drawn wire (§20d) as a schematic-space polyline. An *unplaced* component is
/// still in `placements` (in the bin, §20c totality), so a wire to it draws to the bin —
/// intentional, not a drop. A wire is dropped only when an endpoint is genuinely absent
/// (a DNP-dropped component, a part missing from the lib, or a pin selector resolving to
/// no stub); those cases earn a warning at commit and simply are not drawn (their
/// indices are skipped, keeping every other wire's index stable). The wire's net is the
/// net of endpoint A's pin, falling back to B's — a wire whose ends disagree earns a
/// core warning; either net is a fine cross-highlight target.
fn wire_polylines(
    doc: &Doc,
    placements: &BTreeMap<EntityId, Placement>,
    lib: &PartLib,
    rots: &BTreeMap<String, Orient>,
) -> Vec<RealizedWire> {
    let mut out = Vec::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    let pin_net: BTreeMap<(String, String), String> = doc
        .nets
        .values()
        .flat_map(|net| {
            net.members
                .iter()
                .map(move |m| ((m.comp.to_string(), m.pin.clone()), net.name.clone()))
        })
        .collect();
    for (index, w) in layout.wires().into_iter().enumerate() {
        let (Some((a, a_pin)), Some((b, b_pin))) = (
            wire_end_point(doc, placements, lib, rots, &w.a.comp, &w.a.pin),
            wire_end_point(doc, placements, lib, rots, &w.b.comp, &w.b.pin),
        ) else {
            continue;
        };
        let mut poly = vec![a];
        poly.extend(w.waypoints.iter().copied());
        poly.push(b);
        let net = pin_net
            .get(&(w.a.comp.clone(), a_pin))
            .or_else(|| pin_net.get(&(w.b.comp.clone(), b_pin)))
            .map(NetId::new);
        out.push(RealizedWire { index, poly, net });
    }
    out
}

/// The schematic-space point of a wire endpoint — the stub **tip** of the named pin on
/// the placed symbol (so wires meet the drawn stubs, not the box edge) — plus the
/// resolved stored pin id. `None` if the component is not placed, the part is unknown,
/// or the pin selector resolves to no stub.
fn wire_end_point(
    doc: &Doc,
    placements: &BTreeMap<EntityId, Placement>,
    lib: &PartLib,
    rots: &BTreeMap<String, Orient>,
    comp: &str,
    pin: &str,
) -> Option<(Point, String)> {
    let cid = EntityId::new(comp);
    let pl = placements.get(&cid)?;
    let def = lib.get(&doc.components.get(&cid)?.part)?;
    // Resolve the authored selector to a stored identity, then find that pin's anchor.
    let ids = def.resolve_selector(pin);
    let want = ids.first().map(String::as_str).unwrap_or(pin);
    let body = symbol_body(def);
    let anchor = body.pins.into_iter().find(|p| p.id == want)?;
    let rot = rots.get(comp).copied().unwrap_or(Orient::IDENTITY);
    Some((offset(pl.center, rot.apply(anchor.tip)), anchor.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::history::History;
    use crate::part::part_library;

    /// Elaborate a document from source text.
    fn build(src: &str) -> (Doc, PartLib) {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::LoadText(src.into())), &lib, "t")
            .unwrap_or_else(|e| panic!("commit failed: {e:?}"));
        (h.doc().clone(), lib)
    }

    /// All features matching a style class.
    fn of_class(fs: &SchematicFeatures, class: StyleClass) -> Vec<&SchematicFeature> {
        fs.features.iter().filter(|f| f.class == class).collect()
    }

    /// The stub tip (second polyline point) of a pin, from the stream.
    fn stub_tip(fs: &SchematicFeatures, comp: &str, pin: &str) -> Point {
        let f = fs
            .features
            .iter()
            .find(|f| {
                f.class == StyleClass::PinStub
                    && f.provenance
                        == Provenance::Pin {
                            comp: EntityId::new(comp),
                            pin: pin.to_string(),
                        }
            })
            .unwrap_or_else(|| panic!("no stub for {comp}.{pin}"));
        match &f.shape {
            Shape::Polyline { pts, .. } => {
                assert_eq!(pts.len(), 2, "a stub is base→tip");
                pts[1]
            }
            other => panic!("stub is a polyline, got {other:?}"),
        }
    }

    /// The stream is deterministic: two runs over the same doc are identical, features
    /// and bounds both (the order contract is part of the output).
    #[test]
    fn stream_is_deterministic() {
        let (doc, lib) = build(
            "inst C1 Cap\ninst C2 Cap\ninst U1 MCU\nnet VCC C1.p1 C2.p1\nnc C1.p2\nschematic {\n  row gap=5mm {\n    sym C1\n    sym C2\n    wire C1.p1 C2.p1\n  }\n}\n",
        );
        assert_eq!(
            schematic_features(&doc, &lib),
            schematic_features(&doc, &lib),
            "byte-identical across runs"
        );
    }

    /// Provenance coverage: a placed symbol with a connected pin and an nc pin yields
    /// body / header / stub / pin-name / net-tag / nc-mark primitives, each carrying the
    /// right provenance (component path, stored pin id, net id).
    #[test]
    fn provenance_covers_every_primitive_kind() {
        let (doc, lib) = build(
            "inst C1 Cap\ninst C2 Cap\nnet VCC C1.p1 C2.p1\nnc C1.p2\nschematic {\n  row {\n    sym C1\n    sym C2\n  }\n}\n",
        );
        let fs = schematic_features(&doc, &lib);
        let c1 = EntityId::new("C1");

        let body = of_class(&fs, StyleClass::SymbolOutline);
        assert!(
            body.iter()
                .any(|f| f.provenance == Provenance::Component(c1.clone())),
            "C1 body outline with Component provenance"
        );
        let hdr = of_class(&fs, StyleClass::Header);
        assert!(
            hdr.iter().any(|f| {
                f.provenance == Provenance::Component(c1.clone())
                    && matches!(&f.shape, Shape::Text(t) if t.text == "Cap1 (Cap)")
            }),
            "C1 header text run with the annotated refdes"
        );
        assert!(
            of_class(&fs, StyleClass::PinStub).iter().any(|f| {
                f.provenance
                    == Provenance::Pin {
                        comp: c1.clone(),
                        pin: "p1".into(),
                    }
            }),
            "C1.p1 stub with Pin provenance (stored pin id)"
        );
        assert!(
            of_class(&fs, StyleClass::PinName).iter().any(|f| {
                f.provenance
                    == Provenance::Pin {
                        comp: c1.clone(),
                        pin: "p1".into(),
                    }
                    && matches!(&f.shape, Shape::Text(t) if t.text == "p1")
            }),
            "C1.p1 pin-name text run"
        );
        assert!(
            of_class(&fs, StyleClass::NetTag).iter().any(|f| {
                f.provenance
                    == Provenance::NetTag {
                        comp: c1.clone(),
                        pin: "p1".into(),
                        net: NetId::new("VCC"),
                    }
                    && matches!(&f.shape, Shape::Text(t) if t.text == "VCC")
            }),
            "C1.p1 net tag naming its net"
        );
        assert!(
            of_class(&fs, StyleClass::NcMark).iter().any(|f| {
                f.provenance
                    == Provenance::Pin {
                        comp: c1.clone(),
                        pin: "p2".into(),
                    }
                    && matches!(&f.shape, Shape::Text(t) if t.text == "✕")
            }),
            "C1.p2 nc mark (✕ run, Pin provenance)"
        );
        // A connected pin gets a tag, never an nc mark, and vice versa.
        assert!(
            !of_class(&fs, StyleClass::NcMark).iter().any(|f| {
                f.provenance
                    == Provenance::Pin {
                        comp: c1.clone(),
                        pin: "p1".into(),
                    }
            }),
            "connected C1.p1 must not carry an nc mark"
        );
    }

    /// Rotation (§20b): a 90/270 `rot` swaps the drawn box's extents, and the stubs
    /// rotate with the box — every stub base sits on the rotated box perimeter, every
    /// tip strictly outside (the rot-90/270 stub-detach regression, now pinned at the
    /// stream level).
    #[test]
    fn rotation_swaps_extents_and_stubs_follow() {
        let lib = part_library();
        let base_ext = symbol_extent(&lib["MCU"]); // non-square box (exercises the swap).
        for deg in [0i32, 90, 180, 270] {
            let (doc, lib) = build(&format!(
                "inst U1 MCU\nschematic {{\n  row {{\n    sym U1 rot={deg}\n  }}\n}}\n"
            ));
            let fs = schematic_features(&doc, &lib);
            let body = of_class(&fs, StyleClass::SymbolOutline);
            assert_eq!(body.len(), 1);
            let Shape::Polygon { pts, .. } = &body[0].shape else {
                panic!("body is a polygon");
            };
            let (min_x, max_x) = (
                pts.iter().map(|p| p.x).min().unwrap(),
                pts.iter().map(|p| p.x).max().unwrap(),
            );
            let (min_y, max_y) = (
                pts.iter().map(|p| p.y).min().unwrap(),
                pts.iter().map(|p| p.y).max().unwrap(),
            );
            let (w, h) = (max_x - min_x, max_y - min_y);
            let (want_w, want_h) = if deg == 90 || deg == 270 {
                (base_ext.h, base_ext.w)
            } else {
                (base_ext.w, base_ext.h)
            };
            assert_eq!((w, h), (want_w, want_h), "rot={deg} drawn extent");

            // Stub bases on the perimeter, tips strictly outside (±1 nm halving slack).
            let center = Point {
                x: (min_x + max_x) / 2,
                y: (min_y + max_y) / 2,
            };
            let (hw, hh) = (w / 2, h / 2);
            for f in of_class(&fs, StyleClass::PinStub) {
                let Shape::Polyline { pts, .. } = &f.shape else {
                    panic!("stub is a polyline");
                };
                let base = Point {
                    x: pts[0].x - center.x,
                    y: pts[0].y - center.y,
                };
                let tip = Point {
                    x: pts[1].x - center.x,
                    y: pts[1].y - center.y,
                };
                let on_x = (base.x.abs() - hw).abs() <= 1 && base.y.abs() <= hh + 1;
                let on_y = (base.y.abs() - hh).abs() <= 1 && base.x.abs() <= hw + 1;
                assert!(
                    on_x || on_y,
                    "rot={deg} stub base {base:?} off the {hw}×{hh} box perimeter"
                );
                assert!(
                    tip.x.abs() > hw || tip.y.abs() > hh,
                    "rot={deg} stub tip {tip:?} not outside the {hw}×{hh} box"
                );
            }
        }
    }

    /// Wires meet the drawn stub *tips* (§20d), not the box edge, and authored waypoints
    /// thread between them in order; the wire's provenance carries its stable pre-order
    /// index and the endpoint net.
    #[test]
    fn wire_meets_stub_tips_through_waypoints() {
        let (doc, lib) = build(
            "inst C1 Cap\ninst C2 Cap\nnet N C1.p2 C2.p2\nschematic {\n  row gap=10mm {\n    sym C1\n    sym C2\n    wire C1.p2 C2.p2 via (0mm, -12mm)\n  }\n}\n",
        );
        let fs = schematic_features(&doc, &lib);
        let wires = of_class(&fs, StyleClass::Wire);
        assert_eq!(wires.len(), 1);
        let Shape::Polyline { pts, .. } = &wires[0].shape else {
            panic!("wire is a polyline");
        };
        assert_eq!(pts.len(), 3, "tip, waypoint, tip");
        assert_eq!(pts[0], stub_tip(&fs, "C1", "p2"), "starts at C1.p2's tip");
        assert_eq!(
            pts[1],
            Point { x: 0, y: -12 * MM },
            "authored waypoint in the middle"
        );
        assert_eq!(pts[2], stub_tip(&fs, "C2", "p2"), "ends at C2.p2's tip");
        assert_eq!(
            wires[0].provenance,
            Provenance::Wire {
                index: 0,
                net: Some(NetId::new("N")),
            },
            "stable pre-order index + the endpoints' net"
        );
    }

    /// The unplaced bin (§20c): with one placed and one unplaced symbol the stream ends
    /// with chrome — a divider midway between the lowest placed box bottom and the
    /// highest bin box top, spanning the bounds inset by half a margin, and its label.
    #[test]
    fn unplaced_bin_realizes_divider_and_label() {
        let (doc, lib) =
            build("inst C1 Cap\ninst C2 Cap\nschematic {\n  row {\n    sym C1\n  }\n}\n");
        let fs = schematic_features(&doc, &lib);
        let placements = doc.reflow_schematic(&lib);
        let c1 = &placements[&EntityId::new("C1")];
        let c2 = &placements[&EntityId::new("C2")];
        let want_y = ((c1.center.y - c1.extent.h / 2) + (c2.center.y + c2.extent.h / 2)) / 2;

        let div = of_class(&fs, StyleClass::BinDivider);
        assert_eq!(div.len(), 1);
        assert_eq!(div[0].provenance, Provenance::Chrome);
        let Shape::Polyline { pts, .. } = &div[0].shape else {
            panic!("divider is a polyline");
        };
        assert_eq!(pts[0].y, want_y, "midway between placed bottom and bin top");
        assert_eq!(pts[1].y, want_y);
        assert_eq!(pts[0].x, fs.bounds.x0 + MARGIN / 2);
        assert_eq!(pts[1].x, fs.bounds.x1 - MARGIN / 2);

        let label = of_class(&fs, StyleClass::BinLabel);
        assert_eq!(label.len(), 1);
        assert!(
            matches!(&label[0].shape, Shape::Text(t) if t.text == "unplaced"),
            "bin label run"
        );
        // All placed ⇒ no chrome at all.
        let (doc, lib) = build("inst C1 Cap\nschematic {\n  row {\n    sym C1\n  }\n}\n");
        let fs = schematic_features(&doc, &lib);
        assert!(of_class(&fs, StyleClass::BinDivider).is_empty());
        assert!(of_class(&fs, StyleClass::BinLabel).is_empty());
    }

    /// The shared bounds are the old viewBox math, verbatim: every box corner widened by
    /// stub reach + LABEL_PAD (and HEADER_TEXT_H above), every wire point, ±MARGIN — so
    /// the SVG viewBox and the GUI content rect keep framing from the same numbers.
    #[test]
    fn bounds_match_the_old_viewbox_math() {
        let (doc, lib) = build(
            "inst C1 Cap\ninst U1 MCU\nschematic {\n  row gap=5mm {\n    sym C1\n    sym U1\n  }\n}\n",
        );
        let fs = schematic_features(&doc, &lib);
        let placements = doc.reflow_schematic(&lib);
        let mut xs: Vec<Nm> = Vec::new();
        let mut ys: Vec<Nm> = Vec::new();
        for pl in placements.values() {
            let (hw, hh) = (pl.extent.w / 2, pl.extent.h / 2);
            xs.push(pl.center.x - hw - STUB_LEN - LABEL_PAD);
            ys.push(pl.center.y - hh);
            xs.push(pl.center.x + hw + STUB_LEN + LABEL_PAD);
            ys.push(pl.center.y + hh + HEADER_TEXT_H);
        }
        let want = Bounds {
            x0: xs.iter().min().unwrap() - MARGIN,
            y0: ys.iter().min().unwrap() - MARGIN,
            x1: xs.iter().max().unwrap() + MARGIN,
            y1: ys.iter().max().unwrap() + MARGIN,
        };
        assert_eq!(fs.bounds, want);

        // Wire points join the gather: an authored waypoint below everything drags y0.
        let (doc, lib) = build(
            "inst C1 Cap\ninst C2 Cap\nschematic {\n  row gap=10mm {\n    sym C1\n    sym C2\n    wire C1.p2 C2.p2 via (0mm, -50mm)\n  }\n}\n",
        );
        let fs = schematic_features(&doc, &lib);
        assert_eq!(
            fs.bounds.y0,
            -50 * MM - MARGIN,
            "an outlying waypoint extends the bounds"
        );
    }

    /// The artwork seam: the derived box-with-pins body is one closed outline sized by
    /// [`symbol_extent`], with one anchor per [`pin_slots`] slot whose tip sits exactly
    /// [`STUB_LEN`] outward from its base along the stub line.
    #[test]
    fn symbol_body_seam_matches_the_slot_conventions() {
        let lib = part_library();
        let def = &lib["MCU"];
        let body = symbol_body(def);
        assert_eq!(body.body.len(), 1, "one outline primitive today");
        let (class, Shape::Polygon { pts, .. }) = &body.body[0] else {
            panic!("derived body is a polygon outline");
        };
        assert_eq!(*class, StyleClass::SymbolOutline);
        let ext = symbol_extent(def);
        assert_eq!(pts.iter().map(|p| p.x).max().unwrap(), ext.w / 2);
        assert_eq!(pts.iter().map(|p| p.y).max().unwrap(), ext.h / 2);

        let slots = pin_slots(def);
        assert_eq!(body.pins.len(), slots.len(), "one anchor per slot");
        for (anchor, slot) in body.pins.iter().zip(&slots) {
            assert_eq!(anchor.id, slot.id);
            assert_eq!(anchor.name, slot.name);
            assert_eq!(anchor.base.y, anchor.tip.y, "stub line is horizontal");
            assert_eq!(
                (anchor.tip.x - anchor.base.x).abs(),
                STUB_LEN,
                "tip is STUB_LEN out from the base"
            );
            assert_eq!(
                anchor.tip.x.signum(),
                anchor.base.x.signum(),
                "tip points outward"
            );
        }
    }
}
