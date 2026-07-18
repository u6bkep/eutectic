//! The pane-tree region — the recursive H/V split tree and everything composed
//! inside it: `viewer_body` (the whole viewer composition), pane headers, the
//! board / schematic canvas pane wrappers with their floating tool strips
//! ([`strip`]), the per-frame board overlay builder, the shared cross-view
//! highlight projection, and the conflict banner. Moved out of `app/panels.rs`
//! as pure code motion (gui-module-split).
//!
//! OWNED CANVAS (WP2 board, WP3 schematic): every pane runs on the
//! app-rendered `surface(AppTexture)` path — one view-generic builder
//! ([`EutecticApp::canvas_pane_el`]) parameterized by the pane's view kind;
//! the damascene `viewport()` path is deleted (gui-architecture.md, "Canvas
//! strategy").

pub(crate) mod strip;

use crate::app::canvas_pane::Overlay;
use crate::app::pane::{
    CANVAS_BG, CONFLICT_KEEP_KEY, CONFLICT_RELOAD_KEY, MAX_PANES, PaneNode, pane_index,
    pane_placeholder,
};
use crate::app::{EutecticApp, PaneId, ViewKind};
use crate::chrome::icons;
use crate::findings::Findings;
use crate::pick::SemanticId;
use crate::tool::Tool;
use damascene_core::prelude::*;
use eutectic_core::id::NetId;

impl EutecticApp {
    /// The viewer body: the split tree (center), the right sidebar
    /// (inspector + explorer + layer panel), and the status bar. Reached when the doc
    /// loaded (at least one pane always renders — a board pane falls back to a placeholder
    /// if its projection failed, a schematic pane if the doc has no components).
    pub(crate) fn viewer_body(&self, cx: &BuildCx) -> El {
        // The first live board pane's zoom drives the toolbar/status readout.
        // The cursor readout is set per event.
        let zoom = self.readout_zoom();

        let split = self.pane_split(cx);

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
        let mut work_area = Vec::new();
        if self.library_browser_open.get() {
            work_area.push(self.library_browser());
        }
        work_area.push(split);
        work_area.push(self.right_sidebar());
        children.push(
            row(work_area)
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
    /// (whichever pane shows a board — read off its app-owned camera in the
    /// legacy px-per-mm scale), else 1.0. Board panes no longer consult
    /// `cx.viewport_view` (WP2: they left the viewport system).
    fn readout_zoom(&self) -> f32 {
        for id in self.pane_ids() {
            if self.pane_view(id) == ViewKind::Board {
                return crate::app::canvas_pane::zoom_px_per_mm(&self.pane_camera(id));
            }
        }
        1.0
    }

    /// The recursive split tree, or one full-bleed leaf while maximized.
    fn pane_split(&self, cx: &BuildCx) -> El {
        if let Some(max) = self.maximized.get() {
            return self.pane_el(cx, max);
        }
        self.pane_node_el(cx, &self.pane_tree.borrow().root)
    }

    fn pane_node_el(&self, cx: &BuildCx, node: &PaneNode) -> El {
        match node {
            PaneNode::Leaf(pane) => self.pane_el(cx, *pane),
            PaneNode::Split {
                id,
                axis,
                weights,
                first,
                second,
                ..
            } => {
                let a = self
                    .pane_node_el(cx, first)
                    .width(Size::Fill(weights[0]))
                    .height(Size::Fill(weights[0]));
                let b = self
                    .pane_node_el(cx, second)
                    .width(Size::Fill(weights[1]))
                    .height(Size::Fill(weights[1]));
                let children = [a, resize_handle(id.handle_key(), axis.damascene()), b];
                let container = match axis {
                    crate::app::SplitAxis::Horizontal => row(children),
                    crate::app::SplitAxis::Vertical => column(children),
                };
                container
                    .key(id.container_key())
                    .gap(tokens::SPACE_2)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0))
            }
        }
    }

