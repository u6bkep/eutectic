//! Board outline importer: a `.kicad_pcb`'s `Edge.Cuts` graphics → `(outline, cutouts)`.
//!
//! A `.kicad_pcb` is one big S-expression `(kicad_pcb …)`, so we reuse the
//! tokenizer/reader/`Sexp` machinery ([`super::sexp`]) and the `gr_*` point readers
//! ([`super::footprint`]). This importer lifts **only the board boundary**: the
//! top-level `gr_line` / `gr_arc` / `gr_circle` graphics on the `Edge.Cuts` layer,
//! stitched into closed loops and classified into an outline + cutouts.
//!
//! **Scope.** Outline + cutouts only. Placed footprints, their positions/rotations,
//! nets, tracks, zones, and vias are *not* imported — that is the larger
//! board-round-trip feature (see issue 0017) and is deliberately out of scope here.
//!
//! Coordinates are mm in the file → integer nm via [`mm_to_nm`], matching the
//! fixed-point invariant. Disjoint edges are chained by matching endpoints within a
//! tiny [`TOUCH_TOL`] slack (KiCad coordinates are exact nm, but the slack tolerates
//! any rounding); each closed loop becomes a [`Shape2D::Polygon`](crate::geom::Shape2D) whose edges are
//! `Seg::Line`/`Seg::Arc`. The loop of largest area is the `outline`; the rest are
//! `cutouts`.

use crate::doc::{Nm, Point};
use crate::geom::{self, Seg};

use super::footprint::{dist_nm, gr_arc_points, prim_xy};
use super::sexp::{Sexp, read, tokenize};

/// Endpoint-match slack for stitching `Edge.Cuts` segments into loops, in nm (1 µm).
/// KiCad writes exact nm so consecutive edges normally share an endpoint exactly;
/// this only absorbs sub-µm rounding noise.
const TOUCH_TOL: Nm = 1_000;

/// One `Edge.Cuts` graphic as an undirected edge: endpoints `a`/`b` plus, for an arc,
/// the on-curve `mid` point. Emitted as a [`Seg`] in whichever direction the stitch
/// walks it (an arc's `mid` stays on the curve when reversed).
struct EdgeSeg {
    a: Point,
    b: Point,
    mid: Option<Point>,
}

impl EdgeSeg {
    /// The [`Seg`] for walking this edge away from endpoint `from` (`~a` ⇒ ends at
    /// `b`, else ends at `a`); also returns the far endpoint reached.
    fn seg_from(&self, from: Point) -> (Seg, Point) {
        let end = if near(from, self.a) { self.b } else { self.a };
        match self.mid {
            Some(mid) => (Seg::Arc { mid, end }, end),
            None => (Seg::Line { end }, end),
        }
    }
}

/// Are two points within [`TOUCH_TOL`] of each other (squared, exact i128)?
fn near(p: Point, q: Point) -> bool {
    let (dx, dy) = ((p.x - q.x) as i128, (p.y - q.y) as i128);
    dx * dx + dy * dy <= (TOUCH_TOL as i128) * (TOUCH_TOL as i128)
}

