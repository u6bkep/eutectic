//! The board canvas: a pure projection from an elaborated [`Doc`] to per-layer
//! damascene [`VectorAsset`]s (structural commitment 1, the *layered canvas*; see
//! `docs/gui-architecture.md`, "Canvas strategy").
//!
//! This is milestone 2's read-only board viewer. The projection here is the GUI
//! twin of `ecad-core`'s SVG backend (`export/svg.rs`): it walks the same unified
//! [`world_features`](ecad_core::route::world_features) stream, bins each physical
//! feature onto the board layer it lives on (by matching its z to a stackup slab),
//! and renders each layer's geometry into one [`VectorAsset`]. Where `svg.rs`
//! composites every layer into a single top-view SVG, the canvas keeps each layer
//! as its own asset so visibility toggles simply include or exclude an `El` — never
//! re-tessellate (the whole point of the layered commitment).
//!
//! # Coordinate mapping
//!
//! One asset viewBox unit == **1 mm**; at viewport zoom `1.0` that is one logical
//! pixel per mm. Every layer shares one viewBox anchored to the board content
//! bounding box (with the same 2 mm margin `svg.rs` uses), so the layers register
//! against each other and the whole board frames with a single `FitContent`.
//!
//! The framing tracks `svg.rs`'s but is **not byte-identical** to it: `svg.rs`
//! sizes its viewBox from a hand-gathered point set (pad centres via `pin_world`,
//! footprint `def.graphics` world extents, board-text `Role::Marking` source
//! features), whereas the canvas derives bounds from the `world_features` stream it
//! already walks — the *inflated copper regions* (a trace is its capsule, a pad its
//! full copper) and the lowered silk. Same roles in view (copper + silk, never fab
//! `Datum`/substrate/mask/void), same 2 mm margin, so the board frames the same way;
//! the exact viewBox differs by up to a copper half-width / silk-extent (a few mm on
//! the poc board). See [`content_bounds`].
//!
//! The model's y axis points **up** (ECAD convention); SVG / screen y points
//! **down**. Like `svg.rs` we flip y within the content bounds
//! (`flip(y) = y0 + y1 - y`) so the canvas reads upright and visually matches
//! `poc/out/board.svg` rather than its mirror. [`board_to_view`] is the single
//! nm→mm + y-flip seam; [`Canvas::view_to_board_mm`] is its inverse, for the
//! status-bar cursor readout.
//!
//! # Caching
//!
//! [`Canvas::build_layers`] does the expensive work (walk features, tessellate
//! paths) **once** and returns owned [`BoardLayer`]s the app holds across frames.
//! Per frame, [`Canvas::layer_els`] only *clones* the cached [`VectorAsset`]s into
//! `El`s; `content_hash` makes the GPU upload idempotent, so re-emitting the same
//! asset every frame is free past the first. A `dynamic overlay` seam
//! ([`Canvas::overlay_el`]) is reserved for the per-frame layer (selection / DRC /
//! tool previews) that arrives in milestone 3+; it is empty here.

use damascene_core::prelude::{
    Color, El, PathBuilder, VectorAsset, VectorColor, VectorFill, VectorFillRule, VectorPath,
    VectorRenderMode, vector,
};
use ecad_core::coord::{MM, Nm, Point};
use ecad_core::doc::{Doc, PinRef};
use ecad_core::geom::kernel::{DEFAULT_CIRCLE_SEGS, Region, shape_to_region};
use ecad_core::geom::{Extent, NetFeature, Role, Shape2D, Stackup, ZRange};
use ecad_core::id::NetId;
use ecad_core::part::{PartLib, PinRole};
use ecad_core::route::{DesignRules, world_features};
use std::collections::BTreeMap;

/// The 2 mm content margin `export/svg.rs` adds around the board bounds, so the
/// canvas viewBox matches the ground-truth SVG's framing exactly.
const MARGIN: Nm = 2 * MM;

/// Silk / mask centreline strokes narrower than this (in mm) would tessellate to
/// nothing; lyon also floors stroke width, but we keep an explicit minimum so a
/// zero-radius marking (a filled `fp_poly` mis-emitted as a stroke can't happen —
/// those go through the fill arm — but a hairline authored stroke can) still shows.
const MIN_STROKE_MM: f32 = 0.05;

