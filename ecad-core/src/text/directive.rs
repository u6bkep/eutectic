//! The per-directive line grammar: the flat-line renderer (`render_directive` and its
//! `route`/`via`/prov helpers) and the parser (`enum Item`, `parse_line`).

use super::*;

/// Parse a leading route/via id token (Decision 22): a bare non-negative integer. A
/// non-integer in the id position is a hard `E_PARSE` (the line has the wrong shape — it
/// is not a lenience case, which only covers a *missing* or *duplicate* id).
fn parse_route_id(tok: &str, kind: &str) -> Result<u64, String> {
    tok.parse::<u64>()
        .map_err(|_| format!("{kind} id must be a non-negative integer, got `{tok}`"))
}

/// Serialize the provenance keyword of a persisted route: `pinned` is the default and
/// prints nothing (hand/frozen routing is the common case). `free` marks router-owned
/// copper (the rip-up-able tier). `hint`/`fixed` complete the ladder
/// ([`Provenance`]) so any provenance a route may carry round-trips losslessly rather
/// than silently collapsing to Pinned on save.
pub(crate) fn prov_keyword(p: Provenance) -> &'static str {
    match p {
        Provenance::Pinned => "",
        Provenance::Free => " free",
        Provenance::Hint => " hint",
        Provenance::Fixed => " fixed",
    }
}

/// `route <id> <net> <slab> w=<width> (x,y) (x,y) ... [free|hint|fixed]`. The leading
/// token is the trace's persistent id (Decision 22 — a bare integer, this route's
/// identity across a serialize/parse boundary); the layer is a copper slab name
/// (Decision 13); the width and points are canonical lengths; provenance is a trailing
/// keyword (`pinned` is the default and omitted).
pub(crate) fn render_trace(id: TraceId, t: &Trace) -> String {
    let mut s = format!(
        "route {} {} {} w={}",
        id.0,
        t.net,
        t.layer,
        fmt_len(t.width)
    );
    for p in &t.path {
        s.push(' ');
        s.push_str(&fmt_point(*p));
    }
    s.push_str(prov_keyword(t.prov));
    s
}

/// `via <id> <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]`. The leading
/// token is the via's persistent id (Decision 22). A `None` span is the full copper extent
/// (the common through-via) and prints no span token; an explicit blind/buried span prints
/// `<from>..<to>` (Decision 18).
pub(crate) fn render_via(id: ViaId, v: &Via) -> String {
    let mut s = format!(
        "via {} {} {} drill={} pad={}",
        id.0,
        v.net,
        fmt_point(v.at),
        fmt_len(v.drill),
        fmt_len(v.pad),
    );
    if let Some((from, to)) = &v.span {
        s.push_str(&format!(" {from}..{to}"));
    }
    s.push_str(prov_keyword(v.prov));
    s
}

