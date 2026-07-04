//! The schematic layout tree (Decision 20) — authored structure, derived coordinates.
//!
//! Decision 20 opens the schematic front as *the second derived projection of the
//! generative truth* (the flat netlist is the first). Two things live here, on the two
//! sides of the tier line the whole architecture turns on (docs/architecture.md, §20a):
//!
//!   - **Authored (tier 1):** [`SchematicLayout`] — a tiny nested-container tree
//!     (`row`/`column` with symbols as leaves), a deliberately small CSS-flexbox subset
//!     (§20b). It parses from the `schematic { … }` block grammar in [`crate::text`],
//!     elaborates with real diagnostics ([`validate`]: `E_SCHEMATIC` unknown/duplicate
//!     comp paths and duplicate sibling names, plus a `W_SCHEMATIC_UNPLACED` warning for
//!     any component not in the tree), and round-trips byte-identically.
//!
//!   - **Derived (tier 3):** the *coordinates*, produced by [`reflow`] — a pure,
//!     deterministic, terminating flow of the tree into per-component positions in a
//!     schematic coordinate space independent of the board. It is elaboration-class, not
//!     routing (§20a): no solver, milliseconds, byte-identical every run. Coordinates are
//!     **never serialized** (§20a: re-derivable state is not emitted) — [`reflow`] is an
//!     on-demand function, the same shape as [`crate::elaborate::regions`]/`stackup`
//!     (pure over the authored state), *not* a memoized [`crate::query`] key: the query
//!     engine memoizes on the coarse `conn/geom/route` input revisions, and the layout
//!     tree is not one of those inputs, so a memo keyed on them would go stale on a
//!     tree-only edit. A pure recompute is correct and cheap.
//!
//! The view is **total** (§20c): [`reflow`] always returns a coordinate for *every*
//! component — anything absent from the tree lands in a derived "unplaced bin" (a plain
//! grid), so the schematic never silently omits a part.

use crate::doc::{Nm, Orient, Point};
use crate::id::EntityId;
use crate::part::{PartDef, PartLib};
use std::collections::{BTreeMap, BTreeSet};

// ----------------------------------------------------------------------------
// The authored layout tree (tier 1)
// ----------------------------------------------------------------------------

/// Flow direction of a container — the literal CSS flexbox names (§20b). `Row` lays
/// children out along +x; `Column` stacks them along −y (schematic space is y-up, so a
/// column reads top-to-bottom).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Row,
    Column,
}

/// Cross-axis alignment within a container (the CSS `align-items` subset, §20b):
/// children of unequal cross-axis extent line up at the `Start`, `Center`, or `End` of
/// the container's cross axis. Default `Start`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
}

/// One node in the layout tree: either a nested container or a symbol leaf. Kept as a
/// plain data enum (no methods that mutate) so the future GUI (§21d mode 1) can CRUD it
/// through the command algebra — the model is records, not code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutNode {
    Container(Container),
    Symbol(Symbol),
    /// A drawn, purely **presentational** pin-to-pin connection (§20d). Carries no
    /// layout semantics for the *flow* — it reserves no slot and never moves a symbol —
    /// but it is a real authored node that [`schematic_svg`](crate::schematic_svg) draws
    /// and [`validate`] range-checks / net-cross-checks. A no-op downstream: it has zero
    /// effect on the netlist or DRC (the netlist is the truth; a wire is a picture of it).
    Wire(Wire),
    /// A preserved whole-line comment (stored without its leading `#`), so mixed
    /// authorship inside a `schematic` block round-trips (the Decision-20/21 requirement).
    /// Carries no layout semantics — [`reflow`] and [`validate`] skip it.
    Comment(String),
    /// A preserved blank line, same round-trip purpose as [`LayoutNode::Comment`].
    Blank,
}

/// One endpoint of a drawn [`Wire`]: a component instance path and a pin identity on it
/// (a pad number or `port.signal`, the [`PinRef`](crate::doc::PinRef) vocabulary). Kept
/// as the authored `(comp, pin)` split (the `split_last_dot` idiom in [`crate::text`]) so
/// a hierarchical comp path with dots survives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireEnd {
    pub comp: String,
    pub pin: String,
}

/// A drawn wire (§20d): a presentational line between two pins, optionally routed through
/// authored **waypoints** (`via (x,y) …`). v1 renders it as a straight segment (no
/// waypoints) or a polyline through the waypoints — a deliberately dumb drawing, never a
/// router. Waypoints are schematic-space coordinates (nm), range-checked at parse time
/// like every other authored length (issue 0018). A wire is a no-op to the netlist: it
/// exists only to let an author draw a connection the way they like. Drawing a wire
/// between pins on *different* nets is legal (the tag at each pin still tells the truth)
/// but earns a `W_SCHEMATIC_WIRE` "your drawing disagrees with the netlist" warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wire {
    pub a: WireEnd,
    pub b: WireEnd,
    /// Presentational routing waypoints in schematic space, in draw order. Empty ⇒ a
    /// straight pin-to-pin segment.
    pub waypoints: Vec<Point>,
}

/// A `row`/`column` flow container. `name` is optional but, when present, must be unique
/// among siblings (needed later for GUI addressing / reconciliation — §20b); duplicates
/// are an `E_SCHEMATIC` error. Containers nest arbitrarily and size to their content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub dir: Direction,
    pub name: Option<String>,
    /// Main-axis spacing between children, in nm (`gap=` — literal length in v1;
    /// expressions are a sibling branch). Default 0.
    pub gap: Nm,
    pub align: Align,
    pub children: Vec<LayoutNode>,
}

