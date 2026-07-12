---
id: d11
title: "Content-addressed library references + instantiations; never a bare path, never expanded geometry"
date: 2026-06-30
status: adopted (model decided, mechanism deferred; the name-keyed reference half is live as library packages — [architecture.md §9](../architecture.md#9-library-packages-parts-as-data-names-as-the-dependency-key); content-hash pinning not yet built)
---

> Context: [architecture.md §9](../architecture.md#9-library-packages-parts-as-data-names-as-the-dependency-key) (library packages). Includes sub-decision 11a.
> Moved verbatim from the retired decision record (`docs/geometry-model-convergence.md`) on 2026-07-11.

### Decision 11 — content-addressed library references + instantiations; never a bare path, never expanded geometry

A part is **referenced, not inlined as geometry**. The authoritative storage is a
small reference plus instantiations; geometry is the tracked fold of (resolved source
→ `Feature`s), per Decision 5.

- **`LibraryRef`** = an abstract handle (`library_id : part_name`) **plus a content
  hash** of the source. *Not* a filesystem path. The hash is what the geometry fold
  keys on, so the cache is correct by construction (source changes → hash changes →
  re-fold).
- **Library table** resolves `library_id` → a location (vendored blob, CAS cache, or
  an fs path for local dev). This is the path-abstraction — KiCad's lib-table
  indirection — with the content pin added.
- **Vendored content-addressed store**: the resolved source is vendored into the
  project (or a CAS cache keyed by hash) and committed alongside, so the document is
  **self-contained and reproducible** while storage stays tiny.
- **Instantiation** = `(LibraryRef, transform, overrides)`, where `transform` is the
  Decision-6 exact transform and `overrides` reuse the existing tier-1 provenance
  ladder (a changed value or a moved silk label is the *same kind of thing* as a
  placement override).

What is stored alongside a ref is only what **overrides or selects** — transform,
per-instance overrides, the symbol↔footprint join, the content hash. **Never** the
expanded geometry; that is always the derived fold.

**Why not a bare path.** Everything in this system is built for deterministic,
diffable, reproducible. A raw fs path is a reference into the *environment*, not the
document: the same file folds differently (or fails) on another machine or next year,
and a board diff could hide an invisible library edit. That is the single biggest hole
we could punch in the reproducibility thesis — and it is *the* perennial ECAD pain
("missing footprint", "which library version?"). The fix is the Cargo/Nix pattern:
name it in the ref, pin it by content hash, vendor the content. This is the synthesis
of by-reference (small, single-source-of-truth, dependency-tracked fold) and inlining
(self-contained, reproducible — which is why modern KiCad embeds footprints).

### Decision 11a — the reference is source-agnostic; a native part type is coming

`LibraryRef` points to *some* resolvable source folded to `Feature`s — it does not
care whether that source is a KiCad sexp (today) or a **native component type**
(later, using the *same serialization PCBs use* — just defining pins, pads, graphics,
courtyard, text). Both import paths fold identically and both get cargo-style pinning.

This deliberately opens the door to a **cargo-for-ecad** dependency resolve/fetch
ecosystem — a direct answer to the KiCad library-repo problem that is a chronic sharp
edge in this space (unpinned, environment-dependent, hard to reproduce). Content
hashing now is what makes that future coherent rather than bolted-on.

**Scope:** the *model* (content-addressed ref + vendored source + instantiations) is
decided. The *mechanism* (lockfile format, network fetch, a real resolver) is
deferred — V1 can be vendored files resolved by a trivial table; the hash buys
correctness now and the upgrade path to fetch later. We do not build a package manager
to commit to the model.
