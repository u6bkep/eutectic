//! Reflow (tier 3 — derived coordinates): the pure, deterministic, terminating flow of
//! the authored tree into per-component [`Placement`]s, plus the flexbox packing engine
//! and def-fragment expansion.

use crate::doc::{Nm, Point};
use crate::id::EntityId;
use crate::part::PartLib;
use crate::schematic::symbol::{Extent, MIN_BOX_H, MIN_BOX_W, header_width, symbol_extent};
use crate::schematic::{Align, Container, Direction, LayoutNode, SchematicLayout, Symbol};
use std::collections::{BTreeMap, BTreeSet};

// ----------------------------------------------------------------------------
// Reflow (tier 3 — derived coordinates)
// ----------------------------------------------------------------------------

/// A placed symbol's derived geometry in schematic space: the box center [`Point`] and
/// the (rotation-applied) [`Extent`]. Integer nm, y-up, independent of board space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub center: Point,
    pub extent: Extent,
}

/// A symbol's two flow rectangles. The **drawn** box ([`symbol_extent`], rotation-applied)
/// is what the renderer strokes and what [`Placement::extent`] carries; the **slot** is the
/// space the flow reserves for it — the drawn box widened to fit the header label that hangs
/// off its top-left corner (`slot.w = draw.w.max(header_width(..))`). Keeping them distinct
/// lets a long `refdes (Part)` header reserve room between neighbours without inflating the
/// box geometry (so pin stubs stay put), which is the header-overlap fix: the drawn box is a
/// pure function of the part, the label reservation lives only in the flow.
#[derive(Clone, Copy, Debug)]
struct SymSizing {
    slot: Extent,
    draw: Extent,
}

/// Gap between the placed extent and the unplaced bin, and between bin cells.
const BIN_GAP: Nm = 5_080_000; // 2 pitches of breathing room.

/// The minimum-box extent used for a component whose part is missing from the lib — the
/// view stays total (§20c) even for a dangling part.
pub(crate) const MIN_EXTENT: Extent = Extent {
    w: MIN_BOX_W,
    h: MIN_BOX_H,
};