/// A symbol leaf: one placed component, addressed by its hierarchical instance `path`
/// (the same string an `inst` directive uses). `rot` is an authored orientation
/// (§20b — authored only, no auto-orient), one of the four cardinals. `dx`/`dy` is the
/// pinned-offset escape hatch *within the parent container* (§20b, the CSS
/// absolute-positioning analog): applied after flow placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub path: String,
    /// Authored orientation. Cardinal only in v1; a 90/270 rotation swaps the symbol's
    /// extent during reflow. Default identity.
    pub rot: Orient,
    /// Pinned in-container offset (nm), applied on top of the flow position. Both default
    /// 0 (the common "just flow it" case).
    pub dx: Nm,
    pub dy: Nm,
}

/// The whole authored layout: a root list of nodes (a document has at most one
/// `schematic` block — the last wins, mirroring `board`). This is the reconciliation
/// unit (§20a). An empty layout is the honest default (everything lands in the unplaced
/// bin).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SchematicLayout {
    pub roots: Vec<LayoutNode>,
}

impl Symbol {
    /// Whether the authored rotation is a quarter-turn (90°/270° about z), which swaps
    /// the symbol's width and height during reflow. Uses the rotation-aware compare
    /// ([`Orient::same_rotation`]) so both the exact cardinal quaternion and its antipode
    /// count.
    fn swaps_extent(&self) -> bool {
        let q90 = Orient::from_deg(90).unwrap();
        let q270 = Orient::from_deg(270).unwrap();
        self.rot.same_rotation(q90) || self.rot.same_rotation(q270)
    }
}

impl SchematicLayout {
    /// Every symbol path that appears anywhere in the tree, in a pre-order walk. Used by
    /// [`validate`] and [`reflow`] to relate the tree to the component universe.
    pub(crate) fn symbol_paths(&self) -> Vec<&str> {
        let mut out = Vec::new();
        fn walk<'a>(nodes: &'a [LayoutNode], out: &mut Vec<&'a str>) {
            for n in nodes {
                match n {
                    LayoutNode::Symbol(s) => out.push(s.path.as_str()),
                    LayoutNode::Container(c) => walk(&c.children, out),
                    LayoutNode::Wire(_) | LayoutNode::Comment(_) | LayoutNode::Blank => {}
                }
            }
        }
        walk(&self.roots, out.as_mut());
        out
    }

    /// Every drawn [`Wire`] in the tree, in a pre-order walk — the order
    /// [`schematic_svg`](crate::schematic_svg) draws them and [`validate_wires`] reports
    /// them, so both are deterministic.
    pub fn wires(&self) -> Vec<&Wire> {
        let mut out = Vec::new();
        fn walk<'a>(nodes: &'a [LayoutNode], out: &mut Vec<&'a Wire>) {
            for n in nodes {
                match n {
                    LayoutNode::Wire(w) => out.push(w),
                    LayoutNode::Container(c) => walk(&c.children, out),
                    LayoutNode::Symbol(_) | LayoutNode::Comment(_) | LayoutNode::Blank => {}
                }
            }
        }
        walk(&self.roots, out.as_mut());
        out
    }
}

// ----------------------------------------------------------------------------
// Elaboration / validation (tier 1 diagnostics)
// ----------------------------------------------------------------------------

use crate::diagnostic::{Diagnostic, Location};

/// Validate an authored layout against the elaborated component universe. Two kinds of
/// finding, split like the rest of the codebase splits them (a fault aborts the commit;
/// a finding rides on a valid doc — see `diagnostic.rs`):
///
///   - **Hard `E_SCHEMATIC` errors** (returned): a `sym` whose comp path the *source*
///     never declares (a typo — unknown path), the same comp path placed by two `sym`
///     leaves (duplicate placement), and two sibling containers sharing a `name`
///     (duplicate sibling name — breaks GUI addressing). Collect-all: every offending
///     node is reported in one pass.
///
///   - **A `W_SCHEMATIC_UNPLACED` warning** (returned separately, for the caller to hang
///     on the [`ReconReport`](crate::doc::ReconReport) — the `W_FONT_LOAD` idiom): every
///     component *not* named by any `sym`, plus every `sym` whose path the source declared
///     but a false `if=` depopulated (Decision 21b DNP). Non-blocking; the view stays
///     total (§20c). Not an error, so it does **not** gate `is_clean`.
///
/// The **DNP distinction** (Decision 20c × 21b): a `sym` path in `dnp_dropped` is a
/// component the source *did* declare but a population conditional turned off — toggling
/// a variant must not hard-abort a commit, so it degrades to the unplaced bin (a warning)
/// exactly like a never-placed part, not an `E_SCHEMATIC`. Only a path the source does not
/// know at all is the typo case that aborts.
///
/// `component_ids` is the elaborated (populated) instance universe (keys of
/// `Doc::components`); `dnp_dropped` is the depopulated-path set from
/// [`crate::elaborate::Elaborated`]. Returns the hard errors (empty ⇒ clean) and the
/// sorted list of unplaced ids (never-placed populated parts + DNP-dropped placed paths).
pub fn validate(
    layout: &SchematicLayout,
    component_ids: &BTreeSet<EntityId>,
    dnp_dropped: &BTreeSet<String>,
) -> (Vec<Diagnostic>, Vec<EntityId>) {
    let mut errors = Vec::new();

    // Duplicate sibling container names, walked over the whole tree (siblings = the
    // children of one container, and the root list). Reported once per collision.
    fn check_names(nodes: &[LayoutNode], errors: &mut Vec<Diagnostic>) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for n in nodes {
            if let LayoutNode::Container(c) = n {
                if let Some(name) = &c.name
                    && !seen.insert(name.as_str())
                {
                    errors.push(Diagnostic::error(
                        "E_SCHEMATIC",
                        format!("duplicate sibling container name `{name}`"),
                        Location::None,
                    ));
                }
                check_names(&c.children, errors);
            }
        }
    }
    check_names(&layout.roots, &mut errors);

    // Symbol paths, in pre-order. Four cases:
    //   - populated (in `component_ids`): a real placement; duplicate placement is an error.
    //   - DNP-dropped (in `dnp_dropped`): the source declared it but `if=false` turned it
    //     off — NOT an error; collect it as unplaced so it warns and the view degrades.
    //   - unknown to the source entirely: a typo — hard `E_SCHEMATIC` abort.
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let mut dnp_placed: BTreeSet<EntityId> = BTreeSet::new();
    for path in layout.symbol_paths() {
        if component_ids.contains(&EntityId::new(path)) {
            if !placed.insert(path) {
                errors.push(Diagnostic::error(
                    "E_SCHEMATIC",
                    format!("component `{path}` is placed by more than one `sym`"),
                    Location::Entity(EntityId::new(path)),
                ));
            }
        } else if dnp_dropped.contains(path) {
            // Depopulated variant: degrade to unplaced, do not abort (§20c × 21b).
            dnp_placed.insert(EntityId::new(path));
        } else {
            errors.push(Diagnostic::error(
                "E_SCHEMATIC",
                format!("`sym {path}` names no component instance"),
                Location::Entity(EntityId::new(path)),
            ));
        }
    }

    // Unplaced (a warning, not an error), deterministic id order: every populated component
    // the tree never names, plus every DNP-dropped path a `sym` did name (so a placed but
    // depopulated part is still visibly accounted for). The union is a `BTreeSet` so the
    // result is sorted and dedup'd.
    let placed_ids: BTreeSet<EntityId> = placed.iter().map(|p| EntityId::new(*p)).collect();
    let mut unplaced: BTreeSet<EntityId> = component_ids
        .iter()
        .filter(|id| !placed_ids.contains(id))
        .cloned()
        .collect();
    unplaced.extend(dnp_placed);

    (errors, unplaced.into_iter().collect())
}