pub(crate) fn render_directive(d: &GenDirective) -> String {
    match d {
        GenDirective::Instance {
            path,
            part,
            params,
            label,
        } => {
            // `inst <path> <part> [label=<val>] [p:<key>=<val> ...]`. Values are quoted
            // only when they must be (whitespace / `#` / empty), so the common case stays
            // `p:value=10k`. Params render in `BTreeMap` (key) order for a canonical form.
            let mut s = format!("inst {path} {part}");
            if let Some(l) = label {
                s.push_str(&format!(" label={}", quote_value(l)));
            }
            for (k, v) in params {
                s.push_str(&format!(" p:{k}={}", quote_param_value(v)));
            }
            s
        }
        GenDirective::Param { name, expr } => {
            // `param <name> = <expr>` (Decision 21b). The expression text is emitted
            // verbatim (it *is* the generative program — serializes as authored, never
            // pre-evaluated); the `=` is spaced for readability and re-parsed tolerantly.
            format!("param {name} = {expr}")
        }
        GenDirective::InstGenerative {
            path,
            part,
            params,
            param_exprs,
            label,
            range,
            if_expr,
        } => {
            // The generative `inst`, serialized AS AUTHORED (Decision 21): a range suffix
            // `[<lo>..<hi>]` on the path, an `if=<expr>` clause, verbatim `p:k=v` and
            // expression `p:k=(<expr>)` params. The evaluated/expanded instances are
            // elaboration-only and never serialized. Params render in `BTreeMap` (key)
            // order for a canonical form; a verbatim and an expression key never collide
            // (the parser routes each token to exactly one map).
            let ranged = match range {
                Some((lo, hi)) => format!("{path}[{lo}..{hi}]"),
                None => path.clone(),
            };
            let mut s = format!("inst {ranged} {part}");
            if let Some(l) = label {
                s.push_str(&format!(" label={}", quote_value(l)));
            }
            if let Some(cond) = if_expr {
                // Canonical paren form `if=(<expr>)` (m2): the parens both delimit an
                // expression that may contain spaces and re-parse back to the same
                // `if_expr` (the parser strips one outer pair). No re-quoting.
                s.push_str(&format!(" if=({cond})"));
            }
            for (k, v) in params {
                s.push_str(&format!(" p:{k}={}", quote_param_value(v)));
            }
            // Expression params wear parentheses — the unambiguous "evaluate me" marker.
            for (k, e) in param_exprs {
                s.push_str(&format!(" p:{k}=({e})"));
            }
            s
        }
        GenDirective::Place { path, pos } => format!("place {path} {}", fmt_point(*pos)),
        GenDirective::Fix { path, pos } => format!("fix {path} {}", fmt_point(*pos)),
        GenDirective::Board { outline } => {
            // Serialized as a path (`board <p> [arc <mid> <end>] <p> ...`); an arc edge
            // emits `arc <mid> <end>`. The rect shorthand `boardrect <min> <max>` is
            // parse-only sugar. Corner radius (Minkowski-inflated outlines) is not yet
            // serialized — a noted follow-up.
            format!("board {}", fmt_path(outline.path()))
        }
        GenDirective::Cutout { shape } => {
            format!("cutout {}", fmt_path(shape.path()))
        }
        GenDirective::Hole { center, dia } => {
            // `hole <center> dia=<len>` — an authored NPTH through-hole (Decision 16b).
            format!("hole {} dia={}", fmt_point(*center), fmt_len(*dia))
        }
        GenDirective::Region(r) => {
            // `region <role> [net=<n>] layer=<slab> <p> [arc <mid> <end>] <p> ...`.
            // `layer` is a slab name (Decision 13), emitted verbatim. Corner radius is
            // not serialized (same noted follow-up as board/cutout).
            let mut s = format!("region {}", role_token(&r.role));
            if let Some(n) = &r.net {
                s.push_str(&format!(" net={n}"));
            }
            s.push_str(&format!(" layer={}", r.layer));
            s.push(' ');
            s.push_str(&fmt_path(r.shape.path()));
            s
        }
        GenDirective::Slab(s) => {
            // `slab <name> <z_lo> <z_hi> <role> [material]`. Role uses the same total
            // `role_token` as `region` (so Substrate etc. serialise fine); material is an
            // optional bare name.
            let mut out = format!(
                "slab {} {} {} {}",
                s.name,
                fmt_len(s.z.lo),
                fmt_len(s.z.hi),
                role_token(&s.role)
            );
            if let Some(m) = &s.material {
                out.push(' ');
                out.push_str(&m.name);
            }
            out
        }
        GenDirective::Class { name, entry } => {
            // `class <name> [prefix=<val>] [template=<val>] [p:<key>=<val> ...]`. Defaults
            // reuse the `p:` param namespace from `inst`; keys render in `BTreeMap` order.
            let mut s = format!("class {name}");
            if let Some(p) = &entry.prefix {
                s.push_str(&format!(" prefix={}", quote_value(p)));
            }
            if let Some(t) = &entry.template {
                s.push_str(&format!(" template={}", quote_value(t)));
            }
            for (k, v) in &entry.defaults {
                s.push_str(&format!(" p:{k}={}", quote_value(v)));
            }
            s
        }
        GenDirective::Near { a, b, within } => format!("near {a} {b} {}", fmt_len(*within)),
        GenDirective::MinSep { a, b, gap } => format!("minsep {a} {b} {}", fmt_len(*gap)),
        GenDirective::AlignX { nodes } => format!("alignx {}", nodes.join(" ")),
        GenDirective::AlignY { nodes } => format!("aligny {}", nodes.join(" ")),
        GenDirective::ConnectInterface { a, b } => {
            format!("connect {}.{} {}.{}", a.0, a.1, b.0, b.1)
        }
        GenDirective::ConnectPins { net, pins } => {
            let mut s = format!("net {net}");
            for (comp, pin) in pins {
                s.push_str(&format!(" {comp}.{pin}"));
            }
            s
        }
        GenDirective::NoConnect { pins } => {
            let mut s = String::from("nc");
            for (comp, pin) in pins {
                s.push_str(&format!(" {comp}.{pin}"));
            }
            s
        }
        GenDirective::Rotate { path, orient } => {
            // Readable `<deg> [bottom]` for the 8 board-plane orientations; the exact
            // `quat=(w,x,y,z)` for any other (arbitrary-angle) rotation.
            let cardinal = [
                (0, false),
                (90, false),
                (180, false),
                (270, false),
                (0, true),
                (90, true),
                (180, true),
                (270, true),
            ]
            .into_iter()
            .find(|&(d, b)| {
                let c = Orient::from_deg(d).unwrap();
                let c = if b { c.flipped() } else { c };
                orient.same_rotation(c)
            });
            match cardinal {
                Some((d, b)) => format!("rotate {path} {d}{}", if b { " bottom" } else { "" }),
                None => format!(
                    "rotate {path} quat=({},{},{},{})",
                    orient.w, orient.x, orient.y, orient.z
                ),
            }
        }
        GenDirective::NearPin {
            a,
            b_comp,
            b_pin,
            within,
        } => {
            format!("nearpin {a} {b_comp}.{b_pin} {}", fmt_len(*within))
        }
        GenDirective::Text {
            string,
            at,
            height,
            layer,
            orient,
        } => {
            // `text "<string>" (x,y) h=<len> layer=<slab> [rot=<deg> | rotq=(w,x,y,z)]`.
            // `layer` is a slab name (Decision 13), emitted verbatim. The string is
            // double-quoted (may contain spaces). Identity orientation is omitted; a
            // cardinal about-z rotation serialises readably as `rot=<deg>`, any other
            // rotation as the exact `rotq=(w,x,y,z)` (so it round-trips). The
            // double-quote/backslash escaping of arbitrary strings is a noted follow-up.
            let mut s = format!(
                "text \"{string}\" {} h={} layer={}",
                fmt_point(*at),
                fmt_len(*height),
                layer,
            );
            if *orient != Orient::IDENTITY {
                let cardinal = [0, 90, 180, 270]
                    .into_iter()
                    .find(|&d| orient.same_rotation(Orient::from_deg(d).unwrap()));
                match cardinal {
                    Some(d) => s.push_str(&format!(" rot={d}")),
                    None => s.push_str(&format!(
                        " rotq=({},{},{},{})",
                        orient.w, orient.x, orient.y, orient.z
                    )),
                }
            }
            s
        }
        GenDirective::Font { path } => {
            // `font "<path>"` — the doc-wide outline font (Decision 17). Double-quoted so
            // the path may contain spaces; the built-in stroke font is the default when
            // absent.
            format!("font \"{path}\"")
        }
        GenDirective::Use { name } => {
            // `use <name>` — a library-package declaration (a bare name, never a path;
            // the caller resolves names to directories — see `crate::library`).
            format!("use {name}")
        }
        GenDirective::Def {
            name,
            params,
            body,
            ports,
            layout,
        } => render_def(name, params, body, ports, layout),
    }
}

