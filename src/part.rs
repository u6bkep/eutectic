//! Part library: typed pins and typed interfaces.
//!
//! This is where "make the serial-wire swap unrepresentable" lives. A connection
//! between two devices is made at the *interface* level, and the interface type
//! itself encodes how two instances mate (UART crosses tx<->rx). A designer never
//! wires individual signals, so connecting tx-to-tx is not expressible.

use crate::doc::{Component, Point, MM};
use crate::part::Dir::*;
use std::collections::BTreeMap;

/// Signal/pin electrical direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Bidir,
}

/// Electrical role of a pin, used by ERC (which is just a typecheck over roles).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PinRole {
    PowerIn,
    PowerOut,
    Output,
    Input,
    Bidir,
    Passive,
}

impl PinRole {
    /// Does this role actively drive a net?
    pub fn is_driver(self) -> bool {
        matches!(self, PinRole::PowerOut | PinRole::Output)
    }
    fn from_dir(d: Dir) -> PinRole {
        match d {
            Out => PinRole::Output,
            In => PinRole::Input,
            Bidir => PinRole::Bidir,
        }
    }
}

/// A discrete pin on a part.
#[derive(Clone, Debug)]
pub struct PinDef {
    pub name: String,
    pub role: PinRole,
    /// Local position of the pin relative to the component origin, in nm. Combined
    /// with the component's position + orientation to get a world position.
    pub offset: Point,
}

/// A typed interface (e.g. UART). Defined once; encodes the correct mating so
/// that connecting two instances can never be wired backwards.
#[derive(Clone, Debug)]
pub struct InterfaceDef {
    pub type_name: String,
    /// signal name -> direction
    pub signals: BTreeMap<String, Dir>,
    /// signal name -> local position relative to the component origin, in nm.
    /// Carried alongside `signals` so an interface port's pins have geometry just
    /// like discrete pins do.
    pub offsets: BTreeMap<String, Point>,
    /// how to mate two instances: (signal on side A, signal on side B).
    /// For UART: (tx,rx) and (rx,tx) — the crossing is baked in.
    pub mate: Vec<(String, String)>,
}

/// A part definition: discrete pins + named interface ports.
#[derive(Clone, Debug)]
pub struct PartDef {
    pub name: String,
    pub pins: Vec<PinDef>,
    pub interfaces: BTreeMap<String, InterfaceDef>,
}

impl PartDef {
    /// Resolve the electrical role of a pin reference name.
    /// Interface signals are addressed as `port.signal` (e.g. `uart.tx`).
    pub fn pin_role(&self, pin: &str) -> Option<PinRole> {
        if let Some((port, sig)) = pin.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.signals.get(sig).copied().map(PinRole::from_dir)
        } else {
            self.pins
                .iter()
                .find(|p| p.name == pin)
                .map(|p| p.role)
        }
    }

    /// Resolve a pin reference to its local offset from the component origin.
    /// Interface signals are addressed as `port.signal` (e.g. `uart.tx`), mirroring
    /// [`pin_role`](Self::pin_role).
    pub fn pin_offset(&self, pin: &str) -> Option<Point> {
        if let Some((port, sig)) = pin.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.offsets.get(sig).copied()
        } else {
            self.pins.iter().find(|p| p.name == pin).map(|p| p.offset)
        }
    }
}

/// Absolute (world) position of a pin on a placed component instance:
/// `component position + rotate(local pin offset, component orientation)`.
/// Exact for the four cardinal rotations. Returns `None` if the pin is unknown.
pub fn pin_world(comp: &Component, def: &PartDef, pin: &str) -> Option<Point> {
    let off = def.pin_offset(pin)?;
    let r = comp.orient.rotate(off);
    Some(Point { x: comp.pos.value.x + r.x, y: comp.pos.value.y + r.y })
}

pub type PartLib = BTreeMap<String, PartDef>;

fn uart() -> InterfaceDef {
    InterfaceDef {
        type_name: "UART".into(),
        signals: BTreeMap::from([("tx".into(), Out), ("rx".into(), In)]),
        // Two adjacent pins on the component's right edge.
        offsets: BTreeMap::from([
            ("tx".into(), Point { x: 3 * MM, y: MM }),
            ("rx".into(), Point { x: 3 * MM, y: -MM }),
        ]),
        // The crossing that designers get wrong by hand, encoded once, correctly.
        mate: vec![("tx".into(), "rx".into()), ("rx".into(), "tx".into())],
    }
}

fn pin(name: &str, role: PinRole, offset: Point) -> PinDef {
    PinDef { name: name.into(), role, offset }
}

