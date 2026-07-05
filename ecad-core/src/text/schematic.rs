//! Decision-20 schematic-layout domain: parse (`parse_layout_nodes` + the container/sym/
//! wire header parsers) and serialize (`serialize_layout`, `emit_layout_nodes`, and the
//! line/header renderers), plus the shared `err_line`/`check_coord_range` helpers.

use super::*;
use crate::schematic::{Align, Container, Direction, LayoutNode, Symbol};

/// A span-located diagnostic at column 1 — the layout parsers' one shape.
pub(crate) fn err_line(code: &'static str, msg: String, line: u32) -> Diagnostic {
    Diagnostic::error(code, msg, Location::Span { line, col: 1 })
}

/// Lower a `schematic`/`row`/`column` block body (a [`Node`] sequence) into layout
/// nodes. Trivia (comments/blanks) is **preserved** as [`LayoutNode::Comment`]/`Blank`
/// so mixed authorship inside a `schematic` block round-trips (the Decision-20/21
/// requirement); the semantic walks ([`reflow`](crate::schematic::reflow), validation)
/// skip it. Only `row`/`column` (nested containers) and `sym` (leaves) are valid
/// directive children; anything else is an `E_SCHEMATIC` error. Collect-all: every
/// malformed child is reported.
pub(crate) fn parse_layout_nodes(nodes: &[Node], errors: &mut Vec<Diagnostic>) -> Vec<LayoutNode> {
    let mut out = Vec::new();
    for node in nodes {
        let b = match node {
            Node::Block(b) => b,
            Node::Comment(text) => {
                out.push(LayoutNode::Comment(text.clone()));
                continue;
            }
            Node::Blank => {
                out.push(LayoutNode::Blank);
                continue;
            }
        };
        match b.keyword.as_str() {
            "row" | "column" => {
                if !b.opened_block {
                    errors.push(err_line(
                        "E_SCHEMATIC",
                        format!("`{}` must open a `{{ … }}` block", b.keyword),
                        b.line,
                    ));
                    continue;
                }
                let dir = if b.keyword == "row" {
                    Direction::Row
                } else {
                    Direction::Column
                };
                match parse_container_header(dir, &b.tokens[1..], b.line) {
                    Ok((name, gap, align)) => {
                        // Authored length ingress: `gap` is bounded like every other
                        // coordinate/length (issue 0018) — an over-bound value is
                        // E_COORD_RANGE here, not an add-overflow panic later in reflow.
                        check_coord_range(vec![gap], b.line, errors);
                        let children = parse_layout_nodes(&b.children, errors);
                        out.push(LayoutNode::Container(Container {
                            dir,
                            name,
                            gap,
                            align,
                            children,
                        }));
                    }
                    Err(e) => errors.push(err_line("E_SCHEMATIC", e, b.line)),
                }
            }
            "sym" => {
                if b.opened_block {
                    errors.push(err_line(
                        "E_SCHEMATIC",
                        "`sym` is a leaf and takes no block".to_string(),
                        b.line,
                    ));
                    // Fall through to also report any header error, but a block sym is
                    // already rejected; skip parsing its (nonexistent leaf) header.
                    continue;
                }
                match parse_sym_header(&b.tokens[1..], b.line) {
                    Ok(sym) => {
                        // Bound the pinned offsets, same discipline as `gap` / every other
                        // authored length (issue 0018).
                        check_coord_range(vec![sym.dx, sym.dy], b.line, errors);
                        out.push(LayoutNode::Symbol(sym));
                    }
                    Err(e) => errors.push(err_line("E_SCHEMATIC", e, b.line)),
                }
            }
            "wire" => {
                if b.opened_block {
                    errors.push(err_line(
                        "E_SCHEMATIC",
                        "`wire` is a leaf and takes no block".to_string(),
                        b.line,
                    ));
                    continue;
                }
                match parse_wire_header(&b.rest) {
                    Ok(wire) => {
                        // Range-check the presentational waypoints, same discipline as
                        // `gap` / `dx` / every other authored length (issue 0018) — an
                        // over-bound value is E_COORD_RANGE here, not a later panic.
                        let coords: Vec<Nm> =
                            wire.waypoints.iter().flat_map(|p| [p.x, p.y]).collect();
                        check_coord_range(coords, b.line, errors);
                        out.push(LayoutNode::Wire(wire));
                    }
                    Err(e) => errors.push(err_line("E_SCHEMATIC", e, b.line)),
                }
            }
            other => errors.push(err_line(
                "E_SCHEMATIC",
                format!("`{other}` is not valid inside a layout container (expected `row`, `column`, `sym`, or `wire`)"),
                b.line,
            )),
        }
    }
    out
}

