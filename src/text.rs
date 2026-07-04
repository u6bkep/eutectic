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
//! inst    <path> <part> [label=<v>] [p:<k>=<v> ...]  # instantiate a part; optional
//!                                  #   display label + identity params (quote for spaces)
//! class   <name> [prefix=<v>] [template=<v>] [p:<k>=<v> ...]  # class-registry entry
//! place   <path> (<x>, <y>)        # source default placement (a free DOF)
//! fix     <path> (<x>, <y>)        # hard placement constraint (mechanical datum)
//! board   (<x>, <y>) (<x>, <y>)    # board outline (min corner, max corner)
//! hole    (<x>, <y>) dia=<len>     # authored NPTH through-hole (mounting hole)
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
//! refdes  <path> <string>          # pin a reference designator (opaque; verbatim)
//!
//! # ---- routes state zone (tier-2 materialized, non-derivable — Decision 18) ----
//! route <net> <slab> w=<width> (x,y) (x,y) ...  [free|hint|fixed]   # a routed polyline
//! via   <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]  # a plated via
//! ```
//!
//! Routes live in a `# routes` section beside `# overrides`. They are materialized
//! state the parser fills directly (never re-derived at load — an autorouter is
//! expensive/stochastic), so re-elaboration cannot wipe them. The layer is a copper
//! slab **name** (Decision 13); provenance is a trailing keyword (`pinned` is the
//! default and omitted; `free` = router-owned, `hint`/`fixed` complete the ladder). A
//! via's span defaults to the full copper extent; an explicit `<from>..<to>` names a
//! blind/buried span. Trace/via ids are minted at parse (session-local, never written).
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

use crate::annotate::ClassEntry;
use crate::diagnostic::{Diagnostic, Location};
use crate::doc::{Doc, MM, Nm, Orient, Override, Point, Provenance, Strength};
use crate::elaborate::{DefNode, GenDirective, RegionDecl, Source, board_rect, directive_coords};
use crate::geom::{KeepoutKind, Material, Path, Role, Seg, Shape2D, Slab, ZRange, coord_ok};
use crate::id::{EntityId, TraceId, ViaId};
use crate::route::{Trace, Via};
use std::collections::{BTreeMap, BTreeSet};

/// The parsed tier-1/tier-2 state: the generative program, the ID-keyed override maps,
/// and the persisted routing state zone (Decision 18 — routes are materialized but
/// *not derivable*, so they persist rather than re-solve). A named struct (not a
/// positional tuple) so adding a state section adds a field without churning every
/// destructuring site. `TraceId`/`ViaId` are minted at parse (session-local, never
/// serialized), so the maps key by fresh ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub source: Source,
    pub overrides: BTreeMap<EntityId, Override>,
    pub refdes_pins: BTreeMap<EntityId, String>,
    pub traces: BTreeMap<TraceId, Trace>,
    pub vias: BTreeMap<ViaId, Via>,
    /// tier-1 authored schematic layout tree (Decision 20). `None` when the document has
    /// no `schematic` block — the common case, and what keeps a blockless doc's
    /// serialization byte-identical. The last `schematic` block wins (mirrors `board`).
    pub schematic: Option<crate::schematic::SchematicLayout>,
}

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
    // The authored schematic layout tree (Decision 20), after the flat directives. Only
    // emitted when present, so a blockless doc's text is byte-identical to before this
    // feature (the poc round-trip guard).
    if let Some(layout) = &doc.schematic {
        out.push_str(&serialize_layout(layout));
    }
    // Overrides last, in deterministic id order across both kinds — an entity's pos
    // override and refdes pin land together. (Empty pos overrides — pos == None — are
    // inert and carry no canonical text.)
    let ids: BTreeSet<&EntityId> = doc
        .overrides
        .iter()
        .filter(|(_, ov)| ov.pos.is_some())
        .map(|(id, _)| id)
        .chain(doc.refdes_pins.keys())
        .collect();
    let mut first = true;
    for id in ids {
        if first {
            out.push_str("\n# overrides\n");
            first = false;
        }
        if let Some(pos) = doc.overrides.get(id).and_then(|ov| ov.pos) {
            let kw = match doc.overrides[id].strength {
                Strength::Hint => "hint",
                Strength::Pin => "pin",
            };
            out.push_str(&format!("{kw} {id} {}\n", fmt_point(pos)));
        }
        if let Some(refdes) = doc.refdes_pins.get(id) {
            out.push_str(&format!("refdes {id} {}\n", quote_value(refdes)));
        }
    }

    // The routing state zone (Decision 18) — a second state section beside `# overrides`.
    // Emitted in canonical `BTreeMap` (id) order; the ids themselves are session-local
    // and never printed (a `route`/`via` line carries no id). Empty ⇒ no section, so a
    // routeless doc's text is byte-identical to before this feature.
    if !doc.traces.is_empty() || !doc.vias.is_empty() {
        out.push_str("\n# routes\n");
        for t in doc.traces.values() {
            out.push_str(&render_trace(t));
            out.push('\n');
        }
        for v in doc.vias.values() {
            out.push_str(&render_via(v));
            out.push('\n');
        }
    }
    out
}

/// Serialize the provenance keyword of a persisted route: `pinned` is the default and
/// prints nothing (hand/frozen routing is the common case). `free` marks router-owned
/// copper (the rip-up-able tier). `hint`/`fixed` complete the ladder
/// ([`Provenance`]) so any provenance a route may carry round-trips losslessly rather
/// than silently collapsing to Pinned on save.
fn prov_keyword(p: Provenance) -> &'static str {
    match p {
        Provenance::Pinned => "",
        Provenance::Free => " free",
        Provenance::Hint => " hint",
        Provenance::Fixed => " fixed",
    }
}

/// `route <net> <slab> w=<width> (x,y) (x,y) ... [free|hint|fixed]`. The layer is a
/// copper slab name (Decision 13); the width and points are canonical lengths;
/// provenance is a trailing keyword (`pinned` is the default and omitted).
fn render_trace(t: &Trace) -> String {
    let mut s = format!("route {} {} w={}", t.net, t.layer, fmt_len(t.width));
    for p in &t.path {
        s.push(' ');
        s.push_str(&fmt_point(*p));
    }
    s.push_str(prov_keyword(t.prov));
    s
}

/// `via <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]`. A `None` span
/// is the full copper extent (the common through-via) and prints no span token; an
/// explicit blind/buried span prints `<from>..<to>` (Decision 18).
fn render_via(v: &Via) -> String {
    let mut s = format!(
        "via {} {} drill={} pad={}",
        v.net,
        fmt_point(v.at),
        fmt_len(v.drill),
        fmt_len(v.pad),
    );
    if let Some((from, to)) = &v.span {
        s.push_str(&format!(" {from}..{to}"));
    }
    s.push_str(prov_keyword(v.prov));
    s
}

fn render_directive(d: &GenDirective) -> String {
    match d {
        GenDirective::Instance {
            path,
            part,
            params,
            label,
        } => {
            // `inst <path> <part> [label=<val>] [p:<key>=<val> ...]`. Values are quoted
            // only when they must be (whitespace / `#` / empty), so the common case stays
            // `p:value=10k`. Params render in `BTreeMap` (key) order for a canonical form.
            let mut s = format!("inst {path} {part}");
            if let Some(l) = label {
                s.push_str(&format!(" label={}", quote_value(l)));
            }
            for (k, v) in params {
                s.push_str(&format!(" p:{k}={}", quote_param_value(v)));
            }
            s
        }
        GenDirective::Param { name, expr } => {
            // `param <name> = <expr>` (Decision 21b). The expression text is emitted
            // verbatim (it *is* the generative program — serializes as authored, never
            // pre-evaluated); the `=` is spaced for readability and re-parsed tolerantly.
            format!("param {name} = {expr}")
        }
        GenDirective::InstGenerative {
            path,
            part,
            params,
            param_exprs,
            label,
            range,
            if_expr,
        } => {
            // The generative `inst`, serialized AS AUTHORED (Decision 21): a range suffix
            // `[<lo>..<hi>]` on the path, an `if=<expr>` clause, verbatim `p:k=v` and
            // expression `p:k=(<expr>)` params. The evaluated/expanded instances are
            // elaboration-only and never serialized. Params render in `BTreeMap` (key)
            // order for a canonical form; a verbatim and an expression key never collide
            // (the parser routes each token to exactly one map).
            let ranged = match range {
                Some((lo, hi)) => format!("{path}[{lo}..{hi}]"),
                None => path.clone(),
            };
            let mut s = format!("inst {ranged} {part}");
            if let Some(l) = label {
                s.push_str(&format!(" label={}", quote_value(l)));
            }
            if let Some(cond) = if_expr {
                // Canonical paren form `if=(<expr>)` (m2): the parens both delimit an
                // expression that may contain spaces and re-parse back to the same
                // `if_expr` (the parser strips one outer pair). No re-quoting.
                s.push_str(&format!(" if=({cond})"));
            }
            for (k, v) in params {
                s.push_str(&format!(" p:{k}={}", quote_param_value(v)));
            }
            // Expression params wear parentheses — the unambiguous "evaluate me" marker.
            for (k, e) in param_exprs {
                s.push_str(&format!(" p:{k}=({e})"));
            }
            s
        }
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
        GenDirective::Hole { center, dia } => {
            // `hole <center> dia=<len>` — an authored NPTH through-hole (Decision 16b).
            format!("hole {} dia={}", fmt_point(*center), fmt_len(*dia))
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
        GenDirective::Class { name, entry } => {
            // `class <name> [prefix=<val>] [template=<val>] [p:<key>=<val> ...]`. Defaults
            // reuse the `p:` param namespace from `inst`; keys render in `BTreeMap` order.
            let mut s = format!("class {name}");
            if let Some(p) = &entry.prefix {
                s.push_str(&format!(" prefix={}", quote_value(p)));
            }
            if let Some(t) = &entry.template {
                s.push_str(&format!(" template={}", quote_value(t)));
            }
            for (k, v) in &entry.defaults {
                s.push_str(&format!(" p:{k}={}", quote_value(v)));
            }
            s
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
        GenDirective::Font { path } => {
            // `font "<path>"` — the doc-wide outline font (Decision 17). Double-quoted so
            // the path may contain spaces; the built-in stroke font is the default when
            // absent.
            format!("font \"{path}\"")
        }
        GenDirective::Def {
            name,
            params,
            body,
            ports,
        } => render_def(name, params, body, ports),
    }
}

/// Render a `def` block (Decision 21a) as canonical block-grammar text (no trailing
/// newline — the caller appends one, matching the flat serialize loop). The header is
/// `def <name>` with each declared param as an inline ` param <k>=<default>`; the body is
/// each directive re-rendered and indented one level, interleaved with preserved
/// comment/blank trivia; `port` bindings emit last, in `BTreeMap` (name) order — a
/// deterministic canonical position independent of where they were authored. Body
/// directives round-trip through the same [`render_directive`] the flat program uses
/// (nested def instantiations are ordinary `inst` lines), so a def body is byte-stable
/// across a parse→serialize→parse fixpoint.
fn render_def(
    name: &str,
    params: &[(String, String)],
    body: &[DefNode],
    ports: &BTreeMap<String, (String, String)>,
) -> String {
    let mut s = format!("def {name}");
    for (k, v) in params {
        s.push_str(&format!(" param {k}={v}"));
    }
    s.push_str(" {\n");
    for node in body {
        match node {
            DefNode::Directive(d) => {
                s.push_str(BLOCK_INDENT);
                s.push_str(&render_directive(d));
                s.push('\n');
            }
            DefNode::Comment(text) if text.is_empty() => {
                s.push_str(BLOCK_INDENT);
                s.push_str("#\n");
            }
            DefNode::Comment(text) => {
                s.push_str(BLOCK_INDENT);
                s.push_str(&format!("# {text}\n"));
            }
            DefNode::Blank => s.push('\n'),
        }
    }
    // Ports emit after the body, in canonical (name) order.
    for (pname, (path, sel)) in ports {
        s.push_str(BLOCK_INDENT);
        s.push_str(&format!("port {pname} = {path}.{sel}\n"));
    }
    s.push('}');
    s
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
        // Not authorable via a `region` directive (the `region` parser rejects these),
        // but they ARE authorable as `slab` roles and round-trip that way — so the
        // token must stay stable and lossless. `substrate`/`marking`/`mask`/`datum`
        // are all parsed by the `slab` grammar.
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

// ----------------------------------------------------------------------------
// Block tree (nested-block grammar infrastructure — Phase 0)
// ----------------------------------------------------------------------------
//
// The base grammar is one directive per line. A directive line may additionally
// *open a block* by ending with a trailing `{`; the block closes with a `}` alone
// on its own line, and blocks nest to arbitrary depth. This module owns only the
// generic nested representation and its (de)serialization — no directive here
// consumes a block yet (see [`keyword_takes_block`]). The Decision-20 layout tree
// (`row`/`column`) and Decision-21 `def` bodies are the consumers-to-be: they will
// walk a [`Block`]'s header + children ([`Node`]) without re-tokenizing.
//
// A block body is a [`Node`] sequence: child directives interleaved with the comment
// and blank lines between them. Trivia is preserved *inside* blocks (Decision 21's
// mixed-authorship `def` bodies must round-trip) but dropped at the top level, where
// the flat path has always dropped it.

/// A directive together with any block body it opened. Header tokens are pre-split
/// (whitespace-aware, keeping quoted runs intact — the same tokenization the flat
/// directive path uses), so a consumer walks `keyword`/`tokens`/`children` directly.
/// `line` is the 1-based source line of the header, for diagnostics.
///
/// A leaf directive (no trailing `{`) has an empty `children` and `opened_block ==
/// false`; a block opener has `opened_block == true` (even when the body is empty).
/// The distinction matters to the flat path, which must reject a block on a keyword
/// that does not accept one — an *empty* block is still a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The leading bare token (the directive keyword).
    pub keyword: String,
    /// Every whitespace-separated header token (including the keyword at index 0),
    /// with quoted runs kept intact. Consumers walk these; they are never re-split.
    pub tokens: Vec<String>,
    /// The whitespace-normalized header tail: the header with its keyword removed. The
    /// keyword→tail separator is collapsed to nothing here (this is the tail *after* the
    /// separating whitespace) but the tail's own internal spacing and quoting are
    /// preserved verbatim — so coordinate- and quote-sensitive per-directive parsers see
    /// the same content they parse today. NOT a byte-exact slice of the source line (the
    /// leading separator is normalized); every current per-directive parser is
    /// whitespace-insensitive across the keyword boundary, so this is exact for their
    /// purposes. A consumer needing the raw line should reconstruct via the tokens.
    pub rest: String,
    /// Whether this directive opened a `{ … }` block (true even if the body is empty).
    pub opened_block: bool,
    /// This block's body, in source order: child directives interleaved with the
    /// comment and blank lines between them (trivia is preserved *inside* blocks so
    /// mixed-authorship `def` bodies round-trip — Decision 21). Empty for a leaf.
    pub children: Vec<Node>,
    /// 1-based source line of the header.
    pub line: u32,
}

/// One entry in a block body: a nested directive, or a preserved trivia line (a comment
/// or a blank). Trivia is retained only *inside* blocks; the top-level forest carries
/// only [`Node::Block`] (the flat path's pre-existing behavior — top-level comments and
/// blanks are not tier-1 state and are dropped as they always were).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A nested directive (itself possibly a block opener).
    Block(Block),
    /// A whole-line comment, stored **without** its leading `#` or surrounding
    /// whitespace (re-emitted as `# <text>` at canonical indent). An empty-bodied
    /// comment (`#` alone) stores the empty string.
    Comment(String),
    /// A blank line.
    Blank,
}

impl Block {
    /// The full header line as it feeds the flat directive path: `keyword` then
    /// `rest` (when non-empty). This is what [`parse_line`] receives for a leaf.
    fn header_line(&self) -> String {
        if self.rest.is_empty() {
            self.keyword.clone()
        } else {
            format!("{} {}", self.keyword, self.rest)
        }
    }
}

/// The per-keyword block allowlist. No existing directive accepts a block, so this is
/// `false` for every keyword today: a block opened on any current keyword is a parse
/// error, leaving all existing documents unchanged.
///
/// A Phase-1/2 consumer (the Decision-20 layout containers, Decision-21 `def`) enables
/// its keyword by wiring **three** things — the block tree is already built for every
/// keyword, but nothing walks a block body yet, so opting in is not a one-liner:
///
/// 1. return `true` here for the keyword;
/// 2. add a children-aware arm in [`parse_forest`] *before* the `parse_line`
///    fallthrough, which walks [`Block::children`] (recursing into nested
///    [`Node::Block`]s) and lowers the body into its own tier-1 representation;
/// 3. add storage for that representation in [`Parsed`].
///
/// The recursion path in [`parse_forest`] is exercised end-to-end by a `cfg(test)`
/// block-accepting keyword (see the `testblock` tests), so Phase 1 inherits a tested
/// descent rather than a latent one.
fn keyword_takes_block(keyword: &str) -> bool {
    // A test-only sentinel keyword that opts into blocks, so the `parse_forest` descent
    // path is covered before any real consumer lands (finding 3). Never reachable in a
    // non-test build; real keywords are added to this match when their consumer lands.
    #[cfg(test)]
    if keyword == TEST_BLOCK_KEYWORD {
        return true;
    }
    // Decision 20 layout tree: the `schematic` block and its nested `row`/`column`
    // containers accept block bodies. `sym` leaves do not (they are single-line
    // directives inside a container).
    // Decision 21a: a `def` opens a block body (its sub-circuit); `port` is a leaf
    // directive inside it.
    matches!(keyword, "schematic" | "row" | "column" | "def")
}

/// A `cfg(test)` sentinel keyword that accepts a block, used to exercise the
/// `parse_forest` descent end-to-end. Chosen not to collide with any real directive.
#[cfg(test)]
const TEST_BLOCK_KEYWORD: &str = "testblock";

/// Split a block-tree header into its tokens, keeping quoted runs intact. The leading
/// token is the keyword; `rest` is the original header with the keyword and one run of
/// separating whitespace removed (so coordinate/quote-sensitive per-directive parsers
/// see exactly what the flat path gives them today).
fn split_header(header: &str) -> (String, Vec<String>, String) {
    let tokens = split_ws_quoted(header);
    let keyword = tokens.first().cloned().unwrap_or_default();
    let rest = match header.trim_start().split_once(char::is_whitespace) {
        Some((_, r)) => r.trim().to_string(),
        None => String::new(),
    };
    (keyword, tokens, rest)
}

/// Detect a block-opening trailing `{` on an already comment-stripped line. Returns
/// `(header, opened_block)`: for an opener the trailing `{` is removed and `header`
/// is the directive part; otherwise `header` is the line unchanged. A `{` only opens
/// a block when it is the final non-whitespace character *outside* a quoted string —
/// a brace inside a quoted value (`text "a{b}"`) is literal. The scan is quote-aware
/// and runs on the unquoted remainder after comment stripping, so a `{` after a `#`
/// comment never opens a block either (the comment is already gone).
fn split_block_open(line: &str) -> (&str, bool) {
    let trimmed = line.trim_end();
    // The last character must be `{` *and* lie outside any quoted run to open a block.
    if !trimmed.ends_with('{') {
        return (line, false);
    }
    let mut in_str = false;
    let brace_at = trimmed.len() - 1; // `{` is ASCII, one byte.
    for (i, c) in trimmed.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '{' if !in_str && i == brace_at => {
                return (trimmed[..i].trim_end(), true);
            }
            _ => {}
        }
    }
    // The trailing `{` was inside a quoted string — literal, not a block opener.
    (line, false)
}