/// Validate the drawn wires (§20d) against the elaborated universe — a sibling of
/// [`validate`], kept separate because a wire needs the *part library* (to resolve pin
/// identities) and the *netlist* (to spot a wire drawn across two nets), which `validate`
/// does not. Wires are presentational, so their findings mirror the `sym` gate but never
/// touch the flow:
///
///   - **Hard `E_SCHEMATIC` errors** (returned first): an endpoint whose component path is
///     unknown to the source (a typo, exactly like an unknown `sym` path), or whose pin
///     selector names no pin on that component's part (a typo'd pin). Collect-all.
///   - **`W_SCHEMATIC_WIRE` warnings** (returned second): a wire endpoint on a
///     DNP-dropped component (the wire degrades like a `sym` — non-blocking, §20c × 21b),
///     and a wire whose two endpoints resolve onto *different* nets (a legal but honest
///     "your drawing disagrees with the netlist" signal — the net tag at each pin still
///     tells the truth). Both leave the doc clean.
///
/// `components` is the populated path→part universe; `lib` sizes/enumerates pins;
/// `dnp_dropped` is the depopulated-path set; `pin_net` maps a resolved
/// [`PinRef`](crate::doc::PinRef) to its net name (absent ⇒ the pin joins no net). The
/// wire order is the pre-order [`SchematicLayout::wires`] walk, so output is deterministic.
pub fn validate_wires(
    layout: &SchematicLayout,
    components: &BTreeMap<EntityId, String>,
    lib: &PartLib,
    dnp_dropped: &BTreeSet<String>,
    pin_net: &impl Fn(&crate::doc::PinRef) -> Option<String>,
) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Resolve one wire endpoint to the set of stored pin identities it names, emitting the
    // right diagnostic on the way. Returns `None` when the endpoint should be skipped for
    // the cross-net check (unknown comp/pin already errored, or a DNP-dropped comp warned).
    let resolve_end = |end: &WireEnd,
                       errors: &mut Vec<Diagnostic>,
                       warnings: &mut Vec<Diagnostic>|
     -> Option<Vec<crate::doc::PinRef>> {
        let cid = EntityId::new(end.comp.clone());
        let Some(part) = components.get(&cid) else {
            // Not a populated component: a DNP-dropped path degrades (like a sym); an
            // otherwise-unknown path is a hard typo.
            if dnp_dropped.contains(end.comp.as_str()) {
                warnings.push(
                    Diagnostic::warning(
                        "W_SCHEMATIC_WIRE",
                        format!(
                            "wire endpoint `{}.{}` is on `{}`, which an `if=` variant depopulated; the wire is not drawn",
                            end.comp, end.pin, end.comp
                        ),
                        Location::Entity(cid),
                    ),
                );
            } else {
                errors.push(Diagnostic::error(
                    "E_SCHEMATIC",
                    format!(
                        "wire endpoint `{}.{}` names no component instance",
                        end.comp, end.pin
                    ),
                    Location::Entity(cid),
                ));
            }
            return None;
        };
        let Some(def) = lib.get(part) else {
            // A populated component whose part is missing from the lib: the sym path already
            // renders as a min box; a wire on it can't resolve a pin, so skip it silently
            // (the missing part is its own upstream concern, not a wire error).
            return None;
        };
        let ids = def.resolve_selector(&end.pin);
        if ids.is_empty() {
            errors.push(Diagnostic::error(
                "E_SCHEMATIC",
                format!(
                    "wire endpoint `{}.{}` names no pin on part `{part}`",
                    end.comp, end.pin
                ),
                Location::Entity(cid),
            ));
            return None;
        }
        Some(
            ids.iter()
                .map(|id| crate::doc::PinRef::new(&cid, id))
                .collect(),
        )
    };

    for w in layout.wires() {
        let a = resolve_end(&w.a, &mut errors, &mut warnings);
        let b = resolve_end(&w.b, &mut errors, &mut warnings);
        // Cross-net check only when both endpoints resolved to real pins. Two endpoints
        // "agree" if they share any net (a multi-pad selector fans out; sharing one net is
        // enough to call the drawing honest). A wire where neither side joins any net is
        // silent — there is nothing to disagree with.
        if let (Some(a), Some(b)) = (a, b) {
            let nets_a: BTreeSet<String> = a.iter().filter_map(pin_net).collect();
            let nets_b: BTreeSet<String> = b.iter().filter_map(pin_net).collect();
            if !nets_a.is_empty() && !nets_b.is_empty() && nets_a.is_disjoint(&nets_b) {
                // Deterministic message: name the two nets in sorted order.
                let na = nets_a.iter().next().unwrap();
                let nb = nets_b.iter().next().unwrap();
                warnings.push(
                    Diagnostic::warning(
                        "W_SCHEMATIC_WIRE",
                        format!(
                            "wire `{}.{}` — `{}.{}` connects different nets (`{na}` vs `{nb}`); the drawn wire does not match the netlist",
                            w.a.comp, w.a.pin, w.b.comp, w.b.pin
                        ),
                        Location::None,
                    )
                    .with_help("wires are presentational; the net tag at each pin is the truth"),
                );
            }
        }
    }

    (errors, warnings)
}

