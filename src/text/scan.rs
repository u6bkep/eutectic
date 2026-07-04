//! Pure token/value helpers and scalar/geometry parsers shared across the
//! directive, schematic, and def parsers. No domain state — strings in, values out.

use super::*;

/// Strip a trailing provenance keyword (`free`/`hint`/`fixed`) off a route/via line's
/// tail, returning `(remaining, provenance)`. No keyword ⇒ `Pinned` (the default,
/// Decision 18 — hand-authored routing is pinned). The keyword must be the **last**
/// whitespace token; anything else is left for the caller to parse.
pub(crate) fn split_trailing_prov(s: &str) -> Result<(&str, Provenance), String> {
    let t = s.trim_end();
    let prov = match t.rsplit(char::is_whitespace).next() {
        Some("free") => Provenance::Free,
        Some("hint") => Provenance::Hint,
        Some("fixed") => Provenance::Fixed,
        _ => return Ok((s, Provenance::Pinned)),
    };
    // Drop the keyword we just recognised (it is the final token).
    let cut = t.rfind(char::is_whitespace).map(|i| &t[..i]).unwrap_or("");
    Ok((cut, prov))
}

/// Quote a `key=value` token value only when it needs it: whitespace, a `#` (which the
/// comment stripper would otherwise eat), a double quote, or the empty string. Embedded
/// quotes are not escaped — the same documented limitation as `text`-label serialization.
pub(crate) fn quote_value(v: &str) -> String {
    let needs = v.is_empty() || v.chars().any(|c| c.is_whitespace() || c == '#' || c == '"');
    if needs {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

/// Quote a **verbatim `p:` param value** for emission. Like [`quote_value`], but *also*
/// forces quoting when the value starts with `(` — otherwise a literal value such as
/// `(5V)` would serialize bare and re-parse as an *expression* param (M2). A quoted value
/// re-parses as verbatim, closing that round-trip hole. (Expression params are emitted
/// separately as `p:k=(<expr>)` and never pass through here.)
pub(crate) fn quote_param_value(v: &str) -> String {
    if v.starts_with('(') && !quote_value(v).starts_with('"') {
        format!("\"{v}\"")
    } else {
        quote_value(v)
    }
}

/// Split on whitespace, but keep a double-quoted run (which may hold spaces) intact as
/// part of its token — so `p:desc="a b"` is one token. Quote characters are retained;
/// [`unquote`] strips them from an extracted value.
pub(crate) fn split_ws_quoted(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                cur.push(c);
            }
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

/// Like [`split_ws_quoted`], but *also* keeps a parenthesized run intact so an
/// expression clause with internal whitespace stays one token (`if=(n > 0)`,
/// `p:c=(i + 1)`). Depth-tracked (nested parens are balanced); a double-quoted run is
/// still respected too, and a `(` inside quotes does not open a paren group. Additive:
/// the plain [`split_ws_quoted`] path is unchanged, so directives that use it behave
/// exactly as before.
pub(crate) fn split_ws_quoted_parens(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut depth: i32 = 0;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                cur.push(c);
            }
            '(' if !in_q => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_q => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            c if c.is_whitespace() && !in_q && depth == 0 => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

/// Split a `[<lo>..<hi>]` **range** suffix off an instance path token. Returns the base
/// path and, when present, the `(lo, hi)` expression texts. Only a suffix containing
/// `..` is a range — an ordinary indexed path (`psu.dec[0]`) has no `..` and comes back
/// unchanged with `None`. The bracket must be the final characters of the token.
pub(crate) fn split_range_suffix(tok: &str) -> (String, Option<(String, String)>) {
    if let Some(open) = tok.rfind('[')
        && tok.ends_with(']')
    {
        let inner = &tok[open + 1..tok.len() - 1];
        if let Some((lo, hi)) = inner.split_once("..") {
            return (
                tok[..open].to_string(),
                Some((lo.trim().to_string(), hi.trim().to_string())),
            );
        }
    }
    (tok.to_string(), None)
}

/// Is the whole value a double-quoted run (`"..."`)? A quoted value is **always
/// verbatim** — never expression-detected (M2): quoting is the escape hatch that lets a
/// literal value legitimately start with `(` (`p:v="(5V)"`). We check quoting on the
/// *raw* token before any unquoting so the layering cannot be inverted.
pub(crate) fn is_quoted(v: &str) -> bool {
    v.len() >= 2 && v.starts_with('"') && v.ends_with('"')
}

/// Classify a raw `p:key=value` value (M2 — quote-then-paren layering):
///   - a **quoted** value is verbatim → `Ok(None)`;
///   - a bare value **starting with `(`** is an *expression*; it must be a single
///     balanced `(...)` spanning the whole value → `Ok(Some(inner))`, else an unbalanced
///     `Err` at parse time (m1 — no deferred eval-time surprise, and no silent
///     "`(1` is verbatim but `(1)` evaluates" inconsistency);
///   - any other bare value is verbatim → `Ok(None)`.
///
/// The parens are the unambiguous "evaluate this" marker (Decision 21b); every existing
/// `p:value=10k` stays verbatim.
pub(crate) fn as_expr_value(raw: &str) -> Result<Option<String>, String> {
    let raw = raw.trim();
    if is_quoted(raw) {
        return Ok(None); // quoted ⇒ always verbatim (M2)
    }
    if !raw.starts_with('(') {
        return Ok(None); // ordinary verbatim value
    }
    // Bare, starts with `(` ⇒ intended as an expression. Require a single balanced group
    // that spans the whole value.
    match balanced_paren_body(raw) {
        Some(inner) => Ok(Some(inner.trim().to_string())),
        None => Err(format!(
            "expression value `{raw}` has unbalanced parentheses (write `(<expr>)`)"
        )),
    }
}

/// Parse an `if=` clause value into its expression text with a **parse-time** balance
/// check (m1). `if=` is *always* an expression, so a quoted value is unquoted (quoting
/// only protects whitespace) and a single surrounding `(...)` is stripped; the result
/// must be parenthesis-balanced, else an `Err` here rather than a deferred eval error.
pub(crate) fn parse_if_clause(raw: &str) -> Result<String, String> {
    let v = if is_quoted(raw) {
        unquote(raw).trim()
    } else {
        raw.trim()
    };
    // Strip one balanced outer pair if the whole thing is `(...)`, so both `if=(n>0)` and
    // `if=n>0` are accepted. Then require the remainder to be balanced.
    let inner = balanced_paren_body(v).unwrap_or_else(|| v.to_string());
    if paren_depth_ok(&inner) {
        Ok(inner.trim().to_string())
    } else {
        Err(format!("`if=` expression `{v}` has unbalanced parentheses"))
    }
}

/// If `s` is exactly one balanced parenthesis group spanning its whole (trimmed) length —
/// `(...)` with the opening `(` matched by the *final* `)` — return the inner text; else
/// `None`. Distinguishes `(a+b)` (a wrapped group) from `(a)+(b)` (two groups, not a
/// single wrapper) and from `(a` (unbalanced).
pub(crate) fn balanced_paren_body(s: &str) -> Option<String> {
    let s = s.trim();
    let inner = s.strip_prefix('(')?.strip_suffix(')')?;
    // Walk the inner text: depth must stay ≥ 0 and never return to 0 before the end
    // (which would mean the opening `(` closed early, so the outer pair is not a single
    // wrapper — e.g. `(a)+(b)`).
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                // Depth going negative means this `)` matched the *outer* `(` before the
                // end — the outer pair is not a single wrapper (e.g. `(a)+(b)`).
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    (depth == 0).then(|| inner.to_string())
}

/// Are the parentheses in `s` balanced (every `(` matched, none closing before opening)?
pub(crate) fn paren_depth_ok(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Is `s` a single expression-tier identifier (an ASCII-ish letters/digits/underscore
/// run that does not start with a digit)? Used to validate a `param` name.
pub(crate) fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// Strip one surrounding pair of double quotes from a value, if present.
pub(crate) fn unquote(v: &str) -> &str {
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v)
}

pub(crate) fn two_tokens(rest: &str, usage: &str) -> Result<(String, String), String> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if toks.len() != 2 {
        return Err(format!("expected: {usage}"));
    }
    Ok((toks[0].to_string(), toks[1].to_string()))
}

pub(crate) fn two_tokens_and_len(rest: &str, usage: &str) -> Result<(String, String, Nm), String> {
    let toks: Vec<&str> = rest.split_whitespace().collect();
    if toks.len() != 3 {
        return Err(format!("expected: {usage}"));
    }
    Ok((
        toks[0].to_string(),
        toks[1].to_string(),
        parse_len(toks[2])?,
    ))
}

pub(crate) fn node_list(rest: &str, kw: &str) -> Result<Vec<String>, String> {
    let nodes: Vec<String> = rest.split_whitespace().map(String::from).collect();
    if nodes.is_empty() {
        return Err(format!("{kw} needs at least one node"));
    }
    Ok(nodes)
}

/// `<path> (<x>, <y>)` — path is everything up to the first `(`.
pub(crate) fn path_and_point(rest: &str) -> Result<(String, Point), String> {
    let open = rest.find('(').ok_or("expected a coordinate `(x, y)`")?;
    let path = rest[..open].trim();
    if path.is_empty() {
        return Err("missing path before coordinate".into());
    }
    let pts = extract_points(&rest[open..])?;
    if pts.len() != 1 {
        return Err("expected exactly one coordinate `(x, y)`".into());
    }
    Ok((path.to_string(), pts[0]))
}

/// Split a `comp.field` reference at the *last* dot, so hierarchical comp paths
/// survive (e.g. `psu.dec[0].p1` -> (`psu.dec[0]`, `p1`)).
pub(crate) fn split_last_dot(s: &str, what: &str) -> Result<(String, String), String> {
    match s.rsplit_once('.') {
        Some((comp, field)) if !comp.is_empty() && !field.is_empty() => {
            Ok((comp.to_string(), field.to_string()))
        }
        _ => Err(format!("`{s}` must be of the form <comp>.<{what}>")),
    }
}

/// Pull out every `(x, y)` group from a string, in order.
/// Parse a skeleton [`Path`] from a coordinate list with optional `arc` markers:
/// `(x,y) [arc (mx,my) (ex,ey)] (x,y) ...`. A bare coordinate is a straight edge to
/// that point; `arc <mid> <end>` is a circular arc through the previous point, `mid`,
/// and `end`. The first token must be a coordinate (the start). The inverse of
/// [`fmt_path`]; a list with no `arc` markers yields an all-`Line` path (backward
/// compatible with the old point-only grammar).
pub(crate) fn extract_path(s: &str) -> Result<Path, String> {
    enum PTok {
        Coord(Point),
        Arc,
        Quad,
        Cubic,
    }
    let mut toks = Vec::new();
    let mut rest = s.trim_start();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('(') {
            let close = after.find(')').ok_or("unbalanced '(' in coordinate")?;
            let (xs, ys) = after[..close]
                .split_once(',')
                .ok_or("coordinate must be `(x, y)`")?;
            toks.push(PTok::Coord(Point {
                x: parse_len(xs.trim())?,
                y: parse_len(ys.trim())?,
            }));
            rest = after[close + 1..].trim_start();
        } else {
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '(')
                .unwrap_or(rest.len());
            match &rest[..end] {
                "arc" => toks.push(PTok::Arc),
                "quad" => toks.push(PTok::Quad),
                "cubic" => toks.push(PTok::Cubic),
                other => {
                    return Err(format!(
                        "unexpected token `{other}` in path (expected a coordinate, `arc`, `quad`, or `cubic`)"
                    ));
                }
            }
            rest = rest[end..].trim_start();
        }
    }
    let mut it = toks.into_iter();
    let start = match it.next() {
        Some(PTok::Coord(p)) => p,
        Some(_) => {
            return Err("a path must begin with a coordinate, not `arc`/`quad`/`cubic`".into());
        }
        None => return Err("expected a coordinate `(x, y)`".into()),
    };
    let mut segs = Vec::new();
    while let Some(t) = it.next() {
        match t {
            PTok::Coord(p) => segs.push(Seg::Line { end: p }),
            PTok::Arc => {
                let mid = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err("`arc` needs a midpoint coordinate: arc (mx,my) (ex,ey)".into());
                    }
                };
                let end = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => return Err("`arc` needs an endpoint coordinate after the midpoint".into()),
                };
                segs.push(Seg::Arc { mid, end });
            }
            PTok::Quad => {
                let ctrl = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err(
                            "`quad` needs a control coordinate: quad (cx,cy) (ex,ey)".into()
                        );
                    }
                };
                let end = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => return Err("`quad` needs an endpoint coordinate after the control".into()),
                };
                segs.push(Seg::Quadratic { ctrl, end });
            }
            PTok::Cubic => {
                let c1 = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err(
                            "`cubic` needs two controls + an endpoint: cubic (c1) (c2) (end)"
                                .into(),
                        );
                    }
                };
                let c2 = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => return Err("`cubic` needs a second control coordinate".into()),
                };
                let end = match it.next() {
                    Some(PTok::Coord(p)) => p,
                    _ => {
                        return Err(
                            "`cubic` needs an endpoint coordinate after the controls".into()
                        );
                    }
                };
                segs.push(Seg::Cubic { c1, c2, end });
            }
        }
    }
    Ok(Path { start, segs })
}

