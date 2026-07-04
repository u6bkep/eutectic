//! The Excellon drill backend: the board's holes read **forward** from the unified
//! feature stream ([`drill_hits`]), split by plating into `board-PTH.drl` /
//! `board-NPTH.drl` ([`excellon_drill`] / [`excellon_files`], issue 0022 / Decision 16b)
//! and emitted as a deterministic program ([`excellon_program`]). Coordinates and tool
//! sizes are decimal millimetres via [`fmt_mm`], no float — byte-stable.

use crate::doc::{Nm, Point};
use crate::geom::{Extent, Role};
use crate::part::PartLib;
use std::collections::{BTreeMap, BTreeSet};

use super::netlist::doc_netlist;
use super::svg_writer::fmt_mm;

/// One drilled hole gathered from the unified feature stream: a round hole at a point,
/// or a slot between two points (a routed `G85` hole). `Ord` so a tool's hits emit in a
/// canonical order (byte-stable, diffable output).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DrillKind {
    Round(Point),
    Slot(Point, Point),
}

/// The board's drilled holes, read **forward** from the unified feature stream
/// ([`crate::route::world_features`]): every full-stackup through-cut `Role::Void`, as
/// `(plated, diameter, kind)`. Three producers reach here — a pad drill, a via drill, and
/// an authored `hole` NPTH (Decision 16b, full-z by construction). This is the fix for
/// issue 0022 — the drill file is a query over the same `Void` features the solder-mask
/// export sees, so pad drills (previously omitted), via drills, and mounting holes appear.
///
/// A mask opening is a *partial-z* `Void` (at the mask slab), and a `region void` is
/// single-slab (at its slab's z) — neither is a through-cut, so both are excluded by the
/// full-z gate. An authored `hole`, by contrast, IS full-z and admitted. A void's
/// **plating** is carried by its material (Decision 16b): pad/via drills are plated (a
/// copper barrel), a material-less void (the `hole`) is NPTH. A disc void is a `Round`
/// hit; a capsule (slot) void a `Slot`. Any other drill-void shape is an un-handled seam.
pub(crate) fn drill_hits(doc: &crate::doc::Doc, lib: &PartLib) -> Vec<(bool, Nm, DrillKind)> {
    let su = crate::elaborate::stackup(&doc.source);
    let full = su.full_z();
    // `world_features` cannot fail on a committed doc (the commit-time slab gate — see
    // `route::check_drc`); an `Err` is a broken invariant, made loud rather than emitting
    // an empty (⇒ no-holes) drill program for a board that never materialised.
    let world = crate::route::world_features(
        doc,
        lib,
        &doc_netlist(doc),
        &crate::route::DesignRules::default(),
        &su,
    )
    .expect("world_features on a committed doc (slab gate enforced at commit)");
    let mut hits = Vec::new();
    for nf in world {
        if nf.feature.role != Role::Void {
            continue;
        }
        let Extent::Prism { shape, z } = &nf.feature.extent;
        if Some(*z) != full {
            continue; // not a through-cut (mask opening / single-slab authored void)
        }
        // Plated iff the drill Void carries the copper-barrel material (Decision 16b): a
        // pad/via plated through-hole. Gated on the material *name*, not merely
        // `is_some()`, so a future void with some other material (e.g. a resin-filled or
        // capped via) is not silently classified PTH — authored voids default NPTH.
        let plated = nf
            .feature
            .material
            .as_ref()
            .is_some_and(|m| m.name == "copper");
        let dia = shape.radius() * 2;
        let pts = shape.points();
        let kind = match pts.as_slice() {
            [c] => DrillKind::Round(*c),
            [a, b] => DrillKind::Slot(*a, *b),
            // A drill Void is always a disc or capsule stroke; anything else is a shape
            // no drill-lowering produces today. Leave a loud seam rather than dead code.
            _ => unimplemented!("drill Void with a non-disc/capsule shape ({pts:?})"),
        };
        hits.push((plated, dia, kind));
    }
    hits
}

/// One Excellon drill program for a set of `hits` (all one plating class). Tools are the
/// distinct diameters, sorted and numbered `T1..`; under each tool its hits emit in
/// canonical order — round holes as a coordinate, slots as a `G85` routed hole. `label`
/// names the file's plating in the header comment. Coordinates and tool sizes are
/// decimal millimetres via [`fmt_mm`]. Deterministic.
pub(crate) fn excellon_program(hits: &[(Nm, DrillKind)], label: &str) -> String {
    let dias: BTreeSet<Nm> = hits.iter().map(|(d, _)| *d).collect();
    let tools: BTreeMap<Nm, u32> = dias
        .iter()
        .enumerate()
        .map(|(i, d)| (*d, 1 + i as u32))
        .collect();

    let mut out = String::new();
    out.push_str("M48\n");
    out.push_str(&format!("; Excellon drill: {label}\n"));
    out.push_str("FMAT,2\n");
    out.push_str("METRIC,TZ\n");
    for (d, t) in &tools {
        out.push_str(&format!("T{}C{}\n", t, fmt_mm(*d)));
    }
    out.push_str("%\n");
    for (d, t) in &tools {
        out.push_str(&format!("T{}\n", t));
        let mut kinds: Vec<DrillKind> = hits
            .iter()
            .filter(|(hd, _)| hd == d)
            .map(|(_, k)| *k)
            .collect();
        kinds.sort();
        for k in kinds {
            match k {
                DrillKind::Round(c) => {
                    out.push_str(&format!("X{}Y{}\n", fmt_mm(c.x), fmt_mm(c.y)));
                }
                // A slot is a routed hole: position at one end, then `G85` to the other.
                DrillKind::Slot(a, b) => {
                    out.push_str(&format!(
                        "X{}Y{}G85X{}Y{}\n",
                        fmt_mm(a.x),
                        fmt_mm(a.y),
                        fmt_mm(b.x),
                        fmt_mm(b.y)
                    ));
                }
            }
        }
    }
    out.push_str("T0\n");
    out.push_str("M30\n");
    out
}

/// The board's Excellon drill program(s), split by plating (issue 0022 / Decision 16b):
/// `board-PTH.drl` for plated through-holes (pad + via drills) and `board-NPTH.drl` for
/// non-plated holes. Each file is emitted only when it has holes, so a board with no
/// NPTH holes ships only the PTH file. `(filename, content)` pairs; deterministic.
pub fn excellon_drill(doc: &crate::doc::Doc, lib: &PartLib) -> Vec<(String, String)> {
    excellon_files(drill_hits(doc, lib))
}

/// Split a `(plated, diameter, kind)` hit list into the PTH / NPTH drill files, emitting
/// each only when it has holes. Factored out of [`excellon_drill`] so the split is unit-
/// testable on a synthesized hit list; the end-to-end authoring path for an NPTH hole is
/// the `hole` directive → a full-stackup material-less [`Role::Void`] (Decision 16b), and
/// the through-cut query above classifies it non-plated into `board-NPTH.drl`.
pub(crate) fn excellon_files(hits: Vec<(bool, Nm, DrillKind)>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (plated, label, filename) in [
        (true, "plated through-holes (PTH)", "board-PTH.drl"),
        (false, "non-plated holes (NPTH)", "board-NPTH.drl"),
    ] {
        let group: Vec<(Nm, DrillKind)> = hits
            .iter()
            .filter(|(p, _, _)| *p == plated)
            .map(|(_, d, k)| (*d, *k))
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push((filename.to_string(), excellon_program(&group, label)));
    }
    out
}