// ----------------------------------------------------------------------------
// Symbol sizing (Decision 20e — boxes-with-pins)
// ----------------------------------------------------------------------------

/// The axis-aligned extent of a placed symbol, in nm: the box a Phase-2 renderer draws
/// exactly. `w`/`h` are the full width/height (the box is centered on the component
/// origin, so the half-extents are `w/2`, `h/2`). Kept as a separable value so the
/// renderer sizes identically to what reflow packs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub w: Nm,
    pub h: Nm,
}

/// Layout metrics for the box-with-pins symbol (Decision 20e). All integer nm; no floats
/// anywhere on the sizing path.
///
/// **Pin-side convention (documented, §20 "your call"):** pins split **left/right** by
/// declaration parity — even-indexed pins (0, 2, …) on the left edge, odd-indexed
/// (1, 3, …) on the right. Interface-port signals count as pins on the box edge and join
/// the same split, enumerated after the discrete pins (BTreeMap order — sorted by
/// `port` then `signal`). This is a *layout* convention only (the electrical identity is
/// unchanged); a richer left=inputs/right=outputs rule keys on `PinRole` and is a
/// follow-up. Box **height** grows with the busier side's pin count; box **width** with
/// the longest pin name plus the component-name header.
const PIN_PITCH: Nm = 2_540_000; // 2.54 mm — the classic 100-mil schematic pin grid.
const PIN_MARGIN: Nm = 2_540_000; // top/bottom padding inside the box, one pitch.
const NAME_CHAR_W: Nm = 700_000; // ~0.7 mm nominal advance per name character.
const SIDE_NAME_PAD: Nm = 2_540_000; // clearance between the two columns of pin names.
const MIN_BOX_W: Nm = 5_080_000; // a pinless / tiny part still gets a 2-pitch box.
const MIN_BOX_H: Nm = 5_080_000;

/// Every box-edge pin identity of a part, in the layout enumeration order: discrete pins
/// first (declaration order), then interface-port signals (`port.signal`, BTreeMap
/// order). The names are what widths key on; the count drives height. This is the single
/// definition of "what counts as a pin on the box edge" (§20 — interface ports count).
fn edge_pins(def: &PartDef) -> Vec<String> {
    let mut names: Vec<String> = def.pins.iter().map(|p| p.name.clone()).collect();
    for iface in def.interfaces.values() {
        for sig in iface.signals.keys() {
            names.push(sig.clone());
        }
    }
    names
}

/// Which edge of the symbol box a pin stub sits on (Decision 20e's parity split).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinSide {
    Left,
    Right,
}

/// One pin stub's placement on the symbol box, in the box's own frame (origin at the box
/// center, y-up) — everything [`schematic_svg`](crate::schematic_svg) needs to draw a stub
/// and its label/tag *exactly* where [`symbol_extent`] sized for it, without re-deriving
/// the parity split. `name` is the human label; `id` is the stored pin identity (pad
/// number, or `port.signal`) for the net-tag lookup — the [`PinRef`](crate::doc::PinRef)
/// vocabulary. `dy` is the stub's vertical offset from the box center (positive = up).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinSlot {
    pub name: String,
    pub id: String,
    pub side: PinSide,
    pub dy: Nm,
}

/// The pin stubs of a part, placed on the box edges exactly as [`symbol_extent`] sizes
/// them (Decision 20e): the same enumeration order ([`edge_pins`] — discrete pins, then
/// interface signals) and the same left/right parity split, so a renderer draws precisely
/// what reflow packed. Left and right columns each fill top-down from the box top, at
/// [`PIN_PITCH`] spacing starting [`PIN_MARGIN`] below the top edge. The box half-height is
/// derived from the busier side's count, identical to `symbol_extent`.
///
/// Returned in the [`edge_pins`] order (left/right interleaved by parity), so the output
/// is deterministic. Pairs with [`symbol_extent`] — call both on the same [`PartDef`].
pub fn pin_slots(def: &PartDef) -> Vec<PinSlot> {
    let names = edge_pins(def);
    let ids = edge_pin_ids(def);
    let n = names.len();
    let left = n.div_ceil(2);
    let right = n / 2;
    let side_count = left.max(right) as Nm;
    // Box half-height, matching `symbol_extent`'s `h` (before the MIN_BOX_H floor — the
    // stubs anchor to the pitch grid, not the floored box, which only grows the box, never
    // the pin spacing).
    let h = (side_count * PIN_PITCH + 2 * PIN_MARGIN).max(MIN_BOX_H);
    let half_h = h / 2;
    // The first stub sits PIN_MARGIN below the top edge; each subsequent one a pitch down.
    let stub_dy = |slot: Nm| half_h - PIN_MARGIN - slot * PIN_PITCH;

    let mut out = Vec::new();
    let (mut li, mut ri) = (0i64, 0i64);
    for (i, (name, id)) in names.into_iter().zip(ids).enumerate() {
        let (side, dy) = if i % 2 == 0 {
            let dy = stub_dy(li);
            li += 1;
            (PinSide::Left, dy)
        } else {
            let dy = stub_dy(ri);
            ri += 1;
            (PinSide::Right, dy)
        };
        out.push(PinSlot { name, id, side, dy });
    }
    out
}

