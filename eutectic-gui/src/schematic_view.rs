//! The schematic canvas: a thin projection from the core realized-geometry stream
//! ([`schematic_features`], Decision 23) to damascene [`VectorAsset`]s.
//!
//! This is the schematic twin of [`crate::canvas`]. Where the board canvas walks
//! `world_features`, the schematic canvas consumes `schematic_features` — the same
//! stream the SVG export serializes — so drawing conventions (stub lengths, text
//! heights, margins, the bounds math) live in core exactly once and this module holds
//! **no geometry of its own**: the static asset, the pick candidates, and the content
//! bounds are all folds over the stream and its provenance. What lives here is pure
//! realization — style class → color token, nm → viewBox mm, the y-flip, and text runs
//! → stroked glyphs.
//!
//! **Scheduled for DELETION with the viewport path** (owned-canvas pivot,
//! docs/gui-architecture.md "Canvas strategy"): the owned renderer will ingest the same
//! stream directly. This rewire makes the module thin, not permanent — do not grow it.
//!
//! # Text as stroked glyphs
//!
//! A viewport child is a vector asset in content space, so text can't be a damascene
//! `text()` El (which flows in chrome layout). The stream carries text as *runs*; this
//! projection realizes each run as stroked glyph polylines via
//! [`eutectic_core::font::text_strokes`] (the same public stroke font the board silk
//! uses), honoring the run's baseline anchor and justify. This differs from
//! `schematic_svg.rs`, which realizes the same runs as `<text>`.
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
use eutectic_core::coord::{MM, Nm, Point};
use eutectic_core::doc::Doc;
use eutectic_core::font::{Justify, text_strokes};
use eutectic_core::part::PartLib;
use eutectic_core::schematic::{
    Provenance, STUB_LEN, Shape, StyleClass, TextJustify, TextRun, schematic_features,
};
use std::collections::BTreeSet;

/// Text pen width (mm): a thin stroke so glyphs read as line art, not filled ink. A
/// realization parameter of *this* consumer (the stream carries runs, not pens).
const TEXT_PEN_MM: f32 = 0.12;
/// Overlay highlight stroke (mm) — matches the board overlay accent width.
const OVERLAY_STROKE_MM: f32 = 0.2;

