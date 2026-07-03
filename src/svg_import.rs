//! SVG board-outline import: parse `<path d="…">` geometry into the arc/Bézier-capable
//! [`geom::Shape2D`] and return the authored `(outline, cutouts)` shapes.
//!
//! This is the consumer the Bézier [`geom::Seg::Quadratic`]/[`geom::Seg::Cubic`]
//! primitives unblocked: an SVG `C`/`Q` segment maps straight onto a native curved
//! [`geom::Seg`], with no premature flattening.
//!
//! **Scope: the board *outline* only.** The largest-area closed subpath becomes the
//! `outline`, the rest become `cutouts` — mirroring [`crate::kicad::import_board_outline`].
//! Silkscreen / text / multi-colour SVG (layer attribution by stroke/fill, `<text>`,
//! `<rect>`/`<circle>` primitives) is a deliberate follow-up; only `<path>` `d` geometry
//! is read here.
//!
//! ## Pragmatic XML scan (not a conformant parser)
//!
//! Rather than pull in a full XML parser, [`extract_path_ds`] scans the text for `<path …>`
//! start-tags and lifts each one's `d` attribute value. This is intentionally lightweight:
//! it does not understand entities, CDATA, namespaces, or comments, and it ignores every
//! other element and attribute. It is sufficient for the board-outline use case where the
//! geometry is authored as plain `<path>` elements; a robust XML front-end can replace it
//! later without touching the path-data parser below.
//!
//! ## Coordinate convention
//!
//! - **Units:** 1 SVG user unit = 1 mm = 1_000_000 nm (a scale argument can come later).
//! - **Y axis:** SVG y points *down*; the model is y-*up*. Every imported y is negated so
//!   geometry is not mirrored. (X is unchanged.)
//! - Coordinates are accumulated in f64 user space and rounded to integer nm on emit.
//!
//! ## Elliptical-arc (`A`/`a`) decision
//!
//! Our [`geom::Seg::Arc`] is a *circular* 3-point arc, so:
//! - A **circular, unrotated** arc (`rx ≈ ry`, x-axis-rotation ≈ 0) becomes a single native
//!   [`geom::Seg::Arc`] (`mid` = the point at the arc's angular midpoint, `end` = the arc
//!   endpoint) — keeping the curve authoritative.
//! - A genuinely **elliptical or rotated** arc is **flattened to line segments** at
//!   [`geom::DEFAULT_CHORD_TOL`] (an ellipse has no exact 3-point circular representation).

use crate::doc::{Nm, Point};
use crate::geom::{DEFAULT_CHORD_TOL, Path, Seg, Shape2D};
use std::f64::consts::PI;

/// nm per SVG user unit (1 user unit = 1 mm).
const SCALE: f64 = 1_000_000.0;

