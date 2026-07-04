//! Symbol / role layer.
//!
//! A KiCad **symbol** (`.kicad_sym`, also an S-expression — so we reuse the
//! tokenizer/reader in [`super::sexp`], no second parser) carries exactly the
//! electrical information a footprint lacks: each pin has an *electrical type*
//! (input, power_in, ...), a *functional name* (`GPIO0`, `VDD`, `SWCLK`) and a
//! *pad number* (`12`) that joins it to a footprint pad. This layer:
//!
//!   1. parses a symbol into an intermediate [`Symbol`] (`number`, `name`, type),
//!   2. maps the electrical type to a [`PinRole`] ([`ElecType::role`]),
//!   3. joins a symbol with an imported footprint *by pad number* into a real
//!      [`PartDef`] whose pins carry the functional name + role (from the symbol)
//!      and the offset (from the footprint pad geometry).

use crate::part::{PartDef, PinDef, PinRole};
use std::collections::BTreeMap;

use super::footprint::import_footprint;
use super::sexp::{Sexp, read, tokenize};

/// A pin's electrical type, as spelled in `(pin <type> <style> ...)`.
///
/// This is the closed KiCad vocabulary; an unknown token is a parse error rather
/// than a silent default, so a new KiCad type can't quietly map to `Passive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElecType {
    Input,
    Output,
    Bidirectional,
    TriState,
    Passive,
    Free,
    Unspecified,
    PowerIn,
    PowerOut,
    OpenCollector,
    OpenEmitter,
    NoConnect,
}

impl ElecType {
    pub(crate) fn parse(s: &str) -> Result<ElecType, String> {
        Ok(match s {
            "input" => ElecType::Input,
            "output" => ElecType::Output,
            "bidirectional" => ElecType::Bidirectional,
            "tri_state" => ElecType::TriState,
            "passive" => ElecType::Passive,
            "free" => ElecType::Free,
            "unspecified" => ElecType::Unspecified,
            "power_in" => ElecType::PowerIn,
            "power_out" => ElecType::PowerOut,
            "open_collector" => ElecType::OpenCollector,
            "open_emitter" => ElecType::OpenEmitter,
            "no_connect" => ElecType::NoConnect,
            other => return Err(format!("unknown pin electrical type {other:?}")),
        })
    }

    /// Map a KiCad electrical type onto this prototype's [`PinRole`] (the alphabet
    /// ERC type-checks over).
    ///
    /// The four directional/power types map exactly. Everything else collapses to
    /// [`PinRole::Passive`] — a *deliberate conservative default*:
    /// - `passive`, `free`, `unspecified`, `no_connect` have no driving role.
    /// - `tri_state`, `open_collector`, `open_emitter` *can* drive under some
    ///   conditions, but modelling that needs bus/wired-OR semantics ERC doesn't
    ///   have yet. Calling them `Passive` is the safe choice: it never invents a
    ///   spurious driver-vs-driver conflict. This is the documented place to
    ///   refine once ERC grows wired-OR rules.
    pub fn role(self) -> PinRole {
        match self {
            ElecType::PowerIn => PinRole::PowerIn,
            ElecType::PowerOut => PinRole::PowerOut,
            ElecType::Output => PinRole::Output,
            ElecType::Input => PinRole::Input,
            ElecType::Bidirectional => PinRole::Bidir,
            ElecType::TriState
            | ElecType::Passive
            | ElecType::Free
            | ElecType::Unspecified
            | ElecType::OpenCollector
            | ElecType::OpenEmitter
            | ElecType::NoConnect => PinRole::Passive,
        }
    }
}

/// One pin of a schematic symbol: the manufacturing `number` (join key), the
/// `name` (functional), and its electrical `etype`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolPin {
    pub number: String,
    pub name: String,
    pub etype: ElecType,
}

/// A parsed schematic symbol: its name plus its pins (flattened across units).
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub pins: Vec<SymbolPin>,
    /// The `(property "Footprint" "Lib:Name")` value, if present — the symbol's
    /// own declaration of which footprint it mates with. Useful for locating the
    /// matching `.kicad_mod`.
    pub footprint: Option<String>,
}

/// Result of joining a symbol with a footprint. The [`PartDef`] is built from the
/// footprint's pads (geometry is the manufacturing truth), enriched with symbol
/// names+roles where numbers match. Mismatches are reported, never silently
/// dropped — see [`join_symbol_footprint`].
#[derive(Clone, Debug)]
pub struct JoinReport {
    pub part: PartDef,
    /// Symbol pin numbers with no matching footprint pad (e.g. a power pin the
    /// footprint doesn't expose). `(number, name, role)` so a dropped power pin is
    /// visible to the caller.
    pub symbol_only: Vec<(String, String, PinRole)>,
    /// Footprint pad numbers with no matching symbol pin: kept in the part as
    /// `Passive`, name = number (no functional identity available).
    pub footprint_only: Vec<String>,
}

