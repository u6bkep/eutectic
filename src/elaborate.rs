//! Elaboration: generative source -> materialized instances, with ID-keyed
//! overrides reconciled on top.
//!
//! This is the load-bearing primitive of the whole architecture, exercised here
//! at the schematic-authoring level: clean generative truth + override deltas +
//! reconciliation. The same shape recurs at placement and routing.
//!
//! Reconciliation rules:
//!   - re-elaborating the same source reproduces the same entity ids (paths),
//!     so an override stays attached across a source change (minimal perturbation).
//!   - an override whose target no longer exists is *surfaced as a conflict*,
//!     never silently dropped.

use crate::diagnostic::{Diagnostic, Location};
use crate::doc::*;
use crate::geom::{
    DEFAULT_CHORD_TOL, Feature, NetFeature, Role, Shape2D, Slab, Stackup, ZRange, convex_hull,
};
use crate::id::{EntityId, NetId};
use crate::part::{Dir, PartDef, PartLib, courtyard_half_extents, courtyard_shape};
use crate::solve::{
    COURTYARD_VERIFY_TOL, Constraint, PLACE_TOL, Problem, courtyard_overlap_depth, dist, solve,
};
use std::collections::{BTreeMap, BTreeSet};

// The directive IR (RegionDecl, GenDirective, Source, DefNode, MAX_RANGE_INSTANCES
// and the coords/refs queries) now lives in `crate::ir`, the common downward
// dependency of both `text` and `elaborate`. Re-exported here so every existing
// `crate::elaborate::{...}` path keeps compiling unchanged. `board_rect` and
// `directive_coords` are the thin free-fn forms defined in `crate::ir`.
pub use crate::ir::{
    DefNode, GenDirective, MAX_RANGE_INSTANCES, RegionDecl, Source, board_rect, directive_coords,
    directive_refs,
};

/// Lower every generative directive (`param`, ranged/conditional/expression `inst`) into
/// the plain declarative `Source` the elaboration passes already understand (Decision
/// 21b). Runs once, before Pass 1, so the reconciliation machinery sees only concrete
/// `Instance` directives at concrete `path[i]` paths — an override or refdes pin attaches
/// to an expanded instance exactly as it would to a hand-written `inst path[i]`.
///
/// Steps:
///   1. resolve all `param` declarations into an [`Env`](crate::expr::Env) (cycle-safe);
///   2. for each `InstGenerative`, evaluate its range bounds and, per index `i`
///      (with `i` bound in scope), its `if=` conditional and `p:(expr)` values, emitting a
///      concrete [`Instance`](GenDirective::Instance) — or nothing when `if=` is false;
///   3. copy every other directive through unchanged.
///
/// Returns the expanded source **and** the set of instance paths a false `if=` dropped
/// (so the connectivity passes can skip — with a `W_DNP` warning — pins referencing a
/// depopulated part rather than reporting them as unknown). Any `E_EXPR` fault (parse,
/// unknown param, type error, inexact division, cycle, out-of-range/huge bound) is
/// collected as a house diagnostic; on any fault the whole expansion fails so no partial
/// program elaborates.
#[allow(clippy::type_complexity)]
fn expand_generative(
    source: &Source,
    lib: &PartLib,
) -> Result<
    (
        Source,
        BTreeSet<String>,
        BTreeMap<String, crate::schematic::SchematicLayout>,
    ),
    Vec<Diagnostic>,
> {
    use crate::expr;
    let mut errors: Vec<Diagnostic> = Vec::new();

    // Collect the def table (Decision 21a). A duplicate def name is an authoring conflict;
    // a def name that also names a library part is ambiguous — an `inst … <name>` could
    // mean either, so we reject the *definition* rather than silently letting one win
    // (the chosen rule: surface the collision, never guess). Both are hard faults.
    let mut defs: BTreeMap<String, &GenDirective> = BTreeMap::new();
    for d in source {
        if let GenDirective::Def { name, .. } = d {
            if defs.insert(name.clone(), d).is_some() {
                errors.push(Diagnostic::error(
                    "E_DEF",
                    format!("duplicate def `{name}`"),
                    Location::None,
                ));
            }
            if lib.contains_key(name) {
                errors.push(
                    Diagnostic::error(
                        "E_DEF_PART_AMBIGUOUS",
                        format!(
                            "def `{name}` has the same name as a library part; an `inst … {name}` \
                             would be ambiguous"
                        ),
                        Location::None,
                    )
                    .with_help("rename the def (or the part) so instantiation is unambiguous"),
                );
            }
        }
    }

    // Top-level param environment (Decision 21b). Def bodies extend a *clone* of this with
    // their bound params (outer params visible, def params shadow — the same
    // innermost-wins rule as the range loop variable `i`).
    let decls: BTreeMap<String, String> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Param { name, expr } => Some((name.clone(), expr.clone())),
            _ => None,
        })
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for d in source {
        if let GenDirective::Param { name, .. } = d
            && !seen.insert(name.as_str())
        {
            errors.push(Diagnostic::error(
                "E_EXPR",
                format!("duplicate param `{name}`"),
                Location::None,
            ));
        }
    }
    let env = match expr::resolve_params(&decls) {
        Ok(env) => env,
        Err(e) => {
            errors.push(Diagnostic::error("E_EXPR", e, Location::None));
            return Err(errors);
        }
    };

    // If the def table itself is malformed (dup/ambiguous), fail now — stamping against a
    // broken table would cascade confusingly.
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut out: Source = Vec::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();
    // Port map: `<full-inst-path>.<port>` → the internal pin it binds, as
    // `(<full-comp-path>, <selector>)`. Built while stamping; a binding may itself target
    // another port (a def re-exporting a nested def's port), so it is resolved
    // transitively before rewriting outer connections.
    let mut port_map: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
    let mut authored_nets: BTreeSet<String> = BTreeSet::new();
    let mut internal_nets: BTreeMap<String, String> = BTreeMap::new();
    // Per-instance stamped schematic fragments (Decision 20 embedded in a def), keyed by
    // def-instance path. Filled by `stamp_def` for every instance whose def carries a
    // `schematic { … }` block; consumed by the derived reflow to expand a def-instance sym.
    let mut def_fragments: BTreeMap<String, crate::schematic::SchematicLayout> = BTreeMap::new();

    let mut ctx = ExpandCtx {
        defs: &defs,
        errors: &mut errors,
        out: &mut out,
        dropped: &mut dropped,
        port_map: &mut port_map,
        authored_nets: &mut authored_nets,
        internal_nets: &mut internal_nets,
        def_fragments: &mut def_fragments,
    };
    // Stamp the whole program at the empty prefix. Connections are emitted with their
    // paths prefixed but *ports unresolved*; the rewrite pass below threads them through
    // the port map.
    expand_scope(source, &env, "", &mut Vec::new(), &mut ctx);

    if !errors.is_empty() {
        return Err(errors);
    }

    // Net-name collision (Decision 21a): an authored top-level `net` whose name equals a
    // stamped def-internal net (`net sense[0].fb …` at top level colliding with instance
    // `sense[0]`'s internal `fb`) would silently *merge* the two — a silent-wrong-
    // connectivity class. Reject it, naming both sides. Deliberate internal-net tapping is
    // a future feature behind explicit syntax; silence is not it. Order-independent (both
    // sets are collected across the whole walk before this check).
    for name in &authored_nets {
        if let Some(inst) = internal_nets.get(name) {
            errors.push(
                Diagnostic::error(
                    "E_DEF_NET_COLLISION",
                    format!(
                        "authored net `{name}` collides with the internal net of def instance \
                         `{inst}` (which elaborates to the same path-prefixed name)"
                    ),
                    Location::Net(NetId::new(name.clone())),
                )
                .with_help(
                    "rename the authored net, or connect to the module through a `port` instead \
                     of tapping its internal net by name",
                ),
            );
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Resolve every emitted connection's pins through the port map (transitively — a port
    // may bind another port). `out` at this point holds only concrete `Instance`s and
    // path-prefixed connections; rewrite the connections in place.
    resolve_ports_in_source(&mut out, &port_map, &mut errors);

    if errors.is_empty() {
        Ok((out, dropped, def_fragments))
    } else {
        Err(errors)
    }
}

/// Depth cap on def instantiation nesting (Decision 21a anti-footgun, consistent with the
/// range-count and expression-depth caps). A def reaching itself through any chain is
/// caught earlier as an explicit cycle error; this bounds a pathological but acyclic
/// nesting before it exhausts the stack.
const MAX_DEF_DEPTH: usize = 64;

/// The mutable sinks threaded through the recursive def-stamping walk, bundled so the
/// recursion signature stays legible.
struct ExpandCtx<'a> {
    defs: &'a BTreeMap<String, &'a GenDirective>,
    errors: &'a mut Vec<Diagnostic>,
    out: &'a mut Source,
    dropped: &'a mut BTreeSet<String>,
    port_map: &'a mut BTreeMap<(String, String), (String, String)>,
    /// Net names emitted at the top level (authored `net`/`nc` with no def prefix), each
    /// paired with the `nc`/`net` context. Used to catch an authored net that silently
    /// collides with a stamped def-internal net (see `internal_nets`).
    authored_nets: &'a mut BTreeSet<String>,
    /// Stamped def-internal net names (a `net` emitted under a non-empty instance prefix),
    /// mapped to the def-instance path that produced it — for a collision diagnostic that
    /// names both sides.
    internal_nets: &'a mut BTreeMap<String, String>,
    /// Per-instance stamped schematic layout fragments (Decision 20 embedded in a def):
    /// keyed by the def-instance path (`sense[0]`), the value is the def's layout fragment
    /// with every internal `sym` path and wire endpoint prefixed by that instance path. The
    /// derived reflow expands a doc-level `sym <instance>` into this fragment. Only def
    /// instances whose def carries a `schematic { … }` block appear here.
    def_fragments: &'a mut BTreeMap<String, crate::schematic::SchematicLayout>,
}

