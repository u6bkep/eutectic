//! Schematic hit-testing: pick candidates folded from the
//! [`schematic_features`] stream's provenance (Decision 23 commitment: the
//! drawing and its hit-test derive from ONE stream and cannot drift).
//!
//! The behavior contract is the old `SchematicView::resolve`'s, preserved
//! verbatim through the WP3 owned-canvas rewrite (the *rendering* half of
//! that module died; this pick half moved here):
//!
//! - **candidates**: a pin stub's *tip* (priority 0 — most specific), a
//!   net-carrying wire's polyline → its **net** (priority 1 — a wire is
//!   presentational; its selectable identity is the net it draws, the
//!   cross-view currency), a symbol outline's bbox → its part (priority 2);
//! - **resolution**: among hits within the tolerance, the lowest priority
//!   wins;
//! - **tolerance**: a screen-px grab radius converted through the zoom by
//!   the same [`tolerance_nm`](crate::pick::tolerance_nm) helper as the
//!   board, so picking never gets harder as you zoom out.
//!
//! The pointer arrives in schematic nm (y-up) through the pane camera's
//! `unproject` — same f64 CPU path as the board pick (renderer-spec §7).

use crate::pick::SemanticId;
use eutectic_core::coord::{Nm, Point};
use eutectic_core::schematic::{Provenance, SchematicFeatures, Shape, StyleClass};

/// One pickable schematic feature: a semantic id, the schematic-space test
/// geometry, and the pick priority (pin ▸ wire ▸ symbol).
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The id selected when this candidate wins.
    pub id: SemanticId,
    /// The pick geometry in schematic nm (y-up).
    geom: PickGeom,
    /// Priority — lower wins (pin=0, wire=1, symbol=2).
    pub(crate) priority: u8,
}

/// Schematic pick geometry: a symbol body is a box (half-extents about a
/// centre); a pin is a point at its stub tip; a wire is a polyline.
#[derive(Clone, Debug)]
enum PickGeom {
    /// Axis-aligned box: centre + half-width/half-height.
    Box { c: Point, hw: Nm, hh: Nm },
    /// A point (a pin stub tip) — hit within tolerance.
    Point(Point),
    /// A polyline (a wire) — hit within tolerance of any segment.
    Poly(Vec<Point>),
}

/// Fold the stream into pick candidates (pins ▸ wires ▸ symbol bodies) — the
/// provenance walk the old `SchematicView::build` ran, unchanged.
pub fn candidates(fs: &SchematicFeatures) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for f in &fs.features {
        match (&f.provenance, &f.class, &f.shape) {
            // Symbol body (priority 2 — least specific): the outline's bbox.
            (Provenance::Component(id), StyleClass::SymbolOutline, Shape::Polygon { pts, .. }) => {
                if let Some(b) = bbox(pts) {
                    out.push(Candidate {
                        id: SemanticId::Part(id.clone()),
                        geom: b,
                        priority: 2,
                    });
                }
            }
            // Pins (priority 0 — most specific): the stub tip, keyed by the
            // stored pin id (the `PinRef` join key `SemanticId::Pin` uses).
            (Provenance::Pin { comp, pin }, StyleClass::PinStub, Shape::Polyline { pts, .. }) => {
                if let Some(tip) = pts.last() {
                    out.push(Candidate {
                        id: SemanticId::Pin {
                            comp: comp.clone(),
                            pin: pin.clone(),
                        },
                        geom: PickGeom::Point(*tip),
                        priority: 0,
                    });
                }
            }
            // Wires (priority 1) → net: a wire is presentational; its
            // selectable identity is the net it draws.
            (Provenance::Wire { net: Some(net), .. }, _, Shape::Polyline { pts, .. }) => {
                out.push(Candidate {
                    id: SemanticId::Net(net.clone()),
                    geom: PickGeom::Poly(pts.clone()),
                    priority: 1,
                });
            }
            _ => {}
        }
    }
    out
}

/// Resolve a schematic-space query point (nm) to the winning pick, honoring
/// the pin ▸ wire ▸ symbol priority. `tol_nm` is the schematic-space grab
/// radius (from [`tolerance_nm`](crate::pick::tolerance_nm)). Pure — the old
/// `SchematicView::resolve`, verbatim.
pub fn resolve(cands: &[Candidate], p: Point, tol_nm: Nm) -> Option<SemanticId> {
    let mut best: Option<&Candidate> = None;
    for c in cands {
        if !c.geom.hits(p, tol_nm) {
            continue;
        }
        best = Some(match best {
            None => c,
            Some(b) if c.priority < b.priority => c,
            Some(b) => b,
        });
    }
    best.map(|c| c.id.clone())
}

