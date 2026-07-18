//! Fresh-document open flow shared by File ▸ Open, Ctrl+O, and recent files.

use super::canvas_pane::PaneCam;
use super::domain::DerivedCaches;
use super::{EutecticApp, PaneId, PaneState, PaneTree, ViewKind};
use crate::chrome::actions::ChromeNotice;
use crate::chrome::dialogs::ChromeDialog;
use crate::open_dialog::{OpenDialogLauncher, OpenMsg, OpenPoll, WakeFn, load_domain};
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
    Dialog { request_id: u64 },
    Path(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiscardApproval {
    request_id: u64,
    revision: u64,
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

    pub(crate) fn request_open_dialog(&mut self) {
        let request_id = self.next_open_request_id.get();
        self.next_open_request_id.set(request_id.saturating_add(1));
        self.request_open(PendingOpen::Dialog { request_id });
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
            *self.open_discard_approval.borrow_mut() = match request {
                PendingOpen::Dialog { request_id } => Some(DiscardApproval {
                    request_id,
                    revision: self.domain.revision,
                }),
                PendingOpen::Path(_) => None,
            };
            self.resume_open(request);
        }
    }

    pub(crate) fn cancel_pending_open(&self) {
        self.pending_open.borrow_mut().take();
        self.open_discard_approval.borrow_mut().take();
        self.chrome_dialog.set(None);
    }

    fn resume_open(&mut self, request: PendingOpen) {
        match request {
            PendingOpen::Dialog { request_id } => self.launch_open_dialog(request_id),
            PendingOpen::Path(path) => self.open_path(path),
        }
    }

    fn launch_open_dialog(&self, request_id: u64) {
        if self.open_dialog_busy.replace(true) {
            return;
        }
        self.active_dialog_request_id.set(Some(request_id));
        let reply = self.open_mailbox.begin_launch();
        self.open_dialog_launcher
            .launch(reply, self.background_wakeup.clone());
    }

    /// Drain one native-picker result when the shared chrome-dialog slot is free.
    /// Picks that arrive behind Keymap/About or an unrelated open confirmation remain
    /// queued in the mailbox; they cannot overwrite that dialog's `pending_open`.
    pub(crate) fn drain_open_mailbox(&mut self) {
        if self.chrome_dialog.get().is_some() {
            return;
        }
        match self.open_mailbox.poll() {
            OpenPoll::Empty => {}
            // NB: a never-launched OpenMailbox polls Disconnected every frame
            // (its sender starts dropped), so this arm re-clears these cells
            // until the first launch. Correct only because approval/request-id
            // are set synchronously in the same frame that launch_open_dialog
            // replaces the channel — deferring a launch past the frame that
            // records its approval would wipe the approval here.
            OpenPoll::Disconnected => {
                self.open_dialog_busy.set(false);
                self.active_dialog_request_id.set(None);
                self.open_discard_approval.borrow_mut().take();
            }
            OpenPoll::Message(OpenMsg::Picked(path)) => {
                self.open_dialog_busy.set(false);
                let request_id = self.active_dialog_request_id.take();
                let approval = self.open_discard_approval.borrow_mut().take();
                if let Some(path) = path {
                    let approved =
                        request_id
                            .zip(approval)
                            .is_some_and(|(request_id, approval)| {
                                approval.request_id == request_id
                                    && approval.revision == self.domain.revision
                            });
                    if approved {
                        self.resume_open(PendingOpen::Path(path));
                    } else {
                        self.request_open(PendingOpen::Path(path));
                    }
                }
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
        *self.panes.borrow_mut() = vec![
            Some(PaneState::new(ViewKind::Board)),
            Some(PaneState::new(ViewKind::Schematic)),
        ];
        *self.pane_tree.borrow_mut() = PaneTree::default();
        self.maximized.set(None);
        self.focused_pane.set(PaneId::A);
        *self.pane_cams.borrow_mut() = vec![Some(PaneCam::default()), Some(PaneCam::default())];
        *self.pane_px.borrow_mut() = vec![None, None];
        *self.strip_px.borrow_mut() = vec![None, None];
        self.reset_pane_gpu_slots();
        self.hidden.borrow_mut().clear();
        self.tools.borrow_mut().clear();
        self.measure.set(Default::default());
        self.measure_pane.set(PaneId::A);
        *self.drag.borrow_mut() = None;
        self.suppress_click.set(false);
        *self.route.borrow_mut() = None;
        self.route_pane.set(None);
        *self.trace_drag.borrow_mut() = None;
        *self.camera_pan.borrow_mut() = None;
        *self.raw.borrow_mut() = Default::default();
        *self.active_layer.borrow_mut() = None;
        self.cursor_board_mm.set(None);
        *self.cursor_px.borrow_mut() = vec![None, None];
        self.open_menu.borrow_mut().take();
        self.pane_view_menu.set(None);
        self.recent_open.set(false);
        self.explorer_filter.borrow_mut().clear();
        *self.explorer_filter_selection.borrow_mut() = Default::default();
        *self.inspector_ui.borrow_mut() = Default::default();
        self.palette_open.set(false);
        *self.palette_ui.borrow_mut() = Default::default();
        self.focus_requests.borrow_mut().clear();
    }
}
