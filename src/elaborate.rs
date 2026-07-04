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

/// An authored **filled region**: a `Shape2D` area carrying a [`Role`] — a copper
/// pour (`Conductor`, with the `net` it belongs to and the `layer` slab it fills),
/// a keep-out (`Keepout`), or a filled void (`Void`). This is the *authoritative
/// declaration* (tier-1, in the generative `Source`); the actual knockout fill
/// (`region − foreign_copper ⊕ clearance`) is **derived** later (0004 stage 3), so it
/// is never stored and never goes stale. The shape is in absolute board coordinates
/// (like the board outline), not a footprint-local transform.
///
/// `layer` is a **slab name** (Decision 13) — an arbitrary token resolved against the
/// [`Stackup`] at elaboration (`F.Cu`, `B.Cu`, `F.SilkS`, or any authored slab); an
/// unknown name is a hard error, and a `Conductor` region whose slab is not copper is
/// nonsense (rejected by [`features`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionDecl {
    pub shape: Shape2D,
    pub role: Role,
    pub net: Option<String>,
    pub layer: String,
}

/// A directive in the generative program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenDirective {
    /// Instantiate `part` at hierarchical `path`. Optionally carries identity
    /// `params` (authored display-normal strings — copied verbatim onto the
    /// [`Component`]) and a display `label` template override (Decision 14). Both
    /// default empty/None for the common case (an IC identified by part name alone).
    Instance {
        path: String,
        part: String,
        params: BTreeMap<String, String>,
        label: Option<String>,
    },
    /// A declared **parameter** (Decision 21b): `param <name> = <expr>`. A named value in
    /// the hermetic expression tier — an integer, a decimal-exact SI quantity, or a
    /// boolean — usable by later expressions (`p:` values, range bounds, `if=`). Params
    /// are declarations, not variables: order-independent (they may reference each other),
    /// resolved once per elaboration into an [`Env`](crate::expr::Env) with cycle
    /// detection. The authored `expr` text is stored verbatim and round-trips as written
    /// (it *is* the generative program, Decision 21). Not a placement/connectivity
    /// directive — elaboration resolves it into the param environment before Pass 1;
    /// the placement/connectivity passes ignore it.
    Param {
        name: String,
        /// The authored expression text (e.g. `"3"`, `"n + 1"`, `"4.7k"`). Parsed and
        /// evaluated by [`crate::expr`]; never pre-evaluated (serializes as authored).
        expr: String,
    },
    /// A **generative instance** (Decision 21b) — the expression-bearing form of `inst`
    /// that lowers, during elaboration, into one or more plain [`Instance`](Self::Instance)
    /// directives. Kept a distinct variant so the common `inst` (plain verbatim params,
    /// no range/conditional) stays exactly [`Instance`](Self::Instance) — existing
    /// documents and their construction sites are wholly untouched (the diff is additive).
    ///
    /// It carries the three v1 consumers:
    ///   - **Range instantiation**: `range = Some((lo, hi))` expands `path[i]` for `i` in
    ///     `lo..hi` (**hi exclusive**, the Rust/`psu_module` idiom), binding the implicit
    ///     loop variable `i` in this instance's own param/`if` expressions. Bounds are
    ///     expression texts; the expanded count is capped (anti-footgun).
    ///   - **Expression params**: `param_exprs` are `p:<key>=(<expr>)` values evaluated
    ///     and formatted into the plain instance's verbatim `params` (display-normal
    ///     spelling). `params` here are the *verbatim* `p:` values (non-expression),
    ///     copied through unchanged.
    ///   - **Population conditional**: `if_expr = Some(<expr>)` — a boolean that, when
    ///     false, drops the instance (and, with a `W_DNP` warning, silently skips any
    ///     connection referencing the dropped path — DNP variants).
    ///
    /// Lowered by [`expand_generative`] into concrete `Instance` directives *before* the
    /// reconciliation passes, so overrides/refdes address the expanded `path[i]` by path
    /// exactly as they address a hand-written `inst path[i]`.
    InstGenerative {
        /// The instance path *without* any range suffix (`sense`, not `sense[0..n]`); the
        /// `[i]` index is appended per expansion when `range` is `Some`.
        path: String,
        part: String,
        /// Verbatim (non-expression) `p:` values, copied through unchanged.
        params: BTreeMap<String, String>,
        /// Expression `p:` values (`p:count=(i+1)`), evaluated + formatted per instance.
        param_exprs: BTreeMap<String, String>,
        label: Option<String>,
        /// `(lo, hi)` expression texts for `path[lo..hi]` range expansion (hi exclusive).
        range: Option<(String, String)>,
        /// A population conditional expression; false ⇒ the instance is not elaborated.
        if_expr: Option<String>,
    },
    /// Source-provided default placement (a *free* DOF unless overridden).
    Place {
        path: String,
        pos: Point,
    },
    /// A hard placement constraint (e.g. a connector mated to a mechanical
    /// datum). Outranks user overrides; surfaces conflicts rather than yielding.
    Fix {
        path: String,
        pos: Point,
    },
    /// Board outline (a [`Shape2D`] — rounded/concave/CAD-imported all expressible);
    /// movable components are kept inside it. Use [`board_rect`] for the common
    /// rectangle. The last `Board` in the source wins.
    Board {
        outline: Shape2D,
    },
    /// An interior board cutout / void ([`Shape2D`]); components are kept out of it.
    Cutout {
        shape: Shape2D,
    },
    /// An authored **non-plated through-hole** (NPTH) — a mounting hole, tooling hole,
    /// etc. (Decision 16b: "a mounting hole is an authored NPTH `Void`, not a board
    /// cutout"). Lowers to a full-stackup [`Role::Void`] disc with **no material**, so
    /// [`crate::export::excellon_drill`] classifies it into `board-NPTH.drl`. Distinct
    /// from a [`Cutout`](Self::Cutout) (a milled contour that reaches Edge.Cuts + the
    /// mask via the substrate `Area`'s holes) and from a `region void` (single-slab,
    /// never a through-cut). The [`center`]/`dia` are the drilled hole; the drill file
    /// gets exact center + diameter. NOTE (round-2 finding): the void does **not** yet
    /// knock out the solder mask or an overlapping copper pour, nor does DRC flag copper
    /// intruding on it — those are unenforced for authored `Role::Void`s (see the
    /// findings ledger). It reaches the NPTH drill file, which is the one machinery that
    /// already forward-queries `Role::Void` through-cuts.
    Hole {
        center: Point,
        dia: Nm,
    },
    /// An authored filled region — a copper pour, keep-out, or filled void. See
    /// [`RegionDecl`]. Read by [`regions`]; the knockout fill is derived downstream.
    Region(RegionDecl),
    /// One authored board-stackup [`Slab`] (a named z-slab with a role + optional
    /// material). Accumulated by [`stackup`] into the board [`Stackup`], mirroring how
    /// [`Region`](Self::Region) directives are collected by [`regions`]. This is *not* a
    /// placement/connectivity directive — elaboration's passes ignore it; it is read
    /// only by [`stackup`].
    Slab(Slab),
    /// One authored **class-registry** entry (Decision 14): the conventions —
    /// refdes `prefix`, label `template`, class-default params — for a component
    /// `class`. Accumulated by [`registry`](crate::annotate::registry) over the built-in
    /// seeds, mirroring how [`Slab`](Self::Slab) directives are collected by
    /// [`stackup`]. A display/identity directive — elaboration's placement/connectivity
    /// passes ignore it.
    Class {
        name: String,
        entry: crate::annotate::ClassEntry,
    },
    /// Relational placement constraint solved by the least-change solver.
    Near {
        a: String,
        b: String,
        within: Nm,
    },
    MinSep {
        a: String,
        b: String,
        gap: Nm,
    },
    AlignX {
        nodes: Vec<String>,
    },
    AlignY {
        nodes: Vec<String>,
    },
    /// Connect two interface ports. The crossing is determined by the interface
    /// type's mate map, so it cannot be wired backwards.
    ConnectInterface {
        a: (String, String), // (component path, port name)
        b: (String, String),
    },
    /// Connect discrete pins onto a named net. Each `(comp path, selector)` is
    /// resolved against the component's part: a functional name fans out to *every*
    /// pad with that name (so `IOVDD` connects all six pads), a pad number selects
    /// that one pad. An unresolvable selector aborts elaboration (no silent dangle).
    ConnectPins {
        net: String,
        pins: Vec<(String, String)>,
    }, // (comp path, selector)
    /// Mark pads as deliberately unconnected. Same `(comp path, selector)` shape as
    /// `ConnectPins`; the resolved pads are exempt from the floating-pad check.
    NoConnect {
        pins: Vec<(String, String)>,
    },
    /// Set a component's orientation to a quaternion [`Orient`] (planar rotation,
    /// optionally flipped to the board bottom — both baked into the quaternion at
    /// authoring time). A settable attribute, not a solver DOF. The text front-end
    /// lowers a `<deg> [bottom]` (any angle) or `quat=(w,x,y,z)` into this.
    Rotate {
        path: String,
        orient: Orient,
    },
    /// Like `Near`, but the target is a specific *pin* (`b_comp`.`b_pin`) rather
    /// than a component centroid. The pin's world position tracks its component's
    /// position + orientation during solving.
    NearPin {
        a: String,
        b_comp: String,
        b_pin: String,
        within: Nm,
    },
    /// Authored **board text** — a mutable string lowered to silkscreen (per
    /// Decision 9 in docs/geometry-model-convergence.md). The **authoritative** form
    /// is exactly these fields (string + placement + `height` + `layer` + `orient`);
    /// the `Shape2D` strokes are *derived* by [`features`] through the built-in
    /// stroke [`crate::font`] — never stored, so a rename re-derives. `orient`
    /// defaults to [`Orient::IDENTITY`] (rotated labels are a follow-up). This is
    /// **not** a placement/connectivity directive — elaboration's passes ignore it
    /// (the main matches have `_ => {}` arms); it is read only by the lowering.
    Text {
        string: String,
        at: Point,
        height: Nm,
        /// The **slab name** the silk lands on (Decision 13) — resolved against the
        /// [`Stackup`] at lowering; `F.SilkS` by default. An unknown name is a hard
        /// error (silk now lands at the silk slab's honest z, not copper z).
        layer: String,
        orient: Orient,
    },
    /// Doc-wide **outline-font** selection (Decision 17): a filesystem `path` to a
    /// TTF/OpenType file. When present (the last `Font` directive wins), board text and
    /// footprint labels lower through [`crate::font::text_regions`] (filled glyph
    /// outlines) instead of the built-in stroke font. A missing/unparseable file
    /// **degrades** to the stroke font (a `W_FONT_LOAD` diagnostic, never a hard error);
    /// with no directive the stroke font is the default. Not a placement/connectivity
    /// directive — elaboration's passes ignore it; it is read only by the lowering
    /// ([`resolve_font`]) and by [`elaborate`], which records any load failure on the
    /// [`ReconReport`] (a `W_FONT_LOAD` warning) via [`font_load_failure`].
    Font {
        path: String,
    },
}

