//! Part library: typed pins and typed interfaces.
//!
//! This is where "make the serial-wire swap unrepresentable" lives. A connection
//! between two devices is made at the *interface* level, and the interface type
//! itself encodes how two instances mate (UART crosses tx<->rx). A designer never
//! wires individual signals, so connecting tx-to-tx is not expressible.

use crate::doc::{Component, MM, Nm, Point};
use crate::geom;
use crate::geom::Shape2D;
use crate::part::Dir::*;
use std::collections::BTreeMap;

/// Which copper layer(s) a pad's copper occupies. SMD pads sit on one outer layer;
/// a plated through-hole's copper is `Through` (top + bottom, conceptually a barrel
/// between). The board stackup resolves these to real z when the pad is placed
/// (`geom::Stackup`); this is the layer-relative, stackup-independent form a
/// reusable footprint carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadLayers {
    Top,
    Bottom,
    Through,
}

/// A drilled hole in a pad (a [`geom::Role::Void`](crate::geom::Role) once placed),
/// in **component-local** coordinates — round, or a slot between two points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drill {
    Round { d: Nm },
    Slot { a: Point, b: Point, d: Nm },
}

/// One copper region of a pad: a real [`Shape2D`] (so a custom/compound pad is a
/// *union* of these — the BMP581 case) on a set of layers, in **component-local**
/// coordinates (same frame as [`PinDef::offset`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadCopper {
    pub shape: Shape2D,
    pub layers: PadLayers,
}

/// The physical copper + drill geometry of a pad, attached to a [`PinDef`], in
/// component-local coordinates. `copper` is a union of regions (a simple pad has
/// one; a compound pad has several); `drill` is the optional hole. Unlike the old
/// render-only `Pad`, this is the *real* geometry — render uses it now, and DRC /
/// the router consume it once migrated (it is the honest copper extent, no longer a
/// point). World coordinates come from the component's position + orientation
/// (an [`Orient`](crate::doc::Orient) quaternion), applied with [`Shape2D::map_points`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PadGeo {
    pub copper: Vec<PadCopper>,
    pub drill: Option<Drill>,
}

/// Signal/pin electrical direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    Bidir,
}

/// Electrical role of a pin, used by ERC (which is just a typecheck over roles).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PinRole {
    PowerIn,
    PowerOut,
    Output,
    Input,
    Bidir,
    Passive,
}

impl PinRole {
    /// Does this role actively drive a net?
    pub fn is_driver(self) -> bool {
        matches!(self, PinRole::PowerOut | PinRole::Output)
    }
    fn from_dir(d: Dir) -> PinRole {
        match d {
            Out => PinRole::Output,
            In => PinRole::Input,
            Bidir => PinRole::Bidir,
        }
    }
}

/// A discrete pin on a part.
///
/// `name` vs `number`: the **functional name** (`GPIO0`, `VDD`, `SWCLK`) is the
/// human/agent-facing *selector* humans reference; the pad **number** (`12`, `MP`)
/// is the geometry/manufacturing key, the join key pairing a symbol pin with a
/// footprint pad, **and the stable identity stored in a [`PinRef`]**. Names repeat
/// (six pads named `IOVDD`); numbers are unique within a part, so identity keys on
/// the number. A name fans out to its pads via
/// [`resolve_selector`](PartDef::resolve_selector); `pin_role`/`pin_offset` resolve
/// the resulting *number*. For parts with no functional naming (a raw footprint
/// import, or the toy `part_library`) the two coincide — `number` defaults to
/// `name` via the [`pin`] constructor.
///
/// [`PinRef`]: crate::doc::PinRef
#[derive(Clone, Debug)]
pub struct PinDef {
    pub name: String,
    /// Pad/manufacturing number used as the symbol↔footprint join key. Defaults to
    /// `name` when there is no distinct numbering.
    pub number: String,
    pub role: PinRole,
    /// Local position of the pin relative to the component origin, in nm. Combined
    /// with the component's position + orientation to get a world position.
    pub offset: Point,
    /// Optional real copper + drill geometry ([`PadGeo`]). `Some` for an imported
    /// footprint pad; `None` for the toy `part_library` pins, which carry no
    /// footprint. This is the honest copper extent (render uses it; DRC/router
    /// consume it once migrated) — no longer a render-only simplification.
    pub pad: Option<PadGeo>,
}

/// A typed interface (e.g. UART). Defined once; encodes the correct mating so
/// that connecting two instances can never be wired backwards.
#[derive(Clone, Debug)]
pub struct InterfaceDef {
    pub type_name: String,
    /// signal name -> direction
    pub signals: BTreeMap<String, Dir>,
    /// signal name -> local position relative to the component origin, in nm.
    /// Carried alongside `signals` so an interface port's pins have geometry just
    /// like discrete pins do.
    pub offsets: BTreeMap<String, Point>,
    /// how to mate two instances: (signal on side A, signal on side B).
    /// For UART: (tx,rx) and (rx,tx) — the crossing is baked in.
    pub mate: Vec<(String, String)>,
}

/// A part definition: discrete pins + named interface ports.
#[derive(Clone, Debug)]
pub struct PartDef {
    pub name: String,
    pub pins: Vec<PinDef>,
    pub interfaces: BTreeMap<String, InterfaceDef>,
}