/// A stable identifier for one visual board layer. Distinct from a copper
/// [`Layer`](ecad_core::route::Layer) ordinal: this enumerates *every* visual layer
/// the viewer shows (substrate, each named copper slab, each silk / mask slab), so
/// it keys on the slab name (or a synthetic name for the derived outline layer).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LayerId {
    /// The board substrate outline (∖ cutouts) — the derived board-edge layer, not
    /// a stackup slab. Drawn as an unfilled stroke, like `svg.rs`'s `outline-board`.
    Outline,
    /// A stackup slab, keyed by its authored name (`"F.Cu"`, `"B.SilkS"`, …).
    Slab(String),
}

impl LayerId {
    /// The key an `El` / viewport-independent lookups use for this layer.
    pub fn key(&self) -> String {
        match self {
            LayerId::Outline => "layer:outline".to_string(),
            LayerId::Slab(name) => format!("layer:{name}"),
        }
    }
}

/// One projected board layer: its identity, human label, palette colour, and the
/// tessellated [`VectorAsset`] that draws it. Built once per (doc, layer set) by
/// [`Canvas::build_layers`] and held in app state; [`Canvas::layer_els`] clones the
/// asset into an `El` per frame without rebuilding it.
#[derive(Clone, Debug)]
pub struct BoardLayer {
    /// Stable layer identity (also the `El` key source).
    pub id: LayerId,
    /// Display name for the layer panel (the slab name, or "Board outline").
    pub name: String,
    /// Default palette colour (dark-canvas ECAD convention). Per-net colours are a
    /// later ticket; this is the whole-layer default.
    pub color: Color,
    /// The tessellated geometry for this layer. Empty (`paths.is_empty()`) layers
    /// are still enumerated (so the panel lists them) but contribute no `El`.
    pub asset: VectorAsset,
}

/// The projection from an elaborated document to per-layer assets. Holds only the
/// shared viewBox (content bounds in mm) needed to invert screen → board
/// coordinates; the layers themselves are returned owned from [`build_layers`].
///
/// [`Canvas::build_layers`]: Canvas::build_layers
#[derive(Clone, Debug)]
pub struct Canvas {
    /// Content bounds in nm `(x0, y0, x1, y1)`, margin included — the same box
    /// `svg.rs` derives, and the asset viewBox in mm.
    bounds: (Nm, Nm, Nm, Nm),
}

impl Canvas {
    /// Project `doc` into a canvas: derive the shared content bounds (matching
    /// `svg.rs`) and hold them for coordinate inversion. Cheap — the per-layer
    /// tessellation is [`build_layers`](Canvas::build_layers).
    ///
    /// `Err` only if feature lowering fails (an unknown slab name), which a
    /// committed `Doc` never hits (the commit-time slab gate); the caller surfaces
    /// it as a load error rather than crashing.
    pub fn new(doc: &Doc, lib: &PartLib) -> Result<Canvas, String> {
        let su = ecad_core::elaborate::stackup(&doc.source);
        let features = doc_world_features(doc, lib, &su)?;
        let bounds = content_bounds(doc, &features);
        Ok(Canvas { bounds })
    }

    /// The shared asset viewBox `[min_x, min_y, width, height]` in mm — anchored to
    /// the board content bounds, y already in the flipped (downward) frame so it
    /// equals the model bounds mapped through [`board_to_view`].
    fn view_box(&self) -> [f32; 4] {
        let (x0, y0, x1, y1) = self.bounds;
        [
            nm_to_mm(x0),
            nm_to_mm(y0),
            nm_to_mm(x1 - x0),
            nm_to_mm(y1 - y0),
        ]
    }

    /// Map a viewBox (mm) point back to board coordinates in mm, undoing the y-flip
    /// — the inverse of [`board_to_view`]. This is *only* the flip inverse; it
    /// assumes its input is already in viewBox-mm (not screen or content px). The
    /// status bar goes through [`content_px_to_board_mm`](Canvas::content_px_to_board_mm),
    /// which converts the viewport's content-px to viewBox-mm first.
    pub fn view_to_board_mm(&self, view_mm: (f32, f32)) -> (f32, f32) {
        let (_, y0, _, y1) = self.bounds;
        let flip_sum_mm = nm_to_mm(y0 + y1);
        (view_mm.0, flip_sum_mm - view_mm.1)
    }