/// Extract the pins of one symbol `(symbol ...)` node, descending into nested
/// child unit symbols (`(symbol "Name_0_1" ...)`). Pins are deduped by `number`,
/// keeping the first occurrence (multi-unit parts can repeat a number, e.g. a
/// shared power pin); a later differing definition is ignored.
fn collect_symbol_pins(node: &[Sexp], out: &mut Vec<SymbolPin>, seen: &mut BTreeMap<String, ()>) {
    for item in node {
        if let Some(pin) = item.list_headed("pin") {
            // (pin <etype> <graphic-style> (at ..) (length ..) (name "..") (number ".."))
            let etype_tok = pin.get(1).and_then(Sexp::as_atom).unwrap_or("");
            let etype = match ElecType::parse(etype_tok) {
                Ok(t) => t,
                Err(_) => continue, // tolerate odd entries; not all (pin ..) are electrical
            };
            let name = pin
                .iter()
                .find_map(|s| s.list_headed("name"))
                .and_then(|l| l.get(1))
                .and_then(Sexp::as_atom)
                .unwrap_or("")
                .to_string();
            let number = pin
                .iter()
                .find_map(|s| s.list_headed("number"))
                .and_then(|l| l.get(1))
                .and_then(Sexp::as_atom)
                .unwrap_or("")
                .to_string();
            if number.is_empty() {
                continue; // a pin with no pad number can't join to a footprint
            }
            if seen.insert(number.clone(), ()).is_some() {
                continue; // first definition of this number wins
            }
            out.push(SymbolPin {
                number,
                name,
                etype,
            });
        } else if let Some(child) = item.list_headed("symbol") {
            // Nested unit symbol — recurse to gather its pins too.
            collect_symbol_pins(child, out, seen);
        }
    }
}

/// Build a [`Symbol`] from an already-parsed `(symbol "Name" ...)` node.
fn symbol_from_node(node: &[Sexp]) -> Result<Symbol, String> {
    let name = node
        .get(1)
        .and_then(Sexp::as_atom)
        .ok_or("symbol is missing its name")?
        .to_string();
    if name.is_empty() {
        return Err("symbol name is empty".into());
    }
    // (property "Footprint" "Lib:Name" ...)
    let footprint = node.iter().find_map(|s| {
        let p = s.list_headed("property")?;
        match p.get(1).and_then(Sexp::as_atom) {
            Some("Footprint") => p.get(2).and_then(Sexp::as_atom).filter(|v| !v.is_empty()),
            _ => None,
        }
    });
    let mut pins = Vec::new();
    let mut seen = BTreeMap::new();
    collect_symbol_pins(node, &mut pins, &mut seen);
    Ok(Symbol {
        name,
        pins,
        footprint: footprint.map(str::to_string),
    })
}

/// Find every top-level `(symbol "Name" ...)` node in a parsed root, which is
/// either a `(kicad_symbol_lib ... (symbol ...) ...)` library or a bare
/// `(symbol ...)`.
fn top_level_symbols(root: &Sexp) -> Vec<&[Sexp]> {
    let Some(items) = root.as_list() else {
        return Vec::new();
    };
    match items.first().and_then(Sexp::as_atom) {
        Some("symbol") => vec![items],
        Some("kicad_symbol_lib") => items
            .iter()
            .filter_map(|s| s.list_headed("symbol"))
            .collect(),
        _ => Vec::new(),
    }
}

/// Import the **first** symbol from `.kicad_sym` text (a bare `(symbol ...)` or a
/// `(kicad_symbol_lib ...)` with one or more symbols).
pub fn import_symbol(text: &str) -> Result<Symbol, String> {
    let toks = tokenize(text)?;
    let root = read(&toks)?;
    let node = *top_level_symbols(&root)
        .first()
        .ok_or("no (symbol ...) found in input")?;
    symbol_from_node(node)
}

/// Import a specific named symbol from a `.kicad_sym` library — needed because a
/// real library holds many symbols.
pub fn import_symbol_named(text: &str, name: &str) -> Result<Symbol, String> {
    let toks = tokenize(text)?;
    let root = read(&toks)?;
    let node = top_level_symbols(&root)
        .into_iter()
        .find(|n| n.get(1).and_then(Sexp::as_atom) == Some(name))
        .ok_or_else(|| format!("symbol {name:?} not found in library"))?;
    symbol_from_node(node)
}

