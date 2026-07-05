//! The self-contained S-expression layer shared by every KiCad importer.
//!
//! A `.kicad_mod`/`.kicad_pcb`/`.kicad_sym` file is a single S-expression. We
//! hand-roll a tiny tokenizer + recursive reader (zero dependencies — no
//! serde/sexp crates) and a fixed-point mm→nm converter. This module has no
//! dependency on `part`/`geom` beyond the coordinate ceiling check.

use crate::doc::Nm;

/// A parsed S-expression node: either a leaf atom or a parenthesised list.
///
/// Quoted strings and bare tokens both become [`Sexp::Atom`]; the only quoted
/// value that matters to us is the empty string `""`, which a bare token can
/// never produce, so collapsing them is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

impl Sexp {
    pub(crate) fn as_atom(&self) -> Option<&str> {
        match self {
            Sexp::Atom(s) => Some(s),
            Sexp::List(_) => None,
        }
    }
    pub(crate) fn as_list(&self) -> Option<&[Sexp]> {
        match self {
            Sexp::List(v) => Some(v),
            Sexp::Atom(_) => None,
        }
    }
    /// If this is a list whose head atom equals `head`, return its elements.
    pub(crate) fn list_headed(&self, head: &str) -> Option<&[Sexp]> {
        let v = self.as_list()?;
        match v.first() {
            Some(Sexp::Atom(a)) if a == head => Some(v),
            _ => None,
        }
    }
}

// --- tokenizer ---------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    Open,
    Close,
    Atom(String),
}

/// Tokenize an S-expression. Whitespace separates atoms; `(`/`)` are structural;
/// `"..."` is a quoted atom with backslash escapes (`\"`, `\\`, `\n`, ...).
pub(crate) fn tokenize(text: &str) -> Result<Vec<Tok>, String> {
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
pub(crate) fn read(toks: &[Tok]) -> Result<Sexp, String> {
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
pub(crate) fn mm_to_nm(s: &str) -> Result<Nm, String> {
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
        int_part
            .parse()
            .map_err(|_| format!("integer overflow: {s:?}"))?
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
    let nm = if neg { -nm } else { nm };
    // Enforce the crate-wide coordinate ceiling at the import boundary (issue 0018): an
    // out-of-range imported coordinate is a clean error here, never a silent i128 wrap
    // in the geometry kernel downstream. Every kicad length/coordinate funnels through
    // this converter, so one check covers pads, graphics, drills, and outlines.
    if !crate::geom::coord_ok(nm) {
        return Err(format!(
            "coordinate {nm} nm exceeds the ±{} nm (±1 m) range (issue 0018)",
            crate::geom::MAX_COORD
        ));
    }
    Ok(nm)
}