    /// Map a viewport **content-space** point (the value `ViewportView::unproject`
    /// returns — the child `El`'s layout px, origin-relative, zoom/pan removed) to
    /// board coordinates in mm, for the status-bar cursor readout.
    ///
    /// `content_px` is in the same logical-px frame as `el_rect` (the board `El`'s
    /// laid-out rect from `cx.rect_of_key`). The `VectorAsset` maps its viewBox
    /// `[vx, vy, vw, vh]` onto that rect with independent `sx = rect.w/vw`,
    /// `sy = rect.h/vh` (see damascene `append_vector_asset_mesh`), so the inverse is
    /// `vb = [vx + (px - rect.x)/sx, vy + (py - rect.y)/sy]`. That undoes both the
    /// viewBox min offset and the (possibly non-square, `Size::Fill`) rect scaling —
    /// the two corrections the old direct `view_to_board_mm(unproject())` path
    /// dropped. The resulting viewBox-mm point then goes through the flip inverse.
    ///
    /// Returns `None` for a degenerate rect (zero/negative extent), matching the
    /// asset renderer which draws nothing there.
    pub fn content_px_to_board_mm(
        &self,
        content_px: (f32, f32),
        el_rect: (f32, f32, f32, f32),
    ) -> Option<(f32, f32)> {
        let (rx, ry, rw, rh) = el_rect;
        let [vx, vy, vw, vh] = self.view_box();
        if rw <= 0.0 || rh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return None;
        }
        let sx = rw / vw;
        let sy = rh / vh;
        let view_mm = (vx + (content_px.0 - rx) / sx, vy + (content_px.1 - ry) / sy);
        Some(self.view_to_board_mm(view_mm))
    }

    /// Build every visual layer of the board, tessellating each into its own
    /// [`VectorAsset`]. **The expensive call** — run once per (doc revision, layer
    /// set) and cache the result; do not call it per frame.
    ///
    /// Layers are returned in **draw order** (bottom of the stack first, top last),
    /// so stacking their `El`s in order paints copper over substrate the way the
    /// composite SVG does. The layer *panel* lists them top-first (the caller
    /// reverses for display).
    ///
    /// `Err` propagates a feature-lowering failure (see [`Canvas::new`]).
    pub fn build_layers(&self, doc: &Doc, lib: &PartLib) -> Result<Vec<BoardLayer>, String> {
        let su = ecad_core::elaborate::stackup(&doc.source);
        let features = doc_world_features(doc, lib, &su)?;
        let view_box = self.view_box();
        let (_, y0, _, y1) = self.bounds;
        let flip_sum = y0 + y1;

        // One bucket of VectorPaths per layer id, filled by binning each feature.
        let mut buckets: BTreeMap<String, Vec<VectorPath>> = BTreeMap::new();

        // The derived outline layer: the board region (outline ∖ cutouts) as an
        // unfilled stroked path (matching `svg.rs`'s `outline-board`).
        if let Some(region) = ecad_core::elaborate::board_region(&doc.source) {
            let path = region_stroke_path(&region, flip_sum, outline_color(), 0.1);
            buckets
                .entry(LayerId::Outline.key())
                .or_default()
                .push(path);
        }

        // Every physical feature, binned onto the slab it lives on. Substrate /
        // marking / mask / conductor features each land on their slab; the drill
        // and mask-opening Voids and the substrate solid are handled per role.
        for nf in &features {
            let Extent::Prism { shape, z } = &nf.feature.extent;
            let Some(slab) = slab_of_z(&su, z) else {
                continue; // no slab spans this z (should not happen for lowered features)
            };
            let key = LayerId::Slab(slab.name.clone()).key();
            let color = layer_color(&su, &slab.name);
            let paths = match nf.feature.role {
                // Copper: the honest filled extent. A Stroke (trace / disc pad /
                // capsule) becomes its inflated region; a Polygon / Area fills
                // directly. Even-odd fill so pour knockouts read as voids — the same
                // rule `svg.rs` uses on the same rings. A pour (`Shape2D::Area`) fills
                // **translucent** (0.25, like `svg.rs`'s `fill-opacity="0.25"` on its
                // pour paths) so the board outline drawn beneath still reads through;
                // discrete copper (pads / traces / vias) stays opaque.
                Role::Conductor => {
                    let opacity = if matches!(shape, Shape2D::Area { .. }) {
                        0.25
                    } else {
                        1.0
                    };
                    vec![fill_shape_opacity(shape, flip_sum, color, opacity)]
                }
                // Silk / fab markings: mirror `svg.rs`'s `svg_surface` shape arm —
                // a Stroke draws as a centreline polyline (pen = radius*2), a
                // Polygon / Area as a filled area.
                Role::Marking | Role::Datum => marking_paths(shape, flip_sum, color),
                // The board body substrate: skip here — the Outline layer above is
                // the board edge a reader wants; a filled substrate slab would just
                // be a dark rectangle behind everything. (Kept out of the copper
                // buckets so it never obscures a layer.)
                Role::Substrate => continue,
                // Solder mask solids fill their slab; render translucent so copper
                // beneath still reads (mask is a coloured film, not opaque).
                Role::Mask => vec![fill_shape_opacity(shape, flip_sum, color, 0.3)],
                // Physical holes (authored NPTH, plated drills): an unfilled stroked
                // ring on the outline layer so the hole is visible without a
                // separate drills asset. Matches `svg.rs`'s `hole` circles.
                Role::Void => {
                    let path = fill_shape_opacity(shape, flip_sum, hole_color(), 1.0);
                    buckets
                        .entry(LayerId::Slab(drill_slab_name(&su)).key())
                        .or_default()
                        .push(path);
                    continue;
                }
                Role::Keepout(_) => continue,
            };
            buckets.entry(key).or_default().extend(paths);
        }

        // Materialize the enumerated layer set in draw order, attaching each
        // bucket's paths (empty buckets still enumerate so the panel is complete).
        let mut layers = Vec::new();
        for id in self.layer_order(&su) {
            let paths = buckets.remove(&id.key()).unwrap_or_default();
            layers.push(BoardLayer {
                name: layer_display_name(&id),
                color: match &id {
                    LayerId::Outline => outline_color(),
                    LayerId::Slab(name) => layer_color(&su, name),
                },
                asset: VectorAsset::from_paths(view_box, paths),
                id,
            });
        }
        Ok(layers)
    }

    /// The full visual layer set in **draw order** (painted bottom-first): the
    /// board outline, then every stackup slab ordered bottom-of-stack to top
    /// (ascending z) so higher copper paints over lower. A synthetic drills layer
    /// is appended last so holes sit on top of everything.
    fn layer_order(&self, su: &Stackup) -> Vec<LayerId> {
        let mut slabs: Vec<&ecad_core::geom::Slab> = su.slabs.iter().collect();
        // Ascending z (bottom of the physical stack first) → higher layers paint
        // over lower ones, matching the composite top-view look.
        slabs.sort_by_key(|s| s.z.lo);
        let mut order = vec![LayerId::Outline];
        for s in &slabs {
            order.push(LayerId::Slab(s.name.clone()));
        }
        // The synthetic drills layer (if any feature landed there) draws last.
        let drills = drill_slab_name(su);
        if !order.iter().any(|id| id == &LayerId::Slab(drills.clone())) {
            order.push(LayerId::Slab(drills));
        }
        order
    }

    /// Clone the cached layers into stacked `El`s in draw order, filtered by the
    /// `visible` predicate. **Per-frame** — cheap: only clones assets (the
    /// `content_hash` dedupes the GPU upload). Layers whose asset is empty or that
    /// `visible` rejects contribute no `El`.
    pub fn layer_els<'a>(
        &self,
        layers: &'a [BoardLayer],
        visible: impl Fn(&'a LayerId) -> bool,
    ) -> Vec<El> {
        layers
            .iter()
            .filter(|l| !l.asset.paths.is_empty() && visible(&l.id))
            .map(|l| {
                vector(l.asset.clone())
                    .vector_render_mode(VectorRenderMode::Painted)
                    .key(l.id.key())
            })
            .collect()
    }

    /// The per-frame **dynamic overlay** (structural commitment 1): the layer
    /// selection / hover highlights and tool previews render into, rebuilt every
    /// frame from the live [`Overlay`] and stacked on top of the cached static
    /// layers **without** touching them (no re-tessellation). `None` when the overlay
    /// is empty (nothing selected, no measure in progress) so the caller adds nothing.
    ///
    /// The overlay shares the layers' viewBox, so its geometry registers pixel-exact
    /// against the board; it is `Painted` on top in a single asset. This is the m3
    /// realisation of the m2 seam.
    pub fn overlay_el(&self, overlay: &Overlay) -> Option<El> {
        let (_, y0, _, y1) = self.bounds;
        let flip_sum = y0 + y1;
        let mut paths: Vec<VectorPath> = Vec::new();

        // Selection / hover highlights: an accent stroke tracing each highlighted
        // world-space copper/area shape. Selected is the bright accent; hover is a
        // dimmer pre-select tint (hover events only arrive on enter/drag/down — see
        // the free-hover deviation).
        for (shape, hovered) in &overlay.highlights {
            let color = if *hovered {
                overlay_hover_color()
            } else {
                overlay_select_color()
            };
            paths.push(shape_halo_path(shape, flip_sum, color));
        }

        // Measure preview: the anchored segment (+ live end where a pointer position
        // was available) as an accent dashed-free polyline with endpoint ticks.
        if let Some((a, b)) = overlay.measure {
            paths.push(measure_line_path(a, b, flip_sum));
        }

        if paths.is_empty() {
            return None;
        }
        let asset = VectorAsset::from_paths(self.view_box(), paths);
        Some(
            vector(asset)
                .vector_render_mode(VectorRenderMode::Painted)
                .key("overlay:dynamic"),
        )
    }
}

