//! Typed-interface inference + explicit overlay for imported parts (issue 0010).
//!
//! An imported footprint/symbol join ([`kicad::join_symbol_footprint`]) produces
//! discrete roled pins only: `PartDef.interfaces` is empty, so the
//! "serial-swap-unrepresentable" guarantee (a UART tx can never be wired to another
//! tx — see [`part`](crate::part)) exists *only* for the hand-authored toy library.
//! This module closes that gap two ways, in decreasing order of trust:
//!
//! 1. [`apply_interface`] — an **explicit** authoring overlay, the twin of
//!    [`kicad::apply_role_map`](crate::kicad::apply_role_map): "these named pins form
//!    an interface of type T under port name P". Reliable, no guessing; this is the
//!    path you use when inference won't (or shouldn't) touch a part.
//! 2. [`infer_interfaces`] — **conservative** inference from pin-name conventions
//!    against a small built-in [`registry`]. It attaches an [`InterfaceDef`] *only*
//!    when a complete, unambiguous signal set matches; anything partial, duplicated,
//!    or aliased-ambiguously attaches nothing. Convenience on top of the explicit
//!    path, never a substitute for it.
//!
//! ## How an attached interface resolves
//! An interface port is addressed as `port.signal` (`swd.swdio`); its identity flows
//! into nets as a [`PinRef`](crate::doc::PinRef) of that spelling, and
//! [`PartDef::pin_offset`](crate::part::PartDef::pin_offset) /
//! [`pin_role`](crate::part::PartDef::pin_role) resolve it through the interface's own
//! `signals`/`offsets` maps. So — exactly like a toy part — an attached
//! [`InterfaceDef`] carries an `offsets` map; here it is **copied from the real pad
//! offsets** of the pins it groups, so the ratsnest / router point-seeding
//! ([`pin_world`](crate::part::pin_world)) land on the physical pads, and
//! `connect_interface` in [`elaborate`](crate::elaborate) mates them with the baked
//! crossing unchanged. ERC (a typecheck over roles) sees the signal directions the
//! same way it sees a toy part's.
//!
//! ## Directional conservatism (why bus signals are `Bidir`)
//! A single [`InterfaceDef`] is symmetric — both mated instances share the same
//! signal directions — and the crossing lives in `mate`. UART expresses its
//! controller/peripheral asymmetry entirely through the crossed mate (tx↔rx), so tx
//! stays `Out` and rx stays `In`. But a **shared-bus** interface (SPI SCK/MOSI/CS,
//! I2C SCL, SWD SWCLK) is driven by whichever side is the controller, and pin names
//! alone cannot tell controller from peripheral. Declaring such a line `Out` on both
//! parts would make a straight `SCK↔SCK` mate trip `connect_interface`'s
//! drive-vs-drive conflict on a perfectly legal bus. So every shared/multi-drop line
//! is modelled `Bidir` — the same conservative default `ElecType::role` uses for
//! `tri_state` (never invent a spurious driver conflict). Genuinely single-driver,
//! crossed links (UART) keep their real directions.

use crate::part::{Dir, InterfaceDef, PartDef, PinDef};
use std::collections::BTreeMap;

/// One built-in interface pattern in the [`registry`].
///
/// `signals` are the **canonical** signal names (the spellings that end up as the
/// interface's `signals`/`mate`/`offsets` keys, e.g. `tx`, `swdio`). `aliases` maps a
/// **literal, upper-cased** pin-name spelling to the canonical signal it satisfies —
/// no fuzzy matching, no substring games; a spelling not in this table is simply not a
/// candidate. `mate` is expressed in canonical names.
pub struct IfacePattern {
    pub type_name: &'static str,
    /// canonical signal name -> direction
    pub signals: &'static [(&'static str, Dir)],
    /// how two instances mate, in canonical names (straight for a bus, crossed for UART)
    pub mate: &'static [(&'static str, &'static str)],
    /// upper-cased literal pin-name spelling -> canonical signal
    pub aliases: &'static [(&'static str, &'static str)],
}

use Dir::*;

