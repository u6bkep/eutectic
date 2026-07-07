//! Pane + layout state — the view-dependent half of `gui-architecture.md`
//! through-line 3, plus the small key/const vocabulary shared across the app
//! chrome (route keys, the canvas-target predicate, placeholders). Split out of
//! `app.rs` as pure code motion.

use crate::tool::Tool;
use damascene_core::prelude::*;

/// Which view a pane renders (mockup: the pane header's view-type switcher). v1 has two
/// read-only view kinds; `3D` etc. are wishlist. A schematic and a board pane over the
/// same doc share the semantic selection but project it into their own overlays.
///
/// `Ord`/`Hash` so the kind can key the per-view-kind tool map (revised structural
/// commitment 4): the active tool is stored per KIND, so every pane of a kind shares
/// one tool slot, and a kind with no entry yet defaults to [`Tool::Select`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// The tools this kind's per-pane strip offers, grouped for the strip's thin
    /// separators (UI-oracle strip anatomy: the shared pick tools first, then the
    /// kind-specific group). Applicability is STRUCTURAL: a tool that makes no
    /// sense for a kind (Route on a schematic) simply isn't in its groups — there
    /// is no disabled state and no applicability check anywhere else. Only tools
    /// that exist today are listed; future tools join their group when they land.
    pub(crate) fn strip_groups(self) -> &'static [&'static [Tool]] {
        match self {
            ViewKind::Board => &[&[Tool::Select, Tool::Measure], &[Tool::Route]],
            ViewKind::Schematic => &[&[Tool::Select, Tool::Measure]],
        }
    }

    /// Whether `tool` exists in this kind's strip — the structural-applicability
    /// predicate (a strip click for a tool the kind doesn't offer is ignored, so a
    /// synthesized event can never smuggle Route into the schematic slot).
    pub(crate) fn offers_tool(self, tool: Tool) -> bool {
        self.strip_groups().iter().any(|g| g.contains(&tool))
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

    /// This pane's short key tag (`"a"` / `"b"`), for composed route keys.
    fn tag(self) -> &'static str {
        match self {
            PaneId::A => "a",
            PaneId::B => "b",
        }
    }

    /// The route key of this pane's tool-strip button for `tool`
    /// (`"strip:a:tool:route"`). Public (crate-wide) vocabulary so tests drive
    /// tools exactly the way a user does — through a pane's strip.
    pub fn strip_key(self, tool: Tool) -> String {
        format!("strip:{}:{}", self.tag(), tool.key())
    }

    /// The route key of this pane's strip panel itself. Keyed so the panel's
    /// background swallows pointer events within its own rect (a click between
    /// buttons must not fall through to the canvas below); `on_event` routes it
    /// nowhere, so it is inert chrome. Events outside the panel rect hit the
    /// canvas as usual — the strip never intercepts pan/zoom beyond itself.
    pub(crate) fn strip_panel_key(self) -> &'static str {
        match self {
            PaneId::A => "strip:a:panel",
            PaneId::B => "strip:b:panel",
        }
    }
}

/// Parse a strip-button route key back to its `(pane, tool)` target, if `route`
/// is one (`"strip:a:tool:route"` → `(A, Route)`).
pub(crate) fn strip_target_of_key(route: &str) -> Option<(PaneId, Tool)> {
    let rest = route.strip_prefix("strip:")?;
    let (tag, tool_key) = rest.split_once(':')?;
    let pane = match tag {
        "a" => PaneId::A,
        "b" => PaneId::B,
        _ => return None,
    };
    let tool = Tool::all().into_iter().find(|t| t.key() == tool_key)?;
    Some((pane, tool))
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
/// The toolbar Save button key AND the Ctrl+S hotkey action name (m6 save model).
pub(crate) const SAVE_KEY: &str = "save";
/// The toolbar Undo button key AND the Ctrl+Z hotkey action name.
pub(crate) const UNDO_KEY: &str = "undo";
/// The toolbar Redo button key AND the Ctrl+Shift+Z / Ctrl+Y hotkey action name.
pub(crate) const REDO_KEY: &str = "redo";
/// The conflict banner's "Reload from disk" action (discard my edits, apply disk).
pub(crate) const CONFLICT_RELOAD_KEY: &str = "conflict:reload";
/// The conflict banner's "Keep mine" action (dismiss; doc stays dirty).
pub(crate) const CONFLICT_KEEP_KEY: &str = "conflict:keep";
/// The right-sidebar accordion sections, top to bottom (`gui-architecture.md`
/// "Right sidebar" + the UI oracle). All four headers are always visible; each
/// body expands/collapses independently. The order here is the render order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SidebarSection {
    /// The selection inspector (identity card + property rows), or doc stats.
    Properties,
    /// The board layer panel (visibility, swatch, active-layer radio).
    Layers,
    /// The components + nets explorer.
    Explorer,
    /// The DRC/ERC/connectivity/library findings list.
    Findings,
}

/// The route-key prefix of a sidebar section's accordion header.
const SECTION_KEY_PREFIX: &str = "sidebar:section:";

impl SidebarSection {
    /// The four sections in render order (top to bottom).
    pub(crate) fn all() -> [SidebarSection; 4] {
        [
            SidebarSection::Properties,
            SidebarSection::Layers,
            SidebarSection::Explorer,
            SidebarSection::Findings,
        ]
    }

