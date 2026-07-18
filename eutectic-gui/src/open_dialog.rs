//! Native File ▸ Open plumbing: an injectable dialog launcher, a polled
//! mailbox, fresh-domain loading, and a switchable live-source watcher.

use crate::app::{DomainState, LibSource};
use crate::reload::{POLL_INTERVAL, SourceMsg};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

pub type WakeFn = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenMsg {
    Picked(Option<PathBuf>),
}

pub struct OpenMailbox {
    rx: Receiver<OpenMsg>,
    tx: Sender<OpenMsg>,
}

impl OpenMailbox {
    pub fn new() -> OpenMailbox {
        let (tx, rx) = channel();
        OpenMailbox { rx, tx }
    }

    pub fn sender(&self) -> Sender<OpenMsg> {
        self.tx.clone()
    }

    pub fn push(&self, msg: OpenMsg) {
        let _ = self.tx.send(msg);
    }

    pub fn drain(&self) -> Option<OpenMsg> {
        let mut latest = None;
        while let Ok(msg) = self.rx.try_recv() {
            latest = Some(msg);
        }
        latest
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

/// Spawn one watcher whose target can be replaced after an in-app open. The
/// returned sender is held by the app; dropping it stops the thread.
pub fn spawn_switchable_watcher<W>(
    initial: Option<PathBuf>,
    paths: Receiver<PathBuf>,
    source_tx: Sender<SourceMsg>,
    wake: W,
) where
    W: Fn() + Send + 'static,
{
    std::thread::spawn(move || {
        let mut current = initial;
        let mut last_mtime = current.as_deref().and_then(mtime);
        loop {
            loop {
                match paths.try_recv() {
                    Ok(path) => {
                        last_mtime = mtime(&path);
                        current = Some(path);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            std::thread::sleep(POLL_INTERVAL);
            let Some(path) = current.as_deref() else {
                continue;
            };
            let now = mtime(path);
            if now != last_mtime && now.is_some() {
                if let Ok(source) = std::fs::read_to_string(path) {
                    if source_tx.send(SourceMsg::Changed(source)).is_err() {
                        return;
                    }
                    wake();
                    last_mtime = now;
                }
            } else if now != last_mtime {
                last_mtime = now;
            }
        }
    });
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_is_injectable_and_coalesces() {
        let mailbox = OpenMailbox::new();
        mailbox.push(OpenMsg::Picked(None));
        mailbox.push(OpenMsg::Picked(Some(PathBuf::from("/tmp/latest.eut"))));
        assert_eq!(
            mailbox.drain(),
            Some(OpenMsg::Picked(Some(PathBuf::from("/tmp/latest.eut"))))
        );
        assert_eq!(mailbox.drain(), None);
    }
}
