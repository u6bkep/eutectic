//! The schematic canvas: a read-only projection from the elaborated schematic
//! **reflow layout** to damascene [`VectorAsset`]s (milestone 4).
//!
//! This is the schematic twin of [`crate::canvas`]. Where the board canvas walks
//! `world_features`, the schematic canvas consumes the *same reflow layout the SVG
//! export does* — [`Doc::reflow_schematic`] (per-component [`Placement`]s: box centre +
//! extent, schematic-space nm, y-up) plus the authored presentational wires
//! ([`SchematicLayout::wires`]) — and renders symbol boxes with pin stubs, wires, net
//! tags, and text labels, matching [`schematic_svg`](eutectic_core::schematic_svg)'s
//! conventions (§20c/§20d): stubs from `pin_slots` in the part's unrotated frame,
//! rotated by the authored `rot`; wires meet stub *tips*; tags at the tip, names inside
//! the edge; the flip-within-bounds so the drawing reads upright.
//!
//! # Text as stroked glyphs
//!
//! A viewport child is a vector asset in content space, so text can't be a damascene
//! `text()` El (which flows in chrome layout). Like the board silk layer, the schematic
//! renders every label — the `refdes (part)` header, each pin name, each net tag — as
//! **stroked glyph polylines** via [`eutectic_core::font::text_strokes`] (the same public
//! stroke font the board silk uses), so labels register pixel-exact in the same asset.
//! This differs from `schematic_svg.rs`, which emits `<text>`; see the module deviation
//! note. The geometry (boxes / stubs / wires / tag positions) is byte-conceptually the
//! SVG's; only the glyph *rendering* differs (stroked vs `<text>`).
//!
//! # Caching + overlay
//!
//! Same discipline as the board canvas: [`SchematicView::build`] does the expensive
//! projection once per doc load and holds the static asset + pick candidates; per frame
//! only the cached asset is cloned into an `El`, and a fresh [`crate::canvas::Overlay`]-
//! style highlight asset is stacked on top (never re-tessellating the static layer).

use crate::canvas::pick::{SemanticId, tolerance_nm};
use damascene_core::prelude::{
    Color, El, PathBuilder, VectorAsset, VectorPath, VectorRenderMode, vector,
};
use damascene_core::vector::{VectorLineCap, VectorLineJoin};
use damascene_core::viewport::ViewportView;
use eutectic_core::annotate;
use eutectic_core::coord::{MM, Nm, Point};
use eutectic_core::doc::{Doc, Orient};
use eutectic_core::font::{Justify, text_strokes};
use eutectic_core::id::{EntityId, NetId};
use eutectic_core::part::{PartDef, PartLib};
use eutectic_core::schematic::{PinSide, PinSlot, Placement, Wire, pin_slots, symbol_extent};
use std::collections::{BTreeMap, BTreeSet};

/// Length of a pin stub out from the box edge — mirrors `schematic_svg::STUB_LEN`
/// (half a pin pitch). Kept in sync with the SVG so wires meet the stub tips identically.
const STUB_LEN: Nm = 1_270_000;
/// Text heights (nm), matching `schematic_svg`'s `PIN_TEXT_H` / `HEADER_TEXT_H` /
/// `TAG_TEXT_H`, so the drawing's label sizing tracks the SVG convention.
const PIN_TEXT_H: Nm = 1_000_000;
const HEADER_TEXT_H: Nm = 1_500_000;
const TAG_TEXT_H: Nm = 1_000_000;
/// Drawing margin (nm) — `schematic_svg::MARGIN`.
const MARGIN: Nm = 2 * MM;
/// Horizontal label pad for bounds (nm) — `schematic_svg::LABEL_PAD`, so labels stay in
/// the framed view without measuring glyphs for the viewBox.
const LABEL_PAD: Nm = 20 * MM;

