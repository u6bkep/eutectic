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
            layout,
        } => render_def(name, params, body, ports, layout),
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
/// across a parse→serialize→parse fixpoint. When the def carries a Decision-20 `schematic`
/// layout fragment (over its internal paths), it emits last — after the ports — as an
/// indented `schematic { … }` block, reusing the same [`emit_layout_nodes`] the doc-level
/// block uses so the two agree on indentation and trivia.
fn render_def(
    name: &str,
    params: &[(String, String)],
    body: &[DefNode],
    ports: &BTreeMap<String, (String, String)>,
    layout: &Option<crate::schematic::SchematicLayout>,
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
    // The Decision-20 layout fragment (if any) emits last, as an indented `schematic { … }`
    // block inside the def body. One extra indent level over the doc-level `serialize_layout`
    // (the def body is already one level in), reusing `emit_layout_nodes` so trivia and
    // container indentation match the doc-level block exactly.
    if let Some(layout) = layout {
        s.push_str(BLOCK_INDENT);
        s.push_str("schematic {\n");
        let mut indent = String::from(BLOCK_INDENT);
        indent.push_str(BLOCK_INDENT);
        emit_layout_nodes(&layout.roots, &mut indent, &mut s);
        s.push_str(BLOCK_INDENT);
        s.push_str("}\n");
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
            "wire" => {
                if b.opened_block {
                    errors.push(err_line(
                        "E_SCHEMATIC",
                        "`wire` is a leaf and takes no block".to_string(),
                        b.line,
                    ));
                    continue;
                }
                match parse_wire_header(&b.rest) {
                    Ok(wire) => {
                        // Range-check the presentational waypoints, same discipline as
                        // `gap` / `dx` / every other authored length (issue 0018) — an
                        // over-bound value is E_COORD_RANGE here, not a later panic.
                        let coords: Vec<Nm> =
                            wire.waypoints.iter().flat_map(|p| [p.x, p.y]).collect();
                        check_coord_range(coords, b.line, errors);
                        out.push(LayoutNode::Wire(wire));
                    }
                    Err(e) => errors.push(err_line("E_SCHEMATIC", e, b.line)),
                }
            }
            other => errors.push(err_line(
                "E_SCHEMATIC",
                format!("`{other}` is not valid inside a layout container (expected `row`, `column`, `sym`, or `wire`)"),
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
    // A def body may carry ONE Decision-20 `schematic { … }` layout fragment over its
    // INTERNAL paths (`R1`, not `sense[0].R1`), stamped per instance at expansion time. The
    // last one wins (mirrors the doc-level `schematic` block).
    let mut layout: Option<crate::schematic::SchematicLayout> = None;
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
        // A `schematic { … }` layout fragment is the one block a def body admits (Decision
        // 20 embedded in a def): special-case it BEFORE the generic block rejection so it is
        // not treated like a stray nested block. Its `row`/`column`/`sym`/`wire` children
        // parse through the ordinary layout grammar; the paths are def-internal.
        if child.opened_block && child.keyword == "schematic" {
            if child.tokens.len() > 1 {
                errors.push(err_line(
                    "E_SCHEMATIC",
                    format!("`schematic` takes no arguments (got `{}`)", child.rest),
                    child.line,
                ));
            }
            let roots = parse_layout_nodes(&child.children, errors);
            // Last `schematic` block wins, mirroring the doc-level rule.
            layout = Some(crate::schematic::SchematicLayout { roots });
            continue;
        }
        if child.opened_block {
            // The only other block-opening keyword reachable here is `def` (the allowlist);
            // a nested def definition is rejected. Any other block opener already errored in
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
        layout,
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

/// Parse a `wire` leaf tail (`<aComp>.<aPin> <bComp>.<bPin> [via (x,y) (x,y) …]`). The two
/// endpoints are the leading tokens (each `comp.pin`, split at the *last* dot so a
/// hierarchical comp path with dots survives — the `nearpin` idiom); an optional `via`
/// keyword introduces the presentational waypoint list, a run of `(x,y)` coordinates.
/// Endpoints round-trip: a quoted endpoint token is opaque (so a comp path holding
/// `#`/`=`/spaces survives), matching the `sym` path convention.
///
/// **v1 limitation — an interface *signal* cannot be named directly.** The last-dot split
/// makes `mcu.uart.tx` parse as comp `mcu.uart` + pin `tx`, which then fails validation
/// (no such component) — a loud `E_SCHEMATIC`, never silent. This is deliberate: the wire
/// vocabulary is `comp.pin`, and a `port.signal` has *two* dots. The workaround is exact
/// and always available: **wire to the underlying pad number.** Post-0010, an interface
/// signal bound to a pad *is* that pad (the netlist keys both under the pad-number
/// `PinRef`), so `wire mcu.12 …` addresses the same electrical node the tag renderer draws
/// for `uart.tx`. An abstract (unbound, toy-library) signal has no pad and simply cannot be
/// wired in v1 — its tag still renders (§20c), so the connection is not lost, only the
/// drawn line. Naming `port.signal` endpoints directly is a follow-up if it proves needed.
fn parse_wire_header(rest: &str) -> Result<crate::schematic::Wire, String> {
    use crate::schematic::{Wire, WireEnd};
    const USAGE: &str = "wire <aComp>.<aPin> <bComp>.<bPin> [via (x,y) (x,y) …]";

    // Split the header at the `via` keyword: the part before it holds the two endpoint
    // tokens; the part after is the coordinate list. `via` is matched as a whole
    // whitespace-delimited token so a comp path can't accidentally contain it.
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let via_at = toks.iter().position(|t| *t == "via");
    let (ends, waypoints) = match via_at {
        Some(i) => {
            // Rejoin the tokens after the `via` keyword; `extract_points` scans for the
            // `(x, y)` groups, so a space reintroduced inside a coordinate is harmless.
            let coord_str = toks[i + 1..].join(" ");
            if coord_str.is_empty() {
                return Err(format!(
                    "`via` needs at least one waypoint `(x, y)`: {USAGE}"
                ));
            }
            (&toks[..i], extract_points(&coord_str)?)
        }
        None => (&toks[..], Vec::new()),
    };
    if ends.len() != 2 {
        return Err(format!("expected two endpoints: {USAGE}"));
    }
    let parse_end = |tok: &str| -> Result<WireEnd, String> {
        let (comp, pin) = split_last_dot(unquote(tok), "pin")?;
        Ok(WireEnd { comp, pin })
    };
    Ok(Wire {
        a: parse_end(ends[0])?,
        b: parse_end(ends[1])?,
        waypoints,
    })
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

/// Render just the doc-level `schematic { … }` block for a [`SchematicLayout`] as canonical
/// text — the same bytes [`serialize`] emits for `doc.schematic`, exposed so a caller
/// authoring a layout tree programmatically can append it to a serialized source and feed
/// the whole thing through [`parse`]/`LoadText` (the only ingest path for a schematic tree —
/// there is no `SetSchematic` command). A thin `pub` wrapper over [`serialize_layout`]; the
/// output round-trips byte-identically.
pub fn serialize_schematic_block(layout: &crate::schematic::SchematicLayout) -> String {
    serialize_layout(layout)
}

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
            LayoutNode::Wire(w) => {
                out.push_str(indent);
                out.push_str(&wire_line(w));
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

/// A `wire` leaf line: `wire <a> <b>`, then `via` and the waypoint coordinate list when
/// present (omitted for a straight pin-to-pin wire) — the minimal canonical form. Each
/// endpoint is `comp.pin`, quoted as a whole when a structural character would break
/// re-parsing (matching [`quote_token`]'s rule for the `sym` path).
fn wire_line(w: &crate::schematic::Wire) -> String {
    let end = |e: &crate::schematic::WireEnd| quote_token(&format!("{}.{}", e.comp, e.pin));
    let mut out = format!("wire {} {}", end(&w.a), end(&w.b));
    if !w.waypoints.is_empty() {
        out.push_str(" via");
        for p in &w.waypoints {
            out.push(' ');
            out.push_str(&fmt_point(*p));
        }
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
mod tests;
