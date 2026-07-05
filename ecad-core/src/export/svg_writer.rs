//! Shared SVG emission primitives: coordinate/text formatting and the curve-aware
//! `<path>` builders used by both the board sketch ([`super::svg`]) and, in the case
//! of [`fmt_mm`]/[`xml_escape`], the schematic renderer (`crate::schematic_svg`).
//!
//! Pure integer arithmetic for coordinates ([`fmt_mm`]) keeps the fixed-point
//! determinism invariant intact end to end (see the [`super`] module docs). The arc
//! helpers ([`svg_arc_params`], [`rel_to_start`]) compute their flags with exact
//! integer predicates so the SVG is byte-stable.

use crate::doc::{MM, Nm, Point};
use crate::geom::kernel::Region;
use crate::geom::{Seg, Shape2D, circumcenter};

/// Format a fixed-point nanometre coordinate as a millimetre decimal string with
/// exactly six fractional digits. Pure integer arithmetic — no float, so the
/// fixed-point determinism invariant is preserved end to end (e.g. `-2_000_000` ->
/// `"-2.000000"`, `1_325_000` -> `"1.325000"`).
pub(crate) fn fmt_mm(nm: Nm) -> String {
    let neg = nm < 0;
    let a = nm.unsigned_abs();
    let int = a / MM as u64;
    let frac = a % MM as u64;
    let body = format!("{int}.{frac:06}");
    if neg && a != 0 {
        format!("-{body}")
    } else {
        body
    }
}

/// Minimal XML text escaping for labels.
pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The SVG path `d` for a filled [`Region`]: every ring as an `M …L …Z` subpath. Paired
/// with `fill-rule="evenodd"` so hole rings read as voids. `flip` maps board-y into the
/// SVG (downward) frame.
pub(crate) fn region_svg_d(region: &Region, flip: &impl Fn(Nm) -> Nm) -> String {
    let mut d = String::new();
    for ring in &region.rings {
        if ring.len() < 3 {
            continue;
        }
        for (i, p) in ring.iter().enumerate() {
            let cmd = if i == 0 { "M" } else { "L" };
            d.push_str(&format!("{cmd}{},{} ", fmt_mm(p.x), fmt_mm(flip(p.y))));
        }
        d.push_str("Z ");
    }
    d.trim_end().to_string()
}

/// Does this shape's skeleton contain any curved edge (arc or Bézier)? Straight shapes
/// keep their exact legacy export (polygon / G01 lines); only curve-bearing shapes take
/// the curve-aware `<path>` / contour route.
pub(crate) fn has_curve(s: &Shape2D) -> bool {
    s.path().segs.iter().any(|seg| {
        matches!(
            seg,
            Seg::Arc { .. } | Seg::Quadratic { .. } | Seg::Cubic { .. }
        )
    })
}

/// `mid` and `end` re-expressed relative to `start` (so `start` becomes the origin).
/// All arc predicates work in this frame: translation-invariant, but the degree-4 side
/// test then scales with the board *extent* (the arc's own span, ~cm) rather than the
/// absolute coordinate magnitude, keeping the i128 arithmetic far from overflow even
/// for a board referenced far from the origin.
pub(crate) fn rel_to_start(start: Point, mid: Point, end: Point) -> (Point, Point) {
    (
        Point {
            x: mid.x - start.x,
            y: mid.y - start.y,
        },
        Point {
            x: end.x - start.x,
            y: end.y - start.y,
        },
    )
}

/// SVG elliptical-arc parameters `(radius, large_arc_flag, sweep_flag)` for the arc
/// `start`→`mid`→`end`. The flags are computed **exactly** (integer predicates, in the
/// start-relative frame so they can't overflow at board scale), so the SVG is
/// byte-stable; only the radius uses correctly-rounded `sqrt`.
///
/// - `sweep`: SVG's y axis points *down* (we emit flipped y), which reverses turn
///   handedness, so a model-CCW arc (`turn > 0`) is a screen-CW arc ⇒ `sweep = 0`, and
///   model-CW ⇒ `sweep = 1`.
/// - `large_arc`: 1 iff the sweep exceeds 180°, i.e. the centre and `mid` lie on the
///   **same** side of the chord `start`→`end` (for a minor arc they are on opposite
///   sides; a semicircle puts the centre on the chord ⇒ 0).
pub(crate) fn svg_arc_params(start: Point, mid: Point, end: Point) -> Option<(Nm, u8, u8)> {
    let (b, c) = rel_to_start(start, mid, end); // origin, b=mid, c=end
    let (ux, uy, den) = circumcenter(Point { x: 0, y: 0 }, b, c);
    if den == 0 {
        return None;
    }
    // Centre is start-relative, so radius = |centre − start| = |(cx, cy)|.
    let (cx, cy) = (ux as f64 / den as f64, uy as f64 / den as f64);
    let radius = (cx * cx + cy * cy).sqrt().round() as Nm;
    let sweep: u8 = if den < 0 { 1 } else { 0 };
    // Side of the chord (origin→c) that `mid` (= b) and the centre fall on.
    let side_mid = c.x as i128 * b.y as i128 - c.y as i128 * b.x as i128;
    let num = c.x as i128 * uy - c.y as i128 * ux;
    let side_c = num.signum() * den.signum();
    let large: u8 = if side_mid.signum() == side_c && side_mid != 0 {
        1
    } else {
        0
    };
    Some((radius, large, sweep))
}

/// Build an SVG path `d` for a closed `shape`, walking its skeleton so arc edges become
/// `A` commands (and straight edges `L`). `flip` lifts model-y (up) to SVG-y (down).
pub(crate) fn svg_path_d(shape: &Shape2D, flip: &impl Fn(Nm) -> Nm) -> String {
    let path = shape.path();
    let mut d = format!("M {},{}", fmt_mm(path.start.x), fmt_mm(flip(path.start.y)));
    let mut cur = path.start;
    for seg in &path.segs {
        match seg {
            Seg::Line { end } => {
                d.push_str(&format!(" L {},{}", fmt_mm(end.x), fmt_mm(flip(end.y))));
            }
            Seg::Arc { mid, end } => match svg_arc_params(cur, *mid, *end) {
                Some((r, large, sweep)) => d.push_str(&format!(
                    " A {} {} 0 {} {} {},{}",
                    fmt_mm(r),
                    fmt_mm(r),
                    large,
                    sweep,
                    fmt_mm(end.x),
                    fmt_mm(flip(end.y)),
                )),
                None => d.push_str(&format!(" L {},{}", fmt_mm(end.x), fmt_mm(flip(end.y)))),
            },
            // Béziers export directly — SVG carries them losslessly. Control points are
            // y-flipped alongside the endpoints.
            Seg::Quadratic { ctrl, end } => d.push_str(&format!(
                " Q {},{} {},{}",
                fmt_mm(ctrl.x),
                fmt_mm(flip(ctrl.y)),
                fmt_mm(end.x),
                fmt_mm(flip(end.y)),
            )),
            Seg::Cubic { c1, c2, end } => d.push_str(&format!(
                " C {},{} {},{} {},{}",
                fmt_mm(c1.x),
                fmt_mm(flip(c1.y)),
                fmt_mm(c2.x),
                fmt_mm(flip(c2.y)),
                fmt_mm(end.x),
                fmt_mm(flip(end.y)),
            )),
        }
        cur = seg.end();
    }
    d.push_str(" Z");
    d
}
