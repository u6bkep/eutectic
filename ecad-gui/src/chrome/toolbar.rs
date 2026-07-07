//! The viewer toolbar: app title, filename badge, Save / Undo / Redo, the
//! per-source findings chips, the reload-error chip, layout toggle, tool
//! palette, and framing buttons. Moved out of `app/panels.rs` as pure code
//! motion (gui-module-split).

use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::pane::{LAYOUT_TOGGLE_KEY, REDO_KEY, SAVE_KEY, UNDO_KEY, findings_chip_key};
use crate::app::{EcadApp, PaneLayout};
use crate::findings::FindingSource;
use crate::tool::Tool;
use damascene_core::prelude::*;
use ecad_core::diagnostic::Severity;

impl EcadApp {
    /// The toolbar: app title, filename badge (dirty-dot suffixed), Save (when the
    /// doc has a path) + Undo / Redo, the dual/stacked layout toggle, the global
    /// tool palette, and Fit / Reset framing buttons + a live zoom-percent readout.
    pub(crate) fn viewer_toolbar(&self, zoom: f32) -> El {
        let mut name = self
            .domain
            .filename
            .clone()
            .unwrap_or_else(|| "untitled".into());
        // The dirty marker (m6): commits not yet written to the file show as a
        // bullet on the filename badge, cleared by Save / external reload.
        if self.domain.edit.dirty {
            name.push_str(" •");
        }
        let active = self.tool.get();
        let tool_buttons: Vec<El> = Tool::all()
            .into_iter()
            .map(|t| {
                let b = button(t.label()).key(t.key());
                if t == active { b.primary() } else { b }
            })
            .collect();
        let layout_label = match self.layout.get() {
            PaneLayout::Dual => "Dual",
            PaneLayout::Stacked => "Stacked",
        };
        // The per-source findings chips (mockup chrome): one chip per source (DRC / ERC /
        // NET / LIB) shown only when nonzero, tinted by that source's worst severity; a
        // single neutral ✓ chip when every source is clean. Any chip click toggles the
        // findings panel. The reload-error banner chip (permissive philosophy) sits
        // beside them whenever the freshest source failed to load — unmissable, never a
        // toast.
        let mut lead: Vec<El> = vec![toolbar_title("ecad"), badge(name).info()];
        // Save renders only for a doc that HAS a source path (the m6 save model:
        // no-path docs — fixtures — have no save affordance), primary while dirty
        // so the pending state is glanceable. Undo / Redo always render (no-ops
        // on empty stacks).
        if self.domain.source_path.is_some() {
            let save = button("Save").key(SAVE_KEY);
            lead.push(if self.domain.edit.dirty {
                save.primary()
            } else {
                save
            });
        }
        lead.push(button("Undo").key(UNDO_KEY));
        lead.push(button("Redo").key(REDO_KEY));
        lead.extend(self.findings_chips());
        if let Some(err) = &self.domain.reload_error {
            lead.push(reload_error_chip(err));
        }
        // The save/commit failure chip (m6): persists until the next success.
        if let Some(err) = &self.domain.edit.error {
            let first = err.lines().next().unwrap_or(err);
            lead.push(badge(format!("edit failed: {first}")).destructive());
        }
        lead.push(button(layout_label).key(LAYOUT_TOGGLE_KEY));
        lead.push(button("Libraries").key(LIBRARIES_TOGGLE_KEY));
        lead.push(spacer());
        lead.push(row(tool_buttons).gap(tokens::SPACE_1));
        lead.push(text(format!("{:.0}%", zoom * 100.0)).muted().mono());
        lead.push(button("Fit").key("fit"));
        lead.push(button("Reset").key("reset"));
        toolbar(lead)
            .gap(tokens::SPACE_2)
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2))
    }

    /// The per-source findings chips (mockup menu-bar chrome): one chip per
    /// [`FindingSource`] that has findings this revision, in DRC/ERC/NET/LIB order,
    /// each reading `"NAME n"` (n = total findings for that source) and tinted by the
    /// source's worst severity — red (`destructive`) if any error, amber (`warning`)
    /// otherwise, both through the theme's semantic colors. When every source is clean
    /// a single neutral `"✓"` chip is shown instead. Every chip (including the ✓ one)
    /// is a click-to-toggle-the-findings-panel affordance keyed distinctly. Reads the
    /// cached findings — never recomputes.
    pub(crate) fn findings_chips(&self) -> Vec<El> {
        let findings = &self.derived.borrow().findings;
        // A clickable chip: keyed + focusable + pointer cursor, so a click routes to the
        // app (handled as a findings-panel toggle) exactly like the panel's Hide/Show.
        let chip = |label: String, tag: &str| {
            badge(label)
                .key(findings_chip_key(tag))
                .focusable()
                .cursor(Cursor::Pointer)
        };
        let mut chips: Vec<El> = Vec::new();
        for source in FindingSource::all() {
            let Some((count, worst)) = findings.source_summary(source) else {
                continue;
            };
            let c = chip(format!("{} {count}", source.label()), source.label());
            chips.push(match worst {
                Severity::Error => c.destructive(),
                _ => c.warning(),
            });
        }
        if chips.is_empty() {
            // All sources clean → a single neutral ✓ chip, still click-to-toggle.
            chips.push(chip("✓".to_string(), "ok").muted());
        }
        chips
    }
}

/// The persistent reload-error chip (m5): an unmissable destructive badge in the
/// toolbar shown whenever the *freshest* source failed to parse/elaborate while the
/// last-good doc stays rendered. Not a toast — it persists until a good reload clears
/// `reload_error`. The full error is available in the badge label's first line.
fn reload_error_chip(err: &str) -> El {
    // Compact the multi-line diagnostic to its first line for the chip; the banner is a
    // glanceable "reload failed" cue, not the full report surface.
    let first = err.lines().next().unwrap_or(err);
    badge(format!("reload failed: {first}")).destructive()
}