/// The per-frame dynamic-overlay contents: highlighted world-space shapes (selection
/// and hover) plus an optional measure segment. Built fresh each frame by the app from
/// the semantic selection and tool state. Holds only geometry to *render*, never
/// selection identity (that lives in the [`SelectionModel`](crate::selection::SelectionModel)).
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    /// World-frame (nm, y-up) shapes to highlight, each flagged `true` when it is a
    /// *hover* (dimmer) rather than a committed *selection* (bright).
    pub highlights: Vec<(Shape2D, bool)>,
    /// The measure segment `(anchor, cursor)` in world nm, if a measure is in progress.
    pub measure: Option<(Point, Point)>,
}

/// Accent stroke for a selected feature: a bright halo tracing the shape's outline.
fn shape_halo_path(shape: &Shape2D, flip_sum: Nm, color: Color) -> VectorPath {
    // Trace the shape's honest copper region boundary (same kernel as the fills), so a
    // pad/pour is haloed on its true outline and a trace on its capsule.
    let region = match shape {
        Shape2D::Area { region } => region.clone(),
        _ => shape_to_region(shape, DEFAULT_CIRCLE_SEGS),
    };
    region_stroke_path(&region, flip_sum, color, OVERLAY_STROKE_MM)
}

/// The measure polyline: a two-point segment in the accent colour, round-capped.
fn measure_line_path(a: Point, b: Point, flip_sum: Nm) -> VectorPath {
    let (ax, ay) = board_to_view(a, flip_sum);
    let (bx, by) = board_to_view(b, flip_sum);
    PathBuilder::new()
        .move_to(ax, ay)
        .line_to(bx, by)
        .stroke_solid(measure_color(), OVERLAY_STROKE_MM)
        .stroke_line_cap(damascene_core::vector::VectorLineCap::Round)
        .build()
}

