//! The pick-and-place artifact ([`placement_csv`]) and [`part_pin_ids`], the
//! deterministic enumeration of a part's stable pin identities shared by the SVG and
//! Gerber backends.

use crate::doc::Doc;
use crate::part::PartDef;

use super::svg_writer::fmt_mm;

/// Enumerate the stable pin identities of a part, deterministically: discrete pins
/// by pad **number** in declaration order, then `port.signal` for each interface
/// signal (both `BTreeMap`-ordered). These are exactly the identities
/// [`crate::part::pin_world`] resolves — numbers, not names, since functional names
/// can repeat across pads.
pub(crate) fn part_pin_ids(def: &PartDef) -> Vec<String> {
    let mut ids: Vec<String> = def.pins.iter().map(|p| p.number.clone()).collect();
    for (port, iface) in &def.interfaces {
        for sig in iface.signals.keys() {
            // Issue 0029: skip a signal *bound* to a real pad (`iface.pads`). Its pad is
            // already enumerated by number above — the bound signal *is* that pad (the pin
            // identity `iface_infer` nets under). Enumerating `port.signal` too would draw
            // the same physical pin twice: once as real pad copper (by number) and once as
            // the 0.3 mm fallback dot (`port.signal` finds no `PinDef`), so the fallback
            // painted a spurious duplicate dot over the copper. An abstract (unbound)
            // signal keeps its `port.signal` identity — it has no colliding pad.
            if iface.pads.contains_key(sig) {
                continue;
            }
            ids.push(format!("{port}.{sig}"));
        }
    }
    ids
}

/// A pick-and-place CSV: one row per component, `ref,part,x_mm,y_mm,rotation_deg`.
///
/// Rows iterate in `EntityId` order. Coordinates use [`fmt_mm`] (six-decimal mm);
/// rotation is the component's cardinal orientation in degrees. Refs and part
/// names are hierarchical paths / library keys that contain no commas in the
/// current model, so they are emitted unquoted (a quoting/escaping pass is future
/// work if names ever gain commas).
pub fn placement_csv(doc: &Doc) -> String {
    let mut out = String::new();
    out.push_str("ref,part,x_mm,y_mm,rotation_deg,side\n");
    for c in doc.components.values() {
        // KiCad .pos convention: report the *authored* about-z angle with the side
        // marked separately, so a plain bottom flip is `0,B` (not `180,B`). Raw
        // `to_deg()` would couple the flip axis into the angle and misrotate parts on a
        // P&P line, so un-flip bottom parts first. `flipped()∘flipped() = −q` and
        // `to_deg` is negation-invariant, so this recovers the authored angle exactly.
        let base = if c.orient.is_bottom() {
            c.orient.flipped()
        } else {
            c.orient
        };
        out.push_str(&format!(
            "{},{},{},{},{},{}\n",
            c.id,
            c.part,
            fmt_mm(c.pos.value.x),
            fmt_mm(c.pos.value.y),
            base.to_deg(),
            if c.orient.is_bottom() { "B" } else { "T" },
        ));
    }
    out
}