/// Stroke widths (mm) for the schematic vector paths. The SVG uses 0.1 for boxes/stubs,
/// 0.15 for wires; a text pen is derived from height. Screen-independent (the viewport
/// scales with zoom).
const BOX_STROKE_MM: f32 = 0.1;
const WIRE_STROKE_MM: f32 = 0.15;
/// Text pen width (mm): a thin stroke so glyphs read as line art, not filled ink.
const TEXT_PEN_MM: f32 = 0.12;
/// Overlay highlight stroke (mm) — matches the board overlay accent width.
const OVERLAY_STROKE_MM: f32 = 0.2;

/// The schematic projection held in app state: the shared content bounds (for coordinate
/// inversion + framing) and the cached static [`VectorAsset`] plus the pick candidates.
/// Built once per doc load by [`SchematicView::build`]; per frame only the asset is
/// cloned (`content_hash` dedupes the GPU upload).
#[derive(Clone, Debug)]
pub struct SchematicView {
    /// Content bounds in schematic nm `(x0, y0, x1, y1)`, margin included — the asset
    /// viewBox in mm (y already flipped to read upright).
    bounds: (Nm, Nm, Nm, Nm),
    /// The tessellated static drawing (boxes, stubs, wires, labels). Cloned per frame.
    asset: VectorAsset,
    /// Pickable candidates (pins ▸ wires ▸ symbol bodies), folded from the same reflow the
    /// asset renders. The schematic hit-test input.
    candidates: Vec<SchematicCandidate>,
}

/// One pickable schematic feature: a semantic id, the schematic-space test geometry, and
/// the pick priority (pin ▸ wire ▸ symbol — the schematic analog of the board ordering).
#[derive(Clone, Debug)]
pub struct SchematicCandidate {
    /// The id selected when this candidate wins.
    pub id: SemanticId,
    /// The pick geometry in schematic nm (y-up), one of the shape kinds below.
    geom: PickGeom,
    /// Priority — lower wins (pin=0, wire=1, symbol=2).
    priority: u8,
}

/// Schematic pick geometry: a symbol body is a box (half-extents about a centre); a pin is
/// a point at its stub tip; a wire is a polyline. Containment/nearness is tested per kind.
#[derive(Clone, Debug)]
enum PickGeom {
    /// Axis-aligned box: centre + half-width/half-height.
    Box { c: Point, hw: Nm, hh: Nm },
    /// A point (a pin stub tip) — hit within tolerance.
    Point(Point),
    /// A polyline (a wire) — hit within tolerance of any segment.
    Poly(Vec<Point>),
}

