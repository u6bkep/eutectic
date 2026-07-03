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
use crate::doc::{Doc, MM, Nm, Orient, Override, Point, Strength};
use crate::elaborate::{GenDirective, RegionDecl, Source, board_rect};
use crate::geom::{KeepoutKind, Material, Path, Role, Seg, Shape2D, Slab, ZRange};
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
            // Serialized as a path (`board <p> [arc <mid> <end>] <p> ...`); an arc edge
            // emits `arc <mid> <end>`. The rect shorthand `boardrect <min> <max>` is
            // parse-only sugar. Corner radius (Minkowski-inflated outlines) is not yet
            // serialized — a noted follow-up.
            format!("board {}", fmt_path(outline.path()))
        }
        GenDirective::Cutout { shape } => {
            format!("cutout {}", fmt_path(shape.path()))
        }
        GenDirective::Region(r) => {
            // `region <role> [net=<n>] layer=<slab> <p> [arc <mid> <end>] <p> ...`.
            // `layer` is a slab name (Decision 13), emitted verbatim. Corner radius is
            // not serialized (same noted follow-up as board/cutout).
            let mut s = format!("region {}", role_token(&r.role));
            if let Some(n) = &r.net {
                s.push_str(&format!(" net={n}"));
            }
            s.push_str(&format!(" layer={}", r.layer));
            s.push(' ');
            s.push_str(&fmt_path(r.shape.path()));
            s
        }
        GenDirective::Slab(s) => {
            // `slab <name> <z_lo> <z_hi> <role> [material]`. Role uses the same total
            // `role_token` as `region` (so Substrate etc. serialise fine); material is an
            // optional bare name.
            let mut out = format!(
                "slab {} {} {} {}",
                s.name,
                fmt_len(s.z.lo),
                fmt_len(s.z.hi),
                role_token(&s.role)
            );
            if let Some(m) = &s.material {
                out.push(' ');
                out.push_str(&m.name);
            }
            out
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
        GenDirective::Rotate { path, orient } => {
            // Readable `<deg> [bottom]` for the 8 board-plane orientations; the exact
            // `quat=(w,x,y,z)` for any other (arbitrary-angle) rotation.
            let cardinal = [
                (0, false),
                (90, false),
                (180, false),
                (270, false),
                (0, true),
                (90, true),
                (180, true),
                (270, true),
            ]
            .into_iter()
            .find(|&(d, b)| {
                let c = Orient::from_deg(d).unwrap();
                let c = if b { c.flipped() } else { c };
                orient.same_rotation(c)
            });
            match cardinal {
                Some((d, b)) => format!("rotate {path} {d}{}", if b { " bottom" } else { "" }),
                None => format!(
                    "rotate {path} quat=({},{},{},{})",
                    orient.w, orient.x, orient.y, orient.z
                ),
            }
        }
        GenDirective::NearPin {
            a,
            b_comp,
            b_pin,
            within,
        } => {
            format!("nearpin {a} {b_comp}.{b_pin} {}", fmt_len(*within))
        }
        GenDirective::Text {
            string,
            at,
            height,
            layer,
            orient,
        } => {
            // `text "<string>" (x,y) h=<len> layer=<slab> [rot=<deg> | rotq=(w,x,y,z)]`.
            // `layer` is a slab name (Decision 13), emitted verbatim. The string is
            // double-quoted (may contain spaces). Identity orientation is omitted; a
            // cardinal about-z rotation serialises readably as `rot=<deg>`, any other
            // rotation as the exact `rotq=(w,x,y,z)` (so it round-trips). The
            // double-quote/backslash escaping of arbitrary strings is a noted follow-up.
            let mut s = format!(
                "text \"{string}\" {} h={} layer={}",
                fmt_point(*at),
                fmt_len(*height),
                layer,
            );
            if *orient != Orient::IDENTITY {
                let cardinal = [0, 90, 180, 270]
                    .into_iter()
                    .find(|&d| orient.same_rotation(Orient::from_deg(d).unwrap()));
                match cardinal {
                    Some(d) => s.push_str(&format!(" rot={d}")),
                    None => s.push_str(&format!(
                        " rotq=({},{},{},{})",
                        orient.w, orient.x, orient.y, orient.z
                    )),
                }
            }
            s
        }
    }
}

fn fmt_point(p: Point) -> String {
    format!("({}, {})", fmt_len(p.x), fmt_len(p.y))
}