/// Reflow a layout tree + the elaborated component universe into per-component schematic
/// placements — the derived-tier function (§20a). **Pure, deterministic, terminating:**
/// the same inputs always yield the byte-identical `BTreeMap` (guaranteed by BTreeMap
/// output + the pre-order tree walk + integer-only arithmetic; a determinism test asserts
/// two runs are equal).
///
/// `components` maps each instance id to its part name (the `Doc::components` shape,
/// reduced to what sizing needs); `lib` sizes each part's box. Placed symbols flow
/// through their containers (row along +x, column along −y) with `gap` spacing and
/// cross-axis `align`; nested containers size to their content; each symbol's pinned
/// `dx`/`dy` is added after flow placement; a 90/270 `rot` swaps the extent. Every
/// component **not** in the tree lands in the derived **unplaced bin** — a plain grid
/// below the placed extent (§20c totality: every component always has a coordinate). A
/// component whose part is missing from the lib still gets a [`MIN_EXTENT`] placement.
///
/// `headers` maps each component path to its fully-formatted header label (`"C3 (Cap)"` —
/// the derived refdes + part name the renderer draws above the box). Each placed symbol
/// reserves a flow slot at least [`header_width`] wide so neighbouring headers cannot
/// overlap; a path absent from the map (or an empty label) reserves nothing beyond the box.
/// The label is threaded in rather than re-derived here because the refdes is an annotate
/// query result ([`crate::annotate::refdes`]) reflow's inputs don't carry —
/// [`Doc::reflow_schematic`](crate::doc::Doc::reflow_schematic) computes it and passes it.
pub fn reflow(
    layout: &SchematicLayout,
    components: &BTreeMap<EntityId, String>,
    lib: &PartLib,
    def_fragments: &BTreeMap<String, SchematicLayout>,
    headers: &BTreeMap<EntityId, String>,
) -> BTreeMap<EntityId, Placement> {
    // Decision 20 (def-embedded layout): before the prune/place pipeline, expand every
    // def-instance `sym` (a `sym` whose path is a `def_fragments` key) into the def's
    // stamped fragment as a nested group. A doc-level `sym <inst.internal>` (a real internal
    // component path, placed explicitly in the doc tree) OVERRIDES the fragment's placement
    // of that same path — last-writer-wins with the doc as authority — so we collect the
    // doc-explicit paths first (before expansion) and drop any fragment sym that collides.
    let doc_explicit = doc_explicit_paths(&layout.roots, def_fragments);
    let expanded_layout = SchematicLayout {
        roots: expand_def_syms(&layout.roots, def_fragments, &doc_explicit, 0),
    };
    let layout = &expanded_layout;

    // Extent of an instance path: look up its part in the universe, then size via the lib;
    // an unknown path or missing part degrades to the min box (totality).
    let extent_of = |path: &str| -> Extent {
        components
            .get(&EntityId::new(path))
            .and_then(|part| lib.get(part))
            .map(symbol_extent)
            .unwrap_or(MIN_EXTENT)
    };
    // The header-label reservation for a path (0 if it has no label — bin cells and direct
    // callers that pass no headers).
    let header_reserve = |path: &str| -> Nm {
        headers
            .get(&EntityId::new(path))
            .map(|h| header_width(h))
            .unwrap_or(0)
    };
    // A symbol's drawn box (rot's 90/270 swap applied) plus its flow slot (the box widened
    // to fit its header label). The header hangs off the box's left edge, so a label wider
    // than the box reserves the extra width in the flow — never in the drawn geometry.
    let sized = |sym: &Symbol| -> SymSizing {
        let e = extent_of(&sym.path);
        let draw = if sym.swaps_extent() {
            Extent { w: e.h, h: e.w }
        } else {
            e
        };
        let slot = Extent {
            w: draw.w.max(header_reserve(&sym.path)),
            h: draw.h,
        };
        SymSizing { slot, draw }
    };

    let mut out: BTreeMap<EntityId, Placement> = BTreeMap::new();

    // Prune symbols the tree names but that are not populated components (a DNP-dropped
    // `if=false` part, or a path unknown to the source): they must not render and must not
    // reserve a flow slot (§20c × 21b — a depopulated part is genuinely absent, and
    // validation has already turned any real typo into a hard error before reflow runs).
    // Pruning once here keeps the packing functions oblivious to the component universe.
    let placeable = |path: &str| components.contains_key(&EntityId::new(path));
    let roots = prune_unplaceable(&layout.roots, &placeable);

    // The authored roots lay out as an implicit top-level column at the origin. Its
    // returned extent tells the bin where "below the placed content" is.
    let root = Container {
        dir: Direction::Column,
        name: None,
        gap: 0,
        align: Align::Start,
        children: roots,
    };
    let placed_extent = place_container(&root, Point { x: 0, y: 0 }, &sized, &mut out);

    // Unplaced bin: every component the tree did not place, in id order, into a grid below
    // the placed extent (§20c).
    let placed: BTreeSet<EntityId> = out.keys().cloned().collect();
    let unplaced: Vec<&EntityId> = components
        .keys()
        .filter(|id| !placed.contains(*id))
        .collect();
    if !unplaced.is_empty() {
        place_bin(
            &unplaced,
            &extent_of,
            &header_reserve,
            placed_extent,
            &mut out,
        );
    }

    out
}

/// Recursion depth cap on def-fragment expansion (Decision 20 embedded in a def). A
/// fragment may itself contain a def-instance `sym` (a nested reused circuit), which
/// expands recursively; distinct instance paths make genuine nesting acyclic and shallow,
/// but the guard bounds a pathological fragment before it blows the stack — consistent with
/// [`crate::elaborate::MAX_DEF_DEPTH`].
pub(crate) const MAX_FRAGMENT_DEPTH: usize = 64;