/// The generative program (tier 1 authoritative).
pub type Source = Vec<GenDirective>;

/// Every coordinate/length (nm) a directive contributes — the values an ingest
/// boundary range-checks against [`crate::geom::MAX_COORD`] (issue 0018). Points
/// contribute both components; scalar spans (widths/gaps/heights, slab z-faces) their
/// signed magnitude. Directives with no geometry (connectivity, alignment) contribute
/// nothing. `Rotate`'s quaternion is angle-scaled, not a coordinate, so it is
/// deliberately excluded. Shared by the text-parse and command-ingress validators so
/// both bound the same fields.
pub fn directive_coords(d: &GenDirective) -> Vec<Nm> {
    let shape_coords = |s: &Shape2D| s.coords().into_iter().flat_map(|p| [p.x, p.y]).collect();
    match d {
        GenDirective::Place { pos, .. } | GenDirective::Fix { pos, .. } => vec![pos.x, pos.y],
        GenDirective::Board { outline } => shape_coords(outline),
        GenDirective::Cutout { shape } | GenDirective::Region(RegionDecl { shape, .. }) => {
            shape_coords(shape)
        }
        GenDirective::Hole { center, dia } => vec![center.x, center.y, *dia],
        GenDirective::Text { at, height, .. } => vec![at.x, at.y, *height],
        GenDirective::Near { within, .. } | GenDirective::NearPin { within, .. } => vec![*within],
        GenDirective::MinSep { gap, .. } => vec![*gap],
        GenDirective::Slab(s) => vec![s.z.lo, s.z.hi],
        _ => vec![],
    }
}

