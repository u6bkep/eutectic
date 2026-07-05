//! Canned [`EcadApp`] states for the headless review loop.
//!
//! Per `gui-architecture.md` ("Headless review loop"), GUI panels get the same
//! fixture-and-artifact review discipline the engine's fab outputs get: canned
//! scenes here, lint-clean assertions in the tests below, and SVG/tree/lint
//! artifacts dumped by the `review` binary (`src/bin/review.rs`).
//!
//! The three states are the ones milestone 1 can produce: no document, a
//! document loaded (from a tiny inline `.ecad` source), and a parse-error
//! state.

use crate::app::{DomainState, EcadApp};

/// A tiny but complete `.ecad` document: two parts, two nets, and a board
/// outline. With no `slab` directives the elaborator uses the built-in
/// two-layer stackup, so the stats card shows two copper layers and a
/// 20 x 15 mm board.
pub const SAMPLE_ECAD: &str = "\
inst reg LDO
inst C1 Cap

net VBUS reg.VOUT C1.p1
net GND reg.GND C1.p2

board (0mm, 0mm) (20mm, 0mm) (20mm, 15mm) (0mm, 15mm)
";

/// A source that parses structurally but references a part not in the library,
/// so elaboration reports a diagnostic — the parse/elaborate-error state.
pub const BROKEN_ECAD: &str = "\
inst U1 NotAPart
net GND U1.GND
";

/// The no-document state: nothing loaded.
pub fn no_document() -> EcadApp {
    EcadApp::new(DomainState::empty())
}

/// A document loaded from [`SAMPLE_ECAD`].
pub fn document_loaded() -> EcadApp {
    EcadApp::new(DomainState::from_source(
        SAMPLE_ECAD.to_string(),
        Some("sample.ecad".to_string()),
    ))
}

/// A parse/elaborate-error state loaded from [`BROKEN_ECAD`].
pub fn parse_error() -> EcadApp {
    EcadApp::new(DomainState::from_source(
        BROKEN_ECAD.to_string(),
        Some("broken.ecad".to_string()),
    ))
}

/// Every fixture, paired with a stable scene name for artifact filenames.
pub fn all() -> Vec<(&'static str, EcadApp)> {
    vec![
        ("no_document", no_document()),
        ("document_loaded", document_loaded()),
        ("parse_error", parse_error()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use damascene_core::prelude::*;

    /// Render one fixture through the headless bundle pipeline and assert the
    /// lint is clean. Mirrors the pattern in the damascene-core README
    /// ("Testing without a window").
    fn assert_lint_clean(name: &str, app: &EcadApp) {
        let theme = app.theme();
        let viewport = Rect::new(0.0, 0.0, 1280.0, 800.0);
        let cx = BuildCx::new(&theme).with_viewport(viewport.w, viewport.h);
        let mut root = app.build(&cx);
        let bundle = render_bundle_themed(&mut root, viewport, &theme);
        assert!(
            bundle.lint.findings.is_empty(),
            "fixture `{name}` has lint findings:\n{}",
            bundle.lint.text()
        );
    }

    #[test]
    fn no_document_is_lint_clean() {
        assert_lint_clean("no_document", &no_document());
    }

    #[test]
    fn document_loaded_is_lint_clean() {
        let app = document_loaded();
        // The sample must actually elaborate — otherwise the fixture is
        // silently exercising the error path instead of the loaded path.
        assert!(
            app.domain.doc.is_ok(),
            "sample.ecad failed to elaborate: {:?}",
            app.domain.doc.as_ref().err()
        );
        assert_lint_clean("document_loaded", &app);
    }

    #[test]
    fn parse_error_is_lint_clean() {
        let app = parse_error();
        // The broken source must actually fail — otherwise this fixture is not
        // exercising the error path.
        assert!(
            app.domain.doc.is_err(),
            "broken.ecad unexpectedly elaborated"
        );
        assert_lint_clean("parse_error", &app);
    }
}
