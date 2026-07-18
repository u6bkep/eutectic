//! The live source loop: file-watch mailbox + reload semantics (milestone 5).
//!
//! Editing is source-first (`docs/gui-architecture.md`): the `.eut` file is the
//! source of truth, and the GUI *follows* external edits — author in `$EDITOR`, the
//! window re-elaborates. This module owns the two testable halves of that loop:
//!
//! 1. **The mailbox** ([`SourceMailbox`]): an `mpsc` receiver the app drains in
//!    `before_build`. Every change carries its watched path, so the app can reject a
//!    delayed event from a document that has since been replaced.
//!
//! 2. **The switchable zero-dep watcher** ([`spawn_switchable_watcher`]): the one
//!    host-only polling loop. `main.rs` gives it the startup path and in-app opens send
//!    replacement paths. It drains switches both before and after each sleep and tags
//!    every event with the path it read. `std` only — no `notify` dependency.
//!
//! # Reload semantics (stated exactly; see `reload_semantics` in the report)
//!
//! On a [`SourceMsg::Changed`] the app ([`crate::app::EutecticApp::apply_reload`]):
//!   - re-parses + re-elaborates the new source into a fresh [`DomainState`];
//!   - on **success**: swaps in the new doc, bumps the doc revision (so the canvas /
//!     schematic / explorer / findings caches all rebuild), and **preserves** the
//!     cameras (no re-fit — the user's framing is sacred), layer visibility, pane
//!     layout, and selection — **pruning** only the selected/hovered ids that no
//!     longer resolve in the new doc; recomputes findings; clears any stale error;
//!   - on **failure** (parse/elaborate error): **keeps the last-good doc rendered**
//!     (the canvas never blanks) and records the error string in a persistent slot the
//!     chrome renders as an unmissable banner until a good reload lands. Findings from
//!     the last-good doc are **retained** (they still describe what is on screen).
//!
//! The bump-once / preserve / prune / permissive-failure behaviours are all exercised
//! by the reload tests in `app.rs` via [`SourceMailbox::push`] + `before_build`.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

/// A message from the file-watch thread to the app. Only source *changes* flow today;
/// the enum leaves room for watch-lifecycle events (file removed / re-created) without
/// a signature churn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceMsg {
    /// The watched file's contents changed on disk; carry its path and new source text.
    Changed {
        /// The exact watcher target that produced this source. `None` is reserved for
        /// pathless headless fixtures.
        path: Option<PathBuf>,
        /// The complete new file contents.
        source: String,
    },
}

impl SourceMsg {
    /// Build a pathless change for a headless fixture whose current document also has
    /// no source path. Native watcher events always use [`SourceMsg::Changed`] directly
    /// with `Some(path)`.
    pub fn pathless(source: impl Into<String>) -> SourceMsg {
        SourceMsg::Changed {
            path: None,
            source: source.into(),
        }
    }
}

/// The app-side mailbox: an `mpsc` receiver drained once per frame in `before_build`.
/// Constructed with its paired [`Sender`] via [`SourceMailbox::new`]; the sender is
/// handed to the watch thread (host) or driven by hand (tests).
pub struct SourceMailbox {
    rx: Receiver<SourceMsg>,
    /// A retained clone of the sender, so tests can [`push`](Self::push) messages
    /// directly onto the app's own channel without wiring a separate sender. The
    /// windowed host ignores this and uses the [`Sender`] returned by [`new`].
    tx: Sender<SourceMsg>,
}

impl SourceMailbox {
    /// Build a mailbox and return it with a [`Sender`] the caller hands to the watch
    /// thread. The host keeps the sender alive for the thread; the app keeps the
    /// mailbox.
    pub fn new() -> (SourceMailbox, Sender<SourceMsg>) {
        let (tx, rx) = channel();
        (SourceMailbox { rx, tx: tx.clone() }, tx)
    }

    /// A disconnected mailbox for the no-watch path (no file loaded, or fixtures that
    /// don't reload). Draining it always yields nothing; [`push`](Self::push) still
    /// works for tests (its retained sender feeds its own receiver).
    pub fn disconnected() -> SourceMailbox {
        let (mb, _tx) = SourceMailbox::new();
        // Drop the returned external sender; the retained `tx` still feeds `rx`, so
        // `push` works but no external producer exists.
        mb
    }

    /// Push a message directly onto this mailbox — the headless test entry point. The
    /// next [`drain`](Self::drain) returns it, exactly as if the watch thread had sent
    /// it. Never blocks; the channel is unbounded.
    pub fn push(&self, msg: SourceMsg) {
        // The receiver is alive (we own it), so this only fails if `rx` was dropped —
        // impossible while `&self` is borrowed. Ignore the (unreachable) error.
        let _ = self.tx.send(msg);
    }

