---
id: d06
title: "Orientation is an exact integer quaternion (no mirror flag)"
date: 2026-06-30
status: implemented (Stages 1+1b+2, 2026-06-30 — commits `3ec4fa6`/`3f60b5d`/`92d6e2a`; angle precision `ORIENT_ANGLE_SCALE = 1e6`; bottom-flip convention fixed to Ry(180) 2026-07-03, branch feat/flip-axis — see [n04](n04-convergence-open-items.md))
---

> Context: restated in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) ("Bottom-side convention") and `eutectic-core/src/doc.rs` (`Orient`).
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 6 — orientation is an exact integer quaternion (no mirror flag)

Authoritative orientation = an **integer quaternion** `q = (w, x, y, z): i64`
(`doc::Orient`), the 3D-general form of the rotation. `apply` is

```
apply(p) = M(q) · p / |q|²      where |q|² = w²+x²+y²+z², M(q) integer
```

— an integer matrix·point then **one integer rounding division** (round-half-away):
**no `sin`/`cos`, no `sqrt`**, deterministic across libms, and exact when `|q|²`
divides cleanly. This refines the original "2D direction vector": a quaternion is its
honest 3D generalisation (a planar rotation about z is `(w,0,0,z)`; an off-axis tilt is
any `(w,x,y,z)`) and gives an even cleaner `apply` (no `sqrt` at all). It was chosen
over a stored angle because deriving a rotation from an angle needs `cos`/`sin`
(not IEEE-correctly-rounded — the `hypot` trap); the quaternion stores exact integer
data that *defines* the rotation, deriving the irrational matrix correctly-rounded.

- **No mirror flag.** Bottom-side placement is a *rotation* (a 180° flip about an
  in-plane axis, determinant +1), fully a quaternion — `q` with an x/y component. The
  mirrored *appearance* is a property of the 2D top-view **projection**, not the stored
  transform. "Which side" is **derived** (`Orient::is_bottom` — the sign of where local
  `+z` maps), and a flipped component's pad layers swap Top↔Bottom from that, with no
  bool to keep in sync.
- **Cardinals/flips are exact**, tiny quaternions (`|q|²` ∈ {1, 2}); the existing
  exact-position tests hold unchanged.
- **Arbitrary planar angle**: `30°` lowers to the best integer planar quaternion
  `(w,0,0,z)` with `(w²−z²):2wz ≈ cos:sin` — a one-time rational approximation at
  authoring/parse time (never re-derived at elaboration, so no `cos`/`sin` determinism
  hole). **Authoring intent** ("ring of N, facing outward") lowers to N concrete
  quaternions; the materialised placements are exact-as-stored. (Stage 2.)
- **V1 (Stage 1)** constructs only the 8 board-plane-preserving orientations (4
  about-z × top/flip), all exact; `apply` runs on planar `z = 0` points. Off-axis tilt
  + `Point3D` + 3D solving stay reserved (Decision 3).