/// Overlay stroke width in mm (screen-independent; the viewport scales it with zoom).
const OVERLAY_STROKE_MM: f32 = 0.15;

/// The selection halo accent — a bright cyan, distinct from every layer palette colour.
fn overlay_select_color() -> Color {
    Color::srgb_token("ecad.overlay.select", 0x22, 0xd3, 0xee, 0xff)
}

/// The hover halo — a dimmer cyan (hover is a weaker pre-select cue than selection).
fn overlay_hover_color() -> Color {
    Color::srgb_token("ecad.overlay.hover", 0x22, 0xd3, 0xee, 0x88)
}

/// The measure-line accent — a warm amber, distinct from the selection cyan.
fn measure_color() -> Color {
    Color::srgb_token("ecad.overlay.measure", 0xf5, 0xa5, 0x24, 0xff)
}

// ----------------------------------------------------------------------------
// Coordinate mapping (the single nm→mm + y-flip seam).
// ----------------------------------------------------------------------------

/// A fixed-point nanometre coordinate as millimetres (f32 viewBox units).
pub(crate) fn nm_to_mm(nm: Nm) -> f32 {
    nm as f32 / MM as f32
}

/// Map a board-frame point (nm, y-up) to a viewBox point (mm, y-down) — the canvas
/// twin of `svg.rs`'s `flip`. `flip_sum = y0 + y1` (content bounds), so
/// `view_y = flip_sum - y` before nm→mm, keeping the board upright.
fn board_to_view(p: Point, flip_sum: Nm) -> (f32, f32) {
    (nm_to_mm(p.x), nm_to_mm(flip_sum - p.y))
}

