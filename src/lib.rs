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
//! - `text` — canonical serializer + parser for tier-1 truth (the text front-end).
//! - `export` — deterministic output artifacts (netlist / pick-and-place / SVG).

pub mod command;
pub mod doc;
pub mod elaborate;
pub mod export;
pub mod history;
pub mod id;
pub mod kicad;
pub mod part;
pub mod project;
pub mod query;
pub mod solve;
pub mod text;

/// Build a root document from a generative source by elaborating it once.
pub fn boot(source: elaborate::Source, lib: &part::PartLib) -> Result<doc::Doc, String> {
    let mut h = history::History::new(doc::Doc::default());
    h.commit(command::Transaction::one(command::Command::SetSource(source)), lib, "boot")?;
    Ok(h.doc().clone())
}

#[cfg(test)]
mod tests {
    use super::command::{suggested_resolutions, Command, Resolution, Transaction};
    use super::doc::{DecayReason, Doc, Nm, Point, Provenance, MM};
    use super::elaborate::{psu_module, GenDirective, Source};
    use super::history::History;
    use super::id::EntityId;
    use super::part::part_library;
    use super::query::{Engine, Key};
    use super::solve::{dist, solve, Constraint, Problem, Rect, PLACE_TOL};
    use std::collections::{BTreeMap, BTreeSet};

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

    // ---- physical parts: orientation + pin geometry ----

    #[test]
    fn orientation_round_trips_through_elaboration() {
        // A Rotate directive sets the component's orientation attribute, and it
        // survives elaboration (and a re-elaboration via the same source).
        let d = placed(vec![
            GenDirective::Instance { path: "u1".into(), part: "MCU".into() },
            GenDirective::Rotate { path: "u1".into(), deg: 90 },
        ]);
        use super::doc::Orient;
        assert_eq!(d.components[&EntityId::new("u1")].orient, Orient::Deg90);
        // Default orientation when no Rotate is given.
        let d0 = placed(vec![GenDirective::Instance { path: "u1".into(), part: "MCU".into() }]);
        assert_eq!(d0.components[&EntityId::new("u1")].orient, Orient::Deg0);
    }

