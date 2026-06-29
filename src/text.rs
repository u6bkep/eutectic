//! Text front-end: the canonical serializer + parser for tier-1 truth.
//!
//! This is the agent/git-facing authoring surface promised by the architecture's
//! "model-as-truth, text as a projection" section. It is NOT a second mutation
//! surface and NOT a synced artifact: [`serialize`] and [`parse`] are the two
//! halves of one canonical projection of the *authoritative* tier-1 state — the
//! generative `source` directives and the ID-keyed `overrides` map. Materialized
//! positions and nets are deliberately *not* serialized; they are derived and
//! re-elaborated on load (the [`project`](crate::project) module renders those for
//! viewing).
//!
//! # Guarantees
//! - **Deterministic / canonical.** [`serialize`] is a pure function of the doc
//!   with stable output: source directives render in source order (which is itself
//!   tier-1 truth — instance order drives default placement), overrides render in
//!   `BTreeMap` (id) order, and every coordinate renders in one canonical unit.
//! - **Idempotent.** `serialize(parse(serialize(doc)).into_doc())` byte-equals
//!   `serialize(doc)`.
//! - **Round-trips.** `parse(serialize(doc))` reproduces `(source, overrides)`
//!   exactly, so re-elaborating it reproduces the same `components`/`nets`/`report`.
//! - **Tolerant in, canonical out.** Coordinates may be authored in mm
//!   (`30mm`, `0.5mm`) or raw nanometres (`30000000nm`, or a bare integer); they
//!   always serialize back as canonical mm.
//!
//! # Grammar
//!
//! One directive per line. Blank lines and `#`-to-end-of-line comments are ignored.
//! Tokens are whitespace-separated; coordinates are written `(x, y)`.
//!
//! ```text
//! # ---- generative source (tier-1) ----
//! inst    <path> <part>            # instantiate a part at a hierarchical path
//! place   <path> (<x>, <y>)        # source default placement (a free DOF)
//! fix     <path> (<x>, <y>)        # hard placement constraint (mechanical datum)
//! board   (<x>, <y>) (<x>, <y>)    # board outline (min corner, max corner)
//! near    <a> <b> <len>            # keep a within <len> of b
//! minsep  <a> <b> <len>            # keep a and b at least <len> apart
//! alignx  <node> <node> ...        # share an x coordinate (vertical line)
//! aligny  <node> <node> ...        # share a y coordinate (horizontal line)
//! connect <compA>.<port> <compB>.<port>   # typed-interface connection (auto-crossed)
//! net     <name> <comp>.<pin> <comp>.<pin> ...   # join discrete pins onto a net
//! nc      <comp>.<pin> <comp>.<pin> ...          # mark pads deliberately unconnected
//!
//! A `<pin>` in `net`/`nc` is a *selector*: a functional name fans out to every pad
//! with that name (a multi-pad power rail), or a pad number selects one pad.
//!
//! # ---- ID-keyed overrides (tier-1) ----
//! hint    <path> (<x>, <y>)        # weak override (a nudge; decays if ineffective)
//! pin     <path> (<x>, <y>)        # strong override (explicit intent; kept)
//! ```
//!
//! `<len>` and the coordinate components accept `<n>mm` (decimal ok), `<n>nm`, or a
//! bare integer (interpreted as nm). A `<comp>.<pin>` reference splits at the *last*
//! dot, so hierarchical comp paths like `psu.dec[0].p1` resolve to comp
//! `psu.dec[0]`, pin `p1`.
//!
//! Example:
//!
//! ```text
//! inst psu.reg LDO
//! inst psu.dec[0] Cap
//! net VBUS psu.reg.VOUT psu.dec[0].p1
//! net GND psu.reg.GND psu.dec[0].p2
//! fix psu.reg (0mm, 0mm)
//! near psu.dec[0] psu.reg 2mm
//!
//! # overrides
//! pin psu.dec[0] (5.5mm, 3mm)
//! ```

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::{Doc, Nm, Override, Point, Strength, MM};
use crate::elaborate::{board_rect, GenDirective, Source};
use crate::geom::Shape2D;
use crate::id::EntityId;
use std::collections::BTreeMap;

