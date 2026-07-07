//! The right sidebar — the composition over the four panels (properties,
//! findings, explorer, layers). Moved out of `app/panels.rs` as pure code
//! motion (gui-module-split).

use crate::app::EcadApp;
use damascene_core::prelude::*;

impl EcadApp {
    /// The right sidebar: the properties inspector (above), the explorer (middle), and the
    /// board layer panel (below), matching the mockup anatomy (Properties above Explorer).
    pub(crate) fn right_sidebar(&self) -> El {
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
}
