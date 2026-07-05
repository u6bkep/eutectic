//! review — dump the headless bundle artifacts for every `ecad-gui` fixture.
//!
//! Modeled on `damascene-core`'s `review` example and `dump_showcase_bundles`
//! tool (see the damascene README, "Per-app artifact dumps"): for each fixture
//! it renders through the two-frame [`harness`](ecad_gui::harness) at a fixed
//! viewport — so the dumped canvases show the *fitted* camera the user sees, not
//! the pre-fit reset framing — writes the
//! `{svg,tree.txt,draw_ops.txt,lint.txt,shader_manifest.txt}` artifacts to
//! `ecad-gui/out/` (gitignored), prints where, and exits non-zero if any
//! fixture has lint findings — so the same `main` works as a CI gate.
//!
//! Run with `cargo run -p ecad-gui --bin review`.

use damascene_core::prelude::*;
use ecad_gui::{fixtures, harness};

fn main() -> std::io::Result<()> {
    let out_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");
    let mut had_findings = false;

    for (name, mut app) in fixtures::all() {
        let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
        let bundle = harness::render_settled(&mut app, viewport).bundle;

        let written = write_bundle(&bundle, &out_dir, name)?;
        for p in &written {
            println!("wrote {}", p.display());
        }

        if bundle.lint.findings.is_empty() {
            println!("  {name}: lint clean");
        } else {
            had_findings = true;
            eprintln!("  {name}: {} lint finding(s):", bundle.lint.findings.len());
            eprint!("{}", bundle.lint.text());
        }
    }

    if had_findings {
        std::process::exit(1);
    }
    Ok(())
}