/// The set of component paths a doc-level `sym` places explicitly — every `sym` path in the
/// tree that is NOT itself a def-instance key (i.e. a real leaf-component path the author
/// wrote directly, including a one-off `sym sense[0].R1` into an instance). Collected over
/// the *pre-expansion* tree so the override rule (§20, doc wins) can drop the fragment's
/// copy of any such path. A def-instance `sym` is not "explicit placement of a component" —
/// it is a group that expands — so it is excluded here.
fn doc_explicit_paths(
    nodes: &[LayoutNode],
    def_fragments: &BTreeMap<String, SchematicLayout>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    fn walk(
        nodes: &[LayoutNode],
        def_fragments: &BTreeMap<String, SchematicLayout>,
        out: &mut BTreeSet<String>,
    ) {
        for n in nodes {
            match n {
                LayoutNode::Symbol(s) if !def_fragments.contains_key(&s.path) => {
                    out.insert(s.path.clone());
                }
                LayoutNode::Container(c) => walk(&c.children, def_fragments, out),
                _ => {}
            }
        }
    }
    walk(nodes, def_fragments, &mut out);
    out
}

/// Expand every def-instance `sym` (a `sym` whose path is a `def_fragments` key) into the
/// def's stamped fragment as a nested `Container` group (Decision 20 embedded in a def). The
/// container is a plain zero-gap `Start`-aligned column with no name; its children are the
/// fragment's already-prefixed roots, recursively expanded (a fragment may nest another def
/// instance). A fragment `sym` whose prefixed path is in `doc_explicit` is DROPPED — the
/// doc-level placement of that internal path wins (override precedence).
///
/// A `sym` whose path is neither a def-instance key nor otherwise special is left as-is (a
/// real component, or an unknown/DNP path the ordinary prune step handles). In particular a
/// def instance with NO fragment (its def declared no `schematic` block) is not a key here,
/// so it passes through unchanged and — being neither a component nor a fragment — prunes
/// away, its internal components landing in the unplaced bin (documented totality behaviour).
///
/// **v1 limitation (documented):** a def-instance `sym`'s authored `rot`/`dx`/`dy` are
/// **IGNORED**. A def instance is a GROUP, not a leaf box, so a cardinal rotation or a
/// pinned offset on the group has no well-defined v1 meaning (it would have to transform the
/// whole subtree's coordinate frame). They are silently dropped rather than half-applied;
/// group-level transforms are a follow-up.
fn expand_def_syms(
    nodes: &[LayoutNode],
    def_fragments: &BTreeMap<String, SchematicLayout>,
    doc_explicit: &BTreeSet<String>,
    depth: usize,
) -> Vec<LayoutNode> {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            LayoutNode::Symbol(s) => {
                if let Some(frag) = def_fragments.get(&s.path) {
                    // A def-instance sym: replace with a group of the fragment's roots. The
                    // depth cap is a stack backstop only — a doc that would exceed it is
                    // already rejected at commit by `validate`'s `fragment_depth` check
                    // (E_SCHEMATIC), so a committed doc never reaches this drop. Kept so a
                    // direct `reflow` call on a hand-built over-deep table can't recurse away.
                    if depth >= MAX_FRAGMENT_DEPTH {
                        continue;
                    }
                    // Drop fragment syms the doc places explicitly (override precedence).
                    let children = expand_def_syms(
                        &drop_explicit(&frag.roots, doc_explicit),
                        def_fragments,
                        doc_explicit,
                        depth + 1,
                    );
                    out.push(LayoutNode::Container(Container {
                        dir: Direction::Column,
                        name: None,
                        gap: 0,
                        align: Align::Start,
                        children,
                    }));
                } else {
                    // A real component (or unknown/DNP path handled by the prune step).
                    out.push(n.clone());
                }
            }
            LayoutNode::Container(c) => out.push(LayoutNode::Container(Container {
                children: expand_def_syms(&c.children, def_fragments, doc_explicit, depth),
                ..c.clone()
            })),
            other => out.push(other.clone()),
        }
    }
    out
}