impl PickGeom {
    /// Does this geometry contain / lie within `tol` of `p`?
    fn hits(&self, p: Point, tol: Nm) -> bool {
        let tol = tol.max(0);
        match self {
            PickGeom::Box { c, hw, hh } => {
                (p.x - c.x).abs() <= hw + tol && (p.y - c.y).abs() <= hh + tol
            }
            PickGeom::Point(q) => {
                let dx = (p.x - q.x) as i128;
                let dy = (p.y - q.y) as i128;
                let t = tol as i128;
                dx * dx + dy * dy <= t * t
            }
            PickGeom::Poly(poly) => poly
                .windows(2)
                .any(|w| point_seg_dist2(p, w[0], w[1]) <= (tol as i128) * (tol as i128)),
        }
    }
}

/// Squared distance (nm², i128) from point `p` to segment `a`-`b`.
fn point_seg_dist2(p: Point, a: Point, b: Point) -> i128 {
    let (px, py) = (p.x as i128, p.y as i128);
    let (ax, ay) = (a.x as i128, a.y as i128);
    let (bx, by) = (b.x as i128, b.y as i128);
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx * dx + dy * dy;
    if len2 == 0 {
        let ex = px - ax;
        let ey = py - ay;
        return ex * ex + ey * ey;
    }
    // Clamp the projection parameter t = ((p-a)·(b-a)) / len2 to [0,1].
    let t_num = (px - ax) * dx + (py - ay) * dy;
    let (cx, cy) = if t_num <= 0 {
        (ax, ay)
    } else if t_num >= len2 {
        (bx, by)
    } else {
        (ax + t_num * dx / len2, ay + t_num * dy / len2)
    };
    let ex = px - cx;
    let ey = py - cy;
    ex * ex + ey * ey
}

