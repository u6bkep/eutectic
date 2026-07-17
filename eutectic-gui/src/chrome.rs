//! Window chrome — the menu bar, the icon toolbar, and the status bar (the UI
//! oracle's three chrome regions). Originally moved out of `app/panels.rs` as
//! pure code motion (gui-module-split); the menu bar + icon-toolbar rewrite
//! adopts the oracle anatomy (`docs/ui-oracle`).

pub(crate) mod actions;
pub(crate) mod dialogs;
pub(crate) mod icons;
pub(crate) mod menubar;
pub(crate) mod status_bar;
pub(crate) mod toolbar;

/// Format a viewport zoom as a scale factor `×N` (two significant figures)
/// relative to the canvas's natural scale — one asset viewBox unit is 1 mm, so at
/// viewport zoom `1.0` one board millimetre is one logical pixel (`canvas.rs`
/// module docs). This replaces the meaningless "percent" readout: `×1.0` is the
/// 1 mm : 1 px natural framing, `×12` is twelve pixels per millimetre, `×0.35` is
/// zoomed out. Used by the status bar (the per-pane canvas chip is another
/// slice's job — hence `pub(crate)`).
pub(crate) fn zoom_scale_label(zoom: f32) -> String {
    format!("×{}", two_sig_figs(zoom))
}

/// Format a positive number to two significant figures, choosing the decimal
/// count from its magnitude: `1.0` → `"1.0"`, `2.5` → `"2.5"`, `12.0` → `"12"`,
/// `0.35` → `"0.35"`, `0.035` → `"0.035"`. Non-finite / non-positive inputs
/// (a viewport should never produce them) render as `"0"`.
fn two_sig_figs(v: f32) -> String {
    if !v.is_finite() || v <= 0.0 {
        return "0".to_string();
    }
    let exp = v.abs().log10().floor() as i32;
    // Two sig figs ⇒ one digit after the leading one; clamp so we never ask for a
    // negative precision (large zooms print as integers).
    let decimals = (1 - exp).max(0) as usize;
    format!("{v:.decimals$}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The zoom label reads as a `×N` scale to two significant figures across the
    /// viewport's zoom range (min_zoom 0.02 … max_zoom 64), and `×1.0` is the
    /// natural 1 mm : 1 px framing — never the old meaningless percentage.
    #[test]
    fn zoom_scale_label_is_two_sig_fig_x_notation() {
        assert_eq!(zoom_scale_label(1.0), "×1.0");
        assert_eq!(zoom_scale_label(2.5), "×2.5");
        assert_eq!(zoom_scale_label(12.0), "×12");
        assert_eq!(zoom_scale_label(0.35), "×0.35");
        assert_eq!(zoom_scale_label(0.035), "×0.035");
        // Degenerate inputs never panic or print NaN.
        assert_eq!(zoom_scale_label(0.0), "×0");
        assert_eq!(zoom_scale_label(f32::NAN), "×0");
    }
}