/// Is this comment-stripped, trimmed line a lone block close (`}`)? A `}` only closes
/// a block when it stands alone on its line; a `}` embedded in a directive or inside a
/// quoted value is not a close (quoted values are handled by the tokenizer downstream).
fn is_block_close(line: &str) -> bool {
    line == "}"
}

/// The comment text of a raw line whose code part (quote-aware) is empty — i.e. a
/// whole-line comment. Returns the text after the first unquoted `#`, trimmed, or
/// `None` if the line carries no comment. (A blank line has no `#` and returns `None`.)
fn whole_line_comment(raw: &str) -> Option<&str> {
    let mut in_str = false;
    for (i, c) in raw.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return Some(raw[i + 1..].trim()),
            _ => {}
        }
    }
    None
}

/// Parse text into a forest of [`Block`]s: the nested-block grammar's generic
/// representation. Comment stripping is quote-aware (a `#` inside a quoted value is
/// literal) and happens *before* brace detection on the unquoted remainder. Comment and
/// blank lines *inside a block* are preserved as [`Node`] trivia (Decision 21 mixed
/// authorship); at the top level they are dropped, as the flat path has always done.
/// Errors — an unbalanced `{` (a block left open at end of input), a `}` with no open
/// block, and an empty-keyword block opener (a lone `{`) — are `E_BLOCK` diagnostics
/// located by line number, collected in the house *collect-all* style. On any error the
/// whole parse fails and no partial tree escapes.
pub fn parse_blocks(text: &str) -> Result<Vec<Block>, Vec<Diagnostic>> {
    // A stack of (open-block header, its accumulating body). The bottom frame is the
    // synthetic top-level forest, whose opener is `None` and is never read.
    let mut stack: Vec<(Option<Block>, Vec<Node>)> = vec![(None, Vec::new())];
    let mut errors: Vec<Diagnostic> = Vec::new();
    // Whether the innermost frame is a real (non-bottom) block: trivia is preserved only
    // there, so top-level comments/blanks stay dropped exactly as before.
    let in_block = |stack: &Vec<(Option<Block>, Vec<Node>)>| stack.len() > 1;

    for (i, raw) in text.lines().enumerate() {
        let lineno = (i + 1) as u32;
        let stripped = strip_comment(raw).trim();
        if stripped.is_empty() {
            // A trivia line (blank or whole-line comment). Preserve it inside a block;
            // drop it at the top level (unchanged flat behavior).
            if in_block(&stack) {
                let node = match whole_line_comment(raw) {
                    Some(text) => Node::Comment(text.to_string()),
                    None => Node::Blank,
                };
                stack.last_mut().unwrap().1.push(node);
            }
            continue;
        }
        if is_block_close(stripped) {
            if !in_block(&stack) {
                errors.push(Diagnostic::error(
                    "E_BLOCK",
                    "`}` with no open block".to_string(),
                    Location::Span {
                        line: lineno,
                        col: 1,
                    },
                ));
                continue;
            }
            // Pop the frame, attach its finished body, and push the completed block onto
            // its parent as a `Node::Block`.
            let (opener, children) = stack.pop().expect("checked len > 1");
            let mut block = opener.expect("non-bottom frame always carries an opener");
            block.children = children;
            stack.last_mut().unwrap().1.push(Node::Block(block));
            continue;
        }
        let (header, opened) = split_block_open(stripped);
        let (keyword, tokens, rest) = split_header(header);
        if opened && keyword.is_empty() {
            // A `{` with no directive in front (e.g. a lone `{`). Rejected here in the
            // public API so a malformed opener never reaches a consumer or serializes to
            // a leading-space line (finding 4). No frame is opened.
            errors.push(Diagnostic::error(
                "E_BLOCK",
                "block opener has no directive before `{`".to_string(),
                Location::Span {
                    line: lineno,
                    col: 1,
                },
            ));
            continue;
        }
        let block = Block {
            keyword,
            tokens,
            rest,
            opened_block: opened,
            children: Vec::new(),
            line: lineno,
        };
        if opened {
            // Open a new frame; its body accumulates until the matching `}`.
            stack.push((Some(block), Vec::new()));
        } else {
            stack.last_mut().unwrap().1.push(Node::Block(block));
        }
    }

    // Any frame still open at end of input is an unbalanced `{`. Report each, located
    // at its opener's line.
    while stack.len() > 1 {
        let (opener, _) = stack.pop().expect("checked len > 1");
        let opener = opener.expect("non-bottom frame always carries an opener");
        let header = opener.header_line();
        errors.push(Diagnostic::error(
            "E_BLOCK",
            format!("unbalanced `{{`: block opened by `{header}` is never closed"),
            Location::Span {
                line: opener.line,
                col: 1,
            },
        ));
    }

    if errors.is_empty() {
        // Top-level trivia was never pushed (the bottom frame is not "in a block"), so
        // the bottom frame holds only `Node::Block`; unwrap to the `Vec<Block>` forest.
        let top = stack.pop().expect("bottom frame is always present").1;
        Ok(top
            .into_iter()
            .map(|n| match n {
                Node::Block(b) => b,
                _ => unreachable!("top-level trivia is dropped, never pushed"),
            })
            .collect())
    } else {
        // Report errors in source order (the opener pop order above is innermost-first).
        errors.sort_by_key(|d| match d.location {
            Location::Span { line, .. } => line,
            _ => 0,
        });
        Err(errors)
    }
}

/// The canonical indent for a nested block: two spaces per depth level. Matches the
/// existing serializer's flat, space-based style (it emits no tabs anywhere).
const BLOCK_INDENT: &str = "  ";

/// Serialize a forest of [`Block`]s back to canonical block-grammar text, deterministic
/// and round-tripping ([`parse_blocks`] of the output reproduces the forest). Each
/// directive renders as its header line; a block opener appends ` {`, its body renders
/// indented one level deeper (nested directives, and the comment/blank trivia between
/// them), and a `}` closes at the opener's indent. A comment renders as `# <text>` (or
/// bare `#` when empty), a blank as an empty line. This is the emission half consumers
/// reuse once their keyword opts into blocks; the flat [`serialize`] on a `Doc` is
/// unchanged (routeless/blockless docs stay byte-identical).
pub fn serialize_blocks(blocks: &[Block]) -> String {
    let mut out = String::new();
    // `indent` grows/shrinks by one level per depth so ancestors are never re-indented
    // (emission is O(total output), not O(depth·output)).
    let mut indent = String::new();
    emit_block_seq(blocks, &mut indent, &mut out);
    out
}

/// Emit a sequence of block *directives* (no trivia — the top-level forest and the
/// caller's convenience over a `&[Block]`), wrapping each into a `Node::Block` view.
fn emit_block_seq(blocks: &[Block], indent: &mut String, out: &mut String) {
    for b in blocks {
        emit_block(b, indent, out);
    }
}

/// Emit a block body: nested directives interleaved with preserved trivia.
fn emit_nodes(nodes: &[Node], indent: &mut String, out: &mut String) {
    for n in nodes {
        match n {
            Node::Block(b) => emit_block(b, indent, out),
            Node::Comment(text) => {
                out.push_str(indent);
                if text.is_empty() {
                    out.push_str("#\n");
                } else {
                    out.push_str("# ");
                    out.push_str(text);
                    out.push('\n');
                }
            }
            Node::Blank => out.push('\n'),
        }
    }
}

fn emit_block(b: &Block, indent: &mut String, out: &mut String) {
    out.push_str(indent);
    out.push_str(&b.header_line());
    if b.opened_block {
        out.push_str(" {\n");
        indent.push_str(BLOCK_INDENT);
        emit_nodes(&b.children, indent, out);
        indent.truncate(indent.len() - BLOCK_INDENT.len());
        out.push_str(indent);
        out.push_str("}\n");
    } else {
        out.push('\n');
    }
}

/// Parse canonical (or human-authored) text back into tier-1 state. Comments
/// (`#`...) and blank lines are skipped. Never panics. *Collect-all*: every
/// malformed line is reported (located by line number via [`Location::Span`]), so
/// one parse surfaces all syntax errors at once; on any error the whole parse fails
/// with `Err(Vec<Diagnostic>)` and no partial state escapes.
pub fn parse(text: &str) -> Result<Parsed, Vec<Diagnostic>> {
    // First shape the input into the nested block tree (quote-aware comment stripping,
    // brace balancing). A blockless document produces a flat forest of leaf blocks, so
    // this path is byte-for-byte equivalent to the old per-line loop for existing docs.
    let blocks = parse_blocks(text)?;

    let mut parsed = Parsed::default();
    // Session-local id mints (Decision 18 — ids are never serialized). File order
    // becomes the id order, which is stable across a round-trip because serialize emits
    // in BTreeMap (id) order.
    let mut next_tid: u64 = 1;
    let mut next_vid: u64 = 1;
    let mut errors: Vec<Diagnostic> = Vec::new();

    let top: Vec<Node> = blocks.into_iter().map(Node::Block).collect();
    parse_forest(&top, &mut parsed, &mut next_tid, &mut next_vid, &mut errors);

    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(errors)
    }
}

/// Walk a block body (a `Node` sequence), lowering each directive into `parsed`. Trivia
/// nodes (comments, blanks) carry no tier-1 state and are skipped — the flat path has
/// always dropped them. A block opener on a keyword that accepts blocks
/// ([`keyword_takes_block`]) has its header lowered *and* its children descended into
/// (the tested recursion path — a real consumer replaces this generic descent with one
/// that stores the body). A block opener on any other keyword is a hard `E_BLOCK`
/// error; per the house *collect-all* ethos its children are still line-parsed so their
/// own `E_PARSE` diagnostics surface in the same pass (their results are discarded).
fn parse_forest(
    nodes: &[Node],
    parsed: &mut Parsed,
    next_tid: &mut u64,
    next_vid: &mut u64,
    errors: &mut Vec<Diagnostic>,
) {
    for node in nodes {
        let b = match node {
            Node::Block(b) => b,
            // Trivia is preserved in the tree but is not tier-1 state; skip it.
            Node::Comment(_) | Node::Blank => continue,
        };
        if b.opened_block && !keyword_takes_block(&b.keyword) {
            errors.push(Diagnostic::error(
                "E_BLOCK",
                format!("directive `{}` does not take a block", b.keyword),
                Location::Span {
                    line: b.line,
                    col: 1,
                },
            ));
            // Collect-all: still surface the children's own syntax errors this pass, so
            // fixing the keyword does not reveal a fresh round of errors. Results are
            // discarded (the block was rejected); only diagnostics are kept.
            let mut scratch = Parsed::default();
            let (mut t, mut v) = (1u64, 1u64);
            parse_forest(&b.children, &mut scratch, &mut t, &mut v, errors);
            continue;
        }
        if b.opened_block && b.keyword == "schematic" {
            // Decision 20: lower the whole `schematic { … }` subtree into the tier-1
            // layout. The last block wins (mirrors `board`). Header takes no tokens.
            if b.tokens.len() > 1 {
                errors.push(Diagnostic::error(
                    "E_SCHEMATIC",
                    format!("`schematic` takes no arguments (got `{}`)", b.rest),
                    Location::Span {
                        line: b.line,
                        col: 1,
                    },
                ));
            }
            let roots = parse_layout_nodes(&b.children, errors);
            parsed.schematic = Some(crate::schematic::SchematicLayout { roots });
        } else if b.opened_block && b.keyword == "def" {
            // Decision 21a: a top-level `def <name> [param <k>=<default> ...] { body }`.
            // The body is lowered into its own `Source` fragment (recursing this same
            // walk over its children), so a def body is authored exactly like the flat
            // program — parts, internal nets, `port` bindings, nested def *instantiations*.
            // Nested def *definitions* are rejected (definitions stay top-level, v1).
            parse_def(b, parsed, errors);
        } else if b.opened_block && matches!(b.keyword.as_str(), "row" | "column") {
            // A `row`/`column` opened outside a `schematic` block: the allowlist lets it
            // *parse* as a block (so its body's own errors surface), but it is not a
            // top-level directive. Reject it here and descend for child diagnostics.
            errors.push(Diagnostic::error(
                "E_SCHEMATIC",
                format!("`{}` is only valid inside a `schematic` block", b.keyword),
                Location::Span {
                    line: b.line,
                    col: 1,
                },
            ));
            let _ = parse_layout_nodes(&b.children, errors);
        } else if b.opened_block {
            // An accepted block (only the `cfg(test)` sentinel today). A real consumer's
            // arm parses the header its own way and stores the body; the generic stand-in
            // here just descends into the children (lowered as ordinary directives into
            // `parsed.source`), which exercises the recursion end-to-end.
            parse_forest(&b.children, parsed, next_tid, next_vid, errors);
        } else {
            // A leaf directive lowers through the flat line grammar, exactly as before.
            lower_directive(b, parsed, next_tid, next_vid, errors);
        }
    }
}

