//! Pane + recursive split-tree state — the view-dependent half of
//! `gui-architecture.md` through-line 3, plus the small key/const vocabulary
//! shared across the app chrome (route keys, the canvas-target predicate,
//! placeholders). Leaves carry stable pane ids; internal nodes carry their own
//! weighted H/V divider state.

use super::EutecticApp;
use crate::tool::{MeasureState, Tool};
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

    pub(crate) fn token(self) -> &'static str {
        match self {
            ViewKind::Board => "board",
            ViewKind::Schematic => "schematic",
        }
    }

    /// The tools this kind's per-pane strip offers, grouped for the strip's thin
    /// separators (UI-oracle strip anatomy: the shared pick tools first, then the
    /// kind-specific group). Applicability is STRUCTURAL: a tool that makes no
    /// sense for a kind (Route on a schematic) simply isn't in its groups — there
    /// is no disabled state and no applicability check anywhere else. Only tools
    /// that exist today are listed; future tools join their group when they land.
    pub(crate) fn strip_groups(self) -> &'static [&'static [Tool]] {
        match self {
            ViewKind::Board => &[
                &[Tool::Select, Tool::Pan, Tool::Measure, Tool::Delete],
                &[Tool::Place, Tool::Route],
            ],
            // Schematic Delete is deliberately deferred: its source semantics
            // belong to the schematic-editing campaign.
            ViewKind::Schematic => &[&[Tool::Select, Tool::Pan, Tool::Measure]],
        }
    }

    /// Whether `tool` exists in this kind's strip — the structural-applicability
    /// predicate (a strip click for a tool the kind doesn't offer is ignored, so a
    /// synthesized event can never smuggle Route into the schematic slot).
    pub(crate) fn offers_tool(self, tool: Tool) -> bool {
        self.strip_groups().iter().any(|g| g.contains(&tool))
    }
}

/// Maximum number of live leaves in one split tree (UI oracle ruling).
pub const MAX_PANES: usize = 6;

/// A stable pane-slot id. Slots are reused only after their old leaf is closed;
/// unrelated splits and closes never renumber surviving panes. The first two
/// constants preserve the original `canvas:a` / `canvas:b` route vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaneId(u8);

impl PaneId {
    pub const A: PaneId = PaneId(0);
    pub const B: PaneId = PaneId(1);

    pub(crate) fn from_index(index: usize) -> Option<PaneId> {
        (index < MAX_PANES).then_some(PaneId(index as u8))
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }

    pub(crate) fn all_slots() -> impl Iterator<Item = PaneId> {
        (0..MAX_PANES).map(|index| PaneId(index as u8))
    }

    /// The canvas viewport El key for this pane — distinct per pane so its camera is
    /// independent in damascene's `UiState` (through-line 3), *even when several panes show
    /// the same view kind*.
    pub(crate) fn canvas_key(self) -> &'static str {
        const KEYS: [&str; MAX_PANES] = [
            "canvas:a", "canvas:b", "canvas:c", "canvas:d", "canvas:e", "canvas:f",
        ];
        KEYS[self.index()]
    }

    /// The view-switcher button key for a target view kind in this pane.
    pub(crate) fn switch_key(self, v: ViewKind) -> String {
        format!("{}:option:{}", self.view_select_key(), v.token())
    }

    /// The controlled select key for this pane's view-kind dropdown.
    pub(crate) fn view_select_key(self) -> String {
        format!("pane:{}:view", self.tag())
    }

    /// The maximize-toggle button key for this pane.
    pub(crate) fn maximize_key(self) -> String {
        format!("pane:{}:max", self.tag())
    }

    /// Header action keys. Menu actions use the unqualified constants below
    /// and act on the focused leaf.
    pub(crate) fn split_right_key(self) -> String {
        format!("pane:{}:split-right", self.tag())
    }

    pub(crate) fn split_down_key(self) -> String {
        format!("pane:{}:split-down", self.tag())
    }

    pub(crate) fn close_key(self) -> String {
        format!("pane:{}:close", self.tag())
    }

    /// This pane's short key tag (`"a"` / `"b"`), for composed route keys.
    fn tag(self) -> &'static str {
        const TAGS: [&str; MAX_PANES] = ["a", "b", "c", "d", "e", "f"];
        TAGS[self.index()]
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
        const KEYS: [&str; MAX_PANES] = [
            "strip:a:panel",
            "strip:b:panel",
            "strip:c:panel",
            "strip:d:panel",
            "strip:e:panel",
            "strip:f:panel",
        ];
        KEYS[self.index()]
    }
}

/// Parse a strip-button route key back to its `(pane, tool)` target, if `route`
/// is one (`"strip:a:tool:route"` → `(A, Route)`).
pub(crate) fn strip_target_of_key(route: &str) -> Option<(PaneId, Tool)> {
    let rest = route.strip_prefix("strip:")?;
    let (tag, tool_key) = rest.split_once(':')?;
    let pane = PaneId::all_slots().find(|pane| pane.tag() == tag)?;
    let tool = Tool::all().into_iter().find(|t| t.key() == tool_key)?;
    Some((pane, tool))
}

