//! Import KiCad footprints (`.kicad_mod`) into the part model.
//!
//! A `.kicad_mod` file is a single S-expression. We hand-roll a tiny tokenizer +
//! recursive reader (zero dependencies — no serde/sexp crates) and lift the parts
//! we care about into a [`PartDef`].
//!
//! ## What a footprint *is* (and is not)
//! A footprint is **geometry**: copper pads at positions, silkscreen, courtyard,
//! 3D models. It carries **no electrical roles** — whether a pad is power, an
//! input, or passive comes from the *schematic symbol*, not the footprint. So
//! every imported pin gets [`PinRole::Passive`]; roles must be supplied elsewhere
//! when a footprint is paired with a symbol. Likewise a footprint defines no
//! typed [`InterfaceDef`]s, so `PartDef.interfaces` is always empty here.
//!
//! What we *do* import is the pad-to-pin geometry: one [`PinDef`] per pad, named
//! by the pad's number/name, positioned at the pad's `(at x y)` converted mm→nm.
//!
//! ## Mapping decisions (documented contract)
//! - **Shared pad names** (e.g. two `MP` mounting pads, or a split thermal pad
//!   that reuses one number): we keep the **first** occurrence and drop later pads
//!   with an already-seen name. They are the same electrical pin, and a duplicate
//!   pin name would silently break `PartDef::pin_offset`/`pin_role`, which resolve
//!   by first match.
//! - **Unnamed pads** (`name == ""`, used for thermal/exposed pads and mechanical
//!   features): **skipped**. An empty name carries no electrical identity, and a
//!   footprint's roles come from the symbol anyway.
//! - The pad rotation in `(at x y angle)` is **ignored** for the offset (we import
//!   the pad *position* only).
//!
//! Both the modern `(footprint "name" ...)` and the legacy `(module name ...)`
//! headers are accepted; pad names may be quoted or bare.

use crate::doc::{Nm, Point};
use crate::part::{Pad, PadShape, PartDef, PinDef, PinRole};
use std::collections::BTreeMap;

/// A parsed S-expression node: either a leaf atom or a parenthesised list.
///
/// Quoted strings and bare tokens both become [`Sexp::Atom`]; the only quoted
/// value that matters to us is the empty string `""`, which a bare token can
/// never produce, so collapsing them is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

impl Sexp {
    fn as_atom(&self) -> Option<&str> {
        match self {
            Sexp::Atom(s) => Some(s),
            Sexp::List(_) => None,
        }
    }
    fn as_list(&self) -> Option<&[Sexp]> {
        match self {
            Sexp::List(v) => Some(v),
            Sexp::Atom(_) => None,
        }
    }
    /// If this is a list whose head atom equals `head`, return its elements.
    fn list_headed(&self, head: &str) -> Option<&[Sexp]> {
        let v = self.as_list()?;
        match v.first() {
            Some(Sexp::Atom(a)) if a == head => Some(v),
            _ => None,
        }
    }
}

// --- tokenizer ---------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    Atom(String),
}

/// Tokenize an S-expression. Whitespace separates atoms; `(`/`)` are structural;
/// `"..."` is a quoted atom with backslash escapes (`\"`, `\\`, `\n`, ...).
fn tokenize(text: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '(' => {
                chars.next();
                toks.push(Tok::Open);
            }
            ')' => {
                chars.next();
                toks.push(Tok::Close);
            }
            '"' => {
                chars.next(); // opening quote
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => return Err("unterminated quoted string".into()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            None => return Err("trailing escape in quoted string".into()),
                            // Keep it simple: the escaped char is taken literally,
                            // which is all footprint strings need.
                            Some(e) => s.push(e),
                        },
                        Some(other) => s.push(other),
                    }
                }
                toks.push(Tok::Atom(s));
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                toks.push(Tok::Atom(s));
            }
        }
    }
    Ok(toks)
}

// --- reader ------------------------------------------------------------------