/// Parse a container header tail (`[name] [gap=<len>] [align=start|center|end]`). The
/// optional name is a single leading bare token (no `=`); the rest are `key=value`
/// attributes in any order. An unknown attribute or a repeated one is an error. A
/// **quoted** leading token is always the name (its content is opaque — so a name may
/// contain `=`, `#`, or spaces and still round-trip); a bare token with an `=` is an
/// attribute.
pub(crate) fn parse_container_header(
    _dir: Direction,
    toks: &[String],
    _line: u32,
) -> Result<(Option<String>, Nm, Align), String> {
    let mut name: Option<String> = None;
    let mut gap: Option<Nm> = None;
    let mut align: Option<Align> = None;
    for (i, tok) in toks.iter().enumerate() {
        // A quoted leading token is the name verbatim (opaque content, may hold `=`).
        if i == 0 && name.is_none() && tok.starts_with('"') {
            name = Some(unquote(tok).to_string());
        } else if let Some((k, v)) = tok.split_once('=') {
            match k {
                "gap" => {
                    if gap.is_some() {
                        return Err("duplicate `gap`".into());
                    }
                    gap = Some(parse_len(v)?);
                }
                "align" => {
                    if align.is_some() {
                        return Err("duplicate `align`".into());
                    }
                    align = Some(parse_align(v)?);
                }
                _ => return Err(format!("unknown container attribute `{k}`")),
            }
        } else if i == 0 && name.is_none() {
            // The one bare token is the optional container name (must lead).
            name = Some(unquote(tok).to_string());
        } else {
            return Err(format!(
                "unexpected token `{tok}` (a container name must come first)"
            ));
        }
    }
    Ok((name, gap.unwrap_or(0), align.unwrap_or_default()))
}

pub(crate) fn parse_align(v: &str) -> Result<Align, String> {
    Ok(match v {
        "start" => Align::Start,
        "center" => Align::Center,
        "end" => Align::End,
        _ => return Err(format!("unknown align `{v}` (start | center | end)")),
    })
}

/// Parse a `sym` leaf header (`<comp-path> [rot=0|90|180|270] [dx=<len> dy=<len>]`). The
/// comp path is the leading token; the rest are `key=value` attributes. A **quoted** path
/// token is opaque (so a comp path containing `=`/`#`/spaces — all legal in an `inst`
/// path — round-trips); an unquoted token with an `=` is an attribute.
pub(crate) fn parse_sym_header(toks: &[String], _line: u32) -> Result<Symbol, String> {
    let mut path: Option<String> = None;
    let mut rot = Orient::IDENTITY;
    let (mut dx, mut dy) = (0i64, 0i64);
    let (mut saw_rot, mut saw_dx, mut saw_dy) = (false, false, false);
    for tok in toks {
        if path.is_none() && tok.starts_with('"') {
            // A quoted leading token is the comp path verbatim.
            path = Some(unquote(tok).to_string());
        } else if let Some((k, v)) = tok.split_once('=') {
            match k {
                "rot" => {
                    if saw_rot {
                        return Err("duplicate `rot`".into());
                    }
                    rot = parse_sym_rot(v)?;
                    saw_rot = true;
                }
                "dx" => {
                    if saw_dx {
                        return Err("duplicate `dx`".into());
                    }
                    dx = parse_len(v)?;
                    saw_dx = true;
                }
                "dy" => {
                    if saw_dy {
                        return Err("duplicate `dy`".into());
                    }
                    dy = parse_len(v)?;
                    saw_dy = true;
                }
                _ => return Err(format!("unknown `sym` attribute `{k}`")),
            }
        } else if path.is_none() {
            path = Some(tok.clone());
        } else {
            return Err(format!("unexpected token `{tok}` after the component path"));
        }
    }
    let path = path.ok_or("`sym` needs a component path")?;
    Ok(Symbol { path, rot, dx, dy })
}