/// The bbox of a polygon's points as a [`PickGeom::Box`] (centre +
/// half-extents), or `None` for an empty point list.
fn bbox(pts: &[Point]) -> Option<PickGeom> {
    let (min_x, max_x) = (
        pts.iter().map(|p| p.x).min()?,
        pts.iter().map(|p| p.x).max()?,
    );
    let (min_y, max_y) = (
        pts.iter().map(|p| p.y).min()?,
        pts.iter().map(|p| p.y).max()?,
    );
    Some(PickGeom::Box {
        c: Point {
            x: (min_x + max_x) / 2,
            y: (min_y + max_y) / 2,
        },
        hw: (max_x - min_x) / 2,
        hh: (max_y - min_y) / 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::schematic_domain;
    use eutectic_core::coord::MM;
    use eutectic_core::id::{EntityId, NetId};
    use eutectic_core::part::PartLib;
    use eutectic_core::schematic::{schematic_features, symbol_body};

    /// The schematic fixture's (doc, lib, candidates).
    fn fixture() -> (eutectic_core::doc::Doc, PartLib, Vec<Candidate>) {
        let d = schematic_domain();
        let doc = d
            .doc
            .as_ref()
            .expect("schematic fixture elaborates")
            .clone();
        let fs = schematic_features(&doc, &d.lib);
        let cands = candidates(&fs);
        (doc, d.lib, cands)
    }

    /// The placement centre of a component in schematic space.
    fn center_of(doc: &eutectic_core::doc::Doc, lib: &PartLib, path: &str) -> Point {
        let placements = doc.reflow_schematic(lib);
        placements
            .get(&EntityId::new(path))
            .expect("component placed")
            .center
    }

    /// The stub-tip point of a pin (by display name) on an identity-rotation
    /// symbol centred at `center` — from the core artwork seam.
    fn pin_tip(lib: &PartLib, part: &str, pin_name: &str, center: Point) -> Point {
        let anchor = symbol_body(lib.get(part).expect("part in lib"))
            .pins
            .into_iter()
            .find(|p| p.name == pin_name)
            .unwrap_or_else(|| panic!("{part} has a {pin_name} pin"));
        Point {
            x: center.x + anchor.tip.x,
            y: center.y + anchor.tip.y,
        }
    }

    /// The stored pin id (pad number / `port.signal`) of a pin, by name.
    fn pin_id(lib: &PartLib, part: &str, pin_name: &str) -> String {
        symbol_body(lib.get(part).expect("part in lib"))
            .pins
            .into_iter()
            .find(|p| p.name == pin_name)
            .unwrap_or_else(|| panic!("{part} has a {pin_name} pin"))
            .id
    }

    /// Clicking the centre of a symbol body (clear of its pins) selects that
    /// part — the priority-2 fallback.
    #[test]
    fn click_symbol_body_selects_part() {
        let (doc, lib, cands) = fixture();
        let c = center_of(&doc, &lib, "C1");
        let id = resolve(&cands, c, 0).expect("body hit");
        assert_eq!(id, SemanticId::Part(EntityId::new("C1")), "got {id:?}");
    }

    /// Clicking a pin stub tip selects that pin (by pad number), beating the
    /// body underneath — priority 0 over 2.
    #[test]
    fn click_pin_selects_pin() {
        let (doc, lib, cands) = fixture();
        let center = center_of(&doc, &lib, "U1");
        let tip = pin_tip(&lib, "MCU", "VDD", center);
        match resolve(&cands, tip, 0).expect("pin hit") {
            SemanticId::Pin { comp, pin } => {
                assert_eq!(comp, EntityId::new("U1"));
                assert_eq!(
                    pin,
                    pin_id(&lib, "MCU", "VDD"),
                    "pin id must be the pad NUMBER (the PinRef join key)"
                );
            }
            other => panic!("expected a pin, got {other:?}"),
        }
    }

    /// Clicking a wire segment selects its net (the cross-view currency).
    #[test]
    fn click_wire_selects_net() {
        let (doc, lib, cands) = fixture();
        let a = pin_tip(&lib, "Cap", "p1", center_of(&doc, &lib, "C1"));
        let b = pin_tip(&lib, "MCU", "VDD", center_of(&doc, &lib, "U1"));
        let mid = Point {
            x: (a.x + b.x) / 2,
            y: (a.y + b.y) / 2,
        };
        let id = resolve(&cands, mid, 100_000).expect("wire hit");
        assert_eq!(id, SemanticId::Net(NetId::new("VDD")), "got {id:?}");
    }

    /// A click far outside every feature picks nothing (the deselect path).
    #[test]
    fn empty_spot_picks_nothing() {
        let (_doc, _lib, cands) = fixture();
        let far = Point {
            x: -1_000 * MM,
            y: -1_000 * MM,
        };
        assert!(resolve(&cands, far, 0).is_none());
    }

    /// Tolerance behavior: a point just off a stub tip misses at zero
    /// tolerance and hits with a grab radius — and the radius converts
    /// through the same zoom scaling as the board pick.
    #[test]
    fn tolerance_grabs_near_misses() {
        let (doc, lib, cands) = fixture();
        let tip = pin_tip(&lib, "Cap", "p1", center_of(&doc, &lib, "C1"));
        let off = Point {
            x: tip.x,
            y: tip.y + MM / 2,
        };
        // At zero tolerance the near-miss falls through to the body/wire; a
        // 1 mm radius grabs the pin (priority 0 wins over both).
        assert!(!matches!(
            resolve(&cands, off, 0),
            Some(SemanticId::Pin { .. })
        ));
        assert!(matches!(
            resolve(&cands, off, MM),
            Some(SemanticId::Pin { .. })
        ));
        // Screen-px tolerance grows as the zoom shrinks (shared helper).
        assert!(crate::pick::tolerance_nm(6.0, 0.5) > crate::pick::tolerance_nm(6.0, 1.0));
    }

    /// The poc smoke: the real 44-symbol schematic yields all 44 symbol
    /// bodies + its authored wires as candidates.
    #[test]
    fn poc_schematic_projects_non_empty() {
        let d = crate::fixtures::poc_board_domain();
        let doc = d.doc.as_ref().expect("poc board elaborates");
        let fs = schematic_features(doc, &d.lib);
        let cands = candidates(&fs);
        let bodies = cands
            .iter()
            .filter(|c| matches!(c.id, SemanticId::Part(_)))
            .count();
        assert!(bodies >= 44, "expected ≥44 placed symbols, got {bodies}");
        assert!(
            cands.iter().any(|c| c.priority == 1),
            "poc schematic must project its authored wires"
        );
    }
}
