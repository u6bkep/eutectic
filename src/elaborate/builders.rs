//! Demo / fixture source builders (a stand-in for the textual generative layer) plus the
//! interface-port connector they and elaboration share.

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::*;
use crate::id::{EntityId, NetId};
use crate::ir::{GenDirective, Source};
use crate::part::{Dir, PartLib};
use std::collections::{BTreeMap, BTreeSet};

/// Connect two interface ports using the interface type's mate map. The mate map
/// is the single place the tx<->rx crossing is defined, so connecting two ports
/// always produces correctly-crossed nets — the swap footgun is unrepresentable.
///
/// Both components are assumed present (the caller cascade-checks them); any port /
/// type / drive fault is pushed onto `errors` (the transaction aborts on it), and a
/// fault that prevents wiring returns early without producing partial nets.
pub fn connect_interface(
    components: &BTreeMap<EntityId, Component>,
    lib: &PartLib,
    a: &(String, String),
    b: &(String, String),
    nets: &mut BTreeMap<NetId, Net>,
    errors: &mut Vec<Diagnostic>,
) {
    let (ap, aport) = a;
    let (bp, bport) = b;
    let aid = EntityId::new(ap.clone());
    let bid = EntityId::new(bp.clone());
    let ac = &components[&aid];
    let bc = &components[&bid];
    let adef = &lib[&ac.part];
    let bdef = &lib[&bc.part];
    let (Some(aiface), Some(biface)) = (adef.interfaces.get(aport), bdef.interfaces.get(bport))
    else {
        if !adef.interfaces.contains_key(aport) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_INTERFACE",
                format!(
                    "`{ap}` (part `{}`) has no interface port `{aport}`",
                    ac.part
                ),
                Location::Entity(aid),
            ));
        }
        if !bdef.interfaces.contains_key(bport) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_INTERFACE",
                format!(
                    "`{bp}` (part `{}`) has no interface port `{bport}`",
                    bc.part
                ),
                Location::Entity(bid),
            ));
        }
        return;
    };
    if aiface.type_name != biface.type_name {
        errors.push(Diagnostic::error(
            "E_INTERFACE_MISMATCH",
            format!(
                "interface type mismatch: {} vs {}",
                aiface.type_name, biface.type_name
            ),
            Location::Entity(aid),
        ));
        return;
    }

    for (sa, sb) in &aiface.mate {
        let da = aiface.signals.get(sa).copied();
        let db = biface.signals.get(sb).copied();
        let (Some(da), Some(db)) = (da, db) else {
            errors.push(Diagnostic::error(
                "E_INTERFACE_SIGNAL",
                format!(
                    "interface `{}` mate references a missing signal",
                    aiface.type_name
                ),
                Location::Entity(aid.clone()),
            ));
            continue;
        };
        // Direction sanity: a mated pair must be drive/receive, not both drivers.
        if matches!((da, db), (Dir::Out, Dir::Out)) {
            errors.push(Diagnostic::error(
                "E_DRIVE_CONFLICT",
                format!("drive conflict mating {sa}<->{sb}"),
                Location::Entity(aid.clone()),
            ));
            continue;
        }
        let net_name = format!("{ap}.{aport}.{sa}");
        let nid = NetId::new(net_name.clone());
        let net = nets.entry(nid.clone()).or_insert_with(|| Net {
            id: nid,
            name: net_name,
            members: BTreeSet::new(),
        });
        // Unify pin identity: a signal bound to a real pad (an imported part —
        // `InterfaceDef.pads`) nets under the *pad-number* PinRef, the same identity
        // the discrete pin and the floating-pad check use. Only an abstract interface
        // (no pad binding — the toy library) falls back to the `port.signal` identity,
        // which is safe there precisely because it has no underlying pad to collide
        // with. Without this, a pad wired only via its interface looks floating, and
        // discrete + interface wiring of one pad split across two net nodes.
        let a_pin = match aiface.pads.get(sa) {
            Some(num) => num.clone(),
            None => format!("{aport}.{sa}"),
        };
        let b_pin = match biface.pads.get(sb) {
            Some(num) => num.clone(),
            None => format!("{bport}.{sb}"),
        };
        net.members.insert(PinRef::new(&aid, &a_pin));
        net.members.insert(PinRef::new(&bid, &b_pin));
    }
}

// ---- source-building helpers (a stand-in for the textual generative layer) ----

/// Build the demo power-supply module with `n` decoupling caps fanned off the
/// regulator output. This is the "generator" whose output we later override and
/// re-elaborate to test minimal-perturbation reconciliation.
pub fn psu_module(n: usize) -> Source {
    let mut s = vec![GenDirective::Instance {
        path: "psu.reg".into(),
        part: "LDO".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    }];
    for i in 0..n {
        let dec = format!("psu.dec[{i}]");
        s.push(GenDirective::Instance {
            path: dec.clone(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        s.push(GenDirective::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("psu.reg".into(), "VOUT".into()),
                (dec.clone(), "p1".into()),
            ],
        });
        s.push(GenDirective::ConnectPins {
            net: "GND".into(),
            pins: vec![("psu.reg".into(), "GND".into()), (dec, "p2".into())],
        });
    }
    s
}

/// Generate a **ring** of `count` instances of `part`, evenly spaced on a circle of
/// `radius` about `center`, each rotated to **face outward** (local +x points away
/// from the centre). Per instance `i` (path `{prefix}[i]`) it emits an `Instance`, a
/// `Place` at the ring position, and a `Rotate` to the outward orientation — all
/// concrete: the `cos`/`sin` runs **once here, at generation**, producing exact
/// integer positions + quaternions that elaboration never re-derives. The motivating
/// case: side-firing LEDs around a round board (the arbitrary-angle placement that
/// the cardinal-only `Orient` could not express).
pub fn ring(prefix: &str, part: &str, center: Point, radius: Nm, count: usize) -> Source {
    let mut s = Vec::new();
    for i in 0..count {
        let path = format!("{prefix}[{i}]");
        let deg = 360.0 * i as f64 / count as f64;
        let rad = deg.to_radians();
        let pos = Point {
            x: center.x + (radius as f64 * rad.cos()).round() as Nm,
            y: center.y + (radius as f64 * rad.sin()).round() as Nm,
        };
        s.push(GenDirective::Instance {
            path: path.clone(),
            part: part.to_string(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        s.push(GenDirective::Place {
            path: path.clone(),
            pos,
        });
        s.push(GenDirective::Rotate {
            path,
            orient: Orient::from_angle_deg(deg),
        });
    }
    s
}