/// The maximum def-instance nesting depth reachable from the fragment stamped at `path`,
/// counting `path` itself as depth 1: 1 for a leaf fragment, +1 per nested def-instance
/// `sym` inside it. Mirrors the recursion [`expand_def_syms`] performs at reflow, so
/// [`validate`] can reject (as `E_SCHEMATIC`) any fragment that would exceed
/// [`MAX_FRAGMENT_DEPTH`] and be silently truncated. The `guard` bounds this walk itself
/// against a (validation-time) pathological input — a cycle would be an elaboration-time
/// `E_DEF_CYCLE` long before a fragment table is built, so this only stops runaway
/// recursion, returning a value past the cap so the caller still flags it.
pub(crate) fn fragment_depth(
    path: &str,
    def_fragments: &BTreeMap<String, SchematicLayout>,
    guard: usize,
) -> usize {
    if guard > MAX_FRAGMENT_DEPTH {
        return guard; // past the cap; the caller's `>` check fires regardless
    }
    let Some(frag) = def_fragments.get(path) else {
        return 0;
    };
    let deepest_child = frag
        .symbol_paths()
        .into_iter()
        .filter(|p| def_fragments.contains_key(*p))
        .map(|p| fragment_depth(p, def_fragments, guard + 1))
        .max()
        .unwrap_or(0);
    1 + deepest_child
}

/// Drop every fragment `sym` whose path a doc-level `sym` places explicitly (override
/// precedence, §20): the doc-level placement wins, so the fragment's copy is removed rather
/// than double-placing the same component. Recurses into containers; trivia/wires kept.
fn drop_explicit(nodes: &[LayoutNode], doc_explicit: &BTreeSet<String>) -> Vec<LayoutNode> {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            LayoutNode::Symbol(s) if doc_explicit.contains(&s.path) => {} // doc wins; drop it
            LayoutNode::Container(c) => out.push(LayoutNode::Container(Container {
                children: drop_explicit(&c.children, doc_explicit),
                ..c.clone()
            })),
            other => out.push(other.clone()),
        }
    }
    out
}

/// Drop every `sym` node whose path is not placeable (not a populated component),
/// recursing into containers. Trivia and containers are kept (an emptied container still
/// sizes to zero and consumes its gap slot — harmless, and preserves authored structure).
/// A pure tree transform used by [`reflow`] to make depopulated / unknown symbols vanish
/// from the flow entirely rather than render a phantom box.
fn prune_unplaceable(nodes: &[LayoutNode], placeable: &impl Fn(&str) -> bool) -> Vec<LayoutNode> {
    let mut out = Vec::new();
    for n in nodes {
        match n {
            LayoutNode::Symbol(s) if !placeable(&s.path) => {} // drop it
            LayoutNode::Container(c) => out.push(LayoutNode::Container(Container {
                children: prune_unplaceable(&c.children, placeable),
                ..c.clone()
            })),
            other => out.push(other.clone()),
        }
    }
    out
}

// ----------------------------------------------------------------------------
// Packing (the flow geometry)
// ----------------------------------------------------------------------------
//
// Schematic space is y-up. A `row` advances along +x; a `column` stacks along −y (so it
// reads top-to-bottom). Each placed symbol stores its **box center**. The geometry is a
// two-pass flow per container: measure every child's extent, size the container to the
// content, then place children at running main-axis offsets with cross-axis alignment.
// Every quantity is integer nm; the only division is the exact halving for a box center
// / a `Center` alignment (a symbol box is sized so `w`/`h` need no rounding beyond that).

