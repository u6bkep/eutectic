//! `ecad-gui` native entry point.
//!
//! Usage: `ecad-gui [PATH.ecad]`. With a path, the file is read, parsed, and
//! elaborated through `ecad-core`'s public API (`History` + `Command::LoadText`
//! — the same entry point the `ecad-core` examples use); a load failure is
//! surfaced in the UI rather than crashing (the permissive philosophy). With no
//! path, the window opens in the no-document state.
//!
//! # Live source loop (milestone 5)
//!
//! With a path, a background **file-watch thread** polls the file's mtime (~200 ms)
//! and, on a change, reads the source and sends it over the app's [`SourceMailbox`],
//! then wakes the host ([`HostConfig::with_external_wakeup`]). The app drains the
//! mailbox in `before_build` and re-elaborates — author in `$EDITOR`, the window
//! follows. The thread is spawned **only here** (the windowed path); the drain +
//! reload logic lives in `EcadApp` and is fully testable headlessly by injecting
//! messages onto the mailbox.
//!
//! The window itself is only opened here; the headless review loop
//! (`src/bin/review.rs` and the `fixtures` tests) is what proves the UI in CI.

use damascene_core::prelude::Rect;
use damascene_winit_wgpu::HostConfig;
use ecad_gui::{DomainState, EcadApp, LibSource, Registry, SourceMailbox};

/// The per-machine registry file location — computed **only here** (the
/// registry module itself takes its path as a parameter; tests inject scratch
/// paths and never touch the real config): `$XDG_CONFIG_HOME/ecad/libraries`,
/// falling back to `$HOME/.config/ecad/libraries`. `None` when neither env var
/// is set (registry edits then stay in-memory for the session).
fn default_registry_path() -> Option<std::path::PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(std::path::PathBuf::from(xdg).join("ecad/libraries"));
    }
    std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join(".config/ecad/libraries"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1);

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
            let source =
                std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
            let filename = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            // Resolve the doc's `use` names through the registry (real
            // libraries first, the built-in toy lib appended last).
            let LibSource::Registry {
                registry,
                save_path,
            } = lib_source
            else {
                unreachable!("lib_source is constructed as Registry above")
            };
            DomainState::from_source_registry(source, filename, registry, save_path)
        }
        None => DomainState::empty().with_lib_source(lib_source),
    };

    // The live-source mailbox: the app keeps the receiver; the sender goes to the
    // watch thread. With no file loaded, the app keeps the disconnected default.
    let (mailbox, tx) = SourceMailbox::new();
    let app = EcadApp::new(domain).with_mailbox(mailbox);

    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);

    // Only spawn the watcher when a file was actually loaded. The external-wakeup hook
    // runs once on the UI thread just before the loop starts; it hands the `Wakeup` to
    // the polling thread so a detected change schedules a frame.
    let config = match path {
        Some(path) => {
            let watch_path = std::path::PathBuf::from(path);
            HostConfig::default().with_external_wakeup(move |wakeup| {
                let tx = tx.clone();
                let watch_path = watch_path.clone();
                ecad_gui::reload::spawn_watcher(watch_path, tx, move || wakeup.wake());
            })
        }
        None => {
            // No file: drop the sender so the mailbox is inert (drains to nothing).
            drop(tx);
            HostConfig::default()
        }
    };

    damascene_winit_wgpu::run_with_config("ecad", viewport, app, config)
}