impl SchematicView {
    /// Project `doc` into a schematic canvas: reflow the layout, build the static drawing
    /// asset + pick candidates, and hold the content bounds. `None` when the doc has no
    /// components (an empty schematic — the caller shows an empty pane), so the viewBox is
    /// never degenerate. Never panics.
    pub fn build(doc: &Doc, lib: &PartLib) -> Option<SchematicView> {
        let placements = doc.reflow_schematic(lib);
        if placements.is_empty() {
            return None;
        }
        let refdes = annotate::refdes(doc, lib, &annotate::registry(&doc.source));
        let rots = symbol_rotations(doc);
        let pin_net = pin_net_map(doc);
        let wires = wire_polylines(doc, &placements, lib, &rots);

        let bounds = content_bounds(&placements, &wires);
        let (_, y0, _, y1) = bounds;
        let flip_sum = y0 + y1;
        let view_box = view_box(bounds);

        // --- static paths (wires under symbols, §20d) ---
        let mut paths: Vec<VectorPath> = Vec::new();
        for w in &wires {
            if w.poly.len() < 2 {
                continue;
            }
            paths.push(polyline_path(
                &w.poly,
                flip_sum,
                wire_color(),
                WIRE_STROKE_MM,
            ));
        }
        for (id, pl) in &placements {
            let comp = &doc.components[id];
            let def = lib.get(&comp.part);
            let rot = rots.get(id.as_str()).copied().unwrap_or(Orient::IDENTITY);
            // Box.
            let (hw, hh) = (pl.extent.w / 2, pl.extent.h / 2);
            paths.push(box_path(pl.center, hw, hh, flip_sum, box_color()));
            // Header: `refdes (part)`, baseline-left above the box top-left.
            let designator = refdes.get(id).map(String::as_str).unwrap_or(id.as_str());
            let header = format!("{designator} ({})", comp.part);
            let (hx, hy) = (pl.center.x - hw, pl.center.y + hh + fmt_gap());
            paths.extend(text_paths(
                &header,
                Point { x: hx, y: hy },
                HEADER_TEXT_H,
                Anchor::Start,
                flip_sum,
                box_color(),
            ));
            // Stubs + pin names + net tags.
            if let Some(def) = def {
                let unrot_hw = symbol_extent(def).w / 2;
                for slot in pin_slots(def) {
                    let g = stub_geometry(slot.side, unrot_hw, slot.dy, rot);
                    let base = offset(pl.center, g.base);
                    let tip = offset(pl.center, g.tip);
                    paths.push(polyline_path(
                        &[base, tip],
                        flip_sum,
                        box_color(),
                        BOX_STROKE_MM,
                    ));
                    // Pin name inside the edge.
                    let name_at = offset(pl.center, g.name);
                    paths.extend(text_paths(
                        &slot.name,
                        name_at,
                        PIN_TEXT_H,
                        g.name_anchor,
                        flip_sum,
                        box_color(),
                    ));
                    // Net tag at the tip.
                    let key = (id.to_string(), slot.id.clone());
                    if let Some(net) = pin_net.get(&key) {
                        let tag_at = offset(pl.center, g.tag);
                        paths.extend(text_paths(
                            net,
                            tag_at,
                            TAG_TEXT_H,
                            g.tag_anchor,
                            flip_sum,
                            tag_color(),
                        ));
                    }
                }
            }
        }

        let candidates = build_candidates(doc, lib, &placements, &rots, &wires);
        Some(SchematicView {
            bounds,
            asset: VectorAsset::from_paths(view_box, paths),
            candidates,
        })
    }

    /// The pickable candidates (for the app's hit-test path).
    pub fn candidates(&self) -> &[SchematicCandidate] {
        &self.candidates
    }

    /// The cached static drawing as one `El`, keyed `key` (per-pane). Clones the asset
    /// only — cheap per frame.
    pub fn static_el(&self, key: &str) -> El {
        vector(self.asset.clone())
            .vector_render_mode(VectorRenderMode::Painted)
            .key(key.to_string())
    }

    /// The per-frame highlight overlay `El` for a set of highlighted ids, or `None` when
    /// nothing highlights. Projects each id into its schematic geometry (symbol halo, pin
    /// tick, wire highlight) — the schematic side of cross-view highlighting. `key` is the
    /// per-pane overlay key.
    pub fn overlay_el(&self, highlights: &BTreeSet<SemanticId>, key: &str) -> Option<El> {
        let (_, y0, _, y1) = self.bounds;
        let flip_sum = y0 + y1;
        let mut paths: Vec<VectorPath> = Vec::new();
        for c in &self.candidates {
            if !highlights.contains(&c.id) {
                continue;
            }
            match &c.geom {
                PickGeom::Box { c: ctr, hw, hh } => {
                    paths.push(box_path(*ctr, *hw, *hh, flip_sum, overlay_color()));
                }
                PickGeom::Point(p) => {
                    // A small halo cross at the pin tip.
                    let d = STUB_LEN / 3;
                    paths.push(polyline_path(
                        &[Point { x: p.x - d, y: p.y }, Point { x: p.x + d, y: p.y }],
                        flip_sum,
                        overlay_color(),
                        OVERLAY_STROKE_MM,
                    ));
                    paths.push(polyline_path(
                        &[Point { x: p.x, y: p.y - d }, Point { x: p.x, y: p.y + d }],
                        flip_sum,
                        overlay_color(),
                        OVERLAY_STROKE_MM,
                    ));
                }
                PickGeom::Poly(poly) => {
                    if poly.len() >= 2 {
                        paths.push(polyline_path(
                            poly,
                            flip_sum,
                            overlay_color(),
                            OVERLAY_STROKE_MM,
                        ));
                    }
                }
            }
        }
        if paths.is_empty() {
            return None;
        }
        let asset = VectorAsset::from_paths(view_box(self.bounds), paths);
        Some(
            vector(asset)
                .vector_render_mode(VectorRenderMode::Painted)
                .key(key.to_string()),
        )
    }

