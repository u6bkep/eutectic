//! The icon toolbar (UI oracle, region 2): grouped icon buttons under the menu
//! bar. Groups, left to right (the oracle's `toolbarGroups`, minus the clipboard
//! group — no clipboard exists yet — and the tool group, which lives in the
//! per-pane overlay strips a sibling slice owns):
//!
//! - `[open, save]` — open is disabled (no file-open flow yet); save is wired to
//!   [`SAVE_KEY`] and enabled only with a source path (accent-tinted while dirty).
//! - `[undo, redo]` — wired to [`UNDO_KEY`] / [`REDO_KEY`].
//! - `[zoom in, zoom out, fit]` — only Fit is wired ([`FIT_KEY`]); zoom in/out
//!   ship disabled because damascene 0.4.5 exposes no relative-zoom
//!   `ViewportRequest` (only Fit / Reset / CenterOn), so there is no action to
//!   dispatch without inventing one.
//! - `[findings jump, command palette]` — findings jump toggles the findings
//!   panel ([`FINDINGS_TOGGLE_KEY`], same as the chips); the palette is disabled
//!   (gw-12, unimplemented).
//!
//! Right side: a static `Units: mm` chip stating the current truth. Grid / Snap
//! chips from the oracle are omitted — there is no grid or snap system yet, so a
//! chip for them would state a capability that does not exist.
//!
//! The filename badge, dirty dot, findings chips, and reload-error chip that used
//! to live here moved to the menu bar's right cluster (`chrome::menubar`).

use crate::app::EcadApp;
use crate::app::pane::{FINDINGS_TOGGLE_KEY, REDO_KEY, SAVE_KEY, UNDO_KEY};
use crate::chrome::icons;
use crate::chrome::menubar::FIT_KEY;
use damascene_core::prelude::*;

impl EcadApp {
    /// The icon toolbar strip (oracle region 2). See the module docs for the group
    /// enumeration and the wired-vs-disabled split.
    pub(crate) fn viewer_toolbar(&self) -> El {
        let has_path = self.domain.source_path.is_some();
        let dirty = self.dirty();

        // A disabled icon button carries NO tooltip: a tooltip on an unkeyed node
        // never fires (hit-test only returns keyed nodes) and lints as DeadTooltip.
        let disabled = |src: &str| icon_button(src).disabled();

        // Group 1: open (disabled — no file-open flow) + save.
        let mut save = icon_button(icons::SAVE.clone());
        // Save is only actionable with a file to write (the m6 save model). Accent
        // it while dirty so the pending state is glanceable; disabled (no tooltip)
        // without a path.
        save = if has_path {
            let s = save.tooltip("Save").key(SAVE_KEY);
            if dirty { s.primary() } else { s }
        } else {
            save.disabled()
        };
        let group_file = row([disabled("folder"), save]).gap(tokens::SPACE_1);

        // Group 2: undo / redo (no-ops on empty stacks, always live — as the old
        // toolbar buttons were).
        let group_edit = row([
            icon_button(icons::UNDO.clone())
                .tooltip("Undo")
                .key(UNDO_KEY),
            icon_button(icons::REDO.clone())
                .tooltip("Redo")
                .key(REDO_KEY),
        ])
        .gap(tokens::SPACE_1);

        // Group 3: zoom in / out (disabled — no relative-zoom request) + fit.
        let group_view = row([
            icon_button(icons::ZOOM_IN.clone()).disabled(),
            icon_button(icons::ZOOM_OUT.clone()).disabled(),
            icon_button(icons::FIT.clone())
                .tooltip("Zoom to fit")
                .key(FIT_KEY),
        ])
        .gap(tokens::SPACE_1);

        // Group 4: findings jump (toggles the findings panel) + command palette
        // (disabled — gw-12).
        let group_inspect = row([
            icon_button(icons::FINDINGS.clone())
                .tooltip("Findings")
                .key(FINDINGS_TOGGLE_KEY),
            disabled("command"),
        ])
        .gap(tokens::SPACE_1);

        toolbar([
            group_file,
            group_edit,
            group_view,
            group_inspect,
            spacer(),
            // A static status chip stating the current truth: coordinates are mm
            // (no unit toggle exists yet). Grid / Snap chips are omitted — no grid
            // or snap system exists to describe.
            badge("Units: mm").muted(),
        ])
        .gap(tokens::SPACE_3)
        .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
        .width(Size::Fill(1.0))
    }
}
