//! Decision-21a `def` blocks: the block renderer (`render_def`) and parser
//! (`parse_def`, `parse_port`).

use super::*;

/// Render a `def` block (Decision 21a) as canonical block-grammar text (no trailing
/// newline — the caller appends one, matching the flat serialize loop). The header is
/// `def <name>` with each declared param as an inline ` param <k>=<default>`; the body is
/// each directive re-rendered and indented one level, interleaved with preserved
/// comment/blank trivia; `port` bindings emit last, in `BTreeMap` (name) order — a
/// deterministic canonical position independent of where they were authored. Body
/// directives round-trip through the same [`render_directive`] the flat program uses
/// (nested def instantiations are ordinary `inst` lines), so a def body is byte-stable
/// across a parse→serialize→parse fixpoint. When the def carries a Decision-20 `schematic`
/// layout fragment (over its internal paths), it emits last — after the ports — as an
/// indented `schematic { … }` block, reusing the same [`emit_layout_nodes`] the doc-level
/// block uses so the two agree on indentation and trivia.
pub(crate) fn render_def(
    name: &str,
    params: &[(String, String)],
    body: &[DefNode],
    ports: &BTreeMap<String, (String, String)>,
    layout: &Option<crate::schematic::SchematicLayout>,
) -> String {
    let mut s = format!("def {name}");
    for (k, v) in params {
        s.push_str(&format!(" param {k}={v}"));
    }
    s.push_str(" {\n");
    for node in body {
        match node {
            DefNode::Directive(d) => {
                s.push_str(BLOCK_INDENT);
                s.push_str(&render_directive(d));
                s.push('\n');
            }
            DefNode::Comment(text) if text.is_empty() => {
                s.push_str(BLOCK_INDENT);
                s.push_str("#\n");
            }
            DefNode::Comment(text) => {
                s.push_str(BLOCK_INDENT);
                s.push_str(&format!("# {text}\n"));
            }
            DefNode::Blank => s.push('\n'),
        }
    }
    // Ports emit after the body, in canonical (name) order.
    for (pname, (path, sel)) in ports {
        s.push_str(BLOCK_INDENT);
        s.push_str(&format!("port {pname} = {path}.{sel}\n"));
    }
    // The Decision-20 layout fragment (if any) emits last, as an indented `schematic { … }`
    // block inside the def body. One extra indent level over the doc-level `serialize_layout`
    // (the def body is already one level in), reusing `emit_layout_nodes` so trivia and
    // container indentation match the doc-level block exactly.
    if let Some(layout) = layout {
        s.push_str(BLOCK_INDENT);
        s.push_str("schematic {\n");
        let mut indent = String::from(BLOCK_INDENT);
        indent.push_str(BLOCK_INDENT);
        emit_layout_nodes(&layout.roots, &mut indent, &mut s);
        s.push_str(BLOCK_INDENT);
        s.push_str("}\n");
    }
    s.push('}');
    s
}

