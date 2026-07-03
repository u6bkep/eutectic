//! ecad-core — M1 engine prototype.
//!
//! A vertical slice of the architecture in docs/architecture.md:
//!
//! - `doc` — the immutable three-tier document (source/overrides -> materialized
//!   instances/nets; derived tier lives in `query`).
//! - `command` — the sole mutation surface: atomic transactions.
//! - `history` — the version DAG (undo / branch / replay).
//! - `query` — hand-rolled incremental query engine (Netlist, ERC, DRC).
//! - `route` — routed copper representation (trace/via/layer) + the DRC kernel.
//! - `autoroute` — basic deterministic grid/maze autorouter (transaction-proposer).
//! - `elaborate` — generative source -> instances + ID-keyed override reconcile.
//! - `part` — typed pins & interfaces (makes the serial swap unrepresentable).
//! - `project` — deterministic text projection (agent/git view).
//! - `text` — canonical serializer + parser for tier-1 truth (the text front-end).
//! - `export` — deterministic output artifacts (netlist / pick-and-place / SVG).

pub mod annotate;
pub mod autoroute;
pub mod command;
pub mod diagnostic;
pub mod doc;
pub mod elaborate;
pub mod export;
pub mod font;
pub mod geom;
pub mod history;
pub mod id;
pub mod kicad;
pub mod part;
pub mod project;
pub mod quantity;
pub mod query;
pub mod region;
pub mod route;
pub mod solve;
pub mod svg_import;
pub mod text;
pub mod ttf;

/// Build a root document from a generative source by elaborating it once.
pub fn boot(
    source: elaborate::Source,
    lib: &part::PartLib,
) -> Result<doc::Doc, Vec<diagnostic::Diagnostic>> {
    let mut h = history::History::new(doc::Doc::default());
    h.commit(
        command::Transaction::one(command::Command::SetSource(source)),
        lib,
        "boot",
    )?;
    Ok(h.doc().clone())
}

