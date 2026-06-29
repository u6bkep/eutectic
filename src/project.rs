//! Text projection: a deterministic rendering of the model.
//!
//! This is the agent-facing / git-diffable view. It is a *pure function of the
//! model* (render only) — there is no separate text artifact kept in sync. The
//! BTreeMap iteration order makes the output byte-stable, so a one-thing change
//! produces a one-line diff.

use crate::doc::{DecayReason, Doc, Orient, Provenance};

pub fn render(doc: &Doc) -> String {
    let mut out = String::new();
    out.push_str("# components\n");
    for c in doc.components.values() {
        let prov = match c.pos.prov {
            Provenance::Free => "free",
            Provenance::Hint => "hint",
            Provenance::Pinned => "pinned",
            Provenance::Fixed => "fixed",
        };
        let rot = if c.orient == Orient::Deg0 {
            String::new()
        } else {
            format!(" rot={}", c.orient.to_deg())
        };
        out.push_str(&format!(
            "  {id}: {part} @ ({x},{y}){rot} [{prov}]\n",
            id = c.id,
            part = c.part,
            x = c.pos.value.x,
            y = c.pos.value.y,
        ));
    }
    out.push_str("# nets\n");
    for net in doc.nets.values() {
        let pins: Vec<String> = net
            .members
            .iter()
            .map(|p| format!("{}.{}", p.comp, p.pin))
            .collect();
        out.push_str(&format!("  {}: {}\n", net.name, pins.join(" — ")));
    }
    let r = &doc.report;
    if !r.is_clean() {
        out.push_str("# reconciliation\n");
        for (id, reason) in &r.decayed {
            let why = match reason {
                DecayReason::RedundantWithDefault => "matched default",
                DecayReason::OverriddenByConstraint => "overridden by constraint",
            };
            out.push_str(&format!("  ~ decayed hint `{id}` ({why})\n"));
        }
        for id in &r.pin_conflicts {
            out.push_str(&format!("  ! pin `{id}` conflicts with a hard constraint\n"));
        }
        for id in &r.redundant_pins {
            out.push_str(&format!("  ? pin `{id}` no longer changes the outcome\n"));
        }
        for id in &r.orphaned {
            out.push_str(&format!("  ! orphaned override `{id}` (entity gone)\n"));
        }
    }
    out
}
