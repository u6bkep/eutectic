//! The Libraries menu (library packages, slice 2): the single libraries UI —
//! a modal over the whole window with registry rows (name → path + load status +
//! remove), an add-entry form, and a close affordance. Live edit semantics:
//! add/remove saves the registry file immediately AND re-resolves the current
//! doc through the same path a source reload takes. Split out of `app.rs` as
//! pure code motion.

use crate::app::EutecticApp;
use crate::app::domain::LibSource;
use crate::registry::{self, Registry};
use damascene_core::prelude::*;

/// The toolbar button that opens/closes the Libraries menu (the single
/// libraries UI — registry rows + add/remove + per-row load status).
pub(crate) const LIBRARIES_TOGGLE_KEY: &str = "libraries:toggle";
/// The Libraries menu's Close button.
pub(crate) const LIBRARIES_CLOSE_KEY: &str = "libraries:close";
/// The Libraries modal root key; damascene's `modal` emits `{key}:dismiss` for
/// a click on the scrim outside the panel.
const LIBRARIES_MODAL_KEY: &str = "libraries";
/// The Libraries menu's add-entry button.
pub(crate) const LIBRARIES_ADD_KEY: &str = "libraries:add";
/// The two add-entry text inputs (damascene `text_input` keys).
const LIB_NAME_INPUT_KEY: &str = "libraries:input:name";
const LIB_PATH_INPUT_KEY: &str = "libraries:input:path";
/// The route-key prefix of a registry row's Remove button (name appended).
const LIBRARIES_REMOVE_PREFIX: &str = "libraries:remove:";

/// The Remove-button key for the registry row named `name`.
pub(crate) fn library_remove_key(name: &str) -> String {
    format!("{LIBRARIES_REMOVE_PREFIX}{name}")
}

/// Libraries-menu interaction state: the two add-entry input strings, the
/// shared damascene text [`Selection`] (caret + highlight — the app owns it,
/// per the `text_input` contract), and the last add/remove/save error shown
/// inline in the panel.
#[derive(Default)]
pub(crate) struct LibUi {
    /// The add-entry "name" input value.
    pub(crate) name: String,
    /// The add-entry "path" input value.
    pub(crate) path: String,
    /// The global text selection (which input owns the caret, and where).
    pub(crate) selection: Selection,
    /// The last registry-edit error (invalid name / relative path / save
    /// failure), rendered inline until the next successful edit.
    pub(crate) error: Option<String>,
}

/// One cached Libraries-menu row: name, bound path, and its load status.
pub(crate) type LibRow = (String, std::path::PathBuf, registry::RowStatus);

impl EutecticApp {
    /// The cached Libraries-menu rows (name, path, load status), recomputed
    /// lazily when the cache was invalidated (menu open, registry edit). Row
    /// status probes the filesystem ([`registry::row_status`]) — every row is
    /// probed, whether or not the current doc `use`s it, so the menu doubles
    /// as the "why is my library broken" diagnostic.
    fn lib_rows(&self) -> Vec<LibRow> {
        let mut cache = self.lib_statuses.borrow_mut();
        cache
            .get_or_insert_with(|| match &self.domain.lib_source {
                LibSource::Registry { registry, .. } => registry
                    .iter()
                    .map(|(n, p)| (n.to_string(), p.to_path_buf(), registry::row_status(p)))
                    .collect(),
                LibSource::Fixed(_) => Vec::new(),
            })
            .clone()
    }