impl PartDef {
    /// Resolve the electrical role of a *stored pin identity* (see [`PinRef`]):
    /// a pad **number** for a discrete pin, or `port.signal` for an interface
    /// signal. Pad numbers are unique within a part, so this is unambiguous —
    /// unlike functional names, which repeat (six `IOVDD` pads share a name but
    /// have distinct numbers). Use [`resolve_selector`](Self::resolve_selector) to
    /// turn a user-facing name into the identities this resolves.
    ///
    /// [`PinRef`]: crate::doc::PinRef
    pub fn pin_role(&self, id: &str) -> Option<PinRole> {
        if let Some((port, sig)) = id.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.signals.get(sig).copied().map(PinRole::from_dir)
        } else {
            self.pins.iter().find(|p| p.number == id).map(|p| p.role)
        }
    }

    /// Resolve a *stored pin identity* to its local offset from the component
    /// origin. Identity semantics match [`pin_role`](Self::pin_role): a pad number
    /// for a discrete pin, or `port.signal` for an interface signal.
    pub fn pin_offset(&self, id: &str) -> Option<Point> {
        if let Some((port, sig)) = id.split_once('.') {
            let iface = self.interfaces.get(port)?;
            iface.offsets.get(sig).copied()
        } else {
            self.pins.iter().find(|p| p.number == id).map(|p| p.offset)
        }
    }

    /// Resolve a *connection selector* (a user/agent-facing pin reference) to the
    /// set of stable pin identities it names — the pad **numbers** to store as
    /// [`PinRef`]s. This is the one place a functional name fans out to physical
    /// pads, which is what keeps a multi-pad power rail (six `IOVDD`) from
    /// collapsing to a single member.
    ///
    /// Resolution order:
    /// - `port.signal` (contains `.`) → an interface signal: returns that single
    ///   identity if the port and signal exist, else empty.
    /// - otherwise match by functional **name** first (so `IOVDD` → every IOVDD
    ///   pad's number); if no name matches, fall back to matching a pad **number**
    ///   directly (so `30` / `MP` selects that one pad).
    ///
    /// An **empty** result means the selector names nothing on this part — a typo
    /// or a role gap. Callers must treat that as an error, never a silent no-op.
    /// The fanout is scoped to this one part: a name never reaches across instances.
    ///
    /// [`PinRef`]: crate::doc::PinRef
    pub fn resolve_selector(&self, sel: &str) -> Vec<String> {
        if let Some((port, sig)) = sel.split_once('.') {
            return match self.interfaces.get(port) {
                Some(iface) if iface.signals.contains_key(sig) => vec![sel.to_string()],
                _ => Vec::new(),
            };
        }
        let by_name: Vec<String> = self
            .pins
            .iter()
            .filter(|p| p.name == sel)
            .map(|p| p.number.clone())
            .collect();
        if !by_name.is_empty() {
            return by_name;
        }
        // Fall back to a direct pad-number reference.
        self.pins
            .iter()
            .filter(|p| p.number == sel)
            .map(|p| p.number.clone())
            .collect()
    }
}

/// Absolute (world) position of a pin on a placed component instance:
/// `component position + orient.apply(local pin offset)`. Exact for the
/// lattice-symmetry orientations (cardinals + flips), correctly-rounded otherwise
/// (see [`Orient::apply`](crate::doc::Orient::apply)). Returns `None` if the pin is
/// unknown.
pub fn pin_world(comp: &Component, def: &PartDef, pin: &str) -> Option<Point> {
    let off = def.pin_offset(pin)?;
    let r = comp.orient.apply(off);
    Some(Point {
        x: comp.pos.value.x + r.x,
        y: comp.pos.value.y + r.y,
    })
}

/// Lift a component-local point into world space on a placed component: apply the
/// orientation, translate to the component position. Exact for cardinals/flips,
/// correctly-rounded otherwise.
pub fn to_world(comp: &Component, p: Point) -> Point {
    let r = comp.orient.apply(p);
    Point {
        x: comp.pos.value.x + r.x,
        y: comp.pos.value.y + r.y,
    }
}

/// World-frame copper shape of a pad region on a placed component.
pub fn pad_copper_world(comp: &Component, c: &PadCopper) -> Shape2D {
    c.shape.map_points(|p| to_world(comp, p))
}