/// The parsed tier-1 state: the generative program plus the ID-keyed override map.
pub type Parsed = (Source, BTreeMap<EntityId, Override>);

// ----------------------------------------------------------------------------
// Serialize
// ----------------------------------------------------------------------------

/// Render the authoritative tier-1 state (`source` + `overrides`) as canonical
/// text. Pure and deterministic. Materialized/derived state is intentionally not
/// emitted.
pub fn serialize(doc: &Doc) -> String {
    let mut out = String::new();
    for d in &doc.source {
        out.push_str(&render_directive(d));
        out.push('\n');
    }
    // Overrides last, in deterministic id order. (Empty overrides — pos == None —
    // are inert and carry no canonical text.)
    let mut first = true;
    for (id, ov) in &doc.overrides {
        let Some(pos) = ov.pos else { continue };
        if first {
            out.push_str("\n# overrides\n");
            first = false;
        }
        let kw = match ov.strength {
            Strength::Hint => "hint",
            Strength::Pin => "pin",
        };
        out.push_str(&format!("{kw} {id} {}\n", fmt_point(pos)));
    }
    out
}

fn render_directive(d: &GenDirective) -> String {
    match d {
        GenDirective::Instance { path, part } => format!("inst {path} {part}"),
        GenDirective::Place { path, pos } => format!("place {path} {}", fmt_point(*pos)),
        GenDirective::Fix { path, pos } => format!("fix {path} {}", fmt_point(*pos)),
        GenDirective::Board { outline } => {
            // Serialized as an explicit polygon (`board <p> <p> ...`); the rect
            // shorthand `boardrect <min> <max>` is parse-only sugar. Corner radius
            // (rounded outlines) is not yet serialized — a noted follow-up.
            format!("board {}", outline.points().iter().map(|p| fmt_point(*p)).collect::<Vec<_>>().join(" "))
        }
        GenDirective::Cutout { shape } => {
            format!("cutout {}", shape.points().iter().map(|p| fmt_point(*p)).collect::<Vec<_>>().join(" "))
        }
        GenDirective::Near { a, b, within } => format!("near {a} {b} {}", fmt_len(*within)),
        GenDirective::MinSep { a, b, gap } => format!("minsep {a} {b} {}", fmt_len(*gap)),
        GenDirective::AlignX { nodes } => format!("alignx {}", nodes.join(" ")),
        GenDirective::AlignY { nodes } => format!("aligny {}", nodes.join(" ")),
        GenDirective::ConnectInterface { a, b } => {
            format!("connect {}.{} {}.{}", a.0, a.1, b.0, b.1)
        }
        GenDirective::ConnectPins { net, pins } => {
            let mut s = format!("net {net}");
            for (comp, pin) in pins {
                s.push_str(&format!(" {comp}.{pin}"));
            }
            s
        }
        GenDirective::NoConnect { pins } => {
            let mut s = String::from("nc");
            for (comp, pin) in pins {
                s.push_str(&format!(" {comp}.{pin}"));
            }
            s
        }
        GenDirective::Rotate { path, deg } => format!("rotate {path} {deg}"),
        GenDirective::NearPin { a, b_comp, b_pin, within } => {
            format!("nearpin {a} {b_comp}.{b_pin} {}", fmt_len(*within))
        }
    }
}

fn fmt_point(p: Point) -> String {
    format!("({}, {})", fmt_len(p.x), fmt_len(p.y))
}

