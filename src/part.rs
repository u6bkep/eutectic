//! Part library: typed pins and typed interfaces.
//!
//! This is where "make the serial-wire swap unrepresentable" lives. A connection
//! between two devices is made at the *interface* level, and the interface type
//! itself encodes how two instances mate (UART crosses tx<->rx). A designer never
//! wires individual signals, so connecting tx-to-tx is not expressible.

use crate::doc::{Component, Nm, Point, MM};
use crate::part::Dir::*;
use std::collections::BTreeMap;

/// A pad's copper geometry, as spelled in a footprint's `(pad … <shape> (size w h))`.
/// The four shapes the prototype recognises; an unrecognised footprint shape token
/// is mapped to the nearest of these by the importer (documented there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadShape {
    Circle,
    Rect,
    RoundRect,
    Oval,
}

/// Render-only pad copper geometry attached to a [`PinDef`]: the pad's `size`
/// (width, height in nm) and its [`PadShape`]. This is **fab-output metadata
/// only** — it exists so a footprint pad can *flash as copper* in Gerber. DRC and
/// the autorouter deliberately keep treating a pad as its `pin_world` *point*
/// (radius 0), so this field never changes connectivity or clearance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pad {
    pub size: (Nm, Nm),
    pub shape: PadShape,
}

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
///
/// `name` vs `number`: the **functional name** (`GPIO0`, `VDD`, `SWCLK`) is the
/// human/agent-facing *selector* humans reference; the pad **number** (`12`, `MP`)
/// is the geometry/manufacturing key, the join key pairing a symbol pin with a
/// footprint pad, **and the stable identity stored in a [`PinRef`]**. Names repeat
/// (six pads named `IOVDD`); numbers are unique within a part, so identity keys on
/// the number. A name fans out to its pads via
/// [`resolve_selector`](PartDef::resolve_selector); `pin_role`/`pin_offset` resolve
/// the resulting *number*. For parts with no functional naming (a raw footprint
/// import, or the toy `part_library`) the two coincide — `number` defaults to
/// `name` via the [`pin`] constructor.
///
/// [`PinRef`]: crate::doc::PinRef
#[derive(Clone, Debug)]
pub struct PinDef {
    pub name: String,
    /// Pad/manufacturing number used as the symbol↔footprint join key. Defaults to
    /// `name` when there is no distinct numbering.
    pub number: String,
    pub role: PinRole,
    /// Local position of the pin relative to the component origin, in nm. Combined
    /// with the component's position + orientation to get a world position.
    pub offset: Point,
    /// Optional copper geometry for fab output ([`Pad`]). `Some` for an imported
    /// footprint pad (shape + size); `None` for the toy `part_library` pins, which
    /// carry no footprint. **Render-only** — DRC/the solver treat pads as points and
    /// never read this (see [`Pad`]).
    pub pad: Option<Pad>,
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
    /// Resolve the electrical role of a *stored pin identity* (see [`PinRef`]):
    /// a pad **number** for a discrete pin, or `port.signal` for an interface
    /// signal. Pad numbers are unique within a part, so this is unambiguous —
    /// unlike functional names, which repeat (six `IOVDD` pads share a name but
    /// have distinct numbers). Use [`resolve_selector`](Self::resolve_selector) to
    /// turn a user-facing name into the identities this resolves.
    ///
    /// [`PinRef`]: crate::doc::PinRef
    pub fn pin_role(&self, id: &str) -> Option<PinRole> {
        if let Some((port, sig)) = id.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.signals.get(sig).copied().map(PinRole::from_dir)
        } else {
            self.pins
                .iter()
                .find(|p| p.number == id)
                .map(|p| p.role)
        }
    }

    /// Resolve a *stored pin identity* to its local offset from the component
    /// origin. Identity semantics match [`pin_role`](Self::pin_role): a pad number
    /// for a discrete pin, or `port.signal` for an interface signal.
    pub fn pin_offset(&self, id: &str) -> Option<Point> {
        if let Some((port, sig)) = id.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.offsets.get(sig).copied()
        } else {
            self.pins.iter().find(|p| p.number == id).map(|p| p.offset)
        }
    }

    /// Resolve a *connection selector* (a user/agent-facing pin reference) to the
    /// set of stable pin identities it names — the pad **numbers** to store as
    /// [`PinRef`]s. This is the one place a functional name fans out to physical
    /// pads, which is what keeps a multi-pad power rail (six `IOVDD`) from
    /// collapsing to a single member.
    ///
    /// Resolution order:
    /// - `port.signal` (contains `.`) → an interface signal: returns that single
    ///   identity if the port and signal exist, else empty.
    /// - otherwise match by functional **name** first (so `IOVDD` → every IOVDD
    ///   pad's number); if no name matches, fall back to matching a pad **number**
    ///   directly (so `30` / `MP` selects that one pad).
    ///
    /// An **empty** result means the selector names nothing on this part — a typo
    /// or a role gap. Callers must treat that as an error, never a silent no-op.
    /// The fanout is scoped to this one part: a name never reaches across instances.
    ///
    /// [`PinRef`]: crate::doc::PinRef
    pub fn resolve_selector(&self, sel: &str) -> Vec<String> {
        if let Some((port, sig)) = sel.split_once('.') {
            return match self.interfaces.get(port) {
                Some(iface) if iface.signals.contains_key(sig) => vec![sel.to_string()],
                _ => Vec::new(),
            };
        }
        let by_name: Vec<String> =
            self.pins.iter().filter(|p| p.name == sel).map(|p| p.number.clone()).collect();
        if !by_name.is_empty() {
            return by_name;
        }
        // Fall back to a direct pad-number reference.
        self.pins.iter().filter(|p| p.number == sel).map(|p| p.number.clone()).collect()
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
    // No distinct pad numbering in the toy library: number defaults to the name.
    // The toy parts carry no footprint, so they have no pad copper geometry.
    PinDef { name: name.into(), number: name.into(), role, offset, pad: None }
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

    #[test]
    fn resolve_selector_fans_out_by_name_and_falls_back_to_number() {
        use PinRole::*;
        let mk = |name: &str, number: &str, role| PinDef {
            name: name.into(),
            number: number.into(),
            role,
            offset: Point { x: 0, y: 0 },
            pad: None,
        };
        let part = PartDef {
            name: "P".into(),
            // Two pads share the name VDD (distinct numbers) — the duplicate-power
            // case; numbers are out of order to prove order follows declaration.
            pins: vec![mk("VDD", "1", PowerIn), mk("VDD", "8", PowerIn), mk("GND", "4", Passive)],
            interfaces: BTreeMap::new(),
        };
        // A functional name fans out to *every* matching pad number.
        assert_eq!(part.resolve_selector("VDD"), vec!["1".to_string(), "8".to_string()]);
        assert_eq!(part.resolve_selector("GND"), vec!["4".to_string()]);
        // No name matches -> fall back to a direct pad-number reference.
        assert_eq!(part.resolve_selector("8"), vec!["8".to_string()]);
        // Names nothing -> empty, so the caller raises a hard error (no silent dangle).
        assert!(part.resolve_selector("NOPE").is_empty());
        // Stored identity resolves by number, never by the colliding name.
        assert_eq!(part.pin_role("8"), Some(PowerIn));
        assert_eq!(part.pin_role("VDD"), None);
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