    /// Resolve a schematic-space query point (nm) to the winning pick, honoring the pin ▸
    /// wire ▸ symbol priority. `tol_nm` is the board-space grab radius (from
    /// [`tolerance_nm`]). Pure and unit-testable.
    pub fn resolve(&self, p: Point, tol_nm: Nm) -> Option<SemanticId> {
        let mut best: Option<&SchematicCandidate> = None;
        for c in &self.candidates {
            if !c.geom.hits(p, tol_nm) {
                continue;
            }
            best = Some(match best {
                None => c,
                Some(b) if c.priority < b.priority => c,
                Some(b) => b,
            });
        }
        best.map(|c| c.id.clone())
    }

    /// The laid-out rect of the schematic's vector-asset El inside a pane's
    /// viewport — the `el_rect` [`pointer_to_schematic_nm`](Self::pointer_to_schematic_nm)
    /// expects. Same natural-size layout fact as
    /// [`Canvas::content_rect`](crate::canvas::Canvas::content_rect): the asset
    /// child is laid out at one viewBox unit per logical px anchored at the
    /// viewport's inner top-left, so the honest rect is `(x, y, vw, vh)` — not
    /// the viewport's own rect.
    pub fn content_rect(&self, viewport_rect: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        let [_, _, vw, vh] = view_box(self.bounds);
        (viewport_rect.0, viewport_rect.1, vw, vh)
    }

    /// Map a viewport pointer (logical px) to a schematic point in nm (y-up), composing
    /// unproject + viewBox/rect scale + y-flip + mm→nm — the schematic twin of
    /// [`crate::canvas::pick::pointer_to_board_nm`]. `None` for a degenerate rect.
    pub fn pointer_to_schematic_nm(
        &self,
        pointer_px: (f32, f32),
        el_rect: (f32, f32, f32, f32),
        vv: ViewportView,
    ) -> Option<Point> {
        let (rx, ry, rw, rh) = el_rect;
        let content_px = vv.unproject(pointer_px, (rx, ry));
        let [vx, vy, vw, vh] = view_box(self.bounds);
        if rw <= 0.0 || rh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return None;
        }
        let sx = rw / vw;
        let sy = rh / vh;
        let view_mm = (vx + (content_px.0 - rx) / sx, vy + (content_px.1 - ry) / sy);
        // Undo the y-flip: view_y = flip_sum_mm - schem_y.
        let (_, y0, _, y1) = self.bounds;
        let flip_sum_mm = nm_to_mm(y0 + y1);
        let schem_mm = (view_mm.0, flip_sum_mm - view_mm.1);
        Some(Point {
            x: (schem_mm.0 * MM as f32).round() as Nm,
            y: (schem_mm.1 * MM as f32).round() as Nm,
        })
    }

    /// The tolerance helper, re-exposed so the app converts px→nm the same way as the board.
    pub fn tolerance_nm(tol_px: f32, zoom: f32) -> Nm {
        tolerance_nm(tol_px, zoom)
    }
}

impl PickGeom {
    /// Does this geometry contain / lie within `tol` of `p`?
    fn hits(&self, p: Point, tol: Nm) -> bool {
        let tol = tol.max(0);
        match self {
            PickGeom::Box { c, hw, hh } => {
                (p.x - c.x).abs() <= hw + tol && (p.y - c.y).abs() <= hh + tol
            }
            PickGeom::Point(q) => {
                let dx = (p.x - q.x) as i128;
                let dy = (p.y - q.y) as i128;
                let t = tol as i128;
                dx * dx + dy * dy <= t * t
            }
            PickGeom::Poly(poly) => poly
                .windows(2)
                .any(|w| point_seg_dist2(p, w[0], w[1]) <= (tol as i128) * (tol as i128)),
        }
    }
}