/// Join a parsed [`Symbol`] with an imported footprint [`PartDef`] **by pad
/// number** into a real part. Tolerant: it always produces a part and *reports*
/// any mismatches (never silently drops a pin) — see [`JoinReport`].
///
/// The footprint is the geometry source of truth: the result has one pin per
/// footprint pad. Where the symbol has a pin with the same `number`, that pin
/// takes the symbol's functional **name** and mapped **role**; the **offset**
/// always comes from the footprint pad. Pads with no symbol match stay `Passive`
/// with name = number.
pub fn join_symbol_footprint(symbol: &Symbol, footprint: &PartDef) -> JoinReport {
    let by_number: BTreeMap<&str, &SymbolPin> =
        symbol.pins.iter().map(|p| (p.number.as_str(), p)).collect();
    let mut footprint_only = Vec::new();
    let mut matched: BTreeMap<&str, ()> = BTreeMap::new();

    let mut pins = Vec::with_capacity(footprint.pins.len());
    for pad in &footprint.pins {
        // A footprint PinDef has number == name == pad id.
        match by_number.get(pad.number.as_str()) {
            Some(sp) => {
                matched.insert(pad.number.as_str(), ());
                pins.push(PinDef {
                    name: sp.name.clone(),
                    number: pad.number.clone(),
                    role: sp.etype.role(),
                    offset: pad.offset,
                    pad: pad.pad.clone(), // copper geometry always comes from the footprint
                });
            }
            None => {
                footprint_only.push(pad.number.clone());
                pins.push(PinDef {
                    name: pad.name.clone(),
                    number: pad.number.clone(),
                    role: PinRole::Passive,
                    offset: pad.offset,
                    pad: pad.pad.clone(),
                });
            }
        }
    }

    let symbol_only: Vec<(String, String, PinRole)> = symbol
        .pins
        .iter()
        .filter(|p| !matched.contains_key(p.number.as_str()))
        .map(|p| (p.number.clone(), p.name.clone(), p.etype.role()))
        .collect();

    // Name the part after the footprint (the manufacturable artifact).
    let mut part = PartDef {
        name: footprint.name.clone(),
        pins,
        interfaces: BTreeMap::new(),
        // Silk/courtyard geometry is the footprint's, carried through the join.
        graphics: footprint.graphics.clone(),
        texts: footprint.texts.clone(),
        courtyard: footprint.courtyard.clone(),
        class: None,
    };
    // Conservatively attach typed interfaces from the now-named pins (issue 0010).
    // This is a no-op for any part whose pin names don't form a complete, unambiguous
    // registry signal set — the common case — so it never invents an interface; when
    // it does fire, the pad-number binding keeps interface + discrete identity unified
    // (see [`iface_infer`](super::iface_infer)).
    super::iface_infer::infer_interfaces(&mut part);
    JoinReport {
        part,
        symbol_only,
        footprint_only,
    }
}

/// Convenience: parse the first symbol + the footprint, join them, and return the
/// part. **Strict** — any pin mismatch (a symbol pin with no pad, or a pad with no
/// symbol pin) is returned as an `Err` naming the offending numbers, so a missing
/// power pin can never pass unnoticed. Callers that want to tolerate mismatches
/// should parse + [`join_symbol_footprint`] and inspect the [`JoinReport`].
pub fn import_part(symbol_text: &str, footprint_text: &str) -> Result<PartDef, String> {
    let symbol = import_symbol(symbol_text)?;
    let footprint = import_footprint(footprint_text)?;
    let report = join_symbol_footprint(&symbol, &footprint);
    if !report.symbol_only.is_empty() || !report.footprint_only.is_empty() {
        let sym: Vec<String> = report
            .symbol_only
            .iter()
            .map(|(n, name, role)| format!("{n}({name},{role:?})"))
            .collect();
        return Err(format!(
            "symbol/footprint pin mismatch joining {:?}: symbol-only pads {:?}, footprint-only pads {:?}",
            footprint.name, sym, report.footprint_only
        ));
    }
    Ok(report.part)
}

/// Overlay functional names + electrical roles onto an imported (role-less)
/// footprint, keyed by pad **number** — a lightweight stand-in for a full symbol
/// when none exists (the common case for jellybean parts: regulators, crystals,
/// flash). Each `(number, name, role)` entry renames and roles the pad with that
/// number; pads not in the map keep their imported `(numeric name, Passive)`
/// identity. Returns an error naming any entry whose pad number is absent, so a
/// typo in the role map is a hard fault, not a silent no-op.
///
/// This is the first-class form of the per-pad role assignment that issue 0002
/// called for — the alternative to authoring a whole `.kicad_sym`. It composes with
/// [`resolve_selector`](crate::part::PartDef::resolve_selector): assign a shared
/// name to several pads here and connecting that name nets all of them.
pub fn apply_role_map(mut part: PartDef, map: &[(&str, &str, PinRole)]) -> Result<PartDef, String> {
    for (num, name, role) in map {
        let mut hit = false;
        for p in part.pins.iter_mut() {
            if p.number == *num {
                p.name = (*name).to_string();
                p.role = *role;
                hit = true;
            }
        }
        if !hit {
            return Err(format!(
                "apply_role_map: part `{}` has no pad `{num}`",
                part.name
            ));
        }
    }
    Ok(part)
}
