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
use crate::part::{PartDef, PinDef, PinRole};
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
        pins.push(PinDef { name: pad_name.to_string(), role: PinRole::Passive, offset });
    }

    Ok(PartDef { name, pins, interfaces: BTreeMap::new() })
}

/// Convenience wrapper: read a `.kicad_mod` file from disk and import it.
pub fn import_footprint_file(path: &str) -> Result<PartDef, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    import_footprint(&text)
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
}
