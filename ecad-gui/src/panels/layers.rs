//! The board layer panel: one row per layer with a colour swatch, visibility
//! switch, and (for copper rows) the set-active routing marker — plus the
//! layer-visibility predicates the canvas / pick paths share. Moved out of
//! `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::EcadApp;
use crate::app::pane::switch_key;
use crate::canvas::BoardLayer;
use damascene_core::prelude::*;

impl EcadApp {
    /// Is the layer with `key` currently visible? Layers default on; the toggle
    /// records only the *hidden* set.
    pub(crate) fn layer_visible(&self, key: &str) -> bool {
        !self.hidden.borrow().contains(key)
    }

    /// Visibility of a [`LayerId`] on the per-event pick path, without allocating a key
    /// string per candidate. The hidden set is empty in the common case (nothing hidden),
    /// so this short-circuits to `true` before formatting the `"layer:…"` key; the
    /// `format!` runs only when at least one layer is actually hidden. Equivalent to
    /// `self.layer_visible(&id.key())` but off the hot allocation path (the profiler's
    /// incidental: `resolve`'s visibility closure was allocating a `LayerId` key per
    /// candidate every event — 192× on the poc board).
    pub(crate) fn layer_id_visible(&self, id: &crate::canvas::LayerId) -> bool {
        let hidden = self.hidden.borrow();
        hidden.is_empty() || !hidden.contains(&id.key())
    }

    /// The right sidebar layer panel: one row per layer (top of the stack first),
    /// each a colour swatch, name, and a visibility switch; copper rows also get
    /// the set-active routing marker (m6 slice B). Order mirrors draw order
    /// reversed, so the top copper reads at the top of the list.
    pub(crate) fn layer_panel(&self, layers: &[BoardLayer]) -> El {
        let copper = self.copper_layer_names();
        let active = self.active_layer_name();
        // Draw order is bottom-first; the panel lists top-first.
        let rows: Vec<El> = layers
            .iter()
            .rev()
            .map(|l| self.layer_row(l, &copper, active.as_deref()))
            .collect();
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

    /// One layer-panel row: colour swatch + name + a visibility [`switch`]. A
    /// COPPER row additionally leads with the set-active routing marker (m6
    /// slice B): a small `●`/`○` button — filled when this is the active layer —
    /// visually distinct from (and on the opposite side to) the visibility
    /// toggle. Clicking it makes this slab the active routing layer; while a
    /// route is pending that switch drops a via.
    fn layer_row(&self, l: &BoardLayer, copper: &[String], active: Option<&str>) -> El {
        let key = l.id.key();
        let swatch = El::new(Kind::Custom("layer-swatch"))
            .fill(l.color)
            .stroke(tokens::BORDER)
            .radius(3.0)
            .width(Size::Fixed(14.0))
            .height(Size::Fixed(14.0));
        let mut cells: Vec<El> = Vec::new();
        if let crate::canvas::LayerId::Slab(name) = &l.id
            && copper.iter().any(|n| n == name)
        {
            let is_active = active == Some(name.as_str());
            let marker = button(if is_active { "●" } else { "○" })
                .key(crate::app::pane::active_layer_key(name));
            cells.push(if is_active { marker.primary() } else { marker });
        }
        cells.push(swatch);
        cells.push(text(l.name.clone()).width(Size::Fill(1.0)));
        cells.push(switch(switch_key(&key), self.layer_visible(&key)));
        row(cells)
            .align(Align::Center)
            .gap(tokens::SPACE_2)
            .padding(Sides::y(tokens::SPACE_1))
    }
}
