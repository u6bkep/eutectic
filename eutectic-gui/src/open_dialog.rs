//! Native File ▸ Open plumbing: an injectable dialog launcher, a polled
//! mailbox and permissive fresh-domain loading for paths chosen in the dialog.

use crate::app::{DomainState, LibSource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenMsg {
    Picked(Option<PathBuf>),
}

pub struct OpenMailbox {
    rx: std::cell::RefCell<Receiver<OpenMsg>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenPoll {
    Empty,
    Disconnected,
    Message(OpenMsg),
}

impl OpenMailbox {
    pub fn new() -> OpenMailbox {
        let (tx, rx) = channel();
        drop(tx);
        OpenMailbox {
            rx: std::cell::RefCell::new(rx),
        }
    }

    /// Replace any completed dialog channel and hand its sole sender to a launcher.
    /// With no retained sender, a launcher thread that exits without replying makes the
    /// receiver observably disconnected instead of wedging the busy flag forever.
    pub(crate) fn begin_launch(&self) -> Sender<OpenMsg> {
        let (tx, rx) = channel();
        *self.rx.borrow_mut() = rx;
        tx
    }

    pub(crate) fn poll(&self) -> OpenPoll {
        match self.rx.borrow().try_recv() {
            Ok(msg) => OpenPoll::Message(msg),
            Err(TryRecvError::Empty) => OpenPoll::Empty,
            Err(TryRecvError::Disconnected) => OpenPoll::Disconnected,
        }
    }
}

impl Default for OpenMailbox {
    fn default() -> Self {
        Self::new()
    }
}

pub trait OpenDialogLauncher: Send + Sync {
    fn launch(&self, reply: Sender<OpenMsg>, wake: WakeFn);
}

impl<F> OpenDialogLauncher for F
where
    F: Fn(Sender<OpenMsg>, WakeFn) + Send + Sync,
{
    fn launch(&self, reply: Sender<OpenMsg>, wake: WakeFn) {
        self(reply, wake);
    }
}

pub struct NativeOpenDialog;

impl OpenDialogLauncher for NativeOpenDialog {
    fn launch(&self, reply: Sender<OpenMsg>, wake: WakeFn) {
        std::thread::spawn(move || {
            let path = rfd::FileDialog::new()
                .set_title("Open eutectic document")
                .add_filter("eutectic document", &["eut"])
                .add_filter("All files", &["*"])
                .pick_file();
            let _ = reply.send(OpenMsg::Picked(path));
            wake();
        });
    }
}

pub struct LoadedDomain {
    pub domain: DomainState,
    pub absolute_path: PathBuf,
    pub success: bool,
}

/// Load a path through the same `DomainState` constructors as startup. A read
/// or elaboration failure becomes a no-document domain for the existing red
/// error card; it never terminates the process.
pub fn load_domain(path: &Path, lib_source: LibSource) -> LoadedDomain {
    let absolute_path = absolute_path(path);
    let filename = absolute_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let mut domain = match std::fs::read_to_string(&absolute_path) {
        Ok(source) => match lib_source {
            LibSource::Fixed(lib) => {
                DomainState::from_source_with(source, filename, lib, |_| Vec::new())
            }
            LibSource::Registry {
                registry,
                save_path,
            } => DomainState::from_source_registry(source, filename, registry, save_path),
        },
        Err(error) => {
            let mut domain = DomainState::empty().with_lib_source(lib_source);
            domain.filename = filename;
            domain.doc = Err(format!("reading {}: {error}", absolute_path.display()));
            domain
        }
    };
    domain.source_path = Some(absolute_path.clone());
    let success = domain.doc.is_ok();
    LoadedDomain {
        domain,
        absolute_path,
        success,
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_delivers_each_message_then_reports_disconnect() {
        let mailbox = OpenMailbox::new();
        let tx = mailbox.begin_launch();
        tx.send(OpenMsg::Picked(None)).unwrap();
        tx.send(OpenMsg::Picked(Some(PathBuf::from("/tmp/latest.eut"))))
            .unwrap();
        assert_eq!(mailbox.poll(), OpenPoll::Message(OpenMsg::Picked(None)));
        assert_eq!(
            mailbox.poll(),
            OpenPoll::Message(OpenMsg::Picked(Some(PathBuf::from("/tmp/latest.eut"))))
        );
        drop(tx);
        assert_eq!(mailbox.poll(), OpenPoll::Disconnected);
    }
}