/// Render a skeleton [`Path`] as a coordinate list: `start`, then one coordinate per
/// straight edge, and `arc <mid> <end>` per circular-arc edge. The inverse of
/// [`extract_path`]. (The closing edge of a polygon is implicit, as in the geometry.)
fn fmt_path(path: &Path) -> String {
    let mut toks = vec![fmt_point(path.start)];
    for seg in &path.segs {
        match seg {
            Seg::Line { end } => toks.push(fmt_point(*end)),
            Seg::Arc { mid, end } => {
                toks.push("arc".into());
                toks.push(fmt_point(*mid));
                toks.push(fmt_point(*end));
            }
            Seg::Quadratic { ctrl, end } => {
                toks.push("quad".into());
                toks.push(fmt_point(*ctrl));
                toks.push(fmt_point(*end));
            }
            Seg::Cubic { c1, c2, end } => {
                toks.push("cubic".into());
                toks.push(fmt_point(*c1));
                toks.push(fmt_point(*c2));
                toks.push(fmt_point(*end));
            }
        }
    }
    toks.join(" ")
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

/// Canonical text token for a region [`Role`]. Only the roles a `region` directive can
/// author round-trip here (conductor / void / keep-out by kind); other roles are
/// composed via footprints, not authored as standalone regions.
fn role_token(role: &Role) -> String {
    match role {
        Role::Conductor => "conductor".into(),
        Role::Void => "void".into(),
        Role::Keepout(k) => match k {
            KeepoutKind::Copper => "keepout".into(),
            KeepoutKind::Component => "keepout-component".into(),
            KeepoutKind::Drill => "keepout-drill".into(),
            KeepoutKind::Route => "keepout-route".into(),
        },
        // Not authorable as a region today; emit a stable token so serialization is
        // never lossy-by-panic (parse rejects these, so they never round-trip in).
        Role::Substrate => "substrate".into(),
        Role::Marking => "marking".into(),
        Role::Mask => "mask".into(),
        Role::Datum => "datum".into(),
    }
}

fn parse_role(tok: &str) -> Result<Role, String> {
    Ok(match tok {
        "conductor" => Role::Conductor,
        "void" => Role::Void,
        "keepout" => Role::Keepout(KeepoutKind::Copper),
        "keepout-component" => Role::Keepout(KeepoutKind::Component),
        "keepout-drill" => Role::Keepout(KeepoutKind::Drill),
        "keepout-route" => Role::Keepout(KeepoutKind::Route),
        other => {
            return Err(format!(
                "region: unknown role `{other}` (conductor | void | keepout[-component|-drill|-route])"
            ));
        }
    })
}

/// Parse a `rot=` degree value into an [`Orient`] (about z): an integer cardinal uses
/// the tiny exact quaternion, any other finite angle lowers once (at parse) to a scaled
/// quaternion (same lowering as the `rotate` directive). Mirrors that directive's angle
/// handling; text-side flipping (`bottom`) is a follow-up.
fn parse_rot_deg(r: &str) -> Result<Orient, String> {
    if let Ok(d) = r.parse::<i32>() {
        Ok(Orient::from_deg(d).unwrap_or_else(|| Orient::from_angle_deg(d as f64)))
    } else {
        let deg: f64 = r
            .parse()
            .map_err(|_| format!("`{r}` is not a number of degrees"))?;
        if !deg.is_finite() {
            return Err(format!("rotation angle `{r}` must be finite"));
        }
        Ok(Orient::from_angle_deg(deg))
    }
}

/// Parse a `rotq=` value `(w,x,y,z)` into an exact integer-quaternion [`Orient`] (the
/// canonical serialised form for a non-cardinal text rotation). The all-zero quaternion
/// is rejected (not a rotation).
fn parse_quat_tok(q: &str) -> Result<Orient, String> {
    let inner = q
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or("rotq must be written rotq=(w,x,y,z)")?;
    let n: Vec<&str> = inner.split(',').collect();
    if n.len() != 4 {
        return Err("rotq needs four integer components: rotq=(w,x,y,z)".into());
    }
    let pi = |t: &str| {
        t.trim()
            .parse::<i64>()
            .map_err(|_| format!("`{}` is not an integer", t.trim()))
    };
    let o = Orient {
        w: pi(n[0])?,
        x: pi(n[1])?,
        y: pi(n[2])?,
        z: pi(n[3])?,
    };
    if (o.w, o.x, o.y, o.z) == (0, 0, 0, 0) {
        return Err("rotq=(0,0,0,0) is not a rotation".into());
    }
    Ok(o)
}

// ----------------------------------------------------------------------------
// Parse
// ----------------------------------------------------------------------------

/// The code part of a line: everything before the first `#` that is **not** inside a
/// double-quoted string. Quote-aware so a text label may contain `#` (`text "A#1" …`).
fn strip_comment(raw: &str) -> &str {
    let mut in_str = false;
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &raw[..i],
            _ => {}
        }
    }
    raw
}

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
        // Strip comments and surrounding whitespace. The `#` scan is **quote-aware**:
        // a `#` inside a double-quoted string (a text label) is literal, not a comment,
        // so `text "A#1" …` round-trips. (Embedded `"`/`\` in a string still need
        // escaping — a documented follow-up.)
        let line = strip_comment(raw).trim();
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
                Location::Span {
                    line: lineno,
                    col: 1,
                },
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
            let path = extract_path(rest)?;
            if !path_is_polygon(&path) {
                return Err(
                    "board needs ≥3 outline points (or an arc edge): board (x,y) (x,y) (x,y) ..."
                        .into(),
                );
            }
            Item::Directive(GenDirective::Board {
                outline: Shape2D::polygon_path(path, 0),
            })
        }
        "boardrect" => {
            let pts = extract_points(rest)?;
            if pts.len() != 2 {
                return Err(
                    "boardrect needs two corners: boardrect (minx,miny) (maxx,maxy)".into(),
                );
            }
            Item::Directive(board_rect(pts[0], pts[1]))
        }
        "cutout" => {
            let path = extract_path(rest)?;
            if !path_is_polygon(&path) {
                return Err(
                    "cutout needs ≥3 points (or an arc edge): cutout (x,y) (x,y) (x,y) ...".into(),
                );
            }
            Item::Directive(GenDirective::Cutout {
                shape: Shape2D::polygon_path(path, 0),
            })
        }
        "region" => {
            // `region <role> [net=<n>] [layer=<slab>] (x,y) (x,y) (x,y) ...`. Prefix
            // tokens precede the first point; role is required, net/layer optional.
            // `layer` is a slab name (Decision 13), stored verbatim and resolved
            // against the stackup at elaboration; it defaults to `F.Cu`.
            let open = rest.find('(').ok_or(
                "region needs ≥3 points: region <role> [net=..] [layer=..] (x,y) (x,y) (x,y) ...",
            )?;
            let (prefix, ptspart) = rest.split_at(open);
            let path = extract_path(ptspart)?;
            if !path_is_polygon(&path) {
                return Err("region needs ≥3 points (or an arc edge): region <role> [net=..] [layer=..] (x,y) ...".into());
            }
            let mut role: Option<Role> = None;
            let mut net: Option<String> = None;
            let mut layer = "F.Cu".to_string();
            for tok in prefix.split_whitespace() {
                if let Some(n) = tok.strip_prefix("net=") {
                    net = Some(n.to_string());
                } else if let Some(l) = tok.strip_prefix("layer=") {
                    layer = l.to_string();
                } else if role.is_none() {
                    role = Some(parse_role(tok)?);
                } else {
                    return Err(format!("region: unexpected token `{tok}`"));
                }
            }
            let role = role.ok_or("region needs a role: conductor | void | keepout[-kind]")?;
            Item::Directive(GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon_path(path, 0),
                role,
                net,
                layer,
            }))
        }
        "slab" => {
            // `slab <name> <z_lo> <z_hi> <role> [material]`. z's are lengths (mm/nm via
            // `parse_len`); role uses `parse_role` widened with the slab-only roles a
            // real stackup needs (`substrate`, `marking` for silk, `mask` for solder
            // mask — `region` keeps its narrower vocabulary); material is an optional
            // bare name lowered to `Material::named`.
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.len() < 4 || toks.len() > 5 {
                return Err("slab <name> <z_lo> <z_hi> <role> [material]".into());
            }
            let z = ZRange::new(parse_len(toks[1])?, parse_len(toks[2])?);
            let role = match toks[3] {
                "substrate" => Role::Substrate,
                "marking" => Role::Marking,
                "mask" => Role::Mask,
                "datum" => Role::Datum,
                other => parse_role(other)?,
            };
            let material = toks.get(4).map(|m| Material::named(m));
            Item::Directive(GenDirective::Slab(Slab {
                name: toks[0].to_string(),
                z,
                role,
                material,
            }))
        }
        "near" => {
            let (a, b, len) = two_tokens_and_len(rest, "near <a> <b> <len>")?;
            Item::Directive(GenDirective::Near { a, b, within: len })
        }
        "minsep" => {
            let (a, b, len) = two_tokens_and_len(rest, "minsep <a> <b> <len>")?;
            Item::Directive(GenDirective::MinSep { a, b, gap: len })
        }
        "alignx" => Item::Directive(GenDirective::AlignX {
            nodes: node_list(rest, "alignx")?,
        }),
        "aligny" => Item::Directive(GenDirective::AlignY {
            nodes: node_list(rest, "aligny")?,
        }),
        "rotate" => {
            // `rotate <path> <angle> [bottom]`  |  `rotate <path> quat=(w,x,y,z)`.
            // Split the path off first so a `quat=(...)` with internal whitespace
            // (`quat=(1, 0, 0, 1)`) survives.
            let rest = rest.trim();
            let (path, spec) = rest
                .split_once(char::is_whitespace)
                .map(|(p, s)| (p.to_string(), s.trim()))
                .ok_or("rotate <path> <deg> [bottom]  |  rotate <path> quat=(w,x,y,z)")?;
            let orient = if let Some(q) = spec.strip_prefix("quat=") {
                let inner = q
                    .trim()
                    .strip_prefix('(')
                    .and_then(|s| s.strip_suffix(')'))
                    .ok_or("quat must be written quat=(w,x,y,z)")?;
                let n: Vec<&str> = inner.split(',').collect();
                if n.len() != 4 {
                    return Err("quat needs four integer components: quat=(w,x,y,z)".into());
                }
                let pi = |t: &str| {
                    t.trim()
                        .parse::<i64>()
                        .map_err(|_| format!("`{}` is not an integer", t.trim()))
                };
                let o = Orient {
                    w: pi(n[0])?,
                    x: pi(n[1])?,
                    y: pi(n[2])?,
                    z: pi(n[3])?,
                };
                if (o.w, o.x, o.y, o.z) == (0, 0, 0, 0) {
                    return Err("quat=(0,0,0,0) is not a rotation".into());
                }
                o
            } else {
                // `<angle> [bottom]` — an integer cardinal uses the tiny exact quaternion;
                // any other (finite) angle lowers once, at parse, to a scaled quaternion.
                let toks: Vec<&str> = spec.split_whitespace().collect();
                let (angle_s, bottom) = match toks.as_slice() {
                    [a] => (*a, false),
                    [a, "bottom"] => (*a, true),
                    _ => return Err("rotate <path> <deg> [bottom]".into()),
                };
                let mut o = if let Ok(d) = angle_s.parse::<i32>() {
                    Orient::from_deg(d).unwrap_or_else(|| Orient::from_angle_deg(d as f64))
                } else {
                    let deg: f64 = angle_s
                        .parse()
                        .map_err(|_| format!("`{angle_s}` is not a number of degrees"))?;
                    if !deg.is_finite() {
                        return Err(format!("rotation angle `{angle_s}` must be finite"));
                    }
                    Orient::from_angle_deg(deg)
                };
                if bottom {
                    o = o.flipped();
                }
                o
            };
            Item::Directive(GenDirective::Rotate { path, orient })
        }
        "nearpin" => {
            let (a, bpin, len) = two_tokens_and_len(rest, "nearpin <a> <bComp>.<bPin> <len>")?;
            let (b_comp, b_pin) = split_last_dot(&bpin, "pin")?;
            Item::Directive(GenDirective::NearPin {
                a,
                b_comp,
                b_pin,
                within: len,
            })
        }
        "text" => {
            // `text "<string>" (x,y) h=<len> [layer=<slab>] [rot=<deg> | rotq=(w,x,y,z)]`.
            // Pull the double-quoted string off first (it may contain spaces), then the
            // coordinate, then the remaining `key=value` tokens. `layer` is a slab name
            // (Decision 13), stored verbatim; it defaults to `F.SilkS`.
            const USAGE: &str =
                "text \"<string>\" (x,y) h=<len> [layer=<slab>] [rot=<deg> | rotq=(w,x,y,z)]";
            let q1 = rest.find('"').ok_or(USAGE)?;
            let q2 = rest[q1 + 1..]
                .find('"')
                .map(|i| q1 + 1 + i)
                .ok_or("text: string is missing its closing quote")?;
            let string = rest[q1 + 1..q2].to_string();
            let after = rest[q2 + 1..].trim();
            // The single coordinate `(x, y)`.
            let open = after.find('(').ok_or(USAGE)?;
            let close = after[open..]
                .find(')')
                .map(|i| open + i)
                .ok_or("unbalanced '(' in coordinate")?;
            let pts = extract_points(&after[open..=close])?;
            if pts.len() != 1 {
                return Err("text needs exactly one coordinate `(x, y)`".into());
            }
            let at = pts[0];
            // Trailing key=value tokens: h= (required), layer= (optional, default
            // F.SilkS), rot=/rotq=.
            let mut height: Option<Nm> = None;
            let mut layer = "F.SilkS".to_string();
            let mut orient = Orient::IDENTITY;
            for tok in after[close + 1..].split_whitespace() {
                if let Some(h) = tok.strip_prefix("h=") {
                    height = Some(parse_len(h)?);
                } else if let Some(l) = tok.strip_prefix("layer=") {
                    layer = l.to_string();
                } else if let Some(q) = tok.strip_prefix("rotq=") {
                    orient = parse_quat_tok(q)?;
                } else if let Some(r) = tok.strip_prefix("rot=") {
                    orient = parse_rot_deg(r)?;
                } else {
                    return Err(format!("text: unexpected token `{tok}`"));
                }
            }
            Item::Directive(GenDirective::Text {
                string,
                at,
                height: height.ok_or("text needs h=<len>")?,
                layer,
                orient,
            })
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
                return Err(
                    "net needs a name and at least one pin: net <name> <comp>.<pin> ...".into(),
                );
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
            let strength = if kw == "pin" {
                Strength::Pin
            } else {
                Strength::Hint
            };
            Item::Override(
                EntityId::new(path),
                Override {
                    pos: Some(pos),
                    strength,
                },
            )
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
    Ok((
        toks[0].to_string(),
        toks[1].to_string(),
        parse_len(toks[2])?,
    ))
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
/// Parse a skeleton [`Path`] from a coordinate list with optional `arc` markers:
/// `(x,y) [arc (mx,my) (ex,ey)] (x,y) ...`. A bare coordinate is a straight edge to
/// that point; `arc <mid> <end>` is a circular arc through the previous point, `mid`,
/// and `end`. The first token must be a coordinate (the start). The inverse of
/// [`fmt_path`]; a list with no `arc` markers yields an all-`Line` path (backward
/// compatible with the old point-only grammar).
fn extract_path(s: &str) -> Result<Path, String> {
    enum PTok {
        Coord(Point),
        Arc,
        Quad,
        Cubic,
    }
    let mut toks = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('(') {
            let close = after.find(')').ok_or("unbalanced '(' in coordinate")?;
            let (xs, ys) = after[..close]
                .split_once(',')
                .ok_or("coordinate must be `(x, y)`")?;
            toks.push(PTok::Coord(Point {
                x: parse_len(xs.trim())?,
                y: parse_len(ys.trim())?,
            }));
            rest = after[close + 1..].trim_start();
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '(')
                .unwrap_or(rest.len());
            match &rest[..end] {
                "arc" => toks.push(PTok::Arc),
                "quad" => toks.push(PTok::Quad),
                "cubic" => toks.push(PTok::Cubic),
                other => {
                    return Err(format!(
                        "unexpected token `{other}` in path (expected a coordinate, `arc`, `quad`, or `cubic`)"
                    ));
                }
            }
            rest = rest[end..].trim_start();
        }
    }
    let mut it = toks.into_iter();
    let start = match it.next() {
        Some(PTok::Coord(p)) => p,
        Some(_) => {
            return Err("a path must begin with a coordinate, not `arc`/`quad`/`cubic`".into());
        }
        None => return Err("expected a coordinate `(x, y)`".into()),
    };
    let mut segs = Vec::new();
    while let Some(t) = it.next() {
        match t {
            PTok::Coord(p) => segs.push(Seg::Line { end: p }),
            PTok::Arc => {
                let mid = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err("`arc` needs a midpoint coordinate: arc (mx,my) (ex,ey)".into());
                    }
                };
                let end = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => return Err("`arc` needs an endpoint coordinate after the midpoint".into()),
                };
                segs.push(Seg::Arc { mid, end });
            }
            PTok::Quad => {
                let ctrl = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err(
                            "`quad` needs a control coordinate: quad (cx,cy) (ex,ey)".into()
                        );
                    }
                };
                let end = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => return Err("`quad` needs an endpoint coordinate after the control".into()),
                };
                segs.push(Seg::Quadratic { ctrl, end });
            }
            PTok::Cubic => {
                let c1 = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err(
                            "`cubic` needs two controls + an endpoint: cubic (c1) (c2) (end)"
                                .into(),
                        );
                    }
                };
                let c2 = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => return Err("`cubic` needs a second control coordinate".into()),
                };
                let end = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err(
                            "`cubic` needs an endpoint coordinate after the controls".into()
                        );
                    }
                };
                segs.push(Seg::Cubic { c1, c2, end });
            }
        }
    }
    Ok(Path { start, segs })
}

