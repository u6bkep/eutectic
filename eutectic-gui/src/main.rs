//! `eutectic-gui` native entry point.
//!
//! Usage: `eutectic-gui [PATH.eut]`. With a path, the file is read, parsed, and
//! elaborated through `eutectic-core`'s public API (`History` + `Command::LoadText`
//! — the same entry point the `eutectic-core` examples use); a load failure is
//! surfaced in the UI rather than crashing (the permissive philosophy). With no
//! path, the app opens the repository's `examples/showcase.eut` (overridable with
//! `$EUTECTIC_SHOWCASE`); an installed binary that cannot find the example falls
//! back to the no-document state with an explanatory status note.
//!
//! # Live source loop (milestone 5)
//!
//! With a path, a background **file-watch thread** polls the file's mtime (~200 ms)
//! and, on a change, reads the source and sends it over the app's [`SourceMailbox`],
//! then wakes the host ([`HostConfig::with_external_wakeup`]). The app drains the
//! mailbox in `before_build` and re-elaborates — author in `$EDITOR`, the window
//! follows. The thread is spawned **only here** (the windowed path); the drain +
//! reload logic lives in `EutecticApp` and is fully testable headlessly by injecting
//! messages onto the mailbox.
//!
//! The window itself is only opened here; the headless review loop
//! (`src/bin/review.rs` and the `fixtures` tests) is what proves the UI in CI.

use damascene_core::prelude::Rect;
use eutectic_gui::host::HostConfig;
use eutectic_gui::{DomainState, EutecticApp, LibSource, Registry, SourceMailbox};

/// Resolve the startup request without touching the filesystem. An explicit CLI path
/// always wins. Otherwise `$EUTECTIC_SHOWCASE` wins when it is set (including to a
/// relative path), with the repository example relative to this crate as the default.
/// The bool records whether the path was an explicit CLI request: a missing explicit
/// path keeps the existing fail-fast read error, while a missing dev-tool default
/// degrades to the empty-document UI.
fn requested_path(
    cli_path: Option<std::ffi::OsString>,
    showcase_override: Option<std::ffi::OsString>,
) -> (std::path::PathBuf, bool) {
    if let Some(path) = cli_path {
        return (std::path::PathBuf::from(path), true);
    }
    let path = showcase_override.map_or_else(
        || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/showcase.eut"),
        std::path::PathBuf::from,
    );
    (path, false)
}

/// The per-machine registry file location — computed **only here** (the
/// registry module itself takes its path as a parameter; tests inject scratch
/// paths and never touch the real config): `$XDG_CONFIG_HOME/eutectic/libraries`,
/// falling back to `$HOME/.config/eutectic/libraries`. `None` when neither env var
/// is set (registry edits then stay in-memory for the session).
fn default_registry_path() -> Option<std::path::PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg).join("eutectic/libraries"));
    }
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".config/eutectic/libraries"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (requested_path, explicit_path) = requested_path(
        std::env::args_os().nth(1),
        std::env::var_os("EUTECTIC_SHOWCASE"),
    );
    let (path, startup_note) = if explicit_path || requested_path.is_file() {
        (Some(requested_path), None)
    } else {
        let note = format!(
            "no document: showcase not found at {}; pass a .eut path or set EUTECTIC_SHOWCASE",
            requested_path.display()
        );
        (None, Some(note))
    };

    // The per-machine library registry (library packages, slice 2). A missing
    // file is the empty first-run registry; a malformed one degrades to empty
    // with a stderr warning (the app must still open — the Libraries menu is
    // how you fix it). The broken file is set aside as `libraries.bak` first,
    // so a later menu edit rewrites the live path without destroying the
    // hand-edited original.
    let registry_path = default_registry_path();
    let registry = match &registry_path {
        Some(p) => Registry::load(p).unwrap_or_else(|e| {
            let bak = p.with_extension("bak");
            match std::fs::rename(p, &bak) {
                Ok(()) => eprintln!(
                    "warning: broken library registry ignored ({e}); \
                     original preserved at {}",
                    bak.display()
                ),
                Err(re) => eprintln!(
                    "warning: broken library registry ignored ({e}); \
                     could not set it aside ({re}) — a menu edit will overwrite it"
                ),
            }
            Registry::new()
        }),
        None => Registry::new(),
    };
    let lib_source = LibSource::Registry {
        registry,
        save_path: registry_path,
    };

    let domain = match &path {
        Some(path) => {
            let source = std::fs::read_to_string(path)
                .map_err(|e| format!("reading {}: {e}", path.display()))?;
            let filename = path.file_name().map(|n| n.to_string_lossy().into_owned());
            // Resolve the doc's `use` names through the registry (real
            // libraries first, the built-in toy lib appended last).
            let LibSource::Registry {
                registry,
                save_path,
            } = lib_source
            else {
                unreachable!("lib_source is constructed as Registry above")
            };
            let mut domain =
                DomainState::from_source_registry(source, filename, registry, save_path);
            // The explicit-save target (m6): the loaded file itself. Only the
            // windowed path sets this — fixtures have no save affordance.
            domain.source_path = Some(path.clone());
            domain
        }
        None => {
            let mut domain = DomainState::empty().with_lib_source(lib_source);
            if let Some(note) = startup_note {
                domain.doc = Err(note);
            }
            domain
        }
    };

    // The live-source mailbox: the app keeps the receiver; the sender goes to the
    // watch thread. With no file loaded, the app keeps the disconnected default.
    let (mailbox, tx) = SourceMailbox::new();
    let app = EutecticApp::new(domain).with_mailbox(mailbox);

    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);

    // Only spawn the watcher when a file was actually loaded. The external-wakeup hook
    // runs once on the UI thread just before the loop starts; it hands the `Wakeup` to
    // the polling thread so a detected change schedules a frame.
    let config = match path {
        Some(path) => {
            let watch_path = path;
            HostConfig::default().with_external_wakeup(move |wakeup| {
                let tx = tx.clone();
                let watch_path = watch_path.clone();
                eutectic_gui::reload::spawn_watcher(watch_path, tx, move || wakeup.wake());
            })
        }
        None => {
            // No file: drop the sender so the mailbox is inert (drains to nothing).
            drop(tx);
            HostConfig::default()
        }
    };

    // Run through the WinitWgpuApp path (not the plain-App `run_with_config`
    // wrapper): `EutecticApp` implements the host's GPU seams — `gpu_setup`
    // (owned board-pane textures on the runner's device), `before_paint`
    // (per-frame pane renders behind the damage contract), and
    // `raw_window_event` (free hover / crosshair / middle-drag pan).
    eutectic_gui::host::run_host_app_with_config("eutectic", viewport, app, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arg_path_defaults_to_the_workspace_showcase() {
        let (path, explicit) = requested_path(None, None);
        assert!(!explicit);
        assert_eq!(
            path,
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/showcase.eut")
        );
        assert!(
            path.is_file(),
            "the default showcase ships in the workspace"
        );
    }

    #[test]
    fn cli_then_environment_override_have_the_documented_priority() {
        let override_path = std::ffi::OsString::from("override.eut");
        let cli_path = std::ffi::OsString::from("explicit.eut");

        assert_eq!(
            requested_path(None, Some(override_path.clone())),
            (std::path::PathBuf::from(&override_path), false)
        );
        assert_eq!(
            requested_path(Some(cli_path.clone()), Some(override_path)),
            (std::path::PathBuf::from(cli_path), true)
        );
    }
}
