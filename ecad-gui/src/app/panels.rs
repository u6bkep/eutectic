//! Panel + chrome builders — every `build`-time projection of app state into the
//! widget tree: the toolbar, the two-pane split, the pane canvases + overlays, the
//! right sidebar (inspector / findings / explorer / layers), the status bar, and the
//! DRC chip, plus the findings-row click handler (`select_finding`). Split out of
//! `app.rs` as pure code motion — the `build`/`on_event`/pointer plumbing lives in
//! [`crate::app::events`].

use crate::app::domain::{BoardView, DocStats};
use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::pane::{
    CANVAS_BG, FINDINGS_TOGGLE_KEY, LAYOUT_TOGGLE_KEY, SPLIT_HANDLE_KEY, SPLIT_ROW_KEY,
    finding_row_key, findings_chip_key, pane_index, pane_placeholder, switch_key,
};
use crate::app::{EcadApp, PaneId, PaneLayout, ViewKind};
use crate::canvas::pick::SemanticId;
use crate::canvas::{BoardLayer, Overlay};
use crate::explorer::Explorer;
use crate::findings::{FindingSource, Findings};
use crate::highlight::HighlightSets;
use crate::inspector::InspectorData;
use crate::tool::{Tool, format_readout};
use damascene_core::prelude::*;
use ecad_core::diagnostic::Severity;
use ecad_core::geom::Shape2D;
use ecad_core::id::NetId;

impl EcadApp {
    /// Is the layer with `key` currently visible? Layers default on; the toggle
    /// records only the *hidden* set.
    pub(crate) fn layer_visible(&self, key: &str) -> bool {
        !self.hidden.borrow().contains(key)
    }

    /// The viewer body: the toolbar, the two-pane split (center), the right sidebar
    /// (inspector + explorer + layer panel), and the status bar. Reached when the doc
    /// loaded (at least one pane always renders — a board pane falls back to a placeholder
    /// if its projection failed, a schematic pane if the doc has no components).
    pub(crate) fn viewer_body(&self, cx: &BuildCx) -> El {
        // The active board pane's zoom drives the toolbar/status readout (whichever pane A
        // shows a board, else pane B, else 1.0). The cursor readout is set per event.
        let zoom = self.readout_zoom(cx);

        // The shared cross-view highlight sets, projected once per frame from the selection.
        let sets = self.highlight_sets();

        let split = self.pane_split(cx, &sets);

        column([
            self.viewer_toolbar(zoom),
            row([split, self.right_sidebar()])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
            self.status_bar(zoom),
        ])
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
        .height(Size::Fill(1.0))
    }

    /// The zoom to display in the toolbar / status bar: the active board pane's zoom
    /// (whichever pane shows a board), else 1.0.
    fn readout_zoom(&self, cx: &BuildCx) -> f32 {
        let panes = self.panes.borrow();
        for (i, p) in panes.iter().enumerate() {
            if p.view == ViewKind::Board {
                let id = if i == 0 { PaneId::A } else { PaneId::B };
                return cx.viewport_view(id.canvas_key()).map_or(1.0, |v| v.zoom);
            }
        }
        1.0
    }

    /// The shared cross-view highlight sets for this frame — the selection + hover ids,
    /// projected through [`HighlightSets`] so both panes expand the same way.
    fn highlight_sets(&self) -> HighlightSets {
        match &self.domain.doc {
            Ok(doc) => {
                let sel = self.domain.selection.borrow();
                // Selection + hover both cross-highlight (hover is the pre-select cue).
                HighlightSets::project(sel.selected().chain(sel.hovered()), doc, &self.domain.lib)
            }
            Err(_) => HighlightSets::default(),
        }
    }

