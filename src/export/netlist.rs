//! The connectivity artifact: the human-readable [`netlist`] and the [`doc_netlist`]
//! membership map every geometry exporter feeds into the unified
//! [`crate::route::world_features`] / [`crate::route::pours`] queries.

use crate::doc::Doc;
use std::collections::BTreeMap;

/// The connectivity artifact: every net and the pins it joins, in canonical form.
///
/// One net per line, `name: comp.pin comp.pin ...`. Nets iterate in `NetId` order
/// and pins in `PinRef` order (both `BTree*`), so the output is fully deterministic
/// and is the thing you check a fabricated/assembled board against.
pub fn netlist(doc: &Doc) -> String {
    let mut out = String::new();
    out.push_str("# netlist\n");
    for net in doc.nets.values() {
        let pins: Vec<String> = net
            .members
            .iter()
            .map(|p| format!("{}.{}", p.comp, p.pin))
            .collect();
        out.push_str(&format!("{}: {}\n", net.name, pins.join(" ")));
    }
    out
}

/// The membership netlist from the materialized nets (roles are irrelevant to the
/// geometry producer). The bridge every exporter uses to feed the unified
/// [`crate::route::world_features`] / [`crate::route::pours`] queries.
pub(crate) fn doc_netlist(
    doc: &Doc,
) -> BTreeMap<crate::id::NetId, Vec<(crate::doc::PinRef, crate::part::PinRole)>> {
    use crate::part::PinRole;
    doc.nets
        .iter()
        .map(|(nid, net)| {
            (
                nid.clone(),
                net.members
                    .iter()
                    .map(|pr| (pr.clone(), PinRole::Passive))
                    .collect(),
            )
        })
        .collect()
}
