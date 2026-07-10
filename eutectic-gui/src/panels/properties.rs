//! The properties inspector panel: an identity card + key/value rows for the
//! single selected entity, or the doc-stats card when nothing is selected.
//! Moved out of `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::EutecticApp;
use crate::app::domain::DocStats;
use crate::inspector::InspectorData;
use damascene_core::prelude::*;

impl EutecticApp {
    /// The Properties accordion body: an identity card + key/value rows for the single
    /// selected entity, or the m2 stats card when nothing is selected. Works regardless
    /// of which pane the selection came from (the selection is shared, semantic). This is
    /// the section content only — the accordion header is composed in `panels::sidebar`.
    pub(crate) fn inspector_body(&self) -> El {
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
        sidebar_group(children).width(Size::Fill(1.0))
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
                sidebar_group([
                    text("No selection").muted(),
                    field_row("Parts", text(s.parts.to_string()).mono()),
                    field_row("Nets", text(s.nets.to_string()).mono()),
                    field_row("Copper layers", text(s.layers.to_string()).mono()),
                    field_row("Board", text(board).mono()),
                ])
                .width(Size::Fill(1.0))
            }
            Err(_) => sidebar_group([text("No document").muted()]).width(Size::Fill(1.0)),
        }
    }
}
