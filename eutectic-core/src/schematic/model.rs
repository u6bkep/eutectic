//! The authored layout tree (tier 1): the nested-container model and its tree-walk
//! accessors. A plain data model (no mutation methods) so the future GUI can CRUD it
//! through the command algebra.

use crate::doc::{Nm, Orient, Point};

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
    pub(crate) fn swaps_extent(&self) -> bool {
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

    /// Every [`Symbol`] leaf in the tree, in a pre-order walk (the nodes behind
    /// [`symbol_paths`](Self::symbol_paths)) — for callers that need the authored
    /// `rot`/`dx`/`dy`, not just the path (e.g. [`validate`]'s def-instance ignored-attr
    /// check). Same deterministic order as `symbol_paths`.
    pub(crate) fn symbols(&self) -> Vec<&Symbol> {
        let mut out = Vec::new();
        fn walk<'a>(nodes: &'a [LayoutNode], out: &mut Vec<&'a Symbol>) {
            for n in nodes {
                match n {
                    LayoutNode::Symbol(s) => out.push(s),
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