/// Deep-copy a def's internal layout fragment, prefixing every addressable path by the
/// instance path (`ipath`) so a stamped fragment addresses the same instances the body
/// stamping produced. `Symbol.path` and both `Wire` endpoint `comp`s gain the prefix (via
/// [`prefix_path`], the same idiom the body/net stamping uses); container structure, wire
/// waypoints/pins, and comment/blank trivia are copied unchanged (waypoints are
/// schematic-space coordinates, not paths; a pin is a selector on the now-prefixed comp).
fn prefix_fragment(
    frag: &crate::schematic::SchematicLayout,
    ipath: &str,
) -> crate::schematic::SchematicLayout {
    use crate::schematic::{LayoutNode, SchematicLayout};
    fn walk(nodes: &[LayoutNode], ipath: &str) -> Vec<LayoutNode> {
        nodes
            .iter()
            .map(|n| match n {
                LayoutNode::Symbol(s) => LayoutNode::Symbol(crate::schematic::Symbol {
                    path: prefix_path(ipath, &s.path),
                    ..s.clone()
                }),
                LayoutNode::Wire(w) => LayoutNode::Wire(crate::schematic::Wire {
                    a: crate::schematic::WireEnd {
                        comp: prefix_path(ipath, &w.a.comp),
                        pin: w.a.pin.clone(),
                    },
                    b: crate::schematic::WireEnd {
                        comp: prefix_path(ipath, &w.b.comp),
                        pin: w.b.pin.clone(),
                    },
                    waypoints: w.waypoints.clone(),
                }),
                LayoutNode::Container(c) => LayoutNode::Container(crate::schematic::Container {
                    children: walk(&c.children, ipath),
                    ..c.clone()
                }),
                LayoutNode::Comment(_) | LayoutNode::Blank => n.clone(),
            })
            .collect()
    }
    SchematicLayout {
        roots: walk(&frag.roots, ipath),
    }
}

/// Prefix a def-relative path/name with the instance path (`""` at top level → unchanged;
/// `"sense[0]"` → `"sense[0].fb"`). Internal nets are path-prefixed the same way so two
/// instances of the same def never collide on a net name (`sense[0].fb` vs `sense[1].fb`).
fn prefix_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Stamp a source fragment (the top-level program, or a def body) at `prefix`, evaluating
/// its expressions in `env`. Emits concrete `Instance`s and path-prefixed connections into
/// `ctx.out`; a def instantiation recurses. `chain` is the active def-name stack for cycle
/// detection.
fn expand_scope(
    source: &Source,
    env: &crate::expr::Env,
    prefix: &str,
    chain: &mut Vec<String>,
    ctx: &mut ExpandCtx,
) {
    use crate::expr;
    for d in source {
        match d {
            // Declarations, not materialized directives.
            GenDirective::Param { .. } | GenDirective::Def { .. } => {}
            GenDirective::Instance {
                path,
                part,
                params,
                label,
            } => {
                let ipath = prefix_path(prefix, path);
                if ctx.defs.contains_key(part) {
                    // A plain (non-generative) def instantiation. Its verbatim `p:` params
                    // are the def-param overrides.
                    stamp_def(part, &ipath, params, env, chain, ctx);
                } else {
                    ctx.out.push(GenDirective::Instance {
                        path: ipath,
                        part: part.clone(),
                        params: params.clone(),
                        label: label.clone(),
                    });
                }
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
                let Some(indices) = eval_range(path, range, env, ctx.errors) else {
                    continue;
                };
                for idx in indices {
                    let rel = match idx {
                        Some(i) => format!("{path}[{i}]"),
                        None => path.clone(),
                    };
                    let ipath = prefix_path(prefix, &rel);
                    // Evaluate `if=`/params in the current scope with `i` bound.
                    let scope = bind_index(env, idx);
                    if let Some(cond) = if_expr {
                        match expr::eval_str(cond, &scope).and_then(|v| v.as_bool()) {
                            Ok(true) => {}
                            Ok(false) => {
                                ctx.dropped.insert(ipath);
                                continue;
                            }
                            Err(e) => {
                                ctx.errors.push(Diagnostic::error(
                                    "E_EXPR",
                                    format!("`if=` on `{ipath}`: {e}"),
                                    Location::None,
                                ));
                                continue;
                            }
                        }
                    }
                    let Some(merged) = eval_params(&ipath, params, param_exprs, &scope, ctx.errors)
                    else {
                        continue;
                    };
                    if ctx.defs.contains_key(part) {
                        stamp_def(part, &ipath, &merged, env, chain, ctx);
                    } else {
                        ctx.out.push(GenDirective::Instance {
                            path: ipath,
                            part: part.clone(),
                            params: merged,
                            label: label.clone(),
                        });
                    }
                }
            }
            // Connectivity: prefix the component paths and net name; ports are resolved
            // later. Copied through with paths rewritten.
            GenDirective::ConnectPins { net, pins } => {
                let full = prefix_path(prefix, net);
                if prefix.is_empty() {
                    ctx.authored_nets.insert(full.clone());
                } else {
                    // A stamped def-internal net; remember which instance produced it so a
                    // collision with an authored net names both sides.
                    ctx.internal_nets.insert(full.clone(), prefix.to_string());
                }
                ctx.out.push(GenDirective::ConnectPins {
                    net: full,
                    pins: pins
                        .iter()
                        .map(|(c, s)| (prefix_path(prefix, c), s.clone()))
                        .collect(),
                });
            }
            GenDirective::NoConnect { pins } => {
                ctx.out.push(GenDirective::NoConnect {
                    pins: pins
                        .iter()
                        .map(|(c, s)| (prefix_path(prefix, c), s.clone()))
                        .collect(),
                });
            }
            GenDirective::ConnectInterface { a, b } => {
                ctx.out.push(GenDirective::ConnectInterface {
                    a: (prefix_path(prefix, &a.0), a.1.clone()),
                    b: (prefix_path(prefix, &b.0), b.1.clone()),
                });
            }
            // Everything else is only legal at the top level (a def body's grammar admits
            // only inst/net/nc/connect/port). At the top level (`prefix.is_empty()`) copy
            // it through unchanged; inside a def body it cannot occur (the parser rejects
            // it), so no prefixing of placement/geometry is needed.
            other => ctx.out.push(other.clone()),
        }
    }
}

/// Evaluate a def instance's port surface and stamp its body at `ipath`. `overrides` are
/// the resolved `p:` param values from the instantiation (display-normal strings);
/// `outer_env` is the scope the *overrides were already evaluated in* — the def's own
/// param env is built here from overrides-or-defaults. Records the instance's ports into
/// `ctx.port_map`, then recurses into the body.
///
/// Scope rule: the def body is stamped in the def's param env layered over `outer_env`
/// (the doc/enclosing-def params). The **range loop variable `i` is deliberately NOT
/// visible inside the body** — a ranged def instantiation (`inst sense[0..n] S`) binds `i`
/// only when evaluating the instantiation's *own* `if=`/`p:` (in `expand_scope`), and
/// `stamp_def` receives the un-indexed `outer_env`. So a body expression referring to `i`
/// is an unknown-variable `E_EXPR`. This keeps the body a pure function of its declared
/// params: to make the index available inside, pass it explicitly as a param
/// (`inst sense[0..n] S p:idx=(i)`), which the body then reads as `idx`.
fn stamp_def(
    def_name: &str,
    ipath: &str,
    overrides: &BTreeMap<String, String>,
    outer_env: &crate::expr::Env,
    chain: &mut Vec<String>,
    ctx: &mut ExpandCtx,
) {
    use crate::expr;
    // Cycle: a def reaching itself through any chain of instantiations. This is *dynamic*
    // (walk-time) detection: a recursive instantiation reachable only through a false
    // `if=` is never stamped, so it is never walked and never reported — consistent with
    // `if=false` dropping the whole subtree. A statically-present-but-dead cycle is thus
    // silent by design, not a missed diagnostic.
    if chain.iter().any(|n| n == def_name) {
        chain.push(def_name.to_string());
        ctx.errors.push(Diagnostic::error(
            "E_DEF_CYCLE",
            format!("def cycle: {}", chain.join(" → ")),
            Location::None,
        ));
        chain.pop();
        return;
    }
    if chain.len() >= MAX_DEF_DEPTH {
        ctx.errors.push(Diagnostic::error(
            "E_DEF_DEPTH",
            format!(
                "def nesting exceeds the depth cap ({MAX_DEF_DEPTH}) at `{ipath}` — likely an \
                 unintended explosion"
            ),
            Location::None,
        ));
        return;
    }

    let (params, body, ports, layout) = match ctx.defs.get(def_name) {
        Some(GenDirective::Def {
            params,
            body,
            ports,
            layout,
            ..
        }) => (params, body, ports, layout),
        _ => return, // unreachable: caller checked `defs.contains_key`
    };

    // Build the def's param environment. A declared param takes its value from the `p:`
    // override if given, else its default expression evaluated in the def's scope. Outer
    // params are visible; a def param shadows an outer one of the same name (innermost
    // wins — the same shadowing principle as the range loop variable, though note `i`
    // itself is *not* forwarded into the body; see this fn's doc comment). Unknown `p:`
    // overrides (not a declared param) are a hard fault (a typo, never silently ignored).
    let declared: BTreeSet<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
    for k in overrides.keys() {
        if !declared.contains(k.as_str()) {
            ctx.errors.push(Diagnostic::error(
                "E_DEF",
                format!("`{ipath}` sets `p:{k}`, which def `{def_name}` does not declare"),
                Location::None,
            ));
        }
    }
    let mut def_env = outer_env.clone();
    let mut param_err = false;
    for (k, default) in params {
        let value = match overrides.get(k) {
            // An override arrived as a display-normal string (already evaluated in the
            // outer scope by the caller). Re-parse it as an expression so `p:gain=2` and a
            // default of `2` both land as the same `Value`. A quantity like `4.7k` parses
            // back through the same grammar.
            Some(s) => expr::eval_str(s, outer_env),
            None => expr::eval_str(default, &def_env),
        };
        match value {
            Ok(v) => {
                def_env.insert(k.clone(), v);
            }
            Err(e) => {
                ctx.errors.push(Diagnostic::error(
                    "E_EXPR",
                    format!("def `{def_name}` param `{k}` at `{ipath}`: {e}"),
                    Location::None,
                ));
                param_err = true;
            }
        }
    }
    if param_err {
        return;
    }

    // Record this instance's ports (paths prefixed by `ipath`). A port target's internal
    // path is def-relative, so it gains the instance prefix; its binding may itself be a
    // port (chased transitively in `resolve_ports_in_source`).
    for (pname, (tpath, tsel)) in ports {
        ctx.port_map.insert(
            (ipath.to_string(), pname.clone()),
            (prefix_path(ipath, tpath), tsel.clone()),
        );
    }

    // Record this instance's schematic layout fragment (Decision 20 embedded in a def), if
    // the def declares one. Every internal path is prefixed by `ipath` so the stamped
    // fragment addresses the instances the body stamping produced; the derived reflow later
    // expands a doc-level `sym <ipath>` into this fragment. Two instances of the same def
    // thus record byte-identical fragments modulo their distinct instance prefixes — the
    // "renders identically everywhere" guarantee.
    if let Some(frag) = layout {
        ctx.def_fragments
            .insert(ipath.to_string(), prefix_fragment(frag, ipath));
    }

    // Stamp the body in the def's scope at the instance prefix.
    chain.push(def_name.to_string());
    let body_src: Source = body
        .iter()
        .filter_map(|n| match n {
            DefNode::Directive(d) => Some(d.clone()),
            DefNode::Comment(_) | DefNode::Blank => None,
        })
        .collect();
    expand_scope(&body_src, &def_env, ipath, chain, ctx);
    chain.pop();
}

