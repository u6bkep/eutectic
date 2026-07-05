//! `ecad-gui` native entry point (milestone 1 skeleton).
//!
//! Usage: `ecad-gui [PATH.ecad]`. With a path, the file is read, parsed, and
//! elaborated through `ecad-core`'s public API (`History` + `Command::LoadText`
//! — the same entry point the `ecad-core` examples use); a load failure is
//! surfaced in the UI rather than crashing (the permissive philosophy). With no
//! path, the window opens in the no-document state.
//!
//! The window itself is only opened here; the headless review loop
//! (`src/bin/review.rs` and the `fixtures` tests) is what proves the UI in CI.

use damascene_core::prelude::Rect;
use ecad_gui::{DomainState, EcadApp};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let domain = match std::env::args().nth(1) {
        Some(path) => {
            let source =
                std::fs::read_to_string(&path).map_err(|e| format!("reading {path}: {e}"))?;
            let filename = std::path::Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            DomainState::from_source(source, filename)
        }
        None => DomainState::empty(),
    };

    let app = EcadApp::new(domain);
    let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
    damascene_winit_wgpu::run("ecad", viewport, app)
}