/// The schematic projection held in app state: the shared content bounds (for coordinate
/// inversion + framing) and the cached static [`VectorAsset`] plus the pick candidates.
/// Built once per doc load by [`SchematicView::build`]; per frame only the asset is
/// cloned (`content_hash` dedupes the GPU upload).
#[derive(Clone, Debug)]
pub struct SchematicView {
    /// Content bounds in schematic nm `(x0, y0, x1, y1)` — the stream's shared
    /// [`Bounds`](eutectic_core::schematic::Bounds), margin included; the asset viewBox
    /// in mm (y already flipped to read upright).
    bounds: (Nm, Nm, Nm, Nm),
    /// The tessellated static drawing (boxes, stubs, wires, labels). Cloned per frame.
    asset: VectorAsset,
    /// Pickable candidates (pins ▸ wires ▸ symbol bodies), folded from the stream's
    /// provenance. The schematic hit-test input.
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
    /// Project `doc` into a schematic canvas: run [`schematic_features`] and fold the
    /// stream into the static drawing asset + pick candidates, holding the stream's
    /// content bounds. `None` when the doc has no components (an empty schematic — the
    /// caller shows an empty pane), so the viewBox is never degenerate. Never panics.
    pub fn build(doc: &Doc, lib: &PartLib) -> Option<SchematicView> {
        if doc.components.is_empty() {
            return None;
        }
        let fs = schematic_features(doc, lib);
        let bounds = (fs.bounds.x0, fs.bounds.y0, fs.bounds.x1, fs.bounds.y1);
        let (_, y0, _, y1) = bounds;
        let flip_sum = y0 + y1;

        // The stream's order is the draw order (wires under symbols, chrome last), so
        // one pass emits paths; candidates fold from provenance in the same pass.
        let mut paths: Vec<VectorPath> = Vec::new();
        let mut candidates: Vec<SchematicCandidate> = Vec::new();
        for f in &fs.features {
            let color = class_color(f.class);
            match &f.shape {
                Shape::Polyline { pts, width } => {
                    if pts.len() >= 2 {
                        paths.push(polyline_path(pts, flip_sum, color, nm_to_mm(*width)));
                    }
                }
                Shape::Polygon { pts, width } => {
                    if pts.len() >= 2 {
                        paths.push(closed_path(pts, flip_sum, color, nm_to_mm(*width)));
                    }
                }
                Shape::Disc { center, radius } => {
                    // Reserved (junction dots, gw-26): a zero-length round-cap stroke
                    // reads as a filled dot when something starts emitting discs.
                    paths.push(polyline_path(
                        &[*center, *center],
                        flip_sum,
                        color,
                        nm_to_mm(2 * *radius),
                    ));
                }
                Shape::Text(run) => paths.extend(text_paths(run, flip_sum, color)),
            }

            // Pick candidates, from provenance (commitment 2: hit-testing and rendering
            // derive from the same stream and cannot drift).
            match (&f.provenance, &f.class, &f.shape) {
                // Symbol body (priority 2 — least specific): the outline's bbox.
                (
                    Provenance::Component(id),
                    StyleClass::SymbolOutline,
                    Shape::Polygon { pts, .. },
                ) => {
                    if let Some(b) = bbox(pts) {
                        candidates.push(SchematicCandidate {
                            id: SemanticId::Part(id.clone()),
                            geom: b,
                            priority: 2,
                        });
                    }
                }
                // Pins (priority 0 — most specific): the stub tip, keyed by the stored
                // pin id (pad number — the `PinRef` join key `SemanticId::Pin` uses).
                (
                    Provenance::Pin { comp, pin },
                    StyleClass::PinStub,
                    Shape::Polyline { pts, .. },
                ) => {
                    if let Some(tip) = pts.last() {
                        candidates.push(SchematicCandidate {
                            id: SemanticId::Pin {
                                comp: comp.clone(),
                                pin: pin.clone(),
                            },
                            geom: PickGeom::Point(*tip),
                            priority: 0,
                        });
                    }
                }
                // Wires (priority 1) → net: a wire is presentational; its selectable
                // identity is the net it draws (the cross-view currency).
                (Provenance::Wire { net: Some(net), .. }, _, Shape::Polyline { pts, .. }) => {
                    candidates.push(SchematicCandidate {
                        id: SemanticId::Net(net.clone()),
                        geom: PickGeom::Poly(pts.clone()),
                        priority: 1,
                    });
                }
                _ => {}
            }
        }

        Some(SchematicView {
            bounds,
            asset: VectorAsset::from_paths(view_box(bounds), paths),
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
                    let corners = [
                        Point {
                            x: ctr.x - hw,
                            y: ctr.y - hh,
                        },
                        Point {
                            x: ctr.x + hw,
                            y: ctr.y - hh,
                        },
                        Point {
                            x: ctr.x + hw,
                            y: ctr.y + hh,
                        },
                        Point {
                            x: ctr.x - hw,
                            y: ctr.y + hh,
                        },
                    ];
                    paths.push(closed_path(
                        &corners,
                        flip_sum,
                        overlay_color(),
                        OVERLAY_STROKE_MM,
                    ));
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

/// The bbox of a polygon's points as a [`PickGeom::Box`] (centre + half-extents), or
/// `None` for an empty point list.
fn bbox(pts: &[Point]) -> Option<PickGeom> {
    let (min_x, max_x) = (
        pts.iter().map(|p| p.x).min()?,
        pts.iter().map(|p| p.x).max()?,
    );
    let (min_y, max_y) = (
        pts.iter().map(|p| p.y).min()?,
        pts.iter().map(|p| p.y).max()?,
    );
    Some(PickGeom::Box {
        c: Point {
            x: (min_x + max_x) / 2,
            y: (min_y + max_y) / 2,
        },
        hw: (max_x - min_x) / 2,
        hh: (max_y - min_y) / 2,
    })
}

// ----------------------------------------------------------------------------
// Bounds + coordinate mapping (schematic twin of canvas.rs).
// ----------------------------------------------------------------------------

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
// Path builders (stream shape → damascene path).
// ----------------------------------------------------------------------------

/// A stroked polyline in schematic space (stub / wire / divider / overlay).
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

/// An unfilled closed stroked outline (the symbol body polygon, the overlay halo).
fn closed_path(pts: &[Point], flip_sum: Nm, color: Color, width_mm: f32) -> VectorPath {
    let mut b = PathBuilder::new();
    for (i, p) in pts.iter().enumerate() {
        let (x, y) = to_view(*p, flip_sum);
        b = if i == 0 {
            b.move_to(x, y)
        } else {
            b.line_to(x, y)
        };
    }
    b.close().stroke_solid(color, width_mm).build()
}

/// Realize a stream [`TextRun`] as stroked-glyph polylines (the board-silk approach).
/// The run's `at` is its **baseline** anchor (y-up, matching the glyph frame of
/// [`text_strokes`]: baseline at local y=0, ascending +y), so glyphs place directly;
/// the justify shifts the run's ink horizontally (`End` runs end at the anchor).
fn text_paths(run: &TextRun, flip_sum: Nm, color: Color) -> Vec<VectorPath> {
    if run.text.is_empty() {
        return Vec::new();
    }
    let strokes = text_strokes(&run.text, run.height, Justify::Left);
    // Ink width for the justify shift: the run's x-extent in the local frame.
    let mut min_x = Nm::MAX;
    let mut max_x = Nm::MIN;
    for stroke in &strokes {
        for p in stroke {
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
        }
    }
    let width = if max_x >= min_x { max_x - min_x } else { 0 };
    let shift_x = match run.justify {
        TextJustify::Start => 0,
        TextJustify::End => -width,
    };
    let mut out = Vec::new();
    for stroke in &strokes {
        if stroke.is_empty() {
            continue;
        }
        let placed: Vec<Point> = stroke
            .iter()
            .map(|p| Point {
                x: run.at.x + p.x + shift_x,
                y: run.at.y + p.y,
            })
            .collect();
        out.push(polyline_path(&placed, flip_sum, color, TEXT_PEN_MM));
    }
    out
}

// ----------------------------------------------------------------------------
// Palette: style class → color token (the one style decision this consumer owns).
// ----------------------------------------------------------------------------

/// Map a stream style class to this canvas's color. The stream carries no colors
/// (Decision 23); this table is the GUI's whole schematic "theme".
fn class_color(class: StyleClass) -> Color {
    match class {
        StyleClass::SymbolOutline
        | StyleClass::PinStub
        | StyleClass::Header
        | StyleClass::PinName
        | StyleClass::NcMark => ink_color(),
        StyleClass::Wire => wire_color(),
        StyleClass::NetTag => tag_color(),
        StyleClass::BinDivider | StyleClass::BinLabel => chrome_color(),
    }
}

/// Symbol boxes, stubs, headers, pin names, nc marks — a light ink on the dark canvas.
fn ink_color() -> Color {
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

/// Non-semantic chrome (the unplaced-bin divider + label) — muted, like the SVG's #888.
fn chrome_color() -> Color {
    Color::srgb_token("eutectic.schematic.chrome", 0x88, 0x88, 0x88, 0xff)
}

/// Highlight overlay accent — the same bright cyan the board overlay uses (cross-view
/// consistency).
fn overlay_color() -> Color {
    Color::srgb_token("eutectic.overlay.select", 0x22, 0xd3, 0xee, 0xff)
}

#[cfg(test)]
mod tests;