/// Read the single top-level S-expression. Errors on missing/extra parens or
/// trailing content.
fn read(toks: &[Tok]) -> Result<Sexp, String> {
    let mut pos = 0usize;
    if toks.first() != Some(&Tok::Open) {
        return Err("expected '(' at start".into());
    }
    let node = read_list(toks, &mut pos)?;
    if pos != toks.len() {
        return Err("trailing tokens after top-level expression".into());
    }
    Ok(node)
}

/// Read one node starting at `*pos` (which must point at an `Open` for a list).
fn read_node(toks: &[Tok], pos: &mut usize) -> Result<Sexp, String> {
    match toks.get(*pos) {
        None => Err("unexpected end of input".into()),
        Some(Tok::Open) => read_list(toks, pos),
        Some(Tok::Close) => Err("unexpected ')'".into()),
        Some(Tok::Atom(a)) => {
            let a = a.clone();
            *pos += 1;
            Ok(Sexp::Atom(a))
        }
    }
}

fn read_list(toks: &[Tok], pos: &mut usize) -> Result<Sexp, String> {
    debug_assert_eq!(toks.get(*pos), Some(&Tok::Open));
    *pos += 1; // consume '('
    let mut items = Vec::new();
    loop {
        match toks.get(*pos) {
            None => return Err("unterminated list (missing ')')".into()),
            Some(Tok::Close) => {
                *pos += 1;
                return Ok(Sexp::List(items));
            }
            Some(_) => items.push(read_node(toks, pos)?),
        }
    }
}

// --- mm → nm -----------------------------------------------------------------

/// Convert a decimal millimetre string to integer nanometres (×1_000_000),
/// rounding half-away-from-zero. Parsed by hand (no float) so coordinates stay
/// exact integers, matching the project's fixed-point invariant.
fn mm_to_nm(s: &str) -> Result<Nm, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty number".into());
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!("malformed number: {s:?}"));
    }
    let digits_ok = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok(int_part) || !digits_ok(frac_part) {
        return Err(format!("non-numeric coordinate: {s:?}"));
    }
    let int_val: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| format!("integer overflow: {s:?}"))?
    };
    // Take 6 fractional digits (1 mm = 1e6 nm); the 7th decides rounding.
    let mut frac6: i64 = 0;
    for i in 0..6 {
        frac6 = frac6 * 10 + frac_part.as_bytes().get(i).map_or(0, |b| (b - b'0') as i64);
    }
    let round_up = frac_part.as_bytes().get(6).is_some_and(|b| *b >= b'5');
    let mut nm = int_val
        .checked_mul(1_000_000)
        .and_then(|v| v.checked_add(frac6))
        .ok_or_else(|| format!("coordinate overflow: {s:?}"))?;
    if round_up {
        nm += 1;
    }
    Ok(if neg { -nm } else { nm })
}

// --- footprint → PartDef -----------------------------------------------------

/// Parse a `.kicad_mod` S-expression and produce a [`PartDef`].
///
/// See the module docs for the pad→pin mapping rules (shared names deduped,
/// unnamed pads skipped, roles defaulted to [`PinRole::Passive`], no interfaces).
pub fn import_footprint(text: &str) -> Result<PartDef, String> {
    let toks = tokenize(text)?;
    let root = read(&toks)?;
    let items = root.as_list().ok_or("top-level expression is not a list")?;

    // Header: `(footprint "name" ...)` or legacy `(module name ...)`.
    match items.first().and_then(Sexp::as_atom) {
        Some("footprint") | Some("module") => {}
        other => return Err(format!("expected 'footprint' or 'module', got {other:?}")),
    }
    let name = items
        .get(1)
        .and_then(Sexp::as_atom)
        .ok_or("footprint is missing its name")?
        .to_string();
    if name.is_empty() {
        return Err("footprint name is empty".into());
    }

    let mut pins: Vec<PinDef> = Vec::new();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    for item in items {
        let Some(pad) = item.list_headed("pad") else { continue };
        // (pad <name> <type> <shape> ... (at x y [angle]) ...)
        let pad_name = pad.get(1).and_then(Sexp::as_atom).unwrap_or("");
        if pad_name.is_empty() {
            continue; // unnamed: thermal/exposed/mechanical — no electrical identity
        }
        if seen.insert(pad_name.to_string(), ()).is_some() {
            continue; // shared pad name: keep first occurrence
        }
        let at = pad
            .iter()
            .find_map(|s| s.list_headed("at"))
            .ok_or_else(|| format!("pad {pad_name:?} has no (at ...)"))?;
        let x = at
            .get(1)
            .and_then(Sexp::as_atom)
            .ok_or_else(|| format!("pad {pad_name:?} (at ...) missing x"))?;
        let y = at
            .get(2)
            .and_then(Sexp::as_atom)
            .ok_or_else(|| format!("pad {pad_name:?} (at ...) missing y"))?;
        let offset = Point { x: mm_to_nm(x)?, y: mm_to_nm(y)? };
        // Pad copper geometry for fab output: the shape token (the 4th element of
        // `(pad <name> <type> <shape> ...)`) and `(size w h)`. Render-only — it does
        // not affect roles/offsets. A pad missing its size carries no geometry.
        let pad = parse_pad_geometry(pad)?;
        // A bare footprint has no functional naming: name == number == the pad id.
        pins.push(PinDef {
            name: pad_name.to_string(),
            number: pad_name.to_string(),
            role: PinRole::Passive,
            offset,
            pad,
        });
    }

    Ok(PartDef { name, pins, interfaces: BTreeMap::new() })
}