/// Lower a single directive's header line through the flat [`parse_line`] grammar into
/// `parsed`. Shared by the normal walk and the rejected-block child-diagnostics scan.
fn lower_directive(
    b: &Block,
    parsed: &mut Parsed,
    next_tid: &mut u64,
    next_vid: &mut u64,
    errors: &mut Vec<Diagnostic>,
) {
    let lineno = b.line;
    let line = b.header_line();
    match parse_line(&line) {
        Ok(Item::Directive(d)) => {
            check_coord_range(directive_coords(&d), lineno, errors);
            parsed.source.push(d);
        }
        Ok(Item::Override(id, ov)) => {
            let coords = ov.pos.map_or(vec![], |p| vec![p.x, p.y]);
            check_coord_range(coords, lineno, errors);
            parsed.overrides.insert(id, ov);
        }
        Ok(Item::RefdesPin(id, refdes)) => {
            parsed.refdes_pins.insert(id, refdes);
        }
        Ok(Item::Route(t)) => {
            let coords = t.path.iter().flat_map(|p| [p.x, p.y]).chain([t.width]);
            check_coord_range(coords.collect(), lineno, errors);
            parsed.traces.insert(TraceId(*next_tid), t);
            *next_tid += 1;
        }
        Ok(Item::Via(v)) => {
            check_coord_range(vec![v.at.x, v.at.y, v.drill, v.pad], lineno, errors);
            parsed.vias.insert(ViaId(*next_vid), v);
            *next_vid += 1;
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

// ----------------------------------------------------------------------------
// Decision-20 layout tree: parse
// ----------------------------------------------------------------------------

use crate::schematic::{Align, Container, Direction, LayoutNode, Symbol};

/// Lower a `schematic`/`row`/`column` block body (a [`Node`] sequence) into layout
/// nodes. Trivia (comments/blanks) is **preserved** as [`LayoutNode::Comment`]/`Blank`
/// so mixed authorship inside a `schematic` block round-trips (the Decision-20/21
/// requirement); the semantic walks ([`reflow`](crate::schematic::reflow), validation)
/// skip it. Only `row`/`column` (nested containers) and `sym` (leaves) are valid
/// directive children; anything else is an `E_SCHEMATIC` error. Collect-all: every
/// malformed child is reported.
fn parse_layout_nodes(nodes: &[Node], errors: &mut Vec<Diagnostic>) -> Vec<LayoutNode> {
    let mut out = Vec::new();
    for node in nodes {
        let b = match node {
            Node::Block(b) => b,
            Node::Comment(text) => {
                out.push(LayoutNode::Comment(text.clone()));
                continue;
            }
            Node::Blank => {
                out.push(LayoutNode::Blank);
                continue;
            }
        };
        match b.keyword.as_str() {
            "row" | "column" => {
                if !b.opened_block {
                    errors.push(err_line(
                        "E_SCHEMATIC",
                        format!("`{}` must open a `{{ … }}` block", b.keyword),
                        b.line,
                    ));
                    continue;
                }
                let dir = if b.keyword == "row" {
                    Direction::Row
                } else {
                    Direction::Column
                };
                match parse_container_header(dir, &b.tokens[1..], b.line) {
                    Ok((name, gap, align)) => {
                        // Authored length ingress: `gap` is bounded like every other
                        // coordinate/length (issue 0018) — an over-bound value is
                        // E_COORD_RANGE here, not an add-overflow panic later in reflow.
                        check_coord_range(vec![gap], b.line, errors);
                        let children = parse_layout_nodes(&b.children, errors);
                        out.push(LayoutNode::Container(Container {
                            dir,
                            name,
                            gap,
                            align,
                            children,
                        }));
                    }
                    Err(e) => errors.push(err_line("E_SCHEMATIC", e, b.line)),
                }
            }
            "sym" => {
                if b.opened_block {
                    errors.push(err_line(
                        "E_SCHEMATIC",
                        "`sym` is a leaf and takes no block".to_string(),
                        b.line,
                    ));
                    // Fall through to also report any header error, but a block sym is
                    // already rejected; skip parsing its (nonexistent leaf) header.
                    continue;
                }
                match parse_sym_header(&b.tokens[1..], b.line) {
                    Ok(sym) => {
                        // Bound the pinned offsets, same discipline as `gap` / every other
                        // authored length (issue 0018).
                        check_coord_range(vec![sym.dx, sym.dy], b.line, errors);
                        out.push(LayoutNode::Symbol(sym));
                    }
                    Err(e) => errors.push(err_line("E_SCHEMATIC", e, b.line)),
                }
            }
            other => errors.push(err_line(
                "E_SCHEMATIC",
                format!("`{other}` is not valid inside a layout container (expected `row`, `column`, or `sym`)"),
                b.line,
            )),
        }
    }
    out
}

/// A span-located diagnostic at column 1 — the layout parsers' one shape.
fn err_line(code: &'static str, msg: String, line: u32) -> Diagnostic {
    Diagnostic::error(code, msg, Location::Span { line, col: 1 })
}

/// Parse a top-level `def <name> [param <k>=<default> ...] { body }` (Decision 21a) and
/// push the resulting [`GenDirective::Def`](crate::elaborate::GenDirective::Def) onto
/// `parsed.source`. The header is `def <name>` followed by zero or more
/// `param <k>=<default>` declarations *inline on the header line* — the same
/// declaration-with-default shape a def instantiation later overrides via `p:`. The body
/// is a source fragment (parts, internal nets, `port` bindings, nested def
/// *instantiations*); nested def *definitions* and any non-body directive are rejected.
/// Collect-all: every malformed piece is reported; on any error nothing partial escapes
/// (the caller's `errors` is non-empty, so the whole parse fails).
fn parse_def(b: &Block, parsed: &mut Parsed, errors: &mut Vec<Diagnostic>) {
    // Header: `def <name> [param <k>=<default>]...`. Token 0 is `def`.
    let toks = &b.tokens[1..];
    if toks.is_empty() {
        errors.push(err_line(
            "E_DEF",
            "`def` needs a name: def <name> [param <k>=<default> ...] { ... }".to_string(),
            b.line,
        ));
        return;
    }
    let name = toks[0].clone();
    if !is_ident(&name) {
        errors.push(err_line(
            "E_DEF",
            format!("def name `{name}` must be an identifier (letters, digits, `_`)"),
            b.line,
        ));
    }
    // Inline `param <k>=<default>` declarations. They arrive as pairs of tokens
    // (`param`, `k=default`); a bare `param` with no `k=v`, or a non-`param` token, is an
    // error. Params keep authored order (defaults may reference earlier ones in the def
    // scope, mirroring the doc-level `param` order-independence).
    let mut params: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut i = 1;
    while i < toks.len() {
        if toks[i] != "param" {
            errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}` header: unexpected token `{}` (expected `param <k>=<default>`)",
                    toks[i]
                ),
                b.line,
            ));
            i += 1;
            continue;
        }
        let Some(kv) = toks.get(i + 1) else {
            errors.push(err_line(
                "E_DEF",
                format!("def `{name}` header: `param` needs `<k>=<default>`"),
                b.line,
            ));
            break;
        };
        match kv.split_once('=') {
            Some((k, v)) if is_ident(k) && !v.is_empty() => {
                if !seen.insert(k.to_string()) {
                    errors.push(err_line(
                        "E_DEF",
                        format!("def `{name}`: duplicate param `{k}`"),
                        b.line,
                    ));
                }
                params.push((k.to_string(), v.to_string()));
            }
            _ => errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}` header: malformed param `{kv}` (expected `<ident>=<default>`)"
                ),
                b.line,
            )),
        }
        i += 2;
    }

    // Body: lower each child directive into a `Source` fragment, pulling out `port`
    // bindings. Only body-shaped directives are accepted (inst / net / nc / connect /
    // port); layout, placement, board/stackup, and route directives are out of scope for
    // a def body (Phase 3). A nested `def { … }` is a hard error.
    let mut body: Vec<DefNode> = Vec::new();
    let mut ports: BTreeMap<String, (String, String)> = BTreeMap::new();
    for node in &b.children {
        let child = match node {
            Node::Block(c) => c,
            // Preserve body trivia so a mixed-authorship def body round-trips (Decision 21).
            Node::Comment(text) => {
                body.push(DefNode::Comment(text.clone()));
                continue;
            }
            Node::Blank => {
                body.push(DefNode::Blank);
                continue;
            }
        };
        if child.opened_block {
            // The only block-opening keyword reachable here is `def` (the allowlist); a
            // nested def definition is rejected. Any other block opener already errored in
            // `parse_forest`'s block-rejection arm, but a `def` inside a `def` reaches here.
            errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}`: nested `{}` block is not allowed (def definitions are top-level)",
                    child.keyword
                ),
                child.line,
            ));
            continue;
        }
        match child.keyword.as_str() {
            "port" => {
                match parse_port(&child.rest) {
                    Ok((pname, binding)) => {
                        if ports.insert(pname.clone(), binding).is_some() {
                            errors.push(err_line(
                                "E_DEF",
                                format!("def `{name}`: duplicate port `{pname}`"),
                                child.line,
                            ));
                        }
                    }
                    Err(e) => errors.push(err_line("E_DEF", format!("def `{name}`: {e}"), child.line)),
                }
            }
            "inst" | "net" | "nc" | "connect" => {
                let line = child.header_line();
                match parse_line(&line) {
                    Ok(Item::Directive(d)) => {
                        check_coord_range(directive_coords(&d), child.line, errors);
                        body.push(DefNode::Directive(d));
                    }
                    Ok(_) => errors.push(err_line(
                        "E_DEF",
                        format!("def `{name}`: `{}` is not a valid body directive", child.keyword),
                        child.line,
                    )),
                    Err(e) => errors.push(Diagnostic::error(
                        "E_PARSE",
                        format!("{e} (in `{line}`)"),
                        Location::Span { line: child.line, col: 1 },
                    )),
                }
            }
            other => errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}`: `{other}` is not valid in a def body (expected inst / net / nc / connect / port)"
                ),
                child.line,
            )),
        }
    }

    parsed.source.push(crate::elaborate::GenDirective::Def {
        name,
        params,
        body,
        ports,
    });
}

/// Parse a `port <name> = <internal-path>.<selector>` binding (Decision 21a bare typed
/// ports). Returns `(port-name, (internal-path, selector))`. The `<internal-path>` is a
/// def-relative instance path; the selector is a pin/pad selector resolved against that
/// instance's part at stamp time (same selector grammar as `net`). Named-InterfaceDef
/// ports (`port <name> : <iface-type> ...`) are not implemented (descoped — see report).
fn parse_port(rest: &str) -> Result<(String, (String, String)), String> {
    const USAGE: &str = "port <name> = <internal-path>.<pin-or-selector>";
    // Reject the stretch interface-port form explicitly so it fails loud, not silently.
    if rest.contains(':') && !rest.contains('=') {
        return Err(format!(
            "named-interface ports (`port <name> : <iface>`) are not supported in v1 ({USAGE})"
        ));
    }
    let (name, target) = rest
        .split_once('=')
        .ok_or_else(|| format!("{USAGE} (missing `=`)"))?;
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        return Err(format!("{USAGE} (empty name or target)"));
    }
    if !is_ident(name) {
        return Err(format!("port name `{name}` must be an identifier"));
    }
    let (path, sel) = split_last_dot(target, "port target")?;
    Ok((name.to_string(), (path, sel)))
}

/// Parse a container header tail (`[name] [gap=<len>] [align=start|center|end]`). The
/// optional name is a single leading bare token (no `=`); the rest are `key=value`
/// attributes in any order. An unknown attribute or a repeated one is an error. A
/// **quoted** leading token is always the name (its content is opaque — so a name may
/// contain `=`, `#`, or spaces and still round-trip); a bare token with an `=` is an
/// attribute.
fn parse_container_header(
    _dir: Direction,
    toks: &[String],
    _line: u32,
) -> Result<(Option<String>, Nm, Align), String> {
    let mut name: Option<String> = None;
    let mut gap: Option<Nm> = None;
    let mut align: Option<Align> = None;
    for (i, tok) in toks.iter().enumerate() {
        // A quoted leading token is the name verbatim (opaque content, may hold `=`).
        if i == 0 && name.is_none() && tok.starts_with('"') {
            name = Some(unquote(tok).to_string());
        } else if let Some((k, v)) = tok.split_once('=') {
            match k {
                "gap" => {
                    if gap.is_some() {
                        return Err("duplicate `gap`".into());
                    }
                    gap = Some(parse_len(v)?);
                }
                "align" => {
                    if align.is_some() {
                        return Err("duplicate `align`".into());
                    }
                    align = Some(parse_align(v)?);
                }
                _ => return Err(format!("unknown container attribute `{k}`")),
            }
        } else if i == 0 && name.is_none() {
            // The one bare token is the optional container name (must lead).
            name = Some(unquote(tok).to_string());
        } else {
            return Err(format!(
                "unexpected token `{tok}` (a container name must come first)"
            ));
        }
    }
    Ok((name, gap.unwrap_or(0), align.unwrap_or_default()))
}

fn parse_align(v: &str) -> Result<Align, String> {
    Ok(match v {
        "start" => Align::Start,
        "center" => Align::Center,
        "end" => Align::End,
        _ => return Err(format!("unknown align `{v}` (start | center | end)")),
    })
}

/// Parse a `sym` leaf header (`<comp-path> [rot=0|90|180|270] [dx=<len> dy=<len>]`). The
/// comp path is the leading token; the rest are `key=value` attributes. A **quoted** path
/// token is opaque (so a comp path containing `=`/`#`/spaces — all legal in an `inst`
/// path — round-trips); an unquoted token with an `=` is an attribute.
fn parse_sym_header(toks: &[String], _line: u32) -> Result<Symbol, String> {
    let mut path: Option<String> = None;
    let mut rot = Orient::IDENTITY;
    let (mut dx, mut dy) = (0i64, 0i64);
    let (mut saw_rot, mut saw_dx, mut saw_dy) = (false, false, false);
    for tok in toks {
        if path.is_none() && tok.starts_with('"') {
            // A quoted leading token is the comp path verbatim.
            path = Some(unquote(tok).to_string());
        } else if let Some((k, v)) = tok.split_once('=') {
            match k {
                "rot" => {
                    if saw_rot {
                        return Err("duplicate `rot`".into());
                    }
                    rot = parse_sym_rot(v)?;
                    saw_rot = true;
                }
                "dx" => {
                    if saw_dx {
                        return Err("duplicate `dx`".into());
                    }
                    dx = parse_len(v)?;
                    saw_dx = true;
                }
                "dy" => {
                    if saw_dy {
                        return Err("duplicate `dy`".into());
                    }
                    dy = parse_len(v)?;
                    saw_dy = true;
                }
                _ => return Err(format!("unknown `sym` attribute `{k}`")),
            }
        } else if path.is_none() {
            path = Some(tok.clone());
        } else {
            return Err(format!("unexpected token `{tok}` after the component path"));
        }
    }
    let path = path.ok_or("`sym` needs a component path")?;
    Ok(Symbol { path, rot, dx, dy })
}

/// Parse a `sym` `rot=` value: only the four cardinals are legal in v1 (§20b — authored
/// orientation, no arbitrary angles on the layout leaf). Yields the tiny exact cardinal
/// quaternion.
fn parse_sym_rot(v: &str) -> Result<Orient, String> {
    let d: i32 = v
        .parse()
        .map_err(|_| format!("`rot={v}` must be one of 0, 90, 180, 270"))?;
    match d.rem_euclid(360) {
        0 | 90 | 180 | 270 => Ok(Orient::from_deg(d).unwrap()),
        _ => Err(format!("`rot={v}` must be one of 0, 90, 180, 270")),
    }
}

// ----------------------------------------------------------------------------
// Decision-20 layout tree: serialize
// ----------------------------------------------------------------------------

/// Render a [`SchematicLayout`](crate::schematic::SchematicLayout) as canonical block
/// text: a `schematic { … }` wrapper around the emitted node forest, indented one level.
/// Deterministic and round-tripping: [`parse`] of the output reproduces the tree,
/// including trivia. Emitted only by [`serialize`], and only when a layout is present.
fn serialize_layout(layout: &crate::schematic::SchematicLayout) -> String {
    let mut out = String::from("schematic {\n");
    let mut indent = String::from(BLOCK_INDENT);
    emit_layout_nodes(&layout.roots, &mut indent, &mut out);
    out.push_str("}\n");
    out
}

/// Emit a layout node forest at the current `indent`. Containers open a `{ … }` block and
/// recurse; symbols and trivia are single lines. Mirrors [`emit_nodes`]'s trivia style so
/// the two block emitters agree.
fn emit_layout_nodes(nodes: &[LayoutNode], indent: &mut String, out: &mut String) {
    for n in nodes {
        match n {
            LayoutNode::Container(c) => {
                out.push_str(indent);
                out.push_str(&container_header(c));
                out.push_str(" {\n");
                indent.push_str(BLOCK_INDENT);
                emit_layout_nodes(&c.children, indent, out);
                indent.truncate(indent.len() - BLOCK_INDENT.len());
                out.push_str(indent);
                out.push_str("}\n");
            }
            LayoutNode::Symbol(s) => {
                out.push_str(indent);
                out.push_str(&sym_line(s));
                out.push('\n');
            }
            LayoutNode::Comment(text) => {
                out.push_str(indent);
                if text.is_empty() {
                    out.push_str("#\n");
                } else {
                    out.push_str("# ");
                    out.push_str(text);
                    out.push('\n');
                }
            }
            LayoutNode::Blank => out.push('\n'),
        }
    }
}