/// Import a board outline from SVG text: parse every `<path>`'s `d` geometry, classify the
/// closed subpaths (largest area = `outline`, the rest = `cutouts`), and return the authored
/// board geometry as `(outline, cutouts)` — [`geom::Shape2D`]s that become `Board`/`Cutout`
/// directives (arcs/curves preserved). Errors (never panics) on malformed/empty path data,
/// an unsupported command, or the absence of any closed subpath.
pub fn import_board_outline(svg_text: &str) -> Result<(Shape2D, Vec<Shape2D>), String> {
    let ds = extract_path_ds(svg_text);
    if ds.is_empty() {
        return Err("no <path> element with a d attribute found".into());
    }
    let mut subs: Vec<Sub> = Vec::new();
    for d in &ds {
        parse_d(d, &mut subs)?;
    }

    // Only closed subpaths bound area. start + ≥2 edges ⇒ ≥3 corners (a triangle).
    let mut closed: Vec<(i128, Shape2D)> = subs
        .into_iter()
        .filter(|s| s.closed && s.segs.len() >= 2)
        .map(|s| {
            let shape = Shape2D::polygon_path(
                Path {
                    start: s.start,
                    segs: s.segs,
                },
                0,
            );
            (loop_area(&shape), shape)
        })
        .collect();
    if closed.is_empty() {
        return Err("SVG has no closed subpath to use as a board outline".into());
    }
    // Largest-area loop is the outline, the rest are cutouts (mirrors the KiCad importer).
    closed.sort_by_key(|x| std::cmp::Reverse(x.0));
    let mut it = closed.into_iter().map(|(_, s)| s);
    let outline = it.next().expect("non-empty checked above");
    let cutouts: Vec<Shape2D> = it.collect();
    // Enforce the crate-wide coordinate ceiling at the import boundary (issue 0018):
    // `to_nm` is infallible, so range-check the produced shapes here. An out-of-range
    // SVG coordinate becomes a clean error, never a silent i128 wrap in the geometry
    // kernel. (Only the retained closed subpaths matter — dropped open paths never
    // reach the kernel.)
    for shape in std::iter::once(&outline).chain(&cutouts) {
        for p in shape.coords() {
            if !crate::geom::point_ok(p) {
                return Err(format!(
                    "coordinate ({}, {}) nm exceeds the ±{} nm (±1 m) range (issue 0018)",
                    p.x,
                    p.y,
                    crate::geom::MAX_COORD
                ));
            }
        }
    }
    Ok((outline, cutouts))
}

/// A subpath under construction: its `start` and edges in *model* (nm, y-flipped) space,
/// plus whether it is closed (an explicit `Z`/`z`, or a final edge returning to `start`).
struct Sub {
    start: Point,
    segs: Vec<Seg>,
    closed: bool,
}

/// Convert an SVG user-space point to a model nm point: scale mm→nm and flip y (SVG y-down
/// → model y-up).
fn to_nm(x: f64, y: f64) -> Point {
    Point {
        x: (x * SCALE).round() as Nm,
        y: (-(y * SCALE)).round() as Nm,
    }
}

/// Signed area ×2 (magnitude) of a closed loop via the shoelace formula over its flattened
/// skeleton (arcs/Béziers subdivided to [`DEFAULT_CHORD_TOL`]). Exact i128. Orientation is
/// irrelevant to classification, so the magnitude is returned.
fn loop_area(shape: &Shape2D) -> i128 {
    let pts = shape.path().flatten(DEFAULT_CHORD_TOL);
    let n = pts.len();
    let mut a2: i128 = 0;
    for i in 0..n {
        let p = pts[i];
        let q = pts[(i + 1) % n];
        a2 += p.x as i128 * q.y as i128 - q.x as i128 * p.y as i128;
    }
    a2.abs()
}

// ----------------------------------------------------------------------------
// Pragmatic `<path d="…">` extraction.
// ----------------------------------------------------------------------------

/// Scan `svg` for `<path …>` start-tags and return each one's `d` attribute value. Not a
/// conformant XML parser (see the module docs): it matches `<path` followed by a tag
/// delimiter, reads up to the next `>`, and lifts a `d="…"`/`d='…'` attribute from inside.
fn extract_path_ds(svg: &str) -> Vec<String> {
    let b = svg.as_bytes();
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(rel) = svg[search..].find("<path") {
        let after = search + rel + 5; // just past "<path"
        search = after;
        // Require a tag boundary so we don't match e.g. "<pathological>".
        match b.get(after).copied() {
            Some(c) if c.is_ascii_whitespace() || c == b'/' || c == b'>' => {}
            _ => continue,
        }
        let Some(gt_rel) = svg[after..].find('>') else {
            break;
        };
        let tag = &svg[after..after + gt_rel];
        if let Some(d) = find_d_attr(tag) {
            out.push(d);
        }
        search = after + gt_rel + 1;
    }
    out
}