/// Lift a pad's copper geometry — its [`PadShape`] and `(size w h)` — out of a
/// `(pad <name> <type> <shape> … (size w h) …)` node, for fab output only.
///
/// The shape is the 4th list element. The four shapes KiCad uses for copper pads
/// map directly to [`PadShape`]; `custom`/`trapezoid`/`chamfered_rect` and any
/// other token fall back to [`PadShape::Rect`] (the conservative bounding-box
/// approximation — sufficient for flashing copper). A pad with **no** `(size …)`
/// yields `None` (no geometry to flash), which is not an error.
fn parse_pad_geometry(pad: &[Sexp]) -> Result<Option<Pad>, String> {
    let Some(size) = pad.iter().find_map(|s| s.list_headed("size")) else {
        return Ok(None);
    };
    let w = size.get(1).and_then(Sexp::as_atom).ok_or("pad (size …) missing width")?;
    let h = size.get(2).and_then(Sexp::as_atom).ok_or("pad (size …) missing height")?;
    let shape = match pad.get(3).and_then(Sexp::as_atom) {
        Some("circle") => PadShape::Circle,
        Some("rect") => PadShape::Rect,
        Some("roundrect") => PadShape::RoundRect,
        Some("oval") => PadShape::Oval,
        // Unknown/complex shapes (custom, trapezoid, chamfered_rect, …): treat the
        // pad as its bounding rectangle for flashing purposes.
        _ => PadShape::Rect,
    };
    Ok(Some(Pad { size: (mm_to_nm(w)?, mm_to_nm(h)?), shape }))
}

/// Convenience wrapper: read a `.kicad_mod` file from disk and import it.
pub fn import_footprint_file(path: &str) -> Result<PartDef, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    import_footprint(&text)
}

// =============================================================================
// Symbol / role layer
// =============================================================================
//
// A KiCad **symbol** (`.kicad_sym`, also an S-expression — so we reuse the
// tokenizer/reader above, no second parser) carries exactly the electrical
// information a footprint lacks: each pin has an *electrical type* (input,
// power_in, ...), a *functional name* (`GPIO0`, `VDD`, `SWCLK`) and a *pad
// number* (`12`) that joins it to a footprint pad. This layer:
//
//   1. parses a symbol into an intermediate [`Symbol`] (`number`, `name`, type),
//   2. maps the electrical type to a [`PinRole`] ([`ElecType::role`]),
//   3. joins a symbol with an imported footprint *by pad number* into a real
//      [`PartDef`] whose pins carry the functional name + role (from the symbol)
//      and the offset (from the footprint pad geometry).

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
    fn parse(s: &str) -> Result<ElecType, String> {
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
            out.push(SymbolPin { number, name, etype });
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
    Ok(Symbol { name, pins, footprint: footprint.map(str::to_string) })
}

