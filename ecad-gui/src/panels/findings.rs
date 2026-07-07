//! The findings panel: the collapsible error/warning list, its click-to-select
//! rows (`select_finding` — the row-click handler with the CenterOn navigation),
//! and the parse/elaborate-failure `error_card`. Moved out of `app/panels.rs`
//! as pure code motion (gui-module-split).

use crate::app::pane::{FINDINGS_TOGGLE_KEY, finding_row_key};
use crate::app::{EcadApp, PaneId, ViewKind};
use crate::findings::Findings;
use damascene_core::prelude::*;

impl EcadApp {
    /// The findings panel (right sidebar, collapsible like the explorer): a header with
    /// the error/warning tally, then one click-to-select row per finding (a severity
    /// badge beside the code and message). Clicking a row selects the finding's refs
    /// (cross-highlighting the panes) and centres the focused board pane on the
    /// violation. Collapsed to just the header when `findings_open` is false or when
    /// there are no findings (a clean board shows a compact "no issues" line).
    pub(crate) fn findings_panel(&self, findings: &Findings) -> El {
        let open = self.findings_open.get();
        let title = if findings.is_clean() {
            "Findings".to_string()
        } else {
            format!(
                "Findings ({} err, {} warn)",
                findings.errors, findings.warnings
            )
        };
        let toggle = button(if open { "Hide" } else { "Show" }).key(FINDINGS_TOGGLE_KEY);
        let header = sidebar_header([row([h3(title).width(Size::Fill(1.0)).ellipsis(), toggle])
            .align(Align::Center)
            .width(Size::Fill(1.0))]);
        if !open {
            return sidebar([header]).width(Size::Fill(1.0)).height(Size::Hug);
        }
        if findings.is_clean() {
            return sidebar([
                header,
                sidebar_group([text("No issues — DRC clean.").muted()]),
            ])
            .width(Size::Fill(1.0))
            .height(Size::Hug);
        }
        let rows: Vec<El> = findings
            .items
            .iter()
            .enumerate()
            .map(|(i, f)| self.finding_row(i, f))
            .collect();
        sidebar([
            header,
            sidebar_group([column(rows).gap(tokens::SPACE_1).width(Size::Fill(1.0))]),
        ])
        .width(Size::Fill(1.0))
        .height(Size::Hug)
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
    /// nets / parts / pins), and — if the finding has a derived board point — queue a
    /// `CenterOn` on the focused board pane so the violation comes into view.
    ///
    /// # Click-to-zoom gap (deviation)
    ///
    /// damascene 0.4.5 has **no frame-this-rect ViewportRequest** — only `FitContent`,
    /// `ResetView`, and `CenterOn { key, point }`. So "zoom the focused board pane to the
    /// violation" is realised as a **`CenterOn`** (pan to the point, keeping the current
    /// zoom) rather than a true frame-to-rect. The finding's board point is centred; the
    /// zoom is left as the user set it. Recorded as a deviation in the report.
    pub(crate) fn select_finding(&self, index: usize, cx: &EventCx) {
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
        // CenterOn the focused board pane, if the finding has a board point. The request
        // wants a CONTENT-space point (logical px, pre-transform); the canvas maps the
        // finding's board-mm point through its board→content-px transform using the
        // pane's live laid-out rect (so the pan is exact regardless of the pane's
        // aspect ratio / fitted scale).
        if let (Some((mx, my)), Some(view)) = (f.board_mm, &derived.board)
            && let Some(pane) = self.focused_board_pane()
            && let Some(rect) = cx.rect_of_key(pane.canvas_key())
            && let Some(point) = view.canvas.board_mm_to_content_px(
                (mx, my),
                // The asset's honest content rect (natural viewBox size at the
                // viewport origin) — the frame CenterOn's content point lives in.
                view.canvas.content_rect((rect.x, rect.y, rect.w, rect.h)),
            )
        {
            self.pending.borrow_mut().push(ViewportRequest::CenterOn {
                key: pane.canvas_key().to_string(),
                point,
            });
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
            [text("Pass a path to a .ecad file to load a document.").muted()],
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
