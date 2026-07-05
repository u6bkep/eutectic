//! Pane + layout state — the view-dependent half of `gui-architecture.md`
//! through-line 3, plus the small key/const vocabulary shared across the app
//! chrome (route keys, the canvas-target predicate, placeholders). Split out of
//! `app.rs` as pure code motion.

use damascene_core::prelude::*;

/// Which view a pane renders (mockup: the pane header's view-type switcher). v1 has two
/// read-only view kinds; `3D` etc. are wishlist. A schematic and a board pane over the
/// same doc share the semantic selection but project it into their own overlays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    /// The layered board canvas (milestone 2/3).
    Board,
    /// The read-only schematic view (milestone 4).
    Schematic,
}

impl ViewKind {
    /// The human label for the pane header + switcher.
    pub fn label(self) -> &'static str {
        match self {
            ViewKind::Board => "PCB Layout",
            ViewKind::Schematic => "Schematic",
        }
    }

    /// Both view kinds, in switcher order.
    pub fn all() -> [ViewKind; 2] {
        [ViewKind::Board, ViewKind::Schematic]
    }
}

/// The two-pane orientation (mockup: the dual/stacked toolbar toggle). `Dual` is side-by-
/// side (a `row` split), `Stacked` is over/under (a `column` split). A one-split
/// simplification of the split-tree — fine for v1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneLayout {
    Dual,
    Stacked,
}

/// Which pane a pane index names — `A` (first / left / top) or `B` (second / right /
/// bottom). The two are symmetric; the enum keeps call sites readable and keys stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneId {
    A,
    B,
}

impl PaneId {
    /// The canvas viewport El key for this pane — distinct per pane so the two cameras are
    /// independent in damascene's `UiState` (through-line 3), *even when both panes show
    /// the same view kind*.
    pub(crate) fn canvas_key(self) -> &'static str {
        match self {
            PaneId::A => "canvas:a",
            PaneId::B => "canvas:b",
        }
    }

    /// The dynamic-overlay El key for this pane (stacked over its canvas).
    pub(crate) fn overlay_key(self) -> &'static str {
        match self {
            PaneId::A => "overlay:a",
            PaneId::B => "overlay:b",
        }
    }

    /// The view-switcher button key for a target view kind in this pane.
    pub(crate) fn switch_key(self, v: ViewKind) -> String {
        let p = match self {
            PaneId::A => "a",
            PaneId::B => "b",
        };
        format!(
            "pane:{p}:view:{}",
            match v {
                ViewKind::Board => "board",
                ViewKind::Schematic => "schematic",
            }
        )
    }

    /// The maximize-toggle button key for this pane.
    pub(crate) fn maximize_key(self) -> &'static str {
        match self {
            PaneId::A => "pane:a:max",
            PaneId::B => "pane:b:max",
        }
    }
}

/// Per-pane view state: the *view-dependent* half of through-line 3. A pane is one view
/// over the shared [`DomainState`](crate::app::DomainState), with its own camera keyed by
/// the pane's canvas El key. Milestone 4 makes this real: the pane owns its view kind and
/// whether it has been fit-to-content yet (the initial framing fires once per pane).
#[derive(Clone, Debug)]
pub struct PaneState {
    /// The view this pane renders.
    pub view: ViewKind,
    /// Whether the initial fit-to-content has been queued for this pane's camera.
    pub(crate) fitted: bool,
}

impl PaneState {
    pub(crate) fn new(view: ViewKind) -> Self {
        PaneState {
            view,
            fitted: false,
        }
    }
}

impl Default for PaneState {
    fn default() -> Self {
        PaneState::new(ViewKind::Board)
    }
}

/// A pane index into the `panes` array.
pub(crate) fn pane_index(p: PaneId) -> usize {
    match p {
        PaneId::A => 0,
        PaneId::B => 1,
    }
}

/// The event-route key of the dual/stacked layout toggle button.
pub(crate) const LAYOUT_TOGGLE_KEY: &str = "layout:toggle";
/// The event-route key of the findings-panel collapse toggle.
pub(crate) const FINDINGS_TOGGLE_KEY: &str = "findings:toggle";
/// The route-key prefix of a toolbar findings chip (a source label, or `ok`, appended).
/// Each chip needs its own key (keys are unique in the tree); clicking any of them
/// toggles/focuses the findings panel exactly like [`FINDINGS_TOGGLE_KEY`].
pub(crate) const FINDINGS_CHIP_PREFIX: &str = "findings:chip:";

/// The route key for the toolbar findings chip named `tag` (a source label or `ok`).
pub(crate) fn findings_chip_key(tag: &str) -> String {
    format!("{FINDINGS_CHIP_PREFIX}{tag}")
}

/// Whether `route` is a toolbar findings chip (any of the per-source / ✓ chips).
pub(crate) fn is_findings_chip_key(route: &str) -> bool {
    route.starts_with(FINDINGS_CHIP_PREFIX)
}

/// The route-key prefix of a findings row (index appended).
const FINDINGS_ROW_PREFIX: &str = "finding:row:";

/// The route key for the findings row at `index`.
pub(crate) fn finding_row_key(index: usize) -> String {
    format!("{FINDINGS_ROW_PREFIX}{index}")
}

/// The finding index a route key names, if it is a findings row.
pub(crate) fn finding_index_of_key(route: &str) -> Option<usize> {
    route.strip_prefix(FINDINGS_ROW_PREFIX)?.parse().ok()
}

/// The key of the pane-split resize handle + the split row/column (for `rect_of_key`).
pub(crate) const SPLIT_HANDLE_KEY: &str = "pane:split";
pub(crate) const SPLIT_ROW_KEY: &str = "pane:split-row";

/// The dark canvas background behind the board — an ECAD-dark near-black.
pub(crate) const CANVAS_BG: Color = Color::srgb_token("ecad.canvas.bg", 0x12, 0x14, 0x18, 0xff);

/// The event-route key of a layer's visibility switch.
pub(crate) fn switch_key(layer_key: &str) -> String {
    format!("switch:{layer_key}")
}

/// Is this event target inside a pane canvas? A pointer event routes to a pane viewport
/// (`canvas:a` / `canvas:b`), a stacked board layer / overlay El (keyed `layer:*` /
/// `overlay:*`), or a schematic static El (keyed `schematic:*`). All are canvas hits;
/// chrome (toolbar, sidebar, pane headers) is not.
pub(crate) fn is_canvas_target(target: Option<&str>) -> bool {
    match target {
        Some(k) => {
            k == PaneId::A.canvas_key()
                || k == PaneId::B.canvas_key()
                || k.starts_with("layer:")
                || k.starts_with("overlay:")
                || k.starts_with("schematic:")
        }
        None => false,
    }
}

/// A pane's empty-state placeholder (no board / no schematic to display), filling the
/// pane so the split geometry is unaffected.
pub(crate) fn pane_placeholder(msg: &str) -> El {
    column([text(msg).muted()])
        .align(Align::Center)
        .fill(CANVAS_BG)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
}
