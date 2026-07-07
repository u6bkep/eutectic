//! The right sidebar — a four-section accordion (Properties, Layers, Explorer,
//! Findings) whose headers are **always visible** regardless of expansion, per the
//! UI oracle (`docs/ui-oracle/shell.dc.html`) and `gui-architecture.md` ("Right
//! sidebar"). Each section's body expands/collapses independently; open bodies
//! share the remaining height (`Size::Fill`). The per-section *content* lives in the
//! sibling panel modules ([`crate::panels::properties`] / [`layers`](crate::panels::layers)
//! / [`explorer`](crate::panels::explorer) / [`findings`](crate::panels::findings));
//! this module owns the accordion shell (headers + height sharing).

use crate::app::EcadApp;
use crate::app::pane::SidebarSection;
use crate::panels::findings::findings_header_chips;
use damascene_core::prelude::*;

/// The sidebar width (the oracle's 288 px accordion column).
const SIDEBAR_WIDTH: f32 = 288.0;
/// The always-visible section-header height (the oracle's 34 px header row).
const HEADER_HEIGHT: f32 = 34.0;

impl EcadApp {
    /// The right sidebar: the four-section accordion. All four headers always render;
    /// the [`SectionOpen`](crate::app::pane) state governs which bodies are expanded.
    /// Open bodies each take a `Size::Fill` share of the remaining height and scroll
    /// internally; collapsed sections shrink to just their 34 px header (nothing lives
    /// below an invisible fold — every section is reachable at all times).
    pub(crate) fn right_sidebar(&self) -> El {
        let derived = self.derived.borrow();

        // The section content builders (each an `Option<El>` present only when the
        // section is open) and the header right-side status cells.
        //
        // Layers: the active layer name reads on the header's right; the body applies
        // to board panes, but the header is always present (a schematic-only doc gets
        // an empty-state line) — nothing lives below an invisible fold.
        let layer_status = self
            .active_layer_name()
            .map(|n| vec![text(n).mono().caption().muted()])
            .unwrap_or_default();

        // Flattened so the *headers* (which carry left/right padding) are direct
        // children of the panel surface: that is what keeps the `UnpaddedSurfacePanel`
        // lint quiet while the headers stay full-bleed (a section-wrapper column would
        // put an unpadded node on the panel's left/right edges). The open bodies are
        // `Size::Fill` siblings, so they share the remaining height between them; the
        // fixed-height headers and hairline separators take their natural size first.
        let mut kids: Vec<El> = Vec::new();
        for (i, section) in SidebarSection::all().into_iter().enumerate() {
            if i > 0 {
                kids.push(separator());
            }
            let status = match section {
                SidebarSection::Layers => layer_status.clone(),
                SidebarSection::Findings => findings_header_chips(&derived.findings),
                _ => Vec::new(),
            };
            let body = self.section_open(section).then(|| match section {
                SidebarSection::Properties => self.inspector_body(),
                SidebarSection::Layers => match &derived.board {
                    Some(view) => self.layer_panel_body(&view.layers),
                    None => {
                        sidebar_group([text("No board layers.").muted()]).width(Size::Fill(1.0))
                    }
                },
                SidebarSection::Explorer => self.explorer_body(&derived.explorer),
                SidebarSection::Findings => self.findings_body(&derived.findings),
            });
            kids.extend(self.sidebar_section(section, status, body));
        }

        sidebar(kids)
            // Full-bleed headers: no left/right panel inset (the headers' own
            // `SPACE_3` x-padding insets their content). Top/bottom padding inset the
            // first/last row off the panel edges (keeps `UnpaddedSurfacePanel` quiet
            // on those edges). `SPACE_1` gap ≥ `RING_WIDTH` so a separator never sits
            // on a neighbouring header's focus-ring band.
            .padding(Sides {
                top: tokens::SPACE_2,
                bottom: tokens::SPACE_2,
                left: 0.0,
                right: 0.0,
            })
            .gap(tokens::SPACE_1)
            .width(Size::Fixed(SIDEBAR_WIDTH))
            .height(Size::Fill(1.0))
    }

    /// One accordion section as a flat run of panel children: an always-visible header
    /// plus, when `body` is `Some`, a scrolling body. The header is fixed-height; an
    /// open body is `Size::Fill` so open sections share the sidebar's remaining height.
    fn sidebar_section(
        &self,
        section: SidebarSection,
        status: Vec<El>,
        body: Option<El>,
    ) -> Vec<El> {
        let is_open = body.is_some();
        let mut out = vec![self.sidebar_section_header(section, is_open, status)];
        if let Some(body) = body {
            // Pad the content inside the clipping scroll window (the scroll edge is
            // flush with the panel; the body needs breathing room, and the padding
            // keeps focusable rows' ring bands clear of the scroll scissor).
            let padded = column([body])
                .padding(Sides::all(tokens::SPACE_3))
                .width(Size::Fill(1.0))
                .height(Size::Hug);
            out.push(
                scroll([padded])
                    .key(format!("sidebar:body:{}", section.slug()))
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0))
                    .min_height(0.0),
            );
        }
        out
    }

    /// The always-visible header row of an accordion section: a leading icon, the
    /// uppercase section label (fills the row and ellipsizes so it never collides with
    /// what follows), an optional right-side status (the layer name / the findings
    /// count chips), and the open/closed chevron. Keyed by
    /// [`SidebarSection::toggle_key`] so a click routes to `toggle_section`. Built on
    /// the same focusable-list-row recipe as the findings rows.
    fn sidebar_section_header(&self, section: SidebarSection, open: bool, status: Vec<El>) -> El {
        let chevron = if open {
            "chevron-down"
        } else {
            "chevron-right"
        };
        let mut cells: Vec<El> = vec![
            icon(section.icon())
                .icon_size(tokens::ICON_SM)
                .color(tokens::MUTED_FOREGROUND),
            text(section.label())
                .caption()
                .semibold()
                .muted()
                .ellipsis()
                .width(Size::Fill(1.0)),
        ];
        cells.extend(status);
        cells.push(
            icon(chevron)
                .icon_size(tokens::ICON_XS)
                .color(tokens::MUTED_FOREGROUND),
        );
        row(cells)
            .key(section.toggle_key())
            .style_profile(StyleProfile::Solid)
            .metrics_role(MetricsRole::ListItem)
            .fill(tokens::CARD)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_3, 0.0))
            .height(Size::Fixed(HEADER_HEIGHT))
            .align(Align::Center)
            .width(Size::Fill(1.0))
            .focusable()
            .cursor(Cursor::Pointer)
            .ghost()
    }
}

#[cfg(test)]
impl EcadApp {
    /// Test hook: the Findings accordion header for an arbitrary [`Findings`], so the
    /// truncation-regression test can drive it with pathologically long counts without
    /// having to synthesise a doc that produces them.
    pub(crate) fn findings_header_for_test(
        &self,
        findings: &crate::findings::Findings,
        open: bool,
    ) -> El {
        self.sidebar_section_header(
            SidebarSection::Findings,
            open,
            findings_header_chips(findings),
        )
    }
}