    /// The two-pane split (dual = row, stacked = column), with a draggable resize handle
    /// between the panes — or, when a pane is maximized, that one pane full-bleed.
    fn pane_split(&self, cx: &BuildCx, sets: &HighlightSets) -> El {
        if let Some(max) = self.maximized.get() {
            return self.pane_el(cx, max, sets);
        }
        let a = self.pane_el(cx, PaneId::A, sets);
        let b = self.pane_el(cx, PaneId::B, sets);
        let axis = match self.layout.get() {
            PaneLayout::Dual => Axis::Row,
            PaneLayout::Stacked => Axis::Column,
        };
        let w = self.split_weights.get();
        let a = a.width(Size::Fill(w[0])).height(Size::Fill(w[0]));
        let b = b.width(Size::Fill(w[1])).height(Size::Fill(w[1]));
        let children = [a, resize_handle(SPLIT_HANDLE_KEY, axis), b];
        let container = match self.layout.get() {
            PaneLayout::Dual => row(children),
            PaneLayout::Stacked => column(children),
        };
        container
            .key(SPLIT_ROW_KEY)
            .gap(tokens::SPACE_2)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// One pane: a header row (view-kind label + switcher + maximize toggle) over the
    /// pane's canvas (board or schematic). Fill in both axes so the split weights govern
    /// its size.
    fn pane_el(&self, cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let view = self.panes.borrow()[pane_index(pane)].view;
        let canvas = match view {
            ViewKind::Board => self.board_canvas(cx, pane, sets),
            ViewKind::Schematic => self.schematic_canvas(cx, pane, sets),
        };
        column([self.pane_header(pane, view), canvas])
            .gap(tokens::SPACE_1)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// A pane header (mockup anatomy): the view-kind switcher (a segmented control of
    /// toggle buttons, the active one filled) and a maximize toggle on the right.
    fn pane_header(&self, pane: PaneId, view: ViewKind) -> El {
        let switch_buttons: Vec<El> = ViewKind::all()
            .into_iter()
            .map(|v| {
                let b = button(v.label()).key(pane.switch_key(v));
                if v == view { b.primary() } else { b }
            })
            .collect();
        let max_label = if self.maximized.get() == Some(pane) {
            "Restore"
        } else {
            "Maximize"
        };
        toolbar([
            row(switch_buttons).gap(tokens::SPACE_1),
            spacer(),
            button(max_label).key(pane.maximize_key()),
        ])
        .gap(tokens::SPACE_2)
        .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// A board pane's canvas: the cached layer Els + the per-frame overlay, in a viewport
    /// keyed to *this pane* (independent camera). Falls back to a placeholder when the
    /// board projection failed.
    fn board_canvas(&self, _cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let derived = self.derived.borrow();
        let Some(view) = &derived.board else {
            return pane_placeholder("No board to display");
        };
        // Per-pane El keys: two board panes render the same layers, so namespace each
        // layer / overlay El by the pane (keys must be unique in the tree). The event
        // router still recognises these as canvas targets (the `layer:` / `overlay:`
        // prefixes survive) and the pane is resolved by pointer rect, not by key.
        let prefix = pane.canvas_key();
        let mut children: Vec<El> = view
            .canvas
            .layer_els(&view.layers, |id| self.layer_visible(&id.key()))
            .into_iter()
            .enumerate()
            .map(|(i, el)| el.key(format!("layer:{prefix}:{i}")))
            .collect();
        let overlay = self.build_board_overlay(view, pane, sets, &derived.findings);
        if let Some(el) = view.canvas.overlay_el(&overlay) {
            // Re-key the overlay per pane (the canvas hardcodes "overlay:dynamic"); wrap it
            // in a keyed container so two board panes' overlays don't collide.
            children.push(el.key(format!("overlay:{prefix}")));
        }
        viewport(children)
            .key(pane.canvas_key())
            .min_zoom(0.1)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// A schematic pane's canvas: the cached schematic asset + the per-frame highlight
    /// overlay, in a viewport keyed to this pane. Falls back to a placeholder when the doc
    /// has no components.
    fn schematic_canvas(&self, _cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let derived = self.derived.borrow();
        let Some(view) = &derived.schematic else {
            return pane_placeholder("No schematic to display");
        };
        let static_key = format!("schematic:{}", pane.canvas_key());
        let mut children = vec![view.static_el(&static_key)];
        // Schematic-side findings (ERC / floating-pad with entity refs) halo the symbol:
        // union their entity/net refs into the overlay id set so the affected symbol +
        // net wires ring in the finding accent alongside any selection highlight.
        let finding_ids = self.schematic_finding_ids(&derived.findings);
        let overlay_ids: std::collections::BTreeSet<SemanticId> =
            sets.schematic_ids().union(&finding_ids).cloned().collect();
        if let Some(el) = view.overlay_el(&overlay_ids, pane.overlay_key()) {
            children.push(el);
        }
        viewport(children)
            .key(pane.canvas_key())
            .min_zoom(0.02)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// Build a board pane's dynamic overlay from the cross-view highlight sets + the
    /// measure preview (measure only draws in the pane it is happening in). Highlight
    /// geometry is re-derived from the pick candidates by id (commitment 2). A candidate
    /// lights up when its id — or its net — is in the board highlight set.
    pub(crate) fn build_board_overlay(
        &self,
        view: &BoardView,
        pane: PaneId,
        sets: &HighlightSets,
        findings: &Findings,
    ) -> Overlay {
        let mut highlights: Vec<(Shape2D, bool)> = Vec::new();
        for c in &view.candidates {
            if !self.layer_visible(&c.layer.key()) {
                continue;
            }
            let net = self.candidate_net(&c.id);
            if sets.board_matches(&c.id, net.as_ref()) {
                // Committed selection reads bright; a hover-only match reads dim. A
                // candidate is a hover if its id is hovered and not selected.
                let sel = self.domain.selection.borrow();
                let hovered = sel.is_hovered(&c.id) && !sel.is_selected(&c.id);
                highlights.push((c.shape.clone(), hovered));
            }
        }
        let measure = if self.tool.get() == Tool::Measure && self.measure_pane.get() == pane {
            self.measure.get().segment()
        } else {
            None
        };
        // Findings with a derived board point become violation markers (both board
        // panes show them — a finding is a property of the board, not a pane).
        let finding_markers: Vec<(ecad_core::coord::Point, bool)> = findings
            .items
            .iter()
            .filter_map(|f| {
                let (mx, my) = f.board_mm?;
                Some((
                    ecad_core::coord::Point {
                        x: (mx * ecad_core::coord::MM as f32).round() as ecad_core::coord::Nm,
                        y: (my * ecad_core::coord::MM as f32).round() as ecad_core::coord::Nm,
                    },
                    f.is_error(),
                ))
            })
            .collect();
        Overlay {
            highlights,
            measure,
            findings: finding_markers,
        }
    }

    /// The semantic ids the schematic overlay should ring for findings: the entity /
    /// pin / net refs of every finding (ERC multiple-drivers on a net, a floating pad
    /// on a part). The schematic candidates key on Part / Pin / Net, so these light up
    /// the affected symbol + net wires.
    fn schematic_finding_ids(&self, findings: &Findings) -> std::collections::BTreeSet<SemanticId> {
        findings
            .items
            .iter()
            .flat_map(|f| f.refs.iter().cloned())
            .collect()
    }

    /// The net a board candidate's id belongs to, if any (for the net-expansion match).
    fn candidate_net(&self, id: &SemanticId) -> Option<NetId> {
        let doc = self.domain.doc.as_ref().ok()?;
        match id {
            SemanticId::Trace(t) => doc.traces.get(t).map(|t| t.net.clone()),
            SemanticId::Via(v) => doc.vias.get(v).map(|v| v.net.clone()),
            SemanticId::Pour { net, .. } => Some(net.clone()),
            SemanticId::Pin { comp, pin } => {
                let pr = ecad_core::doc::PinRef::new(comp, pin);
                doc.nets
                    .iter()
                    .find(|(_, n)| n.members.contains(&pr))
                    .map(|(nid, _)| nid.clone())
            }
            _ => None,
        }
    }

    /// The right sidebar: the properties inspector (above), the explorer (middle), and the
    /// board layer panel (below), matching the mockup anatomy (Properties above Explorer).
    fn right_sidebar(&self) -> El {
        let derived = self.derived.borrow();
        let mut children = vec![
            self.inspector_panel(),
            self.findings_panel(&derived.findings),
            self.explorer_panel(&derived.explorer),
        ];
        // The layer panel applies to board panes; show it whenever a board projection
        // exists (global layer visibility is fine for v1).
        if let Some(view) = &derived.board {
            children.push(self.layer_panel(&view.layers));
        }
        scroll([column(children).gap(tokens::SPACE_3).width(Size::Fill(1.0))])
            .width(Size::Fixed(260.0))
            .height(Size::Fill(1.0))
    }

    /// The findings panel (right sidebar, collapsible like the explorer): a header with
    /// the error/warning tally, then one click-to-select row per finding (a severity
    /// badge beside the code and message). Clicking a row selects the finding's refs
    /// (cross-highlighting the panes) and centres the focused board pane on the
    /// violation. Collapsed to just the header when `findings_open` is false or when
    /// there are no findings (a clean board shows a compact "no issues" line).
    fn findings_panel(&self, findings: &Findings) -> El {
        let open = self.findings_open.get();
        let title = if findings.is_clean() {
            "Findings".to_string()
        } else {
            format!(
                "Findings ({} err, {} warn)",
                findings.errors, findings.warnings
            )
        };
        let toggle = button(if open { "Hide" } else { "Show" }).key(FINDINGS_TOGGLE_KEY);
        let header = sidebar_header([row([h3(title).width(Size::Fill(1.0)).ellipsis(), toggle])
            .align(Align::Center)
            .width(Size::Fill(1.0))]);
        if !open {
            return sidebar([header]).width(Size::Fill(1.0)).height(Size::Hug);
        }
        if findings.is_clean() {
            return sidebar([
                header,
                sidebar_group([text("No issues — DRC clean.").muted()]),
            ])
            .width(Size::Fill(1.0))
            .height(Size::Hug);
        }
        let rows: Vec<El> = findings
            .items
            .iter()
            .enumerate()
            .map(|(i, f)| self.finding_row(i, f))
            .collect();
        sidebar([
            header,
            sidebar_group([column(rows).gap(tokens::SPACE_1).width(Size::Fill(1.0))]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// One findings row: a severity badge (error red / warning amber) + the code +
    /// message, as a click-to-select focusable row keyed by index. Built on the same
    /// focusable-list-item anatomy as `sidebar_menu_button` (which is label-only), so a
    /// click routes to the app and the row reads as an interactive nav entry.
    ///
    /// An [informational](crate::findings::Finding::is_informational) finding (an
    /// unresolved part / library-resolution note — no refs, no board point, nothing to
    /// navigate to) renders the same anatomy WITHOUT the interactive affordances: no
    /// key, not focusable, no pointer cursor — a plain data row.
    fn finding_row(&self, index: usize, f: &crate::findings::Finding) -> El {
        let sev = if f.is_error() {
            badge("ERR").destructive()
        } else {
            badge("WARN").warning()
        };
        let body = column([
            text(f.code).mono().caption(),
            text(f.message.clone()).width(Size::Fill(1.0)).wrap_text(),
        ])
        .gap(0.0)
        .width(Size::Fill(1.0));
        let base = row([sev, body])
            .style_profile(StyleProfile::Solid)
            .metrics_role(MetricsRole::ListItem)
            .fill(tokens::CARD)
            .radius(tokens::RADIUS_SM)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
            .align(Align::Center)
            .width(Size::Fill(1.0));
        if f.is_informational() {
            return base;
        }
        base.key(finding_row_key(index))
            .focusable()
            .cursor(Cursor::Pointer)
            .ghost()
    }

    /// The explorer panel (mockup NetExplorer anatomy): Components + Nets sections, each a
    /// list of click-to-select rows with a count badge; the selected row gets the mockup's
    /// selected cue (`sidebar_menu_button`'s `current` treatment).
    fn explorer_panel(&self, explorer: &Explorer) -> El {
        let sel = self.domain.selection.borrow();
        let comp_rows: Vec<El> = explorer
            .components
            .iter()
            .map(|r| self.explorer_row(r, sel.is_selected(&r.id)))
            .collect();
        let net_rows: Vec<El> = explorer
            .nets
            .iter()
            .map(|r| self.explorer_row(r, sel.is_selected(&r.id)))
            .collect();
        sidebar([
            sidebar_header([h3("Explorer")]),
            sidebar_group([
                sidebar_group_label(format!("Components ({})", comp_rows.len())),
                column(comp_rows)
                    .gap(tokens::SPACE_1)
                    .width(Size::Fill(1.0)),
            ]),
            sidebar_group([
                sidebar_group_label(format!("Nets ({})", net_rows.len())),
                column(net_rows).gap(tokens::SPACE_1).width(Size::Fill(1.0)),
            ]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// One explorer row: a click-to-select `sidebar_menu_button` labelled with the id +
    /// secondary text + count badge, `current` when it is the selection.
    fn explorer_row(&self, r: &crate::explorer::ExplorerRow, current: bool) -> El {
        let label = if r.secondary.is_empty() {
            format!("{}  [{}]", r.label, r.count)
        } else {
            format!("{}  ({})  [{}]", r.label, r.secondary, r.count)
        };
        sidebar_menu_button(label, current).key(r.key.clone())
    }

    /// The inspector panel: an identity card + key/value rows for the single selected
    /// entity, or the m2 stats card when nothing is selected. Works regardless of which
    /// pane the selection came from (the selection is shared, semantic).
    fn inspector_panel(&self) -> El {
        let doc = match &self.domain.doc {
            Ok(doc) => doc,
            Err(_) => return self.empty_inspector(),
        };
        let sel = self.domain.selection.borrow();
        let Some(id) = sel.single() else {
            return self.empty_inspector();
        };
        let Some(data) = InspectorData::project(id, doc, &self.domain.lib) else {
            return self.empty_inspector();
        };

        let mut children: Vec<El> =
            vec![column([text(data.kind).muted().mono(), h3(data.primary)]).gap(tokens::SPACE_1)];
        for r in &data.rows {
            children.push(field_row(r.key.clone(), text(r.value.clone()).mono()));
        }
        sidebar([sidebar_header([h3("Properties")]), sidebar_group(children)])
            .width(Size::Fill(1.0))
            .height(Size::Hug)
    }

    /// The inspector's empty state: the m2 doc stats, rendered as sidebar rows.
    fn empty_inspector(&self) -> El {
        match &self.domain.doc {
            Ok(doc) => {
                let s = DocStats::of(doc);
                let board = match s.board_mm {
                    Some((w, h)) => format!("{w:.1} x {h:.1} mm"),
                    None => "none".to_string(),
                };
                sidebar([
                    sidebar_header([h3("Properties")]),
                    sidebar_group([
                        text("No selection").muted(),
                        field_row("Parts", text(s.parts.to_string()).mono()),
                        field_row("Nets", text(s.nets.to_string()).mono()),
                        field_row("Copper layers", text(s.layers.to_string()).mono()),
                        field_row("Board", text(board).mono()),
                    ]),
                ])
                .width(Size::Fill(1.0))
                .height(Size::Hug)
            }
            Err(_) => sidebar([sidebar_header([h3("Properties")])])
                .width(Size::Fill(1.0))
                .height(Size::Hug),
        }
    }

    /// The toolbar: app title, filename badge, the dual/stacked layout toggle, the global
    /// tool palette, and Fit / Reset framing buttons + a live zoom-percent readout.
    fn viewer_toolbar(&self, zoom: f32) -> El {
        let name = self
            .domain
            .filename
            .clone()
            .unwrap_or_else(|| "untitled".into());
        let active = self.tool.get();
        let tool_buttons: Vec<El> = Tool::all()
            .into_iter()
            .map(|t| {
                let b = button(t.label()).key(t.key());
                if t == active { b.primary() } else { b }
            })
            .collect();
        let layout_label = match self.layout.get() {
            PaneLayout::Dual => "Dual",
            PaneLayout::Stacked => "Stacked",
        };
        // The per-source findings chips (mockup chrome): one chip per source (DRC / ERC /
        // NET / LIB) shown only when nonzero, tinted by that source's worst severity; a
        // single neutral ✓ chip when every source is clean. Any chip click toggles the
        // findings panel. The reload-error banner chip (permissive philosophy) sits
        // beside them whenever the freshest source failed to load — unmissable, never a
        // toast.
        let mut lead: Vec<El> = vec![toolbar_title("ecad"), badge(name).info()];
        lead.extend(self.findings_chips());
        if let Some(err) = &self.domain.reload_error {
            lead.push(reload_error_chip(err));
        }
        lead.push(button(layout_label).key(LAYOUT_TOGGLE_KEY));
        lead.push(button("Libraries").key(LIBRARIES_TOGGLE_KEY));
        lead.push(spacer());
        lead.push(row(tool_buttons).gap(tokens::SPACE_1));
        lead.push(text(format!("{:.0}%", zoom * 100.0)).muted().mono());
        lead.push(button("Fit").key("fit"));
        lead.push(button("Reset").key("reset"));
        toolbar(lead)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
    }

    /// The per-source findings chips (mockup menu-bar chrome): one chip per
    /// [`FindingSource`] that has findings this revision, in DRC/ERC/NET/LIB order,
    /// each reading `"NAME n"` (n = total findings for that source) and tinted by the
    /// source's worst severity — red (`destructive`) if any error, amber (`warning`)
    /// otherwise, both through the theme's semantic colors. When every source is clean
    /// a single neutral `"✓"` chip is shown instead. Every chip (including the ✓ one)
    /// is a click-to-toggle-the-findings-panel affordance keyed distinctly. Reads the
    /// cached findings — never recomputes.
    fn findings_chips(&self) -> Vec<El> {
        let findings = &self.derived.borrow().findings;
        // A clickable chip: keyed + focusable + pointer cursor, so a click routes to the
        // app (handled as a findings-panel toggle) exactly like the panel's Hide/Show.
        let chip = |label: String, tag: &str| {
            badge(label)
                .key(findings_chip_key(tag))
                .focusable()
                .cursor(Cursor::Pointer)
        };
        let mut chips: Vec<El> = Vec::new();
        for source in FindingSource::all() {
            let Some((count, worst)) = findings.source_summary(source) else {
                continue;
            };
            let c = chip(format!("{} {count}", source.label()), source.label());
            chips.push(match worst {
                Severity::Error => c.destructive(),
                _ => c.warning(),
            });
        }
        if chips.is_empty() {
            // All sources clean → a single neutral ✓ chip, still click-to-toggle.
            chips.push(chip("✓".to_string(), "ok").muted());
        }
        chips
    }

    /// The right sidebar layer panel: one row per layer (top of the stack first),
    /// each a colour swatch, name, and a visibility switch. Order mirrors draw
    /// order reversed, so the top copper reads at the top of the list.
    fn layer_panel(&self, layers: &[BoardLayer]) -> El {
        // Draw order is bottom-first; the panel lists top-first.
        let rows: Vec<El> = layers.iter().rev().map(|l| self.layer_row(l)).collect();
        sidebar([
            sidebar_header([h3("Layers")]),
            sidebar_group([
                sidebar_group_label("Board"),
                column(rows).gap(tokens::SPACE_1),
            ]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// One layer-panel row: colour swatch + name + a visibility [`switch`].
    fn layer_row(&self, l: &BoardLayer) -> El {
        let key = l.id.key();
        let swatch = El::new(Kind::Custom("layer-swatch"))
            .fill(l.color)
            .stroke(tokens::BORDER)
            .radius(3.0)
            .width(Size::Fixed(14.0))
            .height(Size::Fixed(14.0));
        row([
            swatch,
            text(l.name.clone()).width(Size::Fill(1.0)),
            switch(switch_key(&key), self.layer_visible(&key)),
        ])
        .align(Align::Center)
        .gap(tokens::SPACE_2)
        .padding(Sides::y(tokens::SPACE_1))
    }

    /// The bottom status bar (mockup taste): the live cursor position in board
    /// coordinates and the zoom percent. The cursor readout updates on pointer
    /// enter and while panning — see the module deviation note on free-hover.
    fn status_bar(&self, zoom: f32) -> El {
        let cursor = match self.cursor_board_mm.get() {
            Some((x, y)) => format!("X {x:.2}  Y {y:.2} mm"),
            None => "X --  Y -- mm".to_string(),
        };
        let mut items: Vec<El> = vec![text(cursor).muted().mono()];

        // The measure readout (mockup taste: dx/dy/dist in the status bar) — shown only
        // in Measure mode with a segment in progress.
        if self.tool.get() == Tool::Measure
            && let Some((dx, dy, dist)) = self.measure.get().readout()
        {
            items.push(text(format_readout(dx, dy, dist)).mono());
        }

        items.push(spacer());

        // The selected net name (mockup taste: the status bar carries the selected
        // net). Derived from the single selection via the inspector projection.
        if let Some(net) = self.selected_net() {
            items.push(badge(format!("net {net}")).info());
        }
        // Compact DRC state (mockup status-bar chrome).
        {
            let findings = &self.derived.borrow().findings;
            let drc = if findings.is_clean() {
                "DRC: clean".to_string()
            } else {
                format!("DRC: {} err {} warn", findings.errors, findings.warnings)
            };
            items.push(text(drc).muted().mono());
        }
        items.push(text(format!("Zoom {:.0}%", zoom * 100.0)).muted().mono());

        toolbar(items)
            .gap(tokens::SPACE_3)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_1))
    }

    /// The net name of the current single selection, if it belongs to one (a trace /
    /// via / pin / pour / net selection). `None` for a part or empty selection.
    fn selected_net(&self) -> Option<String> {
        let doc = self.domain.doc.as_ref().ok()?;
        let sel = self.domain.selection.borrow();
        let id = sel.single()?;
        InspectorData::project(id, doc, &self.domain.lib)?.net
    }

    /// Select the finding at `index` (a findings-panel row click): fold ALL of its
    /// semantic refs into the selection (so the panes cross-highlight the offending
    /// nets / parts / pins), and — if the finding has a derived board point — queue a
    /// `CenterOn` on the focused board pane so the violation comes into view.
    ///
    /// # Click-to-zoom gap (deviation)
    ///
    /// damascene 0.4.5 has **no frame-this-rect ViewportRequest** — only `FitContent`,
    /// `ResetView`, and `CenterOn { key, point }`. So "zoom the focused board pane to the
    /// violation" is realised as a **`CenterOn`** (pan to the point, keeping the current
    /// zoom) rather than a true frame-to-rect. The finding's board point is centred; the
    /// zoom is left as the user set it. Recorded as a deviation in the report.
    pub(crate) fn select_finding(&self, index: usize, cx: &EventCx) {
        let derived = self.derived.borrow();
        let Some(f) = derived.findings.items.get(index) else {
            return;
        };
        // Informational rows (unresolved part / library note) have nothing to select
        // or navigate to — and they render without a route key, so this is belt and
        // braces against a stale index.
        if f.is_informational() {
            return;
        }
        // Fold every ref into the selection (multi-select — a clearance highlights BOTH
        // nets). Clear first, then add each ref.
        {
            let mut sel = self.domain.selection.borrow_mut();
            sel.clear();
            for r in &f.refs {
                sel.add(r.clone());
            }
        }
        // CenterOn the focused board pane, if the finding has a board point. The request
        // wants a CONTENT-space point (logical px, pre-transform); the canvas maps the
        // finding's board-mm point through its board→content-px transform using the
        // pane's live laid-out rect (so the pan is exact regardless of the pane's
        // aspect ratio / fitted scale).
        if let (Some((mx, my)), Some(view)) = (f.board_mm, &derived.board)
            && let Some(pane) = self.focused_board_pane()
            && let Some(rect) = cx.rect_of_key(pane.canvas_key())
            && let Some(point) = view
                .canvas
                .board_mm_to_content_px((mx, my), (rect.x, rect.y, rect.w, rect.h))
        {
            self.pending.borrow_mut().push(ViewportRequest::CenterOn {
                key: pane.canvas_key().to_string(),
                point,
            });
        }
    }

    /// The board pane to focus for click-to-zoom: the first pane currently showing a
    /// board (A preferred), respecting a maximized pane. `None` when no board pane is
    /// visible (both panes schematic, or the board didn't project).
    fn focused_board_pane(&self) -> Option<PaneId> {
        let panes = self.panes.borrow();
        let visible = |id: PaneId| self.maximized.get().map(|m| m == id).unwrap_or(true);
        for (i, p) in panes.iter().enumerate() {
            let id = if i == 0 { PaneId::A } else { PaneId::B };
            if p.view == ViewKind::Board && visible(id) {
                return Some(id);
            }
        }
        None
    }
}

/// The persistent reload-error chip (m5): an unmissable destructive badge in the
/// toolbar shown whenever the *freshest* source failed to parse/elaborate while the
/// last-good doc stays rendered. Not a toast — it persists until a good reload clears
/// `reload_error`. The full error is available in the badge label's first line.
fn reload_error_chip(err: &str) -> El {
    // Compact the multi-line diagnostic to its first line for the chip; the banner is a
    // glanceable "reload failed" cue, not the full report surface.
    let first = err.lines().next().unwrap_or(err);
    badge(format!("reload failed: {first}")).destructive()
}

/// The parse/elaborate-failure body: surface the error, never crash (the
/// permissive philosophy starts here).
pub(crate) fn error_card(message: &str) -> El {
    // The empty state uses the same path — "no document" is just an `Err`.
    if message == "no document" {
        return titled_card(
            "No document",
            [text("Pass a path to a .ecad file to load a document.").muted()],
        )
        .width(Size::Fixed(420.0));
    }
    alert([
        alert_title("Could not load document"),
        alert_description(message.to_string()),
    ])
    .destructive()
    .width(Size::Fixed(420.0))
}