// ----------------------------------------------------------------------------
// Feature → path projection.
// ----------------------------------------------------------------------------

/// Build the `world_features` stream for a doc with default design rules — the one
/// producer both the bounds and the layer projection walk, so they always agree.
fn doc_world_features(doc: &Doc, lib: &PartLib, su: &Stackup) -> Result<Vec<NetFeature>, String> {
    let netlist = doc_netlist(doc);
    let rules = DesignRules::default();
    world_features(doc, lib, &netlist, &rules, su)
}

/// The membership netlist `world_features` needs, rebuilt from `doc.nets` (the
/// crate-internal `route::doc_netlist` is not public). Roles are irrelevant to the
/// geometry producer, so every member is `Passive` — matching what the internal
/// bridge does.
fn doc_netlist(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
    doc.nets
        .iter()
        .map(|(nid, net)| {
            (
                nid.clone(),
                net.members
                    .iter()
                    .map(|pr| (pr.clone(), PinRole::Passive))
                    .collect(),
            )
        })
        .collect()
}

/// Content bounds in nm `(x0, y0, x1, y1)` **with the 2 mm margin** — the exact box
/// `svg.rs` computes, so the canvas viewBox matches `board.svg`. Covers the board
/// corners plus only the feature roles `svg.rs` puts in view; falls back to a 10 mm
/// box for an empty document so the viewBox is never degenerate.
///
/// **Role filter (must track `svg.rs`'s bounds loop, `export/svg.rs`):** `svg.rs`
/// gathers component origins + pin pads + footprint `def.graphics` + board corners +
/// traces + vias + `Role::Marking` silk — i.e. copper and silk only. It never puts
/// `Role::Datum` fab geometry/text, `Substrate`, `Mask`, `Void`, or `Keepout` extents
/// in the frame. Those same points arrive here through `world_features` as
/// `Conductor` (pads/traces/vias/pours) and `Marking` (footprint silk + board text),
/// so we include exactly those two roles. Including `Datum` here framed the poc board
/// tiny and off-centre — its F.Fab text runs to x≈[-17.4..73.4] mm, ~35 mm wider than
/// the board — so the filter is load-bearing, not cosmetic.
fn content_bounds(doc: &Doc, features: &[NetFeature]) -> (Nm, Nm, Nm, Nm) {
    let mut pts: Vec<Point> = Vec::new();
    if let Some(region) = ecad_core::elaborate::board_region(&doc.source)
        && let Some((min, max)) = region.bbox()
    {
        pts.push(min);
        pts.push(max);
    }
    for nf in features {
        // Match svg.rs's in-view set: copper (pads/traces/vias/pours) and silk only.
        if !matches!(nf.feature.role, Role::Conductor | Role::Marking) {
            continue;
        }
        let Extent::Prism { shape, .. } = &nf.feature.extent;
        pts.extend(shape.points());
    }
    let (mut x0, mut y0, mut x1, mut y1) = match pts.first() {
        Some(p) => (p.x, p.y, p.x, p.y),
        None => (0, 0, 10 * MM, 10 * MM),
    };
    for p in &pts {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    (x0 - MARGIN, y0 - MARGIN, x1 + MARGIN, y1 + MARGIN)
}

/// The stackup slab whose z-range **contains the midpoint** of a feature's z. A
/// forward query (identity flows from the stackup; z is never reconstructed
/// heuristically): the feature's z was assigned from a slab at lowering, so its
/// midpoint lies inside that slab. Midpoint (not overlap) disambiguates the shared
/// faces between contiguous slabs. `None` if no slab spans it.
fn slab_of_z<'a>(su: &'a Stackup, z: &ZRange) -> Option<&'a ecad_core::geom::Slab> {
    let mid = z.lo + (z.hi - z.lo) / 2;
    // A zero-thickness feature (fab datum slabs are lo == hi) matches the slab whose
    // range touches that plane; prefer strict containment, fall back to touching.
    su.slabs
        .iter()
        .find(|s| s.z.lo <= mid && mid < s.z.hi)
        .or_else(|| su.slabs.iter().find(|s| s.z.lo <= mid && mid <= s.z.hi))
}

