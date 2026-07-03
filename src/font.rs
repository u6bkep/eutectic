//! A built-in, zero-dependency **stroke font** — the first slice of the text
//! subsystem (docs/geometry-model-convergence.md, Decision 9).
//!
//! Glyphs are **centreline polylines**, not filled outlines: each glyph is a list of
//! strokes, and each stroke is a polyline of points on a normalized cell. The
//! lowering ([`crate::elaborate::features`]) scales the cell to world units, traces
//! each polyline at a pen width (`Shape2D::trace`), and emits the result as
//! [`Role::Marking`](crate::geom::Role::Marking) features. The strokes are therefore
//! **derived** geometry — the authoritative form is the string + placement + height +
//! layer carried by [`GenDirective::Text`](crate::elaborate::GenDirective::Text).
//!
//! # Cell coordinate system
//!
//! Points are `(x, y)` integer **font-units** on a cell that is [`GLYPH_ADVANCE`] wide
//! and [`CELL_HEIGHT`] tall. The baseline is `y = 0`, the cap height is `y = 6`, and
//! the visual midline is `y = 3`. Glyphs draw in columns `x ∈ [0, 4]`; the extra
//! width up to [`GLYPH_ADVANCE`] is inter-glyph spacing. `y` increases **upward**
//! (ECAD convention), matching the rest of the geometry model.
//!
//! # Coverage
//!
//! Uppercase `A`–`Z`, digits `0`–`9`, space, `.`, `-`, `:`, `/`, plus `Ω` and `µ` (the
//! two symbols component labels reach for). These are simple utilitarian block glyphs —
//! legible, not beautiful. **Lowercase** letters have no dedicated glyphs yet, so they
//! **case-fold** to the uppercase form; a genuinely **unknown** character renders a
//! fallback box outline so it is visibly wrong rather than silently dropped; a space
//! renders nothing.
//!
//! # Deliberate follow-ups (NOT in this slice)
//!
//! - true lowercase glyphs (today they case-fold) + the rest of printable ASCII,
//! - importing a real Hershey vector font (far larger, properly kerned coverage),
//! - outline / TTF fonts (filled glyphs — a different lowering than this stroke path).

/// The height of the glyph cell in font-units. A glyph drawn at world `height` scales
/// the cell by `height / CELL_HEIGHT`, so the cap height (`y = 6`) lands a touch below
/// the nominal `height`, leaving a unit of leading.
pub const CELL_HEIGHT: i32 = 7;

/// The horizontal advance per glyph in font-units (glyph body `≈4` wide + spacing).
/// The lowering steps the pen by `GLYPH_ADVANCE * height / CELL_HEIGHT` per character.
pub const GLYPH_ADVANCE: i32 = 6;

/// A glyph = a list of polyline strokes; each stroke = a list of cell-space points.
type Glyph = &'static [&'static [(i32, i32)]];

use crate::doc::{Nm, Point};

/// Outline-font entry points (Decision 17), re-exported so the public surface reads
/// `font::text_regions` / `font::TtfFont` beside [`text_strokes`] even though the
/// `ttf-parser`-backed implementation lives in [`crate::ttf`] (keeping this module
/// dependency-free in spirit).
pub use crate::ttf::{TtfFont, text_regions};

/// Horizontal placement of a run of text relative to its anchor origin (Decision 14).
///
/// - [`Justify::Left`] — the anchor is the **baseline-left** corner: the first glyph's
///   left edge sits at local `x = 0` and the baseline at local `y = 0`. This is how
///   board `text` directives lower (their authored `at` *is* the origin).
/// - [`Justify::Center`] — the anchor is the run's **centre**: the run's actual **ink
///   extent** (the bounding box of the rendered strokes, not the advance box — a trailing
///   inter-glyph gap would bias the run off the anchor) is centred on the local origin.
///   This matches KiCad, which anchors footprint text at its centre; the content is live
///   (a refdes/label re-renders), so the centring offset cannot be baked at import and is
///   applied here per lowering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Justify {
    Left,
    Center,
}