/// Parse a `wire` leaf tail (`<aComp>.<aPin> <bComp>.<bPin> [via (x,y) (x,y) …]`). The two
/// endpoints are the leading tokens (each `comp.pin`, split at the *last* dot so a
/// hierarchical comp path with dots survives — the `nearpin` idiom); an optional `via`
/// keyword introduces the presentational waypoint list, a run of `(x,y)` coordinates.
/// Endpoints round-trip: a quoted endpoint token is opaque (so a comp path holding
/// `#`/`=`/spaces survives), matching the `sym` path convention.
///
/// **v1 limitation — an interface *signal* cannot be named directly.** The last-dot split
/// makes `mcu.uart.tx` parse as comp `mcu.uart` + pin `tx`, which then fails validation
/// (no such component) — a loud `E_SCHEMATIC`, never silent. This is deliberate: the wire
/// vocabulary is `comp.pin`, and a `port.signal` has *two* dots. The workaround is exact
/// and always available: **wire to the underlying pad number.** Post-0010, an interface
/// signal bound to a pad *is* that pad (the netlist keys both under the pad-number
/// `PinRef`), so `wire mcu.12 …` addresses the same electrical node the tag renderer draws
/// for `uart.tx`. An abstract (unbound, toy-library) signal has no pad and simply cannot be
/// wired in v1 — its tag still renders (§20c), so the connection is not lost, only the
/// drawn line. Naming `port.signal` endpoints directly is a follow-up if it proves needed.
pub(crate) fn parse_wire_header(rest: &str) -> Result<crate::schematic::Wire, String> {
    use crate::schematic::{Wire, WireEnd};
    const USAGE: &str = "wire <aComp>.<aPin> <bComp>.<bPin> [via (x,y) (x,y) …]";

    // Split the header at the `via` keyword: the part before it holds the two endpoint
    // tokens; the part after is the coordinate list. `via` is matched as a whole
    // whitespace-delimited token so a comp path can't accidentally contain it.
    let toks: Vec<&str> = rest.split_whitespace().collect();
    let via_at = toks.iter().position(|t| *t == "via");
    let (ends, waypoints) = match via_at {
        Some(i) => {
            // Rejoin the tokens after the `via` keyword; `extract_points` scans for the
            // `(x, y)` groups, so a space reintroduced inside a coordinate is harmless.
            let coord_str = toks[i + 1..].join(" ");
            if coord_str.is_empty() {
                return Err(format!(
                    "`via` needs at least one waypoint `(x, y)`: {USAGE}"
                ));
            }
            (&toks[..i], extract_points(&coord_str)?)
        }
        None => (&toks[..], Vec::new()),
    };
    if ends.len() != 2 {
        return Err(format!("expected two endpoints: {USAGE}"));
    }
    let parse_end = |tok: &str| -> Result<WireEnd, String> {
        let (comp, pin) = split_last_dot(unquote(tok), "pin")?;
        Ok(WireEnd { comp, pin })
    };
    Ok(Wire {
        a: parse_end(ends[0])?,
        b: parse_end(ends[1])?,
        waypoints,
    })
}

/// Parse a `sym` `rot=` value: only the four cardinals are legal in v1 (§20b — authored
/// orientation, no arbitrary angles on the layout leaf). Yields the tiny exact cardinal
/// quaternion.
pub(crate) fn parse_sym_rot(v: &str) -> Result<Orient, String> {
    let d: i32 = v
        .parse()
        .map_err(|_| format!("`rot={v}` must be one of 0, 90, 180, 270"))?;
    match d.rem_euclid(360) {
        0 | 90 | 180 | 270 => Ok(Orient::from_deg(d).unwrap()),
        _ => Err(format!("`rot={v}` must be one of 0, 90, 180, 270")),
    }
}

/// Render just the doc-level `schematic { … }` block for a [`SchematicLayout`] as canonical
/// text — the same bytes [`serialize`] emits for `doc.schematic`, exposed so a caller
/// authoring a layout tree programmatically can append it to a serialized source and feed
/// the whole thing through [`parse`]/`LoadText` (the only ingest path for a schematic tree —
/// there is no `SetSchematic` command). A thin `pub` wrapper over [`serialize_layout`]; the
/// output round-trips byte-identically.
pub fn serialize_schematic_block(layout: &crate::schematic::SchematicLayout) -> String {
    serialize_layout(layout)
}

/// Render a [`SchematicLayout`](crate::schematic::SchematicLayout) as canonical block
/// text: a `schematic { … }` wrapper around the emitted node forest, indented one level.
/// Deterministic and round-tripping: [`parse`] of the output reproduces the tree,
/// including trivia. Emitted only by [`serialize`], and only when a layout is present.
pub(crate) fn serialize_layout(layout: &crate::schematic::SchematicLayout) -> String {
    let mut out = String::from("schematic {\n");
    let mut indent = String::from(BLOCK_INDENT);
    emit_layout_nodes(&layout.roots, &mut indent, &mut out);
    out.push_str("}\n");
    out
}

