//! Chrome command handlers that are independent of the menu/toolbar widgets:
//! deterministic convention-based exports, display/grid toggles, focused-pane
//! zoom, and clean quit state.

use crate::app::pane::SidebarSection;
use crate::app::{EutecticApp, PaneId};
use crate::chrome::dialogs::{ABOUT_KEY, ChromeDialog, KEYMAP_KEY};
use damascene_core::prelude::*;
use std::path::{Path, PathBuf};

pub(crate) const EXPORT_GERBERS_KEY: &str = "export:gerbers";
pub(crate) const EXPORT_SVG_KEY: &str = "export:svg";
pub(crate) const QUIT_KEY: &str = "app:quit";
pub(crate) const ZOOM_IN_KEY: &str = "zoom:in";
pub(crate) const ZOOM_OUT_KEY: &str = "zoom:out";
pub(crate) const UNITS_TOGGLE_KEY: &str = "display:units:toggle";
pub(crate) const GRID_TOGGLE_KEY: &str = "display:grid:toggle";
pub(crate) const FINDINGS_PANEL_KEY: &str = "sidebar:section:findings";

/// One persistent non-modal export result shown in the menu-bar status cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChromeNotice {
    pub(crate) message: String,
    pub(crate) error: bool,
}

impl ChromeNotice {
    fn success(message: String) -> ChromeNotice {
        ChromeNotice {
            message,
            error: false,
        }
    }

    fn error(message: String) -> ChromeNotice {
        ChromeNotice {
            message,
            error: true,
        }
    }
}

impl EutecticApp {
    /// Handle a routed chrome command. Kept outside `app/events.rs` so sibling
    /// event work has one narrow integration point.
    pub(crate) fn handle_chrome_event(&mut self, event: &UiEvent) -> bool {
        let clicked = |key| event.is_click_or_activate(key) || event.is_hotkey(key);
        if clicked(EXPORT_GERBERS_KEY) {
            self.export_gerbers();
        } else if clicked(EXPORT_SVG_KEY) {
            self.export_svg();
        } else if clicked(ZOOM_IN_KEY) {
            self.zoom_focused(1.25);
        } else if clicked(ZOOM_OUT_KEY) {
            self.zoom_focused(1.0 / 1.25);
        } else if clicked(UNITS_TOGGLE_KEY) {
            self.toggle_display_units();
        } else if clicked(GRID_TOGGLE_KEY) {
            self.toggle_grid_style();
        } else if clicked(FINDINGS_PANEL_KEY) {
            self.toggle_section(SidebarSection::Findings);
        } else if clicked(QUIT_KEY) {
            // No unsaved-confirm affordance exists in damascene today. Keep this
            // a plain, observable host exit request instead of terminating the
            // process from the UI event callback.
            self.quit_requested.set(true);
        } else if clicked(KEYMAP_KEY) {
            self.chrome_dialog.set(Some(ChromeDialog::Keymap));
        } else if clicked(ABOUT_KEY) {
            self.chrome_dialog.set(Some(ChromeDialog::About));
        } else {
            return false;
        }
        true
    }

    /// Export the engine's full Gerber/Excellon set to `<doc_dir>/fab/`.
    fn export_gerbers(&self) {
        let result = (|| -> Result<(PathBuf, usize), String> {
            let source = self
                .domain
                .source_path
                .as_deref()
                .ok_or_else(|| "document has no source path".to_string())?;
            let doc = self.domain.doc.as_ref().map_err(|e| e.to_string())?;
            let dir = source
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("fab");
            let files = eutectic_core::export::gerber_set(doc, &self.domain.lib)?;
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("creating {}: {e}", dir.display()))?;
            write_files(&dir, &files)?;
            Ok((dir, files.len()))
        })();
        *self.chrome_notice.borrow_mut() = Some(match result {
            Ok((path, count)) => {
                ChromeNotice::success(format!("exported {count} fab files to {}", path.display()))
            }
            Err(err) => ChromeNotice::error(format!("Gerber export failed: {err}")),
        });
    }

    /// Export board and schematic SVGs next to the source document.
    fn export_svg(&self) {
        let result = (|| -> Result<(PathBuf, usize), String> {
            let source = self
                .domain
                .source_path
                .as_deref()
                .ok_or_else(|| "document has no source path".to_string())?;
            let doc = self.domain.doc.as_ref().map_err(|e| e.to_string())?;
            let dir = source.parent().unwrap_or_else(|| Path::new("."));
            let stem = source
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| format!("{} has no UTF-8 file stem", source.display()))?;
            let files = vec![
                (
                    format!("{stem}.svg"),
                    eutectic_core::export::svg(doc, &self.domain.lib)?,
                ),
                (
                    format!("{stem}-schematic.svg"),
                    eutectic_core::schematic_svg::schematic_svg(doc, &self.domain.lib),
                ),
            ];
            write_files(dir, &files)?;
            Ok((dir.to_path_buf(), files.len()))
        })();
        *self.chrome_notice.borrow_mut() = Some(match result {
            Ok((path, count)) => {
                ChromeNotice::success(format!("exported {count} SVG files to {}", path.display()))
            }
            Err(err) => ChromeNotice::error(format!("SVG export failed: {err}")),
        });
    }

    fn zoom_focused(&self, factor: f64) {
        let pane: PaneId = self.focused_pane.get();
        self.pane_zoom_center(pane, factor);
    }
}

fn write_files(dir: &Path, files: &[(String, String)]) -> Result<(), String> {
    for (name, content) in files {
        let path = dir.join(name);
        std::fs::write(&path, content).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(())
}
