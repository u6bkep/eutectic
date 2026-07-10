//! Instance building (renderer-spec §3): scene primitives → the GPU-ready
//! per-plane data — one [`InstanceRaw`] per analytic primitive (capsule /
//! disc / arc stroke) and one batched triangle mesh (from
//! [`tess`](super::tess)) for polygon interiors. Pure CPU, unit-testable;
//! the buffer objects themselves live in [`gpu`](super::gpu).
//!
//! Positions are **anchor-relative f32 nm** (§7): the anchor offset is
//! removed in integer / f64 math before the cast, so a board a metre from
//! the origin costs no precision.

use super::scene::{Plane, Prim, PrimShape, StyleClass};
use super::tess;
use eutectic_core::coord::Point;

/// Instance `kind` values (bits 0–7 of [`InstanceRaw::kind_style`]).
pub const KIND_CAPSULE: u32 = 0;
pub const KIND_DISC: u32 = 1;
pub const KIND_ARC: u32 = 2;

/// Style bits (bits 8–15 of `kind_style`): `0` = fill, `1 + pattern` = dash
/// with that pattern id (indexes the frame uniform's dash table).
const STYLE_SHIFT: u32 = 8;

/// One analytic-primitive instance, 40 bytes, shared by the static per-plane
/// buffers and the dynamic overlay buffer (same schema by design — spec §3).
///
/// Field use by kind:
/// - `KIND_CAPSULE`: `a`, `b` = endpoints; `params = [r, 0, len0, 0]`.
/// - `KIND_DISC`:    `a` = center;         `params = [r, 0, 0, 0]`.
/// - `KIND_ARC`:     `a` = center, `b` = `[a0, a1]` (radians, y-up, signed
///   sweep `a1 − a0`); `params = [radius, half_width, len0, 0]`.
///
/// Lengths (`r`, `len0`, …) are f32 **nm**; the shader scales by the frame's
/// px/nm.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct InstanceRaw {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub params: [f32; 4],
    pub kind_style: u32,
    pub sem: u32,
}

/// One polygon-mesh vertex: anchor-relative position + the semantic id
/// (per-vertex so one buffer batches every polygon of a plane).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MeshVertex {
    pub pos: [f32; 2],
    pub sem: u32,
}

/// A plane's CPU-side GPU data.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaneData {
    pub instances: Vec<InstanceRaw>,
    pub mesh_vertices: Vec<MeshVertex>,
    pub mesh_indices: Vec<u32>,
}

impl PlaneData {
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty() && self.mesh_indices.is_empty()
    }
}

/// Lower one plane's primitives. Deterministic; `TextRun`s contribute
/// nothing in WP1 (annotation text is the schematic slice's MSDF pass — the
/// board producer emits none).
pub fn build_plane_data(plane: &Plane, anchor: Point) -> PlaneData {
    build_prim_data(&plane.prims, anchor)
}

