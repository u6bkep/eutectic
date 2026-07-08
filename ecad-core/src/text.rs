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
//! route <id> <net> <slab> w=<width> (x,y) (x,y) ...  [free|hint|fixed]   # a routed polyline
//! via   <id> <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]  # a plated via
//! ```
//!
//! Routes live in a `# routes` section beside `# overrides`. They are materialized
//! state the parser fills directly (never re-derived at load — an autorouter is
//! expensive/stochastic), so re-elaboration cannot wipe them. The leading `<id>` is the
//! route's persistent identity (Decision 22 — a small integer, machine-maintained; the
//! `# routes` zone is written by the router/GUI/agents, never hand-authored in the common
//! case). The layer is a copper slab **name** (Decision 13); provenance is a trailing
//! keyword (`pinned` is the default and omitted; `free` = router-owned, `hint`/`fixed`
//! complete the ladder). A via's span defaults to the full copper extent; an explicit
//! `<from>..<to>` names a blind/buried span. Parse is lenient (Decision 22): a missing or
//! duplicate id is re-minted with a `W_ROUTE_ID` warning, so a hand edit never bricks the
//! file — the guarantee is only that an id nothing disturbed stays put.
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
use crate::geom::{KeepoutKind, Material, Path, Role, Seg, Shape2D, Slab, ZRange, coord_ok};
use crate::id::{EntityId, TraceId, ViaId};
use crate::ir::{DefNode, GenDirective, RegionDecl, Source, board_rect, directive_coords};
use crate::route::{Trace, Via};
use std::collections::{BTreeMap, BTreeSet};

/// The parsed tier-1/tier-2 state: the generative program, the ID-keyed override maps,
/// and the persisted routing state zone (Decision 18 — routes are materialized but
/// *not derivable*, so they persist rather than re-solve). A named struct (not a
/// positional tuple) so adding a state section adds a field without churning every
/// destructuring site. `TraceId`/`ViaId` come from each `route`/`via` line's id token
/// (Decision 22); a line missing or duplicating an id is re-minted and records a
/// `W_ROUTE_ID` diagnostic in [`warnings`](Self::warnings).
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
    /// Non-fatal findings raised during parse — today only the lenient route-id
    /// diagnostics (`W_ROUTE_ID`: a missing or duplicate `route`/`via` id was re-minted,
    /// Decision 22). Empty on a clean parse (every route carried a distinct id, the
    /// serializer's own output). `LoadText` folds these onto the doc's
    /// [`ReconReport::route_id_warnings`](crate::doc::ReconReport::route_id_warnings); a
    /// hard syntax error never reaches here (parse returns `Err` instead).
    pub warnings: Vec<Diagnostic>,
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
    // Emitted in canonical `BTreeMap` (id) order; each `route`/`via` line carries its id as
    // a leading token (Decision 22 — the route's persistent identity, parsed back verbatim).
    // Empty ⇒ no section, so a routeless doc's text is byte-identical to before this feature.
    if !doc.traces.is_empty() || !doc.vias.is_empty() {
        out.push_str("\n# routes\n");
        for (id, t) in &doc.traces {
            out.push_str(&render_trace(*id, t));
            out.push('\n');
        }
        for (id, v) in &doc.vias {
            out.push_str(&render_via(*id, v));
            out.push('\n');
        }
    }
    out
}

// ----------------------------------------------------------------------------
// Submodules
// ----------------------------------------------------------------------------
//
// text.rs is the facade: it owns the module docs, the `Parsed` struct, the two
// public entry points (`serialize`/`parse`) and the block-forest walk
// (`parse_forest`/`lower_directive`) that dispatches into the domain parsers. The
// per-layer grammar lives in private submodules; every historically-public path
// (`crate::text::{parse, serialize, Parsed, Node, Block, parse_blocks,
// serialize_blocks, serialize_schematic_block}`) is kept reachable via the re-exports
// below. Submodules see each other's items (and the facade's imports) through their
// own `use super::*;`.
mod blocks;
mod def;
mod directive;
mod emit;
mod scan;
mod schematic;

