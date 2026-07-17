//! Minimal Help dialogs. The keymap is intentionally a literal inventory of
//! the chords registered in `app/events.rs`, not an aspirational command list.

use crate::app::EutecticApp;
use damascene_core::prelude::*;

pub(crate) const KEYMAP_KEY: &str = "dialog:keymap:open";
pub(crate) const ABOUT_KEY: &str = "dialog:about:open";
pub(crate) const DIALOG_CLOSE_KEY: &str = "dialog:chrome:close";
const DIALOG_ROOT_KEY: &str = "chrome-dialog";

/// Keys actually handled by `app/events.rs`: registered hotkeys plus Escape's
/// direct contextual handler.
pub(crate) const WIRED_CHORDS: &[(&str, &str)] = &[
    ("Ctrl+S", "Save"),
    ("Ctrl+Z", "Undo"),
    ("Ctrl+Shift+Z", "Redo"),
    ("Ctrl+Y", "Redo"),
    ("+", "Zoom in"),
    ("−", "Zoom out"),
    ("Esc", "Cancel gesture/tool or clear selection"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromeDialog {
    Keymap,
    About,
}

impl EutecticApp {
    pub(crate) fn chrome_dialog_overlay(&self) -> Option<El> {
        let open = self.chrome_dialog.get()?;
        let body = match open {
            ChromeDialog::About => vec![
                text(format!("eutectic {}", env!("CARGO_PKG_VERSION"))).mono(),
                text("A from-scratch electronic design automation suite.")
                    .muted()
                    .wrap_text(),
                row([spacer(), button("Close").key(DIALOG_CLOSE_KEY)]),
            ],
            ChromeDialog::Keymap => {
                let mut rows: Vec<El> = WIRED_CHORDS
                    .iter()
                    .map(|(chord, action)| {
                        row([
                            text(*chord).mono().width(Size::Fixed(130.0)),
                            text(*action).muted(),
                        ])
                        .gap(tokens::SPACE_3)
                    })
                    .collect();
                rows.push(row([spacer(), button("Close").key(DIALOG_CLOSE_KEY)]));
                rows
            }
        };
        Some(modal(
            DIALOG_ROOT_KEY,
            match open {
                ChromeDialog::Keymap => "Keymap",
                ChromeDialog::About => "About eutectic",
            },
            body,
        ))
    }

    pub(crate) fn handle_chrome_dialog_event(&self, event: &UiEvent) -> bool {
        if self.chrome_dialog.get().is_none() {
            return false;
        }
        if event.is_click_or_activate(DIALOG_CLOSE_KEY)
            || event.is_click_or_activate(&format!("{DIALOG_ROOT_KEY}:dismiss"))
            || event.kind == UiEventKind::Escape
        {
            self.chrome_dialog.set(None);
        }
        // A modal owns every event while open, including clicks on inert panel
        // text, so nothing dispatches to the document behind its scrim.
        true
    }
}