/// Evaluate a ranged `inst`'s index set (`Some(lo..hi)` → `[Some(lo)..]`, `None` → a
/// single `[None]`). Returns `None` (and pushes diagnostics) on a bad/over-cap/negative
/// bound so the caller skips the directive. Shared by the top-level and def-body walks.
fn eval_range(
    path: &str,
    range: &Option<(String, String)>,
    env: &crate::expr::Env,
    errors: &mut Vec<Diagnostic>,
) -> Option<Vec<Option<i64>>> {
    use crate::expr;
    let Some((lo_s, hi_s)) = range else {
        return Some(vec![None]);
    };
    let lo = expr::eval_str(lo_s, env).and_then(|v| v.as_index());
    let hi = expr::eval_str(hi_s, env).and_then(|v| v.as_index());
    match (lo, hi) {
        (Ok(lo), Ok(hi)) => {
            if lo < 0 || hi < 0 {
                errors.push(Diagnostic::error(
                    "E_EXPR",
                    format!("range `{path}[{lo}..{hi}]` has a negative bound"),
                    Location::None,
                ));
                return None;
            }
            let count = (hi - lo).max(0);
            if count > MAX_RANGE_INSTANCES {
                errors.push(Diagnostic::error(
                    "E_EXPR",
                    format!(
                        "range `{path}[{lo}..{hi}]` expands to {count} instances, over the \
                         {MAX_RANGE_INSTANCES} cap"
                    ),
                    Location::None,
                ));
                return None;
            }
            Some((lo..hi).map(Some).collect())
        }
        (lo, hi) => {
            for r in [lo, hi] {
                if let Err(e) = r {
                    errors.push(Diagnostic::error(
                        "E_EXPR",
                        format!("range bound of `{path}`: {e}"),
                        Location::None,
                    ));
                }
            }
            None
        }
    }
}

/// Clone `env` and bind the range loop variable `i` when an index is present (innermost
/// wins — shadows a doc/def `param i`, the documented rule). Returns `env` unchanged
/// (cloned) for an unindexed instance.
fn bind_index(env: &crate::expr::Env, idx: Option<i64>) -> crate::expr::Env {
    let mut scope = env.clone();
    if let Some(i) = idx {
        scope.insert("i".to_string(), crate::expr::Value::Int(i));
    }
    scope
}

