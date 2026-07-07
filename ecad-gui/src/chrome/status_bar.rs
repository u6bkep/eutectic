//! The bottom status bar: live cursor readout, measure readout, active-layer /
//! selected-net chips, compact DRC state, and the zoom percent. Moved out of
//! `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::{EcadApp, ViewKind};
use crate::inspector::InspectorData;
use crate::tool::{Tool, format_readout};
use damascene_core::prelude::*;

impl EcadApp {
    /// The bottom status bar (mockup taste): the live cursor position in board
    /// coordinates, the focused pane's live tool, and the zoom percent. The cursor
    /// readout updates on pointer enter and while panning — see the module
    /// deviation note on free-hover.
    pub(crate) fn status_bar(&self, zoom: f32) -> El {
        let cursor = match self.cursor_board_mm.get() {
            Some((x, y)) => format!("X {x:.2}  Y {y:.2} mm"),
            None => "X --  Y -- mm".to_string(),
        };
        let mut items: Vec<El> = vec![text(cursor).muted().mono()];

        // The measure readout (mockup taste: dx/dy/dist in the status bar) — shown only
        // while the board kind's slot is Measure with a segment in progress (measure
        // is a board-pane preview).
        if self.tool_for(ViewKind::Board) == Tool::Measure
            && let Some((dx, dy, dist)) = self.measure.get().readout()
        {
            items.push(text(format_readout(dx, dy, dist)).mono());
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
}
