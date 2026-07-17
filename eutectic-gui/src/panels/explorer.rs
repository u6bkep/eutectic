//! The explorer panel: Components + Nets sections of click-to-select rows.
//! The row projection itself lives in [`crate::explorer`]. Moved out of
//! `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::EutecticApp;
use crate::explorer::Explorer;
use damascene_core::prelude::*;

pub(crate) const EXPLORER_FILTER_KEY: &str = "explorer:filter";

impl EutecticApp {
    /// The Explorer accordion body (mockup NetExplorer anatomy): Components + Nets
    /// sub-groups, each a list of click-to-select rows with a count badge; the selected
    /// row gets the mockup's selected cue (`sidebar_menu_button`'s `current` treatment).
    /// This is the section content only — the accordion header is composed in
    /// `panels::sidebar`.
    pub(crate) fn explorer_body(&self, explorer: &Explorer) -> El {
        let query = self.explorer_filter.borrow();
        let needle = query.trim().to_lowercase();
        let sel = self.domain.selection.borrow();
        let comp_rows: Vec<El> = explorer
            .components
            .iter()
            .filter(|r| {
                needle.is_empty()
                    || r.label.to_lowercase().contains(&needle)
                    || self
                        .explorer_component_value(r)
                        .to_lowercase()
                        .contains(&needle)
            })
            .map(|r| self.explorer_row(r, sel.is_selected(&r.id)))
            .collect();
        let net_rows: Vec<El> = explorer
            .nets
            .iter()
            .filter(|r| needle.is_empty() || r.label.to_lowercase().contains(&needle))
            .map(|r| self.explorer_row(r, sel.is_selected(&r.id)))
            .collect();
        column([
            text_input_with(
                EXPLORER_FILTER_KEY,
                &query,
                &self.explorer_filter_selection.borrow(),
                TextInputOpts::default().placeholder("Filter…"),
            )
            .width(Size::Fill(1.0)),
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
        .gap(tokens::SPACE_3)
        .width(Size::Fill(1.0))
    }

    /// One explorer row: a click-to-select `sidebar_menu_button` labelled with the id +
    /// secondary text + count badge, `current` when it is the selection.
    fn explorer_row(&self, r: &crate::explorer::ExplorerRow, current: bool) -> El {
        let secondary = self.explorer_component_value(r);
        let label = if secondary.is_empty() {
            format!("{}  [{}]", r.label, r.count)
        } else {
            format!("{}  ({secondary})  [{}]", r.label, r.count)
        };
        sidebar_menu_button(label, current).key(r.key.clone())
    }

    fn explorer_component_value(&self, row: &crate::explorer::ExplorerRow) -> String {
        let crate::pick::SemanticId::Part(id) = &row.id else {
            return String::new();
        };
        let Ok(doc) = &self.domain.doc else {
            return row.secondary.clone();
        };
        let Some(comp) = doc.components.get(id) else {
            return row.secondary.clone();
        };
        let Some(def) = self.domain.lib.get(&comp.part) else {
            return comp
                .params
                .get("value")
                .cloned()
                .unwrap_or_else(|| comp.part.clone());
        };
        eutectic_core::annotate::label(comp, def, &eutectic_core::annotate::registry(&doc.source))
    }

    pub(crate) fn handle_explorer_filter_event(&self, event: &UiEvent) -> bool {
        let changed = text_input::apply_event(
            &mut self.explorer_filter.borrow_mut(),
            &mut self.explorer_filter_selection.borrow_mut(),
            event,
            EXPLORER_FILTER_KEY,
        );
        if !changed
            && let Some(selection) = &event.selection
            && event.is_route(EXPLORER_FILTER_KEY)
        {
            *self.explorer_filter_selection.borrow_mut() = selection.clone();
        }
        changed
    }
}