/// Does the path enclose area as a polygon — ≥ 3 corners, or ≥ 1 corner closed by a
/// curved edge (a half-disc is a valid 2-corner arc polygon; a Bézier blob likewise)?
pub(crate) fn path_is_polygon(path: &Path) -> bool {
    let has_curve = path.segs.iter().any(|s| {
        matches!(
            s,
            Seg::Arc { .. } | Seg::Quadratic { .. } | Seg::Cubic { .. }
        )
    });
    let corners = 1 + path.segs.len();
    corners >= 3 || (has_curve && corners >= 2)
}

pub(crate) fn extract_points(s: &str) -> Result<Vec<Point>, String> {
    let mut pts = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let close_rel = rest[open..]
            .find(')')
            .ok_or("unbalanced '(' in coordinate")?;
        let close = open + close_rel;
        let inner = &rest[open + 1..close];
        let (xs, ys) = inner.split_once(',').ok_or("coordinate must be `(x, y)`")?;
        pts.push(Point {
            x: parse_len(xs.trim())?,
            y: parse_len(ys.trim())?,
        });
        rest = &rest[close + 1..];
    }
    // Anything non-whitespace outside the parens is a malformed coordinate.
    if pts.is_empty() {
        return Err("expected a coordinate `(x, y)`".into());
    }
    Ok(pts)
}

