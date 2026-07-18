use super::*;
use crate::app::autoroute::{AUTOROUTE_BOARD_KEY, AUTOROUTE_NET_KEY};
use crate::findings::FindingSource;
use eutectic_core::doc::Provenance;
use eutectic_core::id::{NetId, TraceId};
use eutectic_core::route::Trace;

#[test]
fn autoroute_net_resolves_selection_and_commits_one_free_copper_transaction() {
    let mut app = edit_app();
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(NetId::new("VBUS")));
    assert!(app.can_autoroute_selection());

    app.on_event(click(AUTOROUTE_NET_KEY), &EventCx::new());

    let doc = app.domain.doc.as_ref().expect("document remains loaded");
    assert!(
        !doc.traces.is_empty(),
        "selected VBUS received routed copper"
    );
    assert!(
        doc.traces
            .values()
            .all(|trace| { trace.net == NetId::new("VBUS") && trace.prov == Provenance::Free })
    );
    assert!(
        doc.vias
            .values()
            .all(|via| { via.net == NetId::new("VBUS") && via.prov == Provenance::Free })
    );
    assert_eq!(app.undo_depths(), (1, 0), "all commands are one undo unit");
    assert!(app.dirty());
    assert!(
        app.chrome_notice
            .borrow()
            .as_ref()
            .expect("outcome notice")
            .message
            .starts_with("autoroute: 1/1 nets routed")
    );

    app.undo();
    assert!(app.domain.doc.as_ref().unwrap().traces.is_empty());
    assert!(app.domain.doc.as_ref().unwrap().vias.is_empty());
}

#[test]
fn autoroute_net_selection_resolves_pin_trace_and_via_membership() {
    let pin_app = edit_app();
    pin_app
        .domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Pin {
            comp: EntityId::new("C1"),
            pin: "p2".to_string(),
        });
    assert_eq!(
        pin_app.selected_route_nets(),
        std::collections::BTreeSet::from([NetId::new("VBUS")])
    );

    let routed = crate::fixtures::routed_trace();
    let trace = *routed
        .domain
        .doc
        .as_ref()
        .unwrap()
        .traces
        .keys()
        .next()
        .unwrap();
    routed
        .domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Trace(trace));
    assert_eq!(
        routed.selected_route_nets(),
        std::collections::BTreeSet::from([NetId::new("GND")])
    );

    let via_app = EutecticApp::new(crate::fixtures::board_domain());
    let via = *via_app
        .domain
        .doc
        .as_ref()
        .unwrap()
        .vias
        .keys()
        .next()
        .unwrap();
    via_app
        .domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Via(via));
    assert_eq!(
        via_app.selected_route_nets(),
        std::collections::BTreeSet::from([NetId::new("VBUS")])
    );
}

#[test]
fn autoroute_net_routes_multiple_selected_nets() {
    let mut app = edit_app();
    {
        let mut selection = app.domain.selection.borrow_mut();
        selection.select_only(SemanticId::Net(NetId::new("GND")));
        selection.add(SemanticId::Net(NetId::new("VBUS")));
    }
    assert_eq!(
        app.selected_route_nets(),
        std::collections::BTreeSet::from([NetId::new("GND"), NetId::new("VBUS")])
    );

    app.on_event(click(AUTOROUTE_NET_KEY), &EventCx::new());

    let doc = app.domain.doc.as_ref().expect("document remains loaded");
    let routed_nets: std::collections::BTreeSet<_> = doc
        .traces
        .values()
        .map(|trace| trace.net.clone())
        .chain(doc.vias.values().map(|via| via.net.clone()))
        .collect();
    assert_eq!(
        routed_nets,
        std::collections::BTreeSet::from([NetId::new("VBUS")])
    );
    assert_eq!(app.undo_depths(), (1, 0));
    assert_eq!(
        app.chrome_notice.borrow().as_ref().unwrap().message,
        "autoroute: 2/2 nets routed"
    );
}

#[test]
fn autoroute_board_row_routes_without_selection() {
    let mut app = edit_app();
    assert!(app.domain.selection.borrow().is_empty());

    app.on_event(click(AUTOROUTE_BOARD_KEY), &EventCx::new());

    assert!(app.dirty());
    assert_eq!(app.undo_depths(), (1, 0));
    assert!(
        app.domain
            .doc
            .as_ref()
            .unwrap()
            .traces
            .values()
            .all(|trace| { trace.prov == Provenance::Free })
    );
}

#[test]
fn zero_result_autoroute_does_not_dirty_or_create_undo() {
    let source =
        "inst C1 Cap\nnet SOLO C1.p1\nboard (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)\n";
    let mut app = EutecticApp::new(DomainState::from_source(
        source.to_string(),
        Some("solo.eut".to_string()),
    ));
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(NetId::new("SOLO")));
    let revision = app.revision();

    app.autoroute_selection();

    assert_eq!(app.revision(), revision);
    assert!(!app.dirty());
    assert_eq!(app.undo_depths(), (0, 0));
    assert!(app.domain.doc.as_ref().unwrap().traces.is_empty());
    assert!(app.domain.doc.as_ref().unwrap().vias.is_empty());
}

#[test]
fn unrouted_remainder_stays_a_net_finding_after_autoroute() {
    let source = "\
inst reg LDO
inst dec Cap
net VBUS reg.VOUT dec.p1
net WALL reg.VIN
place reg (0mm, 0mm)
place dec (12mm, 0mm)
board (-6mm, -10mm) (18mm, -10mm) (18mm, 10mm) (-6mm, 10mm)
";
    let domain = DomainState::from_source_with(
        source.to_string(),
        Some("blocked.eut".to_string()),
        eutectic_core::part::part_library(),
        |_| {
            ["F.Cu", "B.Cu"]
                .into_iter()
                .enumerate()
                .map(|(index, layer)| {
                    Command::AddTrace(
                        TraceId(index as u64 + 1),
                        Trace {
                            net: NetId::new("WALL"),
                            layer: layer.to_string(),
                            path: vec![Point::mm(6, -12), Point::mm(6, 12)],
                            width: 200_000,
                            prov: Provenance::Pinned,
                        },
                    )
                })
                .collect()
        },
    );
    let mut app = EutecticApp::new(domain);
    app.domain
        .selection
        .borrow_mut()
        .select_only(SemanticId::Net(NetId::new("VBUS")));

    app.autoroute_selection();

    assert!(!app.dirty(), "failed route emitted no transaction");
    assert!(app.findings().items.iter().any(|finding| {
        finding.source == FindingSource::Net
            && finding.code == "E_DRC_UNROUTED"
            && finding.refs.contains(&SemanticId::Net(NetId::new("VBUS")))
    }));
    assert_eq!(
        app.chrome_notice.borrow().as_ref().unwrap().message,
        "autoroute: 0/1 nets routed"
    );
}
