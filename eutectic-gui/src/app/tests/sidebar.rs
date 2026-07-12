//! Right-sidebar accordion tests: independent per-section toggling, the
//! always-visible-headers invariant, the findings-count chips, the preserved
//! toolbar-chip → Findings-section wiring, and the header-truncation regression.

use super::*;
use crate::app::pane::{SidebarSection, findings_chip_key};
use crate::findings::Findings;
use damascene_core::state::UiState;

/// Depth-first: the routed key of every node in a tree.
fn keys_in(root: &El) -> Vec<String> {
    fn walk(n: &El, out: &mut Vec<String>) {
        if let Some(k) = n.key.as_deref() {
            out.push(k.to_string());
        }
        for c in &n.children {
            walk(c, out);
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

/// Depth-first search for the node carrying `key`.
fn find_by_key<'a>(root: &'a El, key: &str) -> Option<&'a El> {
    if root.key.as_deref() == Some(key) {
        return Some(root);
    }
    root.children.iter().find_map(|c| find_by_key(c, key))
}

/// The built (un-laid-out) tree for `app` at the review viewport.
fn build_tree(app: &EutecticApp) -> El {
    let theme = app.theme();
    let cx = BuildCx::new(&theme).with_viewport(1280.0, 800.0);
    app.build(&cx)
}

/// A header click toggles ONLY that section — the accordion is fully-free
/// (multi-open), not single-open. Properties + Layers open by default; each
/// section's header is reachable regardless of the others' state.
#[test]
fn section_header_click_toggles_only_that_section() {
    let mut app = board();
    let cx = EventCx::new();

    // Properties opens by default; its header closes it, another click reopens.
    assert!(app.section_open(SidebarSection::Properties));
    app.on_event(click(&SidebarSection::Properties.toggle_key()), &cx);
    assert!(!app.section_open(SidebarSection::Properties));
    app.on_event(click(&SidebarSection::Properties.toggle_key()), &cx);
    assert!(app.section_open(SidebarSection::Properties));

    // Findings is closed by default; opening it leaves the others untouched
    // (fully-free expansion, not a single-open radio).
    assert!(!app.section_open(SidebarSection::Findings));
    app.on_event(click(&SidebarSection::Findings.toggle_key()), &cx);
    assert!(app.section_open(SidebarSection::Findings));
    assert!(
        app.section_open(SidebarSection::Properties) && app.section_open(SidebarSection::Layers),
        "toggling Findings must not close the default-open sections"
    );
}

/// The preserved wiring: a toolbar per-source findings chip toggles the Findings
/// accordion section, exactly like clicking that section's header.
#[test]
fn toolbar_findings_chip_toggles_the_findings_section() {
    let mut app = drc_violation();
    let cx = EventCx::new();
    assert!(
        !app.section_open(SidebarSection::Findings),
        "findings starts collapsed"
    );
    app.on_event(click(&findings_chip_key("DRC")), &cx);
    assert!(
        app.section_open(SidebarSection::Findings),
        "a findings chip opens the section"
    );
    app.on_event(click(&findings_chip_key("DRC")), &cx);
    assert!(
        !app.section_open(SidebarSection::Findings),
        "clicking again collapses it"
    );
}

/// The accordion's headline invariant: ALL FOUR section headers render in every
/// document-loaded scene, regardless of which bodies are open (nothing lives below
/// an invisible fold). No-document / parse-error scenes render the error card
/// instead of the sidebar and are skipped.
#[test]
fn all_four_section_headers_render_in_every_loaded_scene() {
    for (name, app) in crate::fixtures::all() {
        if app.domain.doc.is_err() {
            continue;
        }
        let keys = keys_in(&build_tree(&app));
        for section in SidebarSection::all() {
            assert!(
                keys.iter().any(|k| *k == section.toggle_key()),
                "scene `{name}`: sidebar is missing the {section:?} header (key {})",
                section.toggle_key(),
            );
        }
    }
}

/// The `sidebar_findings_expanded` scene proves the invariant with a non-default
/// open set: Findings expanded, Layers collapsed. Both headers must still render,
/// and only the intended bodies are open.
#[test]
fn findings_expanded_scene_keeps_collapsed_layers_header() {
    let app = crate::fixtures::sidebar_findings_expanded();
    assert!(app.section_open(SidebarSection::Findings));
    assert!(!app.section_open(SidebarSection::Layers));
    let keys = keys_in(&build_tree(&app));
    // Every header present…
    for section in SidebarSection::all() {
        assert!(
            keys.iter().any(|k| *k == section.toggle_key()),
            "the {section:?} header must render even with Layers collapsed"
        );
    }
    // …the open bodies present, the collapsed ones absent.
    let body_key = |s: SidebarSection| format!("sidebar:body:{}", s.slug());
    assert!(
        keys.iter()
            .any(|k| *k == body_key(SidebarSection::Findings))
    );
    assert!(
        keys.iter()
            .any(|k| *k == body_key(SidebarSection::Properties))
    );
    assert!(
        !keys.iter().any(|k| *k == body_key(SidebarSection::Layers)),
        "a collapsed section has a header but no body"
    );
}

/// The Findings header's count chips carry the exact error / warning counts, and a
/// zero-count severity shows no chip.
#[test]
fn findings_header_chips_reflect_the_counts() {
    let app = drc_violation();
    let f = app.findings();
    assert!(f.errors > 0, "the drc fixture must flag errors to chip");
    let chips = crate::panels::findings::findings_header_chips(&f);
    let labels: Vec<String> = chips.iter().filter_map(|c| c.text.clone()).collect();
    assert!(
        labels.iter().any(|l| *l == format!("{} err", f.errors)),
        "the error chip must read the error count exactly, got {labels:?}"
    );
    if f.warnings > 0 {
        assert!(labels.iter().any(|l| *l == format!("{} warn", f.warnings)));
    } else {
        assert!(
            !labels.iter().any(|l| l.contains("warn")),
            "no warn chip when warnings == 0, got {labels:?}"
        );
    }
}

/// Truncation regression (the bug this slice fixes): the old header packed the
/// counts into the title text, which ellipsized and collided with the Hide button
/// ("Findings (5 err, 0 wa…"). The new header has no Hide button and a fixed
/// "FINDINGS" label that fills + ellipsizes, with the counts as discrete chips.
/// Even at pathologically long counts, laid out at the real 288 px width: the label
/// keeps a positive width (it ellipsizes rather than pushing the chips out) and no
/// two header cells overlap — collision is structurally impossible.
#[test]
fn findings_header_never_overlaps_with_long_counts() {
    let app = board();
    let theme = app.theme();
    let findings = Findings {
        items: Vec::new(),
        errors: 99999,
        warnings: 88888,
    };
    let header = app.findings_header_for_test(&findings, false);
    // Lay it out with real theme fonts at the sidebar width.
    let mut root = column([header]).width(Size::Fixed(288.0)).height(Size::Hug);
    let mut ui = UiState::new();
    let _ = render_bundle_with_theme(
        &mut root,
        &mut ui,
        Rect::new(0.0, 0.0, 288.0, 200.0),
        &theme,
    );

    let header = find_by_key(&root, &SidebarSection::Findings.toggle_key())
        .expect("the findings header laid out");
    // 0.4.6 moved layout rects in-node: `UiState::rect` only resolves keyed
    // nodes, so unkeyed header cells are read from `El::computed_rect`.
    let cells: Vec<Rect> = header.children.iter().map(|c| c.computed_rect).collect();
    // icon + label + err chip + warn chip + chevron.
    assert_eq!(cells.len(), 5, "header cells: {cells:?}");
    // The label (index 1) keeps a positive width — it never collapses to zero.
    assert!(
        cells[1].w > 0.0,
        "the FINDINGS label collapsed to zero width: {:?}",
        cells[1]
    );
    // No two consecutive cells overlap horizontally.
    for w in cells.windows(2) {
        assert!(
            w[0].right() <= w[1].x + 0.5,
            "header cells overlap: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
    // Every cell stays within the 288 px header.
    let hr = header.computed_rect;
    for c in &cells {
        assert!(
            c.x >= hr.x - 0.5 && c.right() <= hr.right() + 0.5,
            "a header cell escapes the header: cell {c:?} header {hr:?}"
        );
    }
}

/// WP3 successor to the deleted `canvas/tests.rs::enumerates_board_layers`:
/// layer-panel rows now derive from the board scene's plane list
/// (`domain::layer_rows`). Pins the row set — outline first (painted under
/// everything), every stackup slab with copper/mask/silk on both sides — and
/// that the copper swatches keep the warm-top / cool-bottom split. The old
/// `core` (dielectric) and `Drills` rows are gone BY DESIGN: neither toggle
/// has been functional since WP2 (no scene plane maps to them). Whether
/// drills deserve a live toggle again is an open product ruling — if it is
/// made, this test is the one to update.
#[test]
fn layer_panel_rows_pin_scene_planes_and_copper_swatches() {
    let app = edit_app();
    let derived = app.derived.borrow();
    let layers = &derived.board.as_ref().expect("board projects").layers;
    assert_eq!(
        layers.first().expect("rows non-empty").name,
        "Board outline",
        "outline row stays first (painted under everything)"
    );
    let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
    for expected in ["B.SilkS", "B.Mask", "B.Cu", "F.Cu", "F.Mask", "F.SilkS"] {
        assert!(
            names.contains(&expected),
            "missing layer row `{expected}` in {names:?}"
        );
    }
    for gone in ["core", "Drills"] {
        assert!(
            !names.contains(&gone),
            "dead-toggle row `{gone}` must stay retired (its toggle mapped to \
             no plane since WP2); revisit deliberately, not by drift"
        );
    }
    let color_of = |n: &str| {
        layers
            .iter()
            .find(|l| l.name == n)
            .unwrap_or_else(|| panic!("row `{n}`"))
            .color
    };
    assert_ne!(
        color_of("F.Cu"),
        color_of("B.Cu"),
        "copper swatches keep the warm-top / cool-bottom split"
    );
}
