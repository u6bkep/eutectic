//! The small built-in toy library sufficient for the M1 demo — fixture data,
//! cleanly separable from the type model in the parent module.

use crate::doc::{MM, Point};
use crate::part::Dir::*;
use crate::part::{InterfaceDef, PartDef, PartLib, PinDef, PinRole};
use std::collections::BTreeMap;

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
        // Abstract toy interface: no underlying pads, so signals keep `port.signal`
        // identity (nothing to collide with).
        pads: BTreeMap::new(),
    }
}

pub(crate) fn pin(name: &str, role: PinRole, offset: Point) -> PinDef {
    // No distinct pad numbering in the toy library: number defaults to the name.
    // The toy parts carry no footprint, so they have no pad copper geometry.
    PinDef {
        name: name.into(),
        number: name.into(),
        role,
        offset,
        pad: None,
    }
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
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
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
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        },
    );
    lib.insert(
        "MCU".into(),
        PartDef {
            name: "MCU".into(),
            pins: vec![
                pin(
                    "VDD",
                    PowerIn,
                    Point {
                        x: -3 * MM,
                        y: 3 * MM,
                    },
                ),
                pin(
                    "GND",
                    Passive,
                    Point {
                        x: -3 * MM,
                        y: -3 * MM,
                    },
                ),
            ],
            interfaces: BTreeMap::from([("uart".into(), uart())]),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        },
    );
    lib.insert(
        "Sensor".into(),
        PartDef {
            name: "Sensor".into(),
            pins: vec![
                pin(
                    "VDD",
                    PowerIn,
                    Point {
                        x: -3 * MM,
                        y: 3 * MM,
                    },
                ),
                pin(
                    "GND",
                    Passive,
                    Point {
                        x: -3 * MM,
                        y: -3 * MM,
                    },
                ),
            ],
            interfaces: BTreeMap::from([("uart".into(), uart())]),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        },
    );
    lib
}
