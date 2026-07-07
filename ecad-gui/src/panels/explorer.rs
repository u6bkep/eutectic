//! The explorer panel: Components + Nets sections of click-to-select rows.
//! The row projection itself lives in [`crate::explorer`]. Moved out of
//! `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::EcadApp;
use crate::explorer::Explorer;
use damascene_core::prelude::*;

impl EcadApp {
    /// The Explorer accordion body (mockup NetExplorer anatomy): Components + Nets
    /// sub-groups, each a list of click-to-select rows with a count badge; the selected
    /// row gets the mockup's selected cue (`sidebar_menu_button`'s `current` treatment).
    /// This is the section content only — the accordion header is composed in
    /// `panels::sidebar`.
    pub(crate) fn explorer_body(&self, explorer: &Explorer) -> El {
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
        column([
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
        let label = if r.secondary.is_empty() {
            format!("{}  [{}]", r.label, r.count)
        } else {
            format!("{}  ({})  [{}]", r.label, r.secondary, r.count)
        };
        sidebar_menu_button(label, current).key(r.key.clone())
    }
}