    /// The stable slug used in the header's route key (and the `all()` lookup).
    pub(crate) fn slug(self) -> &'static str {
        match self {
            SidebarSection::Properties => "properties",
            SidebarSection::Layers => "layers",
            SidebarSection::Explorer => "explorer",
            SidebarSection::Findings => "findings",
        }
    }

    /// The uppercase header label (the oracle's small-caps section titles).
    pub(crate) fn label(self) -> &'static str {
        match self {
            SidebarSection::Properties => "PROPERTIES",
            SidebarSection::Layers => "LAYERS",
            SidebarSection::Explorer => "EXPLORER",
            SidebarSection::Findings => "FINDINGS",
        }
    }

    /// The leading header icon (a damascene builtin name — the oracle's Material
    /// Symbols are decorative; these are the closest lucide-set equivalents).
    pub(crate) fn icon(self) -> &'static str {
        match self {
            SidebarSection::Properties => "settings",
            SidebarSection::Layers => "layout-dashboard",
            SidebarSection::Explorer => "folder",
            SidebarSection::Findings => "alert-circle",
        }
    }

    /// The event-route key of this section's accordion header (click toggles it).
    pub(crate) fn toggle_key(self) -> String {
        format!("{SECTION_KEY_PREFIX}{}", self.slug())
    }
}

/// The [`SidebarSection`] a route key names, if it is an accordion header.
pub(crate) fn section_of_key(route: &str) -> Option<SidebarSection> {
    let slug = route.strip_prefix(SECTION_KEY_PREFIX)?;
    SidebarSection::all().into_iter().find(|s| s.slug() == slug)
}

/// The per-section expanded/collapsed state of the right-sidebar accordion. All
/// four headers render regardless; this only governs which bodies are open.
/// Fully-free expansion: any subset may be open at once and open bodies share the
/// remaining height (`Size::Fill`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SectionOpen {
    properties: bool,
    layers: bool,
    explorer: bool,
    findings: bool,
}

impl Default for SectionOpen {
    /// Default open: Properties + Layers (per the oracle); Explorer + Findings
    /// start collapsed.
    fn default() -> Self {
        SectionOpen {
            properties: true,
            layers: true,
            explorer: false,
            findings: false,
        }
    }
}

impl SectionOpen {
    /// Is `section` currently expanded?
    pub(crate) fn is_open(self, section: SidebarSection) -> bool {
        match section {
            SidebarSection::Properties => self.properties,
            SidebarSection::Layers => self.layers,
            SidebarSection::Explorer => self.explorer,
            SidebarSection::Findings => self.findings,
        }
    }

    /// `self` with `section`'s open flag set to `open`.
    pub(crate) fn with(mut self, section: SidebarSection, open: bool) -> Self {
        match section {
            SidebarSection::Properties => self.properties = open,
            SidebarSection::Layers => self.layers = open,
            SidebarSection::Explorer => self.explorer = open,
            SidebarSection::Findings => self.findings = open,
        }
        self
    }

    /// `self` with `section` toggled.
    pub(crate) fn toggled(self, section: SidebarSection) -> Self {
        self.with(section, !self.is_open(section))
    }
}

/// The route-key prefix of a toolbar findings chip (a source label, or `ok`, appended).
/// Each chip needs its own key (keys are unique in the tree); clicking any of them
/// toggles the [`SidebarSection::Findings`] accordion section, exactly like clicking
/// that section's header.
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

/// The route-key prefix of a layer row's set-active affordance (m6 slice B). The
/// full key is `active:` + the slab's [`LayerId::key`](crate::canvas::LayerId::key)
/// (`"active:layer:F.Cu"`), so it can never collide with the `switch:`-prefixed
/// visibility toggle or a canvas target.
const ACTIVE_LAYER_PREFIX: &str = "active:layer:";

/// The set-active route key for the copper slab named `name`.
pub(crate) fn active_layer_key(name: &str) -> String {
    format!("{ACTIVE_LAYER_PREFIX}{name}")
}

/// The copper slab name a route key names, if it is a set-active affordance.
pub(crate) fn active_layer_of_key(route: &str) -> Option<&str> {
    route.strip_prefix(ACTIVE_LAYER_PREFIX)
}

/// Is this event target inside a pane canvas? A pointer event routes to a pane viewport
/// (`canvas:a` / `canvas:b`), a stacked board layer / overlay El (keyed `layer:*` /
/// `overlay:*`), the background dot-grid furniture El (keyed `grid:*`), or a schematic
/// static El (keyed `schematic:*`). All are canvas hits; chrome (toolbar, sidebar, pane
/// headers) is not.
///
/// The `grid:*` arm is load-bearing, not decorative: the grid is child 0 of the board
/// viewport, so on a board that projects *no* layer/overlay buckets (no features and no
/// `board_region` outline) the keyed grid El is the top-most hit-test target. Recognising
/// it here makes a click there route to the pane as an ordinary bare-canvas hit (the
/// geometry-only picker finds nothing, so it deselects / pans) instead of being silently
/// dropped. The pass-through is intentional, not an artefact of layer Els shadowing the
/// grid with a coincident content rect.
pub(crate) fn is_canvas_target(target: Option<&str>) -> bool {
    match target {
        Some(k) => {
            k == PaneId::A.canvas_key()
                || k == PaneId::B.canvas_key()
                || k.starts_with("layer:")
                || k.starts_with("overlay:")
                || k.starts_with("grid:")
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