/// Find the `d` attribute value within a single start-tag's interior. `d` must be a
/// standalone attribute (preceded by whitespace / tag start) so the `d` in `id`, `width`,
/// `stroke-dasharray`, etc. is not matched.
fn find_d_attr(tag: &str) -> Option<String> {
    let b = tag.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'd' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'=' {
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    j += 1;
                    let start = j;
                    while j < b.len() && b[j] != q {
                        j += 1;
                    }
                    return Some(tag[start..j].to_string());
                }
            }
        }
        i += 1;
    }
    None
}

// ----------------------------------------------------------------------------
// Path-data (`d`) tokenizer.
// ----------------------------------------------------------------------------

/// Cursor over a `d` attribute's bytes. Yields f64 numbers and command letters, skipping
/// the whitespace/comma separators SVG allows between them.
struct Pd<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Pd<'a> {
    fn new(s: &'a str) -> Pd<'a> {
        Pd {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn skip_seps(&mut self) {
        while matches!(self.b.get(self.i), Some(c) if matches!(c, b' ' | b',' | b'\t' | b'\n' | b'\r' | 0x0c))
        {
            self.i += 1;
        }
    }

    /// Peek the next non-separator byte (advancing past separators), without consuming it.
    fn peek(&mut self) -> Option<u8> {
        self.skip_seps();
        self.b.get(self.i).copied()
    }

    /// Consume the byte at the cursor (the caller has already `peek`ed it).
    fn bump(&mut self) {
        self.i += 1;
    }

    /// Is the next token the start of a number?
    fn at_number(&mut self) -> bool {
        matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == b'+' || c == b'-' || c == b'.')
    }

    /// Parse the next number as f64. Hand-rolled scan so `"10-20"` (two numbers) and
    /// `"1.5.5"` (`1.5` then `.5`) tokenize per the SVG grammar.
    fn number(&mut self) -> Result<f64, String> {
        self.skip_seps();
        let start = self.i;
        if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
            self.i += 1;
        }
        let mut digits = false;
        while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
            self.i += 1;
            digits = true;
        }
        if matches!(self.b.get(self.i), Some(b'.')) {
            self.i += 1;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
                digits = true;
            }
        }
        if !digits {
            return Err(format!("expected a number at offset {start} in path data"));
        }
        // Optional exponent; roll back if it has no digits.
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            let mut exp_digits = false;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_digit()) {
                self.i += 1;
                exp_digits = true;
            }
            if !exp_digits {
                self.i = save;
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).expect("ascii subslice");
        s.parse::<f64>()
            .map_err(|e| format!("bad number {s:?} in path data: {e}"))
    }
}

// ----------------------------------------------------------------------------
// Path-data (`d`) command interpreter.
// ----------------------------------------------------------------------------

/// Ensure there is an open subpath to draw into; if none (e.g. the first draw after a `Z`),
/// open one at the current point. Errors if no `moveto` has established a point yet.
fn ensure(cur: &mut Option<Sub>, cx: f64, cy: f64, have_point: bool) -> Result<&mut Sub, String> {
    if cur.is_none() {
        if !have_point {
            return Err("draw command before any moveto".into());
        }
        *cur = Some(Sub {
            start: to_nm(cx, cy),
            segs: Vec::new(),
            closed: false,
        });
    }
    Ok(cur.as_mut().expect("just set"))
}

/// Finalize a subpath: auto-close it if its last edge already returns to `start`, then push.
fn finalize(subs: &mut Vec<Sub>, mut s: Sub) {
    if !s.closed && s.segs.last().is_some_and(|last| last.end() == s.start) {
        s.closed = true;
    }
    subs.push(s);
}