pub(crate) enum Item {
    Directive(GenDirective),
    Override(EntityId, Override),
    RefdesPin(EntityId, String),
    /// A parsed `route` line and its explicit id (`None` when the line omits one — a
    /// hand edit; the caller mints one with a `W_ROUTE_ID` warning). Decision 22.
    Route(Option<u64>, Trace),
    /// A parsed `via` line and its explicit id (see [`Item::Route`]).
    Via(Option<u64>, Via),
}

pub(crate) fn parse_line(line: &str) -> Result<Item, String> {
    let (kw, rest) = match line.split_once(char::is_whitespace) {
        Some((k, r)) => (k, r.trim()),
        None => (line, ""),
    };
    Ok(match kw {
        "inst" => {
            // `inst <path>[<lo>..<hi>] <part> [label=<val>] [if=<expr>] [p:<key>=<val>|(<expr>) ...]`.
            // `path`/`part` are the two leading bare tokens; `label=` carries the display
            // template; `if=<expr>` a population conditional; `p:<key>=<val>` an identity
            // param (verbatim), and `p:<key>=(<expr>)` an *expression* param (the parens
            // are the "evaluate me" marker — Decision 21b). A `[<lo>..<hi>]` suffix on the
            // path is a range (hi exclusive). Whenever a range, an `if=`, or any `(expr)`
            // param is present the directive is a generative [`GenDirective::InstGenerative`];
            // a plain `inst <path> <part>` with only verbatim params stays exactly
            // [`GenDirective::Instance`] as before, so existing docs are untouched. Tokens
            // are split parens-aware so `if=(n > 0)` / `p:c=(i + 1)` may contain spaces.
            const USAGE: &str = "inst <path>[<lo>..<hi>] <part> [label=<val>] [if=<expr>] [p:<key>=<val>|(<expr>) ...]";
            let toks = split_ws_quoted_parens(rest);
            if toks.len() < 2 {
                return Err(USAGE.into());
            }
            // A `[<lo>..<hi>]` suffix on the path is a range (the `..` disambiguates it
            // from an ordinary indexed path like `dec[0]`, which has no `..`).
            let (base_path, range) = split_range_suffix(&toks[0]);
            let part = toks[1].clone();
            let mut params = BTreeMap::new();
            let mut param_exprs = BTreeMap::new();
            let mut label = None;
            let mut if_expr = None;
            for tok in &toks[2..] {
                if let Some(v) = tok.strip_prefix("label=") {
                    label = Some(unquote(v).to_string());
                } else if let Some(v) = tok.strip_prefix("if=") {
                    if_expr = Some(parse_if_clause(v)?);
                } else if let Some(kv) = tok.strip_prefix("p:") {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| format!("inst param needs p:<key>=<value>: `{tok}`"))?;
                    // A bare `(...)` value is an *expression* param; a quoted value is
                    // always verbatim (M2); any other bare value is verbatim too. An
                    // unbalanced bare `(...)` is a parse-time error (m1).
                    match as_expr_value(v)? {
                        Some(inner) => {
                            param_exprs.insert(k.to_string(), inner);
                        }
                        None => {
                            params.insert(k.to_string(), unquote(v).to_string());
                        }
                    }
                } else {
                    return Err(format!("inst: unexpected token `{tok}` ({USAGE})"));
                }
            }
            // Stay on the plain declarative `Instance` unless a generative feature is used
            // — the round-trip and reconciliation of every existing document is identical.
            if range.is_none() && if_expr.is_none() && param_exprs.is_empty() {
                Item::Directive(GenDirective::Instance {
                    path: base_path,
                    part,
                    params,
                    label,
                })
            } else {
                Item::Directive(GenDirective::InstGenerative {
                    path: base_path,
                    part,
                    params,
                    param_exprs,
                    label,
                    range,
                    if_expr,
                })
            }
        }
        "param" => {
            // `param <name> = <expr>` (Decision 21b). The `=` splits name from the
            // expression; the expression is stored verbatim (parsed/evaluated only at
            // elaboration, so it serializes as authored). The name is a single bare token.
            const USAGE: &str = "param <name> = <expr>";
            let (name, expr) = rest
                .split_once('=')
                .ok_or_else(|| format!("{USAGE} (missing `=`)"))?;
            let name = name.trim();
            let expr = expr.trim();
            if name.is_empty() {
                return Err(format!("{USAGE} (empty name)"));
            }
            if name.split_whitespace().count() != 1 || !is_ident(name) {
                return Err(format!(
                    "param name `{name}` must be a single identifier (letters, digits, `_`)"
                ));
            }
            if expr.is_empty() {
                return Err(format!("{USAGE} (empty expression)"));
            }
            Item::Directive(GenDirective::Param {
                name: name.to_string(),
                expr: expr.to_string(),
            })
        }
        "place" => {
            let (path, pos) = path_and_point(rest)?;
            Item::Directive(GenDirective::Place { path, pos })
        }
        "fix" => {
            let (path, pos) = path_and_point(rest)?;
            Item::Directive(GenDirective::Fix { path, pos })
        }
        "board" => {
            let path = extract_path(rest)?;
            if !path_is_polygon(&path) {
                return Err(
                    "board needs ≥3 outline points (or an arc edge): board (x,y) (x,y) (x,y) ..."
                        .into(),
                );
            }
            Item::Directive(GenDirective::Board {
                outline: Shape2D::polygon_path(path, 0),
            })
        }
        "boardrect" => {
            let pts = extract_points(rest)?;
            if pts.len() != 2 {
                return Err(
                    "boardrect needs two corners: boardrect (minx,miny) (maxx,maxy)".into(),
                );
            }
            Item::Directive(board_rect(pts[0], pts[1]))
        }
        "cutout" => {
            let path = extract_path(rest)?;
            if !path_is_polygon(&path) {
                return Err(
                    "cutout needs ≥3 points (or an arc edge): cutout (x,y) (x,y) (x,y) ...".into(),
                );
            }
            Item::Directive(GenDirective::Cutout {
                shape: Shape2D::polygon_path(path, 0),
            })
        }
        "hole" => {
            // `hole (x,y) dia=<len>` — an authored NPTH through-hole (Decision 16b). The
            // center is the one point; `dia` is a required length (mm/nm), written after
            // the point (as it serializes). Lowers to a full-stackup, non-plated
            // `Role::Void` → `board-NPTH.drl`. The `dia=` token is stripped before the
            // point is parsed, so its position around the coordinate does not matter.
            let mut dia: Option<Nm> = None;
            let mut ptspart = String::new();
            for tok in rest.split_whitespace() {
                if let Some(d) = tok.strip_prefix("dia=") {
                    dia = Some(parse_len(d)?);
                } else {
                    ptspart.push_str(tok);
                    ptspart.push(' ');
                }
            }
            let pts = extract_points(&ptspart)?;
            if pts.len() != 1 {
                return Err("hole needs exactly one center point: hole (x,y) dia=<len>".into());
            }
            let dia = dia.ok_or("hole needs a diameter: hole (x,y) dia=<len>")?;
            if dia <= 0 {
                return Err(format!(
                    "hole: diameter must be positive (got {dia}nm) — a zero/negative drill is degenerate"
                ));
            }
            Item::Directive(GenDirective::Hole {
                center: pts[0],
                dia,
            })
        }
        "region" => {
            // `region <role> [net=<n>] [layer=<slab>] (x,y) (x,y) (x,y) ...`. Prefix
            // tokens precede the first point; role is required, net/layer optional.
            // `layer` is a slab name (Decision 13), stored verbatim and resolved
            // against the stackup at elaboration; it defaults to `F.Cu`.
            let open = rest.find('(').ok_or(
                "region needs ≥3 points: region <role> [net=..] [layer=..] (x,y) (x,y) (x,y) ...",
            )?;
            let (prefix, ptspart) = rest.split_at(open);
            let path = extract_path(ptspart)?;
            if !path_is_polygon(&path) {
                return Err("region needs ≥3 points (or an arc edge): region <role> [net=..] [layer=..] (x,y) ...".into());
            }
            let mut role: Option<Role> = None;
            let mut net: Option<String> = None;
            let mut layer = "F.Cu".to_string();
            for tok in prefix.split_whitespace() {
                if let Some(n) = tok.strip_prefix("net=") {
                    net = Some(n.to_string());
                } else if let Some(l) = tok.strip_prefix("layer=") {
                    layer = l.to_string();
                } else if role.is_none() {
                    role = Some(parse_role(tok)?);
                } else {
                    return Err(format!("region: unexpected token `{tok}`"));
                }
            }
            let role = role.ok_or("region needs a role: conductor | void | keepout[-kind]")?;
            Item::Directive(GenDirective::Region(RegionDecl {
                shape: Shape2D::polygon_path(path, 0),
                role,
                net,
                layer,
            }))
        }
        "slab" => {
            // `slab <name> <z_lo> <z_hi> <role> [material]`. z's are lengths (mm/nm via
            // `parse_len`); role uses `parse_role` widened with the slab-only roles a
            // real stackup needs (`substrate`, `marking` for silk, `mask` for solder
            // mask — `region` keeps its narrower vocabulary); material is an optional
            // bare name lowered to `Material::named`.
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.len() < 4 || toks.len() > 5 {
                return Err("slab <name> <z_lo> <z_hi> <role> [material]".into());
            }
            let z = ZRange::new(parse_len(toks[1])?, parse_len(toks[2])?);
            let role = match toks[3] {
                "substrate" => Role::Substrate,
                "marking" => Role::Marking,
                "mask" => Role::Mask,
                "datum" => Role::Datum,
                other => parse_role(other)?,
            };
            let material = toks.get(4).map(|m| Material::named(m));
            Item::Directive(GenDirective::Slab(Slab {
                name: toks[0].to_string(),
                z,
                role,
                material,
            }))
        }
        "class" => {
            // `class <name> [prefix=<val>] [template=<val>] [p:<key>=<val> ...]`. Defaults
            // reuse the `p:` param namespace from `inst`; `prefix`/`template` values may be
            // quoted (a template with spaces). Merged over the built-in seeds by
            // `annotate::registry`.
            const USAGE: &str = "class <name> [prefix=<val>] [template=<val>] [p:<key>=<val> ...]";
            let toks = split_ws_quoted(rest);
            if toks.is_empty() {
                return Err(USAGE.into());
            }
            let name = toks[0].clone();
            let mut entry = ClassEntry::default();
            for tok in &toks[1..] {
                if let Some(v) = tok.strip_prefix("prefix=") {
                    entry.prefix = Some(unquote(v).to_string());
                } else if let Some(v) = tok.strip_prefix("template=") {
                    entry.template = Some(unquote(v).to_string());
                } else if let Some(kv) = tok.strip_prefix("p:") {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| format!("class default needs p:<key>=<value>: `{tok}`"))?;
                    entry.defaults.insert(k.to_string(), unquote(v).to_string());
                } else {
                    return Err(format!("class: unexpected token `{tok}` ({USAGE})"));
                }
            }
            Item::Directive(GenDirective::Class { name, entry })
        }
        "near" => {
            let (a, b, len) = two_tokens_and_len(rest, "near <a> <b> <len>")?;
            Item::Directive(GenDirective::Near { a, b, within: len })
        }
        "minsep" => {
            let (a, b, len) = two_tokens_and_len(rest, "minsep <a> <b> <len>")?;
            Item::Directive(GenDirective::MinSep { a, b, gap: len })
        }
        "alignx" => Item::Directive(GenDirective::AlignX {
            nodes: node_list(rest, "alignx")?,
        }),
        "aligny" => Item::Directive(GenDirective::AlignY {
            nodes: node_list(rest, "aligny")?,
        }),
        "rotate" => {
            // `rotate <path> <angle> [bottom]`  |  `rotate <path> quat=(w,x,y,z)`.
            // Split the path off first so a `quat=(...)` with internal whitespace
            // (`quat=(1, 0, 0, 1)`) survives.
            let rest = rest.trim();
            let (path, spec) = rest
                .split_once(char::is_whitespace)
                .map(|(p, s)| (p.to_string(), s.trim()))
                .ok_or("rotate <path> <deg> [bottom]  |  rotate <path> quat=(w,x,y,z)")?;
            let orient = if let Some(q) = spec.strip_prefix("quat=") {
                let inner = q
                    .trim()
                    .strip_prefix('(')
                    .and_then(|s| s.strip_suffix(')'))
                    .ok_or("quat must be written quat=(w,x,y,z)")?;
                let n: Vec<&str> = inner.split(',').collect();
                if n.len() != 4 {
                    return Err("quat needs four integer components: quat=(w,x,y,z)".into());
                }
                let pi = |t: &str| {
                    t.trim()
                        .parse::<i64>()
                        .map_err(|_| format!("`{}` is not an integer", t.trim()))
                };
                let o = Orient {
                    w: pi(n[0])?,
                    x: pi(n[1])?,
                    y: pi(n[2])?,
                    z: pi(n[3])?,
                };
                if (o.w, o.x, o.y, o.z) == (0, 0, 0, 0) {
                    return Err("quat=(0,0,0,0) is not a rotation".into());
                }
                o
            } else {
                // `<angle> [bottom]` — an integer cardinal uses the tiny exact quaternion;
                // any other (finite) angle lowers once, at parse, to a scaled quaternion.
                let toks: Vec<&str> = spec.split_whitespace().collect();
                let (angle_s, bottom) = match toks.as_slice() {
                    [a] => (*a, false),
                    [a, "bottom"] => (*a, true),
                    _ => return Err("rotate <path> <deg> [bottom]".into()),
                };
                let mut o = if let Ok(d) = angle_s.parse::<i32>() {
                    Orient::from_deg(d).unwrap_or_else(|| Orient::from_angle_deg(d as f64))
                } else {
                    let deg: f64 = angle_s
                        .parse()
                        .map_err(|_| format!("`{angle_s}` is not a number of degrees"))?;
                    if !deg.is_finite() {
                        return Err(format!("rotation angle `{angle_s}` must be finite"));
                    }
                    Orient::from_angle_deg(deg)
                };
                if bottom {
                    o = o.flipped();
                }
                o
            };
            Item::Directive(GenDirective::Rotate { path, orient })
        }
        "nearpin" => {
            let (a, bpin, len) = two_tokens_and_len(rest, "nearpin <a> <bComp>.<bPin> <len>")?;
            let (b_comp, b_pin) = split_last_dot(&bpin, "pin")?;
            Item::Directive(GenDirective::NearPin {
                a,
                b_comp,
                b_pin,
                within: len,
            })
        }
        "text" => {
            // `text "<string>" (x,y) h=<len> [layer=<slab>] [rot=<deg> | rotq=(w,x,y,z)]`.
            // Pull the double-quoted string off first (it may contain spaces), then the
            // coordinate, then the remaining `key=value` tokens. `layer` is a slab name
            // (Decision 13), stored verbatim; it defaults to `F.SilkS`.
            const USAGE: &str =
                "text \"<string>\" (x,y) h=<len> [layer=<slab>] [rot=<deg> | rotq=(w,x,y,z)]";
            let q1 = rest.find('"').ok_or(USAGE)?;
            let q2 = rest[q1 + 1..]
                .find('"')
                .map(|i| q1 + 1 + i)
                .ok_or("text: string is missing its closing quote")?;
            let string = rest[q1 + 1..q2].to_string();
            let after = rest[q2 + 1..].trim();
            // The single coordinate `(x, y)`.
            let open = after.find('(').ok_or(USAGE)?;
            let close = after[open..]
                .find(')')
                .map(|i| open + i)
                .ok_or("unbalanced '(' in coordinate")?;
            let pts = extract_points(&after[open..=close])?;
            if pts.len() != 1 {
                return Err("text needs exactly one coordinate `(x, y)`".into());
            }
            let at = pts[0];
            // Trailing key=value tokens: h= (required), layer= (optional, default
            // F.SilkS), rot=/rotq=.
            let mut height: Option<Nm> = None;
            let mut layer = "F.SilkS".to_string();
            let mut orient = Orient::IDENTITY;
            for tok in after[close + 1..].split_whitespace() {
                if let Some(h) = tok.strip_prefix("h=") {
                    height = Some(parse_len(h)?);
                } else if let Some(l) = tok.strip_prefix("layer=") {
                    layer = l.to_string();
                } else if let Some(q) = tok.strip_prefix("rotq=") {
                    orient = parse_quat_tok(q)?;
                } else if let Some(r) = tok.strip_prefix("rot=") {
                    orient = parse_rot_deg(r)?;
                } else {
                    return Err(format!("text: unexpected token `{tok}`"));
                }
            }
            Item::Directive(GenDirective::Text {
                string,
                at,
                height: height.ok_or("text needs h=<len>")?,
                layer,
                orient,
            })
        }
        "font" => {
            // `font "<path>"` — the doc-wide outline font (Decision 17). The path is
            // double-quoted so it may contain spaces.
            const USAGE: &str = "font \"<path/to/font.ttf>\"";
            let q1 = rest.find('"').ok_or(USAGE)?;
            let q2 = rest[q1 + 1..]
                .find('"')
                .map(|i| q1 + 1 + i)
                .ok_or("font: path is missing its closing quote")?;
            let path = rest[q1 + 1..q2].to_string();
            if path.is_empty() {
                return Err("font: empty path".into());
            }
            Item::Directive(GenDirective::Font { path })
        }
        "use" => {
            // `use <name>` — declare a library package the document depends on. Exactly
            // one bare token: the library *name* (resolution to a directory is the
            // caller's job — a path, absolute or relative, never appears in a document).
            const USAGE: &str = "use <library-name>";
            let toks: Vec<&str> = rest.split_whitespace().collect();
            match toks.as_slice() {
                [name] => Item::Directive(GenDirective::Use {
                    name: (*name).to_string(),
                }),
                [] => return Err(format!("{USAGE} (missing name)")),
                _ => return Err(format!("use takes exactly one name ({USAGE})")),
            }
        }
        "connect" => {
            let (a, b) = two_tokens(rest, "connect <compA>.<port> <compB>.<port>")?;
            Item::Directive(GenDirective::ConnectInterface {
                a: split_last_dot(&a, "interface port")?,
                b: split_last_dot(&b, "interface port")?,
            })
        }
        "net" => {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.len() < 2 {
                return Err(
                    "net needs a name and at least one pin: net <name> <comp>.<pin> ...".into(),
                );
            }
            let net = toks[0].to_string();
            let mut pins = Vec::new();
            for t in &toks[1..] {
                pins.push(split_last_dot(t, "pin")?);
            }
            Item::Directive(GenDirective::ConnectPins { net, pins })
        }
        "nc" => {
            let toks: Vec<&str> = rest.split_whitespace().collect();
            if toks.is_empty() {
                return Err("nc needs at least one pin: nc <comp>.<pin> ...".into());
            }
            let mut pins = Vec::new();
            for t in &toks {
                pins.push(split_last_dot(t, "pin")?);
            }
            Item::Directive(GenDirective::NoConnect { pins })
        }
        "hint" | "pin" => {
            let (path, pos) = path_and_point(rest)?;
            let strength = if kw == "pin" {
                Strength::Pin
            } else {
                Strength::Hint
            };
            Item::Override(
                EntityId::new(path),
                Override {
                    pos: Some(pos),
                    strength,
                },
            )
        }
        "route" => {
            // `route <id> <net> <slab> w=<width> (x,y) (x,y) ... [free|hint|fixed]`. A
            // leading bare integer `<id>` (Decision 22) then net and slab are the leading
            // bare tokens; `w=` (required) precedes the points; an optional trailing
            // provenance keyword (default `pinned`). The id is *optional* on the way in
            // (lenient parse — a hand edit may omit it, and the caller mints one); it is
            // disambiguated by token count, not by integer-parsing net names: two bare
            // tokens are `<net> <slab>` (no id), three are `<id> <net> <slab>`. The
            // net/slab names are validated at LoadText + commit against the doc (unknown
            // net / unknown-or-non-copper slab → hard `E_UNKNOWN_*`), not here.
            const USAGE: &str =
                "route [<id>] <net> <slab> w=<width> (x,y) (x,y) ... [free|hint|fixed]";
            let open = rest.find('(').ok_or(USAGE)?;
            let (prefix, ptspart) = rest.split_at(open);
            // The points run up to a trailing provenance keyword, if any.
            let (ptspart, prov) = split_trailing_prov(ptspart)?;
            let pts = extract_points(ptspart)?;
            if pts.len() < 2 {
                return Err("route needs at least two points (a polyline)".into());
            }
            let mut width: Option<Nm> = None;
            let mut bare: Vec<&str> = Vec::new();
            for tok in prefix.split_whitespace() {
                if let Some(w) = tok.strip_prefix("w=") {
                    width = Some(parse_len(w)?);
                } else {
                    bare.push(tok);
                }
            }
            let (id, net, layer) = match bare.as_slice() {
                [net, slab] => (None, *net, *slab),
                [id, net, slab] => (Some(parse_route_id(id, "route")?), *net, *slab),
                [] | [_] => return Err(format!("route needs a net and a copper slab ({USAGE})")),
                _ => return Err(format!("route: too many bare tokens ({USAGE})")),
            };
            Item::Route(
                id,
                Trace {
                    net: crate::id::NetId::new(net),
                    layer: layer.to_string(),
                    path: pts,
                    width: width.ok_or("route needs w=<width>")?,
                    prov,
                },
            )
        }
        "via" => {
            // `via <id> <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]`. A
            // leading bare integer `<id>` (Decision 22, optional in — see `route`) then the
            // net; the single coordinate; then `drill=`/`pad=` and an optional
            // `<from>..<to>` blind/buried span (default: full copper extent). A trailing
            // provenance keyword (default `pinned`). Disambiguated by count: one bare token
            // before the coordinate is `<net>`, two are `<id> <net>`.
            const USAGE: &str =
                "via [<id>] <net> (x,y) drill=<d> pad=<p> [<from>..<to>] [free|hint|fixed]";
            let open = rest.find('(').ok_or(USAGE)?;
            let (prefix, after) = rest.split_at(open);
            let close = after.find(')').ok_or("unbalanced '(' in coordinate")?;
            let at = {
                let pts = extract_points(&after[..=close])?;
                if pts.len() != 1 {
                    return Err("via needs exactly one coordinate `(x, y)`".into());
                }
                pts[0]
            };
            let bare: Vec<&str> = prefix.split_whitespace().collect();
            let (id, net) = match bare.as_slice() {
                [net] => (None, *net),
                [id, net] => (Some(parse_route_id(id, "via")?), *net),
                [] => return Err(format!("via needs a net name ({USAGE})")),
                _ => return Err(format!("via: too many bare tokens ({USAGE})")),
            };
            let (tail, prov) = split_trailing_prov(&after[close + 1..])?;
            let mut drill: Option<Nm> = None;
            let mut pad: Option<Nm> = None;
            let mut span: Option<(String, String)> = None;
            for tok in tail.split_whitespace() {
                if let Some(d) = tok.strip_prefix("drill=") {
                    drill = Some(parse_len(d)?);
                } else if let Some(p) = tok.strip_prefix("pad=") {
                    pad = Some(parse_len(p)?);
                } else if let Some((from, to)) = tok.split_once("..") {
                    if from.is_empty() || to.is_empty() {
                        return Err(format!("via span must be `<from>..<to>`: `{tok}`"));
                    }
                    span = Some((from.to_string(), to.to_string()));
                } else {
                    return Err(format!("via: unexpected token `{tok}` ({USAGE})"));
                }
            }
            Item::Via(
                id,
                Via {
                    net: crate::id::NetId::new(net),
                    at,
                    span,
                    drill: drill.ok_or("via needs drill=<d>")?,
                    pad: pad.ok_or("via needs pad=<p>")?,
                    prov,
                },
            )
        }
        "refdes" => {
            // `refdes <path> <string>`: two tokens, the value quoted only when it must
            // be (matching `quote_value` on the way out). The string is opaque — no
            // validation against the derived class prefix.
            let toks = split_ws_quoted(rest);
            if toks.len() != 2 {
                return Err("expected: refdes <path> <string>".into());
            }
            let value = unquote(&toks[1]);
            if value.is_empty() {
                return Err("refdes string must be non-empty".into());
            }
            Item::RefdesPin(EntityId::new(&toks[0]), value.to_string())
        }
        other => return Err(format!("unknown directive `{other}`")),
    })
}