/// The shipped built-in interface registry (issue 0010).
///
/// Deliberately tiny and literal: four well-known interfaces, each with the handful of
/// common spellings actually seen on vendor symbols. Aliases are matched **after**
/// stripping a leading/trailing instance index (see [`split_instance`]), so
/// `UART0_TX` matches via the `UART_TX` alias under instance key `0`.
pub const REGISTRY: &[IfacePattern] = &[
    // UART: the one genuinely crossed, single-driver link. tx drives rx; the swap is
    // unrepresentable because the mate is baked crossed.
    IfacePattern {
        type_name: "UART",
        signals: &[("tx", Out), ("rx", In)],
        mate: &[("tx", "rx"), ("rx", "tx")],
        aliases: &[
            ("TX", "tx"),
            ("TXD", "tx"),
            ("UART_TX", "tx"),
            ("UART_TXD", "tx"),
            ("RX", "rx"),
            ("RXD", "rx"),
            ("UART_RX", "rx"),
            ("UART_RXD", "rx"),
        ],
    },
    // SPI: shared clock + data; direction is controller/peripheral-dependent, so every
    // line is Bidir (see module docs) and the mate is straight by name.
    IfacePattern {
        type_name: "SPI",
        signals: &[
            ("sck", Bidir),
            ("mosi", Bidir),
            ("miso", Bidir),
            ("cs", Bidir),
        ],
        mate: &[
            ("sck", "sck"),
            ("mosi", "mosi"),
            ("miso", "miso"),
            ("cs", "cs"),
        ],
        aliases: &[
            ("SCK", "sck"),
            ("SCLK", "sck"),
            ("SPI_SCK", "sck"),
            ("SPI_CLK", "sck"),
            ("MOSI", "mosi"),
            ("SDO", "mosi"),
            ("SPI_MOSI", "mosi"),
            ("MISO", "miso"),
            ("SDI", "miso"),
            ("SPI_MISO", "miso"),
            ("CS", "cs"),
            ("NSS", "cs"),
            ("SS", "cs"),
            ("SPI_CS", "cs"),
        ],
    },
    // I2C: two shared open-drain lines; both Bidir, straight mate.
    IfacePattern {
        type_name: "I2C",
        signals: &[("sda", Bidir), ("scl", Bidir)],
        mate: &[("sda", "sda"), ("scl", "scl")],
        aliases: &[
            ("SDA", "sda"),
            ("I2C_SDA", "sda"),
            ("SCL", "scl"),
            ("I2C_SCL", "scl"),
        ],
    },
    // SWD: bidirectional data + clock; both Bidir, straight mate.
    IfacePattern {
        type_name: "SWD",
        signals: &[("swdio", Bidir), ("swclk", Bidir)],
        mate: &[("swdio", "swdio"), ("swclk", "swclk")],
        aliases: &[
            ("SWDIO", "swdio"),
            ("SWD_IO", "swdio"),
            ("SWCLK", "swclk"),
            ("SWDCLK", "swclk"),
            ("SWD_CLK", "swclk"),
        ],
    },
];

/// Split a pin name into `(instance_key, core)` where `core` is the spelling used for
/// alias lookup and `instance_key` is a per-interface disambiguator.
///
/// Instance indexing is **only** an embedded run of digits in a `_`-delimited token
/// that is otherwise an interface prefix: `UART0_TX` → key `"0"`, core `"UART_TX"`;
/// `UART1_RX` → key `"1"`, core `"UART_RX"`. A bare `TX` → key `""`, core `"TX"`. The
/// core is upper-cased for case-insensitive alias matching. We do NOT invent indices
/// from arbitrary trailing digits (e.g. a data line `SD0`/`SD1` is *not* instance
/// indexing — those are distinct canonical signals, handled via aliases, not here);
/// this stays literal to avoid the fuzzy-grouping the ticket warns against.
///
/// The rule: find the first maximal digit run that is bracketed by `_` or a
/// leading-alpha prefix and a trailing `_`. In practice we only split when the digits
/// immediately follow a leading alpha run and are immediately followed by `_`
/// (`UART0_`, `SPI1_`). Everything else is instance key `""`.
fn split_instance(name: &str) -> (String, String) {
    let bytes = name.as_bytes();
    // Leading alpha prefix.
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    // A digit run right after the alpha prefix...
    let d0 = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // ...immediately followed by `_` and more (the signal token).
    if d0 > 0 && i > d0 && i < bytes.len() && bytes[i] == b'_' {
        let inst = name[d0..i].to_string();
        // core = prefix + rest-after-index (drop the index, keep the `_` join).
        let core = format!("{}{}", &name[..d0], &name[i..]);
        return (inst, core.to_uppercase());
    }
    (String::new(), name.to_uppercase())
}

