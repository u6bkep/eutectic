//! Canned [`EcadApp`] states for the headless review loop.
//!
//! Per `gui-architecture.md` ("Headless review loop"), GUI panels get the same
//! fixture-and-artifact review discipline the engine's fab outputs get: canned
//! scenes here, lint-clean assertions in the tests below, and SVG/tree/lint
//! artifacts dumped by the `review` binary (`src/bin/review.rs`).
//!
//! The three states are the ones milestone 1 can produce: no document, a
//! document loaded (from a tiny inline `.ecad` source), and a parse-error
//! state.
//!
//! Split into scene-group submodules (gui-module-split) — [`viewer`],
//! [`selection`], [`findings`], [`editing`], [`routing`] — re-exported here so
//! every scene keeps its `crate::fixtures::*` path; this module keeps the
//! file-reading poc-board loader and the scene registry ([`all`]).

mod editing;
mod findings;
mod routing;
mod selection;
mod viewer;

pub use editing::*;
pub use findings::*;
pub use routing::*;
pub use selection::*;
pub use viewer::*;

#[cfg(test)]
mod tests;

use crate::app::{DomainState, EcadApp};

// ---------------------------------------------------------------------------
// The real 4-layer multiprobe board, loaded from `poc/out/board.ecad` with the
// same KiCad-imported library the `poc_multiprobe` example builds. Reads files at
// call time (path relative to the crate manifest) — used by the end-to-end smoke
// test, not the inline-preferred review scene above.
// ---------------------------------------------------------------------------

/// The `poc/` directory, relative to this crate's manifest.
fn poc_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc")
}

/// The real multiprobe board loaded from `poc/out/board.ecad` with the `poc`
/// library package loaded from `poc/parts` (the same manifest-driven
/// [`ecad_core::library::load_library`] path the `poc_multiprobe` example uses —
/// the board's `use poc` directive names it). Panics on a missing/broken package —
/// this is test scaffolding, not the app path. Used by the end-to-end canvas
/// smoke test.
pub fn poc_board_domain() -> DomainState {
    let parts = poc_dir().join("parts");
    let lib = ecad_core::library::load_library(&parts)
        .unwrap_or_else(|e| panic!("load library package from {}: {e}", parts.display()));
    let path = poc_dir().join("out/board.ecad");
    let source =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    DomainState::from_source_with(source, Some("board.ecad".to_string()), lib, |_| Vec::new())
}

/// Every fixture, paired with a stable scene name for artifact filenames. The
/// `poc_board` scene is deliberately excluded — it reads files, so it belongs to
/// the smoke test, not the always-on lint bundle.
pub fn all() -> Vec<(&'static str, EcadApp)> {
    vec![
        ("no_document", no_document()),
        ("document_loaded", document_loaded()),
        ("parse_error", parse_error()),
        ("board", board()),
        ("menubar_open", menubar_open()),
        ("board_with_selection", board_with_selection()),
        ("drc_violation", drc_violation()),
        ("sidebar_findings_expanded", sidebar_findings_expanded()),
        ("measure_in_progress", measure_in_progress()),
        ("dual_cross_highlight", dual_cross_highlight()),
        ("stacked_layout", stacked_layout()),
        ("maximized_pane", maximized_pane()),
        ("dual_boards", dual_boards()),
        ("per_kind_tools", per_kind_tools()),
        ("unresolved_libs", unresolved_libs()),
        ("libraries_menu", libraries_menu()),
        ("drag_in_progress", drag_in_progress()),
        ("dirty_doc", dirty_doc()),
        ("conflict_banner", conflict_banner()),
        ("route_in_progress", route_in_progress()),
        ("routed_trace", routed_trace()),
        ("route_layer_switch", route_layer_switch()),
        ("trace_vertex_drag", trace_vertex_drag()),
    ]
}
