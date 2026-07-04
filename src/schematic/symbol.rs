//! Symbol sizing (Decision 20e — boxes-with-pins): the axis-aligned [`Extent`], the
//! per-pin [`PinSlot`] placement, and the layout metrics reflow packs against.

use crate::doc::Nm;
use crate::part::PartDef;

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
pub(crate) const MIN_BOX_W: Nm = 5_080_000; // a pinless / tiny part still gets a 2-pitch box.
pub(crate) const MIN_BOX_H: Nm = 5_080_000;

/// Every box-edge pin identity of a part, in the layout enumeration order: discrete pins
/// first (declaration order), then interface-port signals (`port.signal`, BTreeMap
/// order). The names are what widths key on; the count drives height. This is the single
/// definition of "what counts as a pin on the box edge" (§20 — interface ports count).
pub(crate) fn edge_pins(def: &PartDef) -> Vec<String> {
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