// Re-export the moved items back into `crate::text::` so `super::*` (submodules and
// `tests`) and every external `crate::text::…` path keep resolving unchanged. These are
// crate-internal (`pub(crate)`) except the historically-`pub` `parse_blocks`/
// `serialize_blocks`/`serialize_schematic_block`, which stay `pub`.
pub(crate) use self::blocks::{BLOCK_INDENT, keyword_takes_block};
pub use self::blocks::{Block, Node, parse_blocks, serialize_blocks};
#[cfg(test)]
pub(crate) use self::blocks::{TEST_BLOCK_KEYWORD, split_header};
pub(crate) use self::def::{parse_def, render_def};
pub(crate) use self::directive::{Item, parse_line, render_directive, render_trace, render_via};
pub(crate) use self::emit::{
    fmt_len, fmt_path, fmt_point, parse_quat_tok, parse_role, parse_rot_deg, role_token,
};
pub(crate) use self::scan::{
    as_expr_value, extract_path, extract_points, is_ident, node_list, parse_if_clause, parse_len,
    path_and_point, path_is_polygon, quote_param_value, quote_value, split_last_dot,
    split_range_suffix, split_trailing_prov, split_ws_quoted, split_ws_quoted_parens, two_tokens,
    two_tokens_and_len, unquote,
};
pub use self::schematic::serialize_schematic_block;
pub(crate) use self::schematic::{
    check_coord_range, emit_layout_nodes, err_line, parse_layout_nodes, serialize_layout,
};

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
    let mut errors: Vec<Diagnostic> = Vec::new();
    // Routes/vias are collected with their (optional) explicit ids and resolved in a
    // second pass (`resolve_route_ids`), so a mint can be seeded above EVERY explicit id
    // in the section — including ids on lines parsed later — and thus never collide with a
    // hand-written id, whatever the file order (Decision 22).
    let mut pending = PendingRoutes::default();

    let top: Vec<Node> = blocks.into_iter().map(Node::Block).collect();
    parse_forest(&top, &mut parsed, &mut pending, &mut errors);
    resolve_route_ids(pending, &mut parsed);

    if errors.is_empty() {
        Ok(parsed)
    } else {
        Err(errors)
    }
}

/// Routes/vias parsed off the flat grammar, each with its explicit id (`None` = the line
/// omitted one) and source line number for diagnostics. Held until [`resolve_route_ids`]
/// assigns final `TraceId`/`ViaId`s in a second pass (Decision 22).
#[derive(Default)]
struct PendingRoutes {
    traces: Vec<(Option<u64>, Trace, u32)>,
    vias: Vec<(Option<u64>, Via, u32)>,
}

/// Resolve the parsed routes' explicit ids into final `TraceId`/`ViaId`s (Decision 22,
/// lenient parse). An explicit id that nothing else claims is kept verbatim; a *missing*
/// id (`None`) or a *duplicate* (an id an earlier line already took) is re-minted with a
/// `W_ROUTE_ID` warning. The mint allocator is seeded above the max explicit id across the
/// whole section, so a minted id can never collide with an explicit one that appears on a
/// later line — the parse is order-independent. Trace and via ids are separate namespaces.
fn resolve_route_ids(pending: PendingRoutes, parsed: &mut Parsed) {
    let mut alloc = crate::id::RouteIdAlloc::above(
        pending.traces.iter().filter_map(|(id, ..)| *id),
        pending.vias.iter().filter_map(|(id, ..)| *id),
    );
    for (id, trace, line) in pending.traces {
        let tid = match id {
            Some(n) if !parsed.traces.contains_key(&TraceId(n)) => TraceId(n),
            explicit => {
                let minted = alloc.mint_trace();
                parsed
                    .warnings
                    .push(route_id_warning("route", explicit, minted.0, line));
                minted
            }
        };
        parsed.traces.insert(tid, trace);
    }
    for (id, via, line) in pending.vias {
        let vid = match id {
            Some(n) if !parsed.vias.contains_key(&ViaId(n)) => ViaId(n),
            explicit => {
                let minted = alloc.mint_via();
                parsed
                    .warnings
                    .push(route_id_warning("via", explicit, minted.0, line));
                minted
            }
        };
        parsed.vias.insert(vid, via);
    }
}

/// The `W_ROUTE_ID` diagnostic for a re-minted route/via id (Decision 22). `explicit` is
/// the id the line carried (`None` = missing → minted; `Some(n)` = a duplicate of an
/// id already taken → re-minted `new` in its place).
fn route_id_warning(kind: &str, explicit: Option<u64>, new: u64, line: u32) -> Diagnostic {
    let msg = match explicit {
        None => format!("{kind} line has no id; minted `{new}`"),
        Some(n) => format!("{kind} id `{n}` is already used; re-minted `{new}`"),
    };
    Diagnostic::warning("W_ROUTE_ID", msg, Location::Span { line, col: 1 })
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
    pending: &mut PendingRoutes,
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
            let mut scratch_pending = PendingRoutes::default();
            parse_forest(&b.children, &mut scratch, &mut scratch_pending, errors);
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
            parse_forest(&b.children, parsed, pending, errors);
        } else {
            // A leaf directive lowers through the flat line grammar, exactly as before.
            lower_directive(b, parsed, pending, errors);
        }
    }
}

/// Lower a single directive's header line through the flat [`parse_line`] grammar into
/// `parsed`. Shared by the normal walk and the rejected-block child-diagnostics scan.
fn lower_directive(
    b: &Block,
    parsed: &mut Parsed,
    pending: &mut PendingRoutes,
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
        Ok(Item::Route(id, t)) => {
            let coords = t.path.iter().flat_map(|p| [p.x, p.y]).chain([t.width]);
            check_coord_range(coords.collect(), lineno, errors);
            // Id resolution/minting is deferred to `resolve_route_ids` (a second pass that
            // has seen every explicit id in the section).
            pending.traces.push((id, t, lineno));
        }
        Ok(Item::Via(id, v)) => {
            check_coord_range(vec![v.at.x, v.at.y, v.drill, v.pad], lineno, errors);
            pending.vias.push((id, v, lineno));
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
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests;
