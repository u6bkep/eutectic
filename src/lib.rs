//! ecad-core — M1 engine prototype.
//!
//! A vertical slice of the architecture in docs/architecture.md:
//!
//! - `doc` — the immutable three-tier document (source/overrides -> materialized
//!   instances/nets; derived tier lives in `query`).
//! - `command` — the sole mutation surface: atomic transactions.
//! - `history` — the version DAG (undo / branch / replay).
//! - `query` — hand-rolled incremental query engine (Netlist, ERC).
//! - `elaborate` — generative source -> instances + ID-keyed override reconcile.
//! - `part` — typed pins & interfaces (makes the serial swap unrepresentable).
//! - `project` — deterministic text projection (agent/git view).

pub mod command;
pub mod doc;
pub mod elaborate;
pub mod history;
pub mod id;
pub mod part;
pub mod project;
pub mod query;
pub mod solve;

/// Build a root document from a generative source by elaborating it once.
pub fn boot(source: elaborate::Source, lib: &part::PartLib) -> Result<doc::Doc, String> {
    let mut h = history::History::new(doc::Doc::default());
    h.commit(command::Transaction::one(command::Command::SetSource(source)), lib, "boot")?;
    Ok(h.doc().clone())
}

#[cfg(test)]
mod tests {
    use super::command::{Command, Transaction};
    use super::doc::{DecayReason, Doc, Point, Provenance, MM};
    use super::elaborate::{psu_module, GenDirective, Source};
    use super::history::History;
    use super::id::EntityId;
    use super::part::part_library;
    use super::query::{Engine, Key};
    use super::solve::{dist, PLACE_TOL};

    fn placed(src: Source) -> Doc {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s").unwrap();
        h.doc().clone()
    }
    fn pos(d: &Doc, id: &str) -> Point {
        d.components[&EntityId::new(id)].pos.value
    }