/// A small built-in library sufficient for the M1 demo.
pub fn part_library() -> PartLib {
    use PinRole::*;
    let mut lib = PartLib::new();

    // Offsets are plausible local pin geometry (nm), not exact footprints: a
    // small SOT-23-ish LDO, a two-terminal cap, and ~6mm ICs with pins on edges.
    lib.insert(
        "LDO".into(),
        PartDef {
            name: "LDO".into(),
            pins: vec![
                pin("VIN", PowerIn, Point { x: -2 * MM, y: 0 }),
                pin("VOUT", PowerOut, Point { x: 2 * MM, y: 0 }),
                pin("GND", Passive, Point { x: 0, y: -2 * MM }),
            ],
            interfaces: BTreeMap::new(),
        },
    );
    lib.insert(
        "Cap".into(),
        PartDef {
            name: "Cap".into(),
            pins: vec![
                pin("p1", Passive, Point { x: -MM, y: 0 }),
                pin("p2", Passive, Point { x: MM, y: 0 }),
            ],
            interfaces: BTreeMap::new(),
        },
    );
    lib.insert(
        "MCU".into(),
        PartDef {
            name: "MCU".into(),
            pins: vec![
                pin("VDD", PowerIn, Point { x: -3 * MM, y: 3 * MM }),
                pin("GND", Passive, Point { x: -3 * MM, y: -3 * MM }),
            ],
            interfaces: BTreeMap::from([("uart".into(), uart())]),
        },
    );
    lib.insert(
        "Sensor".into(),
        PartDef {
            name: "Sensor".into(),
            pins: vec![
                pin("VDD", PowerIn, Point { x: -3 * MM, y: 3 * MM }),
                pin("GND", Passive, Point { x: -3 * MM, y: -3 * MM }),
            ],
            interfaces: BTreeMap::from([("uart".into(), uart())]),
        },
    );
    lib
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Dof, Orient, Provenance};
    use crate::id::EntityId;

    fn comp(part: &str, pos: Point, orient: Orient) -> Component {
        Component {
            id: EntityId::new("u1"),
            part: part.into(),
            pos: Dof { value: pos, prov: Provenance::Free },
            orient,
        }
    }

    #[test]
    fn pin_offset_resolves_discrete_and_interface_pins() {
        let lib = part_library();
        let ldo = &lib["LDO"];
        assert_eq!(ldo.pin_offset("VOUT"), Some(Point { x: 2 * MM, y: 0 }));
        assert_eq!(ldo.pin_offset("nope"), None);
        let mcu = &lib["MCU"];
        // Interface signals addressed as `port.signal`.
        assert_eq!(mcu.pin_offset("uart.tx"), Some(Point { x: 3 * MM, y: MM }));
        assert_eq!(mcu.pin_offset("uart.bogus"), None);
    }

    /// A pin's world position is exact under each of the four cardinal rotations.
    #[test]
    fn pin_world_exact_under_each_cardinal_rotation() {
        let lib = part_library();
        let ldo = &lib["LDO"];
        // VOUT local offset is (2mm, 0); component at (10mm, 5mm).
        let at = Point::mm(10, 5);
        let cases = [
            (Orient::Deg0, Point { x: 12 * MM, y: 5 * MM }),   // (+2, 0)
            (Orient::Deg90, Point { x: 10 * MM, y: 7 * MM }),  // (0, +2)
            (Orient::Deg180, Point { x: 8 * MM, y: 5 * MM }),  // (-2, 0)
            (Orient::Deg270, Point { x: 10 * MM, y: 3 * MM }), // (0, -2)
        ];
        for (o, expected) in cases {
            let c = comp("LDO", at, o);
            assert_eq!(pin_world(&c, ldo, "VOUT"), Some(expected), "rotation {:?}", o);
        }
    }

    #[test]
    fn rotate_is_exact_and_reversible() {
        let p = Point { x: 3 * MM, y: MM };
        assert_eq!(Orient::Deg0.rotate(p), p);
        // Two 180s (or four 90s) return to the original — exact, no drift.
        assert_eq!(Orient::Deg180.rotate(Orient::Deg180.rotate(p)), p);
        let q = Orient::Deg90.rotate(Orient::Deg90.rotate(Orient::Deg90.rotate(Orient::Deg90.rotate(p))));
        assert_eq!(q, p);
    }

    #[test]
    fn orient_from_deg_normalises_and_rejects_off_axis() {
        assert_eq!(Orient::from_deg(-90), Some(Orient::Deg270));
        assert_eq!(Orient::from_deg(450), Some(Orient::Deg90));
        assert_eq!(Orient::from_deg(360), Some(Orient::Deg0));
        assert_eq!(Orient::from_deg(45), None);
    }
}
