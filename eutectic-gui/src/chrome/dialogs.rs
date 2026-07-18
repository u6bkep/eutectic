//! Minimal Help dialogs. The keymap is intentionally a literal inventory of
//! the chords registered in `app/events.rs`, not an aspirational command list.

use crate::app::EutecticApp;
use damascene_core::prelude::*;

pub(crate) const KEYMAP_KEY: &str = "dialog:keymap:open";
pub(crate) const ABOUT_KEY: &str = "dialog:about:open";
pub(crate) const DIALOG_CLOSE_KEY: &str = "dialog:chrome:close";
pub(crate) const OPEN_SAVE_KEY: &str = "dialog:open:save";
pub(crate) const OPEN_DISCARD_KEY: &str = "dialog:open:discard";
pub(crate) const OPEN_CANCEL_KEY: &str = "dialog:open:cancel";
const DIALOG_ROOT_KEY: &str = "chrome-dialog";

/// Keys actually handled by `app/events.rs`: registered hotkeys plus the raw
/// contextual editor keys that deliberately stay out of the global table.
pub(crate) const WIRED_CHORDS: &[(&str, &str)] = &[
    ("Ctrl+S", "Save"),
    ("Ctrl+O", "Open"),
    ("Ctrl+Z", "Undo"),
    ("Ctrl+Shift+Z", "Redo"),
    ("Ctrl+Y", "Redo"),
    ("Ctrl++", "Zoom in"),
    ("Ctrl+=", "Zoom in"),
    ("Ctrl+-", "Zoom out"),
    ("Ctrl+K", "Command palette"),
    ("Del", "Delete selection"),
    ("R", "Rotate selection"),
    ("Esc", "Cancel gesture/tool or clear selection"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChromeDialog {
    Keymap,
    About,
    ConfirmOpen,
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
            ChromeDialog::ConfirmOpen => vec![
                text("This document has unsaved changes. Save it before opening another document?")
                    .wrap_text(),
                row([
                    button("Cancel").key(OPEN_CANCEL_KEY),
                    spacer(),
                    button("Discard changes").key(OPEN_DISCARD_KEY),
                    button("Save and Open").key(OPEN_SAVE_KEY),
                ])
                .gap(tokens::SPACE_2),
            ],
        };
        Some(modal(
            DIALOG_ROOT_KEY,
            match open {
                ChromeDialog::Keymap => "Keymap",
                ChromeDialog::About => "About eutectic",
                ChromeDialog::ConfirmOpen => "Unsaved changes",
            },
            body,
        ))
    }

    pub(crate) fn handle_chrome_dialog_event(&mut self, event: &UiEvent) -> bool {
        let Some(open) = self.chrome_dialog.get() else {
            return false;
        };
        if open == ChromeDialog::ConfirmOpen {
            if event.is_click_or_activate(OPEN_SAVE_KEY) {
                self.confirm_open_save();
            } else if event.is_click_or_activate(OPEN_DISCARD_KEY) {
                self.confirm_open_discard();
            } else if event.is_click_or_activate(OPEN_CANCEL_KEY)
                || event.is_click_or_activate(&format!("{DIALOG_ROOT_KEY}:dismiss"))
                || event.kind == UiEventKind::Escape
            {
                self.cancel_pending_open();
            }
            return true;
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