/// Parse a top-level `def <name> [param <k>=<default> ...] { body }` (Decision 21a) and
/// push the resulting [`GenDirective::Def`](crate::ir::GenDirective::Def) onto
/// `parsed.source`. The header is `def <name>` followed by zero or more
/// `param <k>=<default>` declarations *inline on the header line* — the same
/// declaration-with-default shape a def instantiation later overrides via `p:`. The body
/// is a source fragment (parts, internal nets, `port` bindings, nested def
/// *instantiations*); nested def *definitions* and any non-body directive are rejected.
/// Collect-all: every malformed piece is reported; on any error nothing partial escapes
/// (the caller's `errors` is non-empty, so the whole parse fails).
pub(crate) fn parse_def(b: &Block, parsed: &mut Parsed, errors: &mut Vec<Diagnostic>) {
    // Header: `def <name> [param <k>=<default>]...`. Token 0 is `def`.
    let toks = &b.tokens[1..];
    if toks.is_empty() {
        errors.push(err_line(
            "E_DEF",
            "`def` needs a name: def <name> [param <k>=<default> ...] { ... }".to_string(),
            b.line,
        ));
        return;
    }
    let name = toks[0].clone();
    if !is_ident(&name) {
        errors.push(err_line(
            "E_DEF",
            format!("def name `{name}` must be an identifier (letters, digits, `_`)"),
            b.line,
        ));
    }
    // Inline `param <k>=<default>` declarations. They arrive as pairs of tokens
    // (`param`, `k=default`); a bare `param` with no `k=v`, or a non-`param` token, is an
    // error. Params keep authored order (defaults may reference earlier ones in the def
    // scope, mirroring the doc-level `param` order-independence).
    let mut params: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut i = 1;
    while i < toks.len() {
        if toks[i] != "param" {
            errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}` header: unexpected token `{}` (expected `param <k>=<default>`)",
                    toks[i]
                ),
                b.line,
            ));
            i += 1;
            continue;
        }
        let Some(kv) = toks.get(i + 1) else {
            errors.push(err_line(
                "E_DEF",
                format!("def `{name}` header: `param` needs `<k>=<default>`"),
                b.line,
            ));
            break;
        };
        match kv.split_once('=') {
            Some((k, v)) if is_ident(k) && !v.is_empty() => {
                if !seen.insert(k.to_string()) {
                    errors.push(err_line(
                        "E_DEF",
                        format!("def `{name}`: duplicate param `{k}`"),
                        b.line,
                    ));
                }
                params.push((k.to_string(), v.to_string()));
            }
            _ => errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}` header: malformed param `{kv}` (expected `<ident>=<default>`)"
                ),
                b.line,
            )),
        }
        i += 2;
    }

    // Body: lower each child directive into a `Source` fragment, pulling out `port`
    // bindings. Only body-shaped directives are accepted (inst / net / nc / connect /
    // port); layout, placement, board/stackup, and route directives are out of scope for
    // a def body (Phase 3). A nested `def { … }` is a hard error.
    let mut body: Vec<DefNode> = Vec::new();
    let mut ports: BTreeMap<String, (String, String)> = BTreeMap::new();
    // A def body may carry ONE Decision-20 `schematic { … }` layout fragment over its
    // INTERNAL paths (`R1`, not `sense[0].R1`), stamped per instance at expansion time. The
    // last one wins (mirrors the doc-level `schematic` block).
    let mut layout: Option<crate::schematic::SchematicLayout> = None;
    for node in &b.children {
        let child = match node {
            Node::Block(c) => c,
            // Preserve body trivia so a mixed-authorship def body round-trips (Decision 21).
            Node::Comment(text) => {
                body.push(DefNode::Comment(text.clone()));
                continue;
            }
            Node::Blank => {
                body.push(DefNode::Blank);
                continue;
            }
        };
        // A `schematic { … }` layout fragment is the one block a def body admits (Decision
        // 20 embedded in a def): special-case it BEFORE the generic block rejection so it is
        // not treated like a stray nested block. Its `row`/`column`/`sym`/`wire` children
        // parse through the ordinary layout grammar; the paths are def-internal.
        if child.opened_block && child.keyword == "schematic" {
            if child.tokens.len() > 1 {
                errors.push(err_line(
                    "E_SCHEMATIC",
                    format!("`schematic` takes no arguments (got `{}`)", child.rest),
                    child.line,
                ));
            }
            let roots = parse_layout_nodes(&child.children, errors);
            // Last `schematic` block wins, mirroring the doc-level rule.
            layout = Some(crate::schematic::SchematicLayout { roots });
            continue;
        }
        if child.opened_block {
            // The only other block-opening keyword reachable here is `def` (the allowlist);
            // a nested def definition is rejected. Any other block opener already errored in
            // `parse_forest`'s block-rejection arm, but a `def` inside a `def` reaches here.
            errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}`: nested `{}` block is not allowed (def definitions are top-level)",
                    child.keyword
                ),
                child.line,
            ));
            continue;
        }
        match child.keyword.as_str() {
            "port" => {
                match parse_port(&child.rest) {
                    Ok((pname, binding)) => {
                        if ports.insert(pname.clone(), binding).is_some() {
                            errors.push(err_line(
                                "E_DEF",
                                format!("def `{name}`: duplicate port `{pname}`"),
                                child.line,
                            ));
                        }
                    }
                    Err(e) => errors.push(err_line("E_DEF", format!("def `{name}`: {e}"), child.line)),
                }
            }
            "inst" | "net" | "nc" | "connect" => {
                let line = child.header_line();
                match parse_line(&line) {
                    Ok(Item::Directive(d)) => {
                        check_coord_range(directive_coords(&d), child.line, errors);
                        body.push(DefNode::Directive(d));
                    }
                    Ok(_) => errors.push(err_line(
                        "E_DEF",
                        format!("def `{name}`: `{}` is not a valid body directive", child.keyword),
                        child.line,
                    )),
                    Err(e) => errors.push(Diagnostic::error(
                        "E_PARSE",
                        format!("{e} (in `{line}`)"),
                        Location::Span { line: child.line, col: 1 },
                    )),
                }
            }
            other => errors.push(err_line(
                "E_DEF",
                format!(
                    "def `{name}`: `{other}` is not valid in a def body (expected inst / net / nc / connect / port)"
                ),
                child.line,
            )),
        }
    }

    parsed.source.push(GenDirective::Def {
        name,
        params,
        body,
        ports,
        layout,
    });
}

/// Parse a `port <name> = <internal-path>.<selector>` binding (Decision 21a bare typed
/// ports). Returns `(port-name, (internal-path, selector))`. The `<internal-path>` is a
/// def-relative instance path; the selector is a pin/pad selector resolved against that
/// instance's part at stamp time (same selector grammar as `net`). Named-InterfaceDef
/// ports (`port <name> : <iface-type> ...`) are not implemented (descoped — see report).
pub(crate) fn parse_port(rest: &str) -> Result<(String, (String, String)), String> {
    const USAGE: &str = "port <name> = <internal-path>.<pin-or-selector>";
    // Reject the stretch interface-port form explicitly so it fails loud, not silently.
    if rest.contains(':') && !rest.contains('=') {
        return Err(format!(
            "named-interface ports (`port <name> : <iface>`) are not supported in v1 ({USAGE})"
        ));
    }
    let (name, target) = rest
        .split_once('=')
        .ok_or_else(|| format!("{USAGE} (missing `=`)"))?;
    let name = name.trim();
    let target = target.trim();
    if name.is_empty() || target.is_empty() {
        return Err(format!("{USAGE} (empty name or target)"));
    }
    if !is_ident(name) {
        return Err(format!("port name `{name}` must be an identifier"));
    }
    let (path, sel) = split_last_dot(target, "port target")?;
    Ok((name.to_string(), (path, sel)))
}