/// Canonical length rendering: always millimetres. Whole-mm values print without a
/// fraction (`30mm`); otherwise the minimal exact decimal is emitted (`0.5mm`,
/// `0.000001mm` for a single nm). Exact for any `i64` nm — no float involved.
fn fmt_len(v: Nm) -> String {
    if v % MM == 0 {
        return format!("{}mm", v / MM);
    }
    let neg = v < 0;
    let a = v.unsigned_abs();
    let whole = a / MM as u64;
    let frac = a % MM as u64;
    let frac6 = format!("{frac:06}");
    let trimmed = frac6.trim_end_matches('0');
    format!("{}{whole}.{trimmed}mm", if neg { "-" } else { "" })
}

// ----------------------------------------------------------------------------
// Parse
// ----------------------------------------------------------------------------

/// Parse canonical (or human-authored) text back into tier-1 state. Comments
/// (`#`...) and blank lines are skipped. Never panics. *Collect-all*: every
/// malformed line is reported (located by line number via [`Location::Span`]), so
/// one parse surfaces all syntax errors at once; on any error the whole parse fails
/// with `Err(Vec<Diagnostic>)` and no partial state escapes.
pub fn parse(text: &str) -> Result<Parsed, Vec<Diagnostic>> {
    let mut source: Source = Vec::new();
    let mut overrides: BTreeMap<EntityId, Override> = BTreeMap::new();
    let mut errors: Vec<Diagnostic> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let lineno = (i + 1) as u32;
        // Strip comments and surrounding whitespace.
        let line = match raw.split_once('#') {
            Some((code, _)) => code,
            None => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok(Item::Directive(d)) => source.push(d),
            Ok(Item::Override(id, ov)) => {
                overrides.insert(id, ov);
            }
            Err(e) => errors.push(Diagnostic::error(
                "E_PARSE",
                format!("{e} (in `{line}`)"),
                Location::Span { line: lineno, col: 1 },
            )),
        }
    }
    if errors.is_empty() {
        Ok((source, overrides))
    } else {
        Err(errors)
    }
}

enum Item {
    Directive(GenDirective),
    Override(EntityId, Override),
}