/// A node's laid-out extent — the slot the flow reserves for it. For a symbol this is
/// exactly its box; the pinned `dx`/`dy` (§20b, the CSS absolute-positioning analog) is a
/// pure *position* shift applied in [`place_node`] and deliberately does **not** enlarge
/// the slot, so a pinned offset may overlap a neighbour (that is the escape hatch's whole
/// point — absolute positioning ignores flow).
fn measure(node: &LayoutNode, sized: &impl Fn(&Symbol) -> SymSizing) -> Extent {
    match node {
        LayoutNode::Symbol(s) => sized(s).slot,
        LayoutNode::Container(c) => measure_container(c, sized),
        // A wire is presentational (§20d) and reserves no flow slot; trivia is preserved
        // for round-trip. Neither has flow geometry.
        LayoutNode::Wire(_) | LayoutNode::Comment(_) | LayoutNode::Blank => Extent { w: 0, h: 0 },
    }
}

/// The children that participate in flow — containers and symbols, with trivia
/// (comments/blanks) filtered out. Packing must ignore trivia so a comment between two
/// symbols does not eat a gap slot; the two flow-consuming passes (measure and place)
/// share this so they stay in lockstep.
fn flow_children(c: &Container) -> Vec<&LayoutNode> {
    c.children
        .iter()
        .filter(|n| matches!(n, LayoutNode::Container(_) | LayoutNode::Symbol(_)))
        .collect()
}

/// A container's content extent: main-axis is the sum of child main extents plus gaps;
/// cross-axis is the max child cross extent. Empty container ⇒ zero extent.
fn measure_container(c: &Container, sized: &impl Fn(&Symbol) -> SymSizing) -> Extent {
    let child_ext: Vec<Extent> = flow_children(c).iter().map(|n| measure(n, sized)).collect();
    if child_ext.is_empty() {
        return Extent { w: 0, h: 0 };
    }
    let gaps = c.gap * (child_ext.len() as Nm - 1);
    match c.dir {
        Direction::Row => Extent {
            w: child_ext.iter().map(|e| e.w).sum::<Nm>() + gaps,
            h: child_ext.iter().map(|e| e.h).max().unwrap_or(0),
        },
        Direction::Column => Extent {
            w: child_ext.iter().map(|e| e.w).max().unwrap_or(0),
            h: child_ext.iter().map(|e| e.h).sum::<Nm>() + gaps,
        },
    }
}

/// Place a container whose top-left corner sits at `origin` (max-y, min-x — the natural
/// reading corner in y-up space). Fills `out` with box centers for every symbol beneath
/// it and returns the container's own extent (so a parent can advance past it, and the
/// root caller can find "below the content" for the bin).
fn place_container(
    c: &Container,
    origin: Point,
    sized: &impl Fn(&Symbol) -> SymSizing,
    out: &mut BTreeMap<EntityId, Placement>,
) -> Extent {
    let ext = measure_container(c, sized);
    let children = flow_children(c);
    let child_ext: Vec<Extent> = children.iter().map(|n| measure(n, sized)).collect();

    // Running position along the main axis, tracked as the child slot's leading corner.
    // Row: leading x grows left→right from origin.x. Column: leading y falls top→bottom
    // from origin.y.
    let mut main = 0i64;
    for (child, ce) in children.iter().zip(&child_ext) {
        // Cross-axis offset of this child's slot leading corner, from the container's
        // cross-axis leading corner, per `align`.
        let cross = match c.dir {
            Direction::Row => cross_offset(c.align, ext.h, ce.h),
            Direction::Column => cross_offset(c.align, ext.w, ce.w),
        };
        // The child slot's top-left corner in absolute space.
        let slot = match c.dir {
            Direction::Row => Point {
                x: origin.x + main,
                y: origin.y - cross,
            },
            Direction::Column => Point {
                x: origin.x + cross,
                y: origin.y - main,
            },
        };
        place_node(child, slot, sized, out);
        main += match c.dir {
            Direction::Row => ce.w,
            Direction::Column => ce.h,
        } + c.gap;
    }
    ext
}

/// Cross-axis leading-corner offset for a child of cross extent `child` inside a track of
/// cross extent `track`, per alignment. Integer; `Center` halves the slack (exact for the
/// pitch-based sizes here — any residual nm bias is deterministic).
fn cross_offset(align: Align, track: Nm, child: Nm) -> Nm {
    match align {
        Align::Start => 0,
        Align::Center => (track - child) / 2,
        Align::End => track - child,
    }
}

