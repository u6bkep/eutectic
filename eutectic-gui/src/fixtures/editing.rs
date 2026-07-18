//! The m6 slice-A editing scenes: the editing board, a component drag in
//! progress, a dirty doc, and the disk-conflict banner. Moved verbatim from
//! `fixtures.rs` (gui-module-split).

use super::BOARD_ECAD;
use crate::app::{DomainState, EutecticApp, PaneId};

// ---------------------------------------------------------------------------
// Milestone-6 slice-A scenes: the editing foundation. A drag in progress
// (ghost + live ratsnest in the overlay), a dirty doc (filename bullet + a
// primary Save button), and the disk-conflict banner (external change while
// dirty → explicit Reload / Keep-mine, never silent last-writer).
// ---------------------------------------------------------------------------

/// The m6 editing board: [`BOARD_ECAD`]'s two netted caps + outline + pour,
/// with no command-routed extras — the editing scenes exercise the commit path
/// themselves. The built-in toy library now carries real pad copper (a 0.8 mm
/// top-side square per pin), so the caps are pickable / draggable / routable
/// against the plain `part_library()` — the slice-A `padded_toy_lib` overlay
/// is retired.
pub fn edit_board_domain() -> DomainState {
    DomainState::from_source_with(
        BOARD_ECAD.to_string(),
        Some("board.eut".to_string()),
        eutectic_core::part::part_library(),
        |_| Vec::new(),
    )
}

/// A component drag in progress over the editing board: `C1` grabbed at its own
/// position and dragged +5 mm / +3 mm. The overlay renders the pad-shape GHOST at
/// the uncommitted position and the live RATSNEST (a line from each ghost pad to
/// the nearest other member pad of its net — C2's pads here). Nothing is
/// committed; the doc is untouched and clean.
pub fn drag_in_progress() -> EutecticApp {
    use eutectic_core::coord::{MM, Point};
    use eutectic_core::id::EntityId;
    let app = EutecticApp::new(edit_board_domain());
    let comp = EntityId::new("C1");
    let doc = app.domain.doc.as_ref().expect("edit board elaborates");
    let from = doc.components[&comp].pos.value;
    // C1 sits at (15, 3); drag left+up so the ghost stays inside the board.
    let to = Point {
        x: from.x - 5 * MM,
        y: from.y + 3 * MM,
    };
    let armed = app.set_drag(&comp, PaneId::A, to);
    debug_assert!(armed, "C1 has pad candidates");
    app
}

/// A dirty document (m6 save model): the editing board with one committed GUI
/// edit (C1 pinned 2 mm right of its solved position) and a source path wired,
/// so the chrome shows the dirty bullet on the filename badge and a primary Save
/// button. The path is inert fixture data — nothing is ever written by the scene.
pub fn dirty_doc() -> EutecticApp {
    use eutectic_core::command::{Command, Transaction};
    use eutectic_core::coord::{MM, Point};
    use eutectic_core::id::EntityId;
    let mut domain = edit_board_domain();
    domain.source_path = Some(std::path::PathBuf::from("/nonexistent/eutectic/board.eut"));
    let mut app = EutecticApp::new(domain);
    let comp = EntityId::new("C1");
    let pos = app.domain.doc.as_ref().unwrap().components[&comp].pos.value;
    let target = Point {
        x: pos.x + 2 * MM,
        y: pos.y,
    };
    app.commit_edit(
        Transaction::one(Command::Pin(comp, target)),
        "move component",
    )
    .expect("fixture move commits");
    app
}

/// The conflict banner (m6 save model): the dirty doc above with an external
/// disk change waiting on the mailbox — the harness's first `before_build`
/// routes it into the pending conflict (the doc is dirty, so it is NOT applied)
/// and the persistent banner renders with the two explicit actions.
pub fn conflict_banner() -> EutecticApp {
    use crate::reload::SourceMsg;
    let app = dirty_doc();
    // An externally-edited variant of the board source (a comment prepended is
    // enough — any text that differs from our own last save).
    let external = format!("# edited in $EDITOR\n{BOARD_ECAD}");
    app.mailbox_push(SourceMsg::pathless(external));
    app
}
