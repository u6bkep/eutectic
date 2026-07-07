//! The app-shell test suite, split out of `app.rs` by concern (gui-module-split;
//! every test moved verbatim): [`editing`] (commits, the save model, undo/redo,
//! drag placement), [`libraries`] (registry-driven resolution + the Libraries
//! menu), [`panes`] (layout / maximize / view-switch + per-pane composition),
//! [`reload`] (the m5 live-source loop), [`routing`] (m6 slice B trace drawing +
//! refinement), [`selection`] (explorer / findings clicks + highlight
//! projection), and [`tools`] (per-view-kind tool slots + the pane strips).
//! This module holds the shared helpers + imports, reached by each concern
//! module through `use super::*`.

mod camera;
mod editing;
mod libraries;
mod menubar;
mod panes;
mod reload;
mod routing;
mod selection;
mod sidebar;
mod tools;

use super::*;
use crate::fixtures::{SCHEMATIC_ECAD, board, drc_violation, edit_board_domain};
use crate::fixtures::{dual_boards, schematic_domain};
use crate::reload::SourceMsg;
use ecad_core::command::{Command, Transaction};
use ecad_core::coord::MM;
use ecad_core::coord::{MM as NM_PER_MM, Point};
use ecad_core::id::EntityId;

/// A synthetic click routed to `key`.
fn click(key: &str) -> UiEvent {
    UiEvent::synthetic_click(key)
}

/// A synthetic click on PANE A's tool-strip button for `tool` — the way a user
/// picks a tool (pane A is the board pane in the editing fixtures, so this sets
/// the BOARD kind's slot).
fn strip_click(tool: Tool) -> UiEvent {
    click(&PaneId::A.strip_key(tool))
}

/// A settled render of an app through the harness (drives before_build → reload).
fn settle(app: &mut EcadApp) -> crate::harness::Rendered {
    crate::harness::render_settled(app, Rect::new(0.0, 0.0, 1280.0, 800.0))
}

/// A source that fails the load — a malformed `inst` (missing its part token).
/// An unknown part no longer fails (library packages: it degrades to a
/// `W_UNRESOLVED_PART` finding), so the error path needs a genuine syntax fault.
const BROKEN_SRC: &str = "\
inst U1
net GND U1.GND
";

/// A scratch dir under the system temp dir, removed on drop.
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("ecad-app-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A pointer event of `kind` at `pos`, routed to pane A's canvas — the
/// headless stand-in for the host's pointer routing (`key` IS the target key
/// for real pointer events; `UiTarget` is non-exhaustive, so tests carry the
/// route in `key` and the app's canvas gate accepts either).
fn pointer(kind: UiEventKind, pos: (f32, f32)) -> UiEvent {
    let mut e = UiEvent::synthetic_click(PaneId::A.canvas_key());
    e.kind = kind;
    e.pointer = Some(pos);
    e
}

/// A window-level Escape.
fn escape() -> UiEvent {
    let mut e = UiEvent::synthetic_click("");
    e.key = None;
    e.kind = UiEventKind::Escape;
    e
}

/// A hotkey event for `action` (what damascene emits when a registered
/// chord matches — Ctrl+S/Z/… — with the action name as the route).
fn hotkey(action: &str) -> UiEvent {
    let mut e = UiEvent::synthetic_click(action);
    e.kind = UiEventKind::Hotkey;
    e
}

/// The editing app: the padded board (pickable pads) as pane A.
fn edit_app() -> EcadApp {
    EcadApp::new(edit_board_domain())
}

/// The doc position of component `id`.
fn comp_pos(app: &EcadApp, id: &EntityId) -> Point {
    app.domain.doc.as_ref().unwrap().components[id].pos.value
}

/// Commit a canned move of `C1` by `(dx, dy)` mm — the test shorthand for "a
/// GUI edit happened".
fn commit_move(app: &mut EcadApp, dx: i64, dy: i64) {
    let comp = EntityId::new("C1");
    let p = comp_pos(app, &comp);
    let target = Point {
        x: p.x + dx * NM_PER_MM,
        y: p.y + dy * NM_PER_MM,
    };
    app.commit_edit(Transaction::one(Command::Pin(comp, target)), "move")
        .expect("move commits");
}

/// Map a board point to pane-A screen px in a settled render (the exact
/// inverse composition the pick path applies): board → asset content px
/// (through the asset's honest natural-size content rect) → screen through
/// the pane's live camera.
fn px_of_board(app: &EcadApp, r: &crate::harness::Rendered, p: Point) -> (f32, f32) {
    let canvas = app.board_canvas_clone();
    let rect = r.ui.rect_of_key(PaneId::A.canvas_key()).expect("pane A");
    let vv =
        r.ui.viewport_view_by_key(PaneId::A.canvas_key())
            .expect("pane A view");
    let mm = (p.x as f32 / NM_PER_MM as f32, p.y as f32 / NM_PER_MM as f32);
    let content = canvas
        .board_mm_to_content_px(mm, canvas.content_rect((rect.x, rect.y, rect.w, rect.h)))
        .expect("maps");
    vv.project(content, (rect.x, rect.y))
}

/// The screen→board mapping the pointer handler applies, for computing the
/// exact expected commit target from the synthesized pixel positions.
fn board_of_px(app: &EcadApp, r: &crate::harness::Rendered, px: (f32, f32)) -> Point {
    let canvas = app.board_canvas_clone();
    let rect = r.ui.rect_of_key(PaneId::A.canvas_key()).unwrap();
    let vv = r.ui.viewport_view_by_key(PaneId::A.canvas_key()).unwrap();
    pick::pointer_to_board_nm(
        &canvas,
        px,
        canvas.content_rect((rect.x, rect.y, rect.w, rect.h)),
        vv,
    )
    .expect("in view")
}

/// A pad-candidate center of `comp` (the grab point for drag tests).
fn pad_center_of(app: &EcadApp, comp: &EntityId) -> Point {
    let derived = app.derived.borrow();
    let view = derived.board.as_ref().expect("board projects");
    let c = view
        .candidates
        .iter()
        .find(|c| matches!(&c.id, SemanticId::Pin { comp: cc, .. } if cc == comp))
        .expect("comp has a pad candidate");
    Point {
        x: (c.aabb.0.x + c.aabb.1.x) / 2,
        y: (c.aabb.0.y + c.aabb.1.y) / 2,
    }
}
