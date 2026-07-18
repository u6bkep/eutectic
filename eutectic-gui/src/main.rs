//! `eutectic-gui` native entry point.
//!
//! Usage: `eutectic-gui [PATH.eut]`. With a path, the file is read, parsed, and
//! elaborated through `eutectic-core`'s public API (`History` + `Command::LoadText`
//! — the same entry point the `eutectic-core` examples use); a load failure is
//! surfaced in the UI rather than crashing (the permissive philosophy). With no
//! path, the app opens the repository's `examples/showcase.eut` (overridable with
//! `$EUTECTIC_SHOWCASE`); an installed binary that cannot find the example falls
//! back to the no-document state with an explanatory status note. The environment
//! override is an explicit request, like a CLI path, so a missing override fails with
//! the file read error rather than silently falling back.
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
use eutectic_gui::open_dialog::{NativeOpenDialog, WakeFn, load_domain};
use eutectic_gui::recents::RecentFiles;
use eutectic_gui::{DomainState, EutecticApp, LibSource, Registry, SourceMailbox};
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq, Eq)]
enum StartupDecision {
    OpenPath(std::path::PathBuf),
    FallbackNote(String),
}

fn default_showcase_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../examples/showcase.eut")
}

/// Resolve the complete startup branch headlessly. An explicit CLI path always wins;
/// otherwise `$EUTECTIC_SHOWCASE` wins when set (including to a relative path). Both are
/// explicit requests and therefore proceed to the fail-fast read path even when missing.
/// Only an absent repository default degrades to the explanatory no-document state.
fn startup_decision(
    cli_path: Option<std::ffi::OsString>,
    showcase_override: Option<std::ffi::OsString>,
    default_showcase: std::path::PathBuf,
) -> StartupDecision {
    let (path, explicit) = if let Some(path) = cli_path {
        (std::path::PathBuf::from(path), true)
    } else if let Some(path) = showcase_override {
        (std::path::PathBuf::from(path), true)
    } else {
        (default_showcase, false)
    };

    if explicit || path.is_file() {
        StartupDecision::OpenPath(path)
    } else {
        StartupDecision::FallbackNote(format!(
            "no document: showcase not found at {}; pass a .eut path or set EUTECTIC_SHOWCASE",
            path.display()
        ))
    }
}

fn fallback_domain(note: String, lib_source: LibSource) -> DomainState {
    let mut domain = DomainState::empty().with_lib_source(lib_source);
    domain.doc = Err(note);
    domain
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

/// Recent documents use their own injectable-format file; only this native
/// boundary chooses the real XDG location.
fn default_recents_path() -> Option<std::path::PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg).join("eutectic/recent"));
    }
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".config/eutectic/recent"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let startup = startup_decision(
        std::env::args_os().nth(1),
        std::env::var_os("EUTECTIC_SHOWCASE"),
        default_showcase_path(),
    );

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

    let (path, domain, opened_successfully) = match startup {
        StartupDecision::OpenPath(path) => {
            let loaded = load_domain(&path, lib_source);
            (Some(loaded.absolute_path), loaded.domain, loaded.success)
        }
        StartupDecision::FallbackNote(note) => (None, fallback_domain(note, lib_source), false),
    };

    let recents_path = default_recents_path();
    let mut recents = match recents_path.as_deref() {
        Some(path) => RecentFiles::load(path).unwrap_or_else(|error| {
            eprintln!("warning: recent documents ignored: {error}");
            RecentFiles::new()
        }),
        None => RecentFiles::new(),
    };
    if opened_successfully && let Some(opened) = path.clone() {
        recents.push(opened);
        if let Some(save_path) = recents_path.as_deref()
            && let Err(error) = recents.save(save_path)
        {
            eprintln!("warning: recent documents not saved: {error}");
        }
    }

    // The live-source mailbox: the app keeps the receiver; the sender goes to the
    // watch thread. With no file loaded, the app keeps the disconnected default.
    let (mailbox, tx) = SourceMailbox::new();
    let (watch_path_tx, watch_path_rx) = std::sync::mpsc::channel();
    let wake_slot = Arc::new(Mutex::new(None));
    let wake: WakeFn = {
        let wake_slot = wake_slot.clone();
        Arc::new(move || {
            if let Some(wakeup) = wake_slot.lock().expect("wakeup slot poisoned").as_ref() {
                eutectic_gui::host::Wakeup::wake(wakeup);
            }
        })
    };
    let app = EutecticApp::new(domain)
        .with_mailbox(mailbox)
        .with_recents(recents, recents_path)
        .with_open_services(Arc::new(NativeOpenDialog), wake, watch_path_tx);

    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);

    // The external-wakeup hook runs once on the UI thread just before the loop
    // starts. The watcher begins with the startup path (if any) and switches
    // targets after each in-app open.
    let watch_rx = Arc::new(Mutex::new(Some(watch_path_rx)));
    let config = HostConfig::default().with_external_wakeup(move |wakeup| {
        *wake_slot.lock().expect("wakeup slot poisoned") = Some(wakeup.clone());
        let Some(rx) = watch_rx.lock().expect("watch receiver poisoned").take() else {
            return;
        };
        let tx = tx.clone();
        let initial = path.clone();
        eutectic_gui::open_dialog::spawn_switchable_watcher(initial, rx, tx, move || {
            wakeup.wake();
        });
    });

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
    fn cli_path_wins() {
        let cli_path = std::ffi::OsString::from("explicit.eut");
        let override_path = std::ffi::OsString::from("override.eut");
        assert_eq!(
            startup_decision(
                Some(cli_path.clone()),
                Some(override_path),
                std::path::PathBuf::from("default.eut"),
            ),
            StartupDecision::OpenPath(std::path::PathBuf::from(cli_path))
        );
    }

    #[test]
    fn environment_override_is_an_explicit_open_request() {
        let override_path = std::ffi::OsString::from("missing-override.eut");
        assert_eq!(
            startup_decision(
                None,
                Some(override_path.clone()),
                std::path::PathBuf::from("default.eut"),
            ),
            StartupDecision::OpenPath(std::path::PathBuf::from(override_path))
        );
    }

    #[test]
    fn present_workspace_showcase_opens() {
        let path = default_showcase_path();
        assert!(
            path.is_file(),
            "the default showcase ships in the workspace"
        );
        assert_eq!(
            startup_decision(None, None, path.clone()),
            StartupDecision::OpenPath(path)
        );
    }

    #[test]
    fn missing_workspace_showcase_wires_the_path_note_into_the_domain() {
        let path = std::env::temp_dir().join(format!(
            "eutectic-missing-showcase-{}-{}.eut",
            std::process::id(),
            line!()
        ));
        assert!(!path.is_file(), "test path must remain absent");
        let StartupDecision::FallbackNote(note) = startup_decision(None, None, path.clone()) else {
            panic!("missing implicit showcase should fall back")
        };
        assert!(note.contains(&path.display().to_string()));

        let domain = fallback_domain(
            note.clone(),
            LibSource::Fixed(eutectic_core::part::part_library()),
        );
        assert_eq!(domain.doc.as_ref().expect_err("fallback has no doc"), &note);
    }
}
