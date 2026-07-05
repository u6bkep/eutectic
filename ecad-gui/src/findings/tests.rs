//! Findings projection tests (m5).

use super::*;
use crate::canvas::pick::candidates;
use crate::fixtures::{board_domain, drc_violation_domain};

/// Compute the findings over a domain fixture (the doc + its board pick candidates +
/// the load's library-resolution notes).
fn findings_of(d: &crate::app::DomainState) -> Findings {
    let doc = d.doc.as_ref().expect("fixture elaborates");
    let su = ecad_core::elaborate::stackup(&doc.source);
    let cands = candidates(doc, &d.lib, &su);
    Findings::compute(doc, &d.lib, &cands, &d.lib_notes)
}

/// The deliberate-clearance fixture flags `E_DRC_CLEARANCE` between the two offending
/// nets, and the finding carries BOTH nets as refs (the honest typed-violation
/// recovery, not the lossy single-net diagnostic projection).
#[test]
fn clearance_violation_carries_both_nets() {
    let f = findings_of(&drc_violation_domain());
    let clearance = f
        .items
        .iter()
        .find(|i| i.code == "E_DRC_CLEARANCE")
        .expect("the deliberate short must flag E_DRC_CLEARANCE");
    assert!(clearance.is_error());
    let nets: std::collections::BTreeSet<String> = clearance
        .refs
        .iter()
        .filter_map(|r| match r {
            SemanticId::Net(n) => Some(n.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        nets.contains("NA") && nets.contains("NB"),
        "clearance finding must carry BOTH offending nets, got {nets:?}"
    );
    assert!(f.errors >= 1, "the chip must count at least one error");
}

/// The clearance finding has a derived board-mm halo location (the engine carries no
/// mm — it is derived GUI-side from the pick candidates on the offending nets). The
/// point falls inside the board and near the two parallel traces (y ≈ 7.0–7.3 mm,
/// x centred on the 4–16 mm span).
#[test]
fn clearance_finding_has_board_halo_point() {
    let f = findings_of(&drc_violation_domain());
    let clearance = f
        .items
        .iter()
        .find(|i| i.code == "E_DRC_CLEARANCE")
        .unwrap();
    let (x, y) = clearance
        .board_mm
        .expect("a clearance on netted copper must derive a board point");
    // The traces span x = 4..16 mm at y ≈ 7.0/7.3; the union-bbox centre lands mid-span.
    assert!(
        (x - 10.0).abs() < 3.0,
        "halo x should be near the trace mid-span (~10 mm), got {x}"
    );
    assert!(
        (6.0..=8.5).contains(&y),
        "halo y should be near the parallel traces (~7 mm), got {y}"
    );
}

/// A finding's refs go through `board_matches`-style resolution: the derived point is
/// the union bbox centre of ALL copper on the ref nets, so it is stable and on-board.
#[test]
fn every_board_finding_point_is_inside_the_board() {
    let f = findings_of(&drc_violation_domain());
    for item in &f.items {
        if let Some((x, y)) = item.board_mm {
            assert!(
                (0.0..=20.0).contains(&x) && (0.0..=15.0).contains(&y),
                "finding [{}] point ({x},{y}) must be inside the 20x15 board",
                item.code
            );
        }
    }
}

/// The findings set is sorted errors-first and is deterministic (same doc → same set).
#[test]
fn findings_are_sorted_and_deterministic() {
    let d = drc_violation_domain();
    let a = findings_of(&d);
    let b = findings_of(&d);
    assert_eq!(a, b, "findings must be deterministic over one doc");
    // Errors sort before warnings (all DRC here are errors, so just check the tally is
    // consistent with the item severities).
    let counted_err = a.items.iter().filter(|i| i.is_error()).count();
    assert_eq!(counted_err, a.errors);
    assert_eq!(a.items.len() - counted_err, a.warnings);
}

/// The plain board fixture's findings are the two unrouted-net errors, each with a
/// derived halo point — the baseline the panel/chip render.
#[test]
fn board_fixture_has_unrouted_findings_with_points() {
    let f = findings_of(&board_domain());
    assert!(
        f.items.iter().any(|i| i.code == "E_DRC_UNROUTED"),
        "the board fixture leaves nets unrouted"
    );
    assert!(
        f.items.iter().all(|i| i.board_mm.is_some()),
        "every net-anchored finding on this board has copper → a derived point"
    );
    assert!(!f.is_clean());
}

/// A clean doc (no routing, fully-satisfiable) produces no findings and reads clean —
/// the chip's green state. An empty single-net-per-pin doc with no multi-pin nets has
/// nothing to flag.
#[test]
fn clean_doc_has_no_findings() {
    // A board with a single 1-pin-per-net setup: no ratsnest (nets < 2 pins), no
    // routed copper, no clearance pairs → clean. The cap is placed mid-board so
    // its (toy) pad copper clears the board edge.
    use crate::app::DomainState;
    let src = "\
inst C1 Cap
net SOLO C1.p1
nc C1.p2
place C1 (5mm, 5mm)
board (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)
";
    let d = DomainState::from_source(src.to_string(), Some("clean.ecad".to_string()));
    let f = findings_of(&d);
    assert!(
        f.is_clean(),
        "a doc with no multi-pin nets and no copper must be DRC-clean, got {:?}",
        f.items
    );
    assert_eq!(f.errors, 0);
    assert_eq!(f.warnings, 0);
    // A clean doc has no per-source summary for any source (drives the ✓ chip).
    for source in FindingSource::all() {
        assert_eq!(f.source_summary(source), None);
    }
}

/// Per-source classification: the clearance fixture's DRC error classifies under
/// `FindingSource::Drc`, and `source_summary(Drc)` reports it with error severity —
/// the toolbar's per-source-chip grouping (item 4). ERC/NET/LIB have nothing here.
#[test]
fn drc_violation_classifies_under_drc_source() {
    let f = findings_of(&drc_violation_domain());
    assert!(
        f.items
            .iter()
            .any(|i| i.source == FindingSource::Drc && i.code == "E_DRC_CLEARANCE"),
        "the clearance error must carry the Drc source"
    );
    let (count, worst) = f
        .source_summary(FindingSource::Drc)
        .expect("Drc source is nonzero for the violation fixture");
    assert!(count >= 1);
    assert_eq!(worst, ecad_core::diagnostic::Severity::Error);
    // Every finding's source is one of the four; DRC codes never leak into another.
    assert!(
        f.items
            .iter()
            .all(|i| i.source != FindingSource::Drc || i.code.starts_with("E_DRC_")),
        "only E_DRC_* codes may carry the Drc source"
    );
}
