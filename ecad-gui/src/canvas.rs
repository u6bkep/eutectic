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

    /// The laid-out rect of the board's vector-asset El inside a pane's viewport —
    /// the `el_rect` the `content_px` ↔ `board_mm` mappings below expect.
    ///
    /// Inside a damascene `viewport()`, a `vector(asset)` child with no explicit
    /// size is laid out at its **natural** size — one viewBox unit (mm) per
    /// logical px — anchored at the viewport's inner top-left. So the asset's
    /// stretch rect is `(rect.x, rect.y, vw, vh)`, NOT the viewport's own rect
    /// (under it the per-axis scale `rw/vw` is exactly 1). Passing the viewport
    /// rect instead — the m2/m3 composition — was self-consistent (both pick
    /// directions shared the wrong scale) but mismatched the real painted
    /// geometry; the m6 drag tests, which drive the composition against the real
    /// laid-out UI state, exposed it. Verified against the laid-out child rect in
    /// the settled harness (inverse-transform of `rect_of_key("layer:…")` equals
    /// `(rect.x, rect.y, vw, vh)` bit-for-bit at the probed zoom).
    pub fn content_rect(&self, viewport_rect: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        let [_, _, vw, vh] = self.view_box();
        (viewport_rect.0, viewport_rect.1, vw, vh)
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

    /// Map a board-mm point to the viewport **content-space** point (logical px,
    /// pre-transform) — the exact inverse of [`content_px_to_board_mm`](Self::content_px_to_board_mm),
    /// which a `ViewportRequest::CenterOn` consumes. Applies the y-flip (board→viewBox
    /// mm), then the viewBox→rect scale + offset (`sx = rect.w/vw`, `sy = rect.h/vh`),
    /// so the result is in the same frame the picker's `unproject` produces. `None` for
    /// a degenerate rect. Used by findings click-to-centre.
    pub fn board_mm_to_content_px(
        &self,
        board_mm: (f32, f32),
        el_rect: (f32, f32, f32, f32),
    ) -> Option<(f32, f32)> {
        let (rx, ry, rw, rh) = el_rect;
        let [vx, vy, vw, vh] = self.view_box();
        if rw <= 0.0 || rh <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return None;
        }
        // board → viewBox mm (flip is its own inverse: view_y = flip_sum - board_y).
        let (_, y0, _, y1) = self.bounds;
        let flip_sum_mm = nm_to_mm(y0 + y1);
        let view_mm = (board_mm.0, flip_sum_mm - board_mm.1);
        // viewBox mm → content px (undo the min offset + the per-axis rect scale).
        let sx = rw / vw;
        let sy = rh / vh;
        Some((rx + (view_mm.0 - vx) * sx, ry + (view_mm.1 - vy) * sy))
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
        // unfilled **dashed** stroke in the Edge colour (the UI oracle's board-edge
        // treatment — `#eab308`, dashed). Board-space dashes, so they scale with the
        // rest of the geometry (this is a cached static layer, built without a zoom).
        if let Some(region) = ecad_core::elaborate::board_region(&doc.source) {
            let path = dashed_region_stroke_path(
                &region,
                flip_sum,
                outline_color(),
                EDGE_STROKE_MM,
                EDGE_DASH_MM,
                EDGE_GAP_MM,
            );
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

        // Findings markers: a distinct violation ring at each finding's board point,
        // error-red or warning-amber — visually distinct from the cyan selection halo.
        for (p, is_error) in &overlay.findings {
            let color = if *is_error {
                finding_error_color()
            } else {
                finding_warning_color()
            };
            paths.push(finding_marker_path(*p, flip_sum, color));
        }

        // Drag ghost (m6): the dragged component's pad shapes at the uncommitted
        // position, as outline halos in the ghost accent — clearly "preview, not
        // copper". Rendered before the ratsnest so the lines read on top.
        for shape in &overlay.ghost {
            paths.push(shape_halo_path(shape, flip_sum, ghost_color()));
        }

        // Live ratsnest (m6): a thin straight line from each ghost pad to the
        // nearest other member pad of its net.
        for (a, b) in &overlay.ratsnest {
            let (ax, ay) = board_to_view(*a, flip_sum);
            let (bx, by) = board_to_view(*b, flip_sum);
            paths.push(
                PathBuilder::new()
                    .move_to(ax, ay)
                    .line_to(bx, by)
                    .stroke_solid(ratsnest_color(), RATSNEST_STROKE_MM)
                    .stroke_line_cap(damascene_core::vector::VectorLineCap::Round)
                    .build(),
            );
        }

        // Pending route preview (m6 slice B): each layer run as a polyline at
        // the width the commit will use; the rubber segment thinner / dimmer.
        for (pts, width) in &overlay.route_runs {
            if pts.len() >= 2 {
                let w = nm_to_mm(*width).max(MIN_STROKE_MM);
                paths.push(polyline_path(pts, flip_sum, route_color(), w));
            }
        }
        if let Some((a, b)) = overlay.route_rubber {
            paths.push(polyline_path(
                &[a, b],
                flip_sum,
                route_rubber_color(),
                OVERLAY_STROKE_MM,
            ));
        }
        // Layer-switch via previews: a ring at each drop, sized to the via pad.
        for (p, pad) in &overlay.route_vias {
            paths.push(ring_path(
                *p,
                flip_sum,
                (nm_to_mm(*pad) / 2.0).max(MIN_STROKE_MM),
                route_color(),
            ));
        }

        // Trace-path edit preview (vertex drag): the working path in the ghost
        // accent — same "preview, not copper" convention as the drag ghost.
        if let Some((pts, width)) = &overlay.edit_path
            && pts.len() >= 2
        {
            let w = nm_to_mm(*width).max(MIN_STROKE_MM);
            paths.push(polyline_path(pts, flip_sum, ghost_color(), w));
        }

        // Vertex handles of the selected trace: a small filled square per vertex.
        for h in &overlay.handles {
            paths.push(handle_path(*h, flip_sum));
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

    /// The pane's **visible window in view-mm** (the shared viewBox frame, y
    /// already flipped): the viewport rect's corners unprojected through the
    /// live camera, then mapped from content px to viewBox mm. Because the
    /// vector children are laid out at natural size anchored at the viewport's
    /// inner top-left (see [`content_rect`](Self::content_rect)), content px
    /// and view mm differ only by the rect origin and the viewBox min — no
    /// scale. This is what [`grid_el`](Self::grid_el) must cover.
    pub fn visible_view_mm(
        &self,
        viewport_rect: (f32, f32, f32, f32),
        vv: damascene_core::viewport::ViewportView,
    ) -> (f32, f32, f32, f32) {
        let (rx, ry, rw, rh) = viewport_rect;
        let [vx, vy, ..] = self.view_box();
        let origin = (rx, ry);
        let a = vv.unproject((rx, ry), origin);
        let b = vv.unproject((rx + rw, ry + rh), origin);
        let to_mm = |p: (f32, f32)| (vx + p.0 - rx, vy + p.1 - ry);
        let (a, b) = (to_mm(a), to_mm(b));
        (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }

    /// The **canvas furniture** layer (UI oracle): a subtle background dot grid and
    /// the origin axes, drawn UNDER every board layer. Shares the layers' viewBox so
    /// it registers pixel-exact, and is `Painted` like the layers. `None` for a
    /// degenerate pitch/bounds.
    ///
    /// # Zoom-aware pitch (the adaptive rule)
    ///
    /// The grid pitch adapts so the on-screen dot spacing stays legible: [`grid_pitch_mm`]
    /// picks the smallest `1 / 2 / 5 × 10ⁿ` mm value whose screen spacing
    /// (`pitch_mm · zoom`, since 1 mm = 1 px at zoom 1) is at least [`GRID_MIN_PX`]. That
    /// keeps spacing in `[GRID_MIN_PX, GRID_MIN_PX · 2.5)` ≈ `[8, 20)` px — comfortably
    /// inside the ~8–40 px target. Dot and axis stroke sizes are a fixed fraction of the
    /// pitch, so the tessellated window depends only on the pitch **bucket** and the
    /// lattice index window — never the continuous zoom or pan.
    ///
    /// # Coverage: a viewport-anchored window (grid is effectively unbounded)
    ///
    /// Dots are laid at board multiples of the pitch (so a dot lands on the origin,
    /// where the axes cross), but only across a **window** of the infinite lattice
    /// that covers `visible_view_mm` — the pane's live visible rect — inflated by
    /// half a window per side ([`GRID_WINDOW_MARGIN`]). The user can never out-pan
    /// or out-zoom the grid: whatever rect the camera exposes, the window is
    /// re-anchored to it. The origin axes span the same window, so they cover at
    /// least the visible rect whenever they are in view. `visible` is `None` on the
    /// first frame (no laid-out rect yet); the fallback window is the content
    /// bounds inflated by [`GRID_OVERSCAN`], corrected one frame later.
    ///
    /// # Cost model (derived-state discipline)
    ///
    /// The pitch rule bounds the on-screen dot spacing below by [`GRID_MIN_PX`], so
    /// the **visible** lattice is at most `(pane_w / 8 px) × (pane_h / 8 px)` dots
    /// at any zoom, and the built window at most `(1 + 2·margin)² = 4×` that.
    /// Per build: a **cache hit** — pitch, viewBox, and a visible window still
    /// inside `cache`'s built window — clones the cached asset (the unchanged
    /// `content_hash` dedupes the GPU upload, so re-emitting is free past the
    /// first frame); a miss (pan escaped the margin, pitch bucket changed, or a
    /// reload moved the viewBox) re-tessellates O(window) = O(visible dots) once.
    /// Worst-case per-frame cost is therefore O(visible dots), typical cost an
    /// asset clone — and the old per-build O(board-extent dots) rebuild is gone.
    pub fn grid_el(
        &self,
        zoom: f32,
        visible: Option<(f32, f32, f32, f32)>,
        cache: &mut Option<GridCache>,
    ) -> Option<El> {
        let pitch = grid_pitch_mm(zoom);
        // `grid_pitch_mm` returns a positive finite pitch (guarded zoom → step·decade);
        // this is a defensive floor, not a reachable branch on a committed doc.
        if pitch <= 0.0 {
            return None;
        }
        let view_box = self.view_box();
        let (_, y0, _, y1) = self.bounds;
        let flip_sum_mm = nm_to_mm(y0 + y1);

        // The view-mm rect the grid must cover: the live visible window, or the
        // overscanned content bounds before the first layout.
        let (wx0, wy0, wx1, wy1) = visible.unwrap_or_else(|| {
            let [vx, vy, vw, vh] = view_box;
            let (ox, oy) = (vw * GRID_OVERSCAN, vh * GRID_OVERSCAN);
            (vx - ox, vy - oy, vx + vw + ox, vy + vh + oy)
        });
        if !(wx0.is_finite() && wy0.is_finite() && wx1.is_finite() && wy1.is_finite()) {
            return None;
        }

        // The visible lattice-index window: dots sit at view x = i·pitch and
        // view y = flip_sum − j·pitch (board multiples — a dot on the origin).
        let (vi0, vi1) = (
            grid_index(wx0 / pitch, false),
            grid_index(wx1 / pitch, true),
        );
        let (vj0, vj1) = (
            grid_index((flip_sum_mm - wy1) / pitch, false),
            grid_index((flip_sum_mm - wy0) / pitch, true),
        );

        // Cache hit: same pitch bucket + viewBox, and the visible window still
        // inside the built window → the clone is the whole per-frame cost.
        let hit = cache.as_ref().is_some_and(|c| {
            c.pitch == pitch
                && c.view_box == view_box
                && c.window.0 <= vi0
                && vi1 <= c.window.1
                && c.window.2 <= vj0
                && vj1 <= c.window.3
        });
        if !hit {
            // Rebuild: the visible window inflated by half a window per side
            // (the pan hysteresis), spans defensively clamped so a pathological
            // rect can never explode the tessellation.
            let mi = (((vi1 - vi0) as f32 * GRID_WINDOW_MARGIN).ceil() as i64).max(1);
            let mj = (((vj1 - vj0) as f32 * GRID_WINDOW_MARGIN).ceil() as i64).max(1);
            let window = clamp_window((vi0 - mi, vi1 + mi, vj0 - mj, vj1 + mj));
            let asset = self.build_grid_asset(pitch, flip_sum_mm, window);
            *cache = Some(GridCache {
                pitch,
                view_box,
                window,
                asset,
            });
        }
        let asset = cache.as_ref().expect("filled above").asset.clone();
        Some(
            vector(asset)
                .vector_render_mode(VectorRenderMode::Painted)
                .key("grid:static"),
        )
    }

    /// Tessellate one grid window: the dot field (one filled path — a small
    /// square per lattice point) plus the origin axes where they cross it.
    /// O(window) — called only on a [`GridCache`] miss.
    fn build_grid_asset(
        &self,
        pitch: f32,
        flip_sum_mm: f32,
        window: (i64, i64, i64, i64),
    ) -> VectorAsset {
        let (i0, i1, j0, j1) = window;
        let mut paths: Vec<VectorPath> = Vec::new();

        let r = (pitch * GRID_DOT_FRAC).max(GRID_DOT_MIN_MM);
        let mut dots = PathBuilder::new();
        for i in i0..=i1 {
            let vx = i as f32 * pitch; // board x == view x
            for j in j0..=j1 {
                let vy = flip_sum_mm - j as f32 * pitch; // board y → view y (flip)
                dots = dots
                    .move_to(vx - r, vy - r)
                    .line_to(vx + r, vy - r)
                    .line_to(vx + r, vy + r)
                    .line_to(vx - r, vy + r)
                    .close();
            }
        }
        paths.push(
            dots.fill(Some(VectorFill {
                color: VectorColor::Solid(grid_dot_color()),
                opacity: 1.0,
                rule: VectorFillRule::NonZero,
            }))
            .build(),
        );

        // Origin axes: board x = 0 (view x = 0) and board y = 0 (view y =
        // flip_sum_mm), each spanning the window, in the faint accent. Only
        // drawn when the axis crosses the window.
        let axis_w = (pitch * GRID_AXIS_FRAC).max(MIN_STROKE_MM);
        let (gx0, gx1) = (i0 as f32 * pitch, i1 as f32 * pitch);
        let (gy_top, gy_bot) = (
            flip_sum_mm - j1 as f32 * pitch,
            flip_sum_mm - j0 as f32 * pitch,
        );
        if i0 <= 0 && 0 <= i1 {
            paths.push(
                PathBuilder::new()
                    .move_to(0.0, gy_top)
                    .line_to(0.0, gy_bot)
                    .stroke_solid(grid_axis_color(), axis_w)
                    .build(),
            );
        }
        if j0 <= 0 && 0 <= j1 {
            paths.push(
                PathBuilder::new()
                    .move_to(gx0, flip_sum_mm)
                    .line_to(gx1, flip_sum_mm)
                    .stroke_solid(grid_axis_color(), axis_w)
                    .build(),
            );
        }
        VectorAsset::from_paths(self.view_box(), paths)
    }
}

/// One cached grid window (see [`Canvas::grid_el`]): the tessellated asset plus
/// the exact inputs it was built from, so a build can decide hit/miss with three
/// cheap comparisons. Owned per pane by the app (`EcadApp::grid_caches`) — the
/// two panes have independent cameras, hence independent windows.
#[derive(Clone, Debug)]
pub struct GridCache {
    /// The pitch bucket (mm) the window was tessellated at.
    pitch: f32,
    /// The shared viewBox at build time — a doc reload can move it, which
    /// re-anchors the flip and must invalidate the window.
    view_box: [f32; 4],
    /// The built lattice-index window `(i0, i1, j0, j1)`, inclusive.
    window: (i64, i64, i64, i64),
    /// The tessellated dot field + axes for that window.
    asset: VectorAsset,
}

/// A lattice index from a fractional coordinate (floor / ceil), saturated into
/// the clampable range so a huge-but-finite window can't overflow.
fn grid_index(v: f32, up: bool) -> i64 {
    let v = if up { v.ceil() } else { v.floor() };
    v.clamp(-1e15, 1e15) as i64
}

/// Clamp a window's per-axis span to [`GRID_MAX_SPAN`] indices around its
/// center — a pure defensive bound (a real pane at the minimum 8 px spacing
/// needs ~pane_px/8 ≈ hundreds per axis; this only bites on garbage rects).
fn clamp_window(w: (i64, i64, i64, i64)) -> (i64, i64, i64, i64) {
    let clamp_axis = |lo: i64, hi: i64| {
        if hi - lo <= GRID_MAX_SPAN {
            return (lo, hi);
        }
        let mid = lo + (hi - lo) / 2;
        (mid - GRID_MAX_SPAN / 2, mid + GRID_MAX_SPAN / 2)
    };
    let (i0, i1) = clamp_axis(w.0, w.1);
    let (j0, j1) = clamp_axis(w.2, w.3);
    (i0, i1, j0, j1)
}

/// Defensive per-axis cap on a grid window's index span (≈ an 8k-px pane at
/// the tightest 8 px spacing, doubled by the margin).
const GRID_MAX_SPAN: i64 = 2048;

/// How far past the visible window the built window extends, as a fraction of
/// the visible span per side — the pan hysteresis: a pan must cross half a
/// viewport before the grid re-tessellates.
const GRID_WINDOW_MARGIN: f32 = 0.5;

/// Smallest on-screen dot spacing (logical px) the adaptive grid pitch targets — the
/// lower bound of the ~8–40 px band the UI oracle's grid lives in.
const GRID_MIN_PX: f32 = 8.0;

/// The **pre-layout fallback** window: how far past the content bounds the dot
/// field extends, as a fraction of the content extent per side, on the one frame
/// where no laid-out pane rect exists yet (so [`Canvas::grid_el`] has no visible
/// window to anchor to). From the second frame on the window tracks the camera.
const GRID_OVERSCAN: f32 = 0.5;

/// Grid-dot half-side as a fraction of the pitch. On-screen dot side is
/// `2 · frac · spacing_px`; with spacing ∈ [8, 20) px this keeps the dot ≈ 1–2.4 px —
/// visible but subtle furniture.
const GRID_DOT_FRAC: f32 = 0.06;

/// Floor on the dot half-side in mm, so a dot never tessellates to nothing.
const GRID_DOT_MIN_MM: f32 = 0.01;

/// Origin-axis stroke width as a fraction of the pitch (a hair heavier than a dot).
const GRID_AXIS_FRAC: f32 = 0.06;

/// The board-edge (Edge layer) dash geometry, in board mm: stroke width, dash-on
/// length, gap-off length. Board-space so the dashes scale with the board.
const EDGE_STROKE_MM: f32 = 0.12;
const EDGE_DASH_MM: f32 = 0.8;
const EDGE_GAP_MM: f32 = 0.5;

/// The adaptive grid pitch in board mm for a viewport `zoom`: the smallest
/// `1 / 2 / 5 × 10ⁿ` value whose on-screen spacing (`pitch · zoom`, since 1 mm = 1 px at
/// zoom 1) is at least [`GRID_MIN_PX`]. Because the 1→2→5→10 ratios never exceed 2.5, the
/// resulting spacing lands in `[GRID_MIN_PX, GRID_MIN_PX · 2.5)` px. A non-finite or
/// non-positive zoom falls back to 1.0 (matching the pick-tolerance guard).
pub(crate) fn grid_pitch_mm(zoom: f32) -> f32 {
    let z = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };
    let ideal = GRID_MIN_PX / z; // smallest pitch (mm) that yields ≥ GRID_MIN_PX on screen
    let decade = 10f32.powf(ideal.log10().floor());
    let n = ideal / decade; // in [1, 10)
    let step = if n <= 1.0 {
        1.0
    } else if n <= 2.0 {
        2.0
    } else if n <= 5.0 {
        5.0
    } else {
        10.0
    };
    step * decade
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
    /// DRC / findings markers: a board point (world nm, y-up) + whether it is an error
    /// (vs a warning), drawn as a distinct violation ring — visually separate from the
    /// selection halo (a filled-colour crosshair ring, not a shape-tracing stroke).
    pub findings: Vec<(Point, bool)>,
    /// The in-flight drag ghost (m6): the dragged component's pad shapes translated to
    /// the uncommitted position (world nm, y-up). Empty when no drag is in progress.
    pub ghost: Vec<Shape2D>,
    /// The live ratsnest during a drag (m6): straight segments from each ghost pad to
    /// the nearest other member pad of its net (world nm, y-up).
    pub ratsnest: Vec<(Point, Point)>,
    /// The pending route's committed-to polylines (m6 slice B): one entry per
    /// layer run — `(points, trace width nm)` — drawn at the width the commit
    /// will use, in the route-preview accent.
    pub route_runs: Vec<(Vec<Point>, Nm)>,
    /// The pending route's rubber segment: last waypoint → last known pointer
    /// position (sparse on 0.4.5 free-hover). Drawn thinner / dimmer than the
    /// committed-to runs.
    pub route_rubber: Option<(Point, Point)>,
    /// Via previews for the pending route's layer switches: `(centre, pad
    /// diameter nm)`, drawn as rings in the route accent.
    pub route_vias: Vec<(Point, Nm)>,
    /// The uncommitted trace-path edit preview (m6 slice B vertex drag):
    /// `(working path, width nm)`, drawn in the ghost accent ("preview, not
    /// copper" — same convention as the component drag ghost).
    pub edit_path: Option<(Vec<Point>, Nm)>,
    /// Vertex handles of the selected trace (Select tool): one marker per path
    /// vertex, so the refinement affordance is visible.
    pub handles: Vec<Point>,
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

/// A findings marker: a small ring centred on the finding's board point, drawn as a
/// stroked circle so it reads as a "look here" target distinct from the selection halo
/// (which traces the feature's outline). Radius is a fixed board-mm so it scales with
/// zoom like every overlay stroke.
fn finding_marker_path(p: Point, flip_sum: Nm, color: Color) -> VectorPath {
    ring_path(p, flip_sum, FINDING_MARKER_R_MM, color)
}

/// A stroked ring of radius `r_mm` centred on board point `p` — the marker
/// primitive shared by the findings halo and the route-preview via drop.
/// Approximated with a closed 24-gon (the overlay has no arc builder; it reads
/// as a circle at any practical zoom); stroked, not filled, so the copper
/// beneath still reads.
fn ring_path(p: Point, flip_sum: Nm, r_mm: f32, color: Color) -> VectorPath {
    let (cx, cy) = board_to_view(p, flip_sum);
    let mut b = PathBuilder::new();
    let segs = 24;
    for i in 0..segs {
        let a = std::f32::consts::TAU * (i as f32) / (segs as f32);
        let (x, y) = (cx + r_mm * a.cos(), cy + r_mm * a.sin());
        b = if i == 0 {
            b.move_to(x, y)
        } else {
            b.line_to(x, y)
        };
    }
    b.close().stroke_solid(color, OVERLAY_STROKE_MM).build()
}

/// A stroked polyline through board points (round caps/joins) — the route-preview
/// primitive. `width_mm` is the on-board stroke width (scales with zoom).
fn polyline_path(pts: &[Point], flip_sum: Nm, color: Color, width_mm: f32) -> VectorPath {
    let mut b = PathBuilder::new();
    for (i, p) in pts.iter().enumerate() {
        let (x, y) = board_to_view(*p, flip_sum);
        b = if i == 0 {
            b.move_to(x, y)
        } else {
            b.line_to(x, y)
        };
    }
    b.stroke_solid(color, width_mm)
        .stroke_line_cap(damascene_core::vector::VectorLineCap::Round)
        .stroke_line_join(damascene_core::vector::VectorLineJoin::Round)
        .build()
}

/// A vertex handle: a small filled square centred on the vertex, in the handle
/// accent — visually distinct from both the copper and the selection halo.
fn handle_path(p: Point, flip_sum: Nm) -> VectorPath {
    let (cx, cy) = board_to_view(p, flip_sum);
    let h = HANDLE_HALF_MM;
    PathBuilder::new()
        .move_to(cx - h, cy - h)
        .line_to(cx + h, cy - h)
        .line_to(cx + h, cy + h)
        .line_to(cx - h, cy + h)
        .close()
        .fill(Some(VectorFill {
            color: VectorColor::Solid(handle_color()),
            opacity: 1.0,
            rule: VectorFillRule::EvenOdd,
        }))
        .build()
}

/// Vertex-handle half-side in board mm.
const HANDLE_HALF_MM: f32 = 0.3;

/// Findings marker ring radius in board mm.
const FINDING_MARKER_R_MM: f32 = 1.2;

/// The error-finding marker colour — a saturated red ring, distinct from every layer
/// palette colour and from the cyan selection halo.
fn finding_error_color() -> Color {
    Color::srgb_token("ecad.finding.error", 0xff, 0x45, 0x45, 0xff)
}

/// The warning-finding marker colour — a warm amber ring (matches the DRC-warn chip).
fn finding_warning_color() -> Color {
    Color::srgb_token("ecad.finding.warning", 0xf5, 0xc0, 0x24, 0xff)
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

/// The drag-ghost accent — a violet outline, distinct from selection cyan, measure
/// amber, and every finding/layer colour: reads as "uncommitted preview".
fn ghost_color() -> Color {
    Color::srgb_token("ecad.overlay.ghost", 0xb8, 0x8c, 0xff, 0xdd)
}

/// The live-ratsnest line — a pale desaturated yellow (the classic airwire look),
/// translucent so it never obscures copper.
fn ratsnest_color() -> Color {
    Color::srgb_token("ecad.overlay.ratsnest", 0xe8, 0xe4, 0xa0, 0xbb)
}

/// The route-preview accent — a bright green, distinct from selection cyan,
/// measure amber, ghost violet, and every layer colour: reads as "pending copper".
fn route_color() -> Color {
    Color::srgb_token("ecad.overlay.route", 0x53, 0xdd, 0x6c, 0xee)
}

/// The route rubber segment — the route accent dimmed (uncommitted-to end).
fn route_rubber_color() -> Color {
    Color::srgb_token("ecad.overlay.route.rubber", 0x53, 0xdd, 0x6c, 0x77)
}

/// The vertex-handle fill — near-white, so handles pop over any copper colour.
fn handle_color() -> Color {
    Color::srgb_token("ecad.overlay.handle", 0xf2, 0xf5, 0xff, 0xee)
}

/// Ratsnest stroke width in mm — thinner than the overlay halos (an airwire is a
/// cue, not geometry).
const RATSNEST_STROKE_MM: f32 = 0.08;

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
/// bridge does. Shared by the canvas renderer and the picker (`canvas::pick`) so the
/// two consume one netlist.
pub(crate) fn doc_netlist(doc: &Doc) -> BTreeMap<NetId, Vec<(PinRef, PinRole)>> {
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
/// faces between contiguous slabs. `None` if no slab spans it. Shared by the canvas
/// renderer and the picker (`canvas::pick`) so a feature bins onto the same layer in
/// both.
pub(crate) fn slab_of_z<'a>(su: &'a Stackup, z: &ZRange) -> Option<&'a ecad_core::geom::Slab> {
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

/// A [`Region`] as an **unfilled dashed** stroked outline: each ring is walked
/// edge-by-edge, and the on-mm segments of a `dash_mm`-on / `gap_mm`-off cycle are
/// emitted as separate subpaths (damascene strokes have no native dash array, so the
/// dashes are geometry). The dash phase carries across a ring's edges and its closing
/// edge, so the pattern is continuous around corners. Dashes are in board mm (this is
/// a cached static layer with no zoom in hand), so they scale with the board like
/// every other feature. Rings with fewer than three vertices are skipped.
fn dashed_region_stroke_path(
    region: &Region,
    flip_sum: Nm,
    color: Color,
    width_mm: f32,
    dash_mm: f32,
    gap_mm: f32,
) -> VectorPath {
    let mut b = PathBuilder::new();
    let period = (dash_mm + gap_mm).max(1e-4);
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        // Distance into the current dash cycle (0..period); < dash_mm is "pen down".
        let mut phase = 0.0_f32;
        let n = ring.len();
        for k in 0..n {
            let a = board_to_view(ring[k], flip_sum);
            let c = board_to_view(ring[(k + 1) % n], flip_sum);
            let (dx, dy) = (c.0 - a.0, c.1 - a.1);
            let seg_len = dx.hypot(dy);
            if seg_len <= 1e-6 {
                continue;
            }
            let (ux, uy) = (dx / seg_len, dy / seg_len);
            // March along the segment, cutting the dash pattern into it.
            let mut pos = 0.0_f32;
            while pos < seg_len {
                let in_dash = phase < dash_mm;
                let remaining = if in_dash {
                    dash_mm - phase
                } else {
                    period - phase
                };
                let step = remaining.min(seg_len - pos);
                if in_dash {
                    let (sx, sy) = (a.0 + ux * pos, a.1 + uy * pos);
                    let (ex, ey) = (a.0 + ux * (pos + step), a.1 + uy * (pos + step));
                    b = b.move_to(sx, sy).line_to(ex, ey);
                }
                pos += step;
                phase = (phase + step) % period;
            }
        }
    }
    b.stroke_solid(color, width_mm)
        .stroke_line_cap(damascene_core::vector::VectorLineCap::Butt)
        .build()
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

/// The board outline / edge colour: the UI oracle's Edge token (`#eab308`) — the
/// same amber that strokes the board outline and swatches the Edge layer.
fn outline_color() -> Color {
    Color::srgb_token("ecad.layer.edge", 0xea, 0xb3, 0x08, 0xff)
}

/// Drill / hole colour: near-black, so a plated barrel reads as a punched void.
fn hole_color() -> Color {
    Color::srgb_token("ecad.layer.drill", 0x10, 0x10, 0x10, 0xff)
}

/// The background dot-grid colour — a dim near-bg grey (`#28282e`), so the grid reads
/// as furniture under the copper, never competing with it.
fn grid_dot_color() -> Color {
    Color::srgb_token("ecad.grid.dot", 0x28, 0x28, 0x2e, 0xff)
}

/// The origin-axis colour — the UI accent (`#3b82f6`) at low alpha, a faint hint of
/// where the board origin sits without drawing the eye.
fn grid_axis_color() -> Color {
    Color::srgb_token("ecad.grid.axis", 0x3b, 0x82, 0xf6, 0x55)
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