/// Does the path enclose area as a polygon — ≥ 3 corners, or ≥ 1 corner closed by a
/// curved edge (a half-disc is a valid 2-corner arc polygon; a Bézier blob likewise)?
fn path_is_polygon(path: &Path) -> bool {
    let has_curve = path.segs.iter().any(|s| {
        matches!(
            s,
            Seg::Arc { .. } | Seg::Quadratic { .. } | Seg::Cubic { .. }
        )
    });
    let corners = 1 + path.segs.len();
    corners >= 3 || (has_curve && corners >= 2)
}

fn extract_points(s: &str) -> Result<Vec<Point>, String> {
    let mut pts = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let close_rel = rest[open..]
            .find(')')
            .ok_or("unbalanced '(' in coordinate")?;
        let close = open + close_rel;
        let inner = &rest[open + 1..close];
        let (xs, ys) = inner.split_once(',').ok_or("coordinate must be `(x, y)`")?;
        pts.push(Point {
            x: parse_len(xs.trim())?,
            y: parse_len(ys.trim())?,
        });
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
        whole_str
            .parse()
            .map_err(|_| format!("`{body}mm` has a non-numeric whole part"))?
    };
    if frac_str.len() > 6 {
        return Err(format!(
            "`{body}mm` has sub-nanometre precision (max 6 decimal places)"
        ));
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
            GenDirective::Instance {
                path: "mcu".into(),
                part: "MCU".into(),
            },
            GenDirective::Instance {
                path: "sens".into(),
                part: "Sensor".into(),
            },
            GenDirective::ConnectInterface {
                a: ("mcu".into(), "uart".into()),
                b: ("sens".into(), "uart".into()),
            },
        ]
    }

    /// A scene exercising Board / Near / MinSep / AlignY / Fix.
    fn placement_scene() -> Source {
        vec![
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
            },
            GenDirective::Instance {
                path: "c1".into(),
                part: "Cap".into(),
            },
            GenDirective::Instance {
                path: "c2".into(),
                part: "Cap".into(),
            },
            GenDirective::Fix {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            board_rect(Point::mm(0, 0), Point::mm(50, 50)),
            GenDirective::Near {
                a: "c1".into(),
                b: "reg".into(),
                within: 3 * MM,
            },
            GenDirective::Near {
                a: "c2".into(),
                b: "reg".into(),
                within: 3 * MM,
            },
            GenDirective::MinSep {
                a: "c1".into(),
                b: "c2".into(),
                gap: 4 * MM,
            },
            GenDirective::AlignY {
                nodes: vec!["c1".into(), "c2".into()],
            },
        ]
    }

    /// A hand-built source touching *every* GenDirective variant.
    fn all_variants() -> Source {
        vec![
            GenDirective::Instance {
                path: "psu.reg".into(),
                part: "LDO".into(),
            },
            GenDirective::Instance {
                path: "psu.dec[0]".into(),
                part: "Cap".into(),
            },
            GenDirective::Instance {
                path: "mcu".into(),
                part: "MCU".into(),
            },
            GenDirective::Instance {
                path: "sens".into(),
                part: "Sensor".into(),
            },
            GenDirective::Place {
                path: "psu.dec[0]".into(),
                pos: Point::mm(5, 5),
            },
            GenDirective::Fix {
                path: "psu.reg".into(),
                pos: Point {
                    x: 1,
                    y: -2_500_000,
                },
            },
            board_rect(Point::mm(0, 0), Point::mm(50, 50)),
            GenDirective::Cutout {
                shape: Shape2D::polygon(vec![
                    Point::mm(20, 20),
                    Point::mm(30, 20),
                    Point::mm(25, 30),
                ]),
            },
            // A net-bound copper pour on the bottom layer, and a component keep-out.
            GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon(vec![
                    Point::mm(0, 0),
                    Point::mm(50, 0),
                    Point::mm(50, 50),
                    Point::mm(0, 50),
                ]),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "B.Cu".into(),
            }),
            GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon(vec![
                    Point::mm(10, 10),
                    Point::mm(15, 10),
                    Point::mm(15, 15),
                ]),
                role: Role::Keepout(KeepoutKind::Component),
                net: None,
                layer: "F.Cu".into(),
            }),
            // An authored 3-slab stackup: conductor / substrate / conductor, exercising
            // the substrate role and both material-present and material-absent slabs.
            GenDirective::Slab(Slab {
                name: "B.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: Some(Material::named("copper")),
            }),
            GenDirective::Slab(Slab {
                name: "core".into(),
                z: ZRange::new(35_000, 1_565_000),
                role: Role::Substrate,
                material: None,
            }),
            GenDirective::Slab(Slab {
                name: "F.Cu".into(),
                z: ZRange::new(1_565_000, 1_600_000),
                role: Role::Conductor,
                material: Some(Material::named("copper")),
            }),
            // A zero-height fab datum slab: `datum` role authorable, `lo == hi` z
            // (Decision 15). Round-trips and flows through the stackup like any slab.
            GenDirective::Slab(Slab {
                name: "F.Fab".into(),
                z: ZRange::new(1_600_000, 1_600_000),
                role: Role::Datum,
                material: None,
            }),
            GenDirective::Near {
                a: "psu.dec[0]".into(),
                b: "psu.reg".into(),
                within: 2 * MM,
            },
            GenDirective::MinSep {
                a: "psu.dec[0]".into(),
                b: "mcu".into(),
                gap: MM,
            },
            GenDirective::AlignX {
                nodes: vec!["psu.reg".into(), "psu.dec[0]".into()],
            },
            GenDirective::AlignY {
                nodes: vec!["mcu".into(), "sens".into()],
            },
            GenDirective::Rotate {
                path: "psu.reg".into(),
                orient: Orient::from_deg(90).unwrap(),
            },
            GenDirective::NearPin {
                a: "psu.dec[0]".into(),
                b_comp: "psu.reg".into(),
                b_pin: "VOUT".into(),
                within: 2 * MM,
            },
            // Board text (silk): an identity-oriented label and a cardinally-rotated one.
            GenDirective::Text {
                string: "REF 1".into(),
                at: Point::mm(2, 40),
                height: MM,
                layer: "F.SilkS".into(),
                orient: Orient::IDENTITY,
            },
            GenDirective::Text {
                string: "B1".into(),
                at: Point::mm(10, 40),
                height: 800_000,
                layer: "B.SilkS".into(),
                orient: Orient::from_deg(90).unwrap(),
            },
            GenDirective::ConnectInterface {
                a: ("mcu".into(), "uart".into()),
                b: ("sens".into(), "uart".into()),
            },
            GenDirective::ConnectPins {
                net: "VBUS".into(),
                pins: vec![
                    ("psu.reg".into(), "VOUT".into()),
                    ("psu.dec[0]".into(), "p1".into()),
                ],
            },
            // GND is connected so the conductor pour above references a real net.
            GenDirective::ConnectPins {
                net: "GND".into(),
                pins: vec![("psu.dec[0]".into(), "p2".into())],
            },
            GenDirective::NoConnect {
                pins: vec![
                    ("psu.reg".into(), "GND".into()),
                    ("mcu".into(), "GPIO0".into()),
                ],
            },
        ]
    }

    fn doc_of(source: Source, overrides: BTreeMap<EntityId, Override>) -> Doc {
        Doc {
            source,
            overrides,
            ..Default::default()
        }
    }

    fn placed(src: Source) -> Doc {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
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
            Override {
                pos: Some(Point::mm(7, 3)),
                strength: Strength::Hint,
            },
        );
        overrides.insert(
            EntityId::new("mcu"),
            Override {
                pos: Some(Point {
                    x: 12_345_678,
                    y: -500_000,
                }),
                strength: Strength::Pin,
            },
        );
        let doc = doc_of(all_variants(), overrides);

        let text = serialize(&doc);
        let (src, ovr) = parse(&text).expect("parse");
        assert_eq!(src, doc.source, "source must round-trip");
        assert_eq!(ovr, doc.overrides, "overrides must round-trip");
    }

    /// A `slab` directive parses to the expected `Slab` (name, z's, role, optional
    /// material) and round-trips through `serialize`. Covers material-present,
    /// material-absent, and the `substrate` role (which `region` does not accept).
    #[test]
    fn slab_directive_parses_and_round_trips() {
        let text = "\
slab B.Cu 0mm 0.035mm conductor copper
slab core 0.035mm 1.565mm substrate
slab F.Cu 1.565mm 1.6mm conductor copper";
        let (src, _) = parse(text).expect("parse");
        assert_eq!(
            src,
            vec![
                GenDirective::Slab(Slab {
                    name: "B.Cu".into(),
                    z: ZRange::new(0, 35_000),
                    role: Role::Conductor,
                    material: Some(Material::named("copper")),
                }),
                GenDirective::Slab(Slab {
                    name: "core".into(),
                    z: ZRange::new(35_000, 1_565_000),
                    role: Role::Substrate,
                    material: None,
                }),
                GenDirective::Slab(Slab {
                    name: "F.Cu".into(),
                    z: ZRange::new(1_565_000, 1_600_000),
                    role: Role::Conductor,
                    material: Some(Material::named("copper")),
                }),
            ]
        );
        // Canonical serialization re-parses to the same source.
        let doc = doc_of(src.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc)).unwrap().0, src);
    }

    /// A region directive parses to the expected `RegionDecl` (role, net, layer, and
    /// points), and the inner-layer / keep-out-kind tokens round-trip.
    #[test]
    fn region_directive_parses_and_round_trips() {
        let text = "\
region conductor net=GND layer=B.Cu (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)
region keepout-drill layer=In2.Cu (1mm, 1mm) (2mm, 1mm) (2mm, 2mm)";
        let (src, _) = parse(text).expect("parse");
        assert_eq!(
            src[0],
            GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon(vec![
                    Point::mm(0, 0),
                    Point::mm(10, 0),
                    Point::mm(10, 10),
                    Point::mm(0, 10),
                ]),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "B.Cu".into(),
            })
        );
        assert_eq!(
            src[1],
            GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon(vec![Point::mm(1, 1), Point::mm(2, 1), Point::mm(2, 2)]),
                role: Role::Keepout(KeepoutKind::Drill),
                net: None,
                layer: "In2.Cu".into(), // "In2.Cu" is 1-based ⇒ Inner(1).
            })
        );
        // Canonical serialization re-parses to the same source.
        let doc = doc_of(src.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc)).unwrap().0, src);
    }

    /// A `text` directive parses to the expected `GenDirective::Text` and round-trips,
    /// with and without `rot=`. A quoted string containing a space survives intact.
    #[test]
    fn text_directive_parses_and_round_trips() {
        let text = "\
text \"R12\" (0mm, 0mm) h=1mm layer=F.SilkS
text \"VAL 3V3\" (2mm, 5mm) h=0.8mm layer=B.SilkS rot=90";
        let (src, _) = parse(text).expect("parse");
        assert_eq!(
            src[0],
            GenDirective::Text {
                string: "R12".into(),
                at: Point::mm(0, 0),
                height: MM,
                layer: "F.SilkS".into(),
                orient: Orient::IDENTITY,
            }
        );
        assert_eq!(
            src[1],
            GenDirective::Text {
                string: "VAL 3V3".into(), // a quoted string with a space round-trips
                at: Point::mm(2, 5),
                height: 800_000,
                layer: "B.SilkS".into(),
                orient: Orient::from_deg(90).unwrap(),
            }
        );
        // Canonical serialization re-parses identically (silk tokens + rot survive).
        let doc = doc_of(src.clone(), BTreeMap::new());
        let canon = serialize(&doc);
        assert!(canon.contains("layer=F.SilkS"), "silk token:\n{canon}");
        assert!(canon.contains("rot=90"), "cardinal rot token:\n{canon}");
        assert_eq!(parse(&canon).unwrap().0, src);
    }

    #[test]
    fn text_string_may_contain_a_hash() {
        // A `#` inside a quoted text label is literal, not a comment (quote-aware strip),
        // so it round-trips. (`#` outside quotes still starts a comment.)
        let (src, _) =
            parse("text \"P#1\" (0mm, 0mm) h=1mm layer=F.SilkS  # a real comment").expect("parse");
        let GenDirective::Text { string, .. } = &src[0] else {
            panic!("expected text, got {:?}", src[0]);
        };
        assert_eq!(
            string, "P#1",
            "the in-string # survived; the trailing # was stripped"
        );
        let canon = serialize(&doc_of(src.clone(), BTreeMap::new()));
        assert_eq!(
            parse(&canon).unwrap().0,
            src,
            "round-trips with the # intact"
        );
    }

    /// A region/text `layer=` accepts an **arbitrary slab-name token** (Decision 13),
    /// stored verbatim and round-tripping exactly — including non-default names that no
    /// longer map to a copper ordinal. Also exercises the text `layer=` default
    /// (`F.SilkS`) and the region default (`F.Cu`).
    #[test]
    fn arbitrary_slab_names_round_trip() {
        let text = "\
region keepout layer=F.Fab (0mm, 0mm) (10mm, 0mm) (10mm, 10mm)
region conductor net=GND (0mm, 0mm) (5mm, 0mm) (5mm, 5mm)
text \"HELLO\" (1mm, 1mm) h=1mm layer=My.Custom.Layer
text \"WORLD\" (2mm, 2mm) h=1mm";
        let (src, _) = parse(text).expect("parse");
        // Verbatim storage of the authored names, and the two defaults.
        let GenDirective::Region(r0) = &src[0] else {
            panic!("region 0");
        };
        assert_eq!(
            r0.layer, "F.Fab",
            "arbitrary region slab name stored verbatim"
        );
        let GenDirective::Region(r1) = &src[1] else {
            panic!("region 1");
        };
        assert_eq!(r1.layer, "F.Cu", "region layer defaults to F.Cu");
        let GenDirective::Text { layer: l2, .. } = &src[2] else {
            panic!("text 2");
        };
        assert_eq!(
            l2, "My.Custom.Layer",
            "arbitrary text slab name stored verbatim"
        );
        let GenDirective::Text { layer: l3, .. } = &src[3] else {
            panic!("text 3");
        };
        assert_eq!(l3, "F.SilkS", "text layer defaults to F.SilkS");
        // Canonical serialization re-parses identically.
        let canon = serialize(&doc_of(src.clone(), BTreeMap::new()));
        assert!(
            canon.contains("layer=F.Fab"),
            "arbitrary name serialized:\n{canon}"
        );
        assert!(
            canon.contains("layer=My.Custom.Layer"),
            "verbatim:\n{canon}"
        );
        assert_eq!(
            parse(&canon).unwrap().0,
            src,
            "arbitrary slab names round-trip"
        );
    }

    /// `arc <mid> <end>` edges parse into `Seg::Arc`, mixed freely with straight edges,
    /// and survive a canonical round-trip. A half-disc board (2 corners closed by an
    /// arc) is accepted despite having < 3 corners.
    #[test]
    fn arc_edges_parse_and_round_trip() {
        let text = "\
board (-2mm, 0mm) arc (0mm, 2mm) (2mm, 0mm)
region conductor layer=F.Cu (0mm, 0mm) (4mm, 0mm) arc (5mm, 2mm) (4mm, 4mm) (0mm, 4mm)";
        let (src, _) = parse(text).expect("parse");
        // Board: a 2-corner arc polygon (half-disc).
        assert_eq!(
            src[0],
            GenDirective::Board {
                outline: Shape2D::polygon_path(
                    Path {
                        start: Point::mm(-2, 0),
                        segs: vec![Seg::Arc {
                            mid: Point::mm(0, 2),
                            end: Point::mm(2, 0)
                        }],
                    },
                    0,
                )
            }
        );
        // Region: straight edges with one arc edge among them.
        match &src[1] {
            GenDirective::Region(r) => assert_eq!(
                r.shape.path().segs,
                vec![
                    Seg::Line {
                        end: Point::mm(4, 0)
                    },
                    Seg::Arc {
                        mid: Point::mm(5, 2),
                        end: Point::mm(4, 4)
                    },
                    Seg::Line {
                        end: Point::mm(0, 4)
                    },
                ],
            ),
            other => panic!("expected a region, got {other:?}"),
        }
        // Canonical serialization re-parses to the same source (arc markers survive).
        let doc = doc_of(src.clone(), BTreeMap::new());
        let canon = serialize(&doc);
        assert!(
            canon.contains("arc ("),
            "serialized form carries `arc` markers:\n{canon}"
        );
        assert_eq!(parse(&canon).unwrap().0, src);
    }

    #[test]
    fn bezier_edges_parse_and_round_trip() {
        // A region with one quadratic and one cubic edge, mixed with straight edges.
        let text = "\
region conductor layer=F.Cu (0mm, 0mm) quad (2mm, 3mm) (4mm, 0mm) cubic (5mm, 2mm) (7mm, 2mm) (8mm, 0mm) (0mm, 4mm)";
        let (src, _) = parse(text).expect("parse");
        match &src[0] {
            GenDirective::Region(r) => assert_eq!(
                r.shape.path().segs,
                vec![
                    Seg::Quadratic {
                        ctrl: Point::mm(2, 3),
                        end: Point::mm(4, 0),
                    },
                    Seg::Cubic {
                        c1: Point::mm(5, 2),
                        c2: Point::mm(7, 2),
                        end: Point::mm(8, 0),
                    },
                    Seg::Line {
                        end: Point::mm(0, 4),
                    },
                ],
            ),
            other => panic!("expected a region, got {other:?}"),
        }
        // Canonical serialization re-parses identically (quad/cubic markers survive).
        let doc = doc_of(src.clone(), BTreeMap::new());
        let canon = serialize(&doc);
        assert!(
            canon.contains("quad (") && canon.contains("cubic ("),
            "markers:\n{canon}"
        );
        assert_eq!(parse(&canon).unwrap().0, src);
    }

    #[test]
    fn bezier_path_parse_errors_are_reported() {
        assert!(
            parse("board (0mm,0mm) cubic (1mm,1mm) (2mm,2mm)").is_err(),
            "cubic needs two controls AND an endpoint"
        );
        assert!(
            parse("board (0mm,0mm) quad (1mm,1mm)").is_err(),
            "quad needs a control AND an endpoint"
        );
    }

    #[test]
    fn arc_path_parse_errors_are_reported() {
        assert!(
            parse("board (0mm,0mm) arc (1mm,1mm)").is_err(),
            "arc needs mid AND end"
        );
        assert!(
            parse("board arc (0mm,0mm) (1mm,1mm)").is_err(),
            "path must start with a coord"
        );
        assert!(
            parse("board (0mm,0mm) bogus (1mm,1mm)").is_err(),
            "unknown path token"
        );
    }

    /// Regions are assembled by the shared reader and survive a real commit (they do
    /// not disturb elaboration — no fill/connectivity yet, just storage).
    #[test]
    fn regions_assemble_through_commit() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        let src = vec![
            board_rect(Point::mm(0, 0), Point::mm(20, 20)),
            GenDirective::Instance {
                path: "c0".into(),
                part: "Cap".into(),
            },
            // GND must be a connected net for the conductor pour to validate.
            GenDirective::ConnectPins {
                net: "GND".into(),
                pins: vec![("c0".into(), "p2".into())],
            },
            GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon(vec![Point::mm(0, 0), Point::mm(20, 0), Point::mm(20, 20)]),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "B.Cu".into(),
            }),
        ];
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "r")
            .expect("elaborates");
        let regions = crate::elaborate::regions(&h.doc().source);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].role, Role::Conductor);
        assert_eq!(regions[0].net.as_deref(), Some("GND"));
        assert_eq!(regions[0].layer, "B.Cu");
    }

    /// `serialize(parse(serialize(doc))) == serialize(doc)` — canonical form is a
    /// fixed point.
    #[test]
    fn idempotent() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            EntityId::new("psu.dec[0]"),
            Override {
                pos: Some(Point { x: 1, y: 999_999 }),
                strength: Strength::Pin,
            },
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
        assert_eq!(
            src[1],
            GenDirective::Place {
                path: "psu.reg".into(),
                pos: Point::mm(30, 20)
            }
        );
        assert_eq!(
            src[2],
            GenDirective::Fix {
                path: "psu.reg".into(),
                pos: Point::mm(30, 20)
            }
        );
        assert_eq!(
            src[3],
            GenDirective::Near {
                a: "psu.reg".into(),
                b: "psu.reg".into(),
                within: 500_000
            }
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
        h.commit(
            Transaction::one(Command::SetSource(psu_module(3))),
            &lib,
            "psu",
        )
        .unwrap();
        h.commit(
            Transaction::one(Command::Nudge(
                EntityId::new("psu.dec[1]"),
                Point::mm(42, 7),
            )),
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
        assert!(
            d.report.decayed.is_empty(),
            "fixture should not have decayed hints"
        );
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
            GenDirective::Instance {
                path: "reg".into(),
                part: "LDO".into(),
            },
            GenDirective::Instance {
                path: "dec".into(),
                part: "Cap".into(),
            },
            GenDirective::Fix {
                path: "reg".into(),
                pos: Point::mm(0, 0),
            },
            GenDirective::Rotate {
                path: "reg".into(),
                orient: Orient::from_deg(90).unwrap(),
            },
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
        assert_eq!(
            src[0],
            GenDirective::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(-90).unwrap(),
            }
        );
        assert_eq!(
            src[1],
            GenDirective::NearPin {
                a: "c1".into(),
                b_comp: "u1".into(),
                b_pin: "VOUT".into(),
                within: 1_500_000,
            }
        );
        // Off-axis angles are valid now (Stage 2) — lowered to a quaternion, not rejected.
        assert!(parse("rotate u1 45").is_ok());
        assert!(parse("rotate u1 30.5").is_ok());
        assert!(parse("rotate u1 notnum").is_err());
    }

    #[test]
    fn arbitrary_angle_round_trips_as_a_quaternion() {
        // A non-cardinal angle lowers to a quaternion and serialises as `quat=(…)`
        // (the angle isn't exactly representable; the quaternion is the canonical form).
        let (src, _) = parse("rotate u1 30").unwrap();
        let GenDirective::Rotate { orient, .. } = &src[0] else {
            panic!("expected a rotate, got {:?}", src[0]);
        };
        assert_eq!(*orient, Orient::from_angle_deg(30.0));
        assert_eq!(orient.to_deg(), 30, "≈ 30° about z");
        // Canonical form is the exact quaternion, and re-parses identically.
        let canon = render_directive(&src[0]);
        assert!(
            canon.starts_with("rotate u1 quat=("),
            "non-cardinal serialises as quat: {canon}"
        );
        assert_eq!(parse(&canon).unwrap().0, src);
        // A cardinal still serialises readably (and `bottom` survives).
        assert_eq!(
            render_directive(&parse("rotate u1 90 bottom").unwrap().0[0]),
            "rotate u1 90 bottom"
        );
    }

    #[test]
    fn rotate_rejects_non_finite_and_tolerates_quat_whitespace() {
        // Non-finite angles must be a clean error, never a degenerate (0,0,0,0) orient.
        assert!(parse("rotate u1 nan").is_err());
        assert!(parse("rotate u1 inf").is_err());
        assert!(parse("rotate u1 -inf").is_err());
        assert!(parse("rotate u1 1e309").is_err()); // overflows f64 to +inf
        // `quat=` tolerates whitespace after commas (same as the no-space canonical form).
        let spaced = parse("rotate u1 quat=(1, 0, 0, 1)").unwrap().0;
        let tight = parse("rotate u1 quat=(1,0,0,1)").unwrap().0;
        assert_eq!(spaced, tight);
        assert!(parse("rotate u1 quat=(0,0,0,0)").is_err());
    }

    #[test]
    fn rotate_bottom_authoring_round_trips() {
        let (src, _) = parse("rotate u1 90 bottom").unwrap();
        assert_eq!(
            src[0],
            GenDirective::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(90).unwrap().flipped(),
            }
        );
        // Canonical serialization carries the `bottom` flag and re-parses identically.
        assert_eq!(render_directive(&src[0]), "rotate u1 90 bottom");
        assert_eq!(parse("rotate u1 90").unwrap().0[0], {
            GenDirective::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(90).unwrap(),
            }
        });
        // A stray third token that isn't `bottom` is an error.
        assert!(parse("rotate u1 90 sideways").is_err());
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
        h.commit(Transaction::one(Command::LoadText(text)), &lib, "load")
            .unwrap();
        let loaded = h.doc();

        assert_eq!(loaded.source, reference.source);
        assert_eq!(loaded.components, reference.components);
        assert_eq!(loaded.nets, reference.nets);
    }

    #[test]
    fn load_text_is_atomic_on_parse_error() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::SetSource(psu_module(2))),
            &lib,
            "psu",
        )
        .unwrap();
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
        assert!(
            e.contains("frobnicate"),
            "error should name the offending line: {e}"
        );
    }

    #[test]
    fn parse_error_bad_coordinate() {
        let e = crate::diagnostic::render(&parse("place foo (3mm)").unwrap_err());
        assert!(
            e.contains("1:1"),
            "error should carry the line location: {e}"
        );
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
        assert!(
            text.contains("1:1") && text.contains("3:1"),
            "located by line: {text}"
        );
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
