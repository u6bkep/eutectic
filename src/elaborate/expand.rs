//! The generative expansion engine (Decision 21b): lower `param`/def/ranged/
//! conditional/expression `inst` directives into the plain declarative `Source` the
//! elaboration passes understand. This is the only consumer of [`crate::elaborate::expr`].

use super::expr;
use crate::diagnostic::{Diagnostic, Location};
use crate::id::NetId;
use crate::ir::{DefNode, GenDirective, MAX_RANGE_INSTANCES, Source};
use crate::part::PartLib;
use std::collections::{BTreeMap, BTreeSet};

/// Lower every generative directive (`param`, ranged/conditional/expression `inst`) into
/// the plain declarative `Source` the elaboration passes already understand (Decision
/// 21b). Runs once, before Pass 1, so the reconciliation machinery sees only concrete
/// `Instance` directives at concrete `path[i]` paths — an override or refdes pin attaches
/// to an expanded instance exactly as it would to a hand-written `inst path[i]`.
///
/// Steps:
///   1. resolve all `param` declarations into an [`Env`](super::expr::Env) (cycle-safe);
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
pub(super) fn expand_generative(
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
pub const MAX_DEF_DEPTH: usize = 64;

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
    env: &super::expr::Env,
    prefix: &str,
    chain: &mut Vec<String>,
    ctx: &mut ExpandCtx,
) {
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
    outer_env: &super::expr::Env,
    chain: &mut Vec<String>,
    ctx: &mut ExpandCtx,
) {
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
    env: &super::expr::Env,
    errors: &mut Vec<Diagnostic>,
) -> Option<Vec<Option<i64>>> {
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
fn bind_index(env: &super::expr::Env, idx: Option<i64>) -> super::expr::Env {
    let mut scope = env.clone();
    if let Some(i) = idx {
        scope.insert("i".to_string(), super::expr::Value::Int(i));
    }
    scope
}

/// Evaluate an `inst`'s expression params into display-normal strings, merged over its
/// verbatim ones. Returns `None` (pushing diagnostics) if any expression faults.
fn eval_params(
    ipath: &str,
    params: &BTreeMap<String, String>,
    param_exprs: &BTreeMap<String, String>,
    scope: &super::expr::Env,
    errors: &mut Vec<Diagnostic>,
) -> Option<BTreeMap<String, String>> {
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

/// Format an evaluated [`Value`](super::expr::Value) into the display-normal string a
/// component param stores (Decision 14 — params are authored strings at rest). An
/// integer prints plainly; a quantity prints its minimal decimal via `format_si` with no
/// unit (the unit spelling is a display concern owned by the class template downstream);
/// a boolean prints `true`/`false`.
fn format_value(v: super::expr::Value) -> String {
    use super::expr::Value;
    match v {
        Value::Int(n) => n.to_string(),
        Value::Quantity(q) => q.format_si(""),
        Value::Bool(b) => b.to_string(),
    }
}