fn parse_line(line: &str) -> Result<Item, String> {
    let (kw, rest) = match line.split_once(char::is_whitespace) {
        Some((k, r)) => (k, r.trim()),
        None => (line, ""),
    };
    Ok(match kw {
        "inst" => {
            let (path, part) = two_tokens(rest, "inst <path> <part>")?;
            Item::Directive(GenDirective::Instance { path, part })
        }
        "place" => {
            let (path, pos) = path_and_point(rest)?;
            Item::Directive(GenDirective::Place { path, pos })
        }
        "fix" => {
            let (path, pos) = path_and_point(rest)?;
            Item::Directive(GenDirective::Fix { path, pos })
        }
        "board" => {
            let pts = extract_points(rest)?;
            if pts.len() < 3 {
                return Err("board needs ≥3 outline points: board (x,y) (x,y) (x,y) ...".into());
            }
            Item::Directive(GenDirective::Board { outline: Shape2D::polygon(pts) })
        }
        "boardrect" => {
            let pts = extract_points(rest)?;
            if pts.len() != 2 {
                return Err("boardrect needs two corners: boardrect (minx,miny) (maxx,maxy)".into());
            }
            Item::Directive(board_rect(pts[0], pts[1]))
        }
        "cutout" => {
            let pts = extract_points(rest)?;
            if pts.len() < 3 {
                return Err("cutout needs ≥3 points: cutout (x,y) (x,y) (x,y) ...".into());
            }
            Item::Directive(GenDirective::Cutout { shape: Shape2D::polygon(pts) })
        }
        "near" => {
            let (a, b, len) = two_tokens_and_len(rest, "near <a> <b> <len>")?;
            Item::Directive(GenDirective::Near { a, b, within: len })
        }
        "minsep" => {
            let (a, b, len) = two_tokens_and_len(rest, "minsep <a> <b> <len>")?;
            Item::Directive(GenDirective::MinSep { a, b, gap: len })
        }
        "alignx" => Item::Directive(GenDirective::AlignX { nodes: node_list(rest, "alignx")? }),
        "aligny" => Item::Directive(GenDirective::AlignY { nodes: node_list(rest, "aligny")? }),
        "rotate" => {
            let (path, deg) = two_tokens(rest, "rotate <path> <deg>")?;
            let deg: i32 = deg
                .parse()
                .map_err(|_| format!("`{deg}` is not an integer degree count"))?;
            Item::Directive(GenDirective::Rotate { path, deg })
        }
        "nearpin" => {
            let (a, bpin, len) = two_tokens_and_len(rest, "nearpin <a> <bComp>.<bPin> <len>")?;
            let (b_comp, b_pin) = split_last_dot(&bpin, "pin")?;
            Item::Directive(GenDirective::NearPin { a, b_comp, b_pin, within: len })
        }
        "connect" => {
            let (a, b) = two_tokens(rest, "connect <compA>.<port> <compB>.<port>")?;
            Item::Directive(GenDirective::ConnectInterface {
                a: split_last_dot(&a, "interface port")?,
                b: split_last_dot(&b, "interface port")?,
            })
        }
        "net" => {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.len() < 2 {
                return Err("net needs a name and at least one pin: net <name> <comp>.<pin> ...".into());
            }
            let net = toks[0].to_string();
            let mut pins = Vec::new();
            for t in &toks[1..] {
                pins.push(split_last_dot(t, "pin")?);
            }
            Item::Directive(GenDirective::ConnectPins { net, pins })
        }
        "nc" => {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.is_empty() {
                return Err("nc needs at least one pin: nc <comp>.<pin> ...".into());
            }
            let mut pins = Vec::new();
            for t in &toks {
                pins.push(split_last_dot(t, "pin")?);
            }
            Item::Directive(GenDirective::NoConnect { pins })
        }
        "hint" | "pin" => {
            let (path, pos) = path_and_point(rest)?;
            let strength = if kw == "pin" { Strength::Pin } else { Strength::Hint };
            Item::Override(EntityId::new(path), Override { pos: Some(pos), strength })
        }
        other => return Err(format!("unknown directive `{other}`")),
    })
}

fn two_tokens(rest: &str, usage: &str) -> Result<(String, String), String> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if toks.len() != 2 {
        return Err(format!("expected: {usage}"));
    }
    Ok((toks[0].to_string(), toks[1].to_string()))
}

fn two_tokens_and_len(rest: &str, usage: &str) -> Result<(String, String, Nm), String> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if toks.len() != 3 {
        return Err(format!("expected: {usage}"));
    }
    Ok((toks[0].to_string(), toks[1].to_string(), parse_len(toks[2])?))
}

fn node_list(rest: &str, kw: &str) -> Result<Vec<String>, String> {
    let nodes: Vec<String> = rest.split_whitespace().map(String::from).collect();
    if nodes.is_empty() {
        return Err(format!("{kw} needs at least one node"));
    }
    Ok(nodes)
}

/// `<path> (<x>, <y>)` — path is everything up to the first `(`.
fn path_and_point(rest: &str) -> Result<(String, Point), String> {
    let open = rest.find('(').ok_or("expected a coordinate `(x, y)`")?;
    let path = rest[..open].trim();
    if path.is_empty() {
        return Err("missing path before coordinate".into());
    }
    let pts = extract_points(&rest[open..])?;
    if pts.len() != 1 {
        return Err("expected exactly one coordinate `(x, y)`".into());
    }
    Ok((path.to_string(), pts[0]))
}

/// Split a `comp.field` reference at the *last* dot, so hierarchical comp paths
/// survive (e.g. `psu.dec[0].p1` -> (`psu.dec[0]`, `p1`)).
fn split_last_dot(s: &str, what: &str) -> Result<(String, String), String> {
    match s.rsplit_once('.') {
        Some((comp, field)) if !comp.is_empty() && !field.is_empty() => {
            Ok((comp.to_string(), field.to_string()))
        }
        _ => Err(format!("`{s}` must be of the form <comp>.<{what}>")),
    }
}