/// Squared distance (nm², i128) from point `p` to segment `a`-`b`.
fn point_seg_dist2(p: Point, a: Point, b: Point) -> i128 {
    let (px, py) = (p.x as i128, p.y as i128);
    let (ax, ay) = (a.x as i128, a.y as i128);
    let (bx, by) = (b.x as i128, b.y as i128);
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    if len2 == 0 {
        let ex = px - ax;
        let ey = py - ay;
        return ex * ex + ey * ey;
    }
    // Clamp the projection parameter t = ((p-a)·(b-a)) / len2 to [0,1], in integer math.
    let t_num = (px - ax) * dx + (py - ay) * dy;
    let (cx, cy) = if t_num <= 0 {
        (ax, ay)
    } else if t_num >= len2 {
        (bx, by)
    } else {
        // Closest point = a + (t_num/len2)*(dx,dy); compute with rounding.
        (ax + t_num * dx / len2, ay + t_num * dy / len2)
    };
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey
}

// ----------------------------------------------------------------------------
// Candidate building.
// ----------------------------------------------------------------------------

/// Build the pick candidates from the reflow: one symbol-body box per placed component,
/// one pin-tip point per pin (keyed by pad number → `SemanticId::Pin`), and one polyline
/// per wire (→ the wire's net, if both ends agree, via the net map).
fn build_candidates(
    doc: &Doc,
    lib: &PartLib,
    placements: &BTreeMap<EntityId, Placement>,
    rots: &BTreeMap<String, Orient>,
    wires: &[WirePoly],
) -> Vec<SchematicCandidate> {
    let mut out: Vec<SchematicCandidate> = Vec::new();
    for (id, pl) in placements {
        let comp = &doc.components[id];
        // Symbol body (priority 2 — least specific).
        out.push(SchematicCandidate {
            id: SemanticId::Part(id.clone()),
            geom: PickGeom::Box {
                c: pl.center,
                hw: pl.extent.w / 2,
                hh: pl.extent.h / 2,
            },
            priority: 2,
        });
        // Pins (priority 0 — most specific). Keyed by pad NUMBER (the `PinRef` join key).
        if let Some(def) = lib.get(&comp.part) {
            let unrot_hw = symbol_extent(def).w / 2;
            let rot = rots.get(id.as_str()).copied().unwrap_or(Orient::IDENTITY);
            for slot in pin_slots(def) {
                let g = stub_geometry(slot.side, unrot_hw, slot.dy, rot);
                let tip = offset(pl.center, g.tip);
                out.push(SchematicCandidate {
                    id: SemanticId::Pin {
                        comp: id.clone(),
                        pin: pin_number(def, &slot),
                    },
                    geom: PickGeom::Point(tip),
                    priority: 0,
                });
            }
        }
    }
    // Wires (priority 1) → net. A wire is presentational; its selectable identity is the
    // net it draws (the cross-view currency). Resolve the net from either endpoint pin.
    for w in wires {
        if let Some(net) = w.net.clone() {
            out.push(SchematicCandidate {
                id: SemanticId::Net(net),
                geom: PickGeom::Poly(w.poly.clone()),
                priority: 1,
            });
        }
    }
    out
}

/// The stored pin identity (pad number, or `port.signal`) of a slot — the `PinRef` join
/// key. `PinSlot::id` already carries it; kept as a helper so the intent reads clearly.
fn pin_number(_def: &PartDef, slot: &PinSlot) -> String {
    slot.id.clone()
}

// ----------------------------------------------------------------------------
// Wires (replicating schematic_svg's wire_polylines / wire_end_point).
// ----------------------------------------------------------------------------

/// A drawn wire as a schematic-space polyline plus the net it belongs to (for picking +
/// cross-highlight). The net is the net both endpoint pins agree on, if any.
struct WirePoly {
    poly: Vec<Point>,
    net: Option<NetId>,
}

