//! The bottom status bar: live cursor readout, measure readout, active-layer /
//! selected-net chips, compact DRC findings state, and the focused pane's zoom as
//! a `×N` scale factor ([`chrome::zoom_scale_label`](crate::chrome::zoom_scale_label)).
//! Coordinate and measure values project through the app-wide mm/in display setting.
//! Moved out of `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::EutecticApp;
use crate::inspector::InspectorData;
use crate::tool::Tool;
use damascene_core::prelude::*;

impl EutecticApp {
    /// The bottom status bar (mockup taste): the live cursor position in board
    /// coordinates, the focused pane's live tool, and the zoom percent. The cursor
    /// readout updates on pointer enter and while panning — see the module
    /// deviation note on free-hover.
    pub(crate) fn status_bar(&self, zoom: f32) -> El {
        let units = self.display_units();
        let cursor = match self.cursor_board_mm.get() {
            Some((x, y)) => format!(
                "X {:.2}  Y {:.2} {}",
                units.from_mm(x as f64),
                units.from_mm(y as f64),
                units.label()
            ),
            None => format!("X --  Y -- {}", units.label()),
        };
        let mut items: Vec<El> = vec![text(cursor).muted().mono()];

        // The measure readout (mockup taste: dx/dy/dist in the status bar) — shown only
        // while the focused pane's kind is measuring with a segment in progress.
        if self.live_tool() == Tool::Measure
            && self.measure_pane.get() == self.focused_pane.get()
            && let Some((dx, dy, dist)) = self.measure.get().readout()
        {
            items.push(
                text(format!(
                    "dx {:.2}  dy {:.2}  d {:.2} {}",
                    units.from_mm(dx),
                    units.from_mm(dy),
                    units.from_mm(dist),
                    units.label()
                ))
                .mono(),
            );
        }

        items.push(spacer());

        // The live tool (oracle status-bar anatomy: the FOCUSED pane's view kind's
        // slot — moving focus between a board and a schematic pane swaps this
        // without touching either kind's memory).
        items.push(text(format!("tool {}", self.live_tool().label())).muted());

        // The active-layer chip (mockup status-bar anatomy; m6 slice B): the
        // copper slab new routes land on. Shown whenever a board is loaded.
        if let Some(layer) = self.active_layer_name() {
            items.push(badge(format!("layer {layer}")).info());
        }
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
        // Zoom of the focused pane as a scale factor `×N` relative to the natural
        // 1 mm : 1 px framing (the meaningful readout; the old percentage was
        // relative to nothing). The per-pane canvas zoom chip is another slice.
        items.push(
            text(format!("zoom {}", crate::chrome::zoom_scale_label(zoom)))
                .muted()
                .mono(),
        );

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
}
