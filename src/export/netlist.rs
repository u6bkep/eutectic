//! The connectivity artifact: the human-readable [`netlist`]. The membership map that
//! every geometry exporter feeds into the unified [`crate::route::world_features`] /
//! [`crate::route::pours`] queries lives beside those queries as
//! [`crate::route::doc_netlist`].

use crate::doc::Doc;

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