    fn uart_link() -> Source {
        vec![
            GenDirective::Instance { path: "mcu".into(), part: "MCU".into() },
            GenDirective::Instance { path: "sens".into(), part: "Sensor".into() },
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
        h.commit(Transaction::one(Command::SetSource(uart_link())), &lib, "uart").unwrap();
        let mut eng = Engine::new();
        let nl = eng.query(h.doc(), &lib, Key::Netlist);
        let nl = nl.as_netlist();
        // The net carrying mcu.uart.tx must also carry sens.uart.rx (crossed),
        // never sens.uart.tx. The swap is not expressible.
        let tx_net = nl
            .iter()
            .find(|(_, pins)| {
                pins.iter().any(|(p, _)| p.pin == "uart.tx" && p.comp.as_str() == "mcu")
            })
            .expect("tx net");
        let names: Vec<String> =
            tx_net.1.iter().map(|(p, _)| format!("{}.{}", p.comp, p.pin)).collect();
        assert!(names.contains(&"sens.uart.rx".to_string()), "got {names:?}");
        assert!(!names.contains(&"sens.uart.tx".to_string()));
    }

    #[test]
    fn transaction_is_atomic_on_error() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(2))), &lib, "psu").unwrap();
        let before = super::project::render(h.doc());
        // A source referencing an unknown part must fail and leave head untouched.
        let bad = vec![GenDirective::Instance { path: "x".into(), part: "Nope".into() }];
        let r = h.commit(Transaction::one(Command::SetSource(bad)), &lib, "bad");
        assert!(r.is_err());
        assert_eq!(before, super::project::render(h.doc()));
    }

    #[test]
    fn nudge_skips_both_queries_geometry_only() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(2))), &lib, "psu").unwrap();
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
        assert_eq!(eng.count(Key::Netlist), n0, "netlist must not recompute on a nudge");
        assert_eq!(eng.count(Key::Erc), e0, "erc must not recompute on a nudge");
    }

    #[test]
    fn early_cutoff_skips_erc_when_netlist_value_unchanged() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(2))), &lib, "psu").unwrap();
        let mut eng = Engine::new();
        eng.query(h.doc(), &lib, Key::Erc);
        let (n0, e0) = (eng.count(Key::Netlist), eng.count(Key::Erc));

        // Add an *unconnected* component: bumps connectivity (component set
        // changed) so Netlist recomputes, but the resolved netlist value is
        // identical -> ERC must be skipped by early cutoff.
        let mut src = psu_module(2);
        src.push(GenDirective::Instance { path: "psu.spare".into(), part: "Cap".into() });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "spare").unwrap();
        eng.query(h.doc(), &lib, Key::Erc);
        assert_eq!(eng.count(Key::Netlist), n0 + 1, "netlist should recompute");
        assert_eq!(eng.count(Key::Erc), e0, "erc should be cut off");
    }

    #[test]
    fn override_survives_reelaboration_and_orphans_surface() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(3))), &lib, "psu3").unwrap();
        // Pin dec[1].
        h.commit(
            Transaction::one(Command::Nudge(EntityId::new("psu.dec[1]"), Point::mm(42, 7))),
            &lib,
            "pin dec1",
        )
        .unwrap();
        // Grow the design: dec[1] still exists -> override sticks; others keep
        // their generated defaults (minimal perturbation).
        h.commit(Transaction::one(Command::SetSource(psu_module(5))), &lib, "psu5").unwrap();
        let d = h.doc();
        let dec1 = &d.components[&EntityId::new("psu.dec[1]")];
        assert_eq!(dec1.pos.value, Point::mm(42, 7));
        // A nudge is a hint; an effective hint sticks across re-elaboration.
        assert_eq!(dec1.pos.prov, Provenance::Hint);
        assert!(d.report.is_clean());

        // Shrink so dec[1] disappears: the override is orphaned and surfaced.
        h.commit(Transaction::one(Command::SetSource(psu_module(1))), &lib, "psu1").unwrap();
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
        h.commit(Transaction::one(Command::SetSource(psu_module(n))), &lib, "psu").unwrap();
        h
    }

    #[test]
    fn redundant_hint_decays_and_is_collected() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // Nudge dec[0] to exactly its generated default: the hint does nothing.
        h.commit(Transaction::one(Command::Nudge(dec0.clone(), Point::mm(10, 0))), &lib, "noop")
            .unwrap();
        let d = h.doc();
        assert!(!d.overrides.contains_key(&dec0), "redundant hint should be GC'd");
        assert!(d.report.decayed.iter().any(|(id, r)| *id == dec0
            && *r == DecayReason::RedundantWithDefault));
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Free);
    }

    #[test]
    fn hint_yields_to_constraint_and_decays() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // An effective nudge...
        h.commit(Transaction::one(Command::Nudge(dec0.clone(), Point::mm(5, 5))), &lib, "nudge")
            .unwrap();
        assert_eq!(h.doc().components[&dec0].pos.prov, Provenance::Hint);
        // ...then a hard constraint lands on the same part.
        let mut src = psu_module(2);
        src.push(GenDirective::Fix { path: "psu.dec[0]".into(), pos: Point::mm(8, 8) });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix").unwrap();
        let d = h.doc();
        assert_eq!(d.components[&dec0].pos.value, Point::mm(8, 8));
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Fixed);
        assert!(!d.overrides.contains_key(&dec0), "yielding hint should decay");
        assert!(d.report.decayed.iter().any(|(id, r)| *id == dec0
            && *r == DecayReason::OverriddenByConstraint));
    }

    #[test]
    fn pin_conflicts_with_constraint_loudly_and_is_kept() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        h.commit(Transaction::one(Command::Pin(dec0.clone(), Point::mm(5, 5))), &lib, "pin")
            .unwrap();
        let mut src = psu_module(2);
        src.push(GenDirective::Fix { path: "psu.dec[0]".into(), pos: Point::mm(8, 8) });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix").unwrap();
        let d = h.doc();
        // The constraint wins physically, but the pin is kept and the conflict is loud.
        assert_eq!(d.components[&dec0].pos.value, Point::mm(8, 8));
        assert!(d.report.pin_conflicts.contains(&dec0));
        assert!(d.overrides.contains_key(&dec0), "a conflicting pin must not be silently dropped");
    }

    #[test]
    fn redundant_pin_is_flagged_not_dropped() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // Pin at the default position: does nothing, but is explicit intent.
        h.commit(Transaction::one(Command::Pin(dec0.clone(), Point::mm(10, 0))), &lib, "pin")
            .unwrap();
        let d = h.doc();
        assert!(d.report.redundant_pins.contains(&dec0));
        assert!(d.overrides.contains_key(&dec0), "a pin is advisory-flagged, never auto-removed");
        assert_eq!(d.components[&dec0].pos.prov, Provenance::Pinned);
    }

    #[test]
    fn undo_restores_previous_version() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(2))), &lib, "psu2").unwrap();
        let two = h.doc().components.len();
        h.commit(Transaction::one(Command::SetSource(psu_module(4))), &lib, "psu4").unwrap();
        assert!(h.doc().components.len() > two);
        assert!(h.undo());
        assert_eq!(h.doc().components.len(), two);
    }

    // ---- M3: the least-change placement solver ----

    #[test]
    fn unconstrained_parts_do_not_move() {
        // No constraints: the solver leaves everything at its generated default.
        let d = placed(vec![
            GenDirective::Instance { path: "reg".into(), part: "LDO".into() },
            GenDirective::Instance { path: "c1".into(), part: "Cap".into() },
            GenDirective::Instance { path: "c2".into(), part: "Cap".into() },
        ]);
        assert_eq!(pos(&d, "c1"), Point::mm(10, 0));
        assert_eq!(pos(&d, "c2"), Point::mm(20, 0));
    }

    #[test]
    fn near_pulls_within_bound() {
        let d = placed(vec![
            GenDirective::Instance { path: "a".into(), part: "LDO".into() },
            GenDirective::Instance { path: "b".into(), part: "Cap".into() },
            GenDirective::Near { a: "a".into(), b: "b".into(), within: 2 * MM },
        ]);
        assert!(dist(pos(&d, "a"), pos(&d, "b")) <= (2 * MM + PLACE_TOL) as f64);
    }

    #[test]
    fn minsep_pushes_apart() {
        let d = placed(vec![
            GenDirective::Instance { path: "a".into(), part: "LDO".into() },
            GenDirective::Instance { path: "b".into(), part: "Cap".into() },
            GenDirective::Place { path: "a".into(), pos: Point::mm(0, 0) },
            GenDirective::Place { path: "b".into(), pos: Point::mm(0, 0) },
            GenDirective::MinSep { a: "a".into(), b: "b".into(), gap: 5 * MM },
        ]);
        assert!(dist(pos(&d, "a"), pos(&d, "b")) >= (5 * MM - PLACE_TOL) as f64);
    }

    #[test]
    fn board_outline_contains_parts() {
        let d = placed(vec![
            GenDirective::Instance { path: "a".into(), part: "LDO".into() },
            GenDirective::Place { path: "a".into(), pos: Point::mm(100, 0) },
            GenDirective::Board { min: Point::mm(0, 0), max: Point::mm(50, 50) },
        ]);
        assert!(pos(&d, "a").x <= 50 * MM + PLACE_TOL);
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
            GenDirective::Instance { path: "reg".into(), part: "LDO".into() },
            GenDirective::Fix { path: "reg".into(), pos: Point::mm(0, 0) },
            GenDirective::Instance { path: "dec".into(), part: "Cap".into() },
            GenDirective::Near { a: "dec".into(), b: "reg".into(), within: 0 },
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s").unwrap();
        let dec = EntityId::new("dec");
        h.commit(Transaction::one(Command::Nudge(dec.clone(), Point::mm(0, 0))), &lib, "nudge")
            .unwrap();
        let d = h.doc();
        assert!(d.report.decayed.iter().any(|(id, r)| *id == dec
            && *r == DecayReason::RedundantWithDefault));
        assert!(!d.overrides.contains_key(&dec), "ineffective hint should be GC'd");
        assert!(dist(pos(d, "dec"), Point::mm(0, 0)) <= PLACE_TOL as f64);
    }
}