#[cfg(test)]
mod tests {
    use super::command::{Command, Resolution, Transaction, suggested_resolutions};
    use super::doc::{DecayReason, Doc, MM, Nm, Point, Provenance};
    use super::elaborate::{GenDirective, Source, board_rect, psu_module};
    use super::geom::Shape2D;
    use super::history::History;
    use super::id::{EntityId, NetId, TraceId, ViaId};
    use super::part::part_library;
    use super::query::{Engine, Key};
    use super::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region};
    use super::route::{Trace, Via, Violation};
    use super::solve::{Constraint, PLACE_TOL, Problem, dist, solve};
    use std::collections::{BTreeMap, BTreeSet};

    fn placed(src: Source) -> Doc {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        h.doc().clone()
    }
    fn pos(d: &Doc, id: &str) -> Point {
        d.components[&EntityId::new(id)].pos.value
    }

    fn uart_link() -> Source {
        vec![
            GenDirective::Instance {
                path: "mcu".into(),
                part: "MCU".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "sens".into(),
                part: "Sensor".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::ConnectInterface {
                a: ("mcu".into(), "uart".into()),
                b: ("sens".into(), "uart".into()),
            },
        ]
    }

    #[test]
    fn interface_connection_crosses_tx_rx() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(uart_link())),
            &lib,
            "uart",
        )
        .unwrap();
        let mut eng = Engine::new();
        let nl = eng.query(h.doc(), &lib, Key::Netlist);
        let nl = nl.as_netlist();
        // The net carrying mcu.uart.tx must also carry sens.uart.rx (crossed),
        // never sens.uart.tx. The swap is not expressible.
        let tx_net = nl
            .iter()
            .find(|(_, pins)| {
                pins.iter()
                    .any(|(p, _)| p.pin == "uart.tx" && p.comp.as_str() == "mcu")
            })
            .expect("tx net");
        let names: Vec<String> = tx_net
            .1
            .iter()
            .map(|(p, _)| format!("{}.{}", p.comp, p.pin))
            .collect();
        assert!(names.contains(&"sens.uart.rx".to_string()), "got {names:?}");
        assert!(!names.contains(&"sens.uart.tx".to_string()));
    }

    #[test]
    fn transaction_is_atomic_on_error() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(2))),
            &lib,
            "psu",
        )
        .unwrap();
        let before = super::project::render(h.doc());
        // A source referencing an unknown part must fail and leave head untouched.
        let bad = vec![GenDirective::Instance {
            path: "x".into(),
            part: "Nope".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }];
        let r = h.commit(Transaction::one(Command::SetSource(bad)), &lib, "bad");
        assert!(r.is_err());
        assert_eq!(before, super::project::render(h.doc()));
    }

    #[test]
    fn nudge_skips_both_queries_geometry_only() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(2))),
            &lib,
            "psu",
        )
        .unwrap();
        let mut eng = Engine::new();
        eng.query(h.doc(), &lib, Key::Erc);
        let (n0, e0) = (eng.count(Key::Netlist), eng.count(Key::Erc));

        // Pure geometry edit: pin a decoupler's position.
        h.commit(
            Transaction::one(Command::Nudge(EntityId::new("psu.dec[0]"), Point::mm(5, 5))),
            &lib,
            "nudge",
        )
        .unwrap();
        eng.query(h.doc(), &lib, Key::Erc);
        // Neither query re-ran: connectivity input was untouched.
        assert_eq!(
            eng.count(Key::Netlist),
            n0,
            "netlist must not recompute on a nudge"
        );
        assert_eq!(eng.count(Key::Erc), e0, "erc must not recompute on a nudge");
    }

    #[test]
    fn early_cutoff_skips_erc_when_netlist_value_unchanged() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(2))),
            &lib,
            "psu",
        )
        .unwrap();
        let mut eng = Engine::new();
        eng.query(h.doc(), &lib, Key::Erc);
        let (n0, e0) = (eng.count(Key::Netlist), eng.count(Key::Erc));

        // Add an *unconnected* component: bumps connectivity (component set
        // changed) so Netlist recomputes, but the resolved netlist value is
        // identical -> ERC must be skipped by early cutoff.
        let mut src = psu_module(2);
        src.push(GenDirective::Instance {
            path: "psu.spare".into(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "spare")
            .unwrap();
        eng.query(h.doc(), &lib, Key::Erc);
        assert_eq!(eng.count(Key::Netlist), n0 + 1, "netlist should recompute");
        assert_eq!(eng.count(Key::Erc), e0, "erc should be cut off");
    }

    // A part with several pads sharing one functional name (the real
    // duplicate-power-pin shape: an MCU's six IOVDD pads). Numbers are unique.
    fn dup_power_lib() -> super::part::PartLib {
        use super::part::{PartDef, PinDef, PinRole};
        let mk = |name: &str, number: &str, role| PinDef {
            name: name.into(),
            number: number.into(),
            role,
            offset: Point { x: 0, y: 0 },
            pad: None,
        };
        let mut lib = super::part::PartLib::new();
        lib.insert(
            "PWRCHIP".into(),
            PartDef {
                name: "PWRCHIP".into(),
                pins: vec![
                    mk("VDD", "1", PinRole::PowerIn),
                    mk("VDD", "11", PinRole::PowerIn),
                    mk("VDD", "20", PinRole::PowerIn),
                    mk("GND", "2", PinRole::Passive),
                ],
                interfaces: BTreeMap::new(),
                graphics: Vec::new(),
                texts: Vec::new(),
                courtyard: None,
                class: None,
            },
        );
        lib
    }

    fn dup_power_source() -> Source {
        vec![
            GenDirective::Instance {
                path: "u1".into(),
                part: "PWRCHIP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::ConnectPins {
                net: "+3V3".into(),
                pins: vec![("u1".into(), "VDD".into())],
            },
        ]
    }

    /// Issue 0001 regression: connecting a net to a duplicated power-pin *name*
    /// must net every physical pad, not collapse to one. Three VDD pads → three
    /// members keyed by distinct pad numbers.
    #[test]
    fn duplicate_power_name_fans_out_to_every_pad() {
        let lib = dup_power_lib();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(dup_power_source())),
            &lib,
            "s",
        )
        .unwrap();
        let net = &h.doc().nets[&NetId::new("+3V3")];
        let pads: BTreeSet<String> = net.members.iter().map(|p| p.pin.clone()).collect();
        assert_eq!(
            pads,
            BTreeSet::from(["1".to_string(), "11".to_string(), "20".to_string()]),
            "all three VDD pads must be on the net"
        );
    }

    /// Issue 0001 completeness half: a pad on no net is reported by the Floating
    /// query until it is netted or explicitly no-connect — never silent.
    #[test]
    fn floating_pad_reported_until_netted_or_no_connect() {
        let lib = dup_power_lib();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(dup_power_source())),
            &lib,
            "s",
        )
        .unwrap();
        let mut eng = Engine::new();
        // VDD pads are netted; GND is not → exactly one floating pad.
        let floats = eng.query(h.doc(), &lib, Key::Floating);
        let floats = floats.as_floating();
        assert_eq!(floats.len(), 1, "only GND floats: {floats:?}");
        assert_eq!(floats[0].code, "E_FLOATING_PAD");
        assert!(floats[0].message.contains("GND"), "got {:?}", floats[0]);

        // Marking GND no-connect clears it.
        let mut src = dup_power_source();
        src.push(GenDirective::NoConnect {
            pins: vec![("u1".into(), "GND".into())],
        });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "nc")
            .unwrap();
        let floats = eng.query(h.doc(), &lib, Key::Floating);
        assert!(
            floats.as_floating().is_empty(),
            "no floats after NC: {:?}",
            floats.as_floating()
        );
    }

    /// Issue 0002: a connection to a pin the part doesn't have is a hard
    /// elaboration error (atomic — the transaction never commits), not a silent
    /// dangling member.
    #[test]
    fn connect_to_unknown_pin_is_a_hard_error() {
        let lib = dup_power_lib();
        let mut h = History::new(Default::default());
        let src = vec![
            GenDirective::Instance {
                path: "u1".into(),
                part: "PWRCHIP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::ConnectPins {
                net: "x".into(),
                pins: vec![("u1".into(), "TYPO".into())],
            },
        ];
        let err = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_UNKNOWN_PIN"), "got {err:?}");
        let text = super::diagnostic::render(&err);
        assert!(text.contains("TYPO") && text.contains("u1"), "got {text}");
    }

    /// Collect-all: independent faults are *all* reported in one elaboration, not
    /// just the first — the rustc-style shape we want long-term.
    #[test]
    fn elaboration_collects_all_independent_faults() {
        let lib = dup_power_lib();
        let mut h = History::new(Default::default());
        let src = vec![
            GenDirective::Instance {
                path: "u1".into(),
                part: "PWRCHIP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "u2".into(),
                part: "PWRCHIP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::ConnectPins {
                net: "a".into(),
                pins: vec![("u1".into(), "TYPO1".into())],
            },
            GenDirective::ConnectPins {
                net: "b".into(),
                pins: vec![("u2".into(), "TYPO2".into())],
            },
        ];
        let errs = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap_err();
        assert_eq!(
            errs.iter().filter(|d| d.code == "E_UNKNOWN_PIN").count(),
            2,
            "both typos: {errs:?}"
        );
        let text = super::diagnostic::render(&errs);
        assert!(
            text.contains("TYPO1") && text.contains("TYPO2"),
            "got {text}"
        );
    }

    // A part with one pin carrying a real 0.5 mm square copper pad (top layer) — so
    // DRC sees copper with extent, not a point.
    fn pad_lib() -> super::part::PartLib {
        use super::geom::Shape2D;
        use super::part::{PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole};
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(PadGeo {
                copper: vec![PadCopper {
                    shape: Shape2D::rect(Point { x: 0, y: 0 }, 500_000, 500_000),
                    layers: PadLayers::Top,
                }],
                drill: None,
            }),
        };
        let mut lib = super::part::PartLib::new();
        lib.insert(
            "PAD".into(),
            PartDef {
                name: "PAD".into(),
                pins: vec![pin],
                interfaces: BTreeMap::new(),
                graphics: Vec::new(),
                texts: Vec::new(),
                courtyard: None,
                class: None,
            },
        );
        lib
    }

    fn two_pads_at(x2: Nm) -> Source {
        vec![
            GenDirective::Instance {
                path: "p1".into(),
                part: "PAD".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "p2".into(),
                part: "PAD".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Fix {
                path: "p1".into(),
                pos: Point { x: 0, y: 0 },
            },
            GenDirective::Fix {
                path: "p2".into(),
                pos: Point { x: x2, y: 0 },
            },
            GenDirective::ConnectPins {
                net: "A".into(),
                pins: vec![("p1".into(), "1".into())],
            },
            GenDirective::ConnectPins {
                net: "B".into(),
                pins: vec![("p2".into(), "1".into())],
            },
        ]
    }

    /// Stage B2: the solver keeps movable parts inside an arbitrary board outline and
    /// out of cutouts (not just a rect). A part placed far outside is pulled in; one
    /// placed in a cutout is pushed to the cutout boundary.
    #[test]
    fn solver_respects_outline_and_cutouts() {
        // 20×20 mm board centred at (10,10) → [0,20]²; a 4 mm cutout at the centre.
        let outline = Shape2D::rect(Point::mm(10, 10), 20 * MM, 20 * MM);
        let cutout = Shape2D::rect(Point::mm(10, 10), 4 * MM, 4 * MM); // [8,12]²
        let src = vec![
            GenDirective::Board {
                outline: outline.clone(),
            },
            GenDirective::Cutout {
                shape: cutout.clone(),
            },
            GenDirective::Instance {
                path: "a".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "a".into(),
                pos: Point::mm(50, 50),
            }, // far outside
            GenDirective::Instance {
                path: "b".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "b".into(),
                pos: Point::mm(10, 10),
            }, // in the cutout
        ];
        let d = placed(src);
        // The board region (outline ∖ cutout) as the substrate `Area`, mirroring
        // `elaborate::board_region`.
        let board = Shape2D::Area {
            region: difference(
                &shape_to_region(&outline, DEFAULT_CIRCLE_SEGS),
                &shape_to_region(&cutout, DEFAULT_CIRCLE_SEGS),
            ),
        };
        let (pa, pb) = (pos(&d, "a"), pos(&d, "b"));
        assert!(
            board.contains_point(pa),
            "part placed outside is pulled onto the board: {pa:?}"
        );
        // Pushed from the cutout centre to its boundary (~2 mm away).
        assert!(
            dist(pb, Point::mm(10, 10)) >= (2 * MM - PLACE_TOL) as f64,
            "part in the cutout is pushed to its boundary: {pb:?}"
        );
    }

    /// Issue 0005: the placement solver keeps component courtyards from overlapping.
    /// Two pad-bearing parts placed coincident are pushed apart until their
    /// courtyards (pad bbox + margin) clear. A footprint-less toy part has no
    /// courtyard, so this only governs real (pad-bearing) parts.
    #[test]
    fn placement_avoids_courtyard_overlap() {
        let lib = pad_lib(); // PAD: a 0.5 mm pad → courtyard half-extent 0.25 mm + margin.
        let src = vec![
            GenDirective::Instance {
                path: "p1".into(),
                part: "PAD".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "p2".into(),
                part: "PAD".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "p1".into(),
                pos: Point { x: 0, y: 0 },
            },
            GenDirective::Place {
                path: "p2".into(),
                pos: Point { x: 0, y: 0 },
            }, // coincident
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        let (a, b) = (pos(h.doc(), "p1"), pos(h.doc(), "p2"));
        // Courtyards must not overlap: separated by ≥ (sum of half-extents) on an axis.
        let sep = 2 * (250_000 + super::part::COURTYARD_MARGIN);
        let (dx, dy) = ((a.x - b.x).abs(), (a.y - b.y).abs());
        assert!(
            dx >= sep - PLACE_TOL || dy >= sep - PLACE_TOL,
            "courtyards still overlap: p1={a:?} p2={b:?} (need ≥{sep} on an axis)"
        );
    }

    /// A library with a long rectangular part (`BAR`, a 6 mm × 1 mm pad) and a small
    /// square (`DOT`, a 0.5 mm pad). The rectangle is what makes rotation *matter*: a
    /// square's courtyard is rotation-invariant, but a rotated bar's true hull is very
    /// different from the axis-aligned box of it — the crux of issue 0019.
    fn bar_lib() -> super::part::PartLib {
        use super::geom::Shape2D;
        use super::part::{PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole};
        let pad = |w, h| PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(PadGeo {
                copper: vec![PadCopper {
                    shape: Shape2D::rect(Point { x: 0, y: 0 }, w, h),
                    layers: PadLayers::Top,
                }],
                drill: None,
            }),
        };
        let mk = |name: &str, w, h| PartDef {
            name: name.into(),
            pins: vec![pad(w, h)],
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        };
        // RND is a lone round pad: its copper hull is a single point (no 2-D hull), so
        // `courtyard_shape` is None and it takes the axis-aligned box fallback with a
        // radius-0 courtyard (half-extent 0.25 mm pad + 0.25 mm margin = 0.5 mm).
        let rnd = PartDef {
            name: "RND".into(),
            pins: vec![PinDef {
                name: "1".into(),
                number: "1".into(),
                role: PinRole::Passive,
                offset: Point { x: 0, y: 0 },
                pad: Some(PadGeo {
                    copper: vec![PadCopper {
                        shape: Shape2D::disc(Point { x: 0, y: 0 }, 250_000),
                        layers: PadLayers::Top,
                    }],
                    drill: None,
                }),
            }],
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        };
        let mut lib = super::part::PartLib::new();
        lib.insert("BAR".into(), mk("BAR", 6 * MM, MM));
        lib.insert("DOT".into(), mk("DOT", 500_000, 500_000));
        lib.insert("RND".into(), rnd);
        lib
    }

    /// Issue 0019: the solver exploits the *rotated* polygonal courtyard, not the
    /// axis-aligned box of it. A bar rotated 45° has a large square AABB but a thin
    /// diagonal true hull. A small part parked in the empty corner of that AABB — well
    /// clear of the bar itself — must be left untouched (least change), where the old
    /// AABB-proxy push would have shoved it out of a box it never really occupied.
    #[test]
    fn placement_exploits_rotated_courtyard() {
        use super::doc::Orient;
        let lib = bar_lib();
        // BAR at the origin, rotated 45° so its length runs along the +diagonal.
        // DOT parked on the −diagonal corner (2.3 mm, −2.3 mm): inside the bar's ~±2.7 mm
        // AABB, but ~4 mm from the bar's real hull.
        let src = vec![
            GenDirective::Instance {
                path: "bar".into(),
                part: "BAR".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "dot".into(),
                part: "DOT".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "bar".into(),
                pos: Point { x: 0, y: 0 },
            },
            GenDirective::Rotate {
                path: "bar".into(),
                orient: Orient::from_angle_deg(45.0),
            },
            GenDirective::Place {
                path: "dot".into(),
                pos: Point {
                    x: 2_300_000,
                    y: -2_300_000,
                },
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        let dot = pos(h.doc(), "dot");
        // The DOT clears the rotated bar, so least-change leaves it exactly at its anchor.
        assert!(
            dist(
                dot,
                Point {
                    x: 2_300_000,
                    y: -2_300_000
                }
            ) <= PLACE_TOL as f64,
            "DOT in the empty AABB corner must stay put (0019), got {dot:?}"
        );
        assert!(
            h.doc().report.courtyard_overlaps.is_empty(),
            "no residual overlap: {:?}",
            h.doc().report.courtyard_overlaps
        );

        // Negative control: the same DOT dropped on the bar's centre *does* overlap the
        // real hull and is pushed off it.
        let src2 = vec![
            GenDirective::Instance {
                path: "bar".into(),
                part: "BAR".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "dot".into(),
                part: "DOT".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Rotate {
                path: "bar".into(),
                orient: Orient::from_angle_deg(45.0),
            },
            GenDirective::Place {
                path: "bar".into(),
                pos: Point { x: 0, y: 0 },
            },
            GenDirective::Place {
                path: "dot".into(),
                pos: Point { x: 0, y: 0 },
            },
        ];
        let mut h2 = History::new(Default::default());
        h2.commit(Transaction::one(Command::SetSource(src2)), &lib, "s")
            .unwrap();
        let dot2 = pos(h2.doc(), "dot");
        assert!(
            dist(dot2, Point { x: 0, y: 0 }) > PLACE_TOL as f64,
            "DOT on the bar centre must be pushed off, got {dot2:?}"
        );
    }

    /// Honest verify (Decision 10's third leg / issue 0019): two parts *fixed* into
    /// each other cannot be separated by the push, so the final placement still has a
    /// real courtyard overlap — which the verify reports on the true polygons, rather
    /// than pretending the placement is clean.
    #[test]
    fn honest_verify_reports_fixed_courtyard_overlap() {
        let lib = bar_lib();
        let src = vec![
            GenDirective::Instance {
                path: "d1".into(),
                part: "DOT".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "d2".into(),
                part: "DOT".into(),
                params: BTreeMap::new(),
                label: None,
            },
            // Both hard-fixed 0.2 mm apart: courtyards (0.25 mm half + 0.25 mm margin)
            // deeply overlap and neither can move.
            GenDirective::Fix {
                path: "d1".into(),
                pos: Point { x: 0, y: 0 },
            },
            GenDirective::Fix {
                path: "d2".into(),
                pos: Point { x: 200_000, y: 0 },
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        assert_eq!(
            h.doc().report.courtyard_overlaps,
            vec![(EntityId::new("d1"), EntityId::new("d2"))],
            "the fixed overlap must be reported honestly"
        );
    }

    /// Honest verify catches a *small* fixed/fixed overlap, not just a gross one. Two
    /// RND parts (radius-0 courtyards, 0.5 mm half-extent boxes) hard-fixed so their
    /// boxes overlap by 50 µm — below the old `PLACE_TOL` (0.1 mm) gate that would have
    /// silently swallowed it, above the tightened `COURTYARD_VERIFY_TOL` (3 µm). is_clean
    /// must be false.
    #[test]
    fn honest_verify_catches_small_fixed_overlap() {
        let lib = bar_lib();
        // Box half-extent 0.5 mm ⇒ edges touch at 1.0 mm centre spacing; 0.95 mm ⇒ 50 µm
        // overlap.
        let src = vec![
            GenDirective::Instance {
                path: "r1".into(),
                part: "RND".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "r2".into(),
                part: "RND".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Fix {
                path: "r1".into(),
                pos: Point { x: 0, y: 0 },
            },
            GenDirective::Fix {
                path: "r2".into(),
                pos: Point { x: 950_000, y: 0 },
            },
        ];
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        assert_eq!(
            h.doc().report.courtyard_overlaps,
            vec![(EntityId::new("r1"), EntityId::new("r2"))],
            "a 50 µm fixed overlap must be reported, not swallowed"
        );
        assert!(!h.doc().report.is_clean());
    }

    /// Stage-3 / issue 0006: DRC clearance is pad-aware — it sees the real copper
    /// extent of a pad, not its centre point. Two 0.5 mm pads of different nets
    /// 0.6 mm apart (0.1 mm edge gap) clash a 0.15 mm rule; 1 mm apart they clear.
    #[test]
    fn drc_clearance_is_pad_aware() {
        let lib = pad_lib();

        // Separate engines: each History is its own document lineage (same revision
        // numbers), so one memoizing engine across both would return a stale result.
        let mut close = History::new(Default::default());
        close
            .commit(
                Transaction::one(Command::SetSource(two_pads_at(600_000))),
                &lib,
                "close",
            )
            .unwrap();
        let drc = Engine::new().query(close.doc(), &lib, Key::Drc);
        assert!(
            drc.as_drc()
                .iter()
                .any(|v| matches!(v, Violation::Clearance { .. })),
            "fine-pitch different-net pads must clash (pad-aware): {:?}",
            drc.as_drc()
        );

        let mut far = History::new(Default::default());
        far.commit(
            Transaction::one(Command::SetSource(two_pads_at(1_000_000))),
            &lib,
            "far",
        )
        .unwrap();
        let drc = Engine::new().query(far.doc(), &lib, Key::Drc);
        assert!(
            !drc.as_drc()
                .iter()
                .any(|v| matches!(v, Violation::Clearance { .. })),
            "well-separated pads are clearance-clean: {:?}",
            drc.as_drc()
        );
    }

    /// Cascade suppression: a missing instance referenced many times is reported
    /// *once*, so the real fault isn't buried under its downstream noise.
    #[test]
    fn missing_instance_reported_once_cascade_suppressed() {
        let lib = dup_power_lib();
        let mut h = History::new(Default::default());
        let src = vec![
            GenDirective::Instance {
                path: "u1".into(),
                part: "PWRCHIP".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Near {
                a: "ghost".into(),
                b: "u1".into(),
                within: MM,
            },
            GenDirective::ConnectPins {
                net: "a".into(),
                pins: vec![("ghost".into(), "VDD".into()), ("u1".into(), "VDD".into())],
            },
            GenDirective::ConnectPins {
                net: "b".into(),
                pins: vec![("ghost".into(), "GND".into())],
            },
        ];
        let errs = h
            .commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap_err();
        assert_eq!(
            errs.iter()
                .filter(|d| d.code == "E_UNKNOWN_INSTANCE")
                .count(),
            1,
            "missing `ghost` reported once, cascade suppressed: {errs:?}"
        );
    }

    #[test]
    fn override_survives_reelaboration_and_orphans_surface() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(3))),
            &lib,
            "psu3",
        )
        .unwrap();
        // Pin dec[1].
        h.commit(
            Transaction::one(Command::Nudge(
                EntityId::new("psu.dec[1]"),
                Point::mm(42, 7),
            )),
            &lib,
            "pin dec1",
        )
        .unwrap();
        // Grow the design: dec[1] still exists -> override sticks; others keep
        // their generated defaults (minimal perturbation).
        h.commit(
            Transaction::one(Command::SetSource(psu_module(5))),
            &lib,
            "psu5",
        )
        .unwrap();
        let d = h.doc();
        let dec1 = &d.components[&EntityId::new("psu.dec[1]")];
        assert_eq!(dec1.pos.value, Point::mm(42, 7));
        // A nudge is a hint; an effective hint sticks across re-elaboration.
        assert_eq!(dec1.pos.prov, Provenance::Hint);
        assert!(d.report.is_clean());

        // Shrink so dec[1] disappears: the override is orphaned and surfaced.
        h.commit(
            Transaction::one(Command::SetSource(psu_module(1))),
            &lib,
            "psu1",
        )
        .unwrap();
        let d = h.doc();
        assert!(!d.components.contains_key(&EntityId::new("psu.dec[1]")));
        assert!(
            d.report.orphaned.contains(&EntityId::new("psu.dec[1]")),
            "orphan not surfaced"
        );
    }

    // dec[0]'s generated default is (10mm, 0); these tests lean on that.
    fn pin_or_nudge_doc(n: usize) -> History {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(n))),
            &lib,
            "psu",
        )
        .unwrap();
        h
    }

    #[test]
    fn redundant_hint_decays_and_is_collected() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // Nudge dec[0] to exactly its generated default: the hint does nothing.
        h.commit(
            Transaction::one(Command::Nudge(dec0.clone(), Point::mm(10, 0))),
            &lib,
            "noop",
        )
        .unwrap();
        let d = h.doc();
        assert!(
            !d.overrides.contains_key(&dec0),
            "redundant hint should be GC'd"
        );
        assert!(
            d.report
                .decayed
                .iter()
                .any(|(id, r)| *id == dec0 && *r == DecayReason::RedundantWithDefault)
        );
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Free);
    }

    #[test]
    fn hint_yields_to_constraint_and_decays() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // An effective nudge...
        h.commit(
            Transaction::one(Command::Nudge(dec0.clone(), Point::mm(5, 5))),
            &lib,
            "nudge",
        )
        .unwrap();
        assert_eq!(h.doc().components[&dec0].pos.prov, Provenance::Hint);
        // ...then a hard constraint lands on the same part.
        let mut src = psu_module(2);
        src.push(GenDirective::Fix {
            path: "psu.dec[0]".into(),
            pos: Point::mm(8, 8),
        });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix")
            .unwrap();
        let d = h.doc();
        assert_eq!(d.components[&dec0].pos.value, Point::mm(8, 8));
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Fixed);
        assert!(
            !d.overrides.contains_key(&dec0),
            "yielding hint should decay"
        );
        assert!(
            d.report
                .decayed
                .iter()
                .any(|(id, r)| *id == dec0 && *r == DecayReason::OverriddenByConstraint)
        );
    }

    #[test]
    fn pin_conflicts_with_constraint_loudly_and_is_kept() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        h.commit(
            Transaction::one(Command::Pin(dec0.clone(), Point::mm(5, 5))),
            &lib,
            "pin",
        )
        .unwrap();
        let mut src = psu_module(2);
        src.push(GenDirective::Fix {
            path: "psu.dec[0]".into(),
            pos: Point::mm(8, 8),
        });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix")
            .unwrap();
        let d = h.doc();
        // The constraint wins physically, but the pin is kept and the conflict is loud.
        assert_eq!(d.components[&dec0].pos.value, Point::mm(8, 8));
        assert!(d.report.pin_conflicts.contains(&dec0));
        assert!(
            d.overrides.contains_key(&dec0),
            "a conflicting pin must not be silently dropped"
        );
    }

    #[test]
    fn redundant_pin_is_flagged_not_dropped() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // Pin at the default position: does nothing, but is explicit intent.
        h.commit(
            Transaction::one(Command::Pin(dec0.clone(), Point::mm(10, 0))),
            &lib,
            "pin",
        )
        .unwrap();
        let d = h.doc();
        assert!(d.report.redundant_pins.contains(&dec0));
        assert!(
            d.overrides.contains_key(&dec0),
            "a pin is advisory-flagged, never auto-removed"
        );
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Pinned);
    }

    #[test]
    fn undo_restores_previous_version() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(2))),
            &lib,
            "psu2",
        )
        .unwrap();
        let two = h.doc().components.len();
        h.commit(
            Transaction::one(Command::SetSource(psu_module(4))),
            &lib,
            "psu4",
        )
        .unwrap();
        assert!(h.doc().components.len() > two);
        assert!(h.undo());
        assert_eq!(h.doc().components.len(), two);
    }

    // ---- M3: the least-change placement solver ----

    #[test]
    fn unconstrained_parts_do_not_move() {
        // No constraints: the solver leaves everything at its generated default.
        let d = placed(vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "c2".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
        ]);
        assert_eq!(pos(&d, "c1"), Point::mm(10, 0));
        assert_eq!(pos(&d, "c2"), Point::mm(20, 0));
    }

    #[test]
    fn near_pulls_within_bound() {
        let d = placed(vec![
            GenDirective::Instance {
                path: "a".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "b".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Near {
                a: "a".into(),
                b: "b".into(),
                within: 2 * MM,
            },
        ]);
        assert!(dist(pos(&d, "a"), pos(&d, "b")) <= (2 * MM + PLACE_TOL) as f64);
    }

    #[test]
    fn minsep_pushes_apart() {
        let d = placed(vec![
            GenDirective::Instance {
                path: "a".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "b".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "a".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::Place {
                path: "b".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::MinSep {
                a: "a".into(),
                b: "b".into(),
                gap: 5 * MM,
            },
        ]);
        assert!(dist(pos(&d, "a"), pos(&d, "b")) >= (5 * MM - PLACE_TOL) as f64);
    }

    #[test]
    fn board_outline_contains_parts() {
        let d = placed(vec![
            GenDirective::Instance {
                path: "a".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "a".into(),
                pos: Point::mm(100, 0),
            },
            board_rect(Point::mm(0, 0), Point::mm(50, 50)),
        ]);
        assert!(pos(&d, "a").x <= 50 * MM + PLACE_TOL);
    }

    // ---- physical parts: orientation + pin geometry ----

    #[test]
    fn orientation_round_trips_through_elaboration() {
        // A Rotate directive sets the component's orientation attribute, and it
        // survives elaboration (and a re-elaboration via the same source).
        let d = placed(vec![
            GenDirective::Instance {
                path: "u1".into(),
                part: "MCU".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(90).unwrap(),
            },
        ]);
        use super::doc::Orient;
        assert_eq!(
            d.components[&EntityId::new("u1")].orient,
            Orient::from_deg(90).unwrap()
        );
        // Default orientation when no Rotate is given.
        let d0 = placed(vec![GenDirective::Instance {
            path: "u1".into(),
            part: "MCU".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        }]);
        assert_eq!(
            d0.components[&EntityId::new("u1")].orient,
            Orient::from_deg(0).unwrap()
        );
    }

    #[test]
    fn rotate_off_axis_is_accepted_as_a_quaternion() {
        // Stage 2: an arbitrary planar angle is now valid — lowered to an integer
        // quaternion at authoring time, no longer rejected as "off-axis".
        use super::doc::Orient;
        let lib = part_library();
        let mut h = History::new(Default::default());
        let r = h.commit(
            Transaction::one(Command::SetSource(vec![
                GenDirective::Instance {
                    path: "u1".into(),
                    part: "MCU".into(),
                    params: std::collections::BTreeMap::new(),
                    label: None,
                },
                GenDirective::Rotate {
                    path: "u1".into(),
                    orient: Orient::from_angle_deg(45.0),
                },
            ])),
            &lib,
            "off-axis",
        );
        assert!(r.is_ok(), "an off-axis rotation is valid: {r:?}");
        let o = h.doc().components[&EntityId::new("u1")].orient;
        assert_eq!(o.to_deg(), 45, "≈ 45° about z");
        assert!(!o.is_bottom());
    }

    /// Near-to-pin pulls a component onto a *pin's* world position, accounting for
    /// the host component's orientation. reg is fixed at the origin and rotated 90°,
    /// so its VOUT pin (local (2mm,0)) lands at world (0, 2mm); a cap constrained
    /// `nearpin reg.VOUT 0` is dragged there.
    #[test]
    fn near_to_pin_pulls_component_onto_rotated_pin() {
        use super::doc::Orient;
        use super::part::pin_world;
        let d = placed(vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Fix {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::Rotate {
                path: "reg".into(),
                orient: Orient::from_deg(90).unwrap(),
            },
            GenDirective::NearPin {
                a: "dec".into(),
                b_comp: "reg".into(),
                b_pin: "VOUT".into(),
                within: 0,
            },
        ]);
        let lib = part_library();
        let reg = &d.components[&EntityId::new("reg")];
        let pin_pos = pin_world(reg, &lib["LDO"], "VOUT").unwrap();
        assert_eq!(pin_pos, Point::mm(0, 2), "rotated pin world position");
        // dec's centroid is pulled to the pin world position (within solver tol).
        assert!(
            dist(pos(&d, "dec"), pin_pos) <= PLACE_TOL as f64,
            "dec at {:?}, pin at {:?}",
            pos(&d, "dec"),
            pin_pos
        );
        // Component-level Near still works alongside (sanity: reg stays fixed).
        assert_eq!(pos(&d, "reg"), Point::mm(0, 0));
    }

    #[test]
    fn hint_decays_when_solver_would_place_it_there_anyway() {
        // reg is fixed at the origin; dec is constrained to coincide with it.
        // A nudge of dec to the origin is therefore doing nothing -> it decays,
        // even though the value differs from the *row* default. This is the
        // solver-based definition of "ineffective".
        let lib = part_library();
        let mut h = History::new(Default::default());
        let src = vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Fix {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Near {
                a: "dec".into(),
                b: "reg".into(),
                within: 0,
            },
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        let dec = EntityId::new("dec");
        h.commit(
            Transaction::one(Command::Nudge(dec.clone(), Point::mm(0, 0))),
            &lib,
            "nudge",
        )
        .unwrap();
        let d = h.doc();
        assert!(
            d.report
                .decayed
                .iter()
                .any(|(id, r)| *id == dec && *r == DecayReason::RedundantWithDefault)
        );
        assert!(
            !d.overrides.contains_key(&dec),
            "ineffective hint should be GC'd"
        );
        assert!(dist(pos(d, "dec"), Point::mm(0, 0)) <= PLACE_TOL as f64);
    }

    // ---- M4: resolution UX — acting on ReconReport entries ----

    /// psu_module(2) with dec[0] pinned at (5,5), then a hard Fix at (8,8) lands on
    /// dec[0]: the canonical pin-vs-constraint conflict.
    fn pin_conflict_doc() -> History {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        h.commit(
            Transaction::one(Command::Pin(dec0.clone(), Point::mm(5, 5))),
            &lib,
            "pin",
        )
        .unwrap();
        let mut src = psu_module(2);
        src.push(GenDirective::Fix {
            path: "psu.dec[0]".into(),
            pos: Point::mm(8, 8),
        });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix")
            .unwrap();
        assert!(h.doc().report.pin_conflicts.contains(&dec0));
        h
    }

    #[test]
    fn resolve_orphan_drops_dead_override() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(3))),
            &lib,
            "psu3",
        )
        .unwrap();
        let dec1 = EntityId::new("psu.dec[1]");
        h.commit(
            Transaction::one(Command::Pin(dec1.clone(), Point::mm(42, 7))),
            &lib,
            "pin",
        )
        .unwrap();
        // Shrink so dec[1] disappears -> its override is orphaned.
        h.commit(
            Transaction::one(Command::SetSource(psu_module(1))),
            &lib,
            "psu1",
        )
        .unwrap();
        assert!(h.doc().report.orphaned.contains(&dec1));
        assert!(h.doc().overrides.contains_key(&dec1));

        h.commit(
            Transaction::one(Command::Resolve(dec1.clone(), Resolution::DropOrphan)),
            &lib,
            "resolve orphan",
        )
        .unwrap();
        let d = h.doc();
        assert!(
            !d.overrides.contains_key(&dec1),
            "orphaned override should be dropped"
        );
        assert!(
            !d.report.orphaned.contains(&dec1),
            "orphan entry should be gone"
        );
        assert!(
            d.report.is_clean(),
            "report should be clean after resolving the only issue"
        );
    }

    #[test]
    fn accept_constraint_clears_conflicting_pin() {
        let lib = part_library();
        let mut h = pin_conflict_doc();
        let dec0 = EntityId::new("psu.dec[0]");

        h.commit(
            Transaction::one(Command::Resolve(dec0.clone(), Resolution::AcceptConstraint)),
            &lib,
            "accept constraint",
        )
        .unwrap();
        let d = h.doc();
        // Part sits at the Fix position, as Fixed, with no override and a clean report.
        assert_eq!(d.components[&dec0].pos.value, Point::mm(8, 8));
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Fixed);
        assert!(
            !d.overrides.contains_key(&dec0),
            "no pin override should remain"
        );
        assert!(
            d.report.is_clean(),
            "report should be clean after accepting the constraint"
        );
    }

    #[test]
    fn re_pin_moves_pin_and_is_the_users_call() {
        let lib = part_library();
        let mut h = pin_conflict_doc();
        let dec0 = EntityId::new("psu.dec[0]");

        // Re-pin to a position that still differs from the Fix: the pin is kept and
        // moved, the Fix still wins physically, so the conflict deliberately persists.
        h.commit(
            Transaction::one(Command::Resolve(
                dec0.clone(),
                Resolution::RePin(Point::mm(20, 20)),
            )),
            &lib,
            "re-pin",
        )
        .unwrap();
        let d = h.doc();
        let ov = d
            .overrides
            .get(&dec0)
            .expect("re-pinned override should remain");
        assert_eq!(ov.pos, Some(Point::mm(20, 20)));
        assert_eq!(
            d.components[&dec0].pos.value,
            Point::mm(8, 8),
            "Fix still wins"
        );
        assert!(
            d.report.pin_conflicts.contains(&dec0),
            "re-pin onto a non-Fix point still conflicts"
        );

        // Re-pinning onto the Fix point itself makes the pin redundant, not conflicting.
        h.commit(
            Transaction::one(Command::Resolve(
                dec0.clone(),
                Resolution::RePin(Point::mm(8, 8)),
            )),
            &lib,
            "re-pin onto fix",
        )
        .unwrap();
        let d = h.doc();
        assert!(
            !d.report.pin_conflicts.contains(&dec0),
            "no longer conflicting"
        );
        assert!(
            d.report.redundant_pins.contains(&dec0),
            "now redundant with the Fix"
        );
    }

    #[test]
    fn drop_redundant_pin_unpins_it() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // Pin at the default position: explicit but pointless -> flagged redundant.
        h.commit(
            Transaction::one(Command::Pin(dec0.clone(), Point::mm(10, 0))),
            &lib,
            "pin",
        )
        .unwrap();
        assert!(h.doc().report.redundant_pins.contains(&dec0));

        h.commit(
            Transaction::one(Command::Resolve(dec0.clone(), Resolution::DropRedundant)),
            &lib,
            "drop redundant",
        )
        .unwrap();
        let d = h.doc();
        assert!(
            !d.overrides.contains_key(&dec0),
            "redundant pin should be dropped"
        );
        assert!(
            !d.report.redundant_pins.contains(&dec0),
            "redundant entry should be gone"
        );
        assert!(d.report.is_clean());
        // Position is unchanged (it was redundant), now solver-driven.
        assert_eq!(d.components[&dec0].pos.value, Point::mm(10, 0));
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Free);
    }

    #[test]
    fn resolve_validates_against_report_and_is_atomic() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // No outstanding issue: a clean report.
        assert!(h.doc().report.is_clean());
        let before = super::project::render(h.doc());

        // Each resolution rejects an entity its category does not flag; head untouched.
        for res in [
            Resolution::DropOrphan,
            Resolution::AcceptConstraint,
            Resolution::DropRedundant,
        ] {
            let r = h.commit(
                Transaction::one(Command::Resolve(dec0.clone(), res)),
                &lib,
                "bogus resolve",
            );
            assert!(r.is_err(), "resolving a non-issue must fail");
        }
        assert_eq!(
            before,
            super::project::render(h.doc()),
            "failed resolves leave head untouched"
        );
    }

    #[test]
    fn suggested_resolutions_enumerate_and_apply() {
        let lib = part_library();
        let mut h = pin_conflict_doc();
        let dec0 = EntityId::new("psu.dec[0]");

        let sugg = suggested_resolutions(&h.doc().report);
        // A pin conflict offers two routes: accept-constraint (ready) and re-pin (needs input).
        assert_eq!(sugg.len(), 2);
        assert!(sugg.iter().all(|s| s.entity == dec0));
        let ready: Vec<&Command> = sugg.iter().filter_map(|s| s.command.as_ref()).collect();
        assert_eq!(
            ready.len(),
            1,
            "accept-constraint is ready; re-pin needs a position"
        );
        assert!(matches!(
            ready[0],
            Command::Resolve(id, Resolution::AcceptConstraint) if *id == dec0
        ));

        // The suggested command actually clears the issue when committed.
        h.commit(Transaction::one(ready[0].clone()), &lib, "apply suggestion")
            .unwrap();
        assert!(h.doc().report.is_clean());
        assert!(suggested_resolutions(&h.doc().report).is_empty());
    }

    // ---- M5: the convergence-based real solver ----

    /// The motivating case the old fixed-iteration relaxation got wrong: three
    /// decouplers each `Near` a fixed regulator within 6 mm AND pairwise `MinSep`
    /// 3 mm apart. The new solver satisfies every relation to a tight tolerance —
    /// roughly two orders of magnitude tighter than the old ~0.1–0.2 mm.
    #[test]
    fn three_decouplers_near_and_minsep_satisfied_tightly() {
        // 0.01 mm: well inside the solver's 1 µm convergence tolerance (with margin
        // for nm rounding on output) and ~10–20x tighter than the old relaxation.
        const TOL: Nm = 10_000;
        let mut src = vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Fix {
                path: "reg".into(),
                pos: Point::mm(30, 30),
            },
        ];
        for i in 0..3 {
            let d = format!("dec{i}");
            src.push(GenDirective::Instance {
                path: d.clone(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            });
            src.push(GenDirective::Near {
                a: d,
                b: "reg".into(),
                within: 6 * MM,
            });
        }
        src.push(GenDirective::MinSep {
            a: "dec0".into(),
            b: "dec1".into(),
            gap: 3 * MM,
        });
        src.push(GenDirective::MinSep {
            a: "dec1".into(),
            b: "dec2".into(),
            gap: 3 * MM,
        });
        src.push(GenDirective::MinSep {
            a: "dec0".into(),
            b: "dec2".into(),
            gap: 3 * MM,
        });
        let d = placed(src);

        let reg = pos(&d, "reg");
        for i in 0..3 {
            let p = pos(&d, &format!("dec{i}"));
            assert!(
                dist(p, reg) <= (6 * MM + TOL) as f64,
                "dec{i} is {} nm from reg, want <= {}",
                dist(p, reg),
                6 * MM + TOL
            );
        }
        for (a, b) in [("dec0", "dec1"), ("dec1", "dec2"), ("dec0", "dec2")] {
            let sep = dist(pos(&d, a), pos(&d, b));
            assert!(
                sep >= (3 * MM - TOL) as f64,
                "{a}-{b} sep {sep} nm, want >= {}",
                3 * MM - TOL
            );
        }
    }

    /// A genuinely infeasible set is *reported*, not silently approximated: two
    /// immovable (Fixed) nodes 10 mm apart with a `Near 1mm` between them cannot be
    /// reconciled, so the solver returns `!converged` and names the offending
    /// constraint with a non-zero residual.
    #[test]
    fn infeasible_constraints_are_reported() {
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        let mut anchors = BTreeMap::new();
        anchors.insert(a.clone(), Point::mm(0, 0));
        anchors.insert(b.clone(), Point::mm(10, 0));
        let mut fixed = BTreeSet::new();
        fixed.insert(a.clone());
        fixed.insert(b.clone());
        let prob = Problem {
            anchors,
            fixed,
            board: None,
            constraints: vec![Constraint::Near {
                a: a.clone(),
                b: b.clone(),
                within: MM,
            }],
        };
        let sol = solve(&prob);
        assert!(
            !sol.converged,
            "an infeasible set must not report convergence"
        );
        assert_eq!(
            sol.unsatisfied.len(),
            1,
            "the one violated constraint must be listed"
        );
        // Residual ~= 9 mm (10 mm actual − 1 mm allowed), reported, not hidden.
        assert!(
            (sol.unsatisfied[0].residual - 9 * MM).abs() < PLACE_TOL,
            "residual {} nm, want ~{}",
            sol.unsatisfied[0].residual,
            9 * MM
        );
        // Fixed nodes never moved.
        assert_eq!(sol.positions[&a], Point::mm(0, 0));
        assert_eq!(sol.positions[&b], Point::mm(10, 0));
    }

    /// A `MinSep` larger than the board can fit is reported (clamping fights the
    /// separation forever): infeasibility surfaces as the still-violated `MinSep`.
    #[test]
    fn minsep_larger_than_board_is_reported() {
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        let mut anchors = BTreeMap::new();
        anchors.insert(a.clone(), Point::mm(0, 0));
        anchors.insert(b.clone(), Point::mm(1, 0));
        let prob = Problem {
            anchors,
            fixed: BTreeSet::new(),
            board: Some(Shape2D::Area {
                region: shape_to_region(
                    &Shape2D::rect(Point::mm(1, 1), 2 * MM, 2 * MM),
                    DEFAULT_CIRCLE_SEGS,
                ),
            }),
            // Want 20 mm apart inside a 2 mm board: impossible.
            constraints: vec![Constraint::MinSep { a, b, gap: 20 * MM }],
        };
        let sol = solve(&prob);
        assert!(!sol.converged);
        assert_eq!(sol.unsatisfied.len(), 1);
    }

    /// Least change at the solver boundary: a node touched by no constraint stays
    /// *exactly* (bit-for-bit) at its anchor.
    #[test]
    fn unconstrained_node_stays_exactly_at_anchor() {
        let n = EntityId::new("n");
        let mut anchors = BTreeMap::new();
        anchors.insert(
            n.clone(),
            Point {
                x: 12_345_678,
                y: -9_876_543,
            },
        );
        let prob = Problem {
            anchors,
            fixed: BTreeSet::new(),
            board: None,
            constraints: Vec::new(),
        };
        let sol = solve(&prob);
        assert!(sol.converged);
        assert!(sol.unsatisfied.is_empty());
        assert_eq!(
            sol.positions[&n],
            Point {
                x: 12_345_678,
                y: -9_876_543
            }
        );
    }

    /// Determinism: the same `Problem` solved twice yields identical positions,
    /// convergence flag, and iteration count — bit for bit.
    #[test]
    fn solver_is_deterministic() {
        let make = || {
            let reg = EntityId::new("reg");
            let mut anchors = BTreeMap::new();
            anchors.insert(reg.clone(), Point::mm(30, 30));
            let mut fixed = BTreeSet::new();
            fixed.insert(reg.clone());
            let mut constraints = Vec::new();
            for i in 0..3 {
                let d = EntityId::new(format!("dec{i}"));
                anchors.insert(d.clone(), Point::mm(10 * (i as i64 + 1), 0));
                constraints.push(Constraint::Near {
                    a: d,
                    b: reg.clone(),
                    within: 6 * MM,
                });
            }
            constraints.push(Constraint::MinSep {
                a: EntityId::new("dec0"),
                b: EntityId::new("dec1"),
                gap: 3 * MM,
            });
            constraints.push(Constraint::MinSep {
                a: EntityId::new("dec1"),
                b: EntityId::new("dec2"),
                gap: 3 * MM,
            });
            constraints.push(Constraint::MinSep {
                a: EntityId::new("dec0"),
                b: EntityId::new("dec2"),
                gap: 3 * MM,
            });
            Problem {
                anchors,
                fixed,
                board: None,
                constraints,
            }
        };
        let s1 = solve(&make());
        let s2 = solve(&make());
        assert_eq!(s1.positions, s2.positions);
        assert_eq!(s1.converged, s2.converged);
        assert_eq!(s1.iters, s2.iters);
        assert!(s1.converged, "the feasible 3-decoupler set must converge");
    }

    // ---- routing-core: trace/via representation + DRC ----

    const W: Nm = 200_000; // 0.2 mm, comfortably above the 0.15 mm width rule.

    /// reg(LDO)@(0,0) and dec(Cap)@(10,0), VOUT and p1 joined on net VBUS. Pin
    /// world positions follow from the part library: reg.VOUT = (2mm,0) (LDO VOUT
    /// offset +2mm), dec.p1 = (9mm,0) (Cap p1 offset -1mm). A trace from (2,0) to
    /// (9,0) therefore lands on both pads.
    fn two_pin_design() -> Source {
        vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::Place {
                path: "dec".into(),
                pos: Point::mm(10, 0),
            },
            GenDirective::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("reg".into(), "VOUT".into()), ("dec".into(), "p1".into())],
            },
        ]
    }

    fn routed(src: Source) -> History {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "src")
            .unwrap();
        h
    }

    fn trace(net: &str, layer: &str, path: Vec<Point>, width: Nm) -> Trace {
        Trace {
            net: NetId::new(net),
            layer: layer.to_string(),
            path,
            width,
            prov: Provenance::Pinned,
        }
    }

    /// A correctly hand-routed two-pin net passes DRC clean.
    #[test]
    fn drc_clean_on_routed_two_pin_net() {
        let lib = part_library();
        let mut h = routed(two_pin_design());
        let t = trace("VBUS", "F.Cu", vec![Point::mm(2, 0), Point::mm(9, 0)], W);
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "route",
        )
        .unwrap();
        let mut eng = Engine::new();
        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert!(
            v.as_drc().is_empty(),
            "clean route should pass: {:?}",
            v.as_drc()
        );
    }

    /// An unrouted net is flagged by the ratsnest check (its two pins form two
    /// disconnected islands).
    #[test]
    fn drc_flags_unrouted_net() {
        let lib = part_library();
        let h = routed(two_pin_design());
        let mut eng = Engine::new();
        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert_eq!(
            v.as_drc(),
            &[Violation::Unrouted {
                net: NetId::new("VBUS"),
                islands: 2
            }]
        );
    }

    /// Two traces on different nets, same layer, closer than the clearance rule.
    #[test]
    fn drc_catches_clearance() {
        let lib = part_library();
        // Two single-pin nets (ratsnest trivially satisfied) so only clearance fires.
        let src = vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Place {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::ConnectPins {
                net: "A".into(),
                pins: vec![("reg".into(), "VOUT".into())],
            },
            GenDirective::ConnectPins {
                net: "B".into(),
                pins: vec![("reg".into(), "GND".into())],
            },
        ];
        let mut h = routed(src);
        // Parallel 0.2mm-wide traces 0.1mm apart: 0.1mm < 0.15 + 0.1 + 0.1 = 0.35mm.
        let a = trace("A", "F.Cu", vec![Point::mm(0, 0), Point::mm(10, 0)], W);
        let b = trace(
            "B",
            "F.Cu",
            vec![
                Point { x: 0, y: MM / 10 },
                Point {
                    x: 10 * MM,
                    y: MM / 10,
                },
            ],
            W,
        );
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), a)),
            &lib,
            "a",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(2), b)),
            &lib,
            "b",
        )
        .unwrap();
        let mut eng = Engine::new();
        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert_eq!(
            v.as_drc(),
            &[Violation::Clearance {
                a: NetId::new("A"),
                b: NetId::new("B"),
                layer: "F.Cu".to_string()
            }]
        );
    }

    /// A trace narrower than the minimum width rule is caught (and still routes the
    /// net, so the ratsnest stays clean — only the width fires).
    #[test]
    fn drc_catches_min_width() {
        let lib = part_library();
        let mut h = routed(two_pin_design());
        let t = trace(
            "VBUS",
            "F.Cu",
            vec![Point::mm(2, 0), Point::mm(9, 0)],
            MM / 10,
        ); // 0.1mm
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "thin",
        )
        .unwrap();
        let mut eng = Engine::new();
        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert_eq!(
            v.as_drc(),
            &[Violation::MinWidth {
                trace: TraceId(1),
                width: MM / 10
            }]
        );
    }

    /// A two-layer route joined by a via passes the ratsnest: the via unions copper
    /// across the layers it spans (bonus — exercises vias + multilayer).
    #[test]
    fn drc_via_joins_two_layers() {
        let lib = part_library();
        let mut h = routed(two_pin_design());
        // Top trace pad->via, bottom trace via->pad, via bridging Top..Bottom at (5,0).
        let top = trace("VBUS", "F.Cu", vec![Point::mm(2, 0), Point::mm(5, 0)], W);
        let bot = trace("VBUS", "B.Cu", vec![Point::mm(5, 0), Point::mm(9, 0)], W);
        let via = Via {
            net: NetId::new("VBUS"),
            at: Point::mm(5, 0),
            span: None,
            drill: 300_000,
            pad: 600_000,
            prov: Provenance::Pinned,
        };
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), top)),
            &lib,
            "top",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(2), bot)),
            &lib,
            "bot",
        )
        .unwrap();
        let mut eng = Engine::new();
        // Without the via the two layers are disconnected: ratsnest fails.
        assert!(!eng.query(h.doc(), &lib, Key::Drc).as_drc().is_empty());
        h.commit(
            Transaction::one(Command::AddVia(ViaId(1), via)),
            &lib,
            "via",
        )
        .unwrap();
        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert!(
            v.as_drc().is_empty(),
            "via should bridge the layers: {:?}",
            v.as_drc()
        );
    }

    /// Adding a trace bumps `route_rev` (and only that), and re-runs DRC — turning
    /// an unrouted net clean.
    #[test]
    fn add_trace_bumps_route_rev_and_reruns_drc() {
        let lib = part_library();
        let mut h = routed(two_pin_design());
        let mut eng = Engine::new();
        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert!(!v.as_drc().is_empty(), "unrouted net should flag");
        let d0 = eng.count(Key::Drc);
        let (conn0, geom0, route0) = (h.doc().conn_rev, h.doc().geom_rev, h.doc().route_rev);

        let t = trace("VBUS", "F.Cu", vec![Point::mm(2, 0), Point::mm(9, 0)], W);
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "route",
        )
        .unwrap();
        // Only the routing input moved.
        assert!(h.doc().route_rev > route0, "route_rev must bump");
        assert_eq!(
            h.doc().conn_rev,
            conn0,
            "a route edit must not bump conn_rev"
        );
        assert_eq!(
            h.doc().geom_rev,
            geom0,
            "a route edit must not bump geom_rev"
        );

        let v = eng.query(h.doc(), &lib, Key::Drc);
        assert!(v.as_drc().is_empty(), "now routed: {:?}", v.as_drc());
        assert_eq!(
            eng.count(Key::Drc),
            d0 + 1,
            "DRC must recompute after a route edit"
        );
    }

    /// A routing edit re-runs DRC but does NOT touch ERC/Netlist: the new routing
    /// input is isolated to the queries that read it.
    #[test]
    fn routing_edit_does_not_recompute_erc() {
        let lib = part_library();
        let mut h = routed(two_pin_design());
        let mut eng = Engine::new();
        eng.query(h.doc(), &lib, Key::Erc);
        eng.query(h.doc(), &lib, Key::Drc);
        let (e0, n0) = (eng.count(Key::Erc), eng.count(Key::Netlist));

        let t = trace("VBUS", "F.Cu", vec![Point::mm(2, 0), Point::mm(9, 0)], W);
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "route",
        )
        .unwrap();
        eng.query(h.doc(), &lib, Key::Erc);
        eng.query(h.doc(), &lib, Key::Drc);
        assert_eq!(eng.count(Key::Erc), e0, "a route edit must not re-run ERC");
        assert_eq!(
            eng.count(Key::Netlist),
            n0,
            "a route edit must not re-run Netlist"
        );
    }

    /// A non-routing edit whose resolved netlist is unchanged does NOT recompute
    /// DRC: it is firewalled by early cutoff through the Netlist dependency, while
    /// the geometry and routing inputs are untouched. (Mirrors the ERC early-cutoff
    /// test.) Here an *unconnected* spare's part type is swapped Cap→LDO: this bumps
    /// conn_rev (the component set's shapes changed) so Netlist re-runs, but its
    /// value is identical, its position is unchanged, and no copper moved.
    #[test]
    fn non_routing_edit_does_not_recompute_drc() {
        let lib = part_library();
        let spare = |part: &str| {
            let mut s = two_pin_design();
            s.push(GenDirective::Instance {
                path: "spare".into(),
                part: part.into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            });
            s
        };
        let mut h = routed(spare("Cap"));
        let t = trace("VBUS", "F.Cu", vec![Point::mm(2, 0), Point::mm(9, 0)], W);
        h.commit(
            Transaction::one(Command::AddTrace(TraceId(1), t)),
            &lib,
            "route",
        )
        .unwrap();
        let mut eng = Engine::new();
        eng.query(h.doc(), &lib, Key::Drc);
        let (n0, d0) = (eng.count(Key::Netlist), eng.count(Key::Drc));
        let (geom0, route0) = (h.doc().geom_rev, h.doc().route_rev);

        // Swap the unconnected spare's part type: connectivity-affecting (shape), but
        // geometry- and routing-neutral, and netlist-value-neutral.
        h.commit(
            Transaction::one(Command::SetSource(spare("LDO"))),
            &lib,
            "swap",
        )
        .unwrap();
        assert_eq!(h.doc().geom_rev, geom0, "swap must not move geometry");
        assert_eq!(h.doc().route_rev, route0, "swap must not change routing");

        eng.query(h.doc(), &lib, Key::Drc);
        assert_eq!(
            eng.count(Key::Netlist),
            n0 + 1,
            "Netlist re-runs (conn_rev bumped)"
        );
        assert_eq!(
            eng.count(Key::Drc),
            d0,
            "DRC must be cut off (its result is unchanged)"
        );
    }

    /// Routing commands are validated and atomic: an unknown net, a degenerate
    /// polyline, and removing an absent trace all abort without mutating state.
    #[test]
    fn routing_commands_validate_atomically() {
        let lib = part_library();
        let mut h = routed(two_pin_design());
        let before = h.doc().traces.len();
        // Unknown net.
        let bad = trace("NOPE", "F.Cu", vec![Point::mm(0, 0), Point::mm(1, 0)], W);
        assert!(
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), bad)),
                &lib,
                "x"
            )
            .is_err()
        );
        // Degenerate polyline.
        let stub = trace("VBUS", "F.Cu", vec![Point::mm(0, 0)], W);
        assert!(
            h.commit(
                Transaction::one(Command::AddTrace(TraceId(1), stub)),
                &lib,
                "x"
            )
            .is_err()
        );
        // Remove a trace that does not exist.
        assert!(
            h.commit(
                Transaction::one(Command::RemoveTrace(TraceId(9))),
                &lib,
                "x"
            )
            .is_err()
        );
        assert_eq!(
            h.doc().traces.len(),
            before,
            "no failed command mutated the doc"
        );
    }
}
