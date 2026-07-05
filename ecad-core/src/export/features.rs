//! Cross-backend derived-geometry queries shared by more than one exporter:
//! [`role_features`] (silk/fab surface geometry, feeding both Gerber and SVG passes)
//! and [`pours_of`] (copper-pour fills, feeding the SVG sketch and the copper Gerber).
//! Both are forward views over the unified model, so the same inputs yield the same
//! geometry every exporter sees.

use crate::doc::Doc;
use crate::geom::{Role, Stackup};
use crate::part::PartLib;

use crate::route::doc_netlist;

/// Every world-frame feature of the board carrying `role`: board-level graphics/text
/// (from the converged [`crate::elaborate::features`] view) plus each placed component's
/// footprint graphics ([`crate::part::graphic_features`], side-swapped + placed) and
/// auto-text ([`crate::part::text_features`]). The single forward source of derived
/// surface geometry the mask/silk exporters, the fab SVG pass, and the SVG render share:
/// silk queries [`Role::Marking`], the fab drawing [`Role::Datum`] (Decision 15 — the
/// role is resolved from the slab, so both flow through the same producer). Fallible
/// because the board-level lowering resolves slab names (an unknown one is a hard error,
/// per Decision 13).
///
/// This re-derives the role-sliced surface features independently of the world-frame
/// [`crate::route::world_features`] pipeline and is a candidate for unification with it.
pub(crate) fn role_features(
    doc: &Doc,
    lib: &PartLib,
    su: &Stackup,
    role: Role,
) -> Result<Vec<crate::geom::Feature>, String> {
    let mut out: Vec<crate::geom::Feature> = Vec::new();
    for nf in crate::elaborate::features(&doc.source)? {
        if nf.feature.role == role {
            out.push(nf.feature);
        }
    }
    // Footprint auto-text (Decision 14) rides the same role-filtered path as graphics;
    // `refdes` is a whole-document query, computed once.
    let reg = crate::annotate::registry(&doc.source);
    let refdes = crate::annotate::refdes(doc, lib, &reg);
    let font = crate::elaborate::resolve_font(&doc.source);
    for (id, c) in &doc.components {
        let Some(def) = lib.get(&c.part) else {
            continue;
        };
        for f in crate::part::graphic_features(def, c, su) {
            if f.role == role {
                out.push(f);
            }
        }
        let rd = refdes.get(id).map(String::as_str).unwrap_or("");
        let lbl = crate::annotate::label(c, def, &reg);
        for f in crate::part::text_features(def, c, su, rd, &lbl, font.as_ref()) {
            if f.role == role {
                out.push(f);
            }
        }
    }
    Ok(out)
}

/// The derived copper-pour fills, for export — the [`crate::route::pours`] view over the
/// unified feature stream (the same `Shape2D::Area` conductor features DRC sees). Pure —
/// same inputs, same fills.
pub(crate) fn pours_of(doc: &Doc, lib: &PartLib) -> Vec<crate::route::Pour> {
    let su = crate::elaborate::stackup(&doc.source);
    crate::route::pours(
        doc,
        lib,
        &doc_netlist(doc),
        &crate::route::DesignRules::default(),
        &su,
    )
}