/// Lower a primitive list (shared by static planes and the dynamic overlay).
pub fn build_prim_data(prims: &[Prim], anchor: Point) -> PlaneData {
    let mut out = PlaneData::default();
    for prim in prims {
        let style = match prim.class {
            StyleClass::Fill => 0u32,
            StyleClass::Dash(p) => 1 + p as u32,
        } << STYLE_SHIFT;
        let rel = |p: Point| [(p.x - anchor.x) as f32, (p.y - anchor.y) as f32];
        match &prim.shape {
            PrimShape::Capsule { a, b, r } => out.instances.push(InstanceRaw {
                a: rel(*a),
                b: rel(*b),
                params: [*r as f32, 0.0, prim.len0 as f32, 0.0],
                kind_style: KIND_CAPSULE | style,
                sem: prim.sem,
            }),
            PrimShape::Disc { c, r } => out.instances.push(InstanceRaw {
                a: rel(*c),
                b: rel(*c),
                params: [*r as f32, 0.0, prim.len0 as f32, 0.0],
                kind_style: KIND_DISC | style,
                sem: prim.sem,
            }),
            PrimShape::ArcStroke {
                center,
                radius,
                a0,
                a1,
                half_width,
            } => out.instances.push(InstanceRaw {
                a: [
                    (center[0] - anchor.x as f64) as f32,
                    (center[1] - anchor.y as f64) as f32,
                ],
                b: [*a0 as f32, *a1 as f32],
                params: [*radius as f32, *half_width as f32, prim.len0 as f32, 0.0],
                kind_style: KIND_ARC | style,
                sem: prim.sem,
            }),
            PrimShape::Polygon { rings } => {
                let mesh = tess::triangulate(rings, anchor);
                let base = out.mesh_vertices.len() as u32;
                out.mesh_vertices.extend(
                    mesh.positions
                        .iter()
                        .map(|&pos| MeshVertex { pos, sem: prim.sem }),
                );
                out.mesh_indices
                    .extend(mesh.indices.iter().map(|&i| base + i));
            }
            // WP1 renders no text (spec §6): board silk arrives as Polygon
            // glyphs; TextRun is the schematic slice's MSDF work (WP3).
            PrimShape::TextRun { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::scene::{Justify, PlaneKey};
    use eutectic_core::coord::MM;

    fn pt(x: i64, y: i64) -> Point {
        Point { x, y }
    }

    #[test]
    fn instances_are_anchor_relative_and_kind_tagged() {
        let anchor = pt(10 * MM, 10 * MM);
        let prims = vec![
            Prim::fill(
                3,
                PrimShape::Capsule {
                    a: pt(10 * MM, 10 * MM),
                    b: pt(11 * MM, 10 * MM),
                    r: 250_000,
                },
            ),
            Prim::fill(
                4,
                PrimShape::Disc {
                    c: pt(9 * MM, 10 * MM),
                    r: 300_000,
                },
            ),
        ];
        let d = build_prim_data(&prims, anchor);
        assert_eq!(d.instances.len(), 2);
        let cap = &d.instances[0];
        assert_eq!(cap.kind_style, KIND_CAPSULE);
        assert_eq!(cap.a, [0.0, 0.0]);
        assert_eq!(cap.b, [MM as f32, 0.0]);
        assert_eq!(cap.params[0], 250_000.0);
        assert_eq!(cap.sem, 3);
        let disc = &d.instances[1];
        assert_eq!(disc.kind_style, KIND_DISC);
        assert_eq!(disc.a, [-(MM as f32), 0.0]);
        assert_eq!(disc.sem, 4);
    }

    #[test]
    fn dash_class_and_len0_reach_the_instance() {
        let prims = vec![Prim {
            sem: 1,
            class: StyleClass::Dash(2),
            len0: 1234.5,
            shape: PrimShape::Capsule {
                a: pt(0, 0),
                b: pt(MM, 0),
                r: 100_000,
            },
        }];
        let d = build_prim_data(&prims, pt(0, 0));
        let i = &d.instances[0];
        assert_eq!(i.kind_style, KIND_CAPSULE | ((1 + 2) << STYLE_SHIFT));
        assert_eq!(i.params[2], 1234.5);
    }

    #[test]
    fn polygons_batch_into_one_mesh_with_per_vertex_sems() {
        let sq = |o: i64| vec![pt(o, 0), pt(o + MM, 0), pt(o + MM, MM), pt(o, MM)];
        let prims = vec![
            Prim::fill(5, PrimShape::Polygon { rings: vec![sq(0)] }),
            Prim::fill(
                6,
                PrimShape::Polygon {
                    rings: vec![sq(2 * MM)],
                },
            ),
        ];
        let d = build_prim_data(&prims, pt(0, 0));
        assert!(d.instances.is_empty());
        assert_eq!(d.mesh_indices.len() % 3, 0);
        assert!(d.mesh_vertices.iter().any(|v| v.sem == 5));
        assert!(d.mesh_vertices.iter().any(|v| v.sem == 6));
        // Indices of the second polygon were rebased past the first's
        // vertices: every index is in range.
        assert!(
            d.mesh_indices
                .iter()
                .all(|&i| (i as usize) < d.mesh_vertices.len())
        );
    }

    #[test]
    fn text_runs_build_nothing_in_wp1() {
        let prims = vec![Prim::fill(
            1,
            PrimShape::TextRun {
                pos: pt(0, 0),
                height: MM,
                justify: Justify::Left,
                content: "REF".into(),
            },
        )];
        let d = build_prim_data(&prims, pt(0, 0));
        assert!(d.is_empty());
    }

    #[test]
    fn plane_data_matches_prim_data() {
        let plane = Plane {
            key: PlaneKey::Drills,
            prims: vec![Prim::fill(
                0,
                PrimShape::Disc {
                    c: pt(MM, MM),
                    r: 200_000,
                },
            )],
        };
        assert_eq!(
            build_plane_data(&plane, pt(0, 0)),
            build_prim_data(&plane.prims, pt(0, 0))
        );
    }
}