/// Emit a layout node forest at the current `indent`. Containers open a `{ … }` block and
/// recurse; symbols and trivia are single lines. Mirrors [`emit_nodes`]'s trivia style so
/// the two block emitters agree.
pub(crate) fn emit_layout_nodes(nodes: &[LayoutNode], indent: &mut String, out: &mut String) {
    for n in nodes {
        match n {
            LayoutNode::Container(c) => {
                out.push_str(indent);
                out.push_str(&container_header(c));
                out.push_str(" {\n");
                indent.push_str(BLOCK_INDENT);
                emit_layout_nodes(&c.children, indent, out);
                indent.truncate(indent.len() - BLOCK_INDENT.len());
                out.push_str(indent);
                out.push_str("}\n");
            }
            LayoutNode::Symbol(s) => {
                out.push_str(indent);
                out.push_str(&sym_line(s));
                out.push('\n');
            }
            LayoutNode::Wire(w) => {
                out.push_str(indent);
                out.push_str(&wire_line(w));
                out.push('\n');
            }
            LayoutNode::Comment(text) => {
                out.push_str(indent);
                if text.is_empty() {
                    out.push_str("#\n");
                } else {
                    out.push_str("# ");
                    out.push_str(text);
                    out.push('\n');
                }
            }
            LayoutNode::Blank => out.push('\n'),
        }
    }
}

/// Quote a bare layout token (container name / comp path) when a structural character
/// would otherwise break re-parsing: whitespace, `#` (the comment stripper), `"`, or `=`
/// (the attribute separator). A comp path or name may legally contain `=` (an `inst`
/// path is unrestricted), so this quoting is what keeps such a token round-tripping. The
/// parsers ([`parse_container_header`]/[`parse_sym_header`]) treat a leading quoted token
/// as the opaque name/path.
pub(crate) fn quote_token(v: &str) -> String {
    let needs = v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || c == '#' || c == '"' || c == '=');
    if needs {
        format!("\"{v}\"")
    } else {
        v.to_string()
    }
}

/// The header line of a container (without the trailing ` {`): `row`/`column`, then an
/// optional name, `gap=` (omitted when zero), and `align=` (omitted when `Start`, the
/// default) — the minimal canonical form.
pub(crate) fn container_header(c: &crate::schematic::Container) -> String {
    use crate::schematic::{Align, Direction};
    let mut s = String::from(match c.dir {
        Direction::Row => "row",
        Direction::Column => "column",
    });
    if let Some(name) = &c.name {
        s.push(' ');
        s.push_str(&quote_token(name));
    }
    if c.gap != 0 {
        s.push_str(&format!(" gap={}", fmt_len(c.gap)));
    }
    match c.align {
        Align::Start => {}
        Align::Center => s.push_str(" align=center"),
        Align::End => s.push_str(" align=end"),
    }
    s
}

/// A `sym` leaf line: `sym <path>`, then `rot=` (omitted for identity) and `dx=`/`dy=`
/// (each omitted when zero) — the minimal canonical form.
pub(crate) fn sym_line(s: &crate::schematic::Symbol) -> String {
    let mut out = format!("sym {}", quote_token(&s.path));
    if s.rot != Orient::IDENTITY {
        // v1 rot is cardinal-only (the parser enforces it), so `to_deg` is exact.
        out.push_str(&format!(" rot={}", s.rot.to_deg()));
    }
    if s.dx != 0 {
        out.push_str(&format!(" dx={}", fmt_len(s.dx)));
    }
    if s.dy != 0 {
        out.push_str(&format!(" dy={}", fmt_len(s.dy)));
    }
    out
}

/// A `wire` leaf line: `wire <a> <b>`, then `via` and the waypoint coordinate list when
/// present (omitted for a straight pin-to-pin wire) — the minimal canonical form. Each
/// endpoint is `comp.pin`, quoted as a whole when a structural character would break
/// re-parsing (matching [`quote_token`]'s rule for the `sym` path).
pub(crate) fn wire_line(w: &crate::schematic::Wire) -> String {
    let end = |e: &crate::schematic::WireEnd| quote_token(&format!("{}.{}", e.comp, e.pin));
    let mut out = format!("wire {} {}", end(&w.a), end(&w.b));
    if !w.waypoints.is_empty() {
        out.push_str(" via");
        for p in &w.waypoints {
            out.push(' ');
            out.push_str(&fmt_point(*p));
        }
    }
    out
}

/// Push an `E_COORD_RANGE` error for each coordinate/length exceeding
/// [`crate::geom::MAX_COORD`] (issue 0018), located at `lineno`. A single line can
/// carry several out-of-range values; each is reported (collect-all), and the parse
/// aborts atomically like any other hard fault.
pub(crate) fn check_coord_range(coords: Vec<Nm>, lineno: u32, errors: &mut Vec<Diagnostic>) {
    for n in coords {
        if !coord_ok(n) {
            errors.push(Diagnostic::error(
                "E_COORD_RANGE",
                format!(
                    "coordinate {n} nm exceeds the ±{} nm (±1 m) range",
                    crate::geom::MAX_COORD
                ),
                Location::Span {
                    line: lineno,
                    col: 1,
                },
            ));
        }
    }
}
