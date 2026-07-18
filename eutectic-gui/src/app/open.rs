//! Fresh-document open flow shared by File ▸ Open, Ctrl+O, and recent files.

use super::canvas_pane::PaneCam;
use super::domain::DerivedCaches;
use super::{EutecticApp, PaneId, PaneLayout, PaneState, ViewKind};
use crate::chrome::actions::ChromeNotice;
use crate::chrome::dialogs::ChromeDialog;
use crate::open_dialog::{OpenDialogLauncher, OpenMsg, WakeFn, load_domain};
use crate::recents::RecentFiles;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Sender;

pub(crate) const OPEN_KEY: &str = "file:open";
pub(crate) const OPEN_RECENT_KEY: &str = "file:open-recent";
pub(crate) const RECENT_POPOVER_KEY: &str = "file:recent-popover";
const RECENT_ITEM_PREFIX: &str = "file:recent:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingOpen {
    Dialog,
    Path(PathBuf),
}

pub(crate) fn recent_item_key(index: usize) -> String {
    format!("{RECENT_ITEM_PREFIX}{index}")
}

pub(crate) fn recent_item_index(key: &str) -> Option<usize> {
    key.strip_prefix(RECENT_ITEM_PREFIX)?.parse().ok()
}

impl EutecticApp {
    pub fn with_open_services(
        mut self,
        launcher: Arc<dyn OpenDialogLauncher>,
        wake: WakeFn,
        watch_path_tx: Sender<PathBuf>,
    ) -> EutecticApp {
        self.open_dialog_launcher = launcher;
        self.background_wakeup = wake;
        self.watch_path_tx = Some(watch_path_tx);
        self
    }

    pub fn with_recents(mut self, recents: RecentFiles, save_path: Option<PathBuf>) -> EutecticApp {
        self.recents = std::cell::RefCell::new(recents);
        self.recents_path = save_path;
        self
    }

    pub fn recent_paths(&self) -> Vec<PathBuf> {
        self.recents.borrow().paths().to_vec()
    }

    pub fn open_mailbox_push(&self, msg: OpenMsg) {
        self.open_mailbox.push(msg);
    }

    pub(crate) fn request_open_dialog(&mut self) {
        self.request_open(PendingOpen::Dialog);
    }

    pub(crate) fn request_recent(&mut self, index: usize) {
        let path = self.recents.borrow().paths().get(index).cloned();
        if let Some(path) = path {
            self.request_open(PendingOpen::Path(path));
        }
    }

    fn request_open(&mut self, request: PendingOpen) {
        if self.dirty() {
            *self.pending_open.borrow_mut() = Some(request);
            self.chrome_dialog.set(Some(ChromeDialog::ConfirmOpen));
        } else {
            self.resume_open(request);
        }
    }

    pub(crate) fn confirm_open_save(&mut self) {
        let request = self.pending_open.borrow_mut().take();
        self.chrome_dialog.set(None);
        self.save();
        if self.dirty() {
            return;
        }
        if let Some(request) = request {
            self.resume_open(request);
        }
    }

    pub(crate) fn confirm_open_discard(&mut self) {
        let request = self.pending_open.borrow_mut().take();
        self.chrome_dialog.set(None);
        if let Some(request) = request {
            self.open_discard_approved
                .set(request == PendingOpen::Dialog);
            self.resume_open(request);
        }
    }

    pub(crate) fn cancel_pending_open(&self) {
        self.pending_open.borrow_mut().take();
        self.open_discard_approved.set(false);
        self.chrome_dialog.set(None);
    }

    fn resume_open(&mut self, request: PendingOpen) {
        match request {
            PendingOpen::Dialog => self.launch_open_dialog(),
            PendingOpen::Path(path) => self.open_path(path),
        }
    }

    fn launch_open_dialog(&self) {
        if self.open_dialog_busy.replace(true) {
            return;
        }
        self.open_dialog_launcher
            .launch(self.open_mailbox.sender(), self.background_wakeup.clone());
    }

    pub(crate) fn drain_open_mailbox(&mut self) {
        let Some(OpenMsg::Picked(path)) = self.open_mailbox.drain() else {
            return;
        };
        self.open_dialog_busy.set(false);
        let discard_approved = self.open_discard_approved.replace(false);
        if let Some(path) = path {
            if discard_approved {
                self.resume_open(PendingOpen::Path(path));
            } else {
                self.request_open(PendingOpen::Path(path));
            }
        }
    }

    fn open_path(&mut self, path: PathBuf) {
        let fallback = super::LibSource::Fixed(eutectic_core::part::part_library());
        let lib_source = std::mem::replace(&mut self.domain.lib_source, fallback);
        let old_revision = self.domain.revision;
        let loaded = load_domain(&path, lib_source);
        let opened_path = loaded.absolute_path.clone();
        self.domain = loaded.domain;
        self.domain.revision = old_revision.saturating_add(1);
        *self.derived.borrow_mut() = match &self.domain.doc {
            Ok(doc) => DerivedCaches::build(doc, &self.domain.lib, &self.domain.lib_notes),
            Err(_) => DerivedCaches::empty(),
        };
        self.reset_for_fresh_open();

        if let Some(tx) = &self.watch_path_tx {
            let _ = tx.send(opened_path.clone());
        }
        if loaded.success {
            let save_result = {
                let mut recent = self.recents.borrow_mut();
                recent.push(opened_path.clone());
                self.recents_path
                    .as_deref()
                    .map(|save_path| recent.save(save_path))
            };
            *self.chrome_notice.borrow_mut() = match save_result {
                Some(Err(error)) => Some(ChromeNotice::error(format!(
                    "opened {}; recent files save failed: {error}",
                    opened_path.display()
                ))),
                _ => Some(ChromeNotice::success(format!(
                    "opened {}",
                    opened_path.display()
                ))),
            };
        } else {
            *self.chrome_notice.borrow_mut() = None;
        }
    }

    fn reset_for_fresh_open(&self) {
        *self.panes.borrow_mut() = [
            PaneState::new(ViewKind::Board),
            PaneState::new(ViewKind::Schematic),
        ];
        self.layout.set(PaneLayout::Dual);
        self.maximized.set(None);
        self.split_weights.set([1.0, 1.0]);
        *self.split_drag.borrow_mut() = Default::default();
        self.split_extent.set(0.0);
        self.focused_pane.set(PaneId::A);
        *self.pane_cams.borrow_mut() = [PaneCam::default(), PaneCam::default()];
        self.pane_px.set([None, None]);
        self.strip_px.set([None, None]);
        self.hidden.borrow_mut().clear();
        self.tools.borrow_mut().clear();
        self.measure.set(Default::default());
        self.measure_pane.set(PaneId::A);
        *self.drag.borrow_mut() = None;
        self.suppress_click.set(false);
        *self.route.borrow_mut() = None;
        *self.trace_drag.borrow_mut() = None;
        *self.camera_pan.borrow_mut() = None;
        *self.raw.borrow_mut() = Default::default();
        *self.active_layer.borrow_mut() = None;
        self.cursor_board_mm.set(None);
        self.cursor_px.set([None, None]);
        self.open_menu.borrow_mut().take();
        self.recent_open.set(false);
    }
}