/// Does a graphic item carry `(layer "Edge.Cuts")` (quoted or bare)?
fn on_edge_cuts(list: &[Sexp]) -> bool {
    list.iter()
        .find_map(|s| s.list_headed("layer"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        == Some("Edge.Cuts")
}

/// Import a `.kicad_pcb`'s board outline: parse the `Edge.Cuts` `gr_line`/`gr_arc`/
/// `gr_circle` graphics, stitch them into closed loops, and return the authored board
/// geometry as `(outline, cutouts)` — [`geom::Shape2D`]s that become `Board`/`Cutout`
/// directives (largest-area loop = `outline`, the rest = `cutouts`; arcs preserved).
/// The board's *derived* region (outline ∖ cutouts) is [`elaborate::board_region`].
///
/// **Only the board boundary is imported** — no placed footprints, nets, tracks or
/// zones (that full round-trip is a separate, larger feature; see issue 0017). Errors
/// if there is no `Edge.Cuts` geometry or if its segments do not close into a loop.
pub fn import_board_outline(text: &str) -> Result<(geom::Shape2D, Vec<geom::Shape2D>), String> {
    let toks = tokenize(text)?;
    let root = read(&toks)?;
    let items = root.as_list().ok_or("top-level expression is not a list")?;
    if items.first().and_then(Sexp::as_atom) != Some("kicad_pcb") {
        return Err(format!(
            "expected '(kicad_pcb …)', got {:?}",
            items.first().and_then(Sexp::as_atom)
        ));
    }

    // gr_line / gr_arc become open edges to be stitched; gr_circle is already a
    // closed loop and goes straight into the loop list.
    let mut edges: Vec<EdgeSeg> = Vec::new();
    let mut loops: Vec<geom::Path> = Vec::new();
    for item in items {
        let Some(list) = item.as_list() else { continue };
        let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
        if !matches!(head, "gr_line" | "gr_arc" | "gr_circle") || !on_edge_cuts(list) {
            continue;
        }
        match head {
            "gr_line" => {
                let a = prim_xy(list, "start")?.ok_or("gr_line missing (start …)")?;
                let b = prim_xy(list, "end")?.ok_or("gr_line missing (end …)")?;
                edges.push(EdgeSeg { a, b, mid: None });
            }
            "gr_arc" => {
                let (s, m, e) = gr_arc_points(list, Point { x: 0, y: 0 })?;
                edges.push(EdgeSeg {
                    a: s,
                    b: e,
                    mid: Some(m),
                });
            }
            "gr_circle" => loops.push(circle_loop(list)?),
            _ => unreachable!(),
        }
    }

    if edges.is_empty() && loops.is_empty() {
        return Err("no Edge.Cuts graphics found in board".into());
    }
    loops.extend(stitch_loops(edges)?);

    // Classify by area: the largest loop is the board outline, the rest are cutouts.
    // (For real boards the outline both has the largest area and contains the others.)
    let mut indexed: Vec<(i128, geom::Shape2D)> = loops
        .into_iter()
        .map(|path| {
            let shape = geom::Shape2D::polygon_path(path, 0);
            (loop_area(&shape), shape)
        })
        .collect();
    indexed.sort_by_key(|y| std::cmp::Reverse(y.0));
    let mut shapes = indexed.into_iter().map(|(_, s)| s);
    let outline = shapes
        .next()
        .ok_or("Edge.Cuts has no closed loop to use as the board outline")?;
    Ok((outline, shapes.collect()))
}

/// Convenience wrapper: read a `.kicad_pcb` file from disk and import its outline.
pub fn import_board_outline_file(
    path: &str,
) -> Result<(geom::Shape2D, Vec<geom::Shape2D>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    import_board_outline(&text)
}

/// A `gr_circle (center …)(end …)` → a closed two-semicircle-arc [`geom::Path`]. `end`
/// is a point on the circle, so the radius is `|center − end|`; we walk the circle via
/// the four axis points (cardinal), independent of where `end` sits.
fn circle_loop(list: &[Sexp]) -> Result<geom::Path, String> {
    let c = prim_xy(list, "center")?.ok_or("gr_circle missing (center …)")?;
    let e = prim_xy(list, "end")?.ok_or("gr_circle missing (end …)")?;
    let r = dist_nm(c, e);
    if r <= 0 {
        return Err("gr_circle has zero radius".into());
    }
    let right = Point { x: c.x + r, y: c.y };
    let top = Point { x: c.x, y: c.y + r };
    let left = Point { x: c.x - r, y: c.y };
    let bottom = Point { x: c.x, y: c.y - r };
    Ok(geom::Path {
        start: right,
        segs: vec![
            Seg::Arc {
                mid: top,
                end: left,
            },
            Seg::Arc {
                mid: bottom,
                end: right,
            },
        ],
    })
}

/// Chain undirected [`EdgeSeg`]s into closed loops by matching endpoints within
/// [`TOUCH_TOL`]. Greedy: take any unused edge as a loop seed, then keep appending the
/// edge touching the current open end (in either direction) until it returns to the
/// loop's start. Errors if an edge has no continuation (an open contour, which is not
/// a valid board boundary).
fn stitch_loops(mut edges: Vec<EdgeSeg>) -> Result<Vec<geom::Path>, String> {
    let mut loops = Vec::new();
    while let Some(first) = edges.pop() {
        let loop_start = first.a;
        let (seg0, mut cur) = first.seg_from(loop_start);
        let mut segs = vec![seg0];
        while !near(cur, loop_start) {
            let Some(idx) = edges.iter().position(|e| near(e.a, cur) || near(e.b, cur)) else {
                return Err("Edge.Cuts segments do not form a closed loop (open contour)".into());
            };
            let e = edges.remove(idx);
            let (seg, next) = e.seg_from(cur);
            segs.push(seg);
            cur = next;
        }
        // The loop closes back at `loop_start`. A closing straight edge is the
        // polygon's *implicit* final `Line`, so drop it to avoid a redundant repeated
        // vertex (keep a closing `Arc` — it carries real curvature the implicit line
        // can't). Guard so we never collapse below a triangle.
        if segs.len() >= 3 && matches!(segs.last(), Some(Seg::Line { .. })) {
            segs.pop();
        }
        loops.push(geom::Path {
            start: loop_start,
            segs,
        });
    }
    Ok(loops)
}

/// Signed area ×2 of a closed loop, via the shoelace formula over the polygon's
/// flattened skeleton (arcs subdivided to [`geom::DEFAULT_CHORD_TOL`]). Exact i128;
/// magnitude only is used (orientation is irrelevant to classification).
///
/// Shared with the SVG outline importer ([`crate::svg_import`]), which classifies its
/// closed subpaths the same way.
pub(crate) fn loop_area(shape: &geom::Shape2D) -> i128 {
    let pts = shape.path().flatten(geom::DEFAULT_CHORD_TOL);
    let n = pts.len();
    let mut a2: i128 = 0;
    for i in 0..n {
        let p = pts[i];
        let q = pts[(i + 1) % n];
        a2 += p.x as i128 * q.y as i128 - q.x as i128 * p.y as i128;
    }
    a2.abs()
}