/// Parse a length token into nanometres. Accepts `<n>mm` (decimal allowed),
/// `<n>nm`, or a bare integer (interpreted as nm).
pub(crate) fn parse_len(tok: &str) -> Result<Nm, String> {
    let t = tok.trim();
    if t.is_empty() {
        return Err("empty length".into());
    }
    if let Some(body) = t.strip_suffix("mm") {
        parse_mm(body)
    } else if let Some(body) = t.strip_suffix("nm") {
        parse_int_nm(body)
    } else {
        parse_int_nm(t)
    }
}

pub(crate) fn parse_int_nm(body: &str) -> Result<Nm, String> {
    body.trim()
        .parse::<i64>()
        .map_err(|_| format!("`{body}` is not an integer number of nm"))
}

pub(crate) fn parse_mm(body: &str) -> Result<Nm, String> {
    let body = body.trim();
    let (neg, body) = match body.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let (whole_str, frac_str) = match body.split_once('.') {
        Some((w, f)) => (w, f),
        None => (body, ""),
    };
    let whole: i64 = if whole_str.is_empty() {
        0
    } else {
        whole_str
            .parse()
            .map_err(|_| format!("`{body}mm` has a non-numeric whole part"))?
    };
    if frac_str.len() > 6 {
        return Err(format!(
            "`{body}mm` has sub-nanometre precision (max 6 decimal places)"
        ));
    }
    let frac: i64 = if frac_str.is_empty() {
        0
    } else {
        // Pad on the right to 6 digits: ".5" -> 500000 nm, ".000001" -> 1 nm.
        format!("{frac_str:0<6}")
            .parse()
            .map_err(|_| format!("`{body}mm` has a non-numeric fraction"))?
    };
    let val = whole * MM + frac;
    Ok(if neg { -val } else { val })
}