/// Find every top-level `(symbol "Name" ...)` node in a parsed root, which is
/// either a `(kicad_symbol_lib ... (symbol ...) ...)` library or a bare
/// `(symbol ...)`.
fn top_level_symbols(root: &Sexp) -> Vec<&[Sexp]> {
    let Some(items) = root.as_list() else { return Vec::new() };
    match items.first().and_then(Sexp::as_atom) {
        Some("symbol") => vec![items],
        Some("kicad_symbol_lib") => {
            items.iter().filter_map(|s| s.list_headed("symbol")).collect()
        }
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
                    pad: pad.pad, // copper geometry always comes from the footprint
                });
            }
            None => {
                footprint_only.push(pad.number.clone());
                pins.push(PinDef {
                    name: pad.name.clone(),
                    number: pad.number.clone(),
                    role: PinRole::Passive,
                    offset: pad.offset,
                    pad: pad.pad,
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

    // Name the part after the footprint (the manufacturable artifact); roles and
    // interfaces beyond discrete pins are out of scope for this layer.
    let part = PartDef { name: footprint.name.clone(), pins, interfaces: BTreeMap::new() };
    JoinReport { part, symbol_only, footprint_only }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-contained footprint modelled on a real JST-SH 1x03 vertical header
    /// (`JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical`): three signal pads, two
    /// shared `MP` mounting pads, plus an unnamed exposed pad — trimmed of
    /// silkscreen/courtyard/3D noise but structurally faithful (nested parens,
    /// quoted name, multi-line pads).
    const JST_SH_1X03: &str = r#"
(footprint "JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical"
    (version 20241229)
    (generator "pcbnew")
    (layer "F.Cu")
    (descr "JST SH series connector (with parens) http://example.com")
    (attr smd)
    (fp_line
        (start -2.61 -0.04)
        (end -2.61 1.11)
        (stroke (width 0.12) (type solid))
        (layer "F.SilkS")
    )
    (pad "1" smd roundrect
        (at -1 1.325)
        (size 0.6 1.55)
        (layers "F.Cu" "F.Mask" "F.Paste")
        (roundrect_rratio 0.25)
    )
    (pad "2" smd roundrect
        (at 0 1.325)
        (size 0.6 1.55)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "3" smd roundrect
        (at 1 1.325)
        (size 0.6 1.55)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "MP" smd roundrect
        (at -2.3 -1.2)
        (size 1.2 1.8)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "MP" smd roundrect
        (at 2.3 -1.2)
        (size 1.2 1.8)
        (layers "F.Cu" "F.Mask" "F.Paste")
    )
    (pad "" smd roundrect
        (at 0 0)
        (size 0.3 0.3)
        (layers "F.Cu")
    )
    (model "${KICAD9_3DMODEL_DIR}/Connector_JST.3dshapes/x.step"
        (offset (xyz 0 0 0))
        (scale (xyz 1 1 1))
    )
)
"#;

    #[test]
    fn imports_jst_sh_name_and_pad_count() {
        let p = import_footprint(JST_SH_1X03).unwrap();
        assert_eq!(p.name, "JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical");
        // 1,2,3 + one deduped MP = 4; the two `MP` collapse, the `""` pad is skipped.
        assert_eq!(p.pins.len(), 4);
        let names: Vec<&str> = p.pins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["1", "2", "3", "MP"]);
        // No footprint carries electrical roles or interfaces.
        assert!(p.pins.iter().all(|pin| pin.role == PinRole::Passive));
        assert!(p.interfaces.is_empty());
    }

    #[test]
    fn imports_jst_sh_pad_offsets_in_nm() {
        let p = import_footprint(JST_SH_1X03).unwrap();
        // pad "1" at (-1, 1.325) mm
        assert_eq!(p.pin_offset("1"), Some(Point { x: -1_000_000, y: 1_325_000 }));
        // pad "3" at (1, 1.325) mm
        assert_eq!(p.pin_offset("3"), Some(Point { x: 1_000_000, y: 1_325_000 }));
        // first MP wins: (-2.3, -1.2) mm
        assert_eq!(p.pin_offset("MP"), Some(Point { x: -2_300_000, y: -1_200_000 }));
    }

    #[test]
    fn captures_pad_shape_and_size() {
        use crate::part::PadShape;
        let p = import_footprint(JST_SH_1X03).unwrap();
        // pad "1": roundrect, size 0.6 x 1.55 mm.
        let pad1 = p.pins.iter().find(|pin| pin.name == "1").unwrap().pad.unwrap();
        assert_eq!(pad1.shape, PadShape::RoundRect);
        assert_eq!(pad1.size, (600_000, 1_550_000));
        // A rect pad (FP_4) captures shape Rect and its size.
        let r = import_footprint(FP_4).unwrap();
        let a1 = r.pins.iter().find(|pin| pin.name == "1").unwrap().pad.unwrap();
        assert_eq!(a1.shape, PadShape::Rect);
        assert_eq!(a1.size, (500_000, 500_000));
        // Geometry rides through the symbol/footprint join (footprint is the source).
        let joined = import_part(SYM_LIB, FP_4).unwrap();
        let vdd = joined.pins.iter().find(|pin| pin.name == "VDD").unwrap();
        assert_eq!(vdd.pad.unwrap().shape, PadShape::Rect);
        assert_eq!(vdd.pad.unwrap().size, (500_000, 500_000));
    }

    #[test]
    fn pad_without_size_yields_no_geometry() {
        let src = r#"(footprint "X" (pad "1" smd circle (at 0 0) (layers "F.Cu")))"#;
        let p = import_footprint(src).unwrap();
        assert_eq!(p.pins[0].pad, None);
    }

    #[test]
    fn skips_unnamed_pad() {
        let p = import_footprint(JST_SH_1X03).unwrap();
        assert!(p.pins.iter().all(|pin| !pin.name.is_empty()));
    }

    #[test]
    fn accepts_legacy_module_header_and_bare_pad_names() {
        // Legacy single-line `(module ...)` form with unquoted name and bare pad
        // numbers, and a pad with a rotation angle in `(at x y angle)`.
        let src = r#"(module RP2040-QFN-56 (layer F.Cu) (tedit 5EF32B43)
            (descr "QFN")
            (pad 56 smd roundrect (at -2.6 -3.4375) (size 0.2 0.875) (layers F.Cu F.Mask))
            (pad 1 smd roundrect (at -1.2 -3.4375 90) (size 0.2 0.875) (layers F.Cu F.Mask)))"#;
        let p = import_footprint(src).unwrap();
        assert_eq!(p.name, "RP2040-QFN-56");
        assert_eq!(p.pins.len(), 2);
        assert_eq!(p.pin_offset("56"), Some(Point { x: -2_600_000, y: -3_437_500 }));
        // angle is ignored; only x/y become the offset.
        assert_eq!(p.pin_offset("1"), Some(Point { x: -1_200_000, y: -3_437_500 }));
    }

    #[test]
    fn quoted_name_with_spaces_is_preserved() {
        let src = r#"(footprint "Name With Spaces (rev 2)"
            (layer "F.Cu")
            (pad "A1" smd rect (at 0.5 -0.5) (size 1 1) (layers "F.Cu")))"#;
        let p = import_footprint(src).unwrap();
        assert_eq!(p.name, "Name With Spaces (rev 2)");
        assert_eq!(p.pins.len(), 1);
        assert_eq!(p.pin_offset("A1"), Some(Point { x: 500_000, y: -500_000 }));
    }

    #[test]
    fn rounds_sub_nm_fractional_mm() {
        // 7+ fractional digits: rounds half-away-from-zero at the nm.
        let src = r#"(footprint "R" (pad "1" smd rect (at 0.0000005 -0.0000004) (size 1 1)))"#;
        let p = import_footprint(src).unwrap();
        assert_eq!(p.pin_offset("1"), Some(Point { x: 1, y: 0 }));
    }

    #[test]
    fn malformed_inputs_return_err_not_panic() {
        assert!(import_footprint("(footprint").is_err()); // unterminated list
        assert!(import_footprint("").is_err()); // no expression
        assert!(import_footprint("(symbol \"foo\")").is_err()); // wrong head
        assert!(import_footprint("(footprint)").is_err()); // missing name
        assert!(import_footprint(r#"(footprint "x" (pad "1" smd (at)))"#).is_err()); // at missing x/y
        assert!(import_footprint(r#"(footprint "x" (pad "1" smd (at a b)))"#).is_err()); // non-numeric
        assert!(import_footprint(r#"(footprint "x" "unterminated)"#).is_err()); // bad quote
    }

    /// Optional smoke test over a real on-disk footprint. Guarded on existence so
    /// it is a no-op when the KiCad repo isn't present.
    #[test]
    fn real_file_smoke_test_if_present() {
        let path = "/home/ben/Documents/kalogon/git/Orbiter-Ultra-Hardware-multi_probe/Orbiter_Ultra.pretty/JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical.kicad_mod";
        if !std::path::Path::new(path).exists() {
            return;
        }
        let p = import_footprint_file(path).unwrap();
        assert_eq!(p.name, "JST_SH_BM03B-SRSS-TB_1x03-1MP_P1.00mm_Vertical");
        // 1,2,3 + deduped MP.
        assert_eq!(p.pins.len(), 4);
        assert_eq!(p.pin_offset("1"), Some(Point { x: -1_000_000, y: 1_325_000 }));
    }

    // --- symbol / role layer ------------------------------------------------

    /// A self-contained symbol modelled on a real `.kicad_sym`: a `kicad_symbol_lib`
    /// holding one multi-unit `(symbol ...)`. Pins are split across two child unit
    /// symbols (unit 0 = the power pin, unit 1 = the signal pins), each `(pin ...)`
    /// carrying an electrical type, a functional `(name ...)` and a pad `(number
    /// ...)` — and nested `(effects ...)` noise, like the real files.
    const SYM_LIB: &str = r#"
(kicad_symbol_lib
    (version 20241209)
    (generator "kicad_symbol_editor")
    (symbol "ACME1234"
        (pin_names (offset 0.254))
        (in_bom yes)
        (property "Reference" "U" (at 0 5 0))
        (property "Value" "ACME1234" (at 0 -5 0))
        (property "Footprint" "Acme:ACME-SOT-4" (at 0 -10 0) (effects (hide yes)))
        (symbol "ACME1234_0_1"
            (pin power_in line
                (at -7.62 2.54 0) (length 2.54)
                (name "VDD" (effects (font (size 1.27 1.27))))
                (number "1" (effects (font (size 1.27 1.27))))
            )
        )
        (symbol "ACME1234_1_1"
            (pin output line
                (at 7.62 2.54 180) (length 2.54)
                (name "GPIO0" (effects (font (size 1.27 1.27))))
                (number "2" (effects (font (size 1.27 1.27))))
            )
            (pin bidirectional line
                (at 7.62 0 180) (length 2.54)
                (name "SWDIO" (effects (font (size 1.27 1.27))))
                (number "3" (effects (font (size 1.27 1.27))))
            )
            (pin passive line
                (at 7.62 -2.54 180) (length 2.54)
                (name "GND" (effects (font (size 1.27 1.27))))
                (number "4" (effects (font (size 1.27 1.27))))
            )
        )
    )
)
"#;

    /// Footprint with four pads matching the symbol's numbers 1..4, at distinct
    /// positions so the join's offsets are checkable.
    const FP_4: &str = r#"
(footprint "ACME-SOT-4"
    (layer "F.Cu")
    (pad "1" smd rect (at -1 1) (size 0.5 0.5) (layers "F.Cu"))
    (pad "2" smd rect (at 1 1) (size 0.5 0.5) (layers "F.Cu"))
    (pad "3" smd rect (at 1 -1) (size 0.5 0.5) (layers "F.Cu"))
    (pad "4" smd rect (at -1 -1) (size 0.5 0.5) (layers "F.Cu"))
)
"#;

    #[test]
    fn parses_symbol_pins_across_units() {
        let s = import_symbol(SYM_LIB).unwrap();
        assert_eq!(s.name, "ACME1234");
        assert_eq!(s.footprint.as_deref(), Some("Acme:ACME-SOT-4"));
        // 1 pin in unit 0 + 3 pins in unit 1 = 4, gathered across the nesting.
        assert_eq!(s.pins.len(), 4);
        let by_num: std::collections::BTreeMap<&str, &SymbolPin> =
            s.pins.iter().map(|p| (p.number.as_str(), p)).collect();
        assert_eq!(by_num["1"].name, "VDD");
        assert_eq!(by_num["1"].etype, ElecType::PowerIn);
        assert_eq!(by_num["2"].name, "GPIO0");
        assert_eq!(by_num["3"].etype, ElecType::Bidirectional);
    }

    #[test]
    fn elec_type_to_role_mapping_table() {
        use PinRole::*;
        let cases = [
            ("power_in", PowerIn),
            ("power_out", PowerOut),
            ("output", Output),
            ("input", Input),
            ("bidirectional", Bidir),
            // Everything below collapses to Passive (documented conservative default).
            ("passive", Passive),
            ("free", Passive),
            ("unspecified", Passive),
            ("no_connect", Passive),
            ("tri_state", Passive),
            ("open_collector", Passive),
            ("open_emitter", Passive),
        ];
        for (tok, want) in cases {
            assert_eq!(ElecType::parse(tok).unwrap().role(), want, "type {tok}");
        }
        // Unknown type is an error, not a silent Passive.
        assert!(ElecType::parse("quantum").is_err());
    }

    #[test]
    fn join_pairs_names_roles_numbers_and_offsets() {
        let part = import_part(SYM_LIB, FP_4).unwrap();
        assert_eq!(part.name, "ACME-SOT-4");
        assert_eq!(part.pins.len(), 4);

        // Functional name resolves to symbol role; offset comes from the footprint.
        assert_eq!(part.pin_role("VDD"), Some(PinRole::PowerIn));
        assert_eq!(part.pin_offset("VDD"), Some(Point { x: -1_000_000, y: 1_000_000 }));
        assert_eq!(part.pin_role("GPIO0"), Some(PinRole::Output));
        assert_eq!(part.pin_role("SWDIO"), Some(PinRole::Bidir));
        assert_eq!(part.pin_role("GND"), Some(PinRole::Passive));
        assert_eq!(part.pin_offset("GND"), Some(Point { x: -1_000_000, y: -1_000_000 }));

        // Pad numbers preserved as the manufacturing/join key, distinct from names.
        let vdd = part.pins.iter().find(|p| p.name == "VDD").unwrap();
        assert_eq!(vdd.number, "1");
        let gpio = part.pins.iter().find(|p| p.name == "GPIO0").unwrap();
        assert_eq!(gpio.number, "2");
    }

    #[test]
    fn join_reports_mismatches_without_dropping_pins() {
        // Symbol has a power pin "5" with no pad; footprint has a pad "6" with no
        // symbol pin. Neither must be silently dropped.
        let sym = r#"
(symbol "X"
    (pin power_in line (at 0 0 0) (length 1) (name "VBUS") (number "5"))
    (pin input line (at 0 0 0) (length 1) (name "IN") (number "1"))
)"#;
        let fp = r#"
(footprint "X-FP"
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu"))
    (pad "6" smd rect (at 2 0) (size 1 1) (layers "F.Cu"))
)"#;
        let symbol = import_symbol(sym).unwrap();
        let footprint = import_footprint(fp).unwrap();
        let report = join_symbol_footprint(&symbol, &footprint);

        // The matched pin carries name + role; the unmatched pad stays Passive.
        assert_eq!(report.part.pin_role("IN"), Some(PinRole::Input));
        // The orphan power pin is surfaced (number, name, role), not dropped.
        assert_eq!(
            report.symbol_only,
            vec![("5".to_string(), "VBUS".to_string(), PinRole::PowerIn)]
        );
        // The orphan pad is surfaced and kept Passive with name = number.
        assert_eq!(report.footprint_only, vec!["6".to_string()]);
        let pad6 = report.part.pins.iter().find(|p| p.number == "6").unwrap();
        assert_eq!(pad6.role, PinRole::Passive);
        assert_eq!(pad6.name, "6");

        // The strict convenience wrapper turns any mismatch into an Err.
        assert!(import_part(sym, fp).is_err());
    }

    /// Real-data join: pair a real `.kicad_sym` symbol with the `.kicad_mod` its own
    /// `Footprint` property names. Guarded on existence (no-op without the repo).
    #[test]
    fn real_symbol_footprint_join_if_present() {
        let sym_path =
            "/home/ben/Documents/kalogon/git/Kalogon-KiCad-Repository/Power_Management_TI.kicad_sym";
        let fp_path = "/home/ben/Documents/kalogon/git/Kalogon-KiCad-Repository/footprints/eFuse_TI.pretty/Texas_RPW9919A_VQFN-HR-10.kicad_mod";
        if !std::path::Path::new(sym_path).exists() || !std::path::Path::new(fp_path).exists() {
            return;
        }
        let sym_text = std::fs::read_to_string(sym_path).unwrap();
        let symbol = import_symbol_named(&sym_text, "TPS25981x").unwrap();
        assert_eq!(symbol.footprint.as_deref(), Some("eFuse_TI:Texas_RPW9919A_VQFN-HR-10"));
        let footprint = import_footprint_file(fp_path).unwrap();
        let report = join_symbol_footprint(&symbol, &footprint);

        // Every footprint pad became a pin; a real power pin carries its role.
        assert!(!report.part.pins.is_empty());
        // IN is the eFuse input rail (power_in -> PowerIn).
        assert_eq!(report.part.pin_role("IN"), Some(PinRole::PowerIn));
        // OUT is the switched output rail (power_out -> PowerOut).
        assert_eq!(report.part.pin_role("OUT"), Some(PinRole::PowerOut));
        // PG is open_collector -> Passive (conservative default).
        assert_eq!(report.part.pin_role("PG"), Some(PinRole::Passive));
        // Exact 10/10 join: no orphan pins on either side.
        assert!(report.symbol_only.is_empty() && report.footprint_only.is_empty());
    }

    /// PoC Stage-1 gate: the authoritative RP2350A QFN-60 symbol + footprint
    /// (KiCad official library, vendored under poc/parts/) join cleanly into a
    /// 61-pin part with real RP2350 functions and roles. Guarded on the vendored
    /// files existing, so it is a no-op in a checkout without them.
    #[test]
    fn rp2350a_qfn60_join_if_present() {
        let sym_path = "poc/parts/MCU_RaspberryPi.kicad_sym";
        let fp_path = "poc/parts/RP2350A_QFN-60.kicad_mod";
        if !std::path::Path::new(sym_path).exists() || !std::path::Path::new(fp_path).exists() {
            return;
        }
        let sym = import_symbol_named(&std::fs::read_to_string(sym_path).unwrap(), "RP2350A").unwrap();
        let footprint = import_footprint_file(fp_path).unwrap();
        let report = join_symbol_footprint(&sym, &footprint);
        // 60 signal/power pads + the exposed pad = 61 pins, clean both ways.
        assert_eq!(report.part.pins.len(), 61);
        assert!(report.symbol_only.is_empty() && report.footprint_only.is_empty());
        // Real RP2350 functional names + roles survive the join.
        assert_eq!(report.part.pin_role("GPIO0"), Some(PinRole::Bidir));
        assert_eq!(report.part.pin_role("IOVDD"), Some(PinRole::PowerIn));
        assert_eq!(report.part.pin_role("VREG_LX"), Some(PinRole::PowerOut));
        assert!(report.part.pins.iter().any(|p| p.name == "USB_DP"));
        assert!(report.part.pins.iter().any(|p| p.name == "QSPI_SCLK"));
        // 6 IOVDD + 3 DVDD pads share a functional name (the duplicate-name case
        // the PoC must uniquify before it can net every power pad).
        assert_eq!(report.part.pins.iter().filter(|p| p.name == "IOVDD").count(), 6);
        assert_eq!(report.part.pins.iter().filter(|p| p.name == "DVDD").count(), 3);
    }
}