/// Per-pane view state: the *view-dependent* half of through-line 3. A pane is one view
/// over the shared [`DomainState`](crate::app::DomainState), with its own camera keyed by
/// the pane's canvas El key. Milestone 4 makes this real: the pane owns its view kind and
/// whether it has been fit-to-content yet (the initial framing fires once per pane).
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Divider orientation. Horizontal places the second child to the right;
/// vertical places it below the first child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

impl SplitAxis {
    pub(crate) fn damascene(self) -> Axis {
        match self {
            SplitAxis::Horizontal => Axis::Row,
            SplitAxis::Vertical => Axis::Column,
        }
    }
}

/// Stable identity for one internal split. Its key survives changes elsewhere
/// in the tree, which lets nested divider drags route independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SplitId(u8);

impl SplitId {
    fn new(index: u8) -> SplitId {
        SplitId(index)
    }

    pub(crate) fn handle_key(self) -> &'static str {
        const KEYS: [&str; MAX_PANES - 1] = [
            "pane:split",
            "pane:split:1",
            "pane:split:2",
            "pane:split:3",
            "pane:split:4",
        ];
        KEYS[self.0 as usize]
    }

    pub(crate) fn container_key(self) -> &'static str {
        const KEYS: [&str; MAX_PANES - 1] = [
            "pane:split-row",
            "pane:split-row:1",
            "pane:split-row:2",
            "pane:split-row:3",
            "pane:split-row:4",
        ];
        KEYS[self.0 as usize]
    }
}

/// One node in the recursive layout tree.
#[derive(Clone, Debug)]
pub enum PaneNode {
    Leaf(PaneId),
    Split {
        id: SplitId,
        axis: SplitAxis,
        weights: [f32; 2],
        drag: ResizeWeightsDrag,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

impl PaneNode {
    fn leaf_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            PaneNode::Leaf(id) => out.push(*id),
            PaneNode::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    fn first_leaf(&self) -> PaneId {
        match self {
            PaneNode::Leaf(id) => *id,
            PaneNode::Split { first, .. } => first.first_leaf(),
        }
    }

    fn split_leaf(&mut self, source: PaneId, new: PaneId, split: SplitId, axis: SplitAxis) -> bool {
        match self {
            PaneNode::Leaf(id) if *id == source => {
                *self = PaneNode::Split {
                    id: split,
                    axis,
                    weights: [1.0, 1.0],
                    drag: ResizeWeightsDrag::default(),
                    first: Box::new(PaneNode::Leaf(source)),
                    second: Box::new(PaneNode::Leaf(new)),
                };
                true
            }
            PaneNode::Leaf(_) => false,
            PaneNode::Split { first, second, .. } => {
                first.split_leaf(source, new, split, axis)
                    || second.split_leaf(source, new, split, axis)
            }
        }
    }

    fn close_leaf(&mut self, target: PaneId) -> Option<PaneId> {
        let PaneNode::Split { first, second, .. } = self else {
            return None;
        };
        if matches!(first.as_ref(), PaneNode::Leaf(id) if *id == target) {
            let focus = second.first_leaf();
            *self = (**second).clone();
            return Some(focus);
        }
        if matches!(second.as_ref(), PaneNode::Leaf(id) if *id == target) {
            let focus = first.first_leaf();
            *self = (**first).clone();
            return Some(focus);
        }
        first
            .close_leaf(target)
            .or_else(|| second.close_leaf(target))
    }

    fn collect_splits(&self, out: &mut Vec<SplitId>) {
        if let PaneNode::Split {
            id, first, second, ..
        } = self
        {
            out.push(*id);
            first.collect_splits(out);
            second.collect_splits(out);
        }
    }

    pub(crate) fn split_mut(
        &mut self,
        target: SplitId,
    ) -> Option<(SplitAxis, &mut [f32; 2], &mut ResizeWeightsDrag)> {
        match self {
            PaneNode::Leaf(_) => None,
            PaneNode::Split {
                id,
                axis,
                weights,
                drag,
                first,
                second,
            } => {
                if *id == target {
                    Some((*axis, weights, drag))
                } else {
                    first.split_mut(target).or_else(|| second.split_mut(target))
                }
            }
        }
    }
}

/// The whole recursive pane layout. The default is byte-for-byte the original
/// board | schematic split geometry: row axis, equal weights, original keys.
#[derive(Clone, Debug)]
pub struct PaneTree {
    pub(crate) root: PaneNode,
}

impl Default for PaneTree {
    fn default() -> Self {
        PaneTree {
            root: PaneNode::Split {
                id: SplitId::new(0),
                axis: SplitAxis::Horizontal,
                weights: [1.0, 1.0],
                drag: ResizeWeightsDrag::default(),
                first: Box::new(PaneNode::Leaf(PaneId::A)),
                second: Box::new(PaneNode::Leaf(PaneId::B)),
            },
        }
    }
}

impl PaneTree {
    pub fn leaf_count(&self) -> usize {
        self.root.leaf_count()
    }

    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::with_capacity(self.leaf_count());
        self.root.collect_leaves(&mut out);
        out
    }

    pub(crate) fn split_ids(&self) -> Vec<SplitId> {
        let mut out = Vec::with_capacity(self.leaf_count().saturating_sub(1));
        self.root.collect_splits(&mut out);
        out
    }

    pub(crate) fn split_leaf(&mut self, source: PaneId, new: PaneId, axis: SplitAxis) -> bool {
        if self.leaf_count() >= MAX_PANES {
            return false;
        }
        let used = self.split_ids();
        let Some(index) = (0..MAX_PANES - 1).find(|index| !used.contains(&SplitId(*index as u8)))
        else {
            return false;
        };
        self.root
            .split_leaf(source, new, SplitId::new(index as u8), axis)
    }

    /// Remove a leaf and collapse its parent to the sibling subtree. Returns
    /// the sibling leaf that should receive focus.
    pub(crate) fn close_leaf(&mut self, target: PaneId) -> Option<PaneId> {
        (self.leaf_count() > 1)
            .then(|| self.root.close_leaf(target))
            .flatten()
    }
}

/// A pane index into stable slot storage.
pub(crate) fn pane_index(p: PaneId) -> usize {
    p.index()
}

impl EutecticApp {
    /// Move the shared measurement preview to `pane`. Coordinate spaces are
    /// comparable only within one view kind, so crossing Board ↔ Schematic
    /// cancels the old anchor before the new pane can update its cursor.
    pub(crate) fn claim_measure_pane(&self, pane: PaneId) {
        let previous = self.measure_pane.get();
        if self.pane_view(previous) != self.pane_view(pane) {
            self.measure.set(MeasureState::default());
        }
        self.measure_pane.set(pane);
    }