/// Evaluate an `inst`'s expression params into display-normal strings, merged over its
/// verbatim ones. Returns `None` (pushing diagnostics) if any expression faults.
fn eval_params(
    ipath: &str,
    params: &BTreeMap<String, String>,
    param_exprs: &BTreeMap<String, String>,
    scope: &crate::expr::Env,
    errors: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, String>> {
    use crate::expr;
    let mut merged = params.clone();
    let mut ok = true;
    for (k, ex) in param_exprs {
        match expr::eval_str(ex, scope) {
            Ok(v) => {
                merged.insert(k.clone(), format_value(v));
            }
            Err(e) => {
                errors.push(Diagnostic::error(
                    "E_EXPR",
                    format!("param `p:{k}` on `{ipath}`: {e}"),
                    Location::None,
                ));
                ok = false;
            }
        }
    }
    ok.then_some(merged)
}

/// Rewrite every connection pin in `out` that names a def-instance port through to the
/// bound internal pin (Decision 21a). A port binding may target another port (a def
/// re-exporting a nested def's port), so each `(comp, sel)` is chased transitively until
/// it names a real component pin — with cycle protection (a self-referential port map is
/// a bug, reported rather than looped).
fn resolve_ports_in_source(
    out: &mut Source,
    port_map: &BTreeMap<(String, String), (String, String)>,
    errors: &mut Vec<Diagnostic>,
) {
    if port_map.is_empty() {
        return;
    }
    let mut resolve = |comp: &str, sel: &str| -> (String, String) {
        let mut cur = (comp.to_string(), sel.to_string());
        let mut hops = 0;
        while let Some(next) = port_map.get(&cur) {
            cur = next.clone();
            hops += 1;
            if hops > port_map.len() + 1 {
                errors.push(Diagnostic::error(
                    "E_DEF",
                    format!("port binding cycle resolving `{comp}.{sel}`"),
                    Location::None,
                ));
                break;
            }
        }
        cur
    };
    for d in out.iter_mut() {
        match d {
            GenDirective::ConnectPins { pins, .. } | GenDirective::NoConnect { pins } => {
                for (c, s) in pins.iter_mut() {
                    let (nc, ns) = resolve(c, s);
                    *c = nc;
                    *s = ns;
                }
            }
            GenDirective::ConnectInterface { a, b } => {
                let (ca, sa) = resolve(&a.0, &a.1);
                a.0 = ca;
                a.1 = sa;
                let (cb, sb) = resolve(&b.0, &b.1);
                b.0 = cb;
                b.1 = sb;
            }
            _ => {}
        }
    }
}

/// Format an evaluated [`Value`](crate::expr::Value) into the display-normal string a
/// component param stores (Decision 14 — params are authored strings at rest). An
/// integer prints plainly; a quantity prints its minimal decimal via `format_si` with no
/// unit (the unit spelling is a display concern owned by the class template downstream);
/// a boolean prints `true`/`false`.
fn format_value(v: crate::expr::Value) -> String {
    use crate::expr::Value;
    match v {
        Value::Int(n) => n.to_string(),
        Value::Quantity(q) => q.format_si(""),
        Value::Bool(b) => b.to_string(),
    }
}

/// Result of elaboration before it is folded into a Doc.
pub struct Elaborated {
    pub components: BTreeMap<EntityId, Component>,
    pub nets: BTreeMap<NetId, Net>,
    pub no_connects: BTreeSet<PinRef>,
    pub report: ReconReport,
    /// Instance paths a false `if=` population conditional depopulated (Decision 21b DNP).
    /// These are *intentionally* absent from `components` — not faults, not typos — so a
    /// consumer distinguishing "unknown to the source" from "deliberately unpopulated"
    /// (e.g. the schematic-layout gate, Decision 20c) reads this rather than treating an
    /// absent path as an error. Empty when no `if=` dropped anything.
    pub dnp_dropped: BTreeSet<String>,
    /// Per-instance stamped schematic layout fragments (Decision 20 embedded in a def),
    /// keyed by def-instance path (`sense[0]`). Each is the def's internal `schematic { … }`
    /// fragment with every `sym` path / wire endpoint prefixed by the instance path, so the
    /// derived reflow can expand a doc-level `sym <instance>` into the fragment's placements
    /// (a reused circuit renders identically at every instantiation). Empty when no
    /// instantiated def carries a layout fragment.
    pub def_fragments: BTreeMap<String, crate::schematic::SchematicLayout>,
}

/// Elaborate a source program into materialized instances + connectivity,
/// applying ID-keyed overrides. On a structural fault the whole elaboration aborts
/// (atomic transaction) and returns **all** independent faults it found in one pass
/// (collect-all), suppressing only the cascade from a poisoned entity. Findings on
/// a *valid* model (reconciliation outcomes) ride in the returned [`Elaborated`]'s
/// [`ReconReport`], not in this error channel.
pub fn elaborate(
    source: &Source,
    overrides: &BTreeMap<EntityId, Override>,
    refdes_pins: &BTreeMap<EntityId, String>,
    lib: &PartLib,
) -> Result<Elaborated, Vec<Diagnostic>> {
    // Lower the generative tier (params, ranged/conditional/expression `inst`) into
    // concrete declarative directives *first*, so every pass below — including
    // reconciliation, which addresses instances by their `path[i]` — sees only plain
    // `Instance` directives (Decision 21b). A generative fault (bad expression, cycle,
    // out-of-range bound) aborts the whole transaction, like any structural fault.
    let (expanded, dnp_dropped, def_fragments) = expand_generative(source, lib)?;
    let source = &expanded;

    let mut components: BTreeMap<EntityId, Component> = BTreeMap::new();
    let mut nets: BTreeMap<NetId, Net> = BTreeMap::new();
    let mut order = 0i64; // deterministic default placement counter

    // Collect-all: accumulate structural faults instead of returning the first.
    // `reported_missing` is the cascade-suppression set — an entity that does not
    // exist (failed to instantiate, or never declared) is reported once, and all
    // later references to it are silenced so the real fault isn't buried.
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut reported_missing: BTreeSet<EntityId> = BTreeSet::new();
    // A path depopulated by a false `if=` (Decision 21b DNP) is *intentionally* absent,
    // not a fault: seed it into the cascade-suppression set so **every** reference to it
    // (connection *or* placement) skips silently via `note_missing` instead of raising
    // `E_UNKNOWN_INSTANCE`. The dangling references are surfaced uniformly as `W_DNP`
    // warnings by the single scan below (symmetric across directive kinds — a `near` on a
    // depopulated part is as visible as a `net` on it). `dnp_dangling` collects those.
    let mut dnp_dangling: Vec<(String, String)> = Vec::new();
    for p in &dnp_dropped {
        reported_missing.insert(EntityId::new(p.clone()));
    }
    // A reference is "into a dropped subtree" if its path equals a dropped path *or* lies
    // beneath one (`<dropped>.…`). The latter matters for a `def` instance depopulated by
    // `if=false` (Decision 21a): the whole stamped subtree is never materialized, so a
    // ref to a *leaf pin* of that module (`net OUT a.R1.p2` when `inst a … if=false`) is
    // as intentionally-absent as a ref to `a` itself — it must degrade to `W_DNP`, not
    // hard-error `E_UNKNOWN_INSTANCE`. The prefix rule captures both.
    let is_dnp_dropped = |path: &str| -> bool {
        dnp_dropped.iter().any(|d| {
            path == d.as_str() || path.starts_with(d.as_str()) && path[d.len()..].starts_with('.')
        })
    };
    if !dnp_dropped.is_empty() {
        for d in source {
            for (ctx, path) in directive_refs(d) {
                if is_dnp_dropped(&path) {
                    // Pre-seed the cascade-suppression set so the pass that would resolve
                    // this ref finds it already "reported" and skips it silently (no
                    // `E_UNKNOWN_INSTANCE`), exactly as an exact dropped-path ref is
                    // suppressed. This handles deep refs (`a.R1`) whose specific id was
                    // never in `dnp_dropped` (which holds only the dropped instance path).
                    reported_missing.insert(EntityId::new(path.clone()));
                    dnp_dangling.push((ctx, path));
                }
            }
        }
    }

    // Pass 1: instances.
    for d in source {
        if let GenDirective::Instance {
            path,
            part,
            params,
            label,
        } = d
        {
            let id = EntityId::new(path.clone());
            if !lib.contains_key(part) {
                errors.push(
                    Diagnostic::error(
                        "E_UNKNOWN_PART",
                        format!("instance `{path}` uses unknown part `{part}`"),
                        Location::Entity(id.clone()),
                    )
                    .with_help(known_parts(lib)),
                );
                // Poison: the instance does not exist, so suppress its cascade.
                reported_missing.insert(id);
                continue;
            }
            if components.contains_key(&id) {
                // The first definition wins; the entity exists, so it is NOT poisoned.
                errors.push(Diagnostic::error(
                    "E_DUPLICATE_INSTANCE",
                    format!("duplicate instance `{path}`"),
                    Location::Entity(id),
                ));
                continue;
            }
            // Default placement: a free DOF, laid out in a row.
            let pos = Dof {
                value: Point {
                    x: order * 10 * MM,
                    y: 0,
                },
                prov: Provenance::Free,
            };
            order += 1;
            components.insert(
                id.clone(),
                Component {
                    id,
                    part: part.clone(),
                    pos,
                    orient: Orient::default(),
                    params: params.clone(),
                    label: label.clone(),
                },
            );
        }
    }

    // Pass 2: source-provided default placement (still free).
    for d in source {
        if let GenDirective::Place { path, pos } = d {
            let id = EntityId::new(path.clone());
            if note_missing(
                &id,
                &components,
                &mut reported_missing,
                &mut errors,
                "place",
            ) {
                continue;
            }
            components
                .get_mut(&id)
                .expect("note_missing confirmed presence")
                .pos
                .value = *pos;
        }
    }

    // Pass 2a: orientation (a settable attribute, resolved before constraints so a
    // NearPin target's pin offset can be rotated correctly).
    for d in source {
        if let GenDirective::Rotate { path, orient } = d {
            let id = EntityId::new(path.clone());
            if note_missing(
                &id,
                &components,
                &mut reported_missing,
                &mut errors,
                "rotate",
            ) {
                continue;
            }
            // The quaternion is already valid by construction (the text front-end lowers
            // any angle, so there is no off-axis rejection anymore) — just assign it.
            components
                .get_mut(&id)
                .expect("note_missing confirmed presence")
                .orient = *orient;
        }
    }

    // Pass 2b: collect hard placement constraints (Fix), the board outline, and
    // relational constraints for the solver.
    let mut fixmap: BTreeMap<EntityId, Point> = BTreeMap::new();
    // The board region (outline ∖ cutouts) as a `Shape2D::Area`; movable components are
    // kept inside it (and out of its holes) by the solver.
    let board = board_region(source).map(|region| Shape2D::Area { region });
    let mut relational: Vec<Constraint> = Vec::new();
    for d in source {
        match d {
            GenDirective::Fix { path, pos } => {
                let id = EntityId::new(path.clone());
                if note_missing(&id, &components, &mut reported_missing, &mut errors, "fix") {
                    continue;
                }
                fixmap.insert(id, *pos);
            }
            GenDirective::Near { a, b, within } => {
                let (a, b) = (EntityId::new(a.clone()), EntityId::new(b.clone()));
                // Evaluate both so both are reported if both are missing.
                let am = note_missing(&a, &components, &mut reported_missing, &mut errors, "near");
                let bm = note_missing(&b, &components, &mut reported_missing, &mut errors, "near");
                if am || bm {
                    continue;
                }
                relational.push(Constraint::Near {
                    a,
                    b,
                    within: *within,
                });
            }
            GenDirective::NearPin {
                a,
                b_comp,
                b_pin,
                within,
            } => {
                let aid = EntityId::new(a.clone());
                let bid = EntityId::new(b_comp.clone());
                let am = note_missing(
                    &aid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "nearpin",
                );
                let bm = note_missing(
                    &bid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "nearpin",
                );
                if am || bm {
                    continue;
                }
                // Pre-rotate the target pin's local offset by b's orientation; the
                // result is a constant offset the solver adds to b's position.
                let bc = &components[&bid];
                let bdef = &lib[&bc.part];
                // A selector may name several pads (a power rail); for a geometric
                // anchor we target the first by pad order — deterministic and enough
                // for a placement hint.
                match bdef.resolve_selector(b_pin).into_iter().next() {
                    // A discrete pad always has an offset; an interface signal could
                    // be in `signals` but absent from `offsets` (a malformed
                    // InterfaceDef) — surface that, never panic.
                    Some(num) => match bdef.pin_offset(&num) {
                        Some(off) => relational.push(Constraint::NearPin {
                            a: aid,
                            b: bid,
                            b_off: bc.orient.apply(off),
                            within: *within,
                        }),
                        None => errors.push(Diagnostic::error(
                            "E_PIN_NO_OFFSET",
                            format!("nearpin: `{b_comp}` pin `{b_pin}` has no offset"),
                            Location::Entity(bid),
                        )),
                    },
                    None => errors.push(
                        Diagnostic::error(
                            "E_UNKNOWN_PIN",
                            format!(
                                "nearpin: `{b_comp}` (part `{}`) has no pin `{b_pin}`",
                                bc.part
                            ),
                            Location::Entity(bid),
                        )
                        .with_help(available_pins(bdef)),
                    ),
                }
            }
            GenDirective::MinSep { a, b, gap } => {
                let (a, b) = (EntityId::new(a.clone()), EntityId::new(b.clone()));
                let am = note_missing(
                    &a,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "minsep",
                );
                let bm = note_missing(
                    &b,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "minsep",
                );
                if am || bm {
                    continue;
                }
                relational.push(Constraint::MinSep { a, b, gap: *gap });
            }
            GenDirective::AlignX { nodes } => {
                let nodes: Vec<EntityId> = nodes.iter().map(|n| EntityId::new(n.clone())).collect();
                let mut any_missing = false;
                for n in &nodes {
                    any_missing |=
                        note_missing(n, &components, &mut reported_missing, &mut errors, "alignx");
                }
                if !any_missing {
                    relational.push(Constraint::AlignX { nodes });
                }
            }
            GenDirective::AlignY { nodes } => {
                let nodes: Vec<EntityId> = nodes.iter().map(|n| EntityId::new(n.clone())).collect();
                let mut any_missing = false;
                for n in &nodes {
                    any_missing |=
                        note_missing(n, &components, &mut reported_missing, &mut errors, "aligny");
                }
                if !any_missing {
                    relational.push(Constraint::AlignY { nodes });
                }
            }
            _ => {}
        }
    }

    // Pass 2c: overlap-avoidance (issues 0005 / 0019). No two component courtyards may
    // overlap; generate a NoOverlap constraint for every pair (O(N²), as noted in
    // the ticket). Each courtyard is lowered once per component to a rounded convex
    // polygon in its local frame, already rotated by its orientation (see
    // [`component_courtyard`]); a part with no geometry has none and is dropped here.
    // `components` is a BTreeMap, so the order — and thus the constraint set — is
    // deterministic.
    let courts: Vec<(EntityId, Vec<Point>, Nm)> = components
        .iter()
        .filter_map(|(id, c)| {
            component_courtyard(&lib[&c.part], c.orient).map(|(poly, r)| (id.clone(), poly, r))
        })
        .collect();
    for i in 0..courts.len() {
        for j in (i + 1)..courts.len() {
            relational.push(Constraint::NoOverlap {
                a: courts[i].0.clone(),
                a_poly: courts[i].1.clone(),
                a_r: courts[i].2,
                b: courts[j].0.clone(),
                b_poly: courts[j].1.clone(),
                b_r: courts[j].2,
            });
        }
    }

    // Pass 3: connections. A selector resolves against the part: a functional name
    // fans out to every pad with that name (so a six-pad power rail gets six
    // members), a pad number picks one pad. An unresolvable selector — a typo or a
    // pin the part doesn't have — is reported (each, they don't cascade) and the
    // member is skipped; a reference to a missing component is cascade-suppressed.
    let mut no_connects: BTreeSet<PinRef> = BTreeSet::new();
    for d in source {
        match d {
            GenDirective::ConnectInterface { a, b } => {
                let aid = EntityId::new(a.0.clone());
                let bid = EntityId::new(b.0.clone());
                let am = note_missing(
                    &aid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "connect",
                );
                let bm = note_missing(
                    &bid,
                    &components,
                    &mut reported_missing,
                    &mut errors,
                    "connect",
                );
                if am || bm {
                    continue;
                }
                connect_interface(&components, lib, a, b, &mut nets, &mut errors);
            }
            GenDirective::ConnectPins { net, pins } => {
                let id = NetId::new(net.clone());
                let entry = nets.entry(id.clone()).or_insert_with(|| Net {
                    id,
                    name: net.clone(),
                    members: BTreeSet::new(),
                });
                for (comp, sel) in pins {
                    let cid = EntityId::new(comp.clone());
                    let ctx = format!("net `{net}`");
                    if note_missing(&cid, &components, &mut reported_missing, &mut errors, &ctx) {
                        continue;
                    }
                    let def = &lib[&components[&cid].part];
                    let nums = def.resolve_selector(sel);
                    if nums.is_empty() {
                        errors.push(
                            Diagnostic::error(
                                "E_UNKNOWN_PIN",
                                format!(
                                    "{ctx}: `{comp}` (part `{}`) has no pin `{sel}`",
                                    components[&cid].part
                                ),
                                Location::Entity(cid.clone()),
                            )
                            .with_help(available_pins(def)),
                        );
                        continue;
                    }
                    for n in nums {
                        entry.members.insert(PinRef::new(&cid, &n));
                    }
                }
            }
            GenDirective::NoConnect { pins } => {
                for (comp, sel) in pins {
                    let cid = EntityId::new(comp.clone());
                    if note_missing(
                        &cid,
                        &components,
                        &mut reported_missing,
                        &mut errors,
                        "no-connect",
                    ) {
                        continue;
                    }
                    let def = &lib[&components[&cid].part];
                    let nums = def.resolve_selector(sel);
                    if nums.is_empty() {
                        errors.push(
                            Diagnostic::error(
                                "E_UNKNOWN_PIN",
                                format!(
                                    "no-connect: `{comp}` (part `{}`) has no pin `{sel}`",
                                    components[&cid].part
                                ),
                                Location::Entity(cid.clone()),
                            )
                            .with_help(available_pins(def)),
                        );
                        continue;
                    }
                    for n in nums {
                        no_connects.insert(PinRef::new(&cid, &n));
                    }
                }
            }
            _ => {}
        }
    }

    // Validate region declarations: a copper pour names the net it belongs to, and
    // that net must exist (be connected by some `net`/ConnectPins directive) — a
    // pour on a typo'd or never-connected net is a hard fault, never a silent dangle,
    // the same guarantee `ConnectPins`/`NoConnect` give for pins. Collected, not
    // aborting early.
    for d in source {
        if let GenDirective::Region(r) = d
            && r.role == Role::Conductor
        {
            match &r.net {
                Some(name) if !nets.contains_key(&NetId::new(name.clone())) => {
                    errors.push(
                        Diagnostic::error(
                            "E_UNKNOWN_NET",
                            format!(
                                "copper pour references net `{name}`, which no directive connects"
                            ),
                            Location::Net(NetId::new(name.clone())),
                        )
                        .with_help(
                            "connect that net (e.g. `net <name> ...`), or fix the pour's net name",
                        ),
                    );
                }
                None => errors.push(
                    Diagnostic::error(
                        "E_POUR_NO_NET",
                        "copper pour has no net; a conductor region must name the net it fills",
                        Location::None,
                    )
                    .with_help(
                        "add `net=<name>` to the region, or make it a keep-out/void instead",
                    ),
                ),
                _ => {}
            }
        }
    }

    // Validate slab-name targets (Decision 13): every region / text `layer` must name a
    // slab in the stackup, and a `Conductor` pour must target a copper slab. An unknown
    // name — or a net-bound pour on a non-copper slab (silk) — is a hard fault here (no
    // silent board-z/copper-z fallback), so a committed document always resolves cleanly
    // to the `Feature` model. Collected, not aborting early.
    let su = stackup(source);
    let unknown_slab = |name: &str| -> Diagnostic {
        let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
        Diagnostic::error(
            "E_UNKNOWN_SLAB",
            format!("layer `{name}` names no slab in the stackup"),
            Location::None,
        )
        .with_help(format!("available slabs: {}", names.join(", ")))
    };
    for d in source {
        match d {
            GenDirective::Region(r) => match su.slabs.iter().find(|s| s.name == r.layer) {
                None => errors.push(unknown_slab(&r.layer)),
                Some(slab) if r.role == Role::Conductor && slab.role != Role::Conductor => {
                    errors.push(
                        Diagnostic::error(
                            "E_POUR_NON_COPPER",
                            format!(
                                "copper pour on non-copper slab `{}` (its role is {:?})",
                                r.layer, slab.role
                            ),
                            Location::None,
                        )
                        .with_help(
                            "target a copper slab (e.g. F.Cu / B.Cu), or change the region role",
                        ),
                    );
                }
                _ => {}
            },
            GenDirective::Text { layer, .. } if su.slab_z(layer).is_none() => {
                errors.push(unknown_slab(layer));
            }
            _ => {}
        }
    }

    // Collect-all gate: if the model could not be built cleanly, abort the whole
    // transaction with every fault found. The partial model above is discarded.
    if !errors.is_empty() {
        return Err(errors);
    }

    // Pass 4: place everything with the least-change solver, then reconcile
    // overrides against the solved result.
    //
    // Precedence (via movability): Fix/Pin are immovable anchors; Hint is a
    // movable soft anchor; Free is anchored at the generated default. An override
    // is *ineffective* iff freeing it and re-solving lands it in the same place —
    // i.e. the solver/constraints would have put it there anyway. Ineffective
    // hints decay, ineffective pins are flagged, and a pin a hard Fix contradicts
    // raises a loud conflict.
    let base: BTreeMap<EntityId, Point> = components
        .iter()
        .map(|(k, c)| (k.clone(), c.pos.value))
        .collect();
    let no_suppress = BTreeSet::new();
    // We use only `.positions` here: reconciliation's least-change/decay logic is
    // defined purely by where the solver places nodes. The new `Solution` also
    // carries `converged`/`unsatisfied` (infeasibility), which the engine could
    // surface in a future milestone; today the placement is what reconciliation
    // consumes, so the semantics below are unchanged from the relaxation solver.
    let solved_all = solve(&assemble_problem(
        &base,
        &fixmap,
        overrides,
        board.as_ref(),
        &relational,
        &no_suppress,
    ))
    .positions;

    let mut report = ReconReport::default();
    let mut decayed: BTreeSet<EntityId> = BTreeSet::new();
    let mut prov_map: BTreeMap<EntityId, Provenance> = BTreeMap::new();
    // Decision 17: a doc-wide `font` that fails to load degrades to the stroke font — a
    // finding on a valid doc (a `W_FONT_LOAD` warning), never a fault.
    report.font_load_failure = font_load_failure(source);
    // Issue 0024: an outer copper side with no mask slab, while the stackup does carry a
    // mask — the forgot-one-side footgun. A degrade (a `W_COPPER_NO_MASK` warning), not a
    // fault; the side resolution reuses the same top_mask/bottom_mask query pad openings
    // use, so the lint agrees with what the mask export actually covers.
    report.unmasked_copper = stackup(source).unmasked_outer_copper();
    // DNP variant (Decision 21b): connections referencing an `if=false` depopulated
    // instance were skipped above; surface each dangling reference as a `W_DNP` warning
    // (deduped + sorted for a deterministic report).
    dnp_dangling.sort();
    dnp_dangling.dedup();
    report.dnp_dangling = dnp_dangling;

    for (id, ov) in overrides {
        if !base.contains_key(id) || ov.pos.is_none() {
            continue; // orphans handled below; empty overrides ignored
        }
        let fix = fixmap.get(id).copied();

        // A hard constraint outranks the override regardless of geometry.
        if let Some(fp) = fix {
            match ov.strength {
                Strength::Hint => {
                    report
                        .decayed
                        .push((id.clone(), DecayReason::OverriddenByConstraint));
                    decayed.insert(id.clone());
                }
                Strength::Pin => {
                    if ov.pos != Some(fp) {
                        report.pin_conflicts.push(id.clone());
                    } else {
                        report.redundant_pins.push(id.clone());
                    }
                }
            }
            continue;
        }

        // No hard constraint: is the override doing anything? Re-solve without it.
        let mut suppress = BTreeSet::new();
        suppress.insert(id.clone());
        let solved_wo = solve(&assemble_problem(
            &base,
            &fixmap,
            overrides,
            board.as_ref(),
            &relational,
            &suppress,
        ))
        .positions;
        let effective = dist(solved_all[id], solved_wo[id]) > PLACE_TOL as f64;

        match ov.strength {
            Strength::Hint => {
                if effective {
                    prov_map.insert(id.clone(), Provenance::Hint);
                } else {
                    report
                        .decayed
                        .push((id.clone(), DecayReason::RedundantWithDefault));
                    decayed.insert(id.clone());
                }
            }
            Strength::Pin => {
                if !effective {
                    report.redundant_pins.push(id.clone());
                }
                prov_map.insert(id.clone(), Provenance::Pinned); // pins are kept
            }
        }
    }

    // Final placement with decayed hints freed back to their defaults. This is
    // what a fresh elaboration (after GC) would produce, so the result is stable.
    let solved_final = solve(&assemble_problem(
        &base,
        &fixmap,
        overrides,
        board.as_ref(),
        &relational,
        &decayed,
    ))
    .positions;

    for (id, c) in components.iter_mut() {
        let prov = if fixmap.contains_key(id) {
            Provenance::Fixed
        } else {
            prov_map.get(id).copied().unwrap_or(Provenance::Free)
        };
        c.pos = Dof {
            value: solved_final[id],
            prov,
        };
    }

    // Orphaned overrides: target no longer exists. Surfaced, never dropped. Refdes
    // pins share the orphan channel (same "override targets a dead id" semantics);
    // dedupe so an entity with both a pos override and a refdes pin is flagged once.
    for id in overrides.keys() {
        if !components.contains_key(id) {
            report.orphaned.push(id.clone());
        }
    }
    for id in refdes_pins.keys() {
        if !components.contains_key(id) && !report.orphaned.contains(id) {
            report.orphaned.push(id.clone());
        }
    }

    // Colliding refdes pins (two entities pinned to one string): an authoring
    // conflict surfaced loudly, non-blocking like the pos findings above.
    report.refdes_pin_dups = crate::annotate::duplicate_refdes_pins(refdes_pins);

    // Honest verify (Decision 10's third leg / issue 0019). The solver now pushes the
    // *true* polygonal courtyards, but a converged placement can still leave a residual
    // overlap the push could not clear — two fixed/pinned parts placed into each other.
    // Re-check every NoOverlap pair against the real rounded polygons at the final
    // placement and report any that still overlap. Because the solver push consumes the
    // polygon itself (not the looser AABB proxy), this is the tighter truth — the check
    // deliberately *not* shipped pre-0019, when it could only ever false-positive.
    for c in &relational {
        if let Constraint::NoOverlap {
            a,
            a_poly,
            a_r,
            b,
            b_poly,
            b_r,
        } = c
        {
            let (Some(&pa), Some(&pb)) = (solved_final.get(a), solved_final.get(b)) else {
                continue;
            };
            let world = |poly: &[Point], o: Point| -> Vec<Point> {
                poly.iter()
                    .map(|p| Point {
                        x: p.x + o.x,
                        y: p.y + o.y,
                    })
                    .collect()
            };
            // Report only overlaps beyond the verify tolerance: a converged movable pair
            // carries at most the solver's ~µm residual (convergence slop), which is not
            // a collision. A genuine unresolvable overlap (two fixed parts pinned into
            // each other) penetrates by tens of µm or more. See [`COURTYARD_VERIFY_TOL`].
            let depth = courtyard_overlap_depth(&world(a_poly, pa), *a_r, &world(b_poly, pb), *b_r);
            if depth > COURTYARD_VERIFY_TOL as f64 {
                report.courtyard_overlaps.push((a.clone(), b.clone()));
            }
        }
    }

    Ok(Elaborated {
        components,
        nets,
        no_connects,
        report,
        dnp_dropped,
        def_fragments,
    })
}

/// Record (once) that a referenced entity does not exist, and report it as a
/// structural fault. Returns `true` if `id` is missing (so the caller skips it).
/// The `reported_missing` set is the cascade-suppression mechanism: an entity is
/// reported the *first* time it's found missing, and later references are silenced
/// so the genuine fault (its failed/absent instantiation) isn't buried under noise.
fn note_missing(
    id: &EntityId,
    components: &BTreeMap<EntityId, Component>,
    reported_missing: &mut BTreeSet<EntityId>,
    errors: &mut Vec<Diagnostic>,
    ctx: &str,
) -> bool {
    if components.contains_key(id) {
        return false;
    }
    if reported_missing.insert(id.clone()) {
        errors.push(Diagnostic::error(
            "E_UNKNOWN_INSTANCE",
            format!("{ctx} references unknown instance `{id}`"),
            Location::Entity(id.clone()),
        ));
    }
    true
}

/// A placed component's courtyard as a **rounded convex polygon** in its local frame,
/// already rotated by `orient` (not translated — the solver adds the node position each
/// sweep). Returns `(vertices, radius)`, the keep-out being `hull(vertices) ⊕
/// disc(radius)`, or `None` for a footprint-less part (no courtyard ⇒ exempt from
/// overlap-avoidance, exactly as before).
///
/// Prefers the real polygonal courtyard ([`courtyard_shape`] — the convex pad hull ⊕
/// margin): this is issue 0019's whole point. A *rotated* part reserves its rotated
/// hull, so neighbours nestle into concavities the axis-aligned box would over-reserve.
/// A part with copper but no 2-D hull (a lone round pad / collinear pads) has no polygon
/// courtyard; it falls back to the axis-aligned box proxy from [`courtyard_half_extents`]
/// (via [`oriented_courtyard`]), lowered as a 4-vertex radius-0 polygon so the identical
/// SAT path serves it and its behaviour is unchanged from the pre-0019 AABB push.
///
/// The SAT push treats the courtyard as convex, so we make that real here rather than
/// assuming it: the courtyard skeleton is **flattened** (arcs → chords within
/// [`DEFAULT_CHORD_TOL`], the same seam `bbox` uses) and run through [`convex_hull`].
/// This matters for an *imported* courtyard, which may be non-convex or have an
/// outward-bowing arc edge — walking corners alone ([`Shape2D::points`]) would drop the
/// arc bulge and under-cover it (the one true under-report path). Hulling the flattened
/// skeleton is arc-safe (the bulge's subdivided points are inside the hull) and
/// idempotent on the already-convex derived pad hull.
fn component_courtyard(def: &PartDef, orient: Orient) -> Option<(Vec<Point>, Nm)> {
    if let Some(shape) = courtyard_shape(def) {
        let hull = convex_hull(&shape.path().flatten(DEFAULT_CHORD_TOL));
        if hull.len() >= 3 {
            let verts = hull.into_iter().map(|p| orient.apply(p)).collect();
            return Some((verts, shape.radius()));
        }
        // A degenerate imported courtyard (collinear / <3 distinct points) has no 2-D
        // hull; fall through to the axis-aligned box proxy below.
    }
    let (hw, hh) = oriented_courtyard(def, orient);
    if (hw, hh) == (0, 0) {
        return None;
    }
    Some((
        vec![
            Point { x: hw, y: hh },
            Point { x: -hw, y: hh },
            Point { x: -hw, y: -hh },
            Point { x: hw, y: -hh },
        ],
        0,
    ))
}

/// A part's courtyard half-extents oriented for a placed component. The courtyard is
/// the axis-aligned box `±hw × ±hh`; under the orientation its AABB half-extents are
/// the summed absolute contributions of each rotated axis (so a cardinal 90°/270° turn
/// swaps w/h exactly, and any orientation is handled). Routes through
/// [`Orient::apply`], so it stays exact for cardinals.
fn oriented_courtyard(def: &PartDef, orient: Orient) -> (Nm, Nm) {
    let (hw, hh) = courtyard_half_extents(def);
    let ax = orient.apply(Point { x: hw, y: 0 });
    let ay = orient.apply(Point { x: 0, y: hh });
    (ax.x.abs() + ay.x.abs(), ax.y.abs() + ay.y.abs())
}

/// A `help:` line listing a part's distinct functional pin names — the candidates
/// for an unresolved selector (the "did you mean" surface; fuzzy matching later).
fn available_pins(def: &PartDef) -> String {
    let mut names: Vec<&str> = def.pins.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    format!("available pins: {}", names.join(", "))
}

/// A `help:` line listing the known part names — candidates for an unknown part.
fn known_parts(lib: &PartLib) -> String {
    let names: Vec<&str> = lib.keys().map(String::as_str).collect();
    format!("known parts: {}", names.join(", "))
}

/// Build a solver problem from base placements + overrides + constraints.
/// `suppress` lists override ids to ignore (treat the node as Free at its
/// default) — used to test whether an override is doing anything.
fn assemble_problem(
    base: &BTreeMap<EntityId, Point>,
    fixmap: &BTreeMap<EntityId, Point>,
    overrides: &BTreeMap<EntityId, Override>,
    board: Option<&Shape2D>,
    relational: &[Constraint],
    suppress: &BTreeSet<EntityId>,
) -> Problem {
    let mut anchors = BTreeMap::new();
    let mut fixed = BTreeSet::new();
    for (id, default) in base {
        if let Some(fp) = fixmap.get(id) {
            anchors.insert(id.clone(), *fp);
            fixed.insert(id.clone());
            continue;
        }
        let ov = if suppress.contains(id) {
            None
        } else {
            overrides.get(id)
        };
        match ov.and_then(|o| o.pos.map(|p| (p, o.strength))) {
            Some((p, Strength::Pin)) => {
                anchors.insert(id.clone(), p);
                fixed.insert(id.clone());
            }
            Some((p, Strength::Hint)) => {
                anchors.insert(id.clone(), p); // movable soft anchor
            }
            None => {
                anchors.insert(id.clone(), *default);
            }
        }
    }
    Problem {
        anchors,
        fixed,
        board: board.cloned(),
        constraints: relational.to_vec(),
    }
}

/// The board as a filled [`Region`](crate::region::Region): the last `Board` directive's
/// outline **minus** every `Cutout` (Decision 16c). `None` if there is no `Board` (the
/// solver then leaves placement unbounded). This is the single shared board-geometry
/// reader — elaboration (the substrate/mask `Area` features), the solver (containment),
/// the autorouter (grid bbox), and export (Edge.Cuts, SVG) all fold through it, so the
/// board's truth lives in one place instead of a bespoke `outline`/`cutouts` struct.
///
/// The outline and cutouts are polygonized here (the region kernel flattens arcs at
/// construction, Decision 16b): a curved board edge or round cutout becomes a fine
/// polyline. The authored arcs survive in the `Board`/`Cutout` directives; this derived
/// region does not carry them.
pub fn board_region(source: &Source) -> Option<crate::region::Region> {
    use crate::region::{DEFAULT_CIRCLE_SEGS, difference, shape_to_region, union_all};
    let outline = source.iter().rev().find_map(|d| match d {
        GenDirective::Board { outline } => Some(outline),
        _ => None,
    })?;
    let mut region = shape_to_region(outline, DEFAULT_CIRCLE_SEGS);
    let cutouts: Vec<crate::region::Region> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Cutout { shape } => Some(shape_to_region(shape, DEFAULT_CIRCLE_SEGS)),
            _ => None,
        })
        .collect();
    if !cutouts.is_empty() {
        region = difference(&region, &union_all(cutouts));
    }
    Some(region)
}