    /// The Libraries modal: registry rows, the add-entry form (two text
    /// inputs + Add), the inline error line, and Close. Rendered through
    /// damascene's `modal` (scrim + centered panel), consistent with the
    /// existing panel styling (cards + badges + muted captions).
    pub(crate) fn libraries_modal(&self) -> El {
        let ui = self.lib_ui.borrow();
        let rows = self.lib_rows();

        let mut body: Vec<El> =
            vec![
            text("Per-machine bindings for `use NAME` library packages: NAME → absolute directory.")
                .muted()
                .wrap_text()
                .width(Size::Fill(1.0)),
        ];

        if let LibSource::Fixed(_) = self.domain.lib_source {
            body.push(
                text("No registry attached (fixed library) — edits here cannot apply.").muted(),
            );
        } else if rows.is_empty() {
            body.push(text("No libraries registered.").muted());
        }
        if !rows.is_empty() {
            let row_els: Vec<El> = rows.iter().map(library_row).collect();
            body.push(column(row_els).gap(tokens::SPACE_1).width(Size::Fill(1.0)));
        }

        body.push(separator());
        body.push(
            row([
                text_input_with(
                    LIB_NAME_INPUT_KEY,
                    &ui.name,
                    &ui.selection,
                    TextInputOpts::default().placeholder("name"),
                )
                .width(Size::Fixed(110.0)),
                text_input_with(
                    LIB_PATH_INPUT_KEY,
                    &ui.path,
                    &ui.selection,
                    TextInputOpts::default().placeholder("/absolute/path/to/package"),
                )
                .width(Size::Fill(1.0)),
                button("Add").key(LIBRARIES_ADD_KEY).primary(),
            ])
            .gap(tokens::SPACE_2)
            .align(Align::Center)
            .width(Size::Fill(1.0)),
        );
        if let Some(err) = &ui.error {
            body.push(
                alert([alert_description(err.clone())])
                    .destructive()
                    .width(Size::Fill(1.0)),
            );
        }
        body.push(row([spacer(), button("Close").key(LIBRARIES_CLOSE_KEY)]).width(Size::Fill(1.0)));

        modal(LIBRARIES_MODAL_KEY, "Libraries", body)
    }

    /// Handle an event while the Libraries menu is open. Returns `true` when
    /// the event was consumed by the menu (everything else stays behind the
    /// scrim; unconsumed events fall through to the normal handlers).
    pub(crate) fn handle_libraries_event(&mut self, event: &UiEvent) -> bool {
        // Close affordances: the Close button, a scrim click, Escape.
        if event.is_click_or_activate(LIBRARIES_CLOSE_KEY)
            || event.is_click_or_activate(&format!("{LIBRARIES_MODAL_KEY}:dismiss"))
            || event.kind == UiEventKind::Escape
        {
            self.libraries_open.set(false);
            return true;
        }

        // The two add-entry text inputs: fold edits into the app-owned strings
        // + shared Selection. Pointer events self-gate inside `apply_event`,
        // while key/text events must be offered only to their runtime-routed
        // focused target (otherwise the first input would claim path typing).
        {
            let mut ui = self.lib_ui.borrow_mut();
            let LibUi {
                name,
                path,
                selection,
                ..
            } = &mut *ui;
            let handled = match event.target_key().or_else(|| event.route()) {
                Some(LIB_NAME_INPUT_KEY) => {
                    text_input::apply_event(name, selection, event, LIB_NAME_INPUT_KEY)
                }
                Some(LIB_PATH_INPUT_KEY) => {
                    text_input::apply_event(path, selection, event, LIB_PATH_INPUT_KEY)
                }
                _ => false,
            };
            if handled {
                return true;
            }
            // A runtime selection update (focus moved, drag-select) not folded by
            // either input: adopt it so `App::selection` reports the live state.
            if let Some(sel) = &event.selection {
                *selection = sel.clone();
            }
        }

        if event.is_click_or_activate(LIBRARIES_ADD_KEY) {
            self.add_library_entry();
            return true;
        }
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(name) = event
                .route()
                .and_then(|r| r.strip_prefix(LIBRARIES_REMOVE_PREFIX))
        {
            let name = name.to_string();
            self.remove_library_entry(&name);
            return true;
        }
        false
    }