/// The synthetic layer name drill / hole `Void`s are collected under. A full-stack
/// through-cut spans every slab, so it belongs to no single one; this names the
/// dedicated drills layer.
fn drill_slab_name(_su: &Stackup) -> String {
    "Drills".to_string()
}

/// A filled path for a copper / mask shape at full opacity.
fn fill_shape(shape: &Shape2D, flip_sum: Nm, color: Color) -> VectorPath {
    fill_shape_opacity(shape, flip_sum, color, 1.0)
}

/// A filled even-odd path for `shape`, at `opacity`. Every shape kind is realised
/// as its filled [`Region`] via the same `shape_to_region` kernel `svg.rs` uses for
/// pours, so a Stroke (disc / capsule / trace) fills to its honest copper extent and
/// an Area (pour with holes) keeps its knockouts. **Even-odd** matches `svg.rs`'s
/// `fill-rule="evenodd"` on the same oriented rings, so counters / knockouts read as
/// voids.
fn fill_shape_opacity(shape: &Shape2D, flip_sum: Nm, color: Color, opacity: f32) -> VectorPath {
    let region = match shape {
        Shape2D::Area { region } => region.clone(),
        _ => shape_to_region(shape, DEFAULT_CIRCLE_SEGS),
    };
    region_fill_path(&region, flip_sum, color, opacity)
}

/// Emit a [`Region`]'s rings as one even-odd filled [`VectorPath`]: each ring an
/// `M … L … Z` subpath (the twin of `svg_writer::region_svg_d`). Rings with fewer
/// than three vertices are skipped, as in the SVG writer.
fn region_fill_path(region: &Region, flip_sum: Nm, color: Color, opacity: f32) -> VectorPath {
    let mut b = PathBuilder::new();
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        for (i, p) in ring.iter().enumerate() {
            let (x, y) = board_to_view(*p, flip_sum);
            b = if i == 0 {
                b.move_to(x, y)
            } else {
                b.line_to(x, y)
            };
        }
        b = b.close();
    }
    b.fill(Some(VectorFill {
        color: VectorColor::Solid(color),
        opacity,
        rule: VectorFillRule::EvenOdd,
    }))
    .build()
}

/// A [`Region`] as an **unfilled stroked** outline (each ring closed), `width` in
/// mm — the board-outline / hole look (`svg.rs`'s `fill="none" stroke=…`).
fn region_stroke_path(region: &Region, flip_sum: Nm, color: Color, width_mm: f32) -> VectorPath {
    let mut b = PathBuilder::new();
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        for (i, p) in ring.iter().enumerate() {
            let (x, y) = board_to_view(*p, flip_sum);
            b = if i == 0 {
                b.move_to(x, y)
            } else {
                b.line_to(x, y)
            };
        }
        b = b.close();
    }
    b.stroke_solid(color, width_mm).build()
}

/// Silk / fab markings, mirroring `svg.rs`'s `svg_surface` shape arm: a `Stroke`
/// (`fp_line` / `fp_arc` / stroke-font text) becomes a stroked centreline polyline
/// whose pen is the shape's inflation diameter (`radius * 2`); a `Polygon`
/// (`fp_poly` / `fp_rect`) or `Area` (TTF outline text) becomes a filled area.
fn marking_paths(shape: &Shape2D, flip_sum: Nm, color: Color) -> Vec<VectorPath> {
    match shape {
        Shape2D::Stroke { .. } => {
            let width_mm = (nm_to_mm(shape.radius() * 2)).max(MIN_STROKE_MM);
            let pts = shape.points();
            let mut b = PathBuilder::new();
            for (i, p) in pts.iter().enumerate() {
                let (x, y) = board_to_view(*p, flip_sum);
                b = if i == 0 {
                    b.move_to(x, y)
                } else {
                    b.line_to(x, y)
                };
            }
            vec![
                b.stroke_solid(color, width_mm)
                    .stroke_line_cap(damascene_core::vector::VectorLineCap::Round)
                    .stroke_line_join(damascene_core::vector::VectorLineJoin::Round)
                    .build(),
            ]
        }
        Shape2D::Polygon { .. } => vec![fill_shape(shape, flip_sum, color)],
        Shape2D::Area { region } => vec![region_fill_path(region, flip_sum, color, 1.0)],
    }
}