    /// One pane: a header row (view-kind label + switcher + maximize toggle) over the
    /// pane's canvas (board or schematic) with the floating per-pane tool strip
    /// overlaid top-left inside the canvas (UI-oracle anatomy). Fill in both axes so
    /// the split weights govern its size.
    fn pane_el(&self, cx: &BuildCx, pane: PaneId) -> El {
        let view = self.pane_view(pane);
        let canvas = self.canvas_pane_el(cx, pane, view);
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

    /// A pane header: view-kind dropdown, split-right/down, reserved disabled
    /// pop-out, close, and maximize. Split/close availability is structural.
    fn pane_header(&self, pane: PaneId, view: ViewKind) -> El {
        let max_label = if self.maximized.get() == Some(pane) {
            "Restore"
        } else {
            "Maximize"
        };
        let can_split = self.pane_count() < MAX_PANES;
        let can_close = self.pane_count() > 1;
        let split_right = icon_button(icons::SPLIT_RIGHT.clone());
        let split_down = icon_button(icons::SPLIT_DOWN.clone());
        let close = icon_button("x");
        toolbar([
            select_trigger(pane.view_select_key(), view.label()).width(Size::Fill(1.0)),
            if can_split {
                split_right
                    .tooltip("Split right")
                    .key(pane.split_right_key())
            } else {
                split_right.disabled()
            },
            if can_split {
                split_down.tooltip("Split down").key(pane.split_down_key())
            } else {
                split_down.disabled()
            },
            icon_button(icons::POP_OUT.clone()).disabled(),
            if can_close {
                close.tooltip("Close pane").key(pane.close_key())
            } else {
                close.disabled()
            },
            icon_button(icons::FIT.clone())
                .tooltip(max_label)
                .key(pane.maximize_key()),
        ])
        .gap(tokens::SPACE_1)
        .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .width(Size::Fill(1.0))
        .height(Size::Hug)
    }

    /// Root-level view-kind dropdown for the currently open pane header.
    pub(crate) fn pane_view_overlay(&self) -> Option<El> {
        let pane = self.pane_view_menu.get()?;
        self.panes.borrow().get(pane_index(pane))?.as_ref()?;
        Some(select_menu(
            pane.view_select_key(),
            ViewKind::all().map(|view| (view.token(), view.label())),
        ))
    }

    /// A pane's canvas (owned canvas, WP2+WP3): one keyed container holding
    /// the pane's `surface(AppTexture)` El — the renderer-drawn texture,
    /// composited opaque and clipped to the pane rect — for BOTH view kinds.
    /// The old viewport path (grid El + layer/schematic Els + overlay El
    /// inside `viewport()`) is gone: the grid is procedural in the renderer
    /// (board panes only), layer visibility is a style-table uniform, the
    /// overlay lowers to renderer primitives, and highlight emphasis rides
    /// the semantic state buffer. Headless (the CPU harness / review bin)
    /// has no GPU and no texture; the container still owns the key, the
    /// laid-out rect, and the pointer routing, so every pick/gesture test
    /// runs without a device. Falls back to a placeholder when the view has
    /// no content (no board projection / no schematic components).
    fn canvas_pane_el(&self, cx: &BuildCx, pane: PaneId, view: ViewKind) -> El {
        let has_content = match view {
            ViewKind::Board => self.derived.borrow().board.is_some(),
            ViewKind::Schematic => self.derived.borrow().schematic_scene.is_some(),
        };
        if !has_content {
            return pane_placeholder(match view {
                ViewKind::Board => "No board to display",
                ViewKind::Schematic => "No schematic to display",
            });
        }
        // Capture the pane + strip rects (from the last layout) and the
        // scale factor: the paint pass sizes the pane texture from them and
        // the raw-pointer seams (free hover, middle pan) resolve panes
        // against them. One-frame lag by construction (see
        // `app::canvas_pane` module docs).
        if let Some(d) = cx.diagnostics() {
            self.scale_factor.set(d.scale_factor);
        }
        let key = pane.canvas_key();
        let rect = cx.rect_of_key(key).map(|r| (r.x, r.y, r.w, r.h));
        self.pane_px.borrow_mut()[pane_index(pane)] = rect;
        self.strip_px.borrow_mut()[pane_index(pane)] = cx
            .rect_of_key(pane.strip_panel_key())
            .map(|r| (r.x, r.y, r.w, r.h));

        // Settle this pane's camera for the frame: fit-on-first-show plus
        // any pending Fit/Reset request, against the known rect.
        let cam = match rect {
            Some(r) => self.pane_build_camera(pane, r),
            None => self.pane_camera(pane),
        };

        let mut children: Vec<El> = Vec::new();
        let mut texture_pending = false;
        if let Some((tex, alloc)) = self.pane_texture(pane) {
            // Pixel-accurate compositing: the texture is `alloc` device px,
            // so the El spans `alloc / scale` logical px (the default Fill
            // fit then maps texels 1:1 onto device px); the container's
            // clip crops the allocation-hysteresis overscan beyond the pane.
            let s = self.scale_factor.get().max(0.1);
            children.push(
                surface(tex)
                    .surface_alpha(SurfaceAlpha::Opaque)
                    .width(Size::Fixed(alloc.0 as f32 / s))
                    .height(Size::Fixed(alloc.1 as f32 / s)),
            );
        } else if self.gpu.borrow().is_some() && rect.is_some() {
            // The GPU path is live but the first paint hasn't produced a
            // texture yet — ask for a frame so it appears without input.
            texture_pending = true;
        }
        let mut canvas = stack(children)
            .key(key)
            .clip()
            .fill(CANVAS_BG)
            .width(Size::Fill(1.0))
            .height(Size::Fill(1.0));
        // Continuous redraw ONLY while motion is live (§7 damage rule): a
        // mid-flight glide re-renders each frame until its bit-exact settle;
        // drags ride the host's pointer-driven redraws.
        if self.glide_active() || texture_pending {
            canvas = canvas.redraw_within(std::time::Duration::ZERO);
        }
        with_zoom_chip(canvas, crate::app::canvas_pane::zoom_px_per_mm(&cam))
    }

    /// Build a board pane's dynamic overlay: the preview channels (measure /
    /// drag ghost / pending route / vertex refinement) + finding markers.
    /// Selection/hover highlight geometry is deliberately absent — emphasis
    /// rides the semantic state buffer (spec §5), so the per-frame candidate
    /// walk the old `highlights` field ran is gone.
    pub(crate) fn build_board_overlay(&self, pane: PaneId, findings: &Findings) -> Overlay {
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
        let (mut ghost, ratsnest) = {
            let drag = self.drag.borrow();
            match drag.as_ref() {
                Some(d) if d.pane == pane && d.moved => (d.ghost_shapes(), d.ratsnest()),
                _ => (Vec::new(), Vec::new()),
            }
        };
        ghost.extend(self.place_ghost_shapes(pane));
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
        let finding_markers: Vec<(eutectic_core::coord::Point, bool)> = findings
            .items
            .iter()
            .filter_map(|f| {
                let (mx, my) = f.board_mm?;
                Some((
                    eutectic_core::coord::Point {
                        x: (mx * eutectic_core::coord::MM as f32).round()
                            as eutectic_core::coord::Nm,
                        y: (my * eutectic_core::coord::MM as f32).round()
                            as eutectic_core::coord::Nm,
                    },
                    f.is_error(),
                ))
            })
            .collect();
        Overlay {
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
    pub(crate) fn schematic_finding_ids(
        &self,
        findings: &Findings,
    ) -> std::collections::BTreeSet<SemanticId> {
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
                let pr = eutectic_core::doc::PinRef::new(comp, pin);
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
const CHIP_BG: Color = Color::srgb_token("eutectic.canvas.chip", 0x1a, 0x1a, 0x1f, 0xee);

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