impl PinDef {
    /// World-frame physical features for this pin's pad: each copper region as a
    /// [`Role::Conductor`](geom::Role) prism on its layer's z; a solder-mask opening
    /// per copper region as a [`Role::Void`](geom::Role) prism (the copper expanded by
    /// [`geom::MASK_EXPANSION`]) at its side's mask slab z; plus the drill (if any) as a
    /// [`Role::Void`](geom::Role) prism spanning the *full* stackup. Empty if the pin
    /// has no pad.
    ///
    /// The mask opening deletes mask material where the pad is exposed (Decision 13 — an
    /// opening is a `Void` at mask z, not a negative layer): a surface pad opens its
    /// resolved side's mask, a through pad opens both. The mask slab is found by
    /// **role and z-position** ([`Stackup::top_mask`]/[`Stackup::bottom_mask`] — the
    /// `Role::Mask` slab immediately outboard of the outer copper), respecting the flip;
    /// a custom-named mask slab is opened just the same, and a side with no mask slab
    /// opens nothing. These `Void`s are not copper, so the DRC copper producer / the
    /// Gerber copper path drop them exactly as they drop the drill `Void`.
    ///
    /// The component's position + cardinal [`Orient`](crate::doc::Orient) place the
    /// geometry — copper via [`pad_copper_world`] (the pad's local offset is already
    /// baked into the copper [`Shape2D`]); the drill is built in component-local
    /// coords centred on the pad centre ([`PinDef::offset`] for a round drill, the
    /// stored slot endpoints for a slot — both in `offset`'s frame) and mapped with
    /// the same [`to_world`] transform. The [`Stackup`](geom::Stackup) resolves the
    /// layer-relative [`PadLayers`] to absolute z: `Top`/`Bottom` to the outer copper
    /// z, `Through` **fanned out** to one conductor feature per copper slab (the
    /// "annulus on every copper layer" semantics). Features whose z is degenerate in
    /// the stackup (a missing accessor) are skipped.
    ///
    /// This is the [`PadGeo`]-derives-`Feature`s fold of the geometry-model
    /// convergence (docs/geometry-model-convergence.md, Decision 12): the compact
    /// `PadGeo` stays stored on the pin; the features are the derived view. Purely
    /// additive — it does not alter or replace any existing geometry.
    pub fn pad_features(&self, comp: &Component, stackup: &geom::Stackup) -> Vec<geom::Feature> {
        let Some(pad) = &self.pad else {
            return Vec::new();
        };
        // A flipped (bottom-side) component swaps its outer-layer copper: a `Top` pad
        // lands on the board bottom and vice-versa. Derived from the orientation — no
        // side flag. (The copper *shape* is already flipped by `pad_copper_world`'s
        // `apply`; only the layer assignment needs swapping. `Through` is unaffected.)
        let flipped = comp.orient.is_bottom();
        let mut features = Vec::new();
        for cu in &pad.copper {
            let world = pad_copper_world(comp, cu);
            // Solder-mask opening: the pad copper, expanded by the mask margin, deletes
            // mask material on the side(s) it is exposed (Decision 13 — an opening is a
            // `Void` at mask z, not a negative layer). The mask slab is resolved by
            // **role + z-position** (the `Role::Mask` slab immediately outboard of the
            // outer copper on the pad's resolved side), *not* by a hardcoded name, so a
            // custom-named mask slab is opened exactly like the default F.Mask/B.Mask —
            // symmetric with the by-role mask solid in `elaborate::features`. A side with
            // no mask slab opens nothing (a `Void` is a no-op where no mask exists).
            let opening = world.inflated(geom::MASK_EXPANSION);
            let mask_zs: [Option<geom::ZRange>; 2] = match cu.layers {
                PadLayers::Through => [stackup.top_mask(), stackup.bottom_mask()],
                PadLayers::Top | PadLayers::Bottom => {
                    // XOR with the flip: a Top pad on a flipped part is on the bottom,
                    // so its exposed side (and thus its mask slab) is the bottom mask.
                    if matches!(cu.layers, PadLayers::Top) != flipped {
                        [stackup.top_mask(), None]
                    } else {
                        [stackup.bottom_mask(), None]
                    }
                }
            };
            match cu.layers {
                PadLayers::Top | PadLayers::Bottom => {
                    let is_top_local = matches!(cu.layers, PadLayers::Top);
                    // XOR with the flip: a Top pad on a flipped part is on the bottom.
                    let z = if is_top_local != flipped {
                        stackup.top_copper()
                    } else {
                        stackup.bottom_copper()
                    };
                    if let Some(z) = z {
                        features.push(geom::Feature::prism(geom::Role::Conductor, world, z));
                    }
                }
                PadLayers::Through => {
                    // Fan out: one conductor feature per copper slab, same world shape.
                    for slab in stackup.copper_slabs() {
                        features.push(geom::Feature::prism(
                            geom::Role::Conductor,
                            world.clone(),
                            slab.z,
                        ));
                    }
                }
            }
            for z in mask_zs.into_iter().flatten() {
                features.push(geom::Feature::prism(geom::Role::Void, opening.clone(), z));
            }
        }
        if let Some(drill) = &pad.drill {
            // The drill is a Void that pierces the whole stackup (mask + silk included),
            // centred on the pad centre. A round drill carries no centre, so it sits at
            // the pin offset; a slot's endpoints are already stored in the pin's local
            // frame.
            let local = match *drill {
                Drill::Round { d } => Shape2D::disc(self.offset, d / 2),
                Drill::Slot { a, b, d } => Shape2D::capsule(a, b, d / 2),
            };
            let world = local.map_points(|p| to_world(comp, p));
            if let Some(z) = stackup.full_z() {
                features.push(geom::Feature::prism(geom::Role::Void, world, z));
            }
        }
        features
    }
}

/// Default extra clearance added around a part's copper extent to form its
/// courtyard keep-out, in nm (~0.25 mm, the KiCad-ish default).
pub const COURTYARD_MARGIN: Nm = 250_000;

/// A part's **courtyard** as origin-centred axis-aligned half-extents `(hw, hh)` in
/// component-local nm: the bounding box of its **pad copper**, made symmetric about
/// the origin and grown by [`COURTYARD_MARGIN`]. This is the keep-out the placement
/// solver uses for overlap-avoidance (issue 0005).
///
/// Derived from real copper extent only, so a footprint-less part (the toy
/// `part_library`, `pad: None`) returns `(0, 0)` — it has no defined physical
/// courtyard, so it is exempt from overlap-avoidance (it is an abstract fixture, not
/// a placeable body). Origin-centred (rather than a true offset bbox) keeps it a
/// single half-extent pair that rotates by swapping `hw`/`hh` on a cardinal turn;
/// real footprints are centred on their origin, so this is tight in practice and
/// conservative otherwise.
pub fn courtyard_half_extents(def: &PartDef) -> (Nm, Nm) {
    let (mut mx, mut my) = (0, 0); // max |coordinate| on each axis
    let mut any = false;
    for pin in &def.pins {
        let Some(pad) = &pin.pad else { continue };
        for cu in &pad.copper {
            if let Some((lo, hi)) = cu.shape.bbox() {
                mx = mx.max(lo.x.abs()).max(hi.x.abs());
                my = my.max(lo.y.abs()).max(hi.y.abs());
                any = true;
            }
        }
    }
    if !any {
        return (0, 0);
    }
    (mx + COURTYARD_MARGIN, my + COURTYARD_MARGIN)
}