// ----------------------------------------------------------------------------
// Palette (dark-canvas ECAD conventions). Per-net colours are a later ticket.
// ----------------------------------------------------------------------------

// Layer palette. These are a *domain* palette — physical PCB layer colours (warm
// copper, off-white silk, green mask, amber fab) — not the UI theme's palette, so
// each carries a stable `ecad.layer.*` token name via [`Color::srgb_token`]. The
// token both documents intent and satisfies the bundle lint's "no raw colour" rule
// (the same rule the SVG backend's hardcoded `layer_color` would trip, were it
// linted). Per-net colours are a later ticket.

/// The board outline / edge colour: a pale off-white, like a fab silkscreen edge.
fn outline_color() -> Color {
    Color::srgb_token("ecad.layer.outline", 0xd8, 0xd8, 0xd8, 0xff)
}

/// Drill / hole colour: near-black, so a plated barrel reads as a punched void.
fn hole_color() -> Color {
    Color::srgb_token("ecad.layer.drill", 0x10, 0x10, 0x10, 0xff)
}

/// The default colour for a stackup slab, by role + copper side (top warm, bottom
/// cool, inner green — the same intent as `svg.rs`'s `layer_color`, tuned for a
/// dark canvas). Silk is off-white, mask a translucent green film, fab amber.
fn layer_color(su: &Stackup, name: &str) -> Color {
    let slab = su.slab(name);
    match slab.map(|s| &s.role) {
        Some(Role::Conductor) => copper_color(su, name),
        Some(Role::Marking) => Color::srgb_token("ecad.layer.silk", 0xe0, 0xe0, 0xe0, 0xff),
        Some(Role::Mask) => Color::srgb_token("ecad.layer.mask", 0x1f, 0x6f, 0x43, 0xff),
        Some(Role::Datum) => Color::srgb_token("ecad.layer.fab", 0xc8, 0x8a, 0x2c, 0xff),
        Some(Role::Substrate) => Color::srgb_token("ecad.layer.substrate", 0x2a, 0x2a, 0x2a, 0xff),
        _ => Color::srgb_token("ecad.layer.other", 0x88, 0x88, 0x88, 0xff),
    }
}

/// Top outer copper: a warm red. Bottom: a cool blue. Inner: green. (The same
/// intent as `svg.rs`'s `layer_color` — warm top / cool bottom / green inner —
/// tuned for a dark canvas.) Named so tests can assert the palette stably.
fn copper_color_top() -> Color {
    Color::srgb_token("ecad.layer.cu.top", 0xd6, 0x3a, 0x3a, 0xff)
}
fn copper_color_bottom() -> Color {
    Color::srgb_token("ecad.layer.cu.bottom", 0x3a, 0x7a, 0xd6, 0xff)
}
fn copper_color_inner() -> Color {
    Color::srgb_token("ecad.layer.cu.inner", 0x3a, 0xb0, 0x55, 0xff)
}

/// A copper slab's colour by which outer side it sits on (a forward stackup query,
/// like `svg.rs`'s `copper_side` / `layer_color`). Unknown names fall back to top.
fn copper_color(su: &Stackup, name: &str) -> Color {
    let cu = su.copper_slabs();
    let n = cu.len();
    match cu.iter().position(|s| s.name == name) {
        Some(0) => copper_color_top(),
        Some(i) if i + 1 == n => copper_color_bottom(),
        Some(_) => copper_color_inner(),
        None => copper_color_top(),
    }
}

/// The human label for a layer in the panel.
fn layer_display_name(id: &LayerId) -> String {
    match id {
        LayerId::Outline => "Board outline".to_string(),
        LayerId::Slab(name) => name.clone(),
    }
}

pub mod pick;

#[cfg(test)]
mod tests;