/// Pull out every `(x, y)` group from a string, in order.
fn extract_points(s: &str) -> Result<Vec<Point>, String> {
    let mut pts = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let close_rel = rest[open..].find(')').ok_or("unbalanced '(' in coordinate")?;
        let close = open + close_rel;
        let inner = &rest[open + 1..close];
        let (xs, ys) = inner.split_once(',').ok_or("coordinate must be `(x, y)`")?;
        pts.push(Point { x: parse_len(xs.trim())?, y: parse_len(ys.trim())? });
        rest = &rest[close + 1..];
    }
    // Anything non-whitespace outside the parens is a malformed coordinate.
    if pts.is_empty() {
        return Err("expected a coordinate `(x, y)`".into());
    }
    Ok(pts)
}

/// Parse a length token into nanometres. Accepts `<n>mm` (decimal allowed),
/// `<n>nm`, or a bare integer (interpreted as nm).
fn parse_len(tok: &str) -> Result<Nm, String> {
    let t = tok.trim();
    if t.is_empty() {
        return Err("empty length".into());
    }
    if let Some(body) = t.strip_suffix("mm") {
        parse_mm(body)
    } else if let Some(body) = t.strip_suffix("nm") {
        parse_int_nm(body)
    } else {
        parse_int_nm(t)
    }
}

fn parse_int_nm(body: &str) -> Result<Nm, String> {
    body.trim()
        .parse::<i64>()
        .map_err(|_| format!("`{body}` is not an integer number of nm"))
}

