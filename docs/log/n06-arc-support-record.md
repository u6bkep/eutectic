---
id: n06
title: "Arc support — `Shape2D` carries circular arcs (5 stages) — staged build record"
date: 2026-06-30
status: historical record (done; the `Path`/`Seg` representation and strategy A are current)
---

> Context: the shape vocabulary lives in [architecture.md §8](../architecture.md#8-geometry-purposed-regions-the-physical-model) and `eutectic-core/src/geom/seg.rs`.
> Moved verbatim from `docs/architecture.md` on 2026-07-11.

### Arc support — `Shape2D` carries circular arcs (5 stages, done) — historical record

The `Shape2D` skeleton became a `Path { start, segs: Vec<Seg> }` where `Seg = Line | Arc{mid,end}` —
a **3-point** circular arc (three lattice points: no over-determination, centre/radius derived as
exact rationals at export). The enum is open so a `Cubic` Bézier slots in later as one tessellation
arm + export arms, with no kernel churn (non-circular curves are roadmapped for MCAD, deferred). The
design choice — **"strategy A"**: arcs are authoritative, the exact-integer clearance/boolean kernel
never sees a curve (it consumes a transient flattening at one seam, `Path::flatten`), and export
reads arcs directly — keeps the proven integer kernel untouched and the door open to an arc-exact
kernel later. Tessellation is trig-free (perpendicular-bisector bisection, correctly-rounded `sqrt`
only; turn-sign-aware so a ≥180° sub-arc tessellates the intended side). Stages: (1) representation +
kernel seam; (2) tolerance policy + clearance/region regressions (flattening is *inscribed* ⇒ DRC
optimistic by ≤ one sagitta, ~1µm); (3) export — `G02`/`G03` + `G75` (I/J from the exact-rational
circumcentre, computed start-relative so it can't overflow far from origin) and SVG `A` arcs (flags
exact-integer), straight shapes byte-identical; (4-text) `arc <mid> <end>` in the text grammar, so an
authored half-disc board flows author → DRC → fab end-to-end; (4-import) KiCad `custom` pads import as
compound copper including `gr_arc` (3-point **and** legacy centre/angle), validated against real
footprints (MCP_48QFN: 144 arcs). Two adversarial reviews caught real bugs (≥180° wrong-side
tessellation; a `hypot` determinism leak; the far-from-origin overflow margin) — all fixed with
regressions. Deferred follow-ups: footprint *graphics* import (0016), a `.kicad_pcb` Edge.Cuts
importer (0017), and the `Cubic`/NURBS curve primitive for MCAD bodies.