/// Build the interface *port* name for an attached interface: the lower-cased type
/// name, suffixed with the instance key when multi-instance (`uart`, or `uart0`).
fn port_name(type_name: &str, inst: &str) -> String {
    if inst.is_empty() {
        type_name.to_lowercase()
    } else {
        format!("{}{}", type_name.to_lowercase(), inst)
    }
}

/// Assemble an [`InterfaceDef`] for one matched instance from a canonical-signal →
/// [`PinDef`] grouping. `offsets` are copied from the real pads so the port resolves
/// to physical geometry exactly like a toy part's interface does.
fn build_iface(pat: &IfacePattern, group: &BTreeMap<&str, &PinDef>) -> InterfaceDef {
    let signals: BTreeMap<String, Dir> = pat
        .signals
        .iter()
        .map(|(s, d)| (s.to_string(), *d))
        .collect();
    let offsets: BTreeMap<String, crate::doc::Point> = pat
        .signals
        .iter()
        .map(|(s, _)| (s.to_string(), group[*s].offset))
        .collect();
    let mate: Vec<(String, String)> = pat
        .mate
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    InterfaceDef {
        type_name: pat.type_name.to_string(),
        signals,
        offsets,
        mate,
    }
}

/// Conservatively attach typed [`InterfaceDef`]s to `part` by matching pin names
/// against the built-in [`REGISTRY`] (issue 0010).
///
/// **Never guesses.** For each pattern and each instance key, an interface is attached
/// *only* when:
/// - every canonical signal of the pattern is present exactly once (a complete set),
///   and
/// - no canonical signal is claimed by more than one pin (a duplicate candidate — two
///   pins that both alias to `tx` under the same instance — attaches nothing for that
///   instance).
///
/// Multi-instance grouping is admitted only when unambiguous: `UART0_TX`/`UART0_RX`
/// and `UART1_TX`/`UART1_RX` attach `uart0` and `uart1` independently. A partial set
/// (a lone `SDA` with no `SCL`) attaches nothing. Existing interfaces on the part
/// (e.g. from a prior [`apply_interface`]) are **left untouched**: inference never
/// overwrites an explicit port, and it skips attaching a port whose name is already
/// taken.
///
/// Returns the number of interfaces newly attached (0 is the common, safe outcome).
pub fn infer_interfaces(part: &mut PartDef) -> usize {
    let mut attached = 0;
    for pat in REGISTRY {
        // canonical alias lookup for this pattern
        let alias: BTreeMap<&str, &str> = pat.aliases.iter().copied().collect();
        // (instance, canonical signal) -> candidate pins
        let mut groups: BTreeMap<String, BTreeMap<&str, Vec<&PinDef>>> = BTreeMap::new();
        for pin in &part.pins {
            let (inst, core) = split_instance(&pin.name);
            if let Some(&canon) = alias.get(core.as_str()) {
                groups
                    .entry(inst)
                    .or_default()
                    .entry(canon)
                    .or_default()
                    .push(pin);
            }
        }
        for (inst, by_sig) in &groups {
            // Complete set?
            let complete = pat.signals.iter().all(|(s, _)| by_sig.contains_key(s));
            if !complete {
                continue; // partial match: attach nothing
            }
            // Unambiguous? every canonical signal claimed by exactly one pin.
            let ambiguous = pat.signals.iter().any(|(s, _)| by_sig[s].len() != 1);
            if ambiguous {
                continue; // duplicate candidate: attach nothing for this instance
            }
            let port = port_name(pat.type_name, inst);
            if part.interfaces.contains_key(&port) {
                continue; // an explicit (or prior) port owns this name; never overwrite
            }
            let group: BTreeMap<&str, &PinDef> = pat
                .signals
                .iter()
                .map(|(s, _)| (*s, by_sig[s][0]))
                .collect();
            let iface = build_iface(pat, &group);
            part.interfaces.insert(port, iface);
            attached += 1;
        }
    }
    attached
}

/// A signal binding for the [`apply_interface`] explicit overlay: the **canonical**
/// signal name and the pin **selector** (a functional pin name) that provides it.
pub type SignalBinding<'a> = (&'a str, &'a str);

