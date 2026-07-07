//! The pane-tree region — the two-pane split and everything composed inside it:
//! `viewer_body` (the whole viewer composition), the split + pane headers, the
//! board / schematic canvas pane wrappers with their floating tool strips
//! ([`strip`]), the per-frame board overlay builder, the shared cross-view
//! highlight projection, and the conflict banner. Moved out of `app/panels.rs`
//! as pure code motion (gui-module-split); this is the region a future
//! split-tree rework will own.

pub(crate) mod strip;

use crate::app::domain::BoardView;
use crate::app::pane::{
    CANVAS_BG, CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, SPLIT_HANDLE_KEY, SPLIT_ROW_KEY, pane_index,
    pane_placeholder,
};
use crate::app::{EcadApp, PaneId, PaneLayout, ViewKind};
use crate::canvas::Overlay;
use crate::canvas::pick::SemanticId;
use crate::findings::Findings;
use crate::highlight::HighlightSets;
use crate::tool::Tool;
use damascene_core::prelude::*;
use ecad_core::geom::Shape2D;
use ecad_core::id::NetId;

impl EcadApp {
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

        // The two top chrome strips (oracle regions 1 + 2): the menu bar over the
        // icon toolbar. The open menu itself renders as a root overlay in `build`.
        let mut children = vec![self.menubar_bar(), self.viewer_toolbar()];
        // The persistent conflict banner (m6 save model): rendered whenever the
        // watcher delivered an external change while the doc was dirty. It stays
        // until one of its two explicit actions resolves it — never a toast,
        // never silent last-writer.
        if let Some(banner) = self.conflict_banner() {
            children.push(banner);
        }
        children.push(
            row([split, self.right_sidebar()])
                .gap(tokens::SPACE_3)
                .width(Size::Fill(1.0))
                .height(Size::Fill(1.0)),
        );
        children.push(self.status_bar(zoom));
        column(children)
            .gap(tokens::SPACE_3)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0))
    }

    /// The conflict banner (m6): "file changed on disk; unsaved edits" with the
    /// two explicit resolutions — Reload (discard my edits, apply disk) and Keep
    /// mine (dismiss; doc stays dirty; the next save overwrites disk). `None`
    /// when no conflict is pending.
    fn conflict_banner(&self) -> Option<El> {
        self.domain.edit.conflict.as_ref()?;
        Some(
            alert([
                alert_title("File changed on disk"),
                alert_description(
                    "The file was modified outside this session while you have unsaved \
                     edits. Reload discards your edits and follows the disk; Keep mine \
                     keeps editing (the next Save overwrites the disk).",
                ),
                row([
                    button("Reload from disk").key(CONFLICT_RELOAD_KEY),
                    button("Keep mine").key(CONFLICT_KEEP_KEY).primary(),
                ])
                .gap(tokens::SPACE_2),
            ])
            .destructive()
            .width(Size::Fill(1.0)),
        )
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
    /// pane's canvas (board or schematic) with the floating per-pane tool strip
    /// overlaid top-left inside the canvas (UI-oracle anatomy). Fill in both axes so
    /// the split weights govern its size.
    fn pane_el(&self, cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let view = self.panes.borrow()[pane_index(pane)].view;
        let canvas = match view {
            ViewKind::Board => self.board_canvas(cx, pane, sets),
            ViewKind::Schematic => self.schematic_canvas(cx, pane, sets),
        };
        // Stack the strip over the canvas: the strip layer hugs its own rect at
        // the stack's top-left, so pointer events outside it fall through to the
        // canvas viewport below (pan/zoom is never intercepted beyond the strip).
        let body = stack([canvas, strip::tool_strip(pane, view, self.tool_for(view))])
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
        column([self.pane_header(pane, view), body])
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
    fn board_canvas(&self, cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let derived = self.derived.borrow();
        let Some(view) = &derived.board else {
            return pane_placeholder("No board to display");
        };
        // Per-pane El keys: two board panes render the same layers, so namespace each
        // layer / overlay El by the pane (keys must be unique in the tree). The event
        // router still recognises these as canvas targets (the `layer:` / `overlay:`
        // prefixes survive) and the pane is resolved by pointer rect, not by key.
        let prefix = pane.canvas_key();
        let zoom = cx.viewport_view(pane.canvas_key()).map_or(1.0, |v| v.zoom);
        // Canvas furniture: the dot grid + origin axes, UNDER every board layer (the
        // first child). Its pitch adapts to the pane's zoom and its lattice window is
        // anchored to the pane's visible rect (last frame's camera + rect — the same
        // one-frame lag the zoom above has), so the grid covers the whole viewport at
        // every pan/zoom. Per-pane window cache: a typical build is an asset clone
        // (see `Canvas::grid_el`'s cost model). It shares the layers' viewBox so it
        // registers, and it is never a pick candidate (picking folds the geometry
        // kernel, not canvas Els — see `canvas::pick`).
        let mut children: Vec<El> = Vec::new();
        let visible = cx
            .rect_of_key(pane.canvas_key())
            .zip(cx.viewport_view(pane.canvas_key()))
            .map(|(r, vv)| view.canvas.visible_view_mm((r.x, r.y, r.w, r.h), vv));
        let mut caches = self.grid_caches.borrow_mut();
        if let Some(grid) = view
            .canvas
            .grid_el(zoom, visible, &mut caches[pane_index(pane)])
        {
            children.push(grid.key(format!("grid:{prefix}")));
        }
        drop(caches);
        children.extend(
            view.canvas
                .layer_els(&view.layers, |id| self.layer_visible(&id.key()))
                .into_iter()
                .enumerate()
                .map(|(i, el)| el.key(format!("layer:{prefix}:{i}"))),
        );
        let overlay = self.build_board_overlay(view, pane, sets, &derived.findings);
        if let Some(el) = view.canvas.overlay_el(&overlay) {
            // Re-key the overlay per pane (the canvas hardcodes "overlay:dynamic"); wrap it
            // in a keyed container so two board panes' overlays don't collide.
            children.push(el.key(format!("overlay:{prefix}")));
        }
        let vp = viewport(children)
            .key(pane.canvas_key())
            .min_zoom(0.1)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
        with_zoom_chip(vp, zoom)
    }

    /// A schematic pane's canvas: the cached schematic asset + the per-frame highlight
    /// overlay, in a viewport keyed to this pane. Falls back to a placeholder when the doc
    /// has no components.
    fn schematic_canvas(&self, cx: &BuildCx, pane: PaneId, sets: &HighlightSets) -> El {
        let derived = self.derived.borrow();
        let Some(view) = &derived.schematic else {
            return pane_placeholder("No schematic to display");
        };
        let zoom = cx.viewport_view(pane.canvas_key()).map_or(1.0, |v| v.zoom);
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
        let vp = viewport(children)
            .key(pane.canvas_key())
            .min_zoom(0.02)
            .max_zoom(64.0)
            .pan_bounds(PanBounds::Contain)
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
        with_zoom_chip(vp, zoom)
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
        let measure =
            if self.tool_for(ViewKind::Board) == Tool::Measure && self.measure_pane.get() == pane {
                self.measure.get().segment()
            } else {
                None
            };
        // The drag ghost + live ratsnest (m6): only in the pane the drag is
        // happening in, and only once the drag has crossed the click slop (an
        // un-moved press shows nothing). Both are pure vector math over state
        // captured at drag start — no kernel call, and the static board layers
        // are untouched (never re-tessellated) during the drag.
        let (ghost, ratsnest) = {
            let drag = self.drag.borrow();
            match drag.as_ref() {
                Some(d) if d.pane == pane && d.moved => (d.ghost_shapes(), d.ratsnest()),
                _ => (Vec::new(), Vec::new()),
            }
        };
        // Pending route preview (m6 slice B): board-space geometry, so — like the
        // findings markers — every board pane shows it. Runs render at the width
        // the commit will use; the layer-switch vias as pad-sized rings.
        let (route_runs, route_rubber, route_vias) = {
            let route = self.route.borrow();
            match route.as_ref() {
                Some(r) => {
                    let (width, _, via_pad) = crate::app::route_defaults();
                    (
                        r.runs
                            .iter()
                            .map(|run| (run.points.clone(), width))
                            .collect(),
                        r.rubber(),
                        r.vias.iter().map(|p| (*p, via_pad)).collect(),
                    )
                }
                None => (Vec::new(), None, Vec::new()),
            }
        };
        // Trace-vertex refinement (m6 slice B, Select tool): the selected trace
        // renders vertex handles; an in-flight vertex drag renders its working
        // path as an edit preview (handles track the working path).
        let (edit_path, handles) = {
            let drag = self.trace_drag.borrow();
            if let Some(d) = drag.as_ref() {
                (Some((d.path.clone(), d.width)), d.path.clone())
            } else if self.tool_for(ViewKind::Board) == Tool::Select
                && let Ok(doc) = &self.domain.doc
                && let Some(SemanticId::Trace(tid)) = self.domain.selection.borrow().single()
                && let Some(t) = doc.traces.get(tid)
            {
                (None, t.path.clone())
            } else {
                (None, Vec::new())
            }
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
            ghost,
            ratsnest,
            route_runs,
            route_rubber,
            route_vias,
            edit_path,
            handles,
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

    /// The net a board candidate's id belongs to, if any (for the net-expansion
    /// match, and the Route tool's start-pick net resolution).
    pub(crate) fn candidate_net(&self, id: &SemanticId) -> Option<NetId> {
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
}

/// The per-pane zoom-chip background — the oracle's chip token (`bg-5`, `#1a1a1f`),
/// slightly translucent so it floats over the canvas.
const CHIP_BG: Color = Color::srgb_token("ecad.canvas.chip", 0x1a, 0x1a, 0x1f, 0xee);

/// Overlay a canvas viewport with a small bottom-right **zoom chip** (UI oracle
/// canvas furniture). The chip layer is an unkeyed, non-`block_pointer` fill over the
/// viewport, so it neither becomes a hit target (hit-testing only visits keyed nodes)
/// nor occludes the viewport's geometric wheel-zoom / drag-pan gates — the canvas
/// keeps its pointer behaviour untouched. The viewport keeps its `canvas_key`, so
/// `rect_of_key` / content-bounds lookups still resolve it.
fn with_zoom_chip(canvas: El, zoom: f32) -> El {
    stack([
        canvas,
        column([zoom_chip(zoom)])
            .justify(Justify::End)
            .align(Align::End)
            .padding(Sides::all(tokens::SPACE_2))
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0)),
    ])
    .width(Size::Fill(1.0))
    .height(Size::Fill(1.0))
}

/// The zoom chip itself: `×N` at two significant figures, mono (numerics render in
/// JetBrains Mono per the oracle), in a muted pill.
fn zoom_chip(zoom: f32) -> El {
    text(format_zoom(zoom))
        .mono()
        .muted()
        .caption()
        .fill(CHIP_BG)
        .stroke(tokens::BORDER)
        .radius(tokens::RADIUS_MD)
        .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .width(Size::Hug)
        .height(Size::Hug)
}

/// Format a viewport zoom as `×N` to two significant figures, relative to the natural
/// 1 mm = 1 px scale (`×1.0` at zoom 1). The decimal count tracks the magnitude so two
/// sig figs show at every scale: `×0.10`, `×2.5`, `×12`, `×64`.
///
/// This is deliberately a *different* readout from the status bar's `Zoom NN%` (a
/// whole-percent global readout). If a shared chrome/format helper lands on main, the
/// orchestrator can unify the two at merge; implemented locally here per the slice brief.
pub(crate) fn format_zoom(zoom: f32) -> String {
    let z = zoom.max(1e-6);
    let decimals = (1 - z.log10().floor() as i32).max(0) as usize;
    format!("×{z:.decimals$}")
}

#[cfg(test)]
mod tests {
    use super::format_zoom;

    /// The zoom chip reads `×N` to two significant figures across scales — the decimal
    /// count tracks the magnitude so `[0.1, 1)` shows two decimals, `[1, 10)` one, and
    /// `≥ 10` none. Pins the readout the per-pane chip renders.
    #[test]
    fn zoom_chip_shows_two_sig_figs() {
        assert_eq!(format_zoom(1.0), "×1.0");
        assert_eq!(format_zoom(2.5), "×2.5");
        assert_eq!(format_zoom(0.35), "×0.35");
        assert_eq!(format_zoom(0.1), "×0.10");
        assert_eq!(format_zoom(64.0), "×64");
        assert_eq!(format_zoom(18.0), "×18");
    }
}