    /// Drain every pending message, returning the **latest** source change if any (a
    /// burst of edits coalesces to the last one — reloading intermediate states would
    /// be wasted work). Returns `None` when the mailbox is empty this frame.
    ///
    /// Coalescing to the last message is correct for source reloads: each `Changed`
    /// carries the *whole* file, so the newest supersedes all older ones. A future
    /// message kind that is not idempotent-by-latest would need per-kind handling.
    pub fn drain(&self) -> Option<SourceMsg> {
        let mut latest = None;
        // Drain every queued message; the last wins (coalescing). `try_recv` returns
        // `Empty`/`Disconnected` when nothing more is pending, ending the loop.
        while let Ok(msg) = self.rx.try_recv() {
            latest = Some(msg);
        }
        latest
    }
}

/// Spawn the zero-dependency file-watch thread (host-only). Its target starts at
/// `initial` and is replaced by paths received on `paths`. It polls the current target's
/// mtime every [`POLL_INTERVAL`]; on a change it reads the file and sends a path-tagged
/// [`SourceMsg::Changed`] over `source_tx`, then calls `wake()` to schedule a frame.
///
/// The thread runs until the channel is dropped (the app / host is gone), at which
/// point the `send` errors and the loop exits. Read failures (a transient
/// editor-swap-file dance, a missing file mid-rename) are skipped — the next poll that
/// sees a readable file with a newer mtime resends. Generic over `wake` so the host
/// passes a `move || wakeup.wake()` and tests need not run the thread at all.
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
            if !drain_switches(&paths, &mut current, &mut last_mtime) {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
            // A switch may arrive during the sleep. Apply it before observing mtime so
            // the old path can never be read after a replacement is already queued.
            if !drain_switches(&paths, &mut current, &mut last_mtime) {
                return;
            }
            let Some(path) = current.as_deref() else {
                continue;
            };
            let now = mtime(path);
            // Fire only on a *changed* mtime (Some→Some newer, or None→Some after a
            // transient disappearance). Equal mtimes (the common idle poll) do nothing.
            if now != last_mtime && now.is_some() {
                if let Ok(source) = std::fs::read_to_string(path) {
                    if source_tx
                        .send(SourceMsg::Changed {
                            path: Some(path.to_path_buf()),
                            source,
                        })
                        .is_err()
                    {
                        return; // app gone — stop polling.
                    }
                    wake();
                    // Advance the baseline only on a successful read: if the read
                    // raced an atomic rename-swap and failed, keeping the old
                    // baseline makes the next poll retry this same edit even when
                    // the settled mtime equals the one we just observed.
                    last_mtime = now;
                }
            } else if now != last_mtime {
                // File vanished (Some→None): update the baseline so its re-creation
                // (None→Some) is detected as a change, but send nothing yet.
                last_mtime = now;
            }
        }
    });
}

fn drain_switches(
    paths: &Receiver<PathBuf>,
    current: &mut Option<PathBuf>,
    last_mtime: &mut Option<std::time::SystemTime>,
) -> bool {
    loop {
        match paths.try_recv() {
            Ok(path) => {
                *last_mtime = mtime(&path);
                *current = Some(path);
            }
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// The mtime-poll cadence — ~200 ms, per the milestone spec. Long enough that the poll
/// is negligible, short enough that a save feels live.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// The file's last-modified time as an `Option` (`None` when it can't be stat'd — the
/// file is missing or permission-denied). Used only to detect *change*, never
/// interpreted as an absolute time.
fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pushed message is drained back out — the headless mailbox round-trip the
    /// reload tests rely on.
    #[test]
    fn push_then_drain_roundtrips() {
        let (mb, _tx) = SourceMailbox::new();
        assert_eq!(mb.drain(), None, "empty mailbox drains to nothing");
        let msg = SourceMsg::Changed {
            path: None,
            source: "hello".into(),
        };
        mb.push(msg.clone());
        assert_eq!(mb.drain(), Some(msg));
        assert_eq!(mb.drain(), None, "drained message is consumed once");
    }

    /// A burst of changes coalesces to the latest — reloading intermediate states
    /// would be wasted work, and the newest source supersedes all older ones.
    #[test]
    fn drain_coalesces_to_latest() {
        let (mb, _tx) = SourceMailbox::new();
        for source in ["v1", "v2", "v3"] {
            mb.push(SourceMsg::Changed {
                path: None,
                source: source.into(),
            });
        }
        assert_eq!(
            mb.drain(),
            Some(SourceMsg::Changed {
                path: None,
                source: "v3".into(),
            }),
            "a burst coalesces to the last message"
        );
        assert_eq!(mb.drain(), None);
    }

    /// The external sender feeds the same mailbox — proving the watch-thread path
    /// (host holds the returned `Sender`) delivers to the app's receiver.
    #[test]
    fn external_sender_delivers() {
        let (mb, tx) = SourceMailbox::new();
        let msg = SourceMsg::Changed {
            path: Some(PathBuf::from("/tmp/source.eut")),
            source: "from thread".into(),
        };
        tx.send(msg.clone()).unwrap();
        assert_eq!(mb.drain(), Some(msg));
    }
}