/// Lower `string` to stroke polylines in a **local** frame at world `height`, honouring
/// `justify`. Each returned `Vec<Point>` is one centreline polyline (a single-point one
/// is a dot — [`Shape2D::trace`](crate::geom::Shape2D::trace) of one point is a disc);
/// the caller traces each at its pen width and places it (rotate + translate, then — for
/// footprint text — through `to_world`). This is the one place the per-glyph cell→world
/// scale and advance live, shared by board text ([`Justify::Left`], unchanged) and
/// footprint auto-text ([`Justify::Center`]).
///
/// Cell→world is the integer scale `p * height / CELL_HEIGHT`; glyphs advance `+x` by
/// `GLYPH_ADVANCE` font-units per character. For [`Justify::Center`] the whole run is then
/// shifted so its **ink bounding box** is centred on the origin (both axes). Tracing adds
/// the pen radius symmetrically, so the traced-feature centre coincides with the origin.
pub fn text_strokes(string: &str, height: Nm, justify: Justify) -> Vec<Vec<Point>> {
    let cell_h = CELL_HEIGHT as Nm;
    let mut out: Vec<Vec<Point>> = Vec::new();
    for (i, ch) in string.chars().enumerate() {
        let dx = i as i32 * GLYPH_ADVANCE;
        for stroke in glyph_strokes(ch) {
            let pts: Vec<Point> = stroke
                .iter()
                .map(|&(cx, cy)| Point {
                    x: (dx + cx) as Nm * height / cell_h,
                    y: cy as Nm * height / cell_h,
                })
                .collect();
            out.push(pts);
        }
    }
    if justify == Justify::Center
        && let Some((lo, hi)) = ink_bbox(&out)
    {
        let (ox, oy) = ((lo.x + hi.x) / 2, (lo.y + hi.y) / 2);
        for stroke in &mut out {
            for p in stroke {
                p.x -= ox;
                p.y -= oy;
            }
        }
    }
    out
}