/// Assemble every authored [`RegionDecl`] from the source, in declaration order. The
/// single shared reader for pours / keep-outs / filled voids — the derived fill query
/// (0004 stage 3), DRC, and export all call this, exactly as [`board_region`] is the
/// shared reader for the outline.
pub fn regions(source: &Source) -> Vec<RegionDecl> {
    source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Region(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

/// The board [`Stackup`] for a source — the single shared reader that every consumer
/// lowering an abstract layer to a real `ZRange` must go through (sibling to
/// [`board_region`] / [`regions`]).
///
/// Collects every [`Slab`](GenDirective::Slab) directive, in **declaration order**, into
/// `Stackup { slabs }` — exactly as [`regions`] collects [`RegionDecl`]s. Declaration
/// order is preserved (not sorted): [`Stackup`]'s own accessors order by z where they
/// need to ([`Stackup::copper_slabs`] sorts by z, [`Stackup::board_z`] takes min/max,
/// [`Stackup::slab_z`] looks up by name), so order is functionally irrelevant — and
/// preserving it keeps `parse(serialize(doc)) == doc` trivially. No overlap/gap
/// validation is performed here (`ZRange::new` already normalises `lo ≤ hi`); a future
/// validation pass can layer on top without changing this reader's contract.
///
/// If the source authors **no** slabs, falls back to [`Stackup::default_2layer`] — the
/// unchanged familiar 2-layer default, so existing sources behave exactly as before.
pub fn stackup(source: &Source) -> Stackup {
    let slabs: Vec<Slab> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Slab(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    if slabs.is_empty() {
        Stackup::default_2layer()
    } else {
        Stackup { slabs }
    }
}

/// Lower the authored board/region geometry of a `Source` into the converged
/// [`NetFeature`] model — a [`Feature`] (pure physical geometry) paired with the
/// optional net it carries. This is the additive producer the convergence's Phase 2
/// will wire DRC/export onto; for now it has no callers besides tests. It is the
/// role-filtered union of what [`board_region`] and [`regions`] read today
/// (Decision 12.4), kept as one derived view, threading z through [`stackup`].
///
/// Emitted per directive (net stays an *annotation* alongside the feature, never a
/// field on `Feature` — connectivity is authoritative, Decision 12.1):
///   - the **last** `Board` directive minus every `Cutout` → one [`Role::Substrate`]
///     netless feature carrying a [`Shape2D::Area`] (the [`board_region`], Decision 16c).
///     Cutouts are holes in that Area, not separate `Void` features (Decision 16b).
///     (Unioning several `Board` directives into one multi-substrate body is deferred.)
///   - every `Region` → a feature carrying the authored role + net, at its slab's z
///     (mirrors [`regions`]).
///
/// This is the single **materialization gate** that resolves slab names against the
/// [`Stackup`] (Decision 13), so it is **fallible**: an unknown slab name — on a region
/// or a text label — is a hard error, and a `Conductor` region whose slab is not a
/// copper slab (a net-bound pour on silk) is likewise rejected here.
pub fn features(source: &Source) -> Result<Vec<crate::geom::NetFeature>, String> {
    let su = stackup(source);
    // The physical board *body* extent (the Substrate solid spans it). An empty stackup
    // has no extent — fall back to a zero range so the feature is still emitted.
    let board_z = su.board_z().unwrap_or(ZRange::new(0, 0));

    let mut out: Vec<NetFeature> = Vec::new();

    // Board: the single `Role::Substrate` feature is the board region — the last
    // `Board`'s outline minus every `Cutout` — carried as a `Shape2D::Area` (Decision
    // 16c). Board-level cutouts are *holes* in this Area (routed contours, Decision 16b),
    // not separate `Void` features. The same region (holes included) is the mask area.
    if let Some(region) = board_region(source) {
        let area = Shape2D::Area { region };
        out.push(NetFeature::netless(Feature::prism(
            Role::Substrate,
            area.clone(),
            board_z,
        )));

        // Solder mask: one board-area solid per `Role::Mask` slab in the stackup, at the
        // slab's honest z, carrying the slab's material (Decision 13 — mask is a positive
        // generated solid, and its openings are `Void` deletion volumes; there are no
        // negative layers). The mask area is the *same* board region **including the
        // cutout holes**, so a cutout reads through the mask (its opening) exactly as
        // before — now via the Area's holes rather than a separate cutout Void.
        for slab in su.slabs.iter().filter(|s| s.role == Role::Mask) {
            let mut mask = Feature::prism(Role::Mask, area.clone(), slab.z);
            mask.material = slab.material.clone();
            out.push(NetFeature::netless(mask));
        }
    }

    // Regions: every one, carrying the authored role + net (mirrors `regions`). The
    // slab name resolves to z; an unknown name is a hard error, and a `Conductor`
    // region on a non-copper slab (a net-bound pour on silk) is nonsense.
    for d in source {
        if let GenDirective::Region(RegionDecl {
            shape,
            role,
            net,
            layer,
        }) = d
        {
            let slab = su.slabs.iter().find(|s| &s.name == layer).ok_or_else(|| {
                let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
                format!("unknown slab `{layer}` (available: {})", names.join(", "))
            })?;
            if *role == Role::Conductor && slab.role != Role::Conductor {
                return Err(format!(
                    "Conductor region on non-copper slab `{layer}` (its role is {:?}) \
                     — a net-bound pour must target a copper slab",
                    slab.role
                ));
            }
            // A `Conductor` region is a **copper pour**: its materialised feature is a
            // *filled* `Shape2D::Area` (outline ∖ foreign-copper knockouts), which needs
            // the placed copper to derive — so it is lowered by the unified world-frame
            // producer ([`crate::route::world_features`]), not here. This source-only
            // query still validates the pour's slab above (the materialization gate,
            // Decision 13); it just does not emit the raw outline as geometry.
            if *role == Role::Conductor {
                continue;
            }
            let net_opt = net.as_ref().map(|n| NetId::new(n.clone()));
            out.push(NetFeature::new(
                net_opt,
                Feature::prism(role.clone(), shape.clone(), slab.z),
            ));
        }
    }

    // Holes: every authored NPTH `hole` lowers to a full-stackup `Role::Void` disc with
    // **no material** (Decision 16b — a mounting hole is an authored non-plated `Void`).
    // Full-z so `excellon_drill`'s through-cut query picks it up; material-less so its
    // plating classification is NPTH. The `Some(full)` guard matches the via-drill
    // sibling above: `full_z()` is `None` only for a slab-less stackup, which `stackup()`
    // never yields (it falls back to `default_2layer`), so the drop is unreachable via the
    // normal reader — a hole with no board to drill through contributes no geometry.
    if let Some(full) = su.full_z() {
        for d in source {
            if let GenDirective::Hole { center, dia } = d {
                out.push(NetFeature::netless(Feature::prism(
                    Role::Void,
                    Shape2D::disc(*center, dia / 2),
                    full,
                )));
            }
        }
    }

    // Text: every authored string lowers to `Marking` features (Decision 9). The
    // geometry is derived here, never stored, so a renamed label re-derives. An
    // outline `font` directive (Decision 17), if present and loadable, swaps the stroke
    // font for filled glyph outlines; otherwise the built-in stroke font is used.
    let font = resolve_font(source);
    for d in source {
        if let GenDirective::Text {
            string,
            at,
            height,
            layer,
            orient,
        } = d
        {
            out.extend(text_features(
                string,
                *at,
                *height,
                layer,
                *orient,
                &su,
                font.as_ref(),
            )?);
        }
    }

    Ok(out)
}

/// The doc-wide outline font (Decision 17): the **last** [`GenDirective::Font`]'s file
/// parsed as a [`TtfFont`](crate::font::TtfFont), or `None` when there is no directive
/// **or the file fails to load**. Load failure degrades silently here (rendering must
/// never fail); [`font_diagnostics`] is the channel that surfaces the failure to the user.
pub fn resolve_font(source: &Source) -> Option<crate::font::TtfFont> {
    let path = source.iter().rev().find_map(|d| match d {
        GenDirective::Font { path } => Some(path),
        _ => None,
    })?;
    crate::font::TtfFont::from_path(std::path::Path::new(path)).ok()
}

/// The doc-wide [`GenDirective::Font`] failure, if any: `(path, reason)` when the last
/// `Font` directive's file cannot be read or parsed. `None` when there is no directive or
/// it loads cleanly. Distinct from [`resolve_font`] because feature lowering has no
/// diagnostic channel and must never fail; this feeds the [`ReconReport`]'s
/// `font_load_failure` field, which the `Diagnose` impl renders as a `W_FONT_LOAD`
/// warning — the path that surfaces a silently-ignored directive to the user.
pub fn font_load_failure(source: &Source) -> Option<(String, String)> {
    let path = source.iter().rev().find_map(|d| match d {
        GenDirective::Font { path } => Some(path),
        _ => None,
    })?;
    match crate::font::TtfFont::from_path(std::path::Path::new(path)) {
        Ok(_) => None,
        Err(reason) => Some((path.clone(), reason)),
    }
}

/// Lower one authored [`GenDirective::Text`] into stroke-font features on its named slab
/// (Decision 9). The shared [`crate::font::text_strokes`] produces the glyph centreline
/// polylines in a local frame (left-origin — board text's authored `at` *is* the origin,
/// so it stays [`Justify::Left`](crate::font::Justify::Left)); each is then rotated by
/// `orient` about that origin (exact for [`Orient::IDENTITY`]), translated to `at`, and
/// traced at a pen width of `height / 8` on the named slab's z (an unknown name is a hard
/// error). The feature's [`Role`] is **forward-queried from the resolved slab** — silk
/// slabs are [`Role::Marking`], a fab slab is [`Role::Datum`] (Decision 15) — exactly as
/// [`crate::part::graphic_features`] takes a graphic's role from its slab, rather than
/// hardcoding `Marking` (which silently shipped fab-slab text onto silk). The features are
/// **netless** — marking/fab surface geometry carries no electrical identity.
fn text_features(
    string: &str,
    at: Point,
    height: Nm,
    layer: &str,
    orient: Orient,
    su: &Stackup,
    font: Option<&crate::font::TtfFont>,
) -> Result<Vec<NetFeature>, String> {
    let slab = su.slab(layer).ok_or_else(|| {
        let names: Vec<&str> = su.slabs.iter().map(|s| s.name.as_str()).collect();
        format!("unknown slab `{layer}` (available: {})", names.join(", "))
    })?;
    let (role, z) = (slab.role.clone(), slab.z);
    // rotate about the text origin, then place at `at`.
    let place = |local: Point| {
        let r = orient.apply(local);
        Point {
            x: r.x + at.x,
            y: r.y + at.y,
        }
    };
    let mut out = Vec::new();
    if let Some(font) = font {
        // Outline font: each glyph is a filled `Area` already — place it (no pen trace).
        for shape in crate::font::text_regions(string, height, crate::font::Justify::Left, font) {
            out.push(NetFeature::netless(Feature::prism(
                role.clone(),
                shape.map_points(place),
                z,
            )));
        }
    } else {
        // Stroke font: trace each centreline polyline at a visible pen width.
        let pen = (height / 8).max(1);
        for stroke in crate::font::text_strokes(string, height, crate::font::Justify::Left) {
            let pts: Vec<Point> = stroke.into_iter().map(place).collect();
            out.push(NetFeature::netless(Feature::prism(
                role.clone(),
                Shape2D::trace(pts, pen),
                z,
            )));
        }
    }
    Ok(out)
}

// `board_rect` (the pure `GenDirective` builder that used to live here) now lives
// in `crate::ir` and is re-exported via the glob at the top of this module.

/// Connect two interface ports using the interface type's mate map. The mate map
/// is the single place the tx<->rx crossing is defined, so connecting two ports
/// always produces correctly-crossed nets — the swap footgun is unrepresentable.
///
/// Both components are assumed present (the caller cascade-checks them); any port /
/// type / drive fault is pushed onto `errors` (the transaction aborts on it), and a
/// fault that prevents wiring returns early without producing partial nets.
fn connect_interface(
    components: &BTreeMap<EntityId, Component>,
    lib: &PartLib,
    a: &(String, String),
    b: &(String, String),
    nets: &mut BTreeMap<NetId, Net>,
    errors: &mut Vec<Diagnostic>,
) {
    let (ap, aport) = a;
    let (bp, bport) = b;
    let aid = EntityId::new(ap.clone());
    let bid = EntityId::new(bp.clone());
    let ac = &components[&aid];
    let bc = &components[&bid];
    let adef = &lib[&ac.part];
    let bdef = &lib[&bc.part];
    let (Some(aiface), Some(biface)) = (adef.interfaces.get(aport), bdef.interfaces.get(bport))
    else {
        if !adef.interfaces.contains_key(aport) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_INTERFACE",
                format!(
                    "`{ap}` (part `{}`) has no interface port `{aport}`",
                    ac.part
                ),
                Location::Entity(aid),
            ));
        }
        if !bdef.interfaces.contains_key(bport) {
            errors.push(Diagnostic::error(
                "E_UNKNOWN_INTERFACE",
                format!(
                    "`{bp}` (part `{}`) has no interface port `{bport}`",
                    bc.part
                ),
                Location::Entity(bid),
            ));
        }
        return;
    };
    if aiface.type_name != biface.type_name {
        errors.push(Diagnostic::error(
            "E_INTERFACE_MISMATCH",
            format!(
                "interface type mismatch: {} vs {}",
                aiface.type_name, biface.type_name
            ),
            Location::Entity(aid),
        ));
        return;
    }

    for (sa, sb) in &aiface.mate {
        let da = aiface.signals.get(sa).copied();
        let db = biface.signals.get(sb).copied();
        let (Some(da), Some(db)) = (da, db) else {
            errors.push(Diagnostic::error(
                "E_INTERFACE_SIGNAL",
                format!(
                    "interface `{}` mate references a missing signal",
                    aiface.type_name
                ),
                Location::Entity(aid.clone()),
            ));
            continue;
        };
        // Direction sanity: a mated pair must be drive/receive, not both drivers.
        if matches!((da, db), (Dir::Out, Dir::Out)) {
            errors.push(Diagnostic::error(
                "E_DRIVE_CONFLICT",
                format!("drive conflict mating {sa}<->{sb}"),
                Location::Entity(aid.clone()),
            ));
            continue;
        }
        let net_name = format!("{ap}.{aport}.{sa}");
        let nid = NetId::new(net_name.clone());
        let net = nets.entry(nid.clone()).or_insert_with(|| Net {
            id: nid,
            name: net_name,
            members: BTreeSet::new(),
        });
        // Unify pin identity: a signal bound to a real pad (an imported part —
        // `InterfaceDef.pads`) nets under the *pad-number* PinRef, the same identity
        // the discrete pin and the floating-pad check use. Only an abstract interface
        // (no pad binding — the toy library) falls back to the `port.signal` identity,
        // which is safe there precisely because it has no underlying pad to collide
        // with. Without this, a pad wired only via its interface looks floating, and
        // discrete + interface wiring of one pad split across two net nodes.
        let a_pin = match aiface.pads.get(sa) {
            Some(num) => num.clone(),
            None => format!("{aport}.{sa}"),
        };
        let b_pin = match biface.pads.get(sb) {
            Some(num) => num.clone(),
            None => format!("{bport}.{sb}"),
        };
        net.members.insert(PinRef::new(&aid, &a_pin));
        net.members.insert(PinRef::new(&bid, &b_pin));
    }
}

