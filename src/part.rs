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
/// point). World coordinates come from the component's position + cardinal
/// orientation, applied with [`Shape2D::map_points`].
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
/// `component position + rotate(local pin offset, component orientation)`.
/// Exact for the four cardinal rotations. Returns `None` if the pin is unknown.
pub fn pin_world(comp: &Component, def: &PartDef, pin: &str) -> Option<Point> {
    let off = def.pin_offset(pin)?;
    let r = comp.orient.rotate(off);
    Some(Point {
        x: comp.pos.value.x + r.x,
        y: comp.pos.value.y + r.y,
    })
}

/// Lift a component-local point into world space on a placed component: rotate by
/// the cardinal orientation, translate to the component position. Exact (integer).
pub fn to_world(comp: &Component, p: Point) -> Point {
    let r = comp.orient.rotate(p);
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
    /// [`Role::Conductor`](geom::Role) prism on its layer's z, plus the drill (if
    /// any) as a [`Role::Void`](geom::Role) prism spanning the board. Empty if the
    /// pin has no pad.
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
        let mut features = Vec::new();
        for cu in &pad.copper {
            let world = pad_copper_world(comp, cu);
            match cu.layers {
                PadLayers::Top => {
                    if let Some(z) = stackup.top_copper() {
                        features.push(geom::Feature::prism(geom::Role::Conductor, world, z));
                    }
                }
                PadLayers::Bottom => {
                    if let Some(z) = stackup.bottom_copper() {
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
        }
        if let Some(drill) = &pad.drill {
            // The drill is a Void spanning the whole board, centred on the pad centre.
            // A round drill carries no centre, so it sits at the pin offset; a slot's
            // endpoints are already stored in the pin's local frame.
            let local = match *drill {
                Drill::Round { d } => Shape2D::disc(self.offset, d / 2),
                Drill::Slot { a, b, d } => Shape2D::capsule(a, b, d / 2),
            };
            let world = local.map_points(|p| to_world(comp, p));
            if let Some(z) = stackup.board_z() {
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
                Orient::Deg0,
                Point {
                    x: 12 * MM,
                    y: 5 * MM,
                },
            ), // (+2, 0)
            (
                Orient::Deg90,
                Point {
                    x: 10 * MM,
                    y: 7 * MM,
                },
            ), // (0, +2)
            (
                Orient::Deg180,
                Point {
                    x: 8 * MM,
                    y: 5 * MM,
                },
            ), // (-2, 0)
            (
                Orient::Deg270,
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
        assert_eq!(Orient::Deg0.rotate(p), p);
        // Two 180s (or four 90s) return to the original — exact, no drift.
        assert_eq!(Orient::Deg180.rotate(Orient::Deg180.rotate(p)), p);
        let q = Orient::Deg90
            .rotate(Orient::Deg90.rotate(Orient::Deg90.rotate(Orient::Deg90.rotate(p))));
        assert_eq!(q, p);
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
        let c = comp("P", Point { x: 0, y: 0 }, Orient::Deg0);
        let feats = pin.pad_features(&c, &stackup);
        assert_eq!(feats.len(), 1, "one copper region, no drill");
        assert_eq!(feats[0].role, Role::Conductor);
        let (shape, z) = prism_shape_z(&feats[0]);
        assert_eq!(z, stackup.top_copper().unwrap(), "Top → top copper z");
        // At the origin with Deg0, the world shape == the local shape; bbox matches the
        // world-mapped copper bbox.
        let world = pad_copper_world(&c, &pin.pad.as_ref().unwrap().copper[0]);
        assert_eq!(shape.bbox(), world.bbox());
        assert_eq!(shape.bbox(), pad_shape.bbox());
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
        let c = comp("P", Point { x: 0, y: 0 }, Orient::Deg0);
        let feats = pin.pad_features(&c, &stackup);
        let n_cu = stackup.copper_slabs().len();
        assert_eq!(n_cu, 2, "default 2-layer stackup has two copper slabs");
        let conductors: Vec<_> = feats.iter().filter(|f| f.role == Role::Conductor).collect();
        let voids: Vec<_> = feats.iter().filter(|f| f.role == Role::Void).collect();
        assert_eq!(conductors.len(), n_cu, "one conductor per copper slab");
        assert_eq!(voids.len(), 1, "one drill void");
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
        // The void spans the whole board.
        let (_, vz) = prism_shape_z(voids[0]);
        assert_eq!(vz, stackup.board_z().unwrap(), "drill void spans the board");
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
        let c = comp("P", Point { x: 0, y: 0 }, Orient::Deg90);
        let feats = pin.pad_features(&c, &stackup);
        assert_eq!(feats.len(), 1);
        let (shape, _) = prism_shape_z(&feats[0]);
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
    fn pad_features_no_pad_is_empty() {
        let stackup = Stackup::default_2layer();
        let pin = pin("VIN", PinRole::PowerIn, Point { x: 0, y: 0 });
        let c = comp("P", Point { x: 0, y: 0 }, Orient::Deg0);
        assert!(pin.pad_features(&c, &stackup).is_empty());
    }

    #[test]
    fn orient_from_deg_normalises_and_rejects_off_axis() {
        assert_eq!(Orient::from_deg(-90), Some(Orient::Deg270));
        assert_eq!(Orient::from_deg(450), Some(Orient::Deg90));
        assert_eq!(Orient::from_deg(360), Some(Orient::Deg0));
        assert_eq!(Orient::from_deg(45), None);
    }
}