    /// The Add button: register the (trimmed) name → path from the inputs.
    /// Validation (single-token name, absolute path) happens in
    /// [`Registry::set`]; a failure renders inline and leaves the inputs
    /// untouched for correction.
    fn add_library_entry(&mut self) {
        let (name, path) = {
            let ui = self.lib_ui.borrow();
            (ui.name.trim().to_string(), ui.path.trim().to_string())
        };
        match self.edit_registry(|reg| reg.set(&name, std::path::Path::new(&path))) {
            Ok(()) => {
                let mut ui = self.lib_ui.borrow_mut();
                ui.name.clear();
                ui.path.clear();
                ui.error = None;
            }
            Err(e) => self.lib_ui.borrow_mut().error = Some(e),
        }
    }

    /// A row's Remove button: drop the entry (removing a name that is already
    /// gone is a no-op, not an error).
    fn remove_library_entry(&mut self, name: &str) {
        match self.edit_registry(|reg| {
            reg.remove(name);
            Ok(())
        }) {
            Ok(()) => self.lib_ui.borrow_mut().error = None,
            Err(e) => self.lib_ui.borrow_mut().error = Some(e),
        }
    }

    /// Apply `edit` to the registry, then the **live edit semantics**: save
    /// the registry file immediately (when a save path is wired — `main.rs`
    /// wires the per-machine default; fixtures may leave it `None`),
    /// invalidate the row-status cache, and re-resolve + re-elaborate the
    /// current doc through `EutecticApp::swap_source` — the same swap core a source
    /// reload uses, so the revision bumps and cameras / selection / layer
    /// visibility are preserved exactly as a reload preserves them. Unlike an
    /// *external* reload it does NOT touch the m6 editing state: the doc's
    /// source is the serialize-refreshed current state (unsaved edits
    /// included), so dirty / undo / redo survive a registry edit. The empty
    /// (no-document) state skips the re-elaborate (there is no source to
    /// resolve).
    fn edit_registry(
        &mut self,
        edit: impl FnOnce(&mut Registry) -> Result<(), String>,
    ) -> Result<(), String> {
        let LibSource::Registry {
            registry,
            save_path,
        } = &mut self.domain.lib_source
        else {
            return Err("no registry attached (fixed library)".to_string());
        };
        edit(registry)?;
        let saved = match save_path {
            Some(p) => registry.save(p),
            None => Ok(()),
        };
        *self.lib_statuses.borrow_mut() = None;
        if self.domain.filename.is_some() {
            // Re-elaborating the last-GOOD source says nothing about a *newer*
            // broken source on disk: if a reload-error banner is up, it must
            // survive this registry-triggered reload (only a good reload of
            // fresh source may clear it).
            let prior_error = self.domain.reload_error.clone();
            let source = self.domain.source.clone();
            self.swap_source(source);
            if self.domain.reload_error.is_none() {
                self.domain.reload_error = prior_error;
            }
        }
        saved
    }
}

/// One Libraries-menu registry row: the name + status badge + Remove button
/// over the bound path (and, for a manifest error, the loader's message).
fn library_row((name, path, status): &LibRow) -> El {
    use crate::registry::RowStatus;
    let status_badge = match status {
        RowStatus::Ok { parts } => badge(format!("OK · {parts} parts")).success(),
        RowStatus::Missing => badge("path missing").destructive(),
        RowStatus::Error(_) => badge("manifest error").destructive(),
    };
    let mut lines = vec![
        row([
            text(name.clone()).mono().width(Size::Fill(1.0)).ellipsis(),
            status_badge,
            button("Remove").key(library_remove_key(name)),
        ])
        .gap(tokens::SPACE_2)
        .align(Align::Center)
        .width(Size::Fill(1.0)),
        text(path.display().to_string())
            .muted()
            .caption()
            .mono()
            .width(Size::Fill(1.0))
            .ellipsis(),
    ];
    if let RowStatus::Error(msg) = status {
        lines.push(
            text(msg.clone())
                .caption()
                .wrap_text()
                .width(Size::Fill(1.0)),
        );
    }
    column(lines)
        .gap(tokens::SPACE_1)
        .fill(tokens::CARD)
        .radius(tokens::RADIUS_SM)
        .padding(Sides::xy(tokens::SPACE_2, tokens::SPACE_1))
        .width(Size::Fill(1.0))
}