/// Parse one `d` string, appending its subpaths to `subs`.
fn parse_d(d: &str, subs: &mut Vec<Sub>) -> Result<(), String> {
    let mut p = Pd::new(d);
    let (mut cx, mut cy) = (0.0_f64, 0.0_f64); // current point (user space)
    let (mut sx, mut sy) = (0.0_f64, 0.0_f64); // current subpath start (user space)
    let mut have_point = false;
    let mut cur: Option<Sub> = None;

    while let Some(c) = p.peek() {
        if !c.is_ascii_alphabetic() {
            return Err(format!("expected a path command, found {:?}", c as char));
        }
        p.bump();
        match c {
            b'M' | b'm' => {
                let rel = c == b'm';
                if let Some(s) = cur.take() {
                    finalize(subs, s);
                }
                let x = p.number()?;
                let y = p.number()?;
                if rel && have_point {
                    cx += x;
                    cy += y;
                } else {
                    cx = x;
                    cy = y;
                }
                have_point = true;
                sx = cx;
                sy = cy;
                cur = Some(Sub {
                    start: to_nm(cx, cy),
                    segs: Vec::new(),
                    closed: false,
                });
                // Extra coordinate pairs after a moveto are implicit linetos.
                while p.at_number() {
                    let x = p.number()?;
                    let y = p.number()?;
                    if rel {
                        cx += x;
                        cy += y;
                    } else {
                        cx = x;
                        cy = y;
                    }
                    cur.as_mut()
                        .expect("set above")
                        .segs
                        .push(Seg::Line { end: to_nm(cx, cy) });
                }
            }
            b'L' | b'l' => {
                let rel = c == b'l';
                let mut any = false;
                while p.at_number() {
                    let x = p.number()?;
                    let y = p.number()?;
                    if rel {
                        cx += x;
                        cy += y;
                    } else {
                        cx = x;
                        cy = y;
                    }
                    ensure(&mut cur, cx, cy, have_point)?
                        .segs
                        .push(Seg::Line { end: to_nm(cx, cy) });
                    any = true;
                }
                if !any {
                    return Err("L command with no coordinates".into());
                }
            }
            b'H' | b'h' => {
                let rel = c == b'h';
                let mut any = false;
                while p.at_number() {
                    let x = p.number()?;
                    if rel {
                        cx += x;
                    } else {
                        cx = x;
                    }
                    ensure(&mut cur, cx, cy, have_point)?
                        .segs
                        .push(Seg::Line { end: to_nm(cx, cy) });
                    any = true;
                }
                if !any {
                    return Err("H command with no coordinate".into());
                }
            }
            b'V' | b'v' => {
                let rel = c == b'v';
                let mut any = false;
                while p.at_number() {
                    let y = p.number()?;
                    if rel {
                        cy += y;
                    } else {
                        cy = y;
                    }
                    ensure(&mut cur, cx, cy, have_point)?
                        .segs
                        .push(Seg::Line { end: to_nm(cx, cy) });
                    any = true;
                }
                if !any {
                    return Err("V command with no coordinate".into());
                }
            }
            b'C' | b'c' => {
                let rel = c == b'c';
                let mut any = false;
                while p.at_number() {
                    let x1 = p.number()?;
                    let y1 = p.number()?;
                    let x2 = p.number()?;
                    let y2 = p.number()?;
                    let x = p.number()?;
                    let y = p.number()?;
                    let (c1x, c1y, c2x, c2y, ex, ey) = if rel {
                        (cx + x1, cy + y1, cx + x2, cy + y2, cx + x, cy + y)
                    } else {
                        (x1, y1, x2, y2, x, y)
                    };
                    ensure(&mut cur, cx, cy, have_point)?.segs.push(Seg::Cubic {
                        c1: to_nm(c1x, c1y),
                        c2: to_nm(c2x, c2y),
                        end: to_nm(ex, ey),
                    });
                    cx = ex;
                    cy = ey;
                    any = true;
                }
                if !any {
                    return Err("C command with no coordinates".into());
                }
            }
            b'Q' | b'q' => {
                let rel = c == b'q';
                let mut any = false;
                while p.at_number() {
                    let x1 = p.number()?;
                    let y1 = p.number()?;
                    let x = p.number()?;
                    let y = p.number()?;
                    let (qx, qy, ex, ey) = if rel {
                        (cx + x1, cy + y1, cx + x, cy + y)
                    } else {
                        (x1, y1, x, y)
                    };
                    ensure(&mut cur, cx, cy, have_point)?
                        .segs
                        .push(Seg::Quadratic {
                            ctrl: to_nm(qx, qy),
                            end: to_nm(ex, ey),
                        });
                    cx = ex;
                    cy = ey;
                    any = true;
                }
                if !any {
                    return Err("Q command with no coordinates".into());
                }
            }
            b'A' | b'a' => {
                let rel = c == b'a';
                let mut any = false;
                while p.at_number() {
                    let rx = p.number()?.abs();
                    let ry = p.number()?.abs();
                    let xrot = p.number()?;
                    let fa = p.number()? != 0.0;
                    let fs = p.number()? != 0.0;
                    let x = p.number()?;
                    let y = p.number()?;
                    let (ex, ey) = if rel { (cx + x, cy + y) } else { (x, y) };
                    let segs = arc_segs(cx, cy, ex, ey, rx, ry, xrot, fa, fs);
                    ensure(&mut cur, cx, cy, have_point)?.segs.extend(segs);
                    cx = ex;
                    cy = ey;
                    any = true;
                }
                if !any {
                    return Err("A command with no parameters".into());
                }
            }
            b'Z' | b'z' => {
                let mut s = cur
                    .take()
                    .ok_or("Z/z command with no open subpath".to_string())?;
                s.closed = true;
                finalize(subs, s);
                // The point returns to the subpath start; a following draw (without an
                // intervening M) begins a new subpath there (handled by `ensure`).
                cx = sx;
                cy = sy;
            }
            other => {
                return Err(format!(
                    "unsupported path command {:?} (supported: M L H V C Q A Z, upper/lower)",
                    other as char
                ));
            }
        }
    }
    if let Some(s) = cur.take() {
        finalize(subs, s);
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Elliptical-arc (`A`/`a`) → Seg(s).
// ----------------------------------------------------------------------------

/// Convert one SVG elliptical-arc command (start `(x1,y1)` → end `(x2,y2)`, radii `rx`/`ry`,
/// `xrot_deg` x-axis rotation, large-arc `fa`, sweep `fs`) into model-space [`Seg`]s.
///
/// Circular & unrotated ⇒ a single native [`Seg::Arc`]; otherwise flattened to lines at
/// [`DEFAULT_CHORD_TOL`]. Implements the W3C SVG endpoint→center parameterization (impl.
/// notes F.6.5). All math is in user space; points are converted via [`to_nm`].
#[allow(clippy::too_many_arguments)]
fn arc_segs(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    mut rx: f64,
    mut ry: f64,
    xrot_deg: f64,
    fa: bool,
    fs: bool,
) -> Vec<Seg> {
    // Degenerate radii or coincident endpoints ⇒ a straight line (per the SVG spec).
    if rx == 0.0 || ry == 0.0 || (x1 == x2 && y1 == y2) {
        return vec![Seg::Line { end: to_nm(x2, y2) }];
    }
    let phi = xrot_deg.to_radians();
    let (cosp, sinp) = (phi.cos(), phi.sin());
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let x1p = cosp * dx + sinp * dy;
    let y1p = -sinp * dx + cosp * dy;
    // Scale radii up if they are too small to span the chord.
    let lam = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }
    let sign = if fa != fs { 1.0 } else { -1.0 };
    let num = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let coef = sign * (num / den).sqrt();
    let cxp = coef * (rx * y1p / ry);
    let cyp = coef * (-ry * x1p / rx);
    let cx = cosp * cxp - sinp * cyp + (x1 + x2) / 2.0;
    let cy = sinp * cxp + cosp * cyp + (y1 + y2) / 2.0;

    // Signed angle from u to v.
    let ang = |ux: f64, uy: f64, vx: f64, vy: f64| -> f64 {
        let dot = ux * vx + uy * vy;
        let len = (ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let (ux, uy) = ((x1p - cxp) / rx, (y1p - cyp) / ry);
    let (vx, vy) = ((-x1p - cxp) / rx, (-y1p - cyp) / ry);
    let theta1 = ang(1.0, 0.0, ux, uy);
    let mut dtheta = ang(ux, uy, vx, vy);
    if !fs && dtheta > 0.0 {
        dtheta -= 2.0 * PI;
    }
    if fs && dtheta < 0.0 {
        dtheta += 2.0 * PI;
    }

    // Point on the (possibly rotated) ellipse at parameter angle `t`.
    let pt = |t: f64| -> (f64, f64) {
        let (ct, st) = (t.cos(), t.sin());
        (
            cosp * rx * ct - sinp * ry * st + cx,
            sinp * rx * ct + cosp * ry * st + cy,
        )
    };

    // Circular & unrotated ⇒ a native 3-point arc (mid at the angular midpoint).
    if (rx - ry).abs() < 1e-9 * rx.max(1.0) && phi.abs() < 1e-9 {
        let (mx, my) = pt(theta1 + dtheta / 2.0);
        return vec![Seg::Arc {
            mid: to_nm(mx, my),
            end: to_nm(x2, y2),
        }];
    }

    // Elliptical/rotated ⇒ flatten. Choose the angular step so the per-chord sagitta of
    // the larger radius stays within the chord tolerance.
    let r = rx.max(ry);
    let tol = DEFAULT_CHORD_TOL as f64 / SCALE; // user units
    let step = if r > tol {
        2.0 * (1.0 - tol / r).clamp(-1.0, 1.0).acos()
    } else {
        PI
    };
    let n = ((dtheta.abs() / step).ceil() as usize).max(1);
    let mut out = Vec::with_capacity(n);
    for k in 1..=n {
        if k == n {
            out.push(Seg::Line { end: to_nm(x2, y2) });
        } else {
            let (px, py) = pt(theta1 + dtheta * (k as f64 / n as f64));
            out.push(Seg::Line { end: to_nm(px, py) });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MM: Nm = 1_000_000;

    fn pt(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }

    /// Board membership for an imported `(outline, cutouts)`: inside the outline and
    /// outside every cutout (the former `BoardShape::contains`).
    fn on_board(b: &(Shape2D, Vec<Shape2D>), p: Point) -> bool {
        b.0.contains_point(p) && !b.1.iter().any(|c| c.contains_point(p))
    }

    /// `M 0 0 L 10 0 L 10 8 L 0 8 Z` ⇒ a rectangle. A point inside in *model* space
    /// (y-up) is contained; its y-mirror is not — confirming the y-flip happened.
    #[test]
    fn rectangle_path_is_contained_and_y_flipped() {
        let svg = r#"<svg><path d="M 0 0 L 10 0 L 10 8 L 0 8 Z"/></svg>"#;
        let b = import_board_outline(svg).unwrap();
        assert!(b.1.is_empty());
        // SVG corner (5,4) maps to model (5mm, -4mm): inside.
        assert!(
            on_board(&b, pt(5 * MM, -4 * MM)),
            "interior point on the board"
        );
        // The un-flipped point (5mm, +4mm) must be OFF the board (y was negated).
        assert!(
            !on_board(&b, pt(5 * MM, 4 * MM)),
            "the un-flipped point must be off-board"
        );
        // Four corners.
        assert_eq!(b.0.points().len(), 4);
    }

    /// A closed path with a cubic top edge yields a `Seg::Cubic` in the outline.
    #[test]
    fn cubic_command_becomes_cubic_seg() {
        let svg = r#"<path d="M 0 0 C 2 3 8 3 10 0 L 10 5 L 0 5 Z"/>"#;
        let b = import_board_outline(svg).unwrap();
        assert!(
            b.0.path()
                .segs
                .iter()
                .any(|s| matches!(s, Seg::Cubic { .. })),
            "the C command must map to a Seg::Cubic: {:?}",
            b.0.path().segs
        );
    }

    /// A quadratic command maps to a `Seg::Quadratic`.
    #[test]
    fn quadratic_command_becomes_quadratic_seg() {
        let svg = r#"<path d="M 0 0 Q 5 6 10 0 L 10 5 L 0 5 Z"/>"#;
        let b = import_board_outline(svg).unwrap();
        assert!(
            b.0.path()
                .segs
                .iter()
                .any(|s| matches!(s, Seg::Quadratic { .. })),
            "the Q command must map to a Seg::Quadratic"
        );
    }

    /// Outer rectangle + inner rectangle subpath ⇒ 1 outline + 1 cutout, and the cutout
    /// centre is inside the outline but off the board.
    #[test]
    fn outer_and_inner_subpaths_are_outline_and_cutout() {
        let svg = r#"<path d="M 0 0 L 20 0 L 20 20 L 0 20 Z M 5 5 L 15 5 L 15 15 L 5 15 Z"/>"#;
        let b = import_board_outline(svg).unwrap();
        assert_eq!(b.1.len(), 1, "inner loop is a cutout");
        // Inside outer but outside inner: on board. (model y negated)
        assert!(on_board(&b, pt(2 * MM, -2 * MM)));
        // Centre of the inner rect: inside the outline, carved out ⇒ off-board.
        assert!(b.0.contains_point(pt(10 * MM, -10 * MM)));
        assert!(!on_board(&b, pt(10 * MM, -10 * MM)));
    }

    /// Relative `m`/`l` produce the same outline as the absolute form.
    #[test]
    fn relative_commands_match_absolute() {
        let abs = import_board_outline(r#"<path d="M 0 0 L 10 0 L 10 8 L 0 8 Z"/>"#).unwrap();
        let rel = import_board_outline(r#"<path d="m 0 0 l 10 0 l 0 8 l -10 0 z"/>"#).unwrap();
        assert_eq!(abs.0, rel.0);
    }

    /// Relative cubic `c` matches its absolute `C` equivalent (controls relative to the
    /// current point).
    #[test]
    fn relative_cubic_matches_absolute() {
        let abs =
            import_board_outline(r#"<path d="M 0 0 C 2 3 8 3 10 0 L 10 5 L 0 5 Z"/>"#).unwrap();
        let rel =
            import_board_outline(r#"<path d="m 0 0 c 2 3 8 3 10 0 l 0 5 l -10 0 z"/>"#).unwrap();
        assert_eq!(abs.0, rel.0);
    }

    /// A circular arc (`rx == ry`, no rotation) becomes a native `Seg::Arc`.
    #[test]
    fn circular_arc_becomes_arc_seg() {
        // A half-disc: straight base, semicircular top via a circular A.
        let svg = r#"<path d="M 0 0 A 5 5 0 0 1 10 0 L 10 0 Z"/>"#;
        let b = import_board_outline(svg).unwrap();
        assert!(
            b.0.path().segs.iter().any(|s| matches!(s, Seg::Arc { .. })),
            "a circular A must map to a Seg::Arc: {:?}",
            b.0.path().segs
        );
    }

    #[test]
    fn malformed_and_empty_inputs_error_without_panic() {
        assert!(import_board_outline("").is_err(), "empty input");
        assert!(
            import_board_outline("<svg></svg>").is_err(),
            "no path element"
        );
        assert!(
            import_board_outline(r#"<path d=""/>"#).is_err(),
            "empty d ⇒ no closed subpath"
        );
        assert!(
            import_board_outline(r#"<path d="M 0 0 L 1"/>"#).is_err(),
            "odd coordinate count"
        );
        assert!(
            import_board_outline(r#"<path d="L 1 2 Z"/>"#).is_err(),
            "draw before moveto"
        );
        assert!(
            import_board_outline(r#"<path d="M 0 0 S 1 2 3 4"/>"#).is_err(),
            "unsupported command S"
        );
        // An open (unclosed) path has no closed subpath ⇒ error, not a panic.
        assert!(
            import_board_outline(r#"<path d="M 0 0 L 10 0 L 10 8"/>"#).is_err(),
            "open path is not a board outline"
        );
    }
}