/// The stored pin **identity** of each edge pin, in [`edge_pins`] order: a pad `number`
/// for a discrete pin, `port.signal` for an interface signal — the
/// [`PinRef`](crate::doc::PinRef) vocabulary the netlist keys on. Parallel to
/// [`edge_pins`] (the display names), so the two zip.
fn edge_pin_ids(def: &PartDef) -> Vec<String> {
    let mut ids: Vec<String> = def.pins.iter().map(|p| p.number.clone()).collect();
    for (port, iface) in &def.interfaces {
        for sig in iface.signals.keys() {
            ids.push(format!("{port}.{sig}"));
        }
    }
    ids
}

/// Size the box-with-pins for a part (Decision 20e). Separable from packing so Phase 2's
/// renderer draws exactly this. Pure integer arithmetic.
pub fn symbol_extent(def: &PartDef) -> Extent {
    let names = edge_pins(def);
    let n = names.len();
    // Split by parity: left = even indices, right = odd. Height keyed on the busier side.
    let left = n.div_ceil(2); // indices 0,2,4… -> ceil(n/2)
    let right = n / 2; // indices 1,3,5…    -> floor(n/2)
    let side = left.max(right) as Nm;
    let h = (side * PIN_PITCH + 2 * PIN_MARGIN).max(MIN_BOX_H);

    // Width: the widest left-name + widest right-name + a center gap for the header, with
    // a floor at the component name's own width. Char widths are a nominal fixed advance
    // (no font metrics at layout time — the renderer owns exact glyph advance).
    let name_w = |s: &str| s.chars().count() as Nm * NAME_CHAR_W;
    let mut left_w = 0;
    let mut right_w = 0;
    for (i, nm) in names.iter().enumerate() {
        if i % 2 == 0 {
            left_w = left_w.max(name_w(nm));
        } else {
            right_w = right_w.max(name_w(nm));
        }
    }
    let pins_w = left_w + SIDE_NAME_PAD + right_w;
    let header_w = name_w(&def.name);
    let w = pins_w.max(header_w).max(MIN_BOX_W);

    Extent { w, h }
}

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

/// Gap between the placed extent and the unplaced bin, and between bin cells.
const BIN_GAP: Nm = 5_080_000; // 2 pitches of breathing room.