/// Place one node whose slot top-left corner is `slot` and whose slot extent is `ext`.
/// A symbol's **drawn box** lands flush with the slot's top-left corner (`ext` is the slot,
/// which may be wider than the box to reserve header room — see [`SymSizing`]), so the
/// header that hangs off the box's left edge stays inside the slot and off the neighbour;
/// the box is then shifted by its pinned `dx`/`dy`. The stored [`Placement::extent`] is the
/// drawn box, not the slot. A container recurses.
fn place_node(
    node: &LayoutNode,
    slot: Point,
    sized: &impl Fn(&Symbol) -> SymSizing,
    out: &mut BTreeMap<EntityId, Placement>,
) {
    match node {
        LayoutNode::Symbol(s) => {
            let box_ext = sized(s).draw;
            // The drawn box hugs the slot's top-left corner (the header reservation, if any,
            // is the slack on the box's right). Center = corner minus half the *box* extent
            // in y-up space (the slot height equals the box height, so y is unaffected).
            let center = Point {
                x: slot.x + box_ext.w / 2 + s.dx,
                y: slot.y - box_ext.h / 2 + s.dy,
            };
            out.insert(
                EntityId::new(s.path.clone()),
                Placement {
                    center,
                    extent: box_ext,
                },
            );
        }
        LayoutNode::Container(c) => {
            place_container(c, slot, sized, out);
        }
        // Wires and trivia never reach here (filtered by `flow_children`); handled for
        // totality.
        LayoutNode::Wire(_) | LayoutNode::Comment(_) | LayoutNode::Blank => {}
    }
}

/// Place the unplaced bin: a plain grid of `BIN_COLS` columns, sitting one [`BIN_GAP`]
/// below the placed content (§20c). Cells are uniform so the grid is a clean lattice; the
/// cell width is the widest of every unplaced box **and every unplaced header label**
/// (`header_reserve`), so a header can never spill past its cell onto the next — the same
/// reservation the placed flow makes. Boxes hug their cell's left edge (matching the flow's
/// header-left anchoring), so a header, drawn from the box's left, stays inside its
/// header-wide cell. Ids fill row-major in sorted order. Deterministic.
fn place_bin(
    unplaced: &[&EntityId],
    extent_of: &impl Fn(&str) -> Extent,
    header_reserve: &impl Fn(&str) -> Nm,
    placed: Extent,
    out: &mut BTreeMap<EntityId, Placement>,
) {
    const BIN_COLS: usize = 8;
    let exts: Vec<Extent> = unplaced.iter().map(|id| extent_of(id.as_str())).collect();
    let cell_w = unplaced
        .iter()
        .zip(&exts)
        .map(|(id, e)| e.w.max(header_reserve(id.as_str())))
        .max()
        .unwrap_or(MIN_BOX_W);
    let cell_h = exts.iter().map(|e| e.h).max().unwrap_or(MIN_BOX_H);
    // The bin's top edge is one gap below the placed content's bottom (origin y = 0, so
    // the content spans y ∈ [−placed.h, 0]).
    let top = -placed.h - BIN_GAP;
    for (i, (id, e)) in unplaced.iter().zip(&exts).enumerate() {
        let col = (i % BIN_COLS) as Nm;
        let row = (i / BIN_COLS) as Nm;
        // Cell top-left corner; the box hugs it (its header, anchored at the box's left,
        // then fits within the header-wide cell reserved above). Vertically centred in the
        // cell so uneven box heights read as a tidy row of baselines.
        let slot = Point {
            x: col * (cell_w + BIN_GAP),
            y: top - row * (cell_h + BIN_GAP),
        };
        out.insert(
            (*id).clone(),
            Placement {
                center: Point {
                    x: slot.x + e.w / 2,
                    y: slot.y - cell_h / 2,
                },
                extent: *e,
            },
        );
    }
}