/// **Explicitly** attach a typed interface to `part` (issue 0010) — the twin of
/// [`apply_role_map`](crate::kicad::apply_role_map), for interfaces inference will not
/// or should not touch.
///
/// `type_name` selects the built-in pattern from the [`REGISTRY`] (case-insensitive,
/// so `"uart"` and `"UART"` both work); `port` is the port name to attach under; and
/// `bindings` maps each of the pattern's canonical signals to a pin selector — a
/// functional pin **name** on the part. The pattern supplies the directions and the
/// mate rules (so the crossing/bus semantics are the vetted registry ones, never
/// re-authored per call); this call supplies only *which pins* play each role.
///
/// Strict, like `apply_role_map`:
/// - unknown `type_name` → error,
/// - a binding for a signal the pattern does not define → error,
/// - a missing binding for a pattern signal → error (the set must be complete),
/// - a selector that resolves to anything other than exactly one pad → error (an
///   interface signal is one physical pin; a name matching zero or several pads is a
///   fault, not a silent pick),
/// - the `port` name already taken → error (never silently overwrite).
///
/// On success the interface's `offsets` come from the bound pads' real offsets, so it
/// resolves for connection/ERC exactly like an inferred or toy-part interface.
///
/// Note: this is the API surface. A textual/grammar spelling of the same directive is
/// a follow-up owned by the source-language work (the `def`/ports grammar, Decision
/// 21); this function is what that grammar would lower to. See the branch report.
pub fn apply_interface(
    part: &mut PartDef,
    type_name: &str,
    port: &str,
    bindings: &[SignalBinding],
) -> Result<(), String> {
    let pat = REGISTRY
        .iter()
        .find(|p| p.type_name.eq_ignore_ascii_case(type_name))
        .ok_or_else(|| format!("apply_interface: unknown interface type {type_name:?}"))?;
    if part.interfaces.contains_key(port) {
        return Err(format!(
            "apply_interface: part `{}` already has an interface port `{port}`",
            part.name
        ));
    }
    // Bindings must cover exactly the pattern's canonical signals.
    let mut bound: BTreeMap<&str, &str> = BTreeMap::new();
    for (sig, sel) in bindings {
        if !pat.signals.iter().any(|(s, _)| s == sig) {
            return Err(format!(
                "apply_interface: interface {} has no signal `{sig}`",
                pat.type_name
            ));
        }
        if bound.insert(sig, sel).is_some() {
            return Err(format!(
                "apply_interface: signal `{sig}` bound more than once"
            ));
        }
    }
    for (s, _) in pat.signals {
        if !bound.contains_key(s) {
            return Err(format!(
                "apply_interface: interface {} signal `{s}` is unbound (the set must be complete)",
                pat.type_name
            ));
        }
    }
    // Resolve each selector to exactly one pad and pull its real offset.
    let mut offsets: BTreeMap<String, crate::doc::Point> = BTreeMap::new();
    for (s, _) in pat.signals {
        let sel = bound[s];
        let ids = part.resolve_selector(sel);
        if ids.len() != 1 {
            return Err(format!(
                "apply_interface: selector `{sel}` for signal `{s}` resolves to {} pads (need exactly 1)",
                ids.len()
            ));
        }
        let off = part
            .pin_offset(&ids[0])
            .ok_or_else(|| format!("apply_interface: pad `{}` has no offset", ids[0]))?;
        offsets.insert(s.to_string(), off);
    }
    let signals: BTreeMap<String, Dir> = pat
        .signals
        .iter()
        .map(|(s, d)| (s.to_string(), *d))
        .collect();
    let mate: Vec<(String, String)> = pat
        .mate
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
    part.interfaces.insert(
        port.to_string(),
        InterfaceDef {
            type_name: pat.type_name.to_string(),
            signals,
            offsets,
            mate,
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Point;
    use crate::kicad::{import_footprint, import_symbol, join_symbol_footprint};
    use crate::part::PinRole;

    fn pin(name: &str, number: &str, off: Point) -> PinDef {
        PinDef {
            name: name.into(),
            number: number.into(),
            role: PinRole::Passive,
            offset: off,
            pad: None,
        }
    }

    fn part_with(pins: Vec<PinDef>) -> PartDef {
        PartDef {
            name: "P".into(),
            pins,
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        }
    }

    fn p(x: i64, y: i64) -> Point {
        Point { x, y }
    }

    #[test]
    fn split_instance_only_splits_prefixed_index() {
        assert_eq!(split_instance("UART0_TX"), ("0".into(), "UART_TX".into()));
        assert_eq!(split_instance("UART1_RX"), ("1".into(), "UART_RX".into()));
        assert_eq!(split_instance("TX"), ("".into(), "TX".into()));
        assert_eq!(split_instance("SWDIO"), ("".into(), "SWDIO".into()));
        // A trailing data-line digit is NOT instance indexing (no `_` after digits).
        assert_eq!(split_instance("SD0"), ("".into(), "SD0".into()));
        assert_eq!(split_instance("sda"), ("".into(), "SDA".into()));
    }

    #[test]
    fn uart_complete_set_attaches_crossed_mate() {
        let mut part = part_with(vec![
            pin("TX", "1", p(1000, 0)),
            pin("RX", "2", p(1000, -1000)),
            pin("GND", "3", p(0, 0)),
        ]);
        assert_eq!(infer_interfaces(&mut part), 1);
        let iface = &part.interfaces["uart"];
        assert_eq!(iface.type_name, "UART");
        assert_eq!(iface.signals["tx"], Dir::Out);
        assert_eq!(iface.signals["rx"], Dir::In);
        // The crossing that makes the swap unrepresentable is baked in.
        assert_eq!(
            iface.mate,
            vec![("tx".into(), "rx".into()), ("rx".into(), "tx".into())]
        );
        // Offsets copied from the real pads → resolves to physical geometry.
        assert_eq!(part.pin_offset("uart.tx"), Some(p(1000, 0)));
        assert_eq!(part.pin_offset("uart.rx"), Some(p(1000, -1000)));
        // And the interface signal resolves for connection like a toy part.
        assert_eq!(part.pin_role("uart.tx"), Some(PinRole::Output));
        assert_eq!(
            part.resolve_selector("uart.tx"),
            vec!["uart.tx".to_string()]
        );
    }

    #[test]
    fn partial_set_attaches_nothing() {
        // I2C needs both SDA and SCL; a lone SDA is not enough.
        let mut part = part_with(vec![pin("SDA", "1", p(0, 0)), pin("VCC", "2", p(1, 1))]);
        assert_eq!(infer_interfaces(&mut part), 0);
        assert!(part.interfaces.is_empty());
    }

    #[test]
    fn duplicate_candidate_attaches_nothing() {
        // Two pins both alias to `tx` at the same (empty) instance key → ambiguous.
        let mut part = part_with(vec![
            pin("TX", "1", p(0, 0)),
            pin("TXD", "2", p(1, 0)), // second tx spelling, same instance
            pin("RX", "3", p(0, 1)),
        ]);
        assert_eq!(infer_interfaces(&mut part), 0);
        assert!(part.interfaces.is_empty());
    }

    #[test]
    fn indexed_multi_instance_grouping() {
        let mut part = part_with(vec![
            pin("UART0_TX", "1", p(0, 0)),
            pin("UART0_RX", "2", p(0, 1)),
            pin("UART1_TX", "3", p(1, 0)),
            pin("UART1_RX", "4", p(1, 1)),
        ]);
        assert_eq!(infer_interfaces(&mut part), 2);
        assert_eq!(part.interfaces["uart0"].type_name, "UART");
        assert_eq!(part.interfaces["uart1"].type_name, "UART");
        assert_eq!(part.pin_offset("uart0.tx"), Some(p(0, 0)));
        assert_eq!(part.pin_offset("uart1.rx"), Some(p(1, 1)));
    }

    #[test]
    fn spi_and_i2c_buses_are_bidir_straight_mate() {
        let mut part = part_with(vec![
            pin("SCK", "1", p(0, 0)),
            pin("MOSI", "2", p(0, 1)),
            pin("MISO", "3", p(0, 2)),
            pin("CS", "4", p(0, 3)),
            pin("SDA", "5", p(1, 0)),
            pin("SCL", "6", p(1, 1)),
        ]);
        assert_eq!(infer_interfaces(&mut part), 2);
        let spi = &part.interfaces["spi"];
        assert!(spi.signals.values().all(|d| *d == Dir::Bidir));
        assert!(spi.mate.iter().all(|(a, b)| a == b), "straight mate");
        let i2c = &part.interfaces["i2c"];
        assert_eq!(i2c.signals["sda"], Dir::Bidir);
        assert_eq!(i2c.signals["scl"], Dir::Bidir);
        // Straight mate, in registry-declaration order (sda, scl).
        assert_eq!(
            i2c.mate,
            vec![("sda".into(), "sda".into()), ("scl".into(), "scl".into())]
        );
    }

    #[test]
    fn explicit_overlay_binds_named_pins() {
        // Names the inference would never match (vendor's own spellings).
        let mut part = part_with(vec![
            pin("SERIAL_OUT", "1", p(5, 0)),
            pin("SERIAL_IN", "2", p(5, 1)),
        ]);
        assert_eq!(infer_interfaces(&mut part), 0, "no alias matches these");
        apply_interface(
            &mut part,
            "uart",
            "console",
            &[("tx", "SERIAL_OUT"), ("rx", "SERIAL_IN")],
        )
        .unwrap();
        let iface = &part.interfaces["console"];
        assert_eq!(iface.type_name, "UART");
        assert_eq!(part.pin_offset("console.tx"), Some(p(5, 0)));
        assert_eq!(part.pin_role("console.rx"), Some(PinRole::Input));
    }

    #[test]
    fn explicit_overlay_coexists_with_and_overrides_inference() {
        // A part with an inferable UART plus a vendor-named second link.
        let mut part = part_with(vec![
            pin("TX", "1", p(0, 0)),
            pin("RX", "2", p(0, 1)),
            pin("DBG_OUT", "3", p(2, 0)),
            pin("DBG_IN", "4", p(2, 1)),
        ]);
        // Explicit first: claim the `uart` port name for the vendor pins.
        apply_interface(
            &mut part,
            "UART",
            "uart",
            &[("tx", "DBG_OUT"), ("rx", "DBG_IN")],
        )
        .unwrap();
        // Inference must NOT overwrite the taken port; the TX/RX pins find no free
        // `uart` name, so nothing new attaches.
        assert_eq!(infer_interfaces(&mut part), 0);
        // The explicit binding stands (points at the DBG pads, not TX/RX).
        assert_eq!(part.pin_offset("uart.tx"), Some(p(2, 0)));
    }

    #[test]
    fn explicit_overlay_strict_errors() {
        let mut part = part_with(vec![pin("A", "1", p(0, 0)), pin("B", "2", p(0, 1))]);
        // Unknown type.
        assert!(apply_interface(&mut part, "CAN", "c", &[]).is_err());
        // Unknown signal for the type.
        assert!(apply_interface(&mut part, "uart", "u", &[("clk", "A")]).is_err());
        // Incomplete set (rx unbound).
        assert!(apply_interface(&mut part, "uart", "u", &[("tx", "A")]).is_err());
        // Selector matching no pad.
        assert!(apply_interface(&mut part, "uart", "u", &[("tx", "A"), ("rx", "NOPE")]).is_err());
        // A good one, then a second claim on the same port name errors.
        apply_interface(&mut part, "uart", "u", &[("tx", "A"), ("rx", "B")]).unwrap();
        assert!(apply_interface(&mut part, "uart", "u", &[("tx", "A"), ("rx", "B")]).is_err());
    }

    /// An RP2350-style imported part gains SWD cleanly and does NOT hallucinate a UART
    /// (the real RP2350 UART is GPIO-muxed, so there are no fixed TX/RX pins — honest
    /// inference attaches nothing there). Built from the import path so it exercises
    /// `join_symbol_footprint` output, not a synthetic PartDef.
    #[test]
    fn imported_rp2350_style_gains_swd_and_uart_when_named() {
        let sym = r#"
(symbol "MCU"
    (pin bidirectional line (at 0 0 0) (length 1) (name "SWDIO") (number "1"))
    (pin bidirectional line (at 0 0 0) (length 1) (name "SWCLK") (number "2"))
    (pin output line (at 0 0 0) (length 1) (name "UART0_TX") (number "3"))
    (pin input line (at 0 0 0) (length 1) (name "UART0_RX") (number "4"))
    (pin passive line (at 0 0 0) (length 1) (name "GND") (number "5"))
    (pin bidirectional line (at 0 0 0) (length 1) (name "QSPI_SD0") (number "6"))
)"#;
        let fp = r#"
(footprint "MCU-FP"
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu"))
    (pad "3" smd rect (at 2 0) (size 1 1) (layers "F.Cu"))
    (pad "4" smd rect (at 3 0) (size 1 1) (layers "F.Cu"))
    (pad "5" smd rect (at 4 0) (size 1 1) (layers "F.Cu"))
    (pad "6" smd rect (at 5 0) (size 1 1) (layers "F.Cu"))
)"#;
        let symbol = import_symbol(sym).unwrap();
        let footprint = import_footprint(fp).unwrap();
        let mut part = join_symbol_footprint(&symbol, &footprint).part;
        assert!(part.interfaces.is_empty(), "join attaches no interfaces");
        let n = infer_interfaces(&mut part);
        assert_eq!(
            n, 2,
            "SWD + UART0, but NOT a QSPI (lone SD0 is a partial set)"
        );
        assert_eq!(part.interfaces["swd"].type_name, "SWD");
        assert_eq!(part.interfaces["uart0"].type_name, "UART");
        assert!(
            !part.interfaces.keys().any(|k| k.starts_with("spi")),
            "a lone QSPI_SD0 must not hallucinate an SPI"
        );
        // SWD signal resolves to the real pad offset (pad 1 at origin).
        assert_eq!(part.pin_offset("swd.swdio"), Some(p(0, 0)));
        assert_eq!(part.pin_offset("uart0.tx"), Some(p(2_000_000, 0)));
    }

    /// End-to-end (item 4): an interface attached by *inference* mates through the
    /// real `connect_interface`/netlist path exactly like a toy part. Two imported
    /// parts each gain a `uart` by inference; a `ConnectInterface` between them must
    /// produce the crossed tx→rx net and make the tx↔tx swap unexpressible — the same
    /// guarantee `interface_connection_crosses_tx_rx` checks for the toy library.
    #[test]
    fn inferred_interface_mates_crossed_through_netlist() {
        use crate::command::{Command, Transaction};
        use crate::doc::Doc;
        use crate::elaborate::GenDirective;
        use crate::history::History;
        use crate::part::PartLib;
        use crate::query::{Engine, Key};

        // A UART-bearing part built from the import path, then inferred.
        let uart_part = || {
            let sym = r#"
(symbol "X"
    (pin output line (at 0 0 0) (length 1) (name "TX") (number "1"))
    (pin input line (at 0 0 0) (length 1) (name "RX") (number "2"))
    (pin passive line (at 0 0 0) (length 1) (name "GND") (number "3"))
)"#;
            let fp = r#"
(footprint "X-FP"
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu"))
    (pad "3" smd rect (at 2 0) (size 1 1) (layers "F.Cu"))
)"#;
            let symbol = import_symbol(sym).unwrap();
            let footprint = import_footprint(fp).unwrap();
            let mut part = join_symbol_footprint(&symbol, &footprint).part;
            assert_eq!(infer_interfaces(&mut part), 1);
            part
        };

        let mut lib = PartLib::new();
        let mut a = uart_part();
        a.name = "PartA".into();
        let mut b = uart_part();
        b.name = "PartB".into();
        lib.insert("PartA".into(), a);
        lib.insert("PartB".into(), b);

        let src = vec![
            GenDirective::Instance {
                path: "u1".into(),
                part: "PartA".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::Instance {
                path: "u2".into(),
                part: "PartB".into(),
                params: BTreeMap::new(),
                label: None,
            },
            GenDirective::ConnectInterface {
                a: ("u1".into(), "uart".into()),
                b: ("u2".into(), "uart".into()),
            },
        ];
        let mut h = History::new(Doc::default());
        h.commit(Transaction::one(Command::SetSource(src)), &lib, "s")
            .unwrap();
        let mut eng = Engine::new();
        let nl = eng.query(h.doc(), &lib, Key::Netlist);
        let nl = nl.as_netlist();
        let tx_net = nl
            .iter()
            .find(|(_, pins)| {
                pins.iter()
                    .any(|(p, _)| p.pin == "uart.tx" && p.comp.as_str() == "u1")
            })
            .expect("u1 tx net");
        let names: Vec<String> = tx_net
            .1
            .iter()
            .map(|(p, _)| format!("{}.{}", p.comp, p.pin))
            .collect();
        // Crossed: u1.tx shares a net with u2.rx, never u2.tx.
        assert!(names.contains(&"u2.uart.rx".to_string()), "got {names:?}");
        assert!(!names.contains(&"u2.uart.tx".to_string()), "got {names:?}");
    }
}
