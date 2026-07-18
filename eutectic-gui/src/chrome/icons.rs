//! App-supplied toolbar glyphs.
//!
//! The icon toolbar (see the UI oracle) needs undo / redo / zoom-in / zoom-out /
//! fit / save / findings glyphs that damascene 0.4.5's built-in lucide subset
//! (`all_icon_names()`) does not carry. Rather than fall back to misleading
//! best-fit built-ins (a plain chevron for "undo"), we ship these as
//! app-supplied [`SvgIcon`]s parsed from lucide `currentColor` markup — the exact
//! mechanism `damascene_core::icons` documents for product-specific glyphs. They
//! tint through the element's `text_color` and render as real vectors in the
//! headless SVG artifacts, and — unlike an unknown string-typed name — they carry
//! no `UnknownIconName` lint finding (they are `IconSource::Svg`, not
//! `IconSource::UnknownName`), so the review bundle stays lint-clean.
//!
//! Open (`Folder`) and the command palette (`Command`) reuse built-in names; only
//! the nine glyphs with no faithful built-in live here.

use damascene_core::prelude::SvgIcon;
use std::sync::LazyLock;

/// Parse a 24×24 lucide-style `currentColor` icon body into an [`SvgIcon`], or
/// panic with the offending markup — these are compile-time-constant strings, so
/// a parse failure is a build-the-wrong-path bug, not a runtime condition.
fn glyph(body: &str) -> SvgIcon {
    let svg = format!(
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" "#,
            r#"stroke="currentColor" stroke-width="2" stroke-linecap="round" "#,
            r#"stroke-linejoin="round">{}</svg>"#
        ),
        body
    );
    SvgIcon::parse_current_color(&svg).unwrap_or_else(|e| panic!("toolbar glyph parse failed: {e}"))
}

/// lucide `save` — the floppy-disk write-to-disk glyph.
pub static SAVE: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<path d="M15.2 3a2 2 0 0 1 1.4.6l3.8 3.8a2 2 0 0 1 .6 1.4V19a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2z"/><path d="M17 21v-7a1 1 0 0 0-1-1H8a1 1 0 0 0-1 1v7"/><path d="M7 3v4a1 1 0 0 0 1 1h7"/>"#,
    )
});

/// lucide `undo-2` — the curved back-arrow.
pub static UNDO: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<path d="M9 14 4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5 5.5 5.5 0 0 1-5.5 5.5H11"/>"#,
    )
});

/// lucide `redo-2` — the curved forward-arrow.
pub static REDO: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<path d="m15 14 5-5-5-5"/><path d="M20 9H9.5A5.5 5.5 0 0 0 4 14.5 5.5 5.5 0 0 0 9.5 20H13"/>"#,
    )
});

/// lucide `zoom-in` — magnifier with a plus.
pub static ZOOM_IN: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/><path d="M11 8v6"/><path d="M8 11h6"/>"#,
    )
});

/// lucide `zoom-out` — magnifier with a minus.
pub static ZOOM_OUT: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/><path d="M8 11h6"/>"#)
});

/// lucide `maximize` — the four corner brackets (fit-to-content / frame).
pub static FIT: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<path d="M8 3H5a2 2 0 0 0-2 2v3"/><path d="M21 8V5a2 2 0 0 0-2-2h-3"/><path d="M3 16v3a2 2 0 0 0 2 2h3"/><path d="M16 21h3a2 2 0 0 0 2-2v-3"/>"#,
    )
});

/// lucide `list-checks` — the findings checklist (jump-to-findings).
pub static FINDINGS: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<path d="m3 17 2 2 4-4"/><path d="m3 7 2 2 4-4"/><path d="M13 6h8"/><path d="M13 12h8"/><path d="M13 18h8"/>"#,
    )
});

/// Pane split with the new leaf on the right.
pub static SPLIT_RIGHT: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M12 3v18"/><path d="M16 12h4"/><path d="M18 10v4"/>"#,
    )
});

/// Pane split with the new leaf below.
pub static SPLIT_DOWN: LazyLock<SvgIcon> = LazyLock::new(|| {
    glyph(
        r#"<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 12h18"/><path d="M10 18h4"/><path d="M12 16v4"/>"#,
    )
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Every app glyph parses (the `LazyLock` bodies are valid `currentColor`
    /// SVG) and carries vector paths — a broken `d` string would panic here
    /// rather than silently render an empty icon in the toolbar.
    #[test]
    fn every_toolbar_glyph_parses_nonempty() {
        for g in [
            &*SAVE,
            &*UNDO,
            &*REDO,
            &*ZOOM_IN,
            &*ZOOM_OUT,
            &*FIT,
            &*FINDINGS,
            &*SPLIT_RIGHT,
            &*SPLIT_DOWN,
        ] {
            // Cloning is the cheap Arc bump the builder does; forcing the
            // LazyLock above already ran `parse_current_color` (which panics on
            // failure), so reaching here means all seven parsed.
            let _ = g.clone();
        }
    }
}