// ---- source-building helpers (a stand-in for the textual generative layer) ----

/// Build the demo power-supply module with `n` decoupling caps fanned off the
/// regulator output. This is the "generator" whose output we later override and
/// re-elaborate to test minimal-perturbation reconciliation.
pub fn psu_module(n: usize) -> Source {
    let mut s = vec![GenDirective::Instance {
        path: "psu.reg".into(),
        part: "LDO".into(),
        params: std::collections::BTreeMap::new(),
        label: None,
    }];
    for i in 0..n {
        let dec = format!("psu.dec[{i}]");
        s.push(GenDirective::Instance {
            path: dec.clone(),
            part: "Cap".into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        s.push(GenDirective::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("psu.reg".into(), "VOUT".into()),
                (dec.clone(), "p1".into()),
            ],
        });
        s.push(GenDirective::ConnectPins {
            net: "GND".into(),
            pins: vec![("psu.reg".into(), "GND".into()), (dec, "p2".into())],
        });
    }
    s
}

/// Generate a **ring** of `count` instances of `part`, evenly spaced on a circle of
/// `radius` about `center`, each rotated to **face outward** (local +x points away
/// from the centre). Per instance `i` (path `{prefix}[i]`) it emits an `Instance`, a
/// `Place` at the ring position, and a `Rotate` to the outward orientation — all
/// concrete: the `cos`/`sin` runs **once here, at generation**, producing exact
/// integer positions + quaternions that elaboration never re-derives. The motivating
/// case: side-firing LEDs around a round board (the arbitrary-angle placement that
/// the cardinal-only `Orient` could not express).
pub fn ring(prefix: &str, part: &str, center: Point, radius: Nm, count: usize) -> Source {
    let mut s = Vec::new();
    for i in 0..count {
        let path = format!("{prefix}[{i}]");
        let deg = 360.0 * i as f64 / count as f64;
        let rad = deg.to_radians();
        let pos = Point {
            x: center.x + (radius as f64 * rad.cos()).round() as Nm,
            y: center.y + (radius as f64 * rad.sin()).round() as Nm,
        };
        s.push(GenDirective::Instance {
            path: path.clone(),
            part: part.to_string(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
        s.push(GenDirective::Place {
            path: path.clone(),
            pos,
        });
        s.push(GenDirective::Rotate {
            path,
            orient: Orient::from_angle_deg(deg),
        });
    }
    s
}

#[cfg(test)]
mod tests;