/// The generous upper bound on a single range's expanded instance count (Decision 21b
/// anti-footgun). A negative or absurd bound is an `E_EXPR` fault rather than an attempt
/// to allocate billions of instances; 10k is far beyond any real board's part count on
/// one directive while still catching a runaway expression.
pub const MAX_RANGE_INSTANCES: i64 = 10_000;

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
fn expand_generative(source: &Source) -> Result<(Source, BTreeSet<String>), Vec<Diagnostic>> {
    use crate::expr;
    let mut errors: Vec<Diagnostic> = Vec::new();

    // Step 1: params → environment.
    let decls: BTreeMap<String, String> = source
        .iter()
        .filter_map(|d| match d {
            GenDirective::Param { name, expr } => Some((name.clone(), expr.clone())),
            _ => None,
        })
        .collect();
    // A duplicate `param` name is an authoring conflict (the last would silently win in
    // the map); surface it rather than dropping one.
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
            // Without an environment we cannot evaluate consumers; fail now with what we
            // have (param faults + the duplicate report above).
            return Err(errors);
        }
    };

    let mut out: Source = Vec::new();
    let mut dropped: BTreeSet<String> = BTreeSet::new();

    let eval_at = |text: &str, i: Option<i64>| -> Result<expr::Value, String> {
        match i {
            // Bind the implicit loop variable `i` for this evaluation (shadowing is not a
            // concern — params cannot be named `i` and also be a loop var in the same
            // scope; a param named `i` is simply visible unless a range provides one).
            Some(idx) => {
                let mut scope = env.clone();
                scope.insert("i".to_string(), expr::Value::Int(idx));
                expr::eval_str(text, &scope)
            }
            None => expr::eval_str(text, &env),
        }
    };

    for d in source {
        match d {
            GenDirective::Param { .. } => {} // resolved above; not a materialized directive
            GenDirective::InstGenerative {
                path,
                part,
                params,
                param_exprs,
                label,
                range,
                if_expr,
            } => {
                // Determine the index set: a range yields `lo..hi`, else a single unindexed
                // instance (index `None`).
                let indices: Vec<Option<i64>> = match range {
                    Some((lo_s, hi_s)) => {
                        let lo = eval_at(lo_s, None).and_then(|v| v.as_index());
                        let hi = eval_at(hi_s, None).and_then(|v| v.as_index());
                        match (lo, hi) {
                            (Ok(lo), Ok(hi)) => {
                                if lo < 0 || hi < 0 {
                                    errors.push(Diagnostic::error(
                                        "E_EXPR",
                                        format!("range `{path}[{lo}..{hi}]` has a negative bound"),
                                        Location::None,
                                    ));
                                    continue;
                                }
                                let count = (hi - lo).max(0);
                                if count > MAX_RANGE_INSTANCES {
                                    errors.push(Diagnostic::error(
                                        "E_EXPR",
                                        format!(
                                            "range `{path}[{lo}..{hi}]` expands to {count} \
                                             instances, over the {MAX_RANGE_INSTANCES} cap"
                                        ),
                                        Location::None,
                                    ));
                                    continue;
                                }
                                (lo..hi).map(Some).collect()
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
                                continue;
                            }
                        }
                    }
                    None => vec![None],
                };

                for idx in indices {
                    let ipath = match idx {
                        Some(i) => format!("{path}[{i}]"),
                        None => path.clone(),
                    };
                    // Conditional: a false `if=` depopulates this instance.
                    if let Some(cond) = if_expr {
                        match eval_at(cond, idx).and_then(|v| v.as_bool()) {
                            Ok(true) => {}
                            Ok(false) => {
                                dropped.insert(ipath);
                                continue;
                            }
                            Err(e) => {
                                errors.push(Diagnostic::error(
                                    "E_EXPR",
                                    format!("`if=` on `{ipath}`: {e}"),
                                    Location::None,
                                ));
                                continue;
                            }
                        }
                    }
                    // Evaluate expression params into display-normal strings, merged over
                    // the verbatim ones (an expression key never collides with a verbatim
                    // key — the parser routes each token to exactly one map).
                    let mut merged = params.clone();
                    let mut param_err = false;
                    for (k, ex) in param_exprs {
                        match eval_at(ex, idx) {
                            Ok(v) => {
                                merged.insert(k.clone(), format_value(v));
                            }
                            Err(e) => {
                                errors.push(Diagnostic::error(
                                    "E_EXPR",
                                    format!("param `p:{k}` on `{ipath}`: {e}"),
                                    Location::None,
                                ));
                                param_err = true;
                            }
                        }
                    }
                    if param_err {
                        continue;
                    }
                    out.push(GenDirective::Instance {
                        path: ipath,
                        part: part.clone(),
                        params: merged,
                        label: label.clone(),
                    });
                }
            }
            other => out.push(other.clone()),
        }
    }

    if errors.is_empty() {
        Ok((out, dropped))
    } else {
        Err(errors)
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
    let (expanded, dnp_dropped) = expand_generative(source)?;
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
    // not a fault: seed it into the cascade-suppression set so any reference to it skips
    // silently instead of raising `E_UNKNOWN_INSTANCE`. Connection references are
    // additionally surfaced as `W_DNP` warnings at their sites below (a placement
    // constraint on a depopulated part just skips). `dnp_dangling` collects those.
    let mut dnp_dangling: Vec<(String, String)> = Vec::new();
    for p in &dnp_dropped {
        reported_missing.insert(EntityId::new(p.clone()));
    }
    // Whether a `(comp, _)` reference points at a depopulated instance — records the
    // dangling connection for a `W_DNP` warning and returns true so the caller skips it.
    let note_dnp = |comp: &str, ctx: &str, sink: &mut Vec<(String, String)>| -> bool {
        if dnp_dropped.contains(comp) {
            sink.push((ctx.to_string(), comp.to_string()));
            true
        } else {
            false
        }
    };

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
                // A depopulated (`if=false`) endpoint makes the whole interface connect a
                // no-op — skip it with a `W_DNP` rather than an unknown-instance error.
                let ad = note_dnp(&a.0, "interface connect", &mut dnp_dangling);
                let bd = note_dnp(&b.0, "interface connect", &mut dnp_dangling);
                if ad || bd {
                    continue;
                }
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
                    let ctx = format!("net `{net}`");
                    if note_dnp(comp, &ctx, &mut dnp_dangling) {
                        continue;
                    }
                    let cid = EntityId::new(comp.clone());
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
                    if note_dnp(comp, "no-connect", &mut dnp_dangling) {
                        continue;
                    }
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

/// Build a rectangular [`Board`](GenDirective::Board) directive from opposite corners
/// — sugar over the polygon outline form for the common case.
pub fn board_rect(min: Point, max: Point) -> GenDirective {
    let c = Point {
        x: (min.x + max.x) / 2,
        y: (min.y + max.y) / 2,
    };
    GenDirective::Board {
        outline: Shape2D::rect(c, max.x - min.x, max.y - min.y),
    }
}

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
mod tests {
    use super::*;
    use crate::geom::Extent;

    fn pt(x: Nm, y: Nm) -> Point {
        Point { x, y }
    }

    #[test]
    fn ring_places_instances_around_a_circle_facing_outward() {
        // 12 side-firing LEDs on a 10 mm-radius ring — the arbitrary-angle case.
        let s = ring("led", "LED", pt(0, 0), 10_000_000, 12);
        assert_eq!(s.len(), 36, "12 × (Instance, Place, Rotate)");
        // Pull the (Place, Rotate) for a given index.
        let place_of = |i: usize| {
            s.iter().find_map(|d| match d {
                GenDirective::Place { path, pos } if path == &format!("led[{i}]") => Some(*pos),
                _ => None,
            })
        };
        let rot_of = |i: usize| {
            s.iter().find_map(|d| match d {
                GenDirective::Rotate { path, orient } if path == &format!("led[{i}]") => {
                    Some(*orient)
                }
                _ => None,
            })
        };
        // led[0] at angle 0 → east point, 0°. led[3] at 90° → north, ≈90°. led[6] →
        // west, ≈180°. All exactly on the ring (positions rounded to nm).
        assert_eq!(place_of(0).unwrap(), pt(10_000_000, 0));
        assert_eq!(rot_of(0).unwrap().to_deg(), 0);
        assert_eq!(place_of(3).unwrap(), pt(0, 10_000_000));
        assert_eq!(rot_of(3).unwrap().to_deg(), 90);
        assert_eq!(rot_of(6).unwrap().to_deg(), 180);
        // 30° (= 360/12) is off-axis: led[1] is a real quaternion, not a cardinal.
        assert_eq!(rot_of(1).unwrap().to_deg(), 30);
        assert!(!rot_of(1).unwrap().is_bottom());
    }

    /// Board + cutout + a Top conductor region: `features()` (the source-only geometry
    /// query) lowers exactly one Substrate (an `Area` whose cutout is a *hole*, not a
    /// separate Void — Decision 16b/c) and two mask solids. A **Conductor** region is a
    /// copper pour: its filled `Area` needs the placed copper to knock out, so it is
    /// lowered by [`crate::route::world_features`], not here — `features()` still
    /// *validates* the pour's slab (this call succeeds) but emits no conductor.
    #[test]
    fn features_lowers_board_cutout_and_region() {
        let su = Stackup::default_2layer();
        let src = vec![
            board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
            GenDirective::Cutout {
                shape: Shape2D::rect(pt(5 * MM, 5 * MM), MM, MM),
            },
            GenDirective::Region(RegionDecl {
                shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.Cu".into(),
            }),
        ];

        let feats = features(&src).unwrap();
        // one substrate + two mask solids (F/B.Mask in the default stackup). The cutout is
        // a hole in the substrate Area — no separate Void; the pour is lowered elsewhere.
        assert_eq!(
            feats.len(),
            3,
            "substrate + 2 masks (cutout is a hole; the pour lowers in world_features)"
        );

        let subs: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Substrate)
            .collect();
        assert_eq!(subs.len(), 1, "exactly one substrate feature");
        assert!(subs[0].net.is_none(), "substrate is netless");
        let Extent::Prism { shape, z } = &subs[0].feature.extent;
        assert_eq!(*z, su.board_z().unwrap(), "substrate spans the board body");
        // The substrate is an `Area` (outline ∖ cutout): its region has the outer ring
        // plus one hole (the cutout), and the cutout centre is outside the filled area.
        let region = shape.region().expect("substrate is a Shape2D::Area");
        assert_eq!(region.rings.len(), 2, "outer boundary + one cutout hole");
        assert!(region.contains_point(pt(MM, MM)), "board body is filled");
        assert!(
            !region.contains_point(pt(5 * MM, 5 * MM)),
            "the cutout is a hole, not filled"
        );

        assert!(
            !feats.iter().any(|f| f.feature.role == Role::Void),
            "a board cutout is a hole in the substrate Area, not a Void feature"
        );
        assert!(
            !feats.iter().any(|f| f.feature.role == Role::Conductor),
            "a Conductor pour is lowered by world_features, not the source-only features()"
        );
    }

    /// Every `Role::Mask` slab in the stackup yields exactly one solid mask `Feature`
    /// with the board-outline shape at that slab's z, carrying the slab's material
    /// (Decision 13 — mask is a generated positive solid, not a negative layer). The
    /// default stackup has two mask slabs (F/B.Mask), so a board generates two solids;
    /// a boardless source generates none (no board area to cover).
    #[test]
    fn features_generates_one_mask_solid_per_mask_slab() {
        let su = Stackup::default_2layer();
        let outline = Shape2D::rect(pt(0, 0), 8 * MM, 6 * MM);
        let src = vec![GenDirective::Board {
            outline: outline.clone(),
        }];

        let feats = features(&src).unwrap();
        let masks: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Mask)
            .collect();

        let mask_slabs: Vec<&Slab> = su.slabs.iter().filter(|s| s.role == Role::Mask).collect();
        assert_eq!(mask_slabs.len(), 2, "default stackup has F.Mask + B.Mask");
        assert_eq!(masks.len(), 2, "one mask solid per mask slab");
        assert!(masks.iter().all(|f| f.net.is_none()), "mask is netless");

        // Each solid is the board region (as an `Area`) at its slab's z, with the slab's
        // material. No cutouts here, so the region is just the outline.
        let expected = board_region(&src).unwrap();
        for slab in &mask_slabs {
            let m = masks
                .iter()
                .find(|f| matches!(f.feature.extent, Extent::Prism { z, .. } if z == slab.z))
                .unwrap_or_else(|| panic!("a mask solid at {:?}", slab.z));
            let Extent::Prism { shape, .. } = &m.feature.extent;
            assert_eq!(
                shape.region(),
                Some(&expected),
                "mask solid is the board region"
            );
            assert_eq!(
                m.feature.material, slab.material,
                "carries the slab material"
            );
        }

        // No `Board` ⇒ no board area ⇒ no mask solids.
        let boardless = features(&vec![]).unwrap();
        assert!(
            !boardless.iter().any(|f| f.feature.role == Role::Mask),
            "a boardless source generates no mask"
        );
    }

    /// A custom stackup with no `Role::Mask` slab generates no mask solids (no special
    /// cases — the generator simply finds nothing to emit).
    #[test]
    fn features_no_mask_slab_generates_no_mask() {
        // A minimal 1-copper-slab stackup: no mask, no silk.
        let src: Source = vec![GenDirective::Slab(Slab {
            name: "F.Cu".into(),
            z: ZRange::new(0, 35_000),
            role: Role::Conductor,
            material: Some(crate::geom::Material::named("copper")),
        })]
        .into_iter()
        .chain(std::iter::once(GenDirective::Board {
            outline: Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM),
        }))
        .collect();
        let feats = features(&src).unwrap();
        assert!(
            !feats.iter().any(|f| f.feature.role == Role::Mask),
            "no mask slab ⇒ no mask solid"
        );
    }

    /// Two `Board` directives: only the last outline becomes the substrate feature
    /// (mirrors `board_region`'s "last `Board` wins").
    #[test]
    fn features_last_board_wins() {
        let first = Shape2D::rect(pt(0, 0), 4 * MM, 4 * MM);
        let last = Shape2D::rect(pt(0, 0), 8 * MM, 8 * MM);
        let src = vec![
            GenDirective::Board {
                outline: first.clone(),
            },
            GenDirective::Board {
                outline: last.clone(),
            },
        ];

        let feats = features(&src).unwrap();
        let subs: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Substrate)
            .collect();
        assert_eq!(subs.len(), 1, "only one substrate emitted");
        let Extent::Prism { shape, .. } = &subs[0].feature.extent;
        // The substrate Area is the LAST board's region: it fills out to ±4 mm (the 8 mm
        // board) but the earlier 4 mm board's corner at (±2, ±2) is interior to it, and a
        // point past the 4 mm board (e.g. (3 mm, 0)) is still on the board — proving the
        // larger last outline won.
        let region = shape.region().expect("substrate is a Shape2D::Area");
        assert_eq!(*region, board_region(&src).unwrap());
        assert!(
            region.contains_point(pt(3 * MM, 0)),
            "the LAST (8 mm) board won"
        );
    }

    /// A `text` directive lowers to several `Role::Marking` stroke features sitting on
    /// the named silk slab's **honest z** (not copper z — Decision 13), advancing in +x
    /// across the string (Decision 9).
    #[test]
    fn features_lowers_text_to_marking_strokes() {
        let su = Stackup::default_2layer();
        let src = vec![GenDirective::Text {
            string: "R12".into(),
            at: pt(0, 0),
            height: MM,
            layer: "F.SilkS".into(),
            orient: Orient::IDENTITY,
        }];

        let feats = features(&src).unwrap();
        let marks: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Marking)
            .collect();
        // "R12": R(2) + 1(2) + 2(1) = 5 strokes; in any case several, all netless.
        assert!(
            marks.len() >= 3,
            "expected several marking strokes, got {}",
            marks.len()
        );
        assert!(marks.iter().all(|f| f.net.is_none()), "silk is netless");

        // All markings sit on the F.SilkS slab's honest z — above the top copper, not
        // aliased onto it (the pre-Decision-13 stopgap).
        let silk_z = su.slab_z("F.SilkS").unwrap();
        assert_ne!(
            silk_z,
            su.top_copper().unwrap(),
            "silk z is distinct from copper z"
        );
        for m in &marks {
            let Extent::Prism { z, .. } = m.feature.extent;
            assert_eq!(z, silk_z, "marking on the F.SilkS z");
        }

        // The text advances in +x: the rightmost stroke point of the 3-char string
        // lies well to the right of the origin (the '1' and '2' are advanced glyphs).
        let max_x = marks
            .iter()
            .flat_map(|m| {
                let Extent::Prism { shape, .. } = &m.feature.extent;
                shape.points().into_iter().map(|p| p.x)
            })
            .max()
            .unwrap();
        assert!(max_x > MM, "string advances past the first glyph in +x");
    }

    /// Write the test TTF fixture to a unique temp path (removed by the caller). Board and
    /// footprint lowering resolve fonts by *path*, so an end-to-end test needs a file.
    fn write_fixture_font() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("ecad-test-{}-{stamp}.ttf", std::process::id()));
        std::fs::write(&p, crate::ttf::build_test_ttf()).unwrap();
        p
    }

    /// With a `font` directive resolving to a real file, board text lowers to filled
    /// `Area` markings (outline glyphs) instead of stroke traces — and the font loads
    /// cleanly (no diagnostic).
    #[test]
    fn features_ttf_font_lowers_text_to_area_markings() {
        let path = write_fixture_font();
        let src = vec![
            GenDirective::Font {
                path: path.to_string_lossy().into_owned(),
            },
            GenDirective::Text {
                string: "HOo".into(),
                at: pt(0, 0),
                height: MM,
                layer: "F.SilkS".into(),
                orient: Orient::IDENTITY,
            },
        ];
        let feats = features(&src).unwrap();
        let marks: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Marking)
            .collect();
        assert_eq!(marks.len(), 3, "one Area per inked glyph (H, O, o)");
        for m in &marks {
            let Extent::Prism { shape, .. } = &m.feature.extent;
            assert!(
                matches!(shape, Shape2D::Area { .. }),
                "outline text is a filled Area, got {shape:?}"
            );
        }
        assert_eq!(
            font_load_failure(&src),
            None,
            "a loadable font records no failure"
        );
        std::fs::remove_file(&path).ok();
    }

    /// A `font` directive pointing at a missing file **degrades** to the stroke font
    /// (board text still lowers, as `Stroke` traces — the doc does not fail), and the
    /// failure surfaces on the [`ReconReport`] as a non-blocking `W_FONT_LOAD` warning
    /// that leaves the doc `is_clean`.
    #[test]
    fn missing_font_degrades_to_stroke_with_warning() {
        use crate::diagnostic::Diagnose;
        let src = vec![
            GenDirective::Font {
                path: "/no/such/font/file.ttf".into(),
            },
            GenDirective::Text {
                string: "R12".into(),
                at: pt(0, 0),
                height: MM,
                layer: "F.SilkS".into(),
                orient: Orient::IDENTITY,
            },
        ];
        // Rendering must not fail; it falls back to the stroke font (traced polylines).
        let feats = features(&src).unwrap();
        let marks: Vec<&NetFeature> = feats
            .iter()
            .filter(|f| f.feature.role == Role::Marking)
            .collect();
        assert!(marks.len() >= 3, "stroke fallback still lowers the text");
        for m in &marks {
            let Extent::Prism { shape, .. } = &m.feature.extent;
            assert!(
                matches!(shape, Shape2D::Stroke { .. }),
                "degraded to stroke traces, got {shape:?}"
            );
        }
        // Elaboration succeeds; the failure rides on the report as a warning that does
        // NOT dirty the doc (Decision 17 degrade-never-fail).
        let el = elaborate(&src, &BTreeMap::new(), &BTreeMap::new(), &PartLib::new())
            .expect("elaborates despite the bad font");
        assert!(el.report.font_load_failure.is_some());
        assert!(el.report.is_clean(), "a font degrade keeps the doc clean");
        let diags = el.report.diagnostics();
        let w = diags
            .iter()
            .find(|d| d.code == "W_FONT_LOAD")
            .expect("a W_FONT_LOAD diagnostic");
        assert!(!w.is_error(), "font load failure is a warning");
    }

    /// Issue 0024: an outer copper side with no mask slab — while the stackup carries a
    /// mask elsewhere — surfaces as a non-blocking `W_COPPER_NO_MASK` warning that leaves
    /// the doc `is_clean`. A fully-masked board is silent; a deliberately maskless board
    /// (zero mask slabs) is silent too.
    #[test]
    fn unmasked_copper_warns_but_stays_clean() {
        use crate::diagnostic::Diagnose;
        let cu = |name: &str, lo, hi| {
            GenDirective::Slab(Slab {
                name: name.into(),
                z: ZRange::new(lo, hi),
                role: Role::Conductor,
                material: None,
            })
        };
        let mask = |name: &str, lo, hi| {
            GenDirective::Slab(Slab {
                name: name.into(),
                z: ZRange::new(lo, hi),
                role: Role::Mask,
                material: None,
            })
        };
        let ov = BTreeMap::new();
        let rp = BTreeMap::new();

        // Default stackup (both masks) → no warning.
        let src: Source = vec![];
        let el = elaborate(&src, &ov, &rp, &PartLib::new()).unwrap();
        assert!(
            el.report.unmasked_copper.is_empty(),
            "default board fully masked"
        );

        // F.Mask only, both copper → the bottom copper side is unmasked.
        let f_mask_only: Source = vec![
            cu("F.Cu", 1_965_000, 2_000_000),
            cu("B.Cu", 0, 35_000),
            mask("F.Mask", 2_000_000, 2_010_000),
        ];
        let el = elaborate(&f_mask_only, &ov, &rp, &PartLib::new()).unwrap();
        assert_eq!(el.report.unmasked_copper, vec!["B.Cu".to_string()]);
        assert!(
            el.report.is_clean(),
            "an unmasked-copper warning keeps the doc clean"
        );
        let diags = el.report.diagnostics();
        let w = diags
            .iter()
            .find(|d| d.code == "W_COPPER_NO_MASK")
            .expect("a W_COPPER_NO_MASK diagnostic");
        assert!(!w.is_error(), "unmasked copper is a warning");
        assert!(
            w.message.contains("B.Cu"),
            "the message names the slab: {}",
            w.message
        );

        // Zero mask slabs anywhere → deliberately maskless, silent.
        let bare: Source = vec![cu("F.Cu", 1_965_000, 2_000_000), cu("B.Cu", 0, 35_000)];
        let el = elaborate(&bare, &ov, &rp, &PartLib::new()).unwrap();
        assert!(
            el.report.unmasked_copper.is_empty(),
            "bare-copper board is silent"
        );
        assert!(
            !el.report
                .diagnostics()
                .iter()
                .any(|d| d.code == "W_COPPER_NO_MASK"),
            "no warning for a deliberately maskless board"
        );
    }

    /// An unknown slab name is a hard elaboration error (no silent board-z fallback,
    /// Decision 13); the message names the unknown slab and the available names.
    #[test]
    fn features_unknown_slab_name_is_hard_error() {
        let src = vec![
            board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
            GenDirective::Region(RegionDecl {
                shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
                role: Role::Keepout(crate::geom::KeepoutKind::Copper),
                net: None,
                layer: "Q.Cu".into(),
            }),
        ];
        let err = features(&src).unwrap_err();
        assert!(err.contains("Q.Cu"), "names the unknown slab: {err}");
        assert!(err.contains("F.Cu"), "lists available slabs: {err}");

        // A text label on an unknown slab is likewise a hard error.
        let src = vec![GenDirective::Text {
            string: "X".into(),
            at: pt(0, 0),
            height: MM,
            layer: "Nope".into(),
            orient: Orient::IDENTITY,
        }];
        assert!(features(&src).unwrap_err().contains("Nope"));
    }

    /// A net-bound `Conductor` region on a non-copper slab (silk) is nonsense and is
    /// rejected by the materialization gate (Decision 13).
    #[test]
    fn features_conductor_pour_on_non_copper_slab_errors() {
        let src = vec![
            board_rect(pt(0, 0), pt(10 * MM, 10 * MM)),
            GenDirective::Region(RegionDecl {
                shape: Shape2D::rect(pt(2 * MM, 2 * MM), MM, MM),
                role: Role::Conductor,
                net: Some("GND".into()),
                layer: "F.SilkS".into(),
            }),
        ];
        let err = features(&src).unwrap_err();
        assert!(
            err.contains("F.SilkS") && err.contains("non-copper"),
            "rejects a pour on silk: {err}"
        );
    }

    /// A source with `Slab` directives makes `stackup()` return *those* slabs, in
    /// declaration order — not the 2-layer default.
    #[test]
    fn stackup_reads_authored_slabs() {
        // A non-default 2 mm board (distinct z's from `default_2layer`), with the middle
        // dielectric left material-less to also exercise the optional-material path.
        let authored = vec![
            Slab {
                name: "B.Cu".into(),
                z: ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: Some(crate::geom::Material::named("copper")),
            },
            Slab {
                name: "core".into(),
                z: ZRange::new(35_000, 1_965_000),
                role: Role::Substrate,
                material: None,
            },
            Slab {
                name: "F.Cu".into(),
                z: ZRange::new(1_965_000, 2_000_000),
                role: Role::Conductor,
                material: Some(crate::geom::Material::named("copper")),
            },
        ];
        let src: Source = authored.iter().cloned().map(GenDirective::Slab).collect();
        let su = stackup(&src);
        assert_eq!(
            su.slabs, authored,
            "stackup() returns the authored slabs verbatim"
        );
        assert_ne!(
            su,
            Stackup::default_2layer(),
            "authored slabs are not the default (distinct z's)"
        );
    }

    /// With no `Slab` directives, `stackup()` falls back to the unchanged 2-layer
    /// default — even when the source has other (non-slab) directives.
    #[test]
    fn stackup_defaults_when_no_slabs() {
        assert_eq!(stackup(&vec![]), Stackup::default_2layer());
        let src = vec![board_rect(pt(0, 0), pt(10 * MM, 10 * MM))];
        assert_eq!(
            stackup(&src),
            Stackup::default_2layer(),
            "non-slab directives don't disturb the default"
        );
    }

    // ---- refdes-pin reconciliation ----

    fn part_lib(name: &str) -> PartLib {
        let mut lib = PartLib::new();
        lib.insert(
            name.to_string(),
            PartDef {
                name: name.to_string(),
                pins: vec![],
                interfaces: BTreeMap::new(),
                graphics: vec![],
                texts: vec![],
                courtyard: None,
                class: None,
            },
        );
        lib
    }

    fn inst(path: &str, part: &str) -> GenDirective {
        GenDirective::Instance {
            path: path.to_string(),
            part: part.to_string(),
            params: BTreeMap::new(),
            label: None,
        }
    }

    /// Two entities pinned to one identical string surface as an `E_REFDES_PIN_DUP`
    /// finding on an otherwise-valid elaboration (non-blocking, like pos findings).
    #[test]
    fn duplicate_refdes_pin_is_surfaced() {
        let src = vec![inst("c0", "C"), inst("c1", "C")];
        let mut pins = BTreeMap::new();
        pins.insert(EntityId::new("c0"), "C7".to_string());
        pins.insert(EntityId::new("c1"), "C7".to_string());
        let elab = elaborate(&src, &BTreeMap::new(), &pins, &part_lib("C")).expect("elaborates");
        assert_eq!(
            elab.report.refdes_pin_dups,
            vec![(
                "C7".to_string(),
                vec![EntityId::new("c0"), EntityId::new("c1")]
            )]
        );
        assert!(!elab.report.is_clean());
        // Distinct pins do not collide.
        let mut ok = BTreeMap::new();
        ok.insert(EntityId::new("c0"), "C7".to_string());
        ok.insert(EntityId::new("c1"), "C8".to_string());
        let clean = elaborate(&src, &BTreeMap::new(), &ok, &part_lib("C")).expect("elaborates");
        assert!(clean.report.refdes_pin_dups.is_empty());
    }

    /// A refdes pin on an entity that does not exist after elaboration is orphaned —
    /// the same channel and behavior as a stale position override.
    #[test]
    fn refdes_pin_on_unknown_id_is_orphaned() {
        let src = vec![inst("c0", "C")];
        let mut pins = BTreeMap::new();
        pins.insert(EntityId::new("ghost"), "C9".to_string());
        let elab = elaborate(&src, &BTreeMap::new(), &pins, &part_lib("C")).expect("elaborates");
        assert!(elab.report.orphaned.contains(&EntityId::new("ghost")));
    }

    /// An entity carrying BOTH a pos override and a refdes pin, orphaned, is flagged
    /// exactly once (the refdes-orphan loop dedups against the pos-orphan loop).
    #[test]
    fn orphan_with_both_pos_override_and_refdes_pin_is_flagged_once() {
        let src = vec![inst("c0", "C")];
        let ghost = EntityId::new("ghost");
        let mut overrides = BTreeMap::new();
        overrides.insert(
            ghost.clone(),
            Override {
                pos: Some(Point { x: 1, y: 2 }),
                strength: Strength::Pin,
            },
        );
        let mut pins = BTreeMap::new();
        pins.insert(ghost.clone(), "C9".to_string());
        let elab = elaborate(&src, &overrides, &pins, &part_lib("C")).expect("elaborates");
        assert_eq!(
            elab.report
                .orphaned
                .iter()
                .filter(|&id| *id == ghost)
                .count(),
            1,
            "orphan reported once despite two override kinds"
        );
    }

    /// Issue 0019 (review): an imported courtyard with an outward-bowing arc edge must
    /// be covered by the convex hull the solver reserves. The arc apex is not a corner,
    /// so a corners-only lowering ([`Shape2D::points`]) would drop the bulge and
    /// under-reserve; `component_courtyard` flattens the arc to chords and hulls that, so
    /// the bulge lands inside the reserved polygon.
    #[test]
    fn component_courtyard_covers_an_arc_bulge() {
        use crate::geom::{Path, Seg, Shape2D};
        // Bottom edge (−1,0)→(1,0), then an arc bowing up through (0, 2 mm) back to the
        // start. (0, 2 mm) is the arc mid, not a corner: corners give max-y 0.
        let path = Path {
            start: pt(-1_000_000, 0),
            segs: vec![
                Seg::Line {
                    end: pt(1_000_000, 0),
                },
                Seg::Arc {
                    mid: pt(0, 2_000_000),
                    end: pt(-1_000_000, 0),
                },
            ],
        };
        let def = PartDef {
            name: "ARC".into(),
            pins: Vec::new(),
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: Some(Shape2D::polygon_path(path, 0)),
            class: None,
        };
        let (verts, _r) =
            component_courtyard(&def, Orient::IDENTITY).expect("arc courtyard has a hull");
        let max_y = verts.iter().map(|p| p.y).max().unwrap();
        assert!(
            max_y > 1_500_000,
            "the arc bulge (~2 mm) must be inside the reserved hull, got max-y {max_y}"
        );
    }
}