/// A part's **courtyard** as a real polygon (Decision 10): the convex hull of every
/// pad-copper skeleton vertex, inflated by [`COURTYARD_MARGIN`] (carried as the
/// polygon's Minkowski radius). In **component-local** coordinates, the same frame as
/// the pad copper.
///
/// This is the honest polygonal keep-out — available now for DRC / 3D / render. The
/// placement solver still pushes the cheap axis-aligned [`courtyard_half_extents`]
/// proxy: because this hull is always ⊆ that AABB, a *separate* polygon verify after a
/// converged AABB push can never find an overlap the push left behind, so realising
/// Decision 10's tighter-packing value requires the solver's push itself to consume
/// this polygon — a deferred solver enhancement (issue 0019), not a verify bolt-on.
///
/// Footprint-less parts (the toy `part_library`, every `pad: None`) have no copper, so
/// they return `None` and are exempt from overlap verification — exactly as they are
/// exempt from the proxy push. A degenerate footprint whose copper vertices are
/// collinear (no 2-D hull, e.g. a single round pad) also returns `None`.
///
/// The hull is taken over the skeleton corner vertices ([`Shape2D::points`]); the pad
/// copper's own inflation radius is *not* added, so for round/oval pads the margin is
/// measured from the pad centre-line rather than its copper edge. `COURTYARD_MARGIN`
/// (~0.25 mm) dominates at typical pad scale; the axis-aligned proxy
/// ([`courtyard_half_extents`], which *does* include the radius via `bbox`) stays the
/// conservative pusher.
pub fn courtyard_shape(def: &PartDef) -> Option<Shape2D> {
    let mut pts = Vec::new();
    for pin in &def.pins {
        let Some(pad) = &pin.pad else { continue };
        for cu in &pad.copper {
            pts.extend(cu.shape.points());
        }
    }
    if pts.is_empty() {
        return None;
    }
    let hull = geom::convex_hull(&pts);
    if hull.len() < 3 {
        return None; // no 2-D hull (a lone pad / collinear pads): no polygon courtyard
    }
    Some(Shape2D::polygon_path(
        geom::Path::polyline(hull),
        COURTYARD_MARGIN,
    ))
}

pub type PartLib = BTreeMap<String, PartDef>;

fn uart() -> InterfaceDef {
    InterfaceDef {
        type_name: "UART".into(),
        signals: BTreeMap::from([("tx".into(), Out), ("rx".into(), In)]),
        // Two adjacent pins on the component's right edge.
        offsets: BTreeMap::from([
            ("tx".into(), Point { x: 3 * MM, y: MM }),
            ("rx".into(), Point { x: 3 * MM, y: -MM }),
        ]),
        // The crossing that designers get wrong by hand, encoded once, correctly.
        mate: vec![("tx".into(), "rx".into()), ("rx".into(), "tx".into())],
    }
}

fn pin(name: &str, role: PinRole, offset: Point) -> PinDef {
    // No distinct pad numbering in the toy library: number defaults to the name.
    // The toy parts carry no footprint, so they have no pad copper geometry.
    PinDef {
        name: name.into(),
        number: name.into(),
        role,
        offset,
        pad: None,
    }
}

