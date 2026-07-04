//! The directive IR: the pure data vocabulary shared by the text front-end and
//! elaboration. Hoisted here (out of `elaborate`/`text`) so it forms the common
//! *downward* dependency of both — breaking the former text<->elaborate cycle.
//!
//! This module holds only DATA types and pure builders/queries over them: the
//! generative `GenDirective` program vocabulary, the `Block`/`Node` block-tree
//! data model, and the small pure helpers (`coords`, `refs`, `board_rect`). The
//! parsing/serialization (text.rs) and the elaboration passes (elaborate.rs)
//! stay where they are and depend *on* this module.

use crate::doc::{Nm, Orient, Point};
use crate::geom::{Role, Shape2D, Slab};
use std::collections::BTreeMap;

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
    /// resolved once per elaboration into an `Env` (`crate::elaborate::expr::Env`) with cycle
    /// detection. The authored `expr` text is stored verbatim and round-trips as written
    /// (it *is* the generative program, Decision 21). Not a placement/connectivity
    /// directive — elaboration resolves it into the param environment before Pass 1;
    /// the placement/connectivity passes ignore it.
    Param {
        name: String,
        /// The authored expression text (e.g. `"3"`, `"n + 1"`, `"4.7k"`). Parsed and
        /// evaluated by `crate::elaborate::expr`; never pre-evaluated (serializes as authored).
        expr: String,
    },
    /// A **`def`** — a named, reusable sub-circuit (Decision 21a). The React-component
    /// mental model: `def` ≈ component, ports ≈ props. Its `body` is an ordinary
    /// [`Source`] fragment (parts, internal nets, nested def *instantiations*) authored
    /// against the def's own scope; `params` are declared parameters with default
    /// expression texts (evaluated in the def's scope, overridable at each instantiation
    /// via `p:`); `ports` is the typed I/O surface — each maps an outward port name to a
    /// `(internal-path, selector)` pin the port exposes.
    ///
    /// A `def` is **not** a materialized directive: it declares a template, consumed by
    /// [`expand_defs`] when an `inst <path> <def-name>` names it. Definitions are
    /// **top-level only** (v1): a def body may *instantiate* another def but may not
    /// *define* one. Elaboration's placement/connectivity passes never see a `Def` — the
    /// def-expansion pre-pass strips every one and stamps its body per instantiation.
    Def {
        name: String,
        /// Declared parameters, in authored order: `(param-name, default-expr-text)`.
        /// The default is evaluated in the def's scope when an instantiation omits it.
        params: Vec<(String, String)>,
        /// The def body — a source fragment authored in the def's local scope (paths and
        /// net names are def-relative; they gain the instance path prefix when stamped),
        /// interleaved with the comment/blank trivia between directives so a
        /// mixed-authorship def body round-trips (Decision 21). [`expand_defs`] reads only
        /// the [`DefNode::Directive`] entries; serialization reproduces the trivia.
        body: Vec<DefNode>,
        /// The port surface: port name → `(internal-path, selector)`. A connection to a
        /// def instance's `<inst-path>.<port>` resolves through to this bound internal
        /// pin's PAD identity (Decision 21: pad number is THE identity — no new
        /// namespace). `BTreeMap` for canonical serialization order.
        ports: BTreeMap<String, (String, String)>,
        /// An optional Decision-20 schematic layout fragment authored over the def's
        /// INTERNAL paths (`R1`, not `sense[0].R1`), stamped per instance so a reused
        /// circuit renders identically everywhere it is instantiated.
        layout: Option<crate::schematic::SchematicLayout>,
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
    ///     loop variable `i` in this instance's own param/`if` expressions (the innermost
    ///     binding wins — a range `i` shadows any doc-level `param i`). Bounds are
    ///     expression texts; the expanded count is capped (anti-footgun). Note that
    ///     refdes *numbering* over expanded instances follows the ordinary Decision-14
    ///     annotation (path order), so growing/shrinking a range can renumber at a digit
    ///     boundary (e.g. R9→R10); the stability mechanism for a shipped board is the
    ///     EntityId-keyed refdes override (`refdes <path> <string>`), not the range.
    ///   - **Expression params**: `param_exprs` are `p:<key>=(<expr>)` values evaluated
    ///     and formatted into the plain instance's verbatim `params` (display-normal
    ///     spelling). `params` here are the *verbatim* `p:` values (non-expression),
    ///     copied through unchanged.
    ///   - **Population conditional**: `if_expr = Some(<expr>)` — a boolean that, when
    ///     false, drops the instance; any directive still referencing the dropped path
    ///     (connection *or* placement) is skipped and surfaced as a `W_DNP` warning
    ///     (DNP variants — see [`directive_refs`]).
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

/// One entry in a [`Def`](GenDirective::Def) body: a body directive, or a preserved
/// trivia line (a comment or a blank) so a mixed-authorship def body round-trips
/// (Decision 21). Mirrors [`crate::schematic::LayoutNode`]'s trivia handling; the
/// def-expansion pre-pass ([`expand_defs`]) reads only the [`Directive`](Self::Directive)
/// entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefNode {
    /// A body directive (a part instance, an internal net, a nested def instantiation).
    Directive(GenDirective),
    /// A whole-line comment, stored without its leading `#` (re-emitted at canonical
    /// indent) — same convention as [`Node::Comment`].
    Comment(String),
    /// A blank line.
    Blank,
}

