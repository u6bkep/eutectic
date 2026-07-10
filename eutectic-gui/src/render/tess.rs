//! Polygon-interior tessellation (renderer-spec §3): earcut-class CPU
//! triangulation of [`PrimShape::Polygon`](super::scene::PrimShape::Polygon)
//! ring sets — pours with knockouts, glyphs with counters, rectangular pads.
//!
//! Backed by `lyon_tessellation` (the spec's default candidate): it consumes
//! multiple closed subpaths with an **even-odd** fill rule, which matches how
//! the region kernel's oriented rings have always been filled downstream
//! (`svg.rs`'s `fill-rule="evenodd"`, the old canvas's `VectorFillRule::EvenOdd`)
//! — holes need no special casing, and mildly degenerate input (touching
//! rings from the boolean kernel) is handled robustly. Plain earcut crates
//! were rejected for exactly that: they need explicit hole wiring and choke
//! on touching rings.
//!
//! Rings arrive already flattened at the kernel's fixed nm tolerance; only
//! `line_to` edges reach lyon, so its curve tolerance is irrelevant.
//! Positions are **anchor-relative f32 nm** — the §7 precision rule; the
//! anchor offset was removed in exact integer math first.

use eutectic_core::coord::Point;
use lyon_tessellation::geom::point;
use lyon_tessellation::path::Path as LyonPath;
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillRule, FillTessellator, FillVertex, VertexBuffers,
};

/// A triangulated polygon interior: positions (anchor-relative f32 nm) plus
/// a triangle-list index buffer.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PolyMesh {
    pub positions: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

impl PolyMesh {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

/// Triangulate a ring set (CCW islands / CW holes, or in fact any winding —
/// even-odd) into a triangle list, positions relative to `anchor`.
/// Deterministic for equal input. Degenerate input (all rings < 3 points,
/// zero area) yields an empty mesh; an internal tessellator error is
/// reported as an empty mesh too (a missing pour renders loudly wrong in the
/// goldens rather than crashing the pane).
pub fn triangulate(rings: &[Vec<Point>], anchor: Point) -> PolyMesh {
    let mut builder = LyonPath::builder();
    let mut any = false;
    for ring in rings {
        if ring.len() < 3 {
            continue;
        }
        any = true;
        let rel = |p: &Point| point((p.x - anchor.x) as f32, (p.y - anchor.y) as f32);
        builder.begin(rel(&ring[0]));
        for p in &ring[1..] {
            builder.line_to(rel(p));
        }
        builder.end(true);
    }
    if !any {
        return PolyMesh::default();
    }
    let path = builder.build();
    let mut buffers: VertexBuffers<[f32; 2], u32> = VertexBuffers::new();
    let mut tess = FillTessellator::new();
    let r = tess.tessellate_path(
        &path,
        &FillOptions::default().with_fill_rule(FillRule::EvenOdd),
        &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| {
            let p = v.position();
            [p.x, p.y]
        }),
    );
    if r.is_err() {
        log::warn!("polygon tessellation failed: {r:?}");
        return PolyMesh::default();
    }
    PolyMesh {
        positions: buffers.vertices,
        indices: buffers.indices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eutectic_core::coord::MM;

    fn pt(x: i64, y: i64) -> Point {
        Point { x, y }
    }

    /// Total unsigned area of a mesh's triangles (f64 to avoid f32 sums).
    fn mesh_area(m: &PolyMesh) -> f64 {
        m.indices
            .chunks(3)
            .map(|t| {
                let [a, b, c] = [
                    m.positions[t[0] as usize],
                    m.positions[t[1] as usize],
                    m.positions[t[2] as usize],
                ];
                (((b[0] - a[0]) as f64) * ((c[1] - a[1]) as f64)
                    - ((b[1] - a[1]) as f64) * ((c[0] - a[0]) as f64))
                    .abs()
                    / 2.0
            })
            .sum()
    }

    /// Is `p` strictly inside any triangle of the mesh?
    fn covered(m: &PolyMesh, p: [f32; 2]) -> bool {
        m.indices.chunks(3).any(|t| {
            let [a, b, c] = [
                m.positions[t[0] as usize],
                m.positions[t[1] as usize],
                m.positions[t[2] as usize],
            ];
            let s = |p0: [f32; 2], p1: [f32; 2]| {
                (p1[0] - p0[0]) * (p[1] - p0[1]) - (p1[1] - p0[1]) * (p[0] - p0[0])
            };
            let (d0, d1, d2) = (s(a, b), s(b, c), s(c, a));
            (d0 > 0.0 && d1 > 0.0 && d2 > 0.0) || (d0 < 0.0 && d1 < 0.0 && d2 < 0.0)
        })
    }

    #[test]
    fn square_with_hole_keeps_the_hole() {
        // 10 mm square (CCW) with a 4 mm square hole (CW), anchored at its
        // center so positions are small.
        let outer = vec![
            pt(0, 0),
            pt(10 * MM, 0),
            pt(10 * MM, 10 * MM),
            pt(0, 10 * MM),
        ];
        let hole = vec![
            pt(3 * MM, 3 * MM),
            pt(3 * MM, 7 * MM),
            pt(7 * MM, 7 * MM),
            pt(7 * MM, 3 * MM),
        ];
        let anchor = pt(5 * MM, 5 * MM);
        let mesh = triangulate(&[outer, hole], anchor);
        assert!(!mesh.is_empty());
        let want = (10.0 * 10.0 - 4.0 * 4.0) * (MM as f64) * (MM as f64);
        let got = mesh_area(&mesh);
        assert!(
            (got - want).abs() / want < 1e-4,
            "area {got} vs expected {want}"
        );
        // The hole centroid (= anchor ⇒ rel (0,0)) is uncovered; a point in
        // the solid rim is covered.
        assert!(!covered(&mesh, [0.0, 0.0]), "hole must stay open");
        assert!(covered(&mesh, [(-4 * MM) as f32, 0.0]), "rim must fill");
    }

    #[test]
    fn winding_is_irrelevant_under_even_odd() {
        // Same geometry, hole wound CCW like the outer ring: even-odd still
        // keeps it open (the kernel usually orients holes CW, but glyph
        // sources vary).
        let outer = vec![pt(0, 0), pt(4 * MM, 0), pt(4 * MM, 4 * MM), pt(0, 4 * MM)];
        let hole = vec![
            pt(MM, MM),
            pt(3 * MM, MM),
            pt(3 * MM, 3 * MM),
            pt(MM, 3 * MM),
        ];
        let mesh = triangulate(&[outer, hole], pt(2 * MM, 2 * MM));
        assert!(!covered(&mesh, [0.0, 0.0]));
        let want = (16.0 - 4.0) * (MM as f64) * (MM as f64);
        assert!((mesh_area(&mesh) - want).abs() / want < 1e-4);
    }

    #[test]
    fn degenerate_rings_yield_empty_mesh() {
        assert!(triangulate(&[], pt(0, 0)).is_empty());
        assert!(triangulate(&[vec![pt(0, 0), pt(MM, MM)]], pt(0, 0)).is_empty());
    }

    #[test]
    fn deterministic_across_runs() {
        let outer = vec![pt(0, 0), pt(5 * MM, 0), pt(6 * MM, 4 * MM), pt(-MM, 5 * MM)];
        let a = triangulate(std::slice::from_ref(&outer), pt(0, 0));
        let b = triangulate(&[outer], pt(0, 0));
        assert_eq!(a, b);
    }
}