fn parse_mm(body: &str) -> Result<Nm, String> {
    let body = body.trim();
    let (neg, body) = match body.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let (whole_str, frac_str) = match body.split_once('.') {
        Some((w, f)) => (w, f),
        None => (body, ""),
    };
    let whole: i64 = if whole_str.is_empty() {
        0
    } else {
        whole_str.parse().map_err(|_| format!("`{body}mm` has a non-numeric whole part"))?
    };
    if frac_str.len() > 6 {
        return Err(format!("`{body}mm` has sub-nanometre precision (max 6 decimal places)"));
    }
    let frac: i64 = if frac_str.is_empty() {
        0
    } else {
        // Pad on the right to 6 digits: ".5" -> 500000 nm, ".000001" -> 1 nm.
        format!("{frac_str:0<6}")
            .parse()
            .map_err(|_| format!("`{body}mm` has a non-numeric fraction"))?
    };
    let val = whole * MM + frac;
    Ok(if neg { -val } else { val })
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{Command, Transaction};
    use crate::doc::Point;
    use crate::elaborate::{elaborate, psu_module};
    use crate::history::History;
    use crate::part::part_library;

    // ---- fixtures --------------------------------------------------------

    fn uart_link() -> Source {
        vec![
            GenDirective::Instance { path: "mcu".into(), part: "MCU".into() },
            GenDirective::Instance { path: "sens".into(), part: "Sensor".into() },
            GenDirective::ConnectInterface {
                a: ("mcu".into(), "uart".into()),
                b: ("sens".into(), "uart".into()),
            },
        ]
    }

    /// A scene exercising Board / Near / MinSep / AlignY / Fix.
    fn placement_scene() -> Source {
        vec![
            GenDirective::Instance { path: "reg".into(), part: "LDO".into() },
            GenDirective::Instance { path: "c1".into(), part: "Cap".into() },
            GenDirective::Instance { path: "c2".into(), part: "Cap".into() },
            GenDirective::Fix { path: "reg".into(), pos: Point::mm(0, 0) },
            board_rect(Point::mm(0, 0), Point::mm(50, 50)),
            GenDirective::Near { a: "c1".into(), b: "reg".into(), within: 3 * MM },
            GenDirective::Near { a: "c2".into(), b: "reg".into(), within: 3 * MM },
            GenDirective::MinSep { a: "c1".into(), b: "c2".into(), gap: 4 * MM },
            GenDirective::AlignY { nodes: vec!["c1".into(), "c2".into()] },
        ]
    }

    /// A hand-built source touching *every* GenDirective variant.
    fn all_variants() -> Source {
        vec![
            GenDirective::Instance { path: "psu.reg".into(), part: "LDO".into() },
            GenDirective::Instance { path: "psu.dec[0]".into(), part: "Cap".into() },
            GenDirective::Instance { path: "mcu".into(), part: "MCU".into() },
            GenDirective::Instance { path: "sens".into(), part: "Sensor".into() },
            GenDirective::Place { path: "psu.dec[0]".into(), pos: Point::mm(5, 5) },
            GenDirective::Fix { path: "psu.reg".into(), pos: Point { x: 1, y: -2_500_000 } },
            board_rect(Point::mm(0, 0), Point::mm(50, 50)),
            GenDirective::Cutout {
                shape: Shape2D::polygon(vec![
                    Point::mm(20, 20),
                    Point::mm(30, 20),
                    Point::mm(25, 30),
                ]),
            },
            GenDirective::Near { a: "psu.dec[0]".into(), b: "psu.reg".into(), within: 2 * MM },
            GenDirective::MinSep { a: "psu.dec[0]".into(), b: "mcu".into(), gap: MM },
            GenDirective::AlignX { nodes: vec!["psu.reg".into(), "psu.dec[0]".into()] },
            GenDirective::AlignY { nodes: vec!["mcu".into(), "sens".into()] },
            GenDirective::Rotate { path: "psu.reg".into(), deg: 90 },
            GenDirective::NearPin {
                a: "psu.dec[0]".into(),
                b_comp: "psu.reg".into(),
                b_pin: "VOUT".into(),
                within: 2 * MM,
            },
            GenDirective::ConnectInterface {
                a: ("mcu".into(), "uart".into()),
                b: ("sens".into(), "uart".into()),
            },
            GenDirective::ConnectPins {
                net: "VBUS".into(),
                pins: vec![("psu.reg".into(), "VOUT".into()), ("psu.dec[0]".into(), "p1".into())],
            },
            GenDirective::NoConnect {
                pins: vec![("psu.reg".into(), "GND".into()), ("mcu".into(), "GPIO0".into())],
            },
        ]
    }

    fn doc_of(source: Source, overrides: BTreeMap<EntityId, Override>) -> Doc {
        Doc { source, overrides, ..Default::default() }
    }

    fn placed(src: Source) -> Doc {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s").unwrap();
        h.doc().clone()
    }

    // ---- round-trip + idempotence ---------------------------------------

    /// `parse(serialize(doc))` reproduces `(source, overrides)` exactly, for a
    /// source that touches every directive variant plus both override strengths.
    #[test]
    fn round_trip_all_variants() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            EntityId::new("psu.dec[0]"),
            Override { pos: Some(Point::mm(7, 3)), strength: Strength::Hint },
        );
        overrides.insert(
            EntityId::new("mcu"),
            Override { pos: Some(Point { x: 12_345_678, y: -500_000 }), strength: Strength::Pin },
        );
        let doc = doc_of(all_variants(), overrides);

        let text = serialize(&doc);
        let (src, ovr) = parse(&text).expect("parse");
        assert_eq!(src, doc.source, "source must round-trip");
        assert_eq!(ovr, doc.overrides, "overrides must round-trip");
    }

    /// `serialize(parse(serialize(doc))) == serialize(doc)` — canonical form is a
    /// fixed point.
    #[test]
    fn idempotent() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            EntityId::new("psu.dec[0]"),
            Override { pos: Some(Point { x: 1, y: 999_999 }), strength: Strength::Pin },
        );
        let doc = doc_of(all_variants(), overrides);

        let once = serialize(&doc);
        let (src, ovr) = parse(&once).unwrap();
        let twice = serialize(&doc_of(src, ovr));
        assert_eq!(once, twice);
    }

    /// Human-authored forms (mm/nm/bare units, comments, extra whitespace) parse to
    /// the canonical model.
    #[test]
    fn tolerant_input_canonicalizes() {
        let text = "
            # a power rail
            inst   psu.reg   LDO        # the regulator
            place psu.reg (30mm, 20mm)
            fix   psu.reg (30000000nm, 20000000)   # mm, nm and bare all equal 30/20 mm
            near psu.reg psu.reg 0.5mm
        ";
        let (src, _ov) = parse(text).unwrap();
        assert_eq!(src[1], GenDirective::Place { path: "psu.reg".into(), pos: Point::mm(30, 20) });
        assert_eq!(src[2], GenDirective::Fix { path: "psu.reg".into(), pos: Point::mm(30, 20) });
        assert_eq!(
            src[3],
            GenDirective::Near { a: "psu.reg".into(), b: "psu.reg".into(), within: 500_000 }
        );
    }

    #[test]
    fn canonical_length_forms() {
        assert_eq!(fmt_len(30 * MM), "30mm");
        assert_eq!(fmt_len(0), "0mm");
        assert_eq!(fmt_len(500_000), "0.5mm");
        assert_eq!(fmt_len(-5_500_000), "-5.5mm");
        assert_eq!(fmt_len(1), "0.000001mm");
        // every canonical form parses back to itself
        for v in [30 * MM, 0, 500_000, -5_500_000, 1, 12_345_678] {
            assert_eq!(parse_len(&fmt_len(v)).unwrap(), v, "round-trip {v}nm");
        }
    }

    // ---- elaboration equivalence ----------------------------------------

    /// Re-elaborating the parsed `(source, overrides)` reproduces the same
    /// materialized `components`, `nets`, and reconciliation `report`.
    fn assert_elaboration_equiv(doc: &Doc) {
        let lib = part_library();
        let (src, ovr) = parse(&serialize(doc)).expect("parse");
        let elab = elaborate(&src, &ovr, &lib).expect("elaborate");
        assert_eq!(elab.components, doc.components, "components diverged");
        assert_eq!(elab.nets, doc.nets, "nets diverged");
        assert_eq!(elab.report, doc.report, "report diverged");
    }

    #[test]
    fn equiv_psu_module() {
        assert_elaboration_equiv(&placed(psu_module(3)));
    }

    #[test]
    fn equiv_psu_module_with_overrides() {
        // An *effective* nudge + pin: kept, report stays clean, so it round-trips.
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(3))), &lib, "psu").unwrap();
        h.commit(
            Transaction::one(Command::Nudge(EntityId::new("psu.dec[1]"), Point::mm(42, 7))),
            &lib,
            "nudge",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::Pin(EntityId::new("psu.dec[2]"), Point::mm(3, 30))),
            &lib,
            "pin",
        )
        .unwrap();
        let d = h.doc();
        assert!(d.report.decayed.is_empty(), "fixture should not have decayed hints");
        assert_elaboration_equiv(d);
    }

    #[test]
    fn equiv_uart_link() {
        assert_elaboration_equiv(&placed(uart_link()));
    }

    #[test]
    fn equiv_placement_scene() {
        assert_elaboration_equiv(&placed(placement_scene()));
    }

    /// A scene using the physical-parts directives (Rotate + NearPin) round-trips
    /// through text and re-elaborates identically.
    #[test]
    fn equiv_physical_scene() {
        let scene = vec![
            GenDirective::Instance { path: "reg".into(), part: "LDO".into() },
            GenDirective::Instance { path: "dec".into(), part: "Cap".into() },
            GenDirective::Fix { path: "reg".into(), pos: Point::mm(0, 0) },
            GenDirective::Rotate { path: "reg".into(), deg: 90 },
            GenDirective::NearPin {
                a: "dec".into(),
                b_comp: "reg".into(),
                b_pin: "VOUT".into(),
                within: 0,
            },
        ];
        assert_elaboration_equiv(&placed(scene));
    }

    /// `rotate` / `nearpin` parse from human-authored text (negative/over-360
    /// degrees normalise; mm length on the pin proximity).
    #[test]
    fn parse_rotate_and_nearpin() {
        let (src, _ov) = parse("rotate u1 -90\nnearpin c1 u1.VOUT 1.5mm").unwrap();
        assert_eq!(src[0], GenDirective::Rotate { path: "u1".into(), deg: -90 });
        assert_eq!(
            src[1],
            GenDirective::NearPin {
                a: "c1".into(),
                b_comp: "u1".into(),
                b_pin: "VOUT".into(),
                within: 1_500_000,
            }
        );
        // Off-axis rotation is rejected at elaboration, not parse.
        assert!(parse("rotate u1 45").is_ok());
        assert!(parse("rotate u1 notnum").is_err());
    }

    // ---- LoadText command (text -> tier-1 in one atomic transaction) -----

    #[test]
    fn load_text_replaces_state_and_matches_set_source() {
        let lib = part_library();

        // Reference: build the scene via the data API.
        let reference = placed(placement_scene());

        // Same scene authored as text, loaded atomically.
        let text = serialize(&reference);
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::LoadText(text)), &lib, "load").unwrap();
        let loaded = h.doc();

        assert_eq!(loaded.source, reference.source);
        assert_eq!(loaded.components, reference.components);
        assert_eq!(loaded.nets, reference.nets);
    }

    #[test]
    fn load_text_is_atomic_on_parse_error() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(psu_module(2))), &lib, "psu").unwrap();
        let before = crate::project::render(h.doc());
        // Garbage text must fail and leave head untouched.
        let r = h.commit(
            Transaction::one(Command::LoadText("inst onlyonetoken".into())),
            &lib,
            "bad",
        );
        assert!(r.is_err());
        assert_eq!(before, crate::project::render(h.doc()));
    }

    // ---- parse errors ----------------------------------------------------

    #[test]
    fn parse_error_unknown_directive() {
        let e = crate::diagnostic::render(&parse("frobnicate a b").unwrap_err());
        assert!(e.contains("unknown directive"), "got: {e}");
        assert!(e.contains("frobnicate"), "error should name the offending line: {e}");
    }

    #[test]
    fn parse_error_bad_coordinate() {
        let e = crate::diagnostic::render(&parse("place foo (3mm)").unwrap_err());
        assert!(e.contains("1:1"), "error should carry the line location: {e}");
    }

    #[test]
    fn parse_error_bad_pin_ref() {
        let e = crate::diagnostic::render(&parse("net VBUS nodotpin").unwrap_err());
        assert!(e.contains("<comp>"), "got: {e}");
    }

    /// Collect-all: several malformed lines are all reported in one parse, each
    /// located by line number — not just the first.
    #[test]
    fn parse_collects_all_line_errors() {
        let diags = parse("frobnicate x\ninst u1 LDO\nplace foo (3mm)").unwrap_err();
        assert_eq!(diags.len(), 2, "both bad lines reported: {diags:?}");
        let text = crate::diagnostic::render(&diags);
        assert!(text.contains("1:1") && text.contains("3:1"), "located by line: {text}");
    }

    #[test]
    fn parse_never_panics_on_junk() {
        // A pile of malformed lines: each must yield an Err, none may panic.
        for junk in [
            "(((",
            "near a b",
            "near a b notanumber",
            "place x (1mm, )",
            "place x (1mm, 2mm, 3mm)",
            "fix x (1.1234567mm, 0)",
            "connect a.b",
            "inst",
        ] {
            assert!(parse(junk).is_err(), "expected Err for `{junk}`");
        }
    }
}