/// The generous upper bound on a single range's expanded instance count (Decision 21b
/// anti-footgun). A negative or absurd bound is an `E_EXPR` fault rather than an attempt
/// to allocate billions of instances; 10k is far beyond any real board's part count on
/// one directive while still catching a runaway expression.
pub const MAX_RANGE_INSTANCES: i64 = 10_000;

impl GenDirective {
    /// Every coordinate/length (nm) a directive contributes — the values an ingest
    /// boundary range-checks against [`crate::geom::MAX_COORD`] (issue 0018). Points
    /// contribute both components; scalar spans (widths/gaps/heights, slab z-faces) their
    /// signed magnitude. Directives with no geometry (connectivity, alignment) contribute
    /// nothing. `Rotate`'s quaternion is angle-scaled, not a coordinate, so it is
    /// deliberately excluded. Shared by the text-parse and command-ingress validators so
    /// both bound the same fields.
    pub fn coords(&self) -> Vec<Nm> {
        let shape_coords = |s: &Shape2D| s.coords().into_iter().flat_map(|p| [p.x, p.y]).collect();
        match self {
            GenDirective::Place { pos, .. } | GenDirective::Fix { pos, .. } => vec![pos.x, pos.y],
            GenDirective::Board { outline } => shape_coords(outline),
            GenDirective::Cutout { shape } | GenDirective::Region(RegionDecl { shape, .. }) => {
                shape_coords(shape)
            }
            GenDirective::Hole { center, dia } => vec![center.x, center.y, *dia],
            GenDirective::Text { at, height, .. } => vec![at.x, at.y, *height],
            GenDirective::Near { within, .. } | GenDirective::NearPin { within, .. } => {
                vec![*within]
            }
            GenDirective::MinSep { gap, .. } => vec![*gap],
            GenDirective::Slab(s) => vec![s.z.lo, s.z.hi],
            _ => vec![],
        }
    }

    /// Every instance path a directive *references* (not the path it declares), paired with
    /// a human context label. Used only to surface DNP dangling references (Decision 21b):
    /// when a false `if=` depopulates an instance, any directive that still points at it is
    /// reported as a `W_DNP` warning — uniformly across connectivity (`net`/`nc`/`connect`)
    /// and placement (`near`/`minsep`/`align`/`place`/`fix`/`rotate`/`nearpin`) directives.
    /// A directive that declares or references nothing (a `Board`, `Slab`, `Text`, …) yields
    /// an empty list.
    pub fn refs(&self) -> Vec<(String, String)> {
        match self {
            GenDirective::Place { path, .. } => vec![(format!("place `{path}`"), path.clone())],
            GenDirective::Fix { path, .. } => vec![(format!("fix `{path}`"), path.clone())],
            GenDirective::Rotate { path, .. } => vec![(format!("rotate `{path}`"), path.clone())],
            GenDirective::Near { a, b, .. } => vec![
                (format!("near `{a}`"), a.clone()),
                (format!("near `{b}`"), b.clone()),
            ],
            GenDirective::MinSep { a, b, .. } => vec![
                (format!("minsep `{a}`"), a.clone()),
                (format!("minsep `{b}`"), b.clone()),
            ],
            GenDirective::NearPin { a, b_comp, .. } => vec![
                (format!("nearpin `{a}`"), a.clone()),
                (format!("nearpin `{b_comp}`"), b_comp.clone()),
            ],
            GenDirective::AlignX { nodes } => nodes
                .iter()
                .map(|n| (format!("alignx `{n}`"), n.clone()))
                .collect(),
            GenDirective::AlignY { nodes } => nodes
                .iter()
                .map(|n| (format!("aligny `{n}`"), n.clone()))
                .collect(),
            GenDirective::ConnectInterface { a, b } => vec![
                (format!("connect `{}`", a.0), a.0.clone()),
                (format!("connect `{}`", b.0), b.0.clone()),
            ],
            GenDirective::ConnectPins { net, pins } => pins
                .iter()
                .map(|(comp, _)| (format!("net `{net}`"), comp.clone()))
                .collect(),
            GenDirective::NoConnect { pins } => pins
                .iter()
                .map(|(comp, _)| ("no-connect".to_string(), comp.clone()))
                .collect(),
            _ => vec![],
        }
    }
}

/// Thin free-fn wrapper over [`GenDirective::coords`], kept so the pre-existing
/// call sites (`directive_coords(d)`) in the text-parse and command-ingress
/// validators are unchanged.
pub fn directive_coords(d: &GenDirective) -> Vec<Nm> {
    d.coords()
}

/// Thin free-fn wrapper over [`GenDirective::refs`], kept so the pre-existing
/// call site (`directive_refs(d)`) in elaboration is unchanged.
pub fn directive_refs(d: &GenDirective) -> Vec<(String, String)> {
    d.refs()
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