    #[test]
    fn rotate_off_axis_is_rejected() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        let r = h.commit(
            Transaction::one(Command::SetSource(vec![
                GenDirective::Instance { path: "u1".into(), part: "MCU".into() },
                GenDirective::Rotate { path: "u1".into(), deg: 45 },
            ])),
            &lib,
            "bad-rot",
        );
        assert!(r.is_err(), "off-axis rotation must abort the transaction");
    }

    /// Near-to-pin pulls a component onto a *pin's* world position, accounting for
    /// the host component's orientation. reg is fixed at the origin and rotated 90°,
    /// so its VOUT pin (local (2mm,0)) lands at world (0, 2mm); a cap constrained
    /// `nearpin reg.VOUT 0` is dragged there.
    #[test]
    fn near_to_pin_pulls_component_onto_rotated_pin() {
        use super::part::pin_world;
        let d = placed(vec![
            GenDirective::Instance { path: "reg".into(), part: "LDO".into() },
            GenDirective::Instance { path: "dec".into(), part: "Cap".into() },
            GenDirective::Fix { path: "reg".into(), pos: Point::mm(0, 0) },
            GenDirective::Rotate { path: "reg".into(), deg: 90 },
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

    // ---- M4: resolution UX — acting on ReconReport entries ----

    /// psu_module(2) with dec[0] pinned at (5,5), then a hard Fix at (8,8) lands on
    /// dec[0]: the canonical pin-vs-constraint conflict.
    fn pin_conflict_doc() -> History {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        h.commit(Transaction::one(Command::Pin(dec0.clone(), Point::mm(5, 5))), &lib, "pin")
            .unwrap();
        let mut src = psu_module(2);
        src.push(GenDirective::Fix { path: "psu.dec[0]".into(), pos: Point::mm(8, 8) });
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix").unwrap();
        assert!(h.doc().report.pin_conflicts.contains(&dec0));
        h
    }

    #[test]
    fn resolve_orphan_drops_dead_override() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(3))), &lib, "psu3").unwrap();
        let dec1 = EntityId::new("psu.dec[1]");
        h.commit(Transaction::one(Command::Pin(dec1.clone(), Point::mm(42, 7))), &lib, "pin")
            .unwrap();
        // Shrink so dec[1] disappears -> its override is orphaned.
        h.commit(Transaction::one(Command::SetSource(psu_module(1))), &lib, "psu1").unwrap();
        assert!(h.doc().report.orphaned.contains(&dec1));
        assert!(h.doc().overrides.contains_key(&dec1));

        h.commit(
            Transaction::one(Command::Resolve(dec1.clone(), Resolution::DropOrphan)),
            &lib,
            "resolve orphan",
        )
        .unwrap();
        let d = h.doc();
        assert!(!d.overrides.contains_key(&dec1), "orphaned override should be dropped");
        assert!(!d.report.orphaned.contains(&dec1), "orphan entry should be gone");
        assert!(d.report.is_clean(), "report should be clean after resolving the only issue");
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
        assert!(!d.overrides.contains_key(&dec0), "no pin override should remain");
        assert!(d.report.is_clean(), "report should be clean after accepting the constraint");
    }

    #[test]
    fn re_pin_moves_pin_and_is_the_users_call() {
        let lib = part_library();
        let mut h = pin_conflict_doc();
        let dec0 = EntityId::new("psu.dec[0]");

        // Re-pin to a position that still differs from the Fix: the pin is kept and
        // moved, the Fix still wins physically, so the conflict deliberately persists.
        h.commit(
            Transaction::one(Command::Resolve(dec0.clone(), Resolution::RePin(Point::mm(20, 20)))),
            &lib,
            "re-pin",
        )
        .unwrap();
        let d = h.doc();
        let ov = d.overrides.get(&dec0).expect("re-pinned override should remain");
        assert_eq!(ov.pos, Some(Point::mm(20, 20)));
        assert_eq!(d.components[&dec0].pos.value, Point::mm(8, 8), "Fix still wins");
        assert!(d.report.pin_conflicts.contains(&dec0), "re-pin onto a non-Fix point still conflicts");

        // Re-pinning onto the Fix point itself makes the pin redundant, not conflicting.
        h.commit(
            Transaction::one(Command::Resolve(dec0.clone(), Resolution::RePin(Point::mm(8, 8)))),
            &lib,
            "re-pin onto fix",
        )
        .unwrap();
        let d = h.doc();
        assert!(!d.report.pin_conflicts.contains(&dec0), "no longer conflicting");
        assert!(d.report.redundant_pins.contains(&dec0), "now redundant with the Fix");
    }

    #[test]
    fn drop_redundant_pin_unpins_it() {
        let lib = part_library();
        let mut h = pin_or_nudge_doc(2);
        let dec0 = EntityId::new("psu.dec[0]");
        // Pin at the default position: explicit but pointless -> flagged redundant.
        h.commit(Transaction::one(Command::Pin(dec0.clone(), Point::mm(10, 0))), &lib, "pin")
            .unwrap();
        assert!(h.doc().report.redundant_pins.contains(&dec0));

        h.commit(
            Transaction::one(Command::Resolve(dec0.clone(), Resolution::DropRedundant)),
            &lib,
            "drop redundant",
        )
        .unwrap();
        let d = h.doc();
        assert!(!d.overrides.contains_key(&dec0), "redundant pin should be dropped");
        assert!(!d.report.redundant_pins.contains(&dec0), "redundant entry should be gone");
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
        for res in [Resolution::DropOrphan, Resolution::AcceptConstraint, Resolution::DropRedundant]
        {
            let r = h.commit(
                Transaction::one(Command::Resolve(dec0.clone(), res)),
                &lib,
                "bogus resolve",
            );
            assert!(r.is_err(), "resolving a non-issue must fail");
        }
        assert_eq!(before, super::project::render(h.doc()), "failed resolves leave head untouched");
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
        assert_eq!(ready.len(), 1, "accept-constraint is ready; re-pin needs a position");
        assert!(matches!(
            ready[0],
            Command::Resolve(id, Resolution::AcceptConstraint) if *id == dec0
        ));

        // The suggested command actually clears the issue when committed.
        h.commit(Transaction::one(ready[0].clone()), &lib, "apply suggestion").unwrap();
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
            GenDirective::Instance { path: "reg".into(), part: "LDO".into() },
            GenDirective::Fix { path: "reg".into(), pos: Point::mm(30, 30) },
        ];
        for i in 0..3 {
            let d = format!("dec{i}");
            src.push(GenDirective::Instance { path: d.clone(), part: "Cap".into() });
            src.push(GenDirective::Near { a: d, b: "reg".into(), within: 6 * MM });
        }
        src.push(GenDirective::MinSep { a: "dec0".into(), b: "dec1".into(), gap: 3 * MM });
        src.push(GenDirective::MinSep { a: "dec1".into(), b: "dec2".into(), gap: 3 * MM });
        src.push(GenDirective::MinSep { a: "dec0".into(), b: "dec2".into(), gap: 3 * MM });
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
            assert!(sep >= (3 * MM - TOL) as f64, "{a}-{b} sep {sep} nm, want >= {}", 3 * MM - TOL);
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
            constraints: vec![Constraint::Near { a: a.clone(), b: b.clone(), within: MM }],
        };
        let sol = solve(&prob);
        assert!(!sol.converged, "an infeasible set must not report convergence");
        assert_eq!(sol.unsatisfied.len(), 1, "the one violated constraint must be listed");
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
            board: Some(Rect { min: Point::mm(0, 0), max: Point::mm(2, 2) }),
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
        anchors.insert(n.clone(), Point { x: 12_345_678, y: -9_876_543 });
        let prob =
            Problem { anchors, fixed: BTreeSet::new(), board: None, constraints: Vec::new() };
        let sol = solve(&prob);
        assert!(sol.converged);
        assert!(sol.unsatisfied.is_empty());
        assert_eq!(sol.positions[&n], Point { x: 12_345_678, y: -9_876_543 });
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
                constraints.push(Constraint::Near { a: d, b: reg.clone(), within: 6 * MM });
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
            Problem { anchors, fixed, board: None, constraints }
        };
        let s1 = solve(&make());
        let s2 = solve(&make());
        assert_eq!(s1.positions, s2.positions);
        assert_eq!(s1.converged, s2.converged);
        assert_eq!(s1.iters, s2.iters);
        assert!(s1.converged, "the feasible 3-decoupler set must converge");
    }
}