/// The minimum-box extent used for a component whose part is missing from the lib — the
/// view stays total (§20c) even for a dangling part.
const MIN_EXTENT: Extent = Extent {
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
pub fn reflow(
    layout: &SchematicLayout,
    components: &BTreeMap<EntityId, String>,
    lib: &PartLib,
) -> BTreeMap<EntityId, Placement> {
    // Extent of an instance path: look up its part in the universe, then size via the lib;
    // an unknown path or missing part degrades to the min box (totality).
    let extent_of = |path: &str| -> Extent {
        components
            .get(&EntityId::new(path))
            .and_then(|part| lib.get(part))
            .map(symbol_extent)
            .unwrap_or(MIN_EXTENT)
    };
    // A symbol's laid extent applies the authored rot's 90/270 swap on top of the box.
    let sized = |sym: &Symbol| -> Extent {
        let e = extent_of(&sym.path);
        if sym.swaps_extent() {
            Extent { w: e.h, h: e.w }
        } else {
            e
        }
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
        place_bin(&unplaced, &extent_of, placed_extent, &mut out);
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
fn measure(node: &LayoutNode, sized: &impl Fn(&Symbol) -> Extent) -> Extent {
    match node {
        LayoutNode::Symbol(s) => sized(s),
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
fn measure_container(c: &Container, sized: &impl Fn(&Symbol) -> Extent) -> Extent {
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
    sized: &impl Fn(&Symbol) -> Extent,
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
        place_node(child, slot, *ce, sized, out);
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
/// A symbol lands centered in its slot (the slot *is* its box — [`measure`] does not
/// inflate for the pinned offset), then shifted by its pinned `dx`/`dy`. A container
/// recurses.
fn place_node(
    node: &LayoutNode,
    slot: Point,
    ext: Extent,
    sized: &impl Fn(&Symbol) -> Extent,
    out: &mut BTreeMap<EntityId, Placement>,
) {
    match node {
        LayoutNode::Symbol(s) => {
            let box_ext = sized(s);
            // Center of the slot (top-left corner minus half-extent in y-up space).
            let center = Point {
                x: slot.x + ext.w / 2 + s.dx,
                y: slot.y - ext.h / 2 + s.dy,
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
/// below the placed content (§20c). Cells are uniform (widest × tallest unplaced box) so
/// the grid is a clean lattice; ids fill row-major in sorted order. Deterministic.
fn place_bin(
    unplaced: &[&EntityId],
    extent_of: &impl Fn(&str) -> Extent,
    placed: Extent,
    out: &mut BTreeMap<EntityId, Placement>,
) {
    const BIN_COLS: usize = 8;
    let exts: Vec<Extent> = unplaced.iter().map(|id| extent_of(id.as_str())).collect();
    let cell_w = exts.iter().map(|e| e.w).max().unwrap_or(MIN_BOX_W);
    let cell_h = exts.iter().map(|e| e.h).max().unwrap_or(MIN_BOX_H);
    // The bin's top edge is one gap below the placed content's bottom (origin y = 0, so
    // the content spans y ∈ [−placed.h, 0]).
    let top = -placed.h - BIN_GAP;
    for (i, (id, e)) in unplaced.iter().zip(&exts).enumerate() {
        let col = (i % BIN_COLS) as Nm;
        let row = (i / BIN_COLS) as Nm;
        // Cell top-left corner, then center the box in its cell.
        let slot = Point {
            x: col * (cell_w + BIN_GAP),
            y: top - row * (cell_h + BIN_GAP),
        };
        out.insert(
            (*id).clone(),
            Placement {
                center: Point {
                    x: slot.x + cell_w / 2,
                    y: slot.y - cell_h / 2,
                },
                extent: *e,
            },
        );
    }
}

// ----------------------------------------------------------------------------
// Tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::part_library;

    fn sym(path: &str) -> LayoutNode {
        LayoutNode::Symbol(Symbol {
            path: path.into(),
            rot: Orient::IDENTITY,
            dx: 0,
            dy: 0,
        })
    }

    fn row(children: Vec<LayoutNode>) -> LayoutNode {
        LayoutNode::Container(Container {
            dir: Direction::Row,
            name: None,
            gap: 0,
            align: Align::Start,
            children,
        })
    }

    fn column(children: Vec<LayoutNode>) -> LayoutNode {
        LayoutNode::Container(Container {
            dir: Direction::Column,
            name: None,
            gap: 0,
            align: Align::Start,
            children,
        })
    }

    /// A component universe (path -> part) from `(path, part)` pairs.
    fn universe(pairs: &[(&str, &str)]) -> BTreeMap<EntityId, String> {
        pairs
            .iter()
            .map(|(p, part)| (EntityId::new(*p), part.to_string()))
            .collect()
    }

    fn ids(pairs: &[(&str, &str)]) -> BTreeSet<EntityId> {
        pairs.iter().map(|(p, _)| EntityId::new(*p)).collect()
    }

    /// A DNP-dropped path set from string slices.
    fn dnp(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    // --- sizing -------------------------------------------------------------

    #[test]
    fn symbol_extent_grows_with_pin_count() {
        let lib = part_library();
        let cap = symbol_extent(&lib["Cap"]); // 2 pins
        let ldo = symbol_extent(&lib["LDO"]); // 3 pins
        // More pins on a side => taller box (3 pins: 2 left, 1 right => 2-high side).
        assert!(ldo.h >= cap.h);
        // Every box is at least the minimum.
        assert!(cap.w >= MIN_BOX_W && cap.h >= MIN_BOX_H);
    }

    #[test]
    fn pin_slots_split_by_parity_and_fit_the_box() {
        let lib = part_library();
        let def = &lib["MCU"]; // 2 discrete pins + uart(tx,rx) = 4 edge pins.
        let slots = pin_slots(def);
        assert_eq!(slots.len(), 4);
        // Parity split: even indices left, odd right (2 each).
        assert_eq!(slots.iter().filter(|s| s.side == PinSide::Left).count(), 2);
        assert_eq!(slots.iter().filter(|s| s.side == PinSide::Right).count(), 2);
        // Every stub sits within the box the sizer produced (|dy| ≤ half-height).
        let e = symbol_extent(def);
        for s in &slots {
            assert!(s.dy.abs() <= e.h / 2, "stub {s:?} outside box h={}", e.h);
        }
    }

    #[test]
    fn interface_signals_count_as_edge_pins() {
        let lib = part_library();
        // MCU: 2 discrete pins + a uart interface (tx, rx) = 4 edge pins.
        assert_eq!(edge_pins(&lib["MCU"]).len(), 4);
    }

    // --- packing ------------------------------------------------------------

    #[test]
    fn row_advances_along_x_column_along_neg_y() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);

        let r = SchematicLayout {
            roots: vec![row(vec![sym("C1"), sym("C2")])],
        };
        let pr = reflow(&r, &u, &lib);
        // In a row, C2 sits to the right of C1 (greater x), same y.
        assert!(pr[&EntityId::new("C2")].center.x > pr[&EntityId::new("C1")].center.x);
        assert_eq!(
            pr[&EntityId::new("C1")].center.y,
            pr[&EntityId::new("C2")].center.y
        );

        let c = SchematicLayout {
            roots: vec![column(vec![sym("C1"), sym("C2")])],
        };
        let pc = reflow(&c, &u, &lib);
        // In a column, C2 sits below C1 (lesser y), same x.
        assert!(pc[&EntityId::new("C2")].center.y < pc[&EntityId::new("C1")].center.y);
        assert_eq!(
            pc[&EntityId::new("C1")].center.x,
            pc[&EntityId::new("C2")].center.x
        );
    }

    #[test]
    fn gap_widens_spacing() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
        let mk = |gap: Nm| SchematicLayout {
            roots: vec![LayoutNode::Container(Container {
                dir: Direction::Row,
                name: None,
                gap,
                align: Align::Start,
                children: vec![sym("C1"), sym("C2")],
            })],
        };
        let close = reflow(&mk(0), &u, &lib);
        let far = reflow(&mk(10 * 1_000_000), &u, &lib);
        let dx0 = close[&EntityId::new("C2")].center.x - close[&EntityId::new("C1")].center.x;
        let dx1 = far[&EntityId::new("C2")].center.x - far[&EntityId::new("C1")].center.x;
        assert_eq!(dx1 - dx0, 10 * 1_000_000);
    }

    #[test]
    fn align_shifts_cross_axis() {
        let lib = part_library();
        // A row with a tall MCU and a short Cap: alignment moves the Cap's cross (y) pos.
        let u = universe(&[("U1", "MCU"), ("C1", "Cap")]);
        let mk = |align: Align| SchematicLayout {
            roots: vec![LayoutNode::Container(Container {
                dir: Direction::Row,
                name: None,
                gap: 0,
                align,
                children: vec![sym("U1"), sym("C1")],
            })],
        };
        let start = reflow(&mk(Align::Start), &u, &lib);
        let center = reflow(&mk(Align::Center), &u, &lib);
        let end = reflow(&mk(Align::End), &u, &lib);
        let cap_y = |m: &BTreeMap<EntityId, Placement>| m[&EntityId::new("C1")].center.y;
        // Start puts the short box at the top; End at the bottom; Center between.
        assert!(cap_y(&start) > cap_y(&center));
        assert!(cap_y(&center) > cap_y(&end));
    }

    #[test]
    fn nested_containers_size_to_content() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap"), ("C3", "Cap")]);
        // A column whose first row holds C1,C2 and second row holds C3. All three placed.
        let layout = SchematicLayout {
            roots: vec![column(vec![
                row(vec![sym("C1"), sym("C2")]),
                row(vec![sym("C3")]),
            ])],
        };
        let p = reflow(&layout, &u, &lib);
        assert_eq!(p.len(), 3);
        // The second row (C3) sits below the first (C1/C2).
        assert!(p[&EntityId::new("C3")].center.y < p[&EntityId::new("C1")].center.y);
    }

    #[test]
    fn pinned_offset_shifts_symbol() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap")]);
        let base = SchematicLayout {
            roots: vec![row(vec![sym("C1")])],
        };
        let shifted = SchematicLayout {
            roots: vec![row(vec![LayoutNode::Symbol(Symbol {
                path: "C1".into(),
                rot: Orient::IDENTITY,
                dx: 3_000_000,
                dy: -2_000_000,
            })])],
        };
        let pb = reflow(&base, &u, &lib);
        let ps = reflow(&shifted, &u, &lib);
        let b = pb[&EntityId::new("C1")].center;
        let s = ps[&EntityId::new("C1")].center;
        // dx/dy applied on top of the (unchanged, centered) flow position.
        assert_eq!(s.x - b.x, 3_000_000);
        assert_eq!(s.y - b.y, -2_000_000);
    }

    #[test]
    fn rot_swaps_extent() {
        let lib = part_library();
        let u = universe(&[("U1", "MCU")]);
        let upright = symbol_extent(&lib["MCU"]);
        let layout = SchematicLayout {
            roots: vec![row(vec![LayoutNode::Symbol(Symbol {
                path: "U1".into(),
                rot: Orient::from_deg(90).unwrap(),
                dx: 0,
                dy: 0,
            })])],
        };
        let p = reflow(&layout, &u, &lib);
        let e = p[&EntityId::new("U1")].extent;
        assert_eq!(e.w, upright.h);
        assert_eq!(e.h, upright.w);
    }

    // --- unplaced bin -------------------------------------------------------

    #[test]
    fn unplaced_components_land_in_the_bin() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap"), ("C3", "Cap")]);
        // Only C1 is placed; C2 and C3 fall to the bin.
        let layout = SchematicLayout {
            roots: vec![row(vec![sym("C1")])],
        };
        let p = reflow(&layout, &u, &lib);
        assert_eq!(p.len(), 3); // totality: every component has a coordinate.
        // The bin sits below the placed content (negative y region well under C1).
        assert!(p[&EntityId::new("C2")].center.y < p[&EntityId::new("C1")].center.y);
        assert!(p[&EntityId::new("C3")].center.y < p[&EntityId::new("C1")].center.y);
    }

    #[test]
    fn empty_layout_puts_everything_in_the_bin() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
        let p = reflow(&SchematicLayout::default(), &u, &lib);
        assert_eq!(p.len(), 2);
    }

    #[test]
    fn missing_part_still_gets_a_placement() {
        let lib = part_library();
        // A component whose part is not in the lib: the view stays total (min box).
        let u = universe(&[("X1", "NoSuchPart")]);
        let p = reflow(&SchematicLayout::default(), &u, &lib);
        assert_eq!(p[&EntityId::new("X1")].extent, MIN_EXTENT);
    }

    // --- determinism --------------------------------------------------------

    #[test]
    fn reflow_is_deterministic() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("U1", "MCU"), ("L1", "LDO"), ("C2", "Cap")]);
        let layout = SchematicLayout {
            roots: vec![column(vec![
                row(vec![sym("U1"), sym("L1")]),
                sym("C1"),
                // C2 unplaced -> bin.
            ])],
        };
        // Two runs must be byte-equal. BTreeMap iteration is deterministic, so a
        // Debug-rendered dump is a faithful byte-level proxy for the placement set.
        let dump = |m: &BTreeMap<EntityId, Placement>| format!("{m:?}");
        let a = reflow(&layout, &u, &lib);
        let b = reflow(&layout, &u, &lib);
        assert_eq!(dump(&a), dump(&b));
        assert_eq!(a, b);
    }

    // --- validation ---------------------------------------------------------

    #[test]
    fn unknown_sym_path_is_an_error() {
        let layout = SchematicLayout {
            roots: vec![row(vec![sym("C1"), sym("NOPE")])],
        };
        let (errors, _) = validate(&layout, &ids(&[("C1", "Cap")]), &dnp(&[]));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, "E_SCHEMATIC");
    }

    #[test]
    fn duplicate_sym_is_an_error() {
        let layout = SchematicLayout {
            roots: vec![row(vec![sym("C1"), sym("C1")])],
        };
        let (errors, _) = validate(&layout, &ids(&[("C1", "Cap")]), &dnp(&[]));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("more than one"));
    }

    #[test]
    fn duplicate_sibling_name_is_an_error() {
        let named = |name: &str| {
            LayoutNode::Container(Container {
                dir: Direction::Row,
                name: Some(name.into()),
                gap: 0,
                align: Align::Start,
                children: vec![],
            })
        };
        let layout = SchematicLayout {
            roots: vec![named("power"), named("power")],
        };
        let (errors, _) = validate(&layout, &ids(&[]), &dnp(&[]));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("duplicate sibling"));
    }

    #[test]
    fn same_name_in_different_scopes_is_ok() {
        // Two containers named "col" but in different parents: not siblings, so allowed.
        let inner = |name: &str| {
            LayoutNode::Container(Container {
                dir: Direction::Column,
                name: Some(name.into()),
                gap: 0,
                align: Align::Start,
                children: vec![],
            })
        };
        let layout = SchematicLayout {
            roots: vec![row(vec![inner("col")]), row(vec![inner("col")])],
        };
        let (errors, _) = validate(&layout, &ids(&[]), &dnp(&[]));
        assert!(errors.is_empty());
    }

    #[test]
    fn unplaced_reported_as_warning_set() {
        let layout = SchematicLayout {
            roots: vec![row(vec![sym("C1")])],
        };
        let (errors, unplaced) =
            validate(&layout, &ids(&[("C1", "Cap"), ("C2", "Cap")]), &dnp(&[]));
        assert!(errors.is_empty());
        assert_eq!(unplaced, vec![EntityId::new("C2")]);
    }

    #[test]
    fn dnp_dropped_sym_degrades_to_unplaced_not_error() {
        // A `sym` pointing at a component the source declared but a false `if=`
        // depopulated must NOT be an E_SCHEMATIC abort (Decision 20c × 21b): it degrades to
        // the unplaced warning, like a never-placed part. Only a truly unknown path aborts.
        let layout = SchematicLayout {
            roots: vec![row(vec![sym("C1"), sym("C2")])],
        };
        // C1 is populated; C2 was dropped by `if=false`. No component universe entry for C2.
        let (errors, unplaced) = validate(&layout, &ids(&[("C1", "Cap")]), &dnp(&["C2"]));
        assert!(errors.is_empty(), "DNP-dropped placed sym must not error");
        // C2 surfaces as unplaced (so it warns), and is absent from the placed set.
        assert_eq!(unplaced, vec![EntityId::new("C2")]);
    }

    // --- wire validation ----------------------------------------------------

    fn wire(a: (&str, &str), b: (&str, &str)) -> LayoutNode {
        LayoutNode::Wire(Wire {
            a: WireEnd {
                comp: a.0.into(),
                pin: a.1.into(),
            },
            b: WireEnd {
                comp: b.0.into(),
                pin: b.1.into(),
            },
            waypoints: vec![],
        })
    }

    /// A pin→net lookup from `(comp, pin, net)` triples.
    fn nets(triples: &[(&str, &str, &str)]) -> impl Fn(&crate::doc::PinRef) -> Option<String> {
        let map: BTreeMap<(String, String), String> = triples
            .iter()
            .map(|(c, p, n)| ((c.to_string(), p.to_string()), n.to_string()))
            .collect();
        move |pr: &crate::doc::PinRef| map.get(&(pr.comp.to_string(), pr.pin.clone())).cloned()
    }

    #[test]
    fn wire_to_real_pins_on_same_net_is_silent() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
        let layout = SchematicLayout {
            roots: vec![wire(("C1", "p1"), ("C2", "p1"))],
        };
        let net = nets(&[("C1", "p1", "N1"), ("C2", "p1", "N1")]);
        let (errors, warnings) = validate_wires(&layout, &u, &lib, &dnp(&[]), &net);
        assert!(errors.is_empty());
        assert!(warnings.is_empty(), "same-net wire is honest: {warnings:?}");
    }

    #[test]
    fn wire_across_two_nets_warns_not_errors() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap"), ("C2", "Cap")]);
        let layout = SchematicLayout {
            roots: vec![wire(("C1", "p1"), ("C2", "p1"))],
        };
        // The two pins are on *different* nets: legal but honest disagreement (§20d).
        let net = nets(&[("C1", "p1", "N1"), ("C2", "p1", "N2")]);
        let (errors, warnings) = validate_wires(&layout, &u, &lib, &dnp(&[]), &net);
        assert!(errors.is_empty(), "cross-net is a warning, not an error");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "W_SCHEMATIC_WIRE");
        assert!(warnings[0].message.contains("different nets"));
    }

    #[test]
    fn wire_unknown_comp_or_pin_is_an_error() {
        let lib = part_library();
        let u = universe(&[("C1", "Cap")]);
        // Unknown component `NOPE`, and a real component with a bogus pin.
        let layout = SchematicLayout {
            roots: vec![
                wire(("C1", "p1"), ("NOPE", "p1")),
                wire(("C1", "bogus"), ("C1", "p2")),
            ],
        };
        let (errors, _) = validate_wires(&layout, &u, &lib, &dnp(&[]), &nets(&[]));
        assert_eq!(errors.len(), 2);
        assert!(errors.iter().all(|e| e.code == "E_SCHEMATIC"));
    }

    #[test]
    fn wire_on_dnp_dropped_comp_degrades_to_warning() {
        let lib = part_library();
        // C2 is DNP-dropped (declared, then `if=false`). A wire onto it must not error — it
        // degrades like a `sym` (§20c × 21b), a non-blocking W_SCHEMATIC_WIRE.
        let u = universe(&[("C1", "Cap")]);
        let layout = SchematicLayout {
            roots: vec![wire(("C1", "p1"), ("C2", "p1"))],
        };
        let (errors, warnings) = validate_wires(&layout, &u, &lib, &dnp(&["C2"]), &nets(&[]));
        assert!(
            errors.is_empty(),
            "DNP-dropped wire endpoint must not error"
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "W_SCHEMATIC_WIRE");
        assert!(warnings[0].message.contains("depopulated"));
    }

    #[test]
    fn unknown_path_still_aborts_even_with_dnp_set() {
        // A typo'd path (unknown to both the populated universe AND the DNP-dropped set)
        // stays a hard error, even when some other path is legitimately DNP-dropped.
        let layout = SchematicLayout {
            roots: vec![row(vec![sym("TYPO"), sym("C2")])],
        };
        let (errors, _) = validate(&layout, &ids(&[("C1", "Cap")]), &dnp(&["C2"]));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("TYPO"));
    }
}