/// Quote a bare layout token (container name / comp path) when a structural character
/// would otherwise break re-parsing: whitespace, `#` (the comment stripper), `"`, or `=`
/// (the attribute separator). A comp path or name may legally contain `=` (an `inst`
/// path is unrestricted), so this quoting is what keeps such a token round-tripping. The
/// parsers ([`parse_container_header`]/[`parse_sym_header`]) treat a leading quoted token
/// as the opaque name/path.
fn quote_token(v: &str) -> String {
    let needs = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '=');
    if needs {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

/// The header line of a container (without the trailing ` {`): `row`/`column`, then an
/// optional name, `gap=` (omitted when zero), and `align=` (omitted when `Start`, the
/// default) — the minimal canonical form.
fn container_header(c: &crate::schematic::Container) -> String {
    use crate::schematic::{Align, Direction};
    let mut s = String::from(match c.dir {
        Direction::Row => "row",
        Direction::Column => "column",
    });
    if let Some(name) = &c.name {
        s.push(' ');
        s.push_str(&quote_token(name));
    }
    if c.gap != 0 {
        s.push_str(&format!(" gap={}", fmt_len(c.gap)));
    }
    match c.align {
        Align::Start => {}
        Align::Center => s.push_str(" align=center"),
        Align::End => s.push_str(" align=end"),
    }
    s
}

/// A `sym` leaf line: `sym <path>`, then `rot=` (omitted for identity) and `dx=`/`dy=`
/// (each omitted when zero) — the minimal canonical form.
fn sym_line(s: &crate::schematic::Symbol) -> String {
    let mut out = format!("sym {}", quote_token(&s.path));
    if s.rot != Orient::IDENTITY {
        // v1 rot is cardinal-only (the parser enforces it), so `to_deg` is exact.
        out.push_str(&format!(" rot={}", s.rot.to_deg()));
    }
    if s.dx != 0 {
        out.push_str(&format!(" dx={}", fmt_len(s.dx)));
    }
    if s.dy != 0 {
        out.push_str(&format!(" dy={}", fmt_len(s.dy)));
    }
    out
}

/// Push an `E_COORD_RANGE` error for each coordinate/length exceeding
/// [`crate::geom::MAX_COORD`] (issue 0018), located at `lineno`. A single line can
/// carry several out-of-range values; each is reported (collect-all), and the parse
/// aborts atomically like any other hard fault.
fn check_coord_range(coords: Vec<Nm>, lineno: u32, errors: &mut Vec<Diagnostic>) {
    for n in coords {
        if !coord_ok(n) {
            errors.push(Diagnostic::error(
                "E_COORD_RANGE",
                format!(
                    "coordinate {n} nm exceeds the ±{} nm (±1 m) range",
                    crate::geom::MAX_COORD
                ),
                Location::Span {
                    line: lineno,
                    col: 1,
                },
            ));
        }
    }
}

enum Item {
    Directive(GenDirective),
    Override(EntityId, Override),
    RefdesPin(EntityId, String),
    Route(Trace),
    Via(Via),
}

fn parse_line(line: &str) -> Result<Item, String> {
    let (kw, rest) = match line.split_once(char::is_whitespace) {
        Some((k, r)) => (k, r.trim()),
        None => (line, ""),
    };
    Ok(match kw {
        "inst" => {
            // `inst <path>[<lo>..<hi>] <part> [label=<val>] [if=<expr>] [p:<key>=<val>|(<expr>) ...]`.
            // `path`/`part` are the two leading bare tokens; `label=` carries the display
            // template; `if=<expr>` a population conditional; `p:<key>=<val>` an identity
            // param (verbatim), and `p:<key>=(<expr>)` an *expression* param (the parens
            // are the "evaluate me" marker — Decision 21b). A `[<lo>..<hi>]` suffix on the
            // path is a range (hi exclusive). Whenever a range, an `if=`, or any `(expr)`
            // param is present the directive is a generative [`GenDirective::InstGenerative`];
            // a plain `inst <path> <part>` with only verbatim params stays exactly
            // [`GenDirective::Instance`] as before, so existing docs are untouched. Tokens
            // are split parens-aware so `if=(n > 0)` / `p:c=(i + 1)` may contain spaces.
            const USAGE: &str = "inst <path>[<lo>..<hi>] <part> [label=<val>] [if=<expr>] [p:<key>=<val>|(<expr>) ...]";
            let toks = split_ws_quoted_parens(rest);
            if toks.len() < 2 {
                return Err(USAGE.into());
            }
            // A `[<lo>..<hi>]` suffix on the path is a range (the `..` disambiguates it
            // from an ordinary indexed path like `dec[0]`, which has no `..`).
            let (base_path, range) = split_range_suffix(&toks[0]);
            let part = toks[1].clone();
            let mut params = BTreeMap::new();
            let mut param_exprs = BTreeMap::new();
            let mut label = None;
            let mut if_expr = None;
            for tok in &toks[2..] {
                if let Some(v) = tok.strip_prefix("label=") {
                    label = Some(unquote(v).to_string());
                } else if let Some(v) = tok.strip_prefix("if=") {
                    if_expr = Some(parse_if_clause(v)?);
                } else if let Some(kv) = tok.strip_prefix("p:") {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| format!("inst param needs p:<key>=<value>: `{tok}`"))?;
                    // A bare `(...)` value is an *expression* param; a quoted value is
                    // always verbatim (M2); any other bare value is verbatim too. An
                    // unbalanced bare `(...)` is a parse-time error (m1).
                    match as_expr_value(v)? {
                        Some(inner) => {
                            param_exprs.insert(k.to_string(), inner);
                        }
                        None => {
                            params.insert(k.to_string(), unquote(v).to_string());
                        }
                    }
                } else {
                    return Err(format!("inst: unexpected token `{tok}` ({USAGE})"));
                }
            }
            // Stay on the plain declarative `Instance` unless a generative feature is used
            // — the round-trip and reconciliation of every existing document is identical.
            if range.is_none() && if_expr.is_none() && param_exprs.is_empty() {
                Item::Directive(GenDirective::Instance {
                    path: base_path,
                    part,
                    params,
                    label,
                })
            } else {
                Item::Directive(GenDirective::InstGenerative {
                    path: base_path,
                    part,
                    params,
                    param_exprs,
                    label,
                    range,
                    if_expr,
                })
            }
        }
        "param" => {
            // `param <name> = <expr>` (Decision 21b). The `=` splits name from the
            // expression; the expression is stored verbatim (parsed/evaluated only at
            // elaboration, so it serializes as authored). The name is a single bare token.
            const USAGE: &str = "param <name> = <expr>";
            let (name, expr) = rest
                .split_once('=')
                .ok_or_else(|| format!("{USAGE} (missing `=`)"))?;
            let name = name.trim();
            let expr = expr.trim();
            if name.is_empty() {
                return Err(format!("{USAGE} (empty name)"));
            }
            if name.split_whitespace().count() != 1 || !is_ident(name) {
                return Err(format!(
                    "param name `{name}` must be a single identifier (letters, digits, `_`)"
                ));
            }
            if expr.is_empty() {
                return Err(format!("{USAGE} (empty expression)"));
            }
            Item::Directive(GenDirective::Param {
                name: name.to_string(),
                expr: expr.to_string(),
            })
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
        "hole" => {
            // `hole (x,y) dia=<len>` — an authored NPTH through-hole (Decision 16b). The
            // center is the one point; `dia` is a required length (mm/nm), written after
            // the point (as it serializes). Lowers to a full-stackup, non-plated
            // `Role::Void` → `board-NPTH.drl`. The `dia=` token is stripped before the
            // point is parsed, so its position around the coordinate does not matter.
            let mut dia: Option<Nm> = None;
            let mut ptspart = String::new();
            for tok in rest.split_whitespace() {
                if let Some(d) = tok.strip_prefix("dia=") {
                    dia = Some(parse_len(d)?);
                } else {
                    ptspart.push_str(tok);
                    ptspart.push(' ');
                }
            }
            let pts = extract_points(&ptspart)?;
            if pts.len() != 1 {
                return Err("hole needs exactly one center point: hole (x,y) dia=<len>".into());
            }
            let dia = dia.ok_or("hole needs a diameter: hole (x,y) dia=<len>")?;
            if dia <= 0 {
                return Err(format!(
                    "hole: diameter must be positive (got {dia}nm) — a zero/negative drill is degenerate"
                ));
            }
            Item::Directive(GenDirective::Hole {
                center: pts[0],
                dia,
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
        "class" => {
            // `class <name> [prefix=<val>] [template=<val>] [p:<key>=<val> ...]`. Defaults
            // reuse the `p:` param namespace from `inst`; `prefix`/`template` values may be
            // quoted (a template with spaces). Merged over the built-in seeds by
            // `annotate::registry`.
            const USAGE: &str = "class <name> [prefix=<val>] [template=<val>] [p:<key>=<val> ...]";
            let toks = split_ws_quoted(rest);
            if toks.is_empty() {
                return Err(USAGE.into());
            }
            let name = toks[0].clone();
            let mut entry = ClassEntry::default();
            for tok in &toks[1..] {
                if let Some(v) = tok.strip_prefix("prefix=") {
                    entry.prefix = Some(unquote(v).to_string());
                } else if let Some(v) = tok.strip_prefix("template=") {
                    entry.template = Some(unquote(v).to_string());
                } else if let Some(kv) = tok.strip_prefix("p:") {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| format!("class default needs p:<key>=<value>: `{tok}`"))?;
                    entry.defaults.insert(k.to_string(), unquote(v).to_string());
                } else {
                    return Err(format!("class: unexpected token `{tok}` ({USAGE})"));
                }
            }
            Item::Directive(GenDirective::Class { name, entry })
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
        "font" => {
            // `font "<path>"` — the doc-wide outline font (Decision 17). The path is
            // double-quoted so it may contain spaces.
            const USAGE: &str = "font \"<path/to/font.ttf>\"";
            let q1 = rest.find('"').ok_or(USAGE)?;
            let q2 = rest[q1 + 1..]
                .find('"')
                .map(|i| q1 + 1 + i)
                .ok_or("font: path is missing its closing quote")?;
            let path = rest[q1 + 1..q2].to_string();
            if path.is_empty() {
                return Err("font: empty path".into());
            }
            Item::Directive(GenDirective::Font { path })
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
        "route" => {
            // `route <net> <slab> w=<width> (x,y) (x,y) ... [free|hint|fixed]`. Net and
            // slab are the two leading bare tokens; `w=` (required) precedes the points;
            // an optional trailing provenance keyword (default `pinned`). The net/slab
            // names are validated at LoadText + commit against the doc (unknown net /
            // unknown-or-non-copper slab → hard `E_UNKNOWN_*`), not here — the parser only
            // shapes the line. `TraceId` is minted by the caller.
            const USAGE: &str = "route <net> <slab> w=<width> (x,y) (x,y) ... [free|hint|fixed]";
            let open = rest.find('(').ok_or(USAGE)?;
            let (prefix, ptspart) = rest.split_at(open);
            // The points run up to a trailing provenance keyword, if any.
            let (ptspart, prov) = split_trailing_prov(ptspart)?;
            let pts = extract_points(ptspart)?;
            if pts.len() < 2 {
                return Err("route needs at least two points (a polyline)".into());
            }
            let toks: Vec<&str> = prefix.split_whitespace().collect();
            let mut net: Option<String> = None;
            let mut layer: Option<String> = None;
            let mut width: Option<Nm> = None;
            for tok in toks {
                if let Some(w) = tok.strip_prefix("w=") {
                    width = Some(parse_len(w)?);
                } else if net.is_none() {
                    net = Some(tok.to_string());
                } else if layer.is_none() {
                    layer = Some(tok.to_string());
                } else {
                    return Err(format!("route: unexpected token `{tok}` ({USAGE})"));
                }
            }
            Item::Route(Trace {
                net: crate::id::NetId::new(net.ok_or("route needs a net name")?),
                layer: layer.ok_or("route needs a copper slab name")?,
                path: pts,
                width: width.ok_or("route needs w=<width>")?,
                prov,
            })
        }
        "via" => {
            // `via <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]`. Net is
            // the leading bare token; the single coordinate; then `drill=`/`pad=` and an
            // optional `<from>..<to>` blind/buried span (default: full copper extent). A
            // trailing provenance keyword (default `pinned`).
            const USAGE: &str =
                "via <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]";
            let open = rest.find('(').ok_or(USAGE)?;
            let (prefix, after) = rest.split_at(open);
            let close = after.find(')').ok_or("unbalanced '(' in coordinate")?;
            let at = {
                let pts = extract_points(&after[..=close])?;
                if pts.len() != 1 {
                    return Err("via needs exactly one coordinate `(x, y)`".into());
                }
                pts[0]
            };
            let net = prefix
                .split_whitespace()
                .next()
                .ok_or("via needs a net name")?;
            let (tail, prov) = split_trailing_prov(&after[close + 1..])?;
            let mut drill: Option<Nm> = None;
            let mut pad: Option<Nm> = None;
            let mut span: Option<(String, String)> = None;
            for tok in tail.split_whitespace() {
                if let Some(d) = tok.strip_prefix("drill=") {
                    drill = Some(parse_len(d)?);
                } else if let Some(p) = tok.strip_prefix("pad=") {
                    pad = Some(parse_len(p)?);
                } else if let Some((from, to)) = tok.split_once("..") {
                    if from.is_empty() || to.is_empty() {
                        return Err(format!("via span must be `<from>..<to>`: `{tok}`"));
                    }
                    span = Some((from.to_string(), to.to_string()));
                } else {
                    return Err(format!("via: unexpected token `{tok}` ({USAGE})"));
                }
            }
            Item::Via(Via {
                net: crate::id::NetId::new(net),
                at,
                span,
                drill: drill.ok_or("via needs drill=<d>")?,
                pad: pad.ok_or("via needs pad=<p>")?,
                prov,
            })
        }
        "refdes" => {
            // `refdes <path> <string>`: two tokens, the value quoted only when it must
            // be (matching `quote_value` on the way out). The string is opaque — no
            // validation against the derived class prefix.
            let toks = split_ws_quoted(rest);
            if toks.len() != 2 {
                return Err("expected: refdes <path> <string>".into());
            }
            let value = unquote(&toks[1]);
            if value.is_empty() {
                return Err("refdes string must be non-empty".into());
            }
            Item::RefdesPin(EntityId::new(&toks[0]), value.to_string())
        }
        other => return Err(format!("unknown directive `{other}`")),
    })
}

/// Strip a trailing provenance keyword (`free`/`hint`/`fixed`) off a route/via line's
/// tail, returning `(remaining, provenance)`. No keyword ⇒ `Pinned` (the default,
/// Decision 18 — hand-authored routing is pinned). The keyword must be the **last**
/// whitespace token; anything else is left for the caller to parse.
fn split_trailing_prov(s: &str) -> Result<(&str, Provenance), String> {
    let t = s.trim_end();
    let prov = match t.rsplit(char::is_whitespace).next() {
        Some("free") => Provenance::Free,
        Some("hint") => Provenance::Hint,
        Some("fixed") => Provenance::Fixed,
        _ => return Ok((s, Provenance::Pinned)),
    };
    // Drop the keyword we just recognised (it is the final token).
    let cut = t.rfind(char::is_whitespace).map(|i| &t[..i]).unwrap_or("");
    Ok((cut, prov))
}

/// Quote a `key=value` token value only when it needs it: whitespace, a `#` (which the
/// comment stripper would otherwise eat), a double quote, or the empty string. Embedded
/// quotes are not escaped — the same documented limitation as `text`-label serialization.
fn quote_value(v: &str) -> String {
    let needs = v.is_empty() || v.chars().any(|c| c.is_whitespace() || c == '#' || c == '"');
    if needs {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

/// Quote a **verbatim `p:` param value** for emission. Like [`quote_value`], but *also*
/// forces quoting when the value starts with `(` — otherwise a literal value such as
/// `(5V)` would serialize bare and re-parse as an *expression* param (M2). A quoted value
/// re-parses as verbatim, closing that round-trip hole. (Expression params are emitted
/// separately as `p:k=(<expr>)` and never pass through here.)
fn quote_param_value(v: &str) -> String {
    if v.starts_with('(') && !quote_value(v).starts_with('"') {
        format!("\"{v}\"")
    } else {
        quote_value(v)
    }
}

/// Split on whitespace, but keep a double-quoted run (which may hold spaces) intact as
/// part of its token — so `p:desc="a b"` is one token. Quote characters are retained;
/// [`unquote`] strips them from an extracted value.
fn split_ws_quoted(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

/// Like [`split_ws_quoted`], but *also* keeps a parenthesized run intact so an
/// expression clause with internal whitespace stays one token (`if=(n > 0)`,
/// `p:c=(i + 1)`). Depth-tracked (nested parens are balanced); a double-quoted run is
/// still respected too, and a `(` inside quotes does not open a paren group. Additive:
/// the plain [`split_ws_quoted`] path is unchanged, so directives that use it behave
/// exactly as before.
fn split_ws_quoted_parens(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut depth: i32 = 0;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                cur.push(c);
            }
            '(' if !in_q => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_q => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            c if c.is_whitespace() && !in_q && depth == 0 => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

/// Split a `[<lo>..<hi>]` **range** suffix off an instance path token. Returns the base
/// path and, when present, the `(lo, hi)` expression texts. Only a suffix containing
/// `..` is a range — an ordinary indexed path (`psu.dec[0]`) has no `..` and comes back
/// unchanged with `None`. The bracket must be the final characters of the token.
fn split_range_suffix(tok: &str) -> (String, Option<(String, String)>) {
    if let Some(open) = tok.rfind('[')
        && tok.ends_with(']')
    {
        let inner = &tok[open + 1..tok.len() - 1];
        if let Some((lo, hi)) = inner.split_once("..") {
            return (
                tok[..open].to_string(),
                Some((lo.trim().to_string(), hi.trim().to_string())),
            );
        }
    }
    (tok.to_string(), None)
}

/// Is the whole value a double-quoted run (`"..."`)? A quoted value is **always
/// verbatim** — never expression-detected (M2): quoting is the escape hatch that lets a
/// literal value legitimately start with `(` (`p:v="(5V)"`). We check quoting on the
/// *raw* token before any unquoting so the layering cannot be inverted.
fn is_quoted(v: &str) -> bool {
    v.len() >= 2 && v.starts_with('"') && v.ends_with('"')
}

/// Classify a raw `p:key=value` value (M2 — quote-then-paren layering):
///   - a **quoted** value is verbatim → `Ok(None)`;
///   - a bare value **starting with `(`** is an *expression*; it must be a single
///     balanced `(...)` spanning the whole value → `Ok(Some(inner))`, else an unbalanced
///     `Err` at parse time (m1 — no deferred eval-time surprise, and no silent
///     "`(1` is verbatim but `(1)` evaluates" inconsistency);
///   - any other bare value is verbatim → `Ok(None)`.
///
/// The parens are the unambiguous "evaluate this" marker (Decision 21b); every existing
/// `p:value=10k` stays verbatim.
fn as_expr_value(raw: &str) -> Result<Option<String>, String> {
    let raw = raw.trim();
    if is_quoted(raw) {
        return Ok(None); // quoted ⇒ always verbatim (M2)
    }
    if !raw.starts_with('(') {
        return Ok(None); // ordinary verbatim value
    }
    // Bare, starts with `(` ⇒ intended as an expression. Require a single balanced group
    // that spans the whole value.
    match balanced_paren_body(raw) {
        Some(inner) => Ok(Some(inner.trim().to_string())),
        None => Err(format!(
            "expression value `{raw}` has unbalanced parentheses (write `(<expr>)`)"
        )),
    }
}

/// Parse an `if=` clause value into its expression text with a **parse-time** balance
/// check (m1). `if=` is *always* an expression, so a quoted value is unquoted (quoting
/// only protects whitespace) and a single surrounding `(...)` is stripped; the result
/// must be parenthesis-balanced, else an `Err` here rather than a deferred eval error.
fn parse_if_clause(raw: &str) -> Result<String, String> {
    let v = if is_quoted(raw) {
        unquote(raw).trim()
    } else {
        raw.trim()
    };
    // Strip one balanced outer pair if the whole thing is `(...)`, so both `if=(n>0)` and
    // `if=n>0` are accepted. Then require the remainder to be balanced.
    let inner = balanced_paren_body(v).unwrap_or_else(|| v.to_string());
    if paren_depth_ok(&inner) {
        Ok(inner.trim().to_string())
    } else {
        Err(format!("`if=` expression `{v}` has unbalanced parentheses"))
    }
}

/// If `s` is exactly one balanced parenthesis group spanning its whole (trimmed) length —
/// `(...)` with the opening `(` matched by the *final* `)` — return the inner text; else
/// `None`. Distinguishes `(a+b)` (a wrapped group) from `(a)+(b)` (two groups, not a
/// single wrapper) and from `(a` (unbalanced).
fn balanced_paren_body(s: &str) -> Option<String> {
    let s = s.trim();
    let inner = s.strip_prefix('(')?.strip_suffix(')')?;
    // Walk the inner text: depth must stay ≥ 0 and never return to 0 before the end
    // (which would mean the opening `(` closed early, so the outer pair is not a single
    // wrapper — e.g. `(a)+(b)`).
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                // Depth going negative means this `)` matched the *outer* `(` before the
                // end — the outer pair is not a single wrapper (e.g. `(a)+(b)`).
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    (depth == 0).then(|| inner.to_string())
}

/// Are the parentheses in `s` balanced (every `(` matched, none closing before opening)?
fn paren_depth_ok(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Is `s` a single expression-tier identifier (an ASCII-ish letters/digits/underscore
/// run that does not start with a digit)? Used to validate a `param` name.
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Strip one surrounding pair of double quotes from a value, if present.
fn unquote(v: &str) -> &str {
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
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
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "sens".into(),
                part: "Sensor".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
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
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "c2".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
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
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "psu.dec[0]".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "mcu".into(),
                part: "MCU".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "sens".into(),
                part: "Sensor".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
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
            // An authored NPTH mounting hole (Decision 16b) — center + diameter round-trip.
            GenDirective::Hole {
                center: Point::mm(5, 45),
                dia: 2_700_000,
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

    // ---- routes state zone (Decision 18) --------------------------------

    use crate::doc::Provenance;
    use crate::id::NetId;

    fn tr(net: &str, layer: &str, path: Vec<Point>, width: Nm, prov: Provenance) -> Trace {
        Trace {
            net: NetId::new(net),
            layer: layer.into(),
            path,
            width,
            prov,
        }
    }

    /// The `# routes` state zone round-trips: a doc carrying pinned/free/hint/fixed
    /// traces and a full-span + a blind/buried via reparses to the same `traces`/`vias`
    /// (ids re-minted in the same BTreeMap order, so the maps compare equal).
    #[test]
    fn routes_round_trip() {
        let mut doc = Doc::default();
        doc.traces.insert(
            TraceId(1),
            tr(
                "GND",
                "F.Cu",
                vec![Point::mm(1, 2), Point::mm(5, 2), Point::mm(5, 8)],
                150_000,
                Provenance::Pinned,
            ),
        );
        doc.traces.insert(
            TraceId(2),
            tr(
                "GND",
                "B.Cu",
                vec![Point::mm(2, 1), Point::mm(2, 9)],
                150_000,
                Provenance::Free,
            ),
        );
        doc.traces.insert(
            TraceId(3),
            tr(
                "VCC",
                "F.Cu",
                vec![Point::mm(0, 0), Point::mm(3, 0)],
                200_000,
                Provenance::Hint,
            ),
        );
        doc.traces.insert(
            TraceId(4),
            tr(
                "VCC",
                "F.Cu",
                vec![Point::mm(0, 5), Point::mm(3, 5)],
                200_000,
                Provenance::Fixed,
            ),
        );
        doc.vias.insert(
            ViaId(1),
            Via {
                net: NetId::new("GND"),
                at: Point::mm(5, 8),
                span: None,
                drill: 300_000,
                pad: 600_000,
                prov: Provenance::Pinned,
            },
        );
        doc.vias.insert(
            ViaId(2),
            Via {
                net: NetId::new("VCC"),
                at: Point::mm(3, 0),
                span: Some(("F.Cu".into(), "In1.Cu".into())),
                drill: 250_000,
                pad: 500_000,
                prov: Provenance::Free,
            },
        );

        let text = serialize(&doc);
        let parsed = parse(&text).expect("parse routes");
        assert_eq!(parsed.traces, doc.traces, "traces round-trip:\n{text}");
        assert_eq!(parsed.vias, doc.vias, "vias round-trip:\n{text}");
        // Idempotent: re-serialize the parsed routes byte-equals.
        let doc2 = Doc {
            traces: parsed.traces,
            vias: parsed.vias,
            ..Default::default()
        };
        assert_eq!(serialize(&doc2), text, "serialize is idempotent");
    }

    /// Provenance keywords (Decision 18): `pinned` is the default and prints nothing;
    /// `free`/`hint`/`fixed` are explicit trailing keywords. Hand-authored (keyword-less)
    /// lines parse as Pinned.
    #[test]
    fn route_provenance_keywords() {
        let mut doc = Doc::default();
        doc.traces.insert(
            TraceId(1),
            tr(
                "N",
                "F.Cu",
                vec![Point::mm(0, 0), Point::mm(1, 0)],
                150_000,
                Provenance::Pinned,
            ),
        );
        doc.traces.insert(
            TraceId(2),
            tr(
                "N",
                "F.Cu",
                vec![Point::mm(0, 1), Point::mm(1, 1)],
                150_000,
                Provenance::Free,
            ),
        );
        let text = serialize(&doc);
        // Pinned prints no keyword; Free prints ` free`.
        let route_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("route ")).collect();
        assert!(
            route_lines[0].ends_with(")") && !route_lines[0].contains("free"),
            "pinned prints no keyword: `{}`",
            route_lines[0]
        );
        assert!(route_lines[1].ends_with(" free"), "free prints the keyword");
        // A hand-authored keyword-less line parses as Pinned.
        let hand = "route N F.Cu w=0.15mm (0, 0) (1mm, 0)";
        let p = parse(hand).expect("parse hand route");
        assert_eq!(p.traces[&TraceId(1)].prov, Provenance::Pinned);
    }

    /// A blind/buried via's explicit `<from>..<to>` span parses (Decision 18 — parseable
    /// today even though multilayer stackups are rare).
    #[test]
    fn via_blind_span_parses() {
        let p = parse("via SIG (2mm, 3mm) drill=0.25mm pad=0.5mm F.Cu..In1.Cu free")
            .expect("parse blind via");
        let v = &p.vias[&ViaId(1)];
        assert_eq!(v.span, Some(("F.Cu".into(), "In1.Cu".into())));
        assert_eq!(v.prov, Provenance::Free);
        assert_eq!(v.drill, 250_000);
    }

    /// A routeless doc serializes byte-identically to before this feature (no `# routes`
    /// section), so existing files are undisturbed.
    #[test]
    fn no_routes_no_section() {
        let doc = placed(uart_link());
        assert!(
            !serialize(&doc).contains("# routes"),
            "a routeless doc emits no routes section"
        );
    }

    // ---- round-trip + idempotence ---------------------------------------

    /// `parse(serialize(doc))` reproduces `(source, overrides, refdes_pins)` exactly,
    /// for a source that touches every directive variant, both override strengths, and
    /// refdes pins — including an entity (`mcu`) carrying both a pos pin and a refdes
    /// pin, to exercise the interleaved override section.
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
        let mut doc = doc_of(all_variants(), overrides);
        doc.refdes_pins
            .insert(EntityId::new("psu.dec[0]"), "C7".into());
        doc.refdes_pins.insert(EntityId::new("mcu"), "U3".into());

        let text = serialize(&doc);
        let Parsed {
            source: src,
            overrides: ovr,
            refdes_pins: rd,
            ..
        } = parse(&text).expect("parse");
        assert_eq!(src, doc.source, "source must round-trip");
        assert_eq!(ovr, doc.overrides, "overrides must round-trip");
        assert_eq!(rd, doc.refdes_pins, "refdes pins must round-trip");
    }

    /// A refdes value is opaque (Decision 14), so it may hold whitespace or a `#`; both
    /// must survive serialize→parse via the quote-aware machinery (`quote_value` wraps,
    /// the quote-aware comment strip keeps a quoted `#` literal).
    #[test]
    fn refdes_value_with_whitespace_and_hash_round_trips() {
        let mut doc = doc_of(Vec::new(), BTreeMap::new());
        doc.refdes_pins
            .insert(EntityId::new("a"), "TEST POINT".into());
        doc.refdes_pins.insert(EntityId::new("b"), "X#1".into());
        let Parsed {
            refdes_pins: rd, ..
        } = parse(&serialize(&doc)).expect("parse");
        assert_eq!(rd, doc.refdes_pins);
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
        let Parsed { source: src, .. } = parse(text).expect("parse");
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
        assert_eq!(parse(&serialize(&doc)).unwrap().source, src);
    }

    /// An `inst` directive carrying a display label and identity params parses to the
    /// expected `Instance` (params in `BTreeMap` order, values unquoted) and round-trips.
    /// A quoted value with spaces and a `#` survives (the `#` is not a comment here).
    #[test]
    fn inst_with_params_and_label_round_trips() {
        let text = "inst r1 R_0402 label=\"{value:si:Ω}\" p:tol=5% p:value=4.7k";
        let Parsed { source: src, .. } = parse(text).expect("parse");
        let mut params = BTreeMap::new();
        params.insert("tol".into(), "5%".into());
        params.insert("value".into(), "4.7k".into());
        assert_eq!(
            src,
            vec![GenDirective::Instance {
                path: "r1".into(),
                part: "R_0402".into(),
                params,
                label: Some("{value:si:Ω}".into()),
            }]
        );
        // Canonical serialization re-parses to the same source.
        let doc = doc_of(src.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc)).unwrap().source, src);

        // A quoted param value with a space and a `#` round-trips (not a comment).
        let text2 = "inst u1 MCU p:desc=\"dual # buck\"";
        let Parsed { source: src2, .. } = parse(text2).expect("parse2");
        let doc2 = doc_of(src2.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc2)).unwrap().source, src2);
        if let GenDirective::Instance { params, .. } = &src2[0] {
            assert_eq!(params["desc"], "dual # buck");
        } else {
            panic!("expected Instance");
        }

        // Bare `inst <path> <part>` still parses with empty/None defaults.
        let Parsed { source: bare, .. } = parse("inst q1 NPN").expect("bare");
        assert_eq!(
            bare,
            vec![GenDirective::Instance {
                path: "q1".into(),
                part: "NPN".into(),
                params: BTreeMap::new(),
                label: None,
            }]
        );
    }

    /// A `param` directive (Decision 21b) parses to `GenDirective::Param` and serializes
    /// as authored (the expression text is emitted verbatim, never pre-evaluated).
    #[test]
    fn param_directive_parses_and_round_trips() {
        let text = "param n = 3\nparam gap = n + 1";
        let Parsed { source: src, .. } = parse(text).expect("parse");
        assert_eq!(
            src,
            vec![
                GenDirective::Param {
                    name: "n".into(),
                    expr: "3".into(),
                },
                GenDirective::Param {
                    name: "gap".into(),
                    expr: "n + 1".into(),
                },
            ]
        );
        let doc = doc_of(src.clone(), BTreeMap::new());
        assert_eq!(serialize(&doc).trim(), "param n = 3\nparam gap = n + 1");
        assert_eq!(parse(&serialize(&doc)).unwrap().source, src);
        // A malformed `param` (no `=`, empty name, non-identifier) is rejected.
        assert!(parse("param n 3").is_err());
        assert!(parse("param = 3").is_err());
        assert!(parse("param 1n = 3").is_err());
    }

    /// A generative `inst` — a `[lo..hi]` range, an `if=` conditional, and expression
    /// `p:(...)` params — parses to `GenDirective::InstGenerative` and round-trips as
    /// authored (evaluated results are elaboration-only, never serialized).
    #[test]
    fn generative_inst_parses_and_round_trips() {
        let text = "inst sense[0..n] R_0402 if=(i < 3) p:idx=(i + 1) p:tol=5%";
        let Parsed { source: src, .. } = parse(text).expect("parse");
        let mut params = BTreeMap::new();
        params.insert("tol".into(), "5%".into());
        let mut param_exprs = BTreeMap::new();
        param_exprs.insert("idx".into(), "i + 1".into());
        assert_eq!(
            src,
            vec![GenDirective::InstGenerative {
                path: "sense".into(),
                part: "R_0402".into(),
                params,
                param_exprs,
                label: None,
                range: Some(("0".into(), "n".into())),
                if_expr: Some("i < 3".into()),
            }]
        );
        let doc = doc_of(src.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc)).unwrap().source, src);

        // An ordinary indexed path (`dec[0]`, no `..`) is NOT a range — it stays a plain
        // Instance, so existing docs are untouched.
        let Parsed { source: plain, .. } = parse("inst dec[0] Cap").expect("plain");
        assert_eq!(
            plain,
            vec![GenDirective::Instance {
                path: "dec[0]".into(),
                part: "Cap".into(),
                params: BTreeMap::new(),
                label: None,
            }]
        );
    }

    /// Documented limitation: a param value containing `" ` (a double quote followed by
    /// whitespace) serializes WITHOUT escaping the inner quote — `p:x="a" b"` — so the
    /// tokenizer closes the quoted run at the inner `"` and the trailing `b"` is an
    /// orphan token: the output does not reparse. Pinned here (the same limitation as
    /// `text`-label serialization) so a future escaping fix updates this test on purpose.
    #[test]
    fn embedded_double_quote_is_a_documented_serialize_limitation() {
        let mut params = BTreeMap::new();
        params.insert("x".to_string(), "a\" b".to_string());
        let doc = doc_of(
            vec![GenDirective::Instance {
                path: "u1".into(),
                part: "MCU".into(),
                params,
                label: None,
            }],
            BTreeMap::new(),
        );
        let text = serialize(&doc);
        assert!(
            text.contains("p:x=\"a\" b\""),
            "unescaped inner quote expected: {text}"
        );
        assert!(
            parse(&text).is_err(),
            "embedded `\" ` value is not round-trippable (documented limitation)"
        );
    }

    /// Elaboration copies an instance's `params`/`label` verbatim onto its `Component`.
    #[test]
    fn elaboration_copies_params_and_label_onto_component() {
        let Parsed { source: src, .. } =
            parse("inst c1 Cap label=\"{value}\" p:value=100n").expect("parse");
        let doc = placed(src);
        let c = &doc.components[&EntityId::new("c1")];
        assert_eq!(c.label.as_deref(), Some("{value}"));
        assert_eq!(c.params["value"], "100n");
    }

    /// Range instantiation (Decision 21b): `inst dec[0..n] Cap` with `param n = 3`
    /// elaborates to concrete `dec[0]`, `dec[1]`, `dec[2]` components — hi exclusive.
    #[test]
    fn range_expands_to_indexed_instances() {
        let src = "param n = 3\ninst dec[0..n] Cap p:value=(100n)";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert!(doc.components.contains_key(&EntityId::new("dec[0]")));
        assert!(doc.components.contains_key(&EntityId::new("dec[1]")));
        assert!(doc.components.contains_key(&EntityId::new("dec[2]")));
        assert!(
            !doc.components.contains_key(&EntityId::new("dec[3]")),
            "hi exclusive"
        );
        // The expression param evaluated onto each instance's verbatim params.
        assert_eq!(
            doc.components[&EntityId::new("dec[0]")].params["value"],
            "100n"
        );
    }

    /// The loop variable `i` is bound in each range instance's expressions.
    #[test]
    fn loop_variable_binds_in_range_expressions() {
        let src = "inst r[0..3] Cap p:idx=(i + 1)";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert_eq!(doc.components[&EntityId::new("r[0]")].params["idx"], "1");
        assert_eq!(doc.components[&EntityId::new("r[1]")].params["idx"], "2");
        assert_eq!(doc.components[&EntityId::new("r[2]")].params["idx"], "3");
    }

    /// Changing a range bound preserves surviving instances' identities and decays the
    /// removed one through the existing reconciliation machinery (the reconciliation-
    /// safety requirement). An override pinned to `dec[1]` survives `n: 3→4`, and one
    /// pinned to `dec[3]` orphans (surfaced, never silently dropped) when `n: 4→3`.
    #[test]
    fn range_bound_change_reconciles_by_path() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        // Start at n=3 (dec[0..3]); pin dec[1].
        let Parsed { source: s3, .. } = parse("param n = 3\ninst dec[0..n] Cap").expect("parse");
        h.commit(Transaction::one(Command::SetSource(s3)), &lib, "n3")
            .unwrap();
        h.commit(
            Transaction::one(Command::Pin(EntityId::new("dec[1]"), Point::mm(7, 3))),
            &lib,
            "pin",
        )
        .unwrap();
        assert_eq!(
            h.doc().components[&EntityId::new("dec[1]")].pos.value,
            Point::mm(7, 3),
            "pin holds dec[1] at n=3"
        );
        // Grow to n=4: dec[1]'s identity (and its pin) survives; dec[3] now exists.
        let Parsed { source: s4, .. } = parse("param n = 4\ninst dec[0..n] Cap").expect("parse4");
        h.commit(Transaction::one(Command::SetSource(s4)), &lib, "n4")
            .unwrap();
        assert!(h.doc().components.contains_key(&EntityId::new("dec[3]")));
        assert_eq!(
            h.doc().components[&EntityId::new("dec[1]")].pos.value,
            Point::mm(7, 3),
            "the pin on dec[1] survives the bound change (identity by path)"
        );
        assert!(h.doc().report.orphaned.is_empty());
        // Shrink back to n=3: dec[3] is gone; a pin on it (add one first) would orphan.
        h.commit(
            Transaction::one(Command::Pin(EntityId::new("dec[3]"), Point::mm(9, 9))),
            &lib,
            "pin3",
        )
        .unwrap();
        let Parsed { source: s3b, .. } = parse("param n = 3\ninst dec[0..n] Cap").expect("parse3b");
        h.commit(Transaction::one(Command::SetSource(s3b)), &lib, "shrink")
            .unwrap();
        assert!(!h.doc().components.contains_key(&EntityId::new("dec[3]")));
        assert!(
            h.doc().report.orphaned.contains(&EntityId::new("dec[3]")),
            "the removed instance's override is surfaced as an orphan, not dropped"
        );
    }

    /// `if=` population conditional: a false condition depopulates the instance, and a
    /// connection referencing the dropped part is skipped with a `W_DNP` warning (the
    /// chosen dangling-connection semantics) rather than an `E_UNKNOWN_INSTANCE` error.
    #[test]
    fn if_conditional_depopulates_and_dangles_as_warning() {
        // if=true keeps it.
        let Parsed { source: on, .. } = parse("inst c1 Cap if=(true)").expect("on");
        assert!(placed(on).components.contains_key(&EntityId::new("c1")));

        // if=false drops it; a net referencing it warns (W_DNP), does not error.
        let src = "param populate = false\n\
                   inst c1 Cap\n\
                   inst c2 Cap if=populate\n\
                   net GND c1.p2 c2.p2";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert!(doc.components.contains_key(&EntityId::new("c1")));
        assert!(
            !doc.components.contains_key(&EntityId::new("c2")),
            "c2 is depopulated by if=false"
        );
        // The net referencing c2 is surfaced as a DNP dangle (a warning), and c1 still
        // joins GND (the surviving pin is unaffected).
        assert!(
            doc.report.dnp_dangling.iter().any(|(_, p)| p == "c2"),
            "dangling connection to c2 recorded: {:?}",
            doc.report.dnp_dangling
        );
        let gnd = &doc.nets[&crate::id::NetId::new("GND")];
        assert!(gnd.members.iter().any(|m| m.comp.as_str() == "c1"));
        assert!(!gnd.members.iter().any(|m| m.comp.as_str() == "c2"));
    }

    /// A QUOTED `p:` value is always verbatim, even when it starts with `(` (M2 — the
    /// escape hatch): `p:v="(5V)"` stores the literal `(5V)` and round-trips, while a
    /// bare `p:v=(5)` is an expression.
    #[test]
    fn quoted_paren_value_is_verbatim_not_an_expression() {
        let Parsed { source, .. } = parse("inst c1 Cap p:v=\"(5V)\"").expect("parse");
        // Quoted ⇒ verbatim ⇒ stays a plain Instance with the literal value.
        assert_eq!(
            source,
            vec![GenDirective::Instance {
                path: "c1".into(),
                part: "Cap".into(),
                params: {
                    let mut m = BTreeMap::new();
                    m.insert("v".into(), "(5V)".into());
                    m
                },
                label: None,
            }]
        );
        let doc = doc_of(source.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc)).unwrap().source, source);
        // A bare `(...)` IS an expression (routes to InstGenerative).
        let Parsed { source: ex, .. } = parse("inst c1 Cap p:v=(5)").expect("expr");
        assert!(matches!(ex[0], GenDirective::InstGenerative { .. }));
    }

    /// Unbalanced parentheses on the expression path are a PARSE-time error (m1), not a
    /// deferred eval error — and `(1` no longer silently stays verbatim.
    #[test]
    fn unbalanced_parens_error_at_parse_time() {
        assert!(parse("inst c1 Cap p:v=(1").is_err()); // bare `(1` — was silently verbatim
        assert!(parse("inst c1 Cap if=(n > 0").is_err()); // unbalanced if=
        // A well-formed expression still parses.
        assert!(parse("inst c1 Cap p:v=(1)").is_ok());
        assert!(parse("inst c1 Cap if=(n > 0)").is_ok());
    }

    /// `if=(…)` re-serializes as the canonical paren form `if=(…)` (m2), not re-quoted.
    #[test]
    fn if_clause_serializes_as_canonical_parens() {
        let Parsed { source, .. } = parse("inst c1 Cap if=(n > 0)").expect("parse");
        let doc = doc_of(source.clone(), BTreeMap::new());
        let text = serialize(&doc);
        assert!(text.contains("if=(n > 0)"), "canonical paren form: {text}");
        assert!(!text.contains("if=\""), "not re-quoted: {text}");
        assert_eq!(parse(&text).unwrap().source, source);
    }

    /// A range's loop variable `i` shadows a doc-level `param i` (innermost wins) —
    /// deterministic, per the documented rule.
    #[test]
    fn range_loop_variable_shadows_doc_level_param() {
        let src = "param i = 99\ninst r[0..2] Cap p:idx=(i)";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        // Inside the range, `i` is the loop index (0, 1), not the doc-level 99.
        assert_eq!(doc.components[&EntityId::new("r[0]")].params["idx"], "0");
        assert_eq!(doc.components[&EntityId::new("r[1]")].params["idx"], "1");
    }

    /// A PLACEMENT directive referencing a depopulated part is folded into the same
    /// `W_DNP` dangling report as a connection (symmetric visibility), not silently
    /// vanished.
    #[test]
    fn placement_ref_to_depopulated_part_warns() {
        let src = "inst anchor Cap\n\
                   inst c1 Cap if=(false)\n\
                   near c1 anchor 3mm";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert!(!doc.components.contains_key(&EntityId::new("c1")));
        assert!(
            doc.report.dnp_dangling.iter().any(|(_, p)| p == "c1"),
            "placement ref to c1 recorded as DNP dangle: {:?}",
            doc.report.dnp_dangling
        );
    }

    /// Every `E_EXPR` fault class aborts the commit (collect-all structural fault).
    #[test]
    fn expression_faults_abort_the_commit() {
        let lib = part_library();
        let commit = |src: &str| -> Result<(), Vec<crate::diagnostic::Diagnostic>> {
            let Parsed { source, .. } = parse(src).expect("parse");
            let mut h = History::new(Default::default());
            h.commit(Transaction::one(Command::SetSource(source)), &lib, "x")
                .map(|_| ())
        };
        // unknown param
        assert!(commit("inst r[0..missing] Cap").is_err());
        // param cycle
        assert!(commit("param a = b + 1\nparam b = a + 1\ninst r[0..a] Cap").is_err());
        // type mismatch (bool as a range bound)
        assert!(commit("param f = true\ninst r[0..f] Cap").is_err());
        // inexact division in a param value
        assert!(commit("inst r1 Cap p:v=(1 / 3)").is_err());
        // negative bound
        assert!(commit("inst r[0..-1] Cap").is_err());
        // over the range cap
        assert!(commit("inst r[0..100000] Cap").is_err());
        // if= not a boolean
        assert!(commit("inst r1 Cap if=(1 + 1)").is_err());
    }

    /// A `class` directive parses to the expected `Class { name, ClassEntry }` (prefix,
    /// template, and `p:`-namespaced defaults) and round-trips through `serialize`.
    #[test]
    fn class_directive_parses_and_round_trips() {
        let text = "class R prefix=RES template=\"{value:si:Ω}\" p:tol=5%";
        let Parsed { source: src, .. } = parse(text).expect("parse");
        let mut defaults = BTreeMap::new();
        defaults.insert("tol".into(), "5%".into());
        assert_eq!(
            src,
            vec![GenDirective::Class {
                name: "R".into(),
                entry: ClassEntry {
                    prefix: Some("RES".into()),
                    template: Some("{value:si:Ω}".into()),
                    defaults,
                },
            }]
        );
        let doc = doc_of(src.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc)).unwrap().source, src);

        // A bare `class <name>` (all fields defaulted) also round-trips.
        let Parsed { source: bare, .. } = parse("class LED").expect("bare");
        assert_eq!(
            bare,
            vec![GenDirective::Class {
                name: "LED".into(),
                entry: ClassEntry::default(),
            }]
        );
        let doc2 = doc_of(bare.clone(), BTreeMap::new());
        assert_eq!(parse(&serialize(&doc2)).unwrap().source, bare);
    }

    /// A region directive parses to the expected `RegionDecl` (role, net, layer, and
    /// points), and the inner-layer / keep-out-kind tokens round-trip.
    #[test]
    fn region_directive_parses_and_round_trips() {
        let text = "\
region conductor net=GND layer=B.Cu (0mm, 0mm) (10mm, 0mm) (10mm, 10mm) (0mm, 10mm)
region keepout-drill layer=In2.Cu (1mm, 1mm) (2mm, 1mm) (2mm, 2mm)";
        let Parsed { source: src, .. } = parse(text).expect("parse");
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
        assert_eq!(parse(&serialize(&doc)).unwrap().source, src);
    }

    /// A `text` directive parses to the expected `GenDirective::Text` and round-trips,
    /// with and without `rot=`. A quoted string containing a space survives intact.
    #[test]
    fn text_directive_parses_and_round_trips() {
        let text = "\
text \"R12\" (0mm, 0mm) h=1mm layer=F.SilkS
text \"VAL 3V3\" (2mm, 5mm) h=0.8mm layer=B.SilkS rot=90";
        let Parsed { source: src, .. } = parse(text).expect("parse");
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
        assert_eq!(parse(&canon).unwrap().source, src);
    }

    #[test]
    fn font_directive_parses_and_round_trips() {
        // `font "<path>"` — the doc-wide outline font (Decision 17); the path may contain
        // spaces (quoted).
        let text = "font \"/usr/share/fonts/My Font.ttf\"";
        let Parsed { source: src, .. } = parse(text).expect("parse");
        assert_eq!(
            src[0],
            GenDirective::Font {
                path: "/usr/share/fonts/My Font.ttf".into(),
            }
        );
        let canon = serialize(&doc_of(src.clone(), BTreeMap::new()));
        assert!(canon.contains("font \""), "font token:\n{canon}");
        assert_eq!(parse(&canon).unwrap().source, src);
    }

    #[test]
    fn text_string_may_contain_a_hash() {
        // A `#` inside a quoted text label is literal, not a comment (quote-aware strip),
        // so it round-trips. (`#` outside quotes still starts a comment.)
        let Parsed { source: src, .. } =
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
            parse(&canon).unwrap().source,
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
        let Parsed { source: src, .. } = parse(text).expect("parse");
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
            parse(&canon).unwrap().source,
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
        let Parsed { source: src, .. } = parse(text).expect("parse");
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
        assert_eq!(parse(&canon).unwrap().source, src);
    }

    #[test]
    fn bezier_edges_parse_and_round_trip() {
        // A region with one quadratic and one cubic edge, mixed with straight edges.
        let text = "\
region conductor layer=F.Cu (0mm, 0mm) quad (2mm, 3mm) (4mm, 0mm) cubic (5mm, 2mm) (7mm, 2mm) (8mm, 0mm) (0mm, 4mm)";
        let Parsed { source: src, .. } = parse(text).expect("parse");
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
        assert_eq!(parse(&canon).unwrap().source, src);
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
                params: std::collections::BTreeMap::new(),
                label: None,
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
        let Parsed {
            source: src,
            overrides: ovr,
            ..
        } = parse(&once).unwrap();
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
        let Parsed { source: src, .. } = parse(text).unwrap();
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
        let Parsed {
            source: src,
            overrides: ovr,
            refdes_pins: rp,
            ..
        } = parse(&serialize(doc)).expect("parse");
        let elab = elaborate(&src, &ovr, &rp, &lib).expect("elaborate");
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
                params: std::collections::BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "dec".into(),
                part: "Cap".into(),
                params: std::collections::BTreeMap::new(),
                label: None,
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
        let Parsed { source: src, .. } = parse("rotate u1 -90\nnearpin c1 u1.VOUT 1.5mm").unwrap();
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
        let Parsed { source: src, .. } = parse("rotate u1 30").unwrap();
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
        assert_eq!(parse(&canon).unwrap().source, src);
        // A cardinal still serialises readably (and `bottom` survives).
        assert_eq!(
            render_directive(&parse("rotate u1 90 bottom").unwrap().source[0]),
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
        let spaced = parse("rotate u1 quat=(1, 0, 0, 1)").unwrap().source;
        let tight = parse("rotate u1 quat=(1,0,0,1)").unwrap().source;
        assert_eq!(spaced, tight);
        assert!(parse("rotate u1 quat=(0,0,0,0)").is_err());
    }

    #[test]
    fn rotate_bottom_authoring_round_trips() {
        let Parsed { source: src, .. } = parse("rotate u1 90 bottom").unwrap();
        assert_eq!(
            src[0],
            GenDirective::Rotate {
                path: "u1".into(),
                orient: Orient::from_deg(90).unwrap().flipped(),
            }
        );
        // Canonical serialization carries the `bottom` flag and re-parses identically.
        assert_eq!(render_directive(&src[0]), "rotate u1 90 bottom");
        assert_eq!(parse("rotate u1 90").unwrap().source[0], {
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

    /// A `hole` parses with a positive diameter and round-trips; a zero or negative
    /// diameter is rejected at parse (a degenerate/negative drill tool must not slip
    /// silently into the Excellon output).
    #[test]
    fn hole_requires_positive_diameter() {
        let ok = parse("hole (4mm, 4mm) dia=2.7mm").unwrap();
        assert_eq!(ok.source.len(), 1, "one hole directive");
        assert!(
            parse("hole (4mm, 4mm) dia=0mm").is_err(),
            "zero diameter rejected"
        );
        let e = crate::diagnostic::render(&parse("hole (4mm, 4mm) dia=-1mm").unwrap_err());
        assert!(e.contains("must be positive"), "negative rejected: {e}");
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

    // ---- coordinate-range ceiling (issue 0018) ---------------------------

    /// A point beyond ±MAX_COORD (1 m) is a hard `E_COORD_RANGE` error at the text
    /// boundary — never a silent i128 wrap in the geometry kernel downstream.
    #[test]
    fn parse_rejects_out_of_range_point() {
        let diags = parse("place foo (2000mm, 0)").unwrap_err();
        assert!(
            diags.iter().any(|d| d.code == "E_COORD_RANGE"),
            "expected E_COORD_RANGE: {diags:?}"
        );
    }

    /// An oversized length (a text height here) is caught too — the walker bounds
    /// every nm a directive contributes, not only point coordinates.
    #[test]
    fn parse_rejects_out_of_range_height() {
        let diags = parse(r#"text "A" (0mm, 0mm) h=2000mm layer=F.SilkS"#).unwrap_err();
        assert!(
            diags.iter().any(|d| d.code == "E_COORD_RANGE"),
            "expected E_COORD_RANGE: {diags:?}"
        );
    }

    /// A coordinate exactly at the bound (1 m = MAX_COORD) is accepted; the ceiling
    /// is inclusive, so real board-scale geometry is never rejected.
    #[test]
    fn parse_accepts_coordinate_at_the_bound() {
        assert!(
            parse("place foo (1000mm, 0)").is_ok(),
            "1 m = MAX_COORD must be accepted"
        );
    }

    /// The command surface enforces the same ceiling as the text parser: an
    /// out-of-range `Pin` position is rejected with `E_COORD_RANGE`, so the geometry
    /// kernel never sees a coordinate that could overflow i128 (issue 0018).
    #[test]
    fn command_ingress_rejects_out_of_range_pin() {
        let lib = part_library();
        let doc = Doc::default();
        let err = crate::command::apply(
            &doc,
            &Transaction::one(Command::Pin(EntityId::new("x"), Point::mm(2000, 0))),
            &lib,
            1,
        )
        .unwrap_err();
        assert!(
            err.iter().any(|d| d.code == "E_COORD_RANGE"),
            "expected E_COORD_RANGE: {err:?}"
        );
    }

    // ---- nested block grammar (Phase 0 infrastructure) -------------------

    /// A leaf block, for building expected trees compactly in assertions.
    fn leaf(header: &str, line: u32) -> Block {
        let (keyword, tokens, rest) = split_header(header);
        Block {
            keyword,
            tokens,
            rest,
            opened_block: false,
            children: Vec::new(),
            line,
        }
    }

    /// The nested block within a body node, for terse child assertions.
    fn as_block(n: &Node) -> &Block {
        match n {
            Node::Block(b) => b,
            other => panic!("expected a Node::Block, got {other:?}"),
        }
    }

    /// Nesting to 3+ levels builds the expected tree, with header tokens pre-split and
    /// children in source order. (No keyword accepts a block yet, so this exercises the
    /// generic representation via `parse_blocks`, not `parse`.)
    #[test]
    fn blocks_nest_to_arbitrary_depth() {
        let text = "\
row main gap=2mm {
  column left {
    def inner {
      inst r1 R
    }
  }
  inst c1 Cap
}";
        let forest = parse_blocks(text).expect("parse_blocks");
        // Top level: one `row` opener.
        assert_eq!(forest.len(), 1);
        let row = &forest[0];
        assert_eq!(row.keyword, "row");
        assert_eq!(row.tokens, vec!["row", "main", "gap=2mm"]);
        assert_eq!(row.rest, "main gap=2mm");
        assert!(row.opened_block);
        // `row` has two children: `column` (a block) then `inst c1` (a leaf), in order.
        assert_eq!(row.children.len(), 2);
        let col = as_block(&row.children[0]);
        assert_eq!(col.keyword, "column");
        assert!(col.opened_block);
        assert_eq!(row.children[1], Node::Block(leaf("inst c1 Cap", 7)));
        // `column` -> `def` (block) -> `inst r1` (leaf), 3 levels below the row.
        let def = as_block(&col.children[0]);
        assert_eq!(def.keyword, "def");
        assert!(def.opened_block);
        assert_eq!(def.children, vec![Node::Block(leaf("inst r1 R", 4))]);
    }

    /// An empty block still round-trips as a block (opened_block true, no children) —
    /// the distinction the flat path relies on to reject an empty block on a keyword
    /// that does not take one.
    #[test]
    fn empty_block_is_still_a_block() {
        let forest = parse_blocks("def empty {\n}").expect("parse");
        assert_eq!(forest.len(), 1);
        assert!(forest[0].opened_block);
        assert!(forest[0].children.is_empty());
    }

    /// Comments and blank lines *inside* a block are preserved as trivia nodes, in
    /// order, and round-trip byte-faithfully through serialize -> parse -> serialize
    /// (Decision 21 mixed authorship). Top-level trivia stays dropped (the flat path's
    /// pre-existing behavior).
    #[test]
    fn block_interior_trivia_round_trips() {
        let text = "\
# top-level comment (dropped, as always)
def amp {
  # bias network
  inst r1 R

  # decoupling
  inst c1 Cap
}
";
        let forest = parse_blocks(text).expect("parse");
        let def = &forest[0];
        // The body preserves the two comments, the blank, and two directives in order.
        assert_eq!(
            def.children,
            vec![
                Node::Comment("bias network".into()),
                Node::Block(leaf("inst r1 R", 4)),
                Node::Blank,
                Node::Comment("decoupling".into()),
                Node::Block(leaf("inst c1 Cap", 7)),
            ]
        );
        // Round-trip: the canonical form (top-level comment stripped) is a fixed point,
        // and the interior trivia survives byte-for-byte.
        let canon = serialize_blocks(&forest);
        let expected = "\
def amp {
  # bias network
  inst r1 R

  # decoupling
  inst c1 Cap
}
";
        assert_eq!(
            canon, expected,
            "interior trivia round-trips byte-faithfully"
        );
        // Structural fixpoint: re-parsing the canonical form and re-serializing is a
        // fixed point. (The tree carries source line numbers, which legitimately differ
        // between the original — with its dropped top-level comment on line 1 — and the
        // canonical form; the byte-identity above is the faithful-round-trip guarantee.)
        let reforest = parse_blocks(&canon).unwrap();
        assert_eq!(
            serialize_blocks(&reforest),
            canon,
            "canonical form is a fixpoint"
        );
    }

    /// An unbalanced `{` (a block never closed) is an `E_BLOCK` error located at the
    /// opener's line.
    #[test]
    fn unbalanced_open_is_an_error() {
        let err = parse_blocks("row a {\n  inst r1 R\n").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
        let rendered = crate::diagnostic::render(&err);
        assert!(rendered.contains("1:1"), "located at opener: {rendered}");
        assert!(
            rendered.contains("never closed"),
            "names the failure: {rendered}"
        );
    }

    /// A `}` with no open block is an `E_BLOCK` error located at the stray close.
    #[test]
    fn stray_close_is_an_error() {
        let err = parse_blocks("inst r1 R\n}").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
        let rendered = crate::diagnostic::render(&err);
        assert!(
            rendered.contains("2:1"),
            "located at the stray `}}`: {rendered}"
        );
        assert!(
            rendered.contains("no open block"),
            "names the failure: {rendered}"
        );
    }

    /// Braces inside a quoted value are literal: a trailing `{` inside quotes does not
    /// open a block, and `{`/`}` within a quoted run do not confuse balancing.
    #[test]
    fn braces_inside_quotes_are_literal() {
        // Trailing `{` inside a quoted value: NOT a block opener.
        let forest = parse_blocks("inst r1 R label=\"a { b\"").expect("parse");
        assert_eq!(forest.len(), 1);
        assert!(
            !forest[0].opened_block,
            "a `{{` inside quotes must not open a block"
        );
        // A lone-looking `}` that is actually inside a quoted value is not a close: the
        // whole thing is one directive, and the quoted braces are preserved verbatim.
        let forest = parse_blocks("text \"x{y}z\" (0,0) h=1mm").expect("parse");
        assert_eq!(forest.len(), 1);
        assert!(!forest[0].opened_block);
        assert!(forest[0].rest.contains("x{y}z"), "quoted braces preserved");
    }

    /// Comment stripping is quote-aware and runs *before* brace detection: a `{` after
    /// a `#` comment is stripped away and never opens a block; a `{` before a comment
    /// still opens one.
    #[test]
    fn brace_after_comment_does_not_open_a_block() {
        // `{` lives in the comment: stripped, so no block opens.
        let forest = parse_blocks("inst r1 R  # note { not a block").expect("parse");
        assert_eq!(forest.len(), 1);
        assert!(!forest[0].opened_block, "commented `{{` is not an opener");
        // `{` before the comment DOES open a block (comment stripped off the tail first).
        let forest = parse_blocks("row a {  # opens here\n}").expect("parse");
        assert_eq!(forest.len(), 1);
        assert!(forest[0].opened_block, "pre-comment `{{` opens a block");
    }

    /// No existing keyword accepts a block, so a block opened on a current keyword is a
    /// hard parse error through the full `parse` surface — existing documents are
    /// unchanged, and a stray block cannot silently become an empty directive.
    #[test]
    fn block_on_existing_keyword_is_rejected() {
        let err = parse("inst r1 R {\n}").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
        let rendered = crate::diagnostic::render(&err);
        assert!(
            rendered.contains("does not take a block"),
            "clear message: {rendered}"
        );
        // The children of the rejected block are not lowered as directives.
        assert!(
            !rendered.contains("unknown directive"),
            "children not descended into: {rendered}"
        );
    }

    /// serialize -> parse -> serialize is a fixed point for a block tree, with canonical
    /// two-space-per-depth indentation.
    #[test]
    fn block_serialize_is_a_fixpoint() {
        let text = "\
row main gap=2mm {
  column left {
    inst r1 R
  }
  inst c1 Cap
}
inst top MCU
";
        let forest = parse_blocks(text).expect("parse");
        let once = serialize_blocks(&forest);
        // Canonical indentation is exactly what we authored above.
        assert_eq!(once, text, "canonical two-space indent per depth");
        let reforest = parse_blocks(&once).expect("re-parse");
        let twice = serialize_blocks(&reforest);
        assert_eq!(once, twice, "serialize is a fixed point");
        assert_eq!(forest, reforest, "the tree round-trips structurally");
    }

    /// The flat (blockless) document path is byte-for-byte unchanged: a full-coverage
    /// source serialized by the `Doc` serializer parses back through the new
    /// block-aware `parse` identically to before.
    #[test]
    fn flat_documents_are_unchanged_through_blocks() {
        let doc = doc_of(all_variants(), BTreeMap::new());
        let text = serialize(&doc);
        // parse -> the same source, and the flat forest has no openers.
        assert_eq!(parse(&text).unwrap().source, doc.source);
        let forest = parse_blocks(&text).expect("parse_blocks");
        assert!(
            forest.iter().all(|b| !b.opened_block),
            "a canonical Doc serialization contains no blocks"
        );
    }

    /// A block opener with no directive before `{` (e.g. a lone `{`) is rejected by
    /// `parse_blocks` itself — the public API guardrail (finding 4), so a malformed
    /// opener never reaches a consumer nor serializes to a leading-space line.
    #[test]
    fn empty_keyword_block_is_rejected_by_parse_blocks() {
        let err = parse_blocks("{\n}").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_BLOCK"), "got: {err:?}");
        let rendered = crate::diagnostic::render(&err);
        assert!(
            rendered.contains("no directive before"),
            "clear message: {rendered}"
        );
    }

    /// Collect-all through a rejected block: an unaccepted block's *children* are still
    /// line-parsed, so their own syntax errors surface in the same pass as the
    /// `E_BLOCK` rejection (finding 5) — the author fixes both at once, not in two
    /// rounds.
    #[test]
    fn rejected_block_still_reports_child_errors() {
        // `inst` does not take a block; its child is itself a bad line.
        let err = parse("inst u1 MCU {\n  frobnicate x\n}").unwrap_err();
        let rendered = crate::diagnostic::render(&err);
        assert!(
            err.iter().any(|d| d.code == "E_BLOCK"),
            "the block rejection: {rendered}"
        );
        assert!(
            rendered.contains("unknown directive") && rendered.contains("frobnicate"),
            "the child's own error surfaces too: {rendered}"
        );
        // The child error is located on its own line (2), not the opener's (1).
        assert!(
            rendered.contains("2:1"),
            "child located by line: {rendered}"
        );
    }

    /// The `parse_forest` descent path is exercised end-to-end by a `cfg(test)`
    /// block-accepting keyword (finding 3): a block on `testblock` is *not* rejected,
    /// its children are descended into and lowered as ordinary directives into
    /// `parsed.source`, and the block tree serializes to a fixed point. This gives
    /// Phase 1 a tested recursion path rather than a latent one.
    #[test]
    fn accepted_block_descends_into_children() {
        assert!(
            keyword_takes_block(TEST_BLOCK_KEYWORD),
            "the sentinel keyword opts into blocks"
        );
        let text = "\
testblock amp {
  inst r1 R
  inst c1 Cap
}
inst top MCU
";
        let parsed = parse(text).expect("accepted block parses without E_BLOCK");
        // The descent lowered both children plus the trailing top-level directive; the
        // `testblock` header itself contributes no directive (a real consumer owns it).
        assert_eq!(
            parsed.source,
            vec![
                GenDirective::Instance {
                    path: "r1".into(),
                    part: "R".into(),
                    params: BTreeMap::new(),
                    label: None,
                },
                GenDirective::Instance {
                    path: "c1".into(),
                    part: "Cap".into(),
                    params: BTreeMap::new(),
                    label: None,
                },
                GenDirective::Instance {
                    path: "top".into(),
                    part: "MCU".into(),
                    params: BTreeMap::new(),
                    label: None,
                },
            ]
        );
        // A syntax error *inside* an accepted block is reported at its own line.
        let err = parse("testblock a {\n  place foo (3mm)\n}").unwrap_err();
        assert!(
            crate::diagnostic::render(&err).contains("2:1"),
            "child error located by line: {err:?}"
        );
        // The block tree round-trips (serialize -> parse -> serialize fixpoint).
        let forest = parse_blocks(text).unwrap();
        let once = serialize_blocks(&forest);
        assert_eq!(parse_blocks(&once).unwrap(), forest);
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

    // ---- Decision-20 schematic layout grammar ---------------------------

    use crate::schematic::{Align, Direction, LayoutNode, SchematicLayout};

    /// Parse a schematic block, asserting success, and return its layout.
    fn parse_layout(text: &str) -> SchematicLayout {
        parse(text)
            .unwrap_or_else(|e| panic!("parse failed: {e:?}"))
            .schematic
            .expect("a schematic block")
    }

    #[test]
    fn schematic_block_parses_containers_and_syms() {
        let layout = parse_layout(
            "schematic {\n  row power gap=2mm align=center {\n    sym C1\n    sym U1 rot=90 dx=1mm dy=-2mm\n  }\n  column {\n    sym C2\n  }\n}\n",
        );
        assert_eq!(layout.roots.len(), 2);
        let LayoutNode::Container(power) = &layout.roots[0] else {
            panic!("expected a container");
        };
        assert_eq!(power.dir, Direction::Row);
        assert_eq!(power.name.as_deref(), Some("power"));
        assert_eq!(power.gap, 2_000_000);
        assert_eq!(power.align, Align::Center);
        // The second child of the row is a rotated, pinned symbol.
        let LayoutNode::Symbol(u1) = &power.children[1] else {
            panic!("expected a symbol");
        };
        assert_eq!(u1.path, "U1");
        assert_eq!(u1.rot, Orient::from_deg(90).unwrap());
        assert_eq!(u1.dx, 1_000_000);
        assert_eq!(u1.dy, -2_000_000);
    }

    #[test]
    fn schematic_round_trips_byte_identical() {
        // Canonical text -> parse -> serialize reproduces the input exactly. Note the
        // canonical omissions (align=start, rot=0, dx/dy=0, gap=0 are all elided).
        let canonical = "inst C1 Cap\ninst U1 MCU\nschematic {\n  row power gap=2mm {\n    sym C1\n    sym U1 rot=90\n  }\n  column align=end {\n    sym C1 dx=1mm\n  }\n}\n";
        let parsed = parse(canonical).unwrap();
        let doc = Doc {
            source: parsed.source,
            schematic: parsed.schematic,
            ..Default::default()
        };
        assert_eq!(serialize(&doc), canonical);
    }

    #[test]
    fn schematic_preserves_trivia_round_trip() {
        // Comments and blank lines inside the block survive a round-trip (Decision 20/21).
        let canonical =
            "schematic {\n  # power section\n  row {\n    sym C1\n\n    sym C2\n  }\n}\n";
        let parsed = parse(canonical).unwrap();
        let doc = Doc {
            schematic: parsed.schematic,
            ..Default::default()
        };
        assert_eq!(serialize(&doc), canonical);
    }

    #[test]
    fn schematic_serialize_parse_fixpoint() {
        // A second round is a fixpoint even from a non-canonical (extra-spaced) authoring.
        let authored = "schematic {\n   row   power   gap=2mm   {\n      sym C1\n   }\n}\n";
        let doc1 = Doc {
            schematic: parse(authored).unwrap().schematic,
            ..Default::default()
        };
        let once = serialize(&doc1);
        let doc2 = Doc {
            schematic: parse(&once).unwrap().schematic,
            ..Default::default()
        };
        assert_eq!(serialize(&doc2), once);
    }

    #[test]
    fn nesting_is_arbitrary() {
        let layout = parse_layout(
            "schematic {\n  row {\n    column {\n      row {\n        sym C1\n      }\n    }\n  }\n}\n",
        );
        // Walk three levels down to the symbol.
        let mut node = &layout.roots[0];
        for _ in 0..3 {
            let LayoutNode::Container(c) = node else {
                panic!("expected container");
            };
            node = &c.children[0];
        }
        assert!(matches!(node, LayoutNode::Symbol(s) if s.path == "C1"));
    }

    #[test]
    fn doc_without_schematic_block_is_byte_identical() {
        // The poc guard: a blockless doc serializes exactly as before this feature.
        let src = "inst C1 Cap\ninst C2 Cap\nnet N1 C1.p1 C2.p1\n";
        let doc = Doc {
            source: parse(src).unwrap().source,
            ..Default::default()
        };
        assert_eq!(serialize(&doc), src);
        assert!(doc.schematic.is_none());
    }

    #[test]
    fn last_schematic_block_wins() {
        let layout = parse_layout("schematic {\n  sym C1\n}\nschematic {\n  sym C2\n}\n");
        // The second block replaces the first.
        assert_eq!(layout.roots.len(), 1);
        assert!(matches!(&layout.roots[0], LayoutNode::Symbol(s) if s.path == "C2"));
    }

    #[test]
    fn bad_child_keyword_is_e_schematic() {
        let err = parse("schematic {\n  inst C1 Cap\n}\n").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
    }

    #[test]
    fn row_outside_schematic_is_e_schematic() {
        let err = parse("row {\n  sym C1\n}\n").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
    }

    #[test]
    fn sym_with_block_is_e_schematic() {
        let err = parse("schematic {\n  sym C1 {\n  }\n}\n").unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
    }

    #[test]
    fn bad_align_and_rot_are_errors() {
        assert!(parse("schematic {\n  row align=middle {\n    sym C1\n  }\n}\n").is_err());
        assert!(parse("schematic {\n  row {\n    sym C1 rot=45\n  }\n}\n").is_err());
        assert!(parse("schematic {\n  row {\n    sym C1 bogus=1\n  }\n}\n").is_err());
    }

    #[test]
    fn schematic_takes_no_args() {
        assert!(parse("schematic foo {\n  sym C1\n}\n").is_err());
    }

    #[test]
    fn names_and_paths_with_structural_chars_round_trip() {
        // An `inst` path is unrestricted, so a comp path (and a container name) may hold
        // `=`, `#`, or spaces. Such tokens must serialize quoted and re-parse identically
        // (regression: unquoted, `=` split the token into a bogus attribute and `#` was
        // silently truncated by the comment stripper).
        for (name, path) in [
            ("a=b", "u=1"),
            ("has space", "sens[0].fb"),
            ("with#hash", "n#2"),
        ] {
            let layout = SchematicLayout {
                roots: vec![LayoutNode::Container(crate::schematic::Container {
                    dir: Direction::Row,
                    name: Some(name.into()),
                    gap: 0,
                    align: Align::Start,
                    children: vec![LayoutNode::Symbol(crate::schematic::Symbol {
                        path: path.into(),
                        rot: Orient::IDENTITY,
                        dx: 0,
                        dy: 0,
                    })],
                })],
            };
            let doc = Doc {
                schematic: Some(layout.clone()),
                ..Default::default()
            };
            let text = serialize(&doc);
            let reparsed = parse(&text).unwrap().schematic.unwrap();
            assert_eq!(
                reparsed, layout,
                "round-trip failed for name={name:?} path={path:?} via `{text}`"
            );
        }
    }

    #[test]
    fn schematic_lengths_are_range_checked() {
        // Authored lengths obey the issue-0018 ingress bound (MAX_COORD), like every other
        // coordinate — an over-bound `gap`/`dx` is E_COORD_RANGE at parse, not an
        // add-overflow panic in reflow.
        let over = crate::geom::MAX_COORD + 1;
        let gap_err = parse(&format!(
            "schematic {{\n  row gap={over}nm {{\n    sym C1\n  }}\n}}\n"
        ))
        .unwrap_err();
        assert!(gap_err.iter().any(|d| d.code == "E_COORD_RANGE"));
        let dx_err = parse(&format!(
            "schematic {{\n  row {{\n    sym C1 dx={over}nm\n  }}\n}}\n"
        ))
        .unwrap_err();
        assert!(dx_err.iter().any(|d| d.code == "E_COORD_RANGE"));
    }

    #[test]
    fn max_coord_scale_lengths_reflow_without_panic() {
        // A gap/dx at the MAX_COORD ceiling parses and reflows cleanly (no overflow).
        let big = crate::geom::MAX_COORD;
        let doc = Doc {
            schematic: parse(&format!(
                "schematic {{\n  row gap={big}nm {{\n    sym C1 dx={big}nm dy=-{big}nm\n    sym C2\n  }}\n}}\n"
            ))
            .unwrap()
            .schematic,
            ..Default::default()
        };
        let lib = part_library();
        let parts = BTreeMap::from([
            (EntityId::new("C1"), "Cap".to_string()),
            (EntityId::new("C2"), "Cap".to_string()),
        ]);
        // Must not panic (debug add-overflow) — the whole point of the range check.
        let placed = crate::schematic::reflow(&doc.schematic.unwrap(), &parts, &lib);
        assert_eq!(placed.len(), 2);
    }

    #[test]
    fn canonical_defaults_are_elided_first_pass() {
        // Explicitly-authored defaults (align=start, rot=0, gap=0, dx=0, dy=0) all elide on
        // the FIRST serialization — guards against a regression that starts emitting them
        // (which the already-canonical fixpoint tests would not catch).
        let authored = "schematic {\n  row power gap=0mm align=start {\n    sym C1 rot=0 dx=0mm dy=0mm\n  }\n}\n";
        let expected = "schematic {\n  row power {\n    sym C1\n  }\n}\n";
        let doc = Doc {
            schematic: parse(authored).unwrap().schematic,
            ..Default::default()
        };
        assert_eq!(serialize(&doc), expected);
    }

    #[test]
    fn load_text_carries_and_validates_schematic() {
        // End-to-end: LoadText parses the block, the post-elaborate gate validates paths,
        // and an unplaced component surfaces as a non-blocking W_SCHEMATIC_UNPLACED.
        let lib = part_library();
        let text = "inst C1 Cap\ninst C2 Cap\nnet N1 C1.p1 C2.p1\nnet N2 C1.p2 C2.p2\nschematic {\n  row {\n    sym C1\n  }\n}\n";
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::LoadText(text.into())),
            &lib,
            "load",
        )
        .unwrap();
        let doc = h.doc();
        assert!(doc.schematic.is_some());
        // C2 is not placed -> reported, but the commit still succeeded (view is total).
        assert_eq!(doc.report.unplaced_components, vec![EntityId::new("C2")]);
        assert!(doc.report.is_clean()); // unplaced is a warning, not a dirtying finding.
    }

    #[test]
    fn load_text_rejects_unknown_sym_path() {
        let lib = part_library();
        // `sym NOPE` names no instance -> E_SCHEMATIC aborts the transaction (atomic).
        let text = "inst C1 Cap\nschematic {\n  sym NOPE\n}\n";
        let mut h = History::new(Default::default());
        let err = h
            .commit(
                Transaction::one(Command::LoadText(text.into())),
                &lib,
                "load",
            )
            .unwrap_err();
        assert!(err.iter().any(|d| d.code == "E_SCHEMATIC"));
    }

    #[test]
    fn load_text_dnp_placed_symbol_degrades_not_aborts() {
        // End-to-end (Decision 20c × 21b): a `sym` placing a component that a false `if=`
        // depopulates must COMMIT (not hard-abort a variant toggle) — the symbol is absent
        // from reflow and the part surfaces as W_SCHEMATIC_UNPLACED.
        let lib = part_library();
        let text = "param populate = false\n\
                    inst C1 Cap\n\
                    inst C2 Cap if=populate\n\
                    net N1 C1.p1 C1.p2\n\
                    schematic {\n  row {\n    sym C1\n    sym C2\n  }\n}\n";
        let mut h = History::new(Default::default());
        h.commit(
            Transaction::one(Command::LoadText(text.into())),
            &lib,
            "load",
        )
        .expect("a DNP-dropped placed symbol must not abort the commit");
        let doc = h.doc();
        // C2 is depopulated -> not a real component, surfaced as unplaced, and warns.
        assert!(!doc.components.contains_key(&EntityId::new("C2")));
        assert_eq!(doc.report.unplaced_components, vec![EntityId::new("C2")]);
        assert!(doc.report.is_clean()); // W_SCHEMATIC_UNPLACED is a non-dirtying warning.
        // Reflow places only the populated C1; C2 is absent from the output entirely.
        let placed = doc.reflow_schematic(&lib);
        assert!(placed.contains_key(&EntityId::new("C1")));
        assert!(
            !placed.contains_key(&EntityId::new("C2")),
            "a depopulated part must not appear in reflow output"
        );
    }

    // ---- Decision-21a `def` construct ------------------------------------

    /// Elaborate `source` against the toy library and return the diagnostics, panicking if
    /// it unexpectedly succeeded. (`Elaborated` isn't `Debug`, so `expect_err` can't be
    /// used directly.)
    fn elab_err(source: &Source) -> Vec<Diagnostic> {
        let lib = part_library();
        match elaborate(source, &Default::default(), &Default::default(), &lib) {
            Ok(_) => panic!("expected elaboration to fail"),
            Err(e) => e,
        }
    }

    /// Return the elaborated net whose name is `name`, panicking if absent.
    fn net_named<'a>(doc: &'a Doc, name: &str) -> &'a crate::doc::Net {
        doc.nets
            .values()
            .find(|n| n.name == name)
            .unwrap_or_else(|| panic!("net `{name}` not found in {:?}", doc.nets.keys()))
    }

    /// A `def` stamps its body per instantiation with path prefixing: `sense[0].R1`-style
    /// component paths and path-prefixed internal nets, so two instances never collide.
    #[test]
    fn def_stamps_body_with_path_prefix() {
        let src = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n  net fb R1.p2 C1.p1\n}\n\
                   inst a rc\ninst b rc";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        for p in ["a.R1", "a.C1", "b.R1", "b.C1"] {
            assert!(
                doc.components.contains_key(&EntityId::new(p)),
                "stamped component `{p}` missing"
            );
        }
        // Internal net `fb` is path-prefixed per instance — distinct nets, no collision.
        let a_fb = net_named(&doc, "a.fb");
        let b_fb = net_named(&doc, "b.fb");
        assert!(a_fb.members.iter().any(|m| m.comp.as_str() == "a.R1"));
        assert!(b_fb.members.iter().any(|m| m.comp.as_str() == "b.R1"));
        assert!(!a_fb.members.iter().any(|m| m.comp.as_str() == "b.R1"));
    }

    /// A connection to a def instance's port resolves through to the bound internal pin's
    /// pad identity (no new namespace) — an outer `net VOUT amp.out` lands on `amp.R1`'s
    /// pad, not a phantom port pin.
    #[test]
    fn def_port_resolves_to_bound_internal_pin() {
        let src = "def divider {\n  inst R1 Cap\n  inst R2 Cap\n  net mid R1.p2 R2.p1\n  \
                   port out = R1.p2\n}\n\
                   inst d divider\nnet VOUT d.out";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        let vout = net_named(&doc, "VOUT");
        // The outer net reaches the internal R1 pad p2 — the port's binding.
        assert!(
            vout.members
                .iter()
                .any(|m| m.comp.as_str() == "d.R1" && m.pin.as_str() == "p2"),
            "VOUT should reach d.R1.p2 via the port, got {:?}",
            vout.members
        );
    }

    /// Def params: a default is used when the instantiation omits it; a `p:` override
    /// replaces it; the value flows into body expressions (evaluated in the def scope).
    #[test]
    fn def_params_default_and_override() {
        let src = "def rc param val=100n {\n  inst C1 Cap p:value=(val)\n}\n\
                   inst a rc\ninst b rc p:val=220n";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert_eq!(
            doc.components[&EntityId::new("a.C1")].params["value"],
            "100n",
            "default param flows into a body expression"
        );
        assert_eq!(
            doc.components[&EntityId::new("b.C1")].params["value"],
            "220n",
            "p: override replaces the default"
        );
    }

    /// A def param shadows an outer doc param of the same name (innermost wins — the same
    /// rule as the range loop variable `i`). The body reads the def param's value.
    #[test]
    fn def_param_shadows_outer_doc_param() {
        let src = "param val = 1n\n\
                   def rc param val=999n {\n  inst C1 Cap p:value=(val)\n}\n\
                   inst a rc";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert_eq!(
            doc.components[&EntityId::new("a.C1")].params["value"],
            "999n",
            "the def param shadows the outer doc param"
        );
    }

    /// An outer doc param is visible inside a def body when not shadowed.
    #[test]
    fn def_body_sees_outer_param() {
        let src = "param gain = 5\n\
                   def amp {\n  inst C1 Cap p:g=(gain)\n}\n\
                   inst a amp";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert_eq!(doc.components[&EntityId::new("a.C1")].params["g"], "5");
    }

    /// Def instantiation composes with a range: `inst sense[0..n] SenseDef` stamps the
    /// body under each `sense[i]` prefix, and the loop variable is usable in `p:`.
    #[test]
    fn def_instantiation_with_range() {
        let src = "param n = 2\n\
                   def sensor {\n  inst U Cap\n}\n\
                   inst sense[0..n] sensor";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        assert!(doc.components.contains_key(&EntityId::new("sense[0].U")));
        assert!(doc.components.contains_key(&EntityId::new("sense[1].U")));
        assert!(!doc.components.contains_key(&EntityId::new("sense[2].U")));
    }

    /// Nested def instantiation composes paths, and a re-exported port (a def's port bound
    /// to a nested def's port) resolves transitively to the deepest real pin.
    #[test]
    fn nested_def_composes_and_reexports_port() {
        let src = "def leaf {\n  inst R Cap\n  port o = R.p2\n}\n\
                   def mid {\n  inst inner leaf\n  port o = inner.o\n}\n\
                   inst top mid\nnet OUT top.o";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        // Path composition: top → mid.inner → leaf.R
        assert!(
            doc.components.contains_key(&EntityId::new("top.inner.R")),
            "nested path did not compose: {:?}",
            doc.components.keys().collect::<Vec<_>>()
        );
        // Transitive port resolution: OUT reaches top.inner.R.p2.
        let out = net_named(&doc, "OUT");
        assert!(
            out.members
                .iter()
                .any(|m| m.comp.as_str() == "top.inner.R" && m.pin.as_str() == "p2"),
            "OUT should reach top.inner.R.p2, got {:?}",
            out.members
        );
    }

    /// A def reaching itself through any instantiation chain is an `E_DEF_CYCLE` error
    /// naming the cycle — not an infinite loop.
    #[test]
    fn def_cycle_is_an_error() {
        let src = "def a {\n  inst x b\n}\n\
                   def b {\n  inst y a\n}\n\
                   inst top a";
        let Parsed { source, .. } = parse(src).expect("parse");
        let err = elab_err(&source);
        assert!(
            err.iter().any(|d| d.code == "E_DEF_CYCLE"),
            "expected E_DEF_CYCLE, got {:?}",
            err.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// A def whose name also names a library part is rejected at elaboration
    /// (`E_DEF_PART_AMBIGUOUS`) rather than silently shadowing.
    #[test]
    fn def_name_colliding_with_part_is_ambiguous() {
        let src = "def Cap {\n  inst X Cap\n}\ninst a Cap";
        let Parsed { source, .. } = parse(src).expect("parse");
        let err = elab_err(&source);
        assert!(
            err.iter().any(|d| d.code == "E_DEF_PART_AMBIGUOUS"),
            "expected E_DEF_PART_AMBIGUOUS, got {:?}",
            err.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// `if=false` on a def instance drops the whole stamped subtree; an external net
    /// referencing a dropped port dangles as `W_DNP`, never an unknown-instance error.
    #[test]
    fn def_instance_if_false_drops_subtree() {
        let src = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n  port o = R1.p2\n}\n\
                   inst a rc if=(false)\nnet OUT a.o";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        // The whole subtree is gone.
        assert!(!doc.components.contains_key(&EntityId::new("a.R1")));
        assert!(!doc.components.contains_key(&EntityId::new("a.C1")));
        // The external reference to the dropped instance dangles as a warning.
        assert!(
            doc.report.dnp_dangling.iter().any(|(_, p)| p == "a"),
            "dangling connection to dropped def instance recorded: {:?}",
            doc.report.dnp_dangling
        );
    }

    /// Refdes stays board-global flat across hierarchical def paths (industry
    /// convention): two `Cap` instances stamped from two def instances get R1/R2 (or the
    /// class prefix), numbered over all instances regardless of their hierarchical path.
    #[test]
    fn def_refdes_stays_board_global_flat() {
        let src = "def rc {\n  inst K Cap\n}\n\
                   inst a rc\ninst b rc";
        let Parsed { source, .. } = parse(src).expect("parse");
        let doc = placed(source);
        let lib = part_library();
        let rd = crate::annotate::refdes(&doc, &lib, &crate::annotate::registry(&[]));
        let designators: BTreeSet<String> = [
            rd[&EntityId::new("a.K")].clone(),
            rd[&EntityId::new("b.K")].clone(),
        ]
        .into_iter()
        .collect();
        // Two distinct board-global designators over the two hierarchical instances.
        assert_eq!(
            designators.len(),
            2,
            "two stamped Caps must get two distinct board-global refdes, got {designators:?}"
        );
    }

    /// A `def` document round-trips byte-identically through parse → serialize → parse →
    /// serialize (canonical fixpoint), preserving body directives, params, ports, and
    /// interior trivia.
    #[test]
    fn def_serialize_parse_fixpoint() {
        let authored = "def rc param val=100n {\n  # the resistor\n  inst R1 Cap p:value=(val)\n\n  \
                        inst C1 Cap\n  port out = R1.p2\n}\ninst a rc p:val=220n\n";
        let once = serialize(&Doc {
            source: parse(authored).unwrap().source,
            ..Default::default()
        });
        let twice = serialize(&Doc {
            source: parse(&once).unwrap().source,
            ..Default::default()
        });
        assert_eq!(once, twice, "def serialization must reach a fixpoint");
        // The fixpoint form preserves the def structure.
        assert!(once.contains("def rc param val=100n {"));
        assert!(once.contains("  # the resistor"));
        assert!(once.contains("  inst R1 Cap p:value=(val)"));
        assert!(once.contains("  port out = R1.p2"));
    }

    /// The poc guard: a document with no `def` serializes byte-identically to before this
    /// feature — the def machinery adds nothing to a blockless program's text.
    #[test]
    fn defless_doc_is_byte_identical() {
        let src = "inst U1 MCU\ninst c1 Cap\nnet GND U1.GND c1.p2\n";
        let doc = Doc {
            source: parse(src).unwrap().source,
            ..Default::default()
        };
        assert_eq!(
            serialize(&doc),
            src,
            "a def-free doc must be byte-identical"
        );
    }

    /// A `p:` override naming a param the def does not declare is a hard error (a typo,
    /// never silently ignored).
    #[test]
    fn def_unknown_param_override_is_an_error() {
        let src = "def rc param val=1n {\n  inst C1 Cap\n}\ninst a rc p:nope=2n";
        let Parsed { source, .. } = parse(src).expect("parse");
        let err = elab_err(&source);
        assert!(
            err.iter().any(|d| d.code == "E_DEF"),
            "expected E_DEF, got {:?}",
            err.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// A nested def *definition* (a `def` inside a `def` body) is rejected — definitions
    /// stay top-level in v1.
    #[test]
    fn nested_def_definition_is_rejected() {
        let src = "def outer {\n  def inner {\n    inst X Cap\n  }\n}\ninst a outer";
        let errs = parse(src).expect_err("a nested def definition must fail parsing");
        assert!(
            errs.iter().any(|d| d.code == "E_DEF"),
            "expected E_DEF, got {:?}",
            errs.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// A ref to a *leaf pin* of a def instance dropped by `if=false` degrades to `W_DNP`,
    /// exactly like a ref to the instance itself — not a hard `E_UNKNOWN_INSTANCE` (the
    /// prefix rule: a path beneath a dropped subtree is intentionally-absent). With
    /// `if=true` the same connection resolves normally; a genuinely unknown deep path (no
    /// such def instance ever) still hard-errors.
    #[test]
    fn deep_ref_into_dropped_def_degrades_to_warning() {
        let def = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n}\n";
        // if=false: deep pin ref into the never-stamped subtree degrades.
        let src = format!("{def}inst a rc if=(false)\nnet OUT a.R1.p2");
        let Parsed { source, .. } = parse(&src).expect("parse");
        let doc = placed(source);
        assert!(!doc.components.contains_key(&EntityId::new("a.R1")));
        assert!(
            doc.report.dnp_dangling.iter().any(|(_, p)| p == "a.R1"),
            "deep ref into dropped subtree should be W_DNP, got {:?}",
            doc.report.dnp_dangling
        );

        // if=true: the same deep ref connects normally.
        let src_on = format!("{def}inst a rc if=(true)\nnet OUT a.R1.p2");
        let Parsed { source: on, .. } = parse(&src_on).expect("parse on");
        let doc_on = placed(on);
        let out = net_named(&doc_on, "OUT");
        assert!(
            out.members
                .iter()
                .any(|m| m.comp.as_str() == "a.R1" && m.pin.as_str() == "p2"),
            "with if=true the deep pin connects, got {:?}",
            out.members
        );

        // A genuinely unknown deep path (no def instance `zzz` ever) still hard-errors.
        let bad = format!("{def}inst a rc\nnet OUT zzz.R1.p2");
        let Parsed { source: b, .. } = parse(&bad).expect("parse bad");
        let err = elab_err(&b);
        assert!(
            err.iter().any(|d| d.code == "E_UNKNOWN_INSTANCE"),
            "an unknown deep path must still hard-error, got {:?}",
            err.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
    }

    /// An authored top-level net whose name equals a stamped def-internal net is a hard
    /// `E_DEF_NET_COLLISION` (not a silent merge), naming both sides. Tested in both
    /// authoring orders (authored-before-def-inst and after), since a silent merge would
    /// be order-independent too.
    #[test]
    fn authored_net_colliding_with_internal_net_is_an_error() {
        let def = "def rc {\n  inst R1 Cap\n  inst C1 Cap\n  net fb R1.p2 C1.p1\n}\n";
        // The stamped internal net is `a.fb`; author a top-level `net a.fb …` that collides.
        for order in [
            format!("{def}inst a rc\nnet a.fb R1.p1"),
            format!("{def}net a.fb R1.p1\ninst a rc"),
        ] {
            let Parsed { source, .. } = parse(&order).expect("parse");
            let err = elab_err(&source);
            assert!(
                err.iter().any(|d| d.code == "E_DEF_NET_COLLISION"),
                "expected E_DEF_NET_COLLISION for `{order}`, got {:?}",
                err.iter().map(|d| &d.code).collect::<Vec<_>>()
            );
        }
    }

    /// The range loop variable `i` is NOT visible inside a def body (the body is a pure
    /// function of its declared params). A body expression referencing `i` is an `E_EXPR`
    /// unknown variable — the index must be passed explicitly via a `p:`.
    #[test]
    fn range_index_not_visible_inside_def_body() {
        let src = "param n = 2\n\
                   def s {\n  inst U Cap p:idx=(i)\n}\n\
                   inst sense[0..n] s";
        let Parsed { source, .. } = parse(src).expect("parse");
        let err = elab_err(&source);
        assert!(
            err.iter().any(|d| d.code == "E_EXPR"),
            "referencing `i` inside a def body must be E_EXPR, got {:?}",
            err.iter().map(|d| &d.code).collect::<Vec<_>>()
        );
        // The explicit-forward form works: pass the index as a param.
        let ok = "param n = 2\n\
                  def s param idx=0 {\n  inst U Cap p:tag=(idx)\n}\n\
                  inst sense[0..n] s p:idx=(i)";
        let Parsed { source: oks, .. } = parse(ok).expect("parse ok");
        let doc = placed(oks);
        assert_eq!(
            doc.components[&EntityId::new("sense[0].U")].params["tag"],
            "0"
        );
        assert_eq!(
            doc.components[&EntityId::new("sense[1].U")].params["tag"],
            "1"
        );
    }

    /// An override pinned to a stamped def-instance path survives a def param change and
    /// orphans (surfaced, not dropped) when the instance disappears — reconciliation flows
    /// through stamped paths exactly as for hand-written ones.
    #[test]
    fn def_override_survives_and_decays_by_stamped_path() {
        let lib = part_library();
        let mut h = History::new(Default::default());
        let base = "param n = 2\ndef s {\n  inst U Cap\n}\ninst sense[0..n] s";
        let Parsed { source: s2, .. } = parse(base).expect("parse");
        h.commit(Transaction::one(Command::SetSource(s2)), &lib, "n2")
            .unwrap();
        // Pin a stamped path.
        h.commit(
            Transaction::one(Command::Pin(EntityId::new("sense[1].U"), Point::mm(5, 5))),
            &lib,
            "pin",
        )
        .unwrap();
        assert_eq!(
            h.doc().components[&EntityId::new("sense[1].U")].pos.value,
            Point::mm(5, 5),
            "pin holds the stamped path"
        );
        // Shrink the range: sense[1] disappears; the pin orphans, is not silently dropped.
        let Parsed { source: s1, .. } =
            parse("param n = 1\ndef s {\n  inst U Cap\n}\ninst sense[0..n] s").expect("parse1");
        h.commit(Transaction::one(Command::SetSource(s1)), &lib, "n1")
            .unwrap();
        assert!(
            !h.doc()
                .components
                .contains_key(&EntityId::new("sense[1].U"))
        );
        assert!(
            h.doc()
                .report
                .orphaned
                .contains(&EntityId::new("sense[1].U")),
            "the removed stamped instance's override is surfaced as an orphan"
        );
    }
}