/// Each drawn wire as a schematic-space polyline (pin-A tip, waypoints, pin-B tip),
/// replicating `schematic_svg::wire_polylines` (a wire is dropped only when an endpoint is
/// genuinely absent / unresolvable). Also resolves the wire's net from its endpoints.
fn wire_polylines(
    doc: &Doc,
    placements: &BTreeMap<EntityId, Placement>,
    lib: &PartLib,
    rots: &BTreeMap<String, Orient>,
) -> Vec<WirePoly> {
    let mut out = Vec::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    let pin_net = pin_net_map(doc);
    for w in layout.wires() {
        let (Some(a), Some(b)) = (
            wire_end_point(doc, placements, lib, rots, &w.a.comp, &w.a.pin),
            wire_end_point(doc, placements, lib, rots, &w.b.comp, &w.b.pin),
        ) else {
            continue;
        };
        let mut poly = vec![a.0];
        poly.extend(w.waypoints.iter().copied());
        poly.push(b.0);
        // The wire's net: the net of endpoint A (matching the tag drawn there); fall back
        // to B. A wire whose ends disagree earns a core warning; either net is a fine
        // cross-highlight target.
        let net = wire_net(&pin_net, w, a.1, b.1);
        out.push(WirePoly { poly, net });
    }
    out
}

/// The net a wire highlights: the net of endpoint A's pin, else endpoint B's. Each `_pin_id`
/// is the resolved stored pin id (pad number / `port.signal`) at that end.
fn wire_net(
    pin_net: &BTreeMap<(String, String), String>,
    w: &Wire,
    a_pin: String,
    b_pin: String,
) -> Option<NetId> {
    let a = pin_net.get(&(w.a.comp.clone(), a_pin));
    let b = pin_net.get(&(w.b.comp.clone(), b_pin));
    a.or(b).map(NetId::new)
}

/// The schematic-space point of a wire endpoint (the pin's stub *tip*), replicating
/// `schematic_svg::wire_end_point`. Returns the tip point and the resolved stored pin id.
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
    let ids = def.resolve_selector(pin);
    let want = ids.first().map(String::as_str).unwrap_or(pin);
    let slot = pin_slots(def).into_iter().find(|s| s.id == want)?;
    let rot = rots.get(comp).copied().unwrap_or(Orient::IDENTITY);
    let unrot_hw = symbol_extent(def).w / 2;
    let g = stub_geometry(slot.side, unrot_hw, slot.dy, rot);
    Some((offset(pl.center, g.tip), slot.id))
}

// ----------------------------------------------------------------------------
// Stub geometry (replicating schematic_svg's private stub_geometry).
// ----------------------------------------------------------------------------

/// The text anchor for a label — start / middle / end relative to the given origin, so
/// pin names hug the interior and net tags read outward (like the SVG's `text-anchor`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Start,
    End,
}

/// Geometry of one pin stub in the box frame (offsets from the box centre), replicating
/// `schematic_svg::stub_geometry`: base on the edge, tip, tag anchor, name anchor, and the
/// two text anchors — all rotated by the authored cardinal `rot`.
struct StubGeom {
    base: Point,
    tip: Point,
    tag: Point,
    name: Point,
    name_anchor: Anchor,
    tag_anchor: Anchor,
}

fn stub_geometry(side: PinSide, hw: Nm, dy: Nm, rot: Orient) -> StubGeom {
    let sign: Nm = match side {
        PinSide::Left => -1,
        PinSide::Right => 1,
    };
    let edge = Point {
        x: sign * hw,
        y: dy,
    };
    let tip = Point {
        x: sign * (hw + STUB_LEN),
        y: dy,
    };
    let tag = Point {
        x: sign * (hw + STUB_LEN + STUB_LEN / 2),
        y: dy,
    };
    let name = Point {
        x: sign * (hw - STUB_LEN / 4),
        y: dy,
    };
    let (name_anchor, tag_anchor) = match side {
        PinSide::Left => (Anchor::Start, Anchor::End),
        PinSide::Right => (Anchor::End, Anchor::Start),
    };
    StubGeom {
        base: rot.apply(edge),
        tip: rot.apply(tip),
        tag: rot.apply(tag),
        name: rot.apply(name),
        name_anchor,
        tag_anchor,
    }
}

/// Add a box-frame offset to a component centre → an absolute schematic point.
fn offset(center: Point, off: Point) -> Point {
    Point {
        x: center.x + off.x,
        y: center.y + off.y,
    }
}