    /// The active tool of view kind `kind`. A kind with no entry defaults to
    /// [`Tool::Select`].
    pub fn tool_for(&self, kind: ViewKind) -> Tool {
        self.tools.borrow().get(&kind).copied().unwrap_or_default()
    }

    /// Set a view kind's active tool. Changing a kind cancels a measurement
    /// owned by that kind; board changes also cancel route/refinement previews.
    pub fn set_tool(&self, kind: ViewKind, tool: Tool) {
        if self.tool_for(kind) != tool {
            let measure_kind = self.pane_view(self.measure_pane.get());
            if measure_kind == kind {
                self.measure.set(MeasureState::default());
            }
            if kind == ViewKind::Board {
                *self.route.borrow_mut() = None;
                self.route_pane.set(None);
                *self.trace_drag.borrow_mut() = None;
                self.clear_place_cursor();
                if tool != Tool::Place {
                    self.library_browser_open.set(false);
                    self.clear_library_preview_textures();
                }
            }
            *self.camera_pan.borrow_mut() = None;
        }
        self.tools.borrow_mut().insert(kind, tool);
    }

    /// The focused pane's view-kind tool without changing either kind's memory.
    pub fn live_tool(&self) -> Tool {
        let kind = self.pane_view(self.focused_pane.get());
        self.tool_for(kind)
    }
}

/// Focused-pane View menu actions.
pub(crate) const SPLIT_RIGHT_KEY: &str = "pane:split-right";
pub(crate) const SPLIT_DOWN_KEY: &str = "pane:split-down";
pub(crate) const CLOSE_PANE_KEY: &str = "pane:close";
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

/// The dark canvas background behind the board — an ECAD-dark near-black.
pub(crate) const CANVAS_BG: Color = Color::srgb_token("eutectic.canvas.bg", 0x12, 0x14, 0x18, 0xff);

/// The event-route key of a layer's visibility switch.
pub(crate) fn switch_key(layer_key: &str) -> String {
    format!("switch:{layer_key}")
}

/// The route-key prefix of a layer row's set-active affordance (m6 slice B). The
/// full key is `active:` + the slab's [`LayerId::key`](crate::pick::LayerId::key)
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

/// Is this event target inside a pane canvas? On the owned canvas every
/// pane's interior is ONE stable keyed container (`canvas:a` … `canvas:f`) — the
/// viewport-era child Els (`layer:*` / `overlay:*` / `grid:*` /
/// `schematic:*`) died with the viewport path (WP3), so those keys no longer
/// occur in the tree. Chrome (toolbar, sidebar, pane headers) is not a
/// canvas hit.
pub(crate) fn is_canvas_target(target: Option<&str>) -> bool {
    target.is_some_and(|key| PaneId::all_slots().any(|pane| key == pane.canvas_key()))
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
