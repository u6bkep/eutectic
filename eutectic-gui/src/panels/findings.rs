//! The findings panel: the collapsible error/warning list, its click-to-select
//! rows (`select_finding` — the row-click handler with the CenterOn navigation),
//! and the parse/elaborate-failure `error_card`. Moved out of `app/panels.rs`
//! as pure code motion (gui-module-split).

use crate::app::pane::finding_row_key;
use crate::app::{EutecticApp, PaneId, ViewKind};
use crate::findings::Findings;
use damascene_core::prelude::*;

/// The small err/warn count chips shown on the right of the Findings accordion
/// header (the oracle's tiny red/amber pills). One error chip when `errors > 0`,
/// one warning chip when `warnings > 0`; nothing when the board is clean.
///
/// These carry no route key: a click anywhere on the header (chips included)
/// bubbles to the header row and toggles the section. Because the section label
/// fills the row and ellipsizes, the chips always keep their natural width — the
/// old "Findings (5 err, 0 wa…" truncation-into-the-Hide-button collision is
/// structurally impossible now (there is no Hide button, and the counts are
/// discrete boxes, not part of the title text).
pub(crate) fn findings_header_chips(findings: &Findings) -> Vec<El> {
    let mut chips = Vec::new();
    if findings.errors > 0 {
        chips.push(badge(format!("{} err", findings.errors)).destructive());
    }
    if findings.warnings > 0 {
        chips.push(badge(format!("{} warn", findings.warnings)).warning());
    }
    chips
}

impl EutecticApp {
    /// The Findings accordion body: one click-to-select row per finding (a severity
    /// badge beside the code and message), or a compact "no issues" line when the board
    /// is clean. Clicking a row selects the finding's refs (cross-highlighting the panes)
    /// and centres the focused board pane on the violation. This is the section content
    /// only — the accordion header (with the err/warn count chips) is composed in
    /// `panels::sidebar`.
    pub(crate) fn findings_body(&self, findings: &Findings) -> El {
        if findings.is_clean() {
            return sidebar_group([text("No issues — DRC clean.").muted()]).width(Size::Fill(1.0));
        }
        let rows: Vec<El> = findings
            .items
            .iter()
            .enumerate()
            .map(|(i, f)| self.finding_row(i, f))
            .collect();
        sidebar_group([column(rows).gap(tokens::SPACE_1).width(Size::Fill(1.0))])
            .width(Size::Fill(1.0))
    }

    /// One findings row: a severity badge (error red / warning amber) + the code +
    /// message, as a click-to-select focusable row keyed by index. Built on the same
    /// focusable-list-item anatomy as `sidebar_menu_button` (which is label-only), so a
    /// click routes to the app and the row reads as an interactive nav entry.
    ///
    /// An [informational](crate::findings::Finding::is_informational) finding (an
    /// unresolved part / library-resolution note — no refs, no board point, nothing to
    /// navigate to) renders the same anatomy WITHOUT the interactive affordances: no
    /// key, not focusable, no pointer cursor — a plain data row.
    fn finding_row(&self, index: usize, f: &crate::findings::Finding) -> El {
        let sev = if f.is_error() {
            badge("ERR").destructive()
        } else {
            badge("WARN").warning()
        };
        let body = column([
            text(f.code).mono().caption(),
            text(f.message.clone()).width(Size::Fill(1.0)).wrap_text(),
        ])
        .gap(0.0)
        .width(Size::Fill(1.0));
        let base = row([sev, body])
            .style_profile(StyleProfile::Solid)
            .metrics_role(MetricsRole::ListItem)
            .fill(tokens::CARD)
            .radius(tokens::RADIUS_SM)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
            .align(Align::Center)
            .width(Size::Fill(1.0));
        if f.is_informational() {
            return base;
        }
        base.key(finding_row_key(index))
            .focusable()
            .cursor(Cursor::Pointer)
            .ghost()
    }

    /// Select the finding at `index` (a findings-panel row click): fold ALL of its
    /// semantic refs into the selection (so the panes cross-highlight the offending
    /// nets / parts / pins), and — if the finding has a derived board point — glide
    /// the focused board pane's camera onto it so the violation comes into view.
    ///
    /// # Click-to-zoom gap (deviation, preserved from the viewport era)
    ///
    /// "Zoom the focused board pane to the violation" is realised as a **center-on**
    /// (glide the camera center to the point, keeping the current zoom) rather than a
    /// true frame-to-rect. The finding's board point is centred; the zoom is left as
    /// the user set it.
    pub(crate) fn select_finding(&self, index: usize, _cx: &EventCx) {
        let derived = self.derived.borrow();
        let Some(f) = derived.findings.items.get(index) else {
            return;
        };
        // Informational rows (unresolved part / library note) have nothing to select
        // or navigate to — and they render without a route key, so this is belt and
        // braces against a stale index.
        if f.is_informational() {
            return;
        }
        // Fold every ref into the selection (multi-select — a clearance highlights BOTH
        // nets). Clear first, then add each ref.
        {
            let mut sel = self.domain.selection.borrow_mut();
            sel.clear();
            for r in &f.refs {
                sel.add(r.clone());
            }
        }
        // Center the focused board pane's app-owned camera on the finding's board
        // point (WP2: plain camera-target math on the glide — no viewport request,
        // no rect needed; board mm → nm is the only conversion).
        if let Some((mx, my)) = f.board_mm
            && let Some(pane) = self.focused_board_pane()
        {
            let mm = eutectic_core::coord::MM as f64;
            self.board_center_on(pane, (mx as f64 * mm, my as f64 * mm));
        }
    }

    /// The board pane to focus for click-to-zoom: the first pane currently showing a
    /// board (A preferred), respecting a maximized pane. `None` when no board pane is
    /// visible (both panes schematic, or the board didn't project).
    fn focused_board_pane(&self) -> Option<PaneId> {
        let panes = self.panes.borrow();
        let visible = |id: PaneId| self.maximized.get().map(|m| m == id).unwrap_or(true);
        for (i, p) in panes.iter().enumerate() {
            let id = if i == 0 { PaneId::A } else { PaneId::B };
            if p.view == ViewKind::Board && visible(id) {
                return Some(id);
            }
        }
        None
    }
}

/// The parse/elaborate-failure body: surface the error, never crash (the
/// permissive philosophy starts here).
pub(crate) fn error_card(message: &str) -> El {
    // The empty state uses the same path — "no document" is just an `Err`.
    if message == "no document" {
        return titled_card(
            "No document",
            [text("Pass a path to a .eut file to load a document.").muted()],
        )
        .width(Size::Fixed(420.0));
    }
    alert([
        alert_title("Could not load document"),
        alert_description(message.to_string()),
    ])
    .destructive()
    .width(Size::Fixed(420.0))
}