/// A small vertical gap (nm) lifting the header off the box top — `schematic_svg::fmt_gap`.
fn fmt_gap() -> Nm {
    500_000
}

// ----------------------------------------------------------------------------
// Shared reflow reads (net map, rotations) — the same data schematic_svg derives.
// ----------------------------------------------------------------------------

/// Pin identity `(comp, pin-id)` → net name, from the materialized netlist — the tag
/// source (`schematic_svg`'s `pin_net`).
fn pin_net_map(doc: &Doc) -> BTreeMap<(String, String), String> {
    doc.nets
        .values()
        .flat_map(|net| {
            net.members
                .iter()
                .map(move |m| ((m.comp.to_string(), m.pin.clone()), net.name.clone()))
        })
        .collect()
}

/// Authored schematic rotation per component path (`schematic_svg`'s `symbol_rotations`).
fn symbol_rotations(doc: &Doc) -> BTreeMap<String, Orient> {
    let mut out = BTreeMap::new();
    let Some(layout) = &doc.schematic else {
        return out;
    };
    for s in layout_symbols(layout) {
        out.insert(s.0, s.1);
    }
    out
}

/// Every `(path, rot)` in the tree — a thin pre-order walk over the public wires/roots API
/// is not available, so we walk `symbol_paths` paired with rot via the public `symbols`
/// accessor is `pub(crate)`; instead reconstruct from the layout's public surface.
fn layout_symbols(layout: &eutectic_core::schematic::SchematicLayout) -> Vec<(String, Orient)> {
    // `SchematicLayout` exposes `wires()` publicly; symbol rot is read via a pre-order walk
    // of `roots` (public field). Recurse over LayoutNode.
    use eutectic_core::schematic::LayoutNode;
    fn walk(nodes: &[LayoutNode], out: &mut Vec<(String, Orient)>) {
        for n in nodes {
            match n {
                LayoutNode::Symbol(s) => out.push((s.path.clone(), s.rot)),
                LayoutNode::Container(c) => walk(&c.children, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&layout.roots, &mut out);
    out
}

// ----------------------------------------------------------------------------
// Bounds + coordinate mapping (schematic twin of canvas.rs).
// ----------------------------------------------------------------------------

/// Content bounds in schematic nm `(x0, y0, x1, y1)` with the margin — replicating
/// `schematic_svg`'s bounds loop (box corners + stub/label reach + wire points).
fn content_bounds(
    placements: &BTreeMap<EntityId, Placement>,
    wires: &[WirePoly],
) -> (Nm, Nm, Nm, Nm) {
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
    (x0, y0, x1, y1)
}

/// The asset viewBox `[min_x, min_y, w, h]` in mm from schematic-nm bounds (y-down frame).
fn view_box(bounds: (Nm, Nm, Nm, Nm)) -> [f32; 4] {
    let (x0, y0, x1, y1) = bounds;
    [
        nm_to_mm(x0),
        nm_to_mm(y0),
        nm_to_mm(x1 - x0),
        nm_to_mm(y1 - y0),
    ]
}

/// Fixed-point nm → mm f32.
fn nm_to_mm(nm: Nm) -> f32 {
    nm as f32 / MM as f32
}

/// Schematic point (nm, y-up) → viewBox (mm, y-down): `view_y = flip_sum - y`.
fn to_view(p: Point, flip_sum: Nm) -> (f32, f32) {
    (nm_to_mm(p.x), nm_to_mm(flip_sum - p.y))
}

// ----------------------------------------------------------------------------
// Path builders.
// ----------------------------------------------------------------------------

/// An unfilled stroked box (the symbol body outline).
fn box_path(center: Point, hw: Nm, hh: Nm, flip_sum: Nm, color: Color) -> VectorPath {
    let corners = [
        Point {
            x: center.x - hw,
            y: center.y - hh,
        },
        Point {
            x: center.x + hw,
            y: center.y - hh,
        },
        Point {
            x: center.x + hw,
            y: center.y + hh,
        },
        Point {
            x: center.x - hw,
            y: center.y + hh,
        },
    ];
    let mut b = PathBuilder::new();
    for (i, p) in corners.iter().enumerate() {
        let (x, y) = to_view(*p, flip_sum);
        b = if i == 0 {
            b.move_to(x, y)
        } else {
            b.line_to(x, y)
        };
    }
    b.close().stroke_solid(color, BOX_STROKE_MM).build()
}

/// A stroked polyline in schematic space (stub / wire / overlay).
fn polyline_path(pts: &[Point], flip_sum: Nm, color: Color, width_mm: f32) -> VectorPath {
    let mut b = PathBuilder::new();
    for (i, p) in pts.iter().enumerate() {
        let (x, y) = to_view(*p, flip_sum);
        b = if i == 0 {
            b.move_to(x, y)
        } else {
            b.line_to(x, y)
        };
    }
    b.stroke_solid(color, width_mm)
        .stroke_line_cap(VectorLineCap::Round)
        .stroke_line_join(VectorLineJoin::Round)
        .build()
}

/// Text as stroked-glyph polylines (the board-silk approach), placed at `origin` (nm, the
/// baseline-left of the run before anchor adjustment) at `height`, honoring the anchor by
/// shifting the run's ink width. The glyph strokes are y-up in the local frame (baseline at
/// y=0, ascending +y), which matches schematic space, so they place directly.
fn text_paths(
    s: &str,
    origin: Point,
    height: Nm,
    anchor: Anchor,
    flip_sum: Nm,
    color: Color,
) -> Vec<VectorPath> {
    if s.is_empty() {
        return Vec::new();
    }
    let strokes = text_strokes(s, height, Justify::Left);
    // Ink width for anchor adjustment: the run's x-extent in the local frame.
    let mut min_x = Nm::MAX;
    let mut max_x = Nm::MIN;
    for stroke in &strokes {
        for p in stroke {
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
        }
    }
    let width = if max_x >= min_x { max_x - min_x } else { 0 };
    // Left-justified: origin.x is the left edge. For an `End` anchor the run should end at
    // origin.x, so shift left by the full width. (There is no Center anchor use here.)
    let shift_x = match anchor {
        Anchor::Start => 0,
        Anchor::End => -width,
    };
    // Baseline: text_strokes puts the baseline at local y=0; centre the run vertically on
    // the origin so a tag/name sits centred on its stub line (the SVG nudges by TEXT_H/3;
    // vertical centring reads equivalently and is anchor-agnostic).
    let shift_y = -height / 2;
    let mut out = Vec::new();
    for stroke in &strokes {
        if stroke.is_empty() {
            continue;
        }
        let placed: Vec<Point> = stroke
            .iter()
            .map(|p| Point {
                x: origin.x + p.x + shift_x,
                y: origin.y + p.y + shift_y,
            })
            .collect();
        out.push(polyline_path(&placed, flip_sum, color, TEXT_PEN_MM));
    }
    out
}

// ----------------------------------------------------------------------------
// Palette (schematic is monochrome line art; a light ink on the dark canvas).
// ----------------------------------------------------------------------------

/// Symbol boxes, stubs, headers, pin names — a light ink on the dark canvas.
fn box_color() -> Color {
    Color::srgb_token("eutectic.schematic.ink", 0xd8, 0xd8, 0xd8, 0xff)
}

/// Wires — a green trace, matching the SVG's `#0a0` wire stroke intent.
fn wire_color() -> Color {
    Color::srgb_token("eutectic.schematic.wire", 0x2e, 0xa0, 0x43, 0xff)
}

/// Net tags — a muted cyan so net labels read distinct from the ink.
fn tag_color() -> Color {
    Color::srgb_token("eutectic.schematic.tag", 0x6f, 0xb7, 0xc9, 0xff)
}

/// Highlight overlay accent — the same bright cyan the board overlay uses (cross-view
/// consistency).
fn overlay_color() -> Color {
    Color::srgb_token("eutectic.overlay.select", 0x22, 0xd3, 0xee, 0xff)
}

#[cfg(test)]
mod tests;