/// A small built-in library sufficient for the M1 demo.
pub fn part_library() -> PartLib {
    use PinRole::*;
    let mut lib = PartLib::new();

    // Offsets are plausible local pin geometry (nm), not exact footprints: a
    // small SOT-23-ish LDO, a two-terminal cap, and ~6mm ICs with pins on edges.
    lib.insert(
        "LDO".into(),
        PartDef {
            name: "LDO".into(),
            pins: vec![
                pin("VIN", PowerIn, Point { x: -2 * MM, y: 0 }),
                pin("VOUT", PowerOut, Point { x: 2 * MM, y: 0 }),
                pin("GND", Passive, Point { x: 0, y: -2 * MM }),
            ],
            interfaces: BTreeMap::new(),
        },
    );
    lib.insert(
        "Cap".into(),
        PartDef {
            name: "Cap".into(),
            pins: vec![
                pin("p1", Passive, Point { x: -MM, y: 0 }),
                pin("p2", Passive, Point { x: MM, y: 0 }),
            ],
            interfaces: BTreeMap::new(),
        },
    );
    lib.insert(
        "MCU".into(),
        PartDef {
            name: "MCU".into(),
            pins: vec![
                pin(
                    "VDD",
                    PowerIn,
                    Point {
                        x: -3 * MM,
                        y: 3 * MM,
                    },
                ),
                pin(
                    "GND",
                    Passive,
                    Point {
                        x: -3 * MM,
                        y: -3 * MM,
                    },
                ),
            ],
            interfaces: BTreeMap::from([("uart".into(), uart())]),
        },
    );
    lib.insert(
        "Sensor".into(),
        PartDef {
            name: "Sensor".into(),
            pins: vec![
                pin(
                    "VDD",
                    PowerIn,
                    Point {
                        x: -3 * MM,
                        y: 3 * MM,
                    },
                ),
                pin(
                    "GND",
                    Passive,
                    Point {
                        x: -3 * MM,
                        y: -3 * MM,
                    },
                ),
            ],
            interfaces: BTreeMap::from([("uart".into(), uart())]),
        },
    );
    lib
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{Dof, Orient, Provenance};
    use crate::id::EntityId;

    fn comp(part: &str, pos: Point, orient: Orient) -> Component {
        Component {
            id: EntityId::new("u1"),
            part: part.into(),
            pos: Dof {
                value: pos,
                prov: Provenance::Free,
            },
            orient,
        }
    }

    #[test]
    fn pin_offset_resolves_discrete_and_interface_pins() {
        let lib = part_library();
        let ldo = &lib["LDO"];
        assert_eq!(ldo.pin_offset("VOUT"), Some(Point { x: 2 * MM, y: 0 }));
        assert_eq!(ldo.pin_offset("nope"), None);
        let mcu = &lib["MCU"];
        // Interface signals addressed as `port.signal`.
        assert_eq!(mcu.pin_offset("uart.tx"), Some(Point { x: 3 * MM, y: MM }));
        assert_eq!(mcu.pin_offset("uart.bogus"), None);
    }

    #[test]
    fn resolve_selector_fans_out_by_name_and_falls_back_to_number() {
        use PinRole::*;
        let mk = |name: &str, number: &str, role| PinDef {
            name: name.into(),
            number: number.into(),
            role,
            offset: Point { x: 0, y: 0 },
            pad: None,
        };
        let part = PartDef {
            name: "P".into(),
            // Two pads share the name VDD (distinct numbers) — the duplicate-power
            // case; numbers are out of order to prove order follows declaration.
            pins: vec![
                mk("VDD", "1", PowerIn),
                mk("VDD", "8", PowerIn),
                mk("GND", "4", Passive),
            ],
            interfaces: BTreeMap::new(),
        };
        // A functional name fans out to *every* matching pad number.
        assert_eq!(
            part.resolve_selector("VDD"),
            vec!["1".to_string(), "8".to_string()]
        );
        assert_eq!(part.resolve_selector("GND"), vec!["4".to_string()]);
        // No name matches -> fall back to a direct pad-number reference.
        assert_eq!(part.resolve_selector("8"), vec!["8".to_string()]);
        // Names nothing -> empty, so the caller raises a hard error (no silent dangle).
        assert!(part.resolve_selector("NOPE").is_empty());
        // Stored identity resolves by number, never by the colliding name.
        assert_eq!(part.pin_role("8"), Some(PowerIn));
        assert_eq!(part.pin_role("VDD"), None);
    }

    /// A pin's world position is exact under each of the four cardinal rotations.
    #[test]
    fn pin_world_exact_under_each_cardinal_rotation() {
        let lib = part_library();
        let ldo = &lib["LDO"];
        // VOUT local offset is (2mm, 0); component at (10mm, 5mm).
        let at = Point::mm(10, 5);
        let cases = [
            (
                Orient::from_deg(0).unwrap(),
                Point {
                    x: 12 * MM,
                    y: 5 * MM,
                },
            ), // (+2, 0)
            (
                Orient::from_deg(90).unwrap(),
                Point {
                    x: 10 * MM,
                    y: 7 * MM,
                },
            ), // (0, +2)
            (
                Orient::from_deg(180).unwrap(),
                Point {
                    x: 8 * MM,
                    y: 5 * MM,
                },
            ), // (-2, 0)
            (
                Orient::from_deg(270).unwrap(),
                Point {
                    x: 10 * MM,
                    y: 3 * MM,
                },
            ), // (0, -2)
        ];
        for (o, expected) in cases {
            let c = comp("LDO", at, o);
            assert_eq!(
                pin_world(&c, ldo, "VOUT"),
                Some(expected),
                "rotation {:?}",
                o
            );
        }
    }

    #[test]
    fn rotate_is_exact_and_reversible() {
        let p = Point { x: 3 * MM, y: MM };
        assert_eq!(Orient::from_deg(0).unwrap().apply(p), p);
        // Two 180s (or four 90s) return to the original — exact, no drift.
        assert_eq!(
            Orient::from_deg(180)
                .unwrap()
                .apply(Orient::from_deg(180).unwrap().apply(p)),
            p
        );
        let q = Orient::from_deg(90).unwrap().apply(
            Orient::from_deg(90).unwrap().apply(
                Orient::from_deg(90)
                    .unwrap()
                    .apply(Orient::from_deg(90).unwrap().apply(p)),
            ),
        );
        assert_eq!(q, p);
    }

    #[test]
    fn quaternion_cardinals_match_legacy_rotation_exactly() {
        let p = Point { x: 3 * MM, y: MM };
        assert_eq!(Orient::from_deg(0).unwrap().apply(p), p);
        assert_eq!(
            Orient::from_deg(90).unwrap().apply(p),
            Point { x: -p.y, y: p.x }
        );
        assert_eq!(
            Orient::from_deg(180).unwrap().apply(p),
            Point { x: -p.x, y: -p.y }
        );
        assert_eq!(
            Orient::from_deg(270).unwrap().apply(p),
            Point { x: p.y, y: -p.x }
        );
        // Default is identity, not the all-zero (invalid) quaternion.
        assert_eq!(Orient::default(), Orient::IDENTITY);
        assert_eq!(Orient::IDENTITY.apply(p), p);
    }

    #[test]
    fn flip_to_bottom_is_a_rotation_not_a_mirror_flag() {
        // 180° about the x-axis = flip-to-bottom: a pure rotation, no bool needed.
        let flip = Orient {
            w: 0,
            x: 1,
            y: 0,
            z: 0,
        };
        assert!(flip.is_bottom(), "local +z now points down ⇒ bottom side");
        assert!(
            !Orient::from_deg(90).unwrap().is_bottom(),
            "an about-z turn stays top side"
        );
        // Applied to a planar point it flips y and stays in-plane (exact).
        assert_eq!(flip.apply(Point { x: 5, y: 3 }), Point { x: 5, y: -3 });
    }

    #[test]
    fn to_deg_projects_cardinals_exactly() {
        for d in [0, 90, 180, 270] {
            assert_eq!(Orient::from_deg(d).unwrap().to_deg(), d);
        }
    }

    #[test]
    fn degenerate_quaternion_apply_is_a_safe_no_op() {
        // A zero quaternion isn't a rotation; `apply` must not divide by zero (defence
        // in depth — the parser also rejects it). It falls back to leaving the point put.
        let zero = Orient {
            w: 0,
            x: 0,
            y: 0,
            z: 0,
        };
        assert_eq!(zero.apply(Point { x: 5, y: 3 }), Point { x: 5, y: 3 });
    }

    #[test]
    fn arbitrary_angle_rotates_correctly() {
        // 30° about z: apply to (1mm, 0) ≈ (cos30, sin30)·1mm = (866025, 500000) nm.
        let o = Orient::from_angle_deg(30.0);
        let r = o.apply(Point { x: MM, y: 0 });
        assert!(
            (r.x - 866_025).abs() < 50 && (r.y - 500_000).abs() < 50,
            "got {r:?}"
        );
        assert_eq!(o.to_deg(), 30);
    }

    #[test]
    fn bottom_side_pad_swaps_to_the_bottom_copper_layer() {
        let su = Stackup::default_2layer();
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(surface_pad(Shape2D::disc(Point { x: 0, y: 0 }, MM))), // a Top pad
        };
        let top = comp("P", Point { x: 0, y: 0 }, Orient::default());
        let bot = comp("P", Point { x: 0, y: 0 }, Orient::default().flipped());
        assert!(bot.orient.is_bottom() && !top.orient.is_bottom());
        let tf = pin.pad_features(&top, &su);
        let bf = pin.pad_features(&bot, &su);
        let (_, z_top) = prism_shape_z(&tf[0]);
        let (_, z_bot) = prism_shape_z(&bf[0]);
        assert_eq!(
            z_top,
            su.top_copper().unwrap(),
            "top-side Top pad → top copper"
        );
        assert_eq!(
            z_bot,
            su.bottom_copper().unwrap(),
            "flipped Top pad → bottom copper (derived from orientation, no flag)"
        );
    }

    use crate::geom::{self, Extent, Role, Shape2D, Stackup};

    /// A surface pad: one copper region on `Top`, no drill.
    fn surface_pad(shape: Shape2D) -> PadGeo {
        PadGeo {
            copper: vec![PadCopper {
                shape,
                layers: PadLayers::Top,
            }],
            drill: None,
        }
    }

    fn prism_shape_z(f: &geom::Feature) -> (&Shape2D, geom::ZRange) {
        match &f.extent {
            Extent::Prism { shape, z } => (shape, *z),
        }
    }

    #[test]
    fn pad_features_surface_pad_one_conductor_on_top() {
        let stackup = Stackup::default_2layer();
        // A 1mm square pad offset (1mm,0) in the footprint frame.
        let pad_shape = Shape2D::rect(Point { x: MM, y: 0 }, MM, MM);
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: MM, y: 0 },
            pad: Some(surface_pad(pad_shape.clone())),
        };
        let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(0).unwrap());
        let feats = pin.pad_features(&c, &stackup);
        let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
        assert_eq!(conductors.len(), 1, "one copper region, no drill");
        let (shape, z) = prism_shape_z(conductors[0]);
        assert_eq!(z, stackup.top_copper().unwrap(), "Top → top copper z");
        // At the origin with Deg0, the world shape == the local shape; bbox matches the
        // world-mapped copper bbox.
        let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
        assert_eq!(shape.bbox(), world.bbox());
        assert_eq!(shape.bbox(), pad_shape.bbox());
    }

    /// A surface pad emits one mask-opening `Void` on its resolved side's mask slab:
    /// F.Mask for a top-placed pad, B.Mask for a flipped (bottom) one, and the opening
    /// is the pad copper inflated by [`geom::MASK_EXPANSION`] (Decision 13).
    #[test]
    fn pad_features_surface_pad_opens_its_side_mask() {
        let su = Stackup::default_2layer();
        let pad_shape = Shape2D::rect(Point { x: MM, y: 0 }, MM, MM);
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: MM, y: 0 },
            pad: Some(surface_pad(pad_shape)),
        };

        // Top-placed: opens F.Mask, at the F.Mask z, expanded by the margin.
        let top = comp("P", Point { x: 0, y: 0 }, Orient::default());
        let tf = pin.pad_features(&top, &su);
        let opens: Vec<_> = tf.iter().filter(|f| f.role == Role::Void).collect();
        assert_eq!(opens.len(), 1, "one mask opening for a surface pad");
        let (shape, z) = prism_shape_z(opens[0]);
        assert_eq!(z, su.slab_z("F.Mask").unwrap(), "top pad opens F.Mask");
        let world = pad_copper_world(&top, &pin.pad.as_ref().unwrap().copper[0]);
        assert_eq!(
            *shape,
            world.inflated(geom::MASK_EXPANSION),
            "opening is the copper expanded by the mask margin"
        );

        // Flipped (bottom): opens B.Mask instead (derived from orientation, no flag).
        let bot = comp("P", Point { x: 0, y: 0 }, Orient::default().flipped());
        let bf = pin.pad_features(&bot, &su);
        let opens: Vec<_> = bf.iter().filter(|f| f.role == Role::Void).collect();
        assert_eq!(opens.len(), 1, "one mask opening for a flipped surface pad");
        assert_eq!(
            prism_shape_z(opens[0]).1,
            su.slab_z("B.Mask").unwrap(),
            "flipped pad opens B.Mask"
        );
    }

    /// A custom stackup with no mask slab opens nothing (a `Void` is a no-op where no
    /// mask exists — not an error). The copper still lowers as usual.
    #[test]
    fn pad_features_no_mask_slab_opens_nothing() {
        let su = Stackup {
            slabs: vec![geom::Slab {
                name: "F.Cu".into(),
                z: geom::ZRange::new(0, 35_000),
                role: Role::Conductor,
                material: None,
            }],
        };
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(surface_pad(Shape2D::rect(Point { x: 0, y: 0 }, MM, MM))),
        };
        let c = comp("P", Point { x: 0, y: 0 }, Orient::default());
        let feats = pin.pad_features(&c, &su);
        assert!(
            !feats.iter().any(|f| f.role == Role::Void),
            "no mask slab ⇒ no opening"
        );
        assert_eq!(
            feats.iter().filter(|f| f.role == Role::Conductor).count(),
            1,
            "copper still lowers"
        );
    }

    /// The opening is resolved by role + z-position, not by a hardcoded slab name: a
    /// custom stackup whose mask slab is named `TopMask` still gets a pad opening at
    /// that slab's z. Guards the review's solid-by-role vs opening-by-name asymmetry —
    /// `elaborate::features` masks this slab by role, so the opening must find it too.
    #[test]
    fn pad_features_opening_resolves_custom_named_mask_slab() {
        let su = Stackup {
            slabs: vec![
                geom::Slab {
                    name: "F.Cu".into(),
                    z: geom::ZRange::new(0, 35_000),
                    role: Role::Conductor,
                    material: None,
                },
                geom::Slab {
                    name: "TopMask".into(),
                    z: geom::ZRange::new(35_000, 60_000),
                    role: Role::Mask,
                    material: Some(geom::Material::named("soldermask")),
                },
            ],
        };
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(surface_pad(Shape2D::rect(Point { x: 0, y: 0 }, MM, MM))),
        };
        let c = comp("P", Point { x: 0, y: 0 }, Orient::default());
        let feats = pin.pad_features(&c, &su);
        let opens: Vec<_> = feats.iter().filter(|f| f.role == Role::Void).collect();
        assert_eq!(opens.len(), 1, "the differently-named mask slab is opened");
        assert_eq!(
            prism_shape_z(opens[0]).1,
            su.slab_z("TopMask").unwrap(),
            "opening lands at the custom-named mask slab's z"
        );
    }

    #[test]
    fn pad_features_through_pad_fans_out_with_drill_void() {
        let stackup = Stackup::default_2layer();
        let pad_shape = Shape2D::disc(Point { x: 0, y: 0 }, MM);
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(PadGeo {
                copper: vec![PadCopper {
                    shape: pad_shape.clone(),
                    layers: PadLayers::Through,
                }],
                drill: Some(Drill::Round { d: MM / 2 }),
            }),
        };
        let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(0).unwrap());
        let feats = pin.pad_features(&c, &stackup);
        let n_cu = stackup.copper_slabs().len();
        assert_eq!(n_cu, 2, "default 2-layer stackup has two copper slabs");
        let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
        let voids: Vec<_> = feats.iter().filter(|f| f.role == Role::Void).collect();
        assert_eq!(conductors.len(), n_cu, "one conductor per copper slab");
        // Voids: the drill (spanning the full stackup) + the two mask openings (a
        // through pad opens both F.Mask and B.Mask).
        let drill_void: Vec<_> = voids
            .iter()
            .filter(|f| prism_shape_z(f).1 == stackup.full_z().unwrap())
            .collect();
        assert_eq!(drill_void.len(), 1, "one drill void");
        assert_eq!(
            voids.len(),
            3,
            "drill void + two mask openings (both sides)"
        );
        // The two mask openings are on F.Mask and B.Mask (a through pad opens both).
        let mut mask_zs: Vec<_> = voids
            .iter()
            .map(|f| prism_shape_z(f).1)
            .filter(|z| *z != stackup.full_z().unwrap())
            .collect();
        mask_zs.sort_by_key(|z| z.lo);
        let mut want = vec![
            stackup.slab_z("F.Mask").unwrap(),
            stackup.slab_z("B.Mask").unwrap(),
        ];
        want.sort_by_key(|z| z.lo);
        assert_eq!(mask_zs, want, "through pad opens both F.Mask and B.Mask");
        // All conductor features share the same world shape, one per slab z.
        let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
        let mut zs: Vec<_> = conductors
            .iter()
            .map(|f| {
                let (shape, z) = prism_shape_z(f);
                assert_eq!(
                    *shape, world,
                    "every fan-out feature shares the world shape"
                );
                z
            })
            .collect();
        zs.sort_by_key(|z| z.lo);
        let slab_zs = {
            let mut v: Vec<_> = stackup.copper_slabs().iter().map(|s| s.z).collect();
            v.sort_by_key(|z| z.lo);
            v
        };
        assert_eq!(zs, slab_zs, "fan-out covers every copper slab z");
        // The drill void spans the full stackup (pierces mask + silk, not just the body).
        let (_, vz) = prism_shape_z(drill_void[0]);
        assert_eq!(
            vz,
            stackup.full_z().unwrap(),
            "drill void pierces the full stackup"
        );
    }

    #[test]
    fn pad_features_slot_drill_is_a_world_mapped_capsule() {
        // Hardens the slot-drill frame the Phase-1 agent verified only by reasoning:
        // the slot endpoints are world-mapped through the *same* `to_world` as copper
        // (so a rotated/translated component moves them), and the void spans the board.
        let stackup = Stackup::default_2layer();
        let a = Point { x: -MM, y: 0 };
        let b = Point { x: MM, y: 0 };
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 0, y: 0 },
            pad: Some(PadGeo {
                copper: vec![PadCopper {
                    shape: Shape2D::disc(Point { x: 0, y: 0 }, MM),
                    layers: PadLayers::Through,
                }],
                drill: Some(Drill::Slot { a, b, d: MM / 2 }),
            }),
        };
        // Rotated + translated so a raw (un-mapped) slot would land in the wrong place.
        let c = comp(
            "P",
            Point { x: 5 * MM, y: 0 },
            Orient::from_deg(90).unwrap(),
        );
        let feats = pin.pad_features(&c, &stackup);
        // The drill void is the one spanning the full stackup; the others are mask
        // openings (a through pad opens both sides).
        let drill_void: Vec<_> = feats
            .iter()
            .filter(|f| f.role == Role::Void && prism_shape_z(f).1 == stackup.full_z().unwrap())
            .collect();
        assert_eq!(drill_void.len(), 1, "one drill void");
        let (shape, vz) = prism_shape_z(drill_void[0]);
        // Drill `d` is a diameter, so the capsule radius is `d / 2` (= MM / 4).
        let expected = Shape2D::capsule(a, b, MM / 4).map_points(|p| to_world(&c, p));
        assert_eq!(*shape, expected, "slot void is the world-mapped capsule");
        assert_eq!(
            vz,
            stackup.full_z().unwrap(),
            "slot void pierces the full stackup"
        );
    }

    #[test]
    fn pad_features_rotated_component_rotates_world_shape() {
        let stackup = Stackup::default_2layer();
        // Pad at (2mm, 0) in the footprint frame; a Deg90 component rotates it to
        // (0, 2mm). Reusing pad_copper_world means the feature shape moves with it.
        let pad_shape = Shape2D::rect(Point { x: 2 * MM, y: 0 }, MM, MM);
        let pin = PinDef {
            name: "1".into(),
            number: "1".into(),
            role: PinRole::Passive,
            offset: Point { x: 2 * MM, y: 0 },
            pad: Some(surface_pad(pad_shape)),
        };
        let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(90).unwrap());
        let feats = pin.pad_features(&c, &stackup);
        let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
        assert_eq!(conductors.len(), 1);
        let (shape, _) = prism_shape_z(conductors[0]);
        let (lo, hi) = shape.bbox().unwrap();
        // The pad centre moved from (2mm,0) to (0,2mm); its bbox is now centred there.
        let cx = (lo.x + hi.x) / 2;
        let cy = (lo.y + hi.y) / 2;
        assert_eq!((cx, cy), (0, 2 * MM), "Deg90 rotates the world shape");
        // And it matches the world-mapped copper directly.
        let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
        assert_eq!(shape.bbox(), world.bbox());
    }

    #[test]
    fn courtyard_shape_covers_the_pads_plus_margin() {
        // Two 1mm square pads at (±2mm, 0). The hull of their corners spans
        // x∈[-2.5,2.5]mm, y∈[-0.5,0.5]mm; the courtyard is that polygon inflated by
        // COURTYARD_MARGIN.
        let mk = |cx: Nm| PinDef {
            name: "p".into(),
            number: "p".into(),
            role: PinRole::Passive,
            offset: Point { x: cx, y: 0 },
            pad: Some(surface_pad(Shape2D::rect(Point { x: cx, y: 0 }, MM, MM))),
        };
        let def = PartDef {
            name: "R".into(),
            pins: vec![mk(2 * MM), mk(-2 * MM)],
            interfaces: BTreeMap::new(),
        };
        let court = courtyard_shape(&def).expect("a real pad part has a courtyard");
        assert!(
            matches!(court, Shape2D::Polygon { .. }),
            "courtyard is a polygon"
        );
        assert_eq!(
            court.radius(),
            COURTYARD_MARGIN,
            "radius carries the margin"
        );
        // The polygon skeleton is the pad hull; its bbox is the hull bbox + margin.
        let (lo, hi) = court.bbox().unwrap();
        assert_eq!(lo.x, -25 * MM / 10 - COURTYARD_MARGIN);
        assert_eq!(hi.x, 25 * MM / 10 + COURTYARD_MARGIN);
        assert_eq!(lo.y, -5 * MM / 10 - COURTYARD_MARGIN);
        assert_eq!(hi.y, 5 * MM / 10 + COURTYARD_MARGIN);
        // The hull encloses each pad centre.
        assert!(court.contains_point(Point { x: 2 * MM, y: 0 }));
        assert!(court.contains_point(Point { x: -2 * MM, y: 0 }));
        // A disc sitting just outside the hull but within the margin overlaps it.
        let probe = Shape2D::disc(
            Point {
                x: 26 * MM / 10,
                y: 0,
            },
            1,
        );
        assert!(
            geom::clearance_violated(&court, &probe, 0),
            "a point within the margin band is inside the courtyard keep-out"
        );
    }

    #[test]
    fn courtyard_shape_is_none_without_a_footprint() {
        // Toy library parts carry no pads → no physical courtyard.
        let lib = part_library();
        assert!(courtyard_shape(&lib["LDO"]).is_none());
        // A single round pad has only one skeleton vertex: no 2-D hull → None.
        let one = PartDef {
            name: "dot".into(),
            pins: vec![PinDef {
                name: "1".into(),
                number: "1".into(),
                role: PinRole::Passive,
                offset: Point { x: 0, y: 0 },
                pad: Some(surface_pad(Shape2D::disc(Point { x: 0, y: 0 }, MM))),
            }],
            interfaces: BTreeMap::new(),
        };
        assert!(courtyard_shape(&one).is_none());
    }

    #[test]
    fn pad_features_no_pad_is_empty() {
        let stackup = Stackup::default_2layer();
        let pin = pin("VIN", PinRole::PowerIn, Point { x: 0, y: 0 });
        let c = comp("P", Point { x: 0, y: 0 }, Orient::from_deg(0).unwrap());
        assert!(pin.pad_features(&c, &stackup).is_empty());
    }

    #[test]
    fn orient_from_deg_normalises_and_rejects_off_axis() {
        assert_eq!(Orient::from_deg(-90), Some(Orient::from_deg(270).unwrap()));
        assert_eq!(Orient::from_deg(450), Some(Orient::from_deg(90).unwrap()));
        assert_eq!(Orient::from_deg(360), Some(Orient::from_deg(0).unwrap()));
        assert_eq!(Orient::from_deg(45), None);
    }
}