/// The bounding box of every point across `strokes` (their centreline ink extent), or
/// `None` if there are no points (empty/all-space run).
fn ink_bbox(strokes: &[Vec<Point>]) -> Option<(Point, Point)> {
    let mut pts = strokes.iter().flatten();
    let first = *pts.next()?;
    let (mut lo, mut hi) = (first, first);
    for p in pts {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    Some((lo, hi))
}

// ---- uppercase A–Z ----------------------------------------------------------
const A: Glyph = &[&[(0, 0), (2, 6), (4, 0)], &[(1, 3), (3, 3)]];
const B: Glyph = &[
    &[(0, 0), (0, 6), (3, 6), (4, 5), (4, 4), (3, 3), (0, 3)],
    &[(3, 3), (4, 2), (4, 1), (3, 0), (0, 0)],
];
const C: Glyph = &[&[
    (4, 5),
    (3, 6),
    (1, 6),
    (0, 5),
    (0, 1),
    (1, 0),
    (3, 0),
    (4, 1),
]];
const D: Glyph = &[&[(0, 0), (0, 6), (2, 6), (4, 4), (4, 2), (2, 0), (0, 0)]];
const E: Glyph = &[&[(4, 6), (0, 6), (0, 0), (4, 0)], &[(0, 3), (3, 3)]];
const F: Glyph = &[&[(4, 6), (0, 6), (0, 0)], &[(0, 3), (3, 3)]];
const G: Glyph = &[&[
    (4, 5),
    (3, 6),
    (1, 6),
    (0, 5),
    (0, 1),
    (1, 0),
    (3, 0),
    (4, 1),
    (4, 3),
    (2, 3),
]];
const H: Glyph = &[&[(0, 0), (0, 6)], &[(4, 0), (4, 6)], &[(0, 3), (4, 3)]];
const I: Glyph = &[&[(0, 6), (4, 6)], &[(2, 6), (2, 0)], &[(0, 0), (4, 0)]];
const J: Glyph = &[&[(4, 6), (4, 1), (3, 0), (1, 0), (0, 1)]];
const K: Glyph = &[&[(0, 0), (0, 6)], &[(4, 6), (0, 3), (4, 0)]];
const L: Glyph = &[&[(0, 6), (0, 0), (4, 0)]];
const M: Glyph = &[&[(0, 0), (0, 6), (2, 3), (4, 6), (4, 0)]];
const N: Glyph = &[&[(0, 0), (0, 6), (4, 0), (4, 6)]];
const O: Glyph = &[&[
    (1, 0),
    (0, 1),
    (0, 5),
    (1, 6),
    (3, 6),
    (4, 5),
    (4, 1),
    (3, 0),
    (1, 0),
]];
const P: Glyph = &[&[(0, 0), (0, 6), (3, 6), (4, 5), (4, 4), (3, 3), (0, 3)]];
const Q: Glyph = &[
    &[
        (1, 0),
        (0, 1),
        (0, 5),
        (1, 6),
        (3, 6),
        (4, 5),
        (4, 1),
        (3, 0),
        (1, 0),
    ],
    &[(2, 2), (4, 0)],
];
const R: Glyph = &[
    &[(0, 0), (0, 6), (3, 6), (4, 5), (4, 4), (3, 3), (0, 3)],
    &[(2, 3), (4, 0)],
];
const S: Glyph = &[&[
    (4, 5),
    (3, 6),
    (1, 6),
    (0, 5),
    (0, 4),
    (1, 3),
    (3, 3),
    (4, 2),
    (4, 1),
    (3, 0),
    (1, 0),
    (0, 1),
]];
const T: Glyph = &[&[(0, 6), (4, 6)], &[(2, 6), (2, 0)]];
const U: Glyph = &[&[(0, 6), (0, 1), (1, 0), (3, 0), (4, 1), (4, 6)]];
const V: Glyph = &[&[(0, 6), (2, 0), (4, 6)]];
const W: Glyph = &[&[(0, 6), (1, 0), (2, 3), (3, 0), (4, 6)]];
const X: Glyph = &[&[(0, 0), (4, 6)], &[(0, 6), (4, 0)]];
const Y: Glyph = &[&[(0, 6), (2, 3), (4, 6)], &[(2, 3), (2, 0)]];
const Z: Glyph = &[&[(0, 6), (4, 6), (0, 0), (4, 0)]];

// ---- digits 0–9 -------------------------------------------------------------
const D0: Glyph = &[
    &[
        (1, 0),
        (0, 1),
        (0, 5),
        (1, 6),
        (3, 6),
        (4, 5),
        (4, 1),
        (3, 0),
        (1, 0),
    ],
    &[(1, 1), (3, 5)],
];
const D1: Glyph = &[&[(1, 5), (2, 6), (2, 0)], &[(1, 0), (3, 0)]];
const D2: Glyph = &[&[(0, 5), (1, 6), (3, 6), (4, 5), (4, 4), (0, 0), (4, 0)]];
const D3: Glyph = &[&[
    (0, 6),
    (4, 6),
    (2, 3),
    (4, 2),
    (4, 1),
    (3, 0),
    (1, 0),
    (0, 1),
]];
const D4: Glyph = &[&[(3, 0), (3, 6), (0, 2), (4, 2)]];
const D5: Glyph = &[&[
    (4, 6),
    (0, 6),
    (0, 3),
    (3, 3),
    (4, 2),
    (4, 1),
    (3, 0),
    (1, 0),
    (0, 1),
]];
const D6: Glyph = &[&[
    (4, 5),
    (3, 6),
    (1, 6),
    (0, 5),
    (0, 1),
    (1, 0),
    (3, 0),
    (4, 1),
    (4, 2),
    (3, 3),
    (0, 3),
]];
const D7: Glyph = &[&[(0, 6), (4, 6), (1, 0)]];
const D8: Glyph = &[
    &[
        (1, 3),
        (0, 4),
        (0, 5),
        (1, 6),
        (3, 6),
        (4, 5),
        (4, 4),
        (3, 3),
        (1, 3),
    ],
    &[
        (3, 3),
        (4, 2),
        (4, 1),
        (3, 0),
        (1, 0),
        (0, 1),
        (0, 2),
        (1, 3),
    ],
];
const D9: Glyph = &[
    &[
        (4, 4),
        (3, 3),
        (1, 3),
        (0, 4),
        (0, 5),
        (1, 6),
        (3, 6),
        (4, 5),
        (4, 4),
    ],
    &[(4, 4), (4, 1), (3, 0), (1, 0)],
];

// ---- punctuation ------------------------------------------------------------
/// A space: no strokes (advance only).
const SPACE: Glyph = &[];
/// A period: a single point ⇒ a disc of the pen radius (`Shape2D::trace` of one point).
const DOT: Glyph = &[&[(2, 0)]];
const DASH: Glyph = &[&[(1, 3), (3, 3)]];
const COLON: Glyph = &[&[(2, 1)], &[(2, 4)]];
const SLASH: Glyph = &[&[(0, 0), (4, 6)]];

// ---- symbols commonly used in component labels -----------------------------
/// Ω (ohm, U+03A9): a horseshoe loop splaying into two feet — so `{value:si:Ω}` renders
/// honestly instead of a fallback box.
const OMEGA: Glyph = &[&[
    (0, 0),
    (1, 0),
    (1, 2),
    (0, 3),
    (0, 5),
    (1, 6),
    (3, 6),
    (4, 5),
    (4, 3),
    (3, 2),
    (3, 0),
    (4, 0),
]];
/// µ (micro, U+00B5; U+03BC greek mu aliases here): a `u` bowl whose left stem drops as
/// the descender leg.
const MU: Glyph = &[&[(0, 5), (0, 0)], &[(0, 1), (1, 0), (3, 0), (4, 1), (4, 5)]];

/// The fallback for any character not covered (the rest of printable ASCII / symbols is a
/// deliberate follow-up): a box outline, so an unsupported glyph is *visibly* wrong
/// rather than silently dropped. Lowercase does **not** reach this — it case-folds.
const FALLBACK: Glyph = &[&[(0, 0), (4, 0), (4, 6), (0, 6), (0, 0)]];

/// The stroke polylines for `ch` in cell coordinates. Covers uppercase `A`–`Z`, digits
/// `0`–`9`, space, `.`, `-`, `:`, `/`, `Ω`, and `µ` (`µ`/`μ` share a glyph). A space
/// returns an empty slice (advance only); **lowercase** case-folds to its uppercase glyph;
/// any other unsupported character returns the [`FALLBACK`] box. The returned strokes are
/// scaled + traced by the lowering.
pub fn glyph_strokes(ch: char) -> Glyph {
    match ch {
        'A' => A,
        'B' => B,
        'C' => C,
        'D' => D,
        'E' => E,
        'F' => F,
        'G' => G,
        'H' => H,
        'I' => I,
        'J' => J,
        'K' => K,
        'L' => L,
        'M' => M,
        'N' => N,
        'O' => O,
        'P' => P,
        'Q' => Q,
        'R' => R,
        'S' => S,
        'T' => T,
        'U' => U,
        'V' => V,
        'W' => W,
        'X' => X,
        'Y' => Y,
        'Z' => Z,
        '0' => D0,
        '1' => D1,
        '2' => D2,
        '3' => D3,
        '4' => D4,
        '5' => D5,
        '6' => D6,
        '7' => D7,
        '8' => D8,
        '9' => D9,
        ' ' => SPACE,
        '.' => DOT,
        '-' => DASH,
        ':' => COLON,
        '/' => SLASH,
        'Ω' => OMEGA,
        // Micro sign (U+00B5) and Greek small mu (U+03BC) share one glyph.
        'µ' | 'μ' => MU,
        // No dedicated lowercase glyphs yet: case-fold to the uppercase form rather than
        // drop to a box. (Uppercase never falls back here, so this can't loop.)
        c if c.is_ascii_lowercase() => glyph_strokes(c.to_ascii_uppercase()),
        _ => FALLBACK,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every covered character returns at least one stroke (space is the lone, intended
    /// exception), and the advance is positive.
    #[test]
    fn covered_chars_have_strokes_and_advance() {
        const { assert!(GLYPH_ADVANCE > 0) };
        let covered = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-:/";
        for ch in covered.chars() {
            assert!(
                !glyph_strokes(ch).is_empty(),
                "`{ch}` should have ≥1 stroke"
            );
        }
        // Space is the one covered glyph that is intentionally empty (advance only).
        assert!(glyph_strokes(' ').is_empty(), "space is advance-only");
    }

    /// A genuinely unknown character falls back to a visible box rather than vanishing —
    /// but lowercase does not: it case-folds to its uppercase glyph.
    #[test]
    fn unknown_char_boxes_but_lowercase_case_folds() {
        assert_eq!(glyph_strokes('@'), FALLBACK, "unsupported char → box");
        for (lo, up) in [('a', 'A'), ('r', 'R'), ('k', 'K'), ('z', 'Z')] {
            assert_eq!(
                glyph_strokes(lo),
                glyph_strokes(up),
                "`{lo}` folds to `{up}`"
            );
            assert_ne!(glyph_strokes(lo), FALLBACK, "`{lo}` is not a box");
        }
    }

    /// Ω and µ render real glyphs (so `{value:si:Ω}` / IEC labels are honest), and the
    /// two mu code points share one glyph.
    #[test]
    fn ohm_and_micro_have_glyphs() {
        for ch in ['Ω', 'µ', 'μ'] {
            assert_ne!(glyph_strokes(ch), FALLBACK, "`{ch}` should have a glyph");
            assert!(!glyph_strokes(ch).is_empty());
        }
        assert_eq!(
            glyph_strokes('µ'),
            glyph_strokes('μ'),
            "U+00B5 and U+03BC alias"
        );
    }

    /// Every stroke point sits inside the cell bounds (`x ∈ [0, GLYPH_ADVANCE]`,
    /// `y ∈ [0, CELL_HEIGHT]`), so scaling can't surprise the layout.
    #[test]
    fn strokes_stay_within_the_cell() {
        let all = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-:/ ";
        for ch in all.chars().chain(['a', '@', 'Ω', 'µ', 'μ']) {
            for stroke in glyph_strokes(ch) {
                for &(x, y) in *stroke {
                    assert!((0..=GLYPH_ADVANCE).contains(&x), "{ch}: x={x} out of cell");
                    assert!((0..=CELL_HEIGHT).contains(&y), "{ch}: y={y} out of cell");
                }
            }
        }
    }
}
