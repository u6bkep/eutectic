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
//! when a footprint is paired with a symbol. A footprint alone defines no typed
//! [`InterfaceDef`]s. Once a symbol supplies functional pin names,
//! [`join_symbol_footprint`] runs a conservative interface-inference pass
//! ([`iface_infer::infer_interfaces`](crate::iface_infer::infer_interfaces), issue
//! 0010) over the joined part, so `PartDef.interfaces` gains a typed port only where
//! the pin names form a complete, unambiguous registry match (empty otherwise).
//!
//! What we *do* import is the pad-to-pin geometry: one [`PinDef`] per pad, named
//! by the pad's number/name, positioned at the pad's `(at x y)` converted mm→nm —
//! plus the footprint's non-copper **graphics** (issue 0016):
//! - `fp_line`/`fp_arc`/`fp_circle`/`fp_poly`/`fp_rect` on `F.SilkS`/`B.SilkS` and
//!   `F.Fab`/`B.Fab` → [`PartDef::graphics`]. Their [`Role`](geom::Role) is taken from
//!   the resolved slab by [`part::graphic_features`](crate::part::graphic_features):
//!   silk slabs are [`Role::Marking`](geom::Role); a fab slab is
//!   [`Role::Datum`](geom::Role) (Decision 15). Because `graphic_features` skips a slab
//!   absent from the stackup, fab graphics materialize into features **only** if the
//!   user authors an `F.Fab`/`B.Fab` slab — the default stackup has none.
//! - A courtyard polygon (`fp_poly`/`fp_rect` on `F.CrtYd`/`B.CrtYd`) →
//!   [`PartDef::courtyard`], the authoritative courtyard (Decision 10). Loose
//!   `fp_line`/`fp_arc` courtyard *segments* are not yet stitched into a loop.
//! - **Footprint text** (`fp_text reference|value|user`, and the v7
//!   `property "Reference"|"Value"` form) → [`PartDef::texts`] as [`FpText`] anchors
//!   (Decision 14): `reference`→[`FpTextKind::Reference`], `value`→[`FpTextKind::Label`]
//!   (both discard their placeholder string — the anchor re-derives it live at lowering),
//!   `user`→[`FpTextKind::Literal`] (except a whole-string `${REFERENCE}`/`${VALUE}` KiCad
//!   text variable, which resolves to the live Reference/Label anchor). Height is the
//!   font-size *height* component; the
//!   stroke thickness is ignored (the pen is the `height / 8` rule); `hide` is lifted (a
//!   hidden anchor round-trips as data but produces no features). Lowered by
//!   [`part::text_features`](crate::part::text_features).
//!
//! Still **skipped**: paste (`F.Paste`/`B.Paste`) — paste is *derived* at export from
//! pad geometry, never authored (Decision 15).
//! Layer references are **side-relative**: a footprint is authored top-side, so its
//! `F.*` graphics swap to `B.*` when the component is placed bottom-side (see
//! [`part::swap_side`](crate::part::swap_side)).
//!
//! ## Mapping decisions (documented contract)
//! - **Shared pad ids** (e.g. two `MP` mounting pads, or a split thermal pad that
//!   reuses one number): we keep the **first** occurrence and drop later pads with
//!   an already-seen id. They are the same electrical pad — pad id (the pad number)
//!   is the stable identity a `PinRef` keys on, so it must stay unique within a
//!   part. (Distinct pads that share a *functional name* after a symbol join — six
//!   `IOVDD` — are all kept; names may collide, ids may not.)
//! - **Unnamed pads** (`name == ""`, used for thermal/exposed pads and mechanical
//!   features): **skipped**. An empty name carries no electrical identity, and a
//!   footprint's roles come from the symbol anyway.
//! - The pad rotation in `(at x y angle)` is **ignored** for the offset (we import
//!   the pad *position* only).
//!
//! Both the modern `(footprint "name" ...)` and the legacy `(module name ...)`
//! headers are accepted; pad names may be quoted or bare.

use crate::doc::{Nm, Orient, Point};
use crate::geom;
use crate::geom::{Seg, Shape2D};
use crate::part::{
    Drill, FpGraphic, FpText, FpTextKind, PadCopper, PadGeo, PadLayers, PartDef, PinDef, PinRole,
};
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
        let Some(pad) = item.list_headed("pad") else {
            continue;
        };
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
        let offset = Point {
            x: mm_to_nm(x)?,
            y: mm_to_nm(y)?,
        };
        // Real pad copper + drill geometry, in component-local coords centred at the
        // pad's `(at)`. The shape/size/drill/layers/rotation are all lifted here.
        let pad = parse_pad_geometry(pad, offset)?;
        // A bare footprint has no functional naming: name == number == the pad id.
        pins.push(PinDef {
            name: pad_name.to_string(),
            number: pad_name.to_string(),
            role: PinRole::Passive,
            offset,
            pad,
        });
    }

    // Footprint graphics: silkscreen + fab → `graphics` (side-relative slab names; the
    // role is taken from the resolved slab at lowering, so fab graphics materialize only
    // if the stackup carries a fab slab — Decision 15), and a courtyard outline → the
    // authoritative `courtyard` (Decision 10). Still skipped: `fp_text`/auto-text (a
    // separate branch) and paste (Decision 15: derived at export) — see the module doc.
    let mut graphics: Vec<FpGraphic> = Vec::new();
    let mut courtyard: Option<Shape2D> = None;
    for item in items {
        let Some((shape, layer)) = parse_fp_graphic(item)? else {
            continue;
        };
        match layer.as_str() {
            "F.SilkS" | "B.SilkS" | "F.Fab" | "B.Fab" => graphics.push(FpGraphic { shape, layer }),
            // A courtyard is a single closed outline. We take a `fp_poly`/`fp_rect`
            // (a `Shape2D::Polygon`); loose `fp_line`/`fp_arc` courtyard segments are
            // not stitched into a loop yet, so they are ignored (noted). Last one wins.
            "F.CrtYd" | "B.CrtYd" if matches!(shape, Shape2D::Polygon { .. }) => {
                courtyard = Some(shape);
            }
            _ => {}
        }
    }

    // Footprint text → `texts` (Decision 14): `fp_text reference|value|user` and the v7
    // `property "Reference"|"Value"` form. The placeholder string ("REF**"/the value
    // placeholder) is discarded — a Reference/Label anchor re-derives its string at
    // lowering; only `user` text keeps its literal. `hide` anchors import as data (they
    // round-trip) but produce no features.
    let mut texts: Vec<FpText> = Vec::new();
    for item in items {
        if let Some(t) = parse_fp_text(item)? {
            texts.push(t);
        }
    }

    Ok(PartDef {
        name,
        pins,
        interfaces: BTreeMap::new(),
        graphics,
        texts,
        courtyard,
        // The importer does not infer class from a footprint (Decision 14, out of scope).
        class: None,
    })
}

/// Parse one footprint text node into an [`FpText`] anchor, or `Ok(None)` if it isn't
/// footprint text (or lacks a `(layer …)`). Two forms:
///
/// - classic `(fp_text reference|value|user "STR" (at x y [rot]) (layer L) [hide]
///   (effects (font (size H W) (thickness T))))`, and
/// - v7 `(property "Reference"|"Value" "STR" (at …) (layer L) [(hide yes)] (effects …))`.
///
/// Mapping (Decision 14): `reference`/`Reference` → [`FpTextKind::Reference`] (placeholder
/// discarded), `value`/`Value` → [`FpTextKind::Label`] (placeholder discarded), `user` →
/// [`FpTextKind::Literal`] keeping the string — except a `user` string that is *exactly*
/// the `${REFERENCE}`/`${VALUE}` KiCad text variable resolves to the live Reference/Label
/// anchor (fab layers commonly echo the refdes this way); mixed content stays literal
/// (see [`text_kind_from_user`]). Height is the font `(size H …)` height component
/// (default 1 mm if absent); the stroke `(thickness …)` is **ignored** — the pen is the
/// `height / 8` rule (Decision 14). `(at … rot)` becomes a local about-z [`Orient`] (exact
/// for cardinals). The layer name is kept as imported (side-relative). Other `property`
/// names (Footprint/Datasheet/…) are footprint metadata, not silk, and return `Ok(None)`.
fn parse_fp_text(item: &Sexp) -> Result<Option<FpText>, String> {
    let Some(list) = item.as_list() else {
        return Ok(None);
    };
    let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
    let kind = match head {
        "fp_text" => match list.get(1).and_then(Sexp::as_atom).unwrap_or("") {
            "reference" => FpTextKind::Reference,
            "value" => FpTextKind::Label,
            "user" => text_kind_from_user(list.get(2).and_then(Sexp::as_atom).unwrap_or("")),
            _ => return Ok(None),
        },
        "property" => match list.get(1).and_then(Sexp::as_atom).unwrap_or("") {
            "Reference" => FpTextKind::Reference,
            "Value" => FpTextKind::Label,
            _ => return Ok(None), // metadata property, not silk text
        },
        _ => return Ok(None),
    };
    let Some(layer) = layer_name(list) else {
        return Ok(None);
    };
    let at = prim_xy(list, "at")?.unwrap_or(Point { x: 0, y: 0 });
    let rot = list
        .iter()
        .find_map(|s| s.list_headed("at"))
        .and_then(|a| a.get(3))
        .and_then(Sexp::as_atom)
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    // Cardinal rotations get the tiny exact quaternion; off-axis angles are approximated.
    let orient = Orient::from_deg(rot as i32).unwrap_or_else(|| Orient::from_angle_deg(rot));
    let height = text_font_height(list).unwrap_or(1_000_000); // KiCad default text size ≈ 1 mm
    Ok(Some(FpText {
        kind,
        at,
        height,
        layer,
        orient,
        hide: text_hidden(list),
    }))
}

/// Map a `fp_text user` string to a kind: the KiCad text variables `${REFERENCE}` and
/// `${VALUE}`, matched as the **whole** string, become the live Reference/Label anchors;
/// anything else (including mixed content like `X ${REFERENCE}`) stays a verbatim literal.
fn text_kind_from_user(s: &str) -> FpTextKind {
    match s {
        "${REFERENCE}" => FpTextKind::Reference,
        "${VALUE}" => FpTextKind::Label,
        _ => FpTextKind::Literal(s.to_string()),
    }
}

/// A footprint text's font **height** in nm: the first component of
/// `(effects (font (size H W) …))` (KiCad lists height then width). `None` if absent.
fn text_font_height(list: &[Sexp]) -> Option<Nm> {
    list.iter()
        .find_map(|s| s.list_headed("effects"))
        .and_then(|eff| eff.iter().find_map(|s| s.list_headed("font")))
        .and_then(|font| font.iter().find_map(|s| s.list_headed("size")))
        .and_then(|size| size.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| mm_to_nm(a).ok())
}

/// Is a footprint text hidden? Both the classic bare `hide` atom (at the text level) and
/// the v7 `(hide yes)` list — at the text level or nested in `(effects …)` — count.
/// `(hide no)` is explicitly not hidden.
fn text_hidden(list: &[Sexp]) -> bool {
    let hidden_in = |l: &[Sexp]| {
        l.iter().any(|s| s.as_atom() == Some("hide"))
            || l.iter()
                .find_map(|s| s.list_headed("hide"))
                .is_some_and(|h| h.get(1).and_then(Sexp::as_atom) != Some("no"))
    };
    hidden_in(list)
        || list
            .iter()
            .find_map(|s| s.list_headed("effects"))
            .is_some_and(hidden_in)
}

/// Parse one footprint graphic (`fp_line`/`fp_arc`/`fp_circle`/`fp_poly`/`fp_rect`)
/// into its component-local [`Shape2D`] + slab layer name. Coordinates are already in
/// the footprint frame (no pad-centre offset), so this reuses the `gr_*` point readers
/// with a zero centre. Stroke width comes from `(stroke (width w))` (modern) or a bare
/// `(width w)` (legacy) and, per this crate's convention, is baked into the shape's
/// Minkowski radius — `fp_line`→capsule, `fp_arc`→arc stroke (both `width/2`); a
/// zero-width stroke carries no ink ⇒ `Ok(None)`. `fp_rect`/`fp_poly` build the filled
/// polygon; `fp_circle` builds a filled disc (an outline-only circle is approximated as
/// filled — the same simplification the custom-pad `gr_circle` path makes). `Ok(None)`
/// for any other head or a graphic with no `(layer …)`.
fn parse_fp_graphic(item: &Sexp) -> Result<Option<(Shape2D, String)>, String> {
    let Some(list) = item.as_list() else {
        return Ok(None);
    };
    let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
    let origin = Point { x: 0, y: 0 };
    let width = graphic_width(list);
    let shape = match head {
        "fp_line" => {
            let s = prim_xy(list, "start")?.ok_or("fp_line missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("fp_line missing (end …)")?;
            (width > 0).then(|| Shape2D::capsule(s, e, width / 2))
        }
        "fp_arc" => {
            if width <= 0 {
                None
            } else {
                let (start, mid, end) = gr_arc_points(list, origin)?;
                Some(Shape2D::arc(start, mid, end, width))
            }
        }
        "fp_circle" => {
            let c = prim_xy(list, "center")?.ok_or("fp_circle missing (center …)")?;
            let e = prim_xy(list, "end")?.ok_or("fp_circle missing (end …)")?;
            let r = dist_nm(c, e);
            (r > 0).then(|| Shape2D::disc(c, r))
        }
        "fp_rect" => {
            let s = prim_xy(list, "start")?.ok_or("fp_rect missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("fp_rect missing (end …)")?;
            Some(Shape2D::polygon(vec![
                s,
                Point { x: e.x, y: s.y },
                e,
                Point { x: s.x, y: e.y },
            ]))
        }
        "fp_poly" => {
            let pts = prim_pts(list)?;
            (pts.len() >= 3).then(|| Shape2D::polygon(pts))
        }
        _ => return Ok(None),
    };
    let (Some(shape), Some(layer)) = (shape, layer_name(list)) else {
        return Ok(None);
    };
    Ok(Some((shape, layer)))
}

/// A footprint graphic's stroke width in nm: modern `(stroke (width w) …)` or the
/// legacy bare `(width w)`. `0` (⇒ a filled, unstroked shape) if neither is present.
fn graphic_width(list: &[Sexp]) -> Nm {
    if let Some(w) = list
        .iter()
        .find_map(|s| s.list_headed("stroke"))
        .and_then(|st| st.iter().find_map(|s| s.list_headed("width")))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| mm_to_nm(a).ok())
    {
        return w;
    }
    prim_width(list)
}

/// A graphic item's `(layer "X")` name (quoted or bare), if present.
fn layer_name(list: &[Sexp]) -> Option<String> {
    list.iter()
        .find_map(|s| s.list_headed("layer"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .map(str::to_string)
}

/// Lift a pad's real copper + drill geometry out of a
/// `(pad <name> <type> <shape> (at x y [angle]) (size w h) (layers …) (drill …) …)`
/// node, in component-local coordinates centred at `center` (the pad's `(at)`).
///
/// `circle`/`rect`/`roundrect`/`oval` build exact [`Shape2D`]s; `trapezoid`/`custom`/
/// `chamfered_rect` and any other token fall back to the bounding rectangle — a
/// conservative copper extent. (Full custom `(primitives …)` import is a follow-up;
/// the [`PadGeo`] representation already supports compound pads as a union.) The pad
/// `(at)` angle is baked into the geometry — exact for cardinal rotations, off-axis
/// angles float-rotated and rounded to nm *at import* (like mm→nm). A pad with no
/// `(size …)` and no `(drill …)` yields `None`.
fn parse_pad_geometry(pad: &[Sexp], center: Point) -> Result<Option<PadGeo>, String> {
    let pad_type = pad.get(2).and_then(Sexp::as_atom).unwrap_or("");
    let shape_tok = pad.get(3).and_then(Sexp::as_atom).unwrap_or("");
    let angle = pad
        .iter()
        .find_map(|s| s.list_headed("at"))
        .and_then(|at| at.get(3))
        .and_then(Sexp::as_atom)
        .and_then(|a| a.parse::<f64>().ok())
        .unwrap_or(0.0);

    let drill = parse_drill(pad, center, angle)?;
    let layers = pad_layers(pad, pad_type);

    let copper = if let Some(size) = pad.iter().find_map(|s| s.list_headed("size")) {
        let w = mm_to_nm(
            size.get(1)
                .and_then(Sexp::as_atom)
                .ok_or("pad (size …) missing width")?,
        )?;
        let h = mm_to_nm(
            size.get(2)
                .and_then(Sexp::as_atom)
                .ok_or("pad (size …) missing height")?,
        )?;
        let shapes: Vec<Shape2D> = match shape_tok {
            "circle" => vec![Shape2D::disc(center, w / 2)],
            "roundrect" => {
                let rratio = pad
                    .iter()
                    .find_map(|s| s.list_headed("roundrect_rratio"))
                    .and_then(|l| l.get(1))
                    .and_then(Sexp::as_atom)
                    .and_then(|a| a.parse::<f64>().ok())
                    .unwrap_or(0.25);
                let r = ((w.min(h) as f64) * rratio).round() as Nm;
                vec![Shape2D::round_rect(center, w, h, r)]
            }
            "oval" => vec![oval_shape(center, w, h)],
            // A custom pad is the union of its anchor + `(primitives …)` — including
            // `gr_arc` edges, now that `Shape2D` carries arcs.
            "custom" => parse_custom_copper(pad, center, w, h)?,
            // trapezoid / chamfered_rect / …: bounding rectangle (a documented
            // conservative fallback; only `custom` gets exact compound geometry).
            _ => vec![Shape2D::rect(center, w, h)],
        };
        // The pad `(at)` angle rotates the whole compound shape.
        shapes
            .into_iter()
            .map(|s| PadCopper {
                shape: rotate_shape(s, center, angle),
                layers,
            })
            .collect()
    } else {
        Vec::new()
    };

    if copper.is_empty() && drill.is_none() {
        return Ok(None);
    }
    Ok(Some(PadGeo { copper, drill }))
}

/// An oval/pill pad of size `w`×`h` centred at `c`: a capsule along the longer axis
/// (a circle when `w == h`).
fn oval_shape(c: Point, w: Nm, h: Nm) -> Shape2D {
    if w == h {
        Shape2D::disc(c, w / 2)
    } else if w > h {
        let dx = (w - h) / 2;
        Shape2D::capsule(
            Point {
                x: c.x - dx,
                y: c.y,
            },
            Point {
                x: c.x + dx,
                y: c.y,
            },
            h / 2,
        )
    } else {
        let dy = (h - w) / 2;
        Shape2D::capsule(
            Point {
                x: c.x,
                y: c.y - dy,
            },
            Point {
                x: c.x,
                y: c.y + dy,
            },
            w / 2,
        )
    }
}

/// The copper of a `custom` pad: its anchor shape (the `(size …)` rectangle, or a disc
/// for `(anchor circle)`) **unioned** with every `(primitives …)` element, in
/// pre-rotation world coords (centred at the pad `(at)`). KiCad renders a custom pad as
/// exactly this union; [`PadGeo::copper`] is already a `Vec` for it. Unknown primitive
/// kinds (e.g. `gr_text`) are skipped. The pad `(at)` rotation is applied by the caller.
fn parse_custom_copper(pad: &[Sexp], center: Point, w: Nm, h: Nm) -> Result<Vec<Shape2D>, String> {
    let anchor = pad
        .iter()
        .find_map(|s| s.list_headed("options"))
        .and_then(|o| o.iter().find_map(|s| s.list_headed("anchor")))
        .and_then(|a| a.get(1))
        .and_then(Sexp::as_atom)
        .unwrap_or("rect");
    let mut shapes = vec![match anchor {
        "circle" => Shape2D::disc(center, w.min(h) / 2),
        _ => Shape2D::rect(center, w, h),
    }];
    if let Some(prims) = pad.iter().find_map(|s| s.list_headed("primitives")) {
        for prim in &prims[1..] {
            if let Some(shape) = parse_primitive(prim, center)? {
                shapes.push(shape);
            }
        }
    }
    Ok(shapes)
}

/// One custom-pad primitive → a [`Shape2D`] in pre-rotation world coords (`center` +
/// the primitive's pad-local coordinates). Handles `gr_circle` / `gr_line` / `gr_rect`
/// / `gr_poly` / `gr_arc`; other kinds (text, etc.) return `None`. Filled primitives
/// become filled shapes; stroked ones (`width > 0`) become the stroke ⊕ width/2.
fn parse_primitive(prim: &Sexp, center: Point) -> Result<Option<Shape2D>, String> {
    let Some(list) = prim.as_list() else {
        return Ok(None);
    };
    let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
    let off = |p: Point| Point {
        x: center.x + p.x,
        y: center.y + p.y,
    };
    Ok(match head {
        "gr_circle" => {
            let c = prim_xy(list, "center")?.ok_or("gr_circle missing (center …)")?;
            let e = prim_xy(list, "end")?.ok_or("gr_circle missing (end …)")?;
            let r = dist_nm(c, e);
            (r > 0).then(|| Shape2D::disc(off(c), r))
        }
        "gr_line" => {
            let s = prim_xy(list, "start")?.ok_or("gr_line missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("gr_line missing (end …)")?;
            let width = prim_width(list);
            (width > 0).then(|| Shape2D::capsule(off(s), off(e), width / 2))
        }
        "gr_rect" => {
            let s = prim_xy(list, "start")?.ok_or("gr_rect missing (start …)")?;
            let e = prim_xy(list, "end")?.ok_or("gr_rect missing (end …)")?;
            Some(Shape2D::polygon(vec![
                off(s),
                off(Point { x: e.x, y: s.y }),
                off(e),
                off(Point { x: s.x, y: e.y }),
            ]))
        }
        "gr_poly" => {
            let pts = prim_pts(list)?;
            (pts.len() >= 3).then(|| Shape2D::polygon(pts.into_iter().map(off).collect()))
        }
        "gr_arc" => parse_gr_arc(list, center)?,
        _ => None,
    })
}

/// A `gr_arc` primitive → an arc-stroke [`Shape2D`]. Two KiCad encodings:
///   - **3-point** `(start)(mid)(end)`: used directly (matches our [`Seg::Arc`]).
///   - **legacy** `(start = centre)(end = arc start point)(angle = swept °)`: the end
///     and mid are the arc-start rotated by `angle` and `angle/2` about the centre.
///     Using the *same* `angle` for both guarantees the mid lands on the swept arc
///     whatever the sign convention. Zero-width arcs carry no copper ⇒ `None`.
fn parse_gr_arc(list: &[Sexp], center: Point) -> Result<Option<Shape2D>, String> {
    let width = prim_width(list);
    if width <= 0 {
        return Ok(None);
    }
    let (start, mid, end) = gr_arc_points(list, center)?;
    Ok(Some(Shape2D::arc(start, mid, end, width)))
}

/// The three lattice points `(start, mid, end)` of a `gr_arc`, in `center`-offset
/// coords, normalising both KiCad encodings (the shared core of [`parse_gr_arc`] and
/// the board-outline importer, neither of which cares about stroke width):
///   - **3-point** `(start)(mid)(end)`: used directly (matches our [`Seg::Arc`]).
///   - **legacy** `(start = centre)(end = arc start)(angle = swept °)`: the arc runs
///     from the arc-start point, with `end`/`mid` its rotation by `angle`/`angle/2`
///     about the centre (the same `angle` for both keeps the mid on the swept side
///     whatever the sign convention).
fn gr_arc_points(list: &[Sexp], center: Point) -> Result<(Point, Point, Point), String> {
    let off = |p: Point| Point {
        x: center.x + p.x,
        y: center.y + p.y,
    };
    let start = prim_xy(list, "start")?.ok_or("gr_arc missing (start …)")?;
    let end = prim_xy(list, "end")?.ok_or("gr_arc missing (end …)")?;
    if let Some(mid) = prim_xy(list, "mid")? {
        Ok((off(start), off(mid), off(end)))
    } else if let Some(angle) = prim_angle(list) {
        let (c, p0) = (off(start), off(end));
        Ok((
            p0,
            rotate_point(p0, c, angle / 2.0),
            rotate_point(p0, c, angle),
        ))
    } else {
        Err("gr_arc needs either (mid …) or (angle …)".into())
    }
}

/// A `(<head> x y)` child of `list`, mm→nm. `Ok(None)` if absent, `Err` if malformed.
fn prim_xy(list: &[Sexp], head: &str) -> Result<Option<Point>, String> {
    let Some(l) = list.iter().find_map(|s| s.list_headed(head)) else {
        return Ok(None);
    };
    let x = mm_to_nm(
        l.get(1)
            .and_then(Sexp::as_atom)
            .ok_or(format!("{head} missing x"))?,
    )?;
    let y = mm_to_nm(
        l.get(2)
            .and_then(Sexp::as_atom)
            .ok_or(format!("{head} missing y"))?,
    )?;
    Ok(Some(Point { x, y }))
}

/// A primitive's `(width w)` in nm (0 if absent ⇒ a filled, not stroked, primitive).
fn prim_width(list: &[Sexp]) -> Nm {
    list.iter()
        .find_map(|s| s.list_headed("width"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| mm_to_nm(a).ok())
        .unwrap_or(0)
}

/// A primitive's `(angle a)` in degrees (legacy `gr_arc` sweep), if present.
fn prim_angle(list: &[Sexp]) -> Option<f64> {
    list.iter()
        .find_map(|s| s.list_headed("angle"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        .and_then(|a| a.parse::<f64>().ok())
}

/// A `gr_poly`'s `(pts (xy x y) …)` as points (mm→nm).
fn prim_pts(list: &[Sexp]) -> Result<Vec<Point>, String> {
    let Some(pts) = list.iter().find_map(|s| s.list_headed("pts")) else {
        return Ok(vec![]);
    };
    let mut out = Vec::new();
    for xy in &pts[1..] {
        if let Some(l) = xy.list_headed("xy") {
            let x = mm_to_nm(l.get(1).and_then(Sexp::as_atom).ok_or("xy missing x")?)?;
            let y = mm_to_nm(l.get(2).and_then(Sexp::as_atom).ok_or("xy missing y")?)?;
            out.push(Point { x, y });
        }
    }
    Ok(out)
}

/// Distance between two points, nm, rounded (import-time float — like mm→nm rounding).
fn dist_nm(a: Point, b: Point) -> Nm {
    let (dx, dy) = ((a.x - b.x) as f64, (a.y - b.y) as f64);
    (dx * dx + dy * dy).sqrt().round() as Nm
}

/// Rotate a point about `center` by `deg` (KiCad CCW degrees). Exact for the four
/// cardinal angles; off-axis angles use float trig rounded to nm (import-time only).
fn rotate_point(p: Point, center: Point, deg: f64) -> Point {
    let d = ((deg % 360.0) + 360.0) % 360.0;
    if d == 0.0 {
        return p;
    }
    let (dx, dy) = (p.x - center.x, p.y - center.y);
    let (rx, ry) = if d == 90.0 {
        (-dy, dx)
    } else if d == 180.0 {
        (-dx, -dy)
    } else if d == 270.0 {
        (dy, -dx)
    } else {
        let r = d.to_radians();
        let (sin, cos) = (r.sin(), r.cos());
        (
            ((dx as f64) * cos - (dy as f64) * sin).round() as Nm,
            ((dx as f64) * sin + (dy as f64) * cos).round() as Nm,
        )
    };
    Point {
        x: center.x + rx,
        y: center.y + ry,
    }
}

/// Rotate a shape's vertices about `center` by `deg` (see [`rotate_point`]).
fn rotate_shape(s: Shape2D, center: Point, deg: f64) -> Shape2D {
    s.map_points(|p| rotate_point(p, center, deg))
}

/// Parse a pad's `(drill <d>)` (round) or `(drill oval <w> <h>)` (slot, along the
/// longer axis), centred at `center` and rotated by the pad `(at)` angle so the
/// drill agrees with the copper. `None` if the pad has no drill. (A drill `(offset
/// …)` is not yet applied — the hole sits at the pad centre; rare, noted.)
fn parse_drill(pad: &[Sexp], center: Point, angle: f64) -> Result<Option<Drill>, String> {
    let Some(d) = pad.iter().find_map(|s| s.list_headed("drill")) else {
        return Ok(None);
    };
    match d.get(1).and_then(Sexp::as_atom) {
        Some("oval") => {
            let w = mm_to_nm(
                d.get(2)
                    .and_then(Sexp::as_atom)
                    .ok_or("drill oval missing w")?,
            )?;
            let h = mm_to_nm(
                d.get(3)
                    .and_then(Sexp::as_atom)
                    .ok_or("drill oval missing h")?,
            )?;
            let (a, b, dia) = if w >= h {
                let dx = (w - h) / 2;
                (
                    Point {
                        x: center.x - dx,
                        y: center.y,
                    },
                    Point {
                        x: center.x + dx,
                        y: center.y,
                    },
                    h,
                )
            } else {
                let dy = (h - w) / 2;
                (
                    Point {
                        x: center.x,
                        y: center.y - dy,
                    },
                    Point {
                        x: center.x,
                        y: center.y + dy,
                    },
                    w,
                )
            };
            Ok(Some(Drill::Slot {
                a: rotate_point(a, center, angle),
                b: rotate_point(b, center, angle),
                d: dia,
            }))
        }
        Some(tok) => Ok(Some(Drill::Round { d: mm_to_nm(tok)? })),
        None => Ok(None),
    }
}

/// Which copper layer(s) a pad occupies: through-hole types span the board; otherwise
/// read `(layers …)` — `*.` or both outer layers ⇒ through, a lone `B.Cu` ⇒ bottom,
/// else top.
fn pad_layers(pad: &[Sexp], pad_type: &str) -> PadLayers {
    if pad_type == "thru_hole" || pad_type == "np_thru_hole" {
        return PadLayers::Through;
    }
    if let Some(l) = pad.iter().find_map(|s| s.list_headed("layers")) {
        let toks: Vec<&str> = l.iter().skip(1).filter_map(Sexp::as_atom).collect();
        let (has_f, has_b) = (toks.contains(&"F.Cu"), toks.contains(&"B.Cu"));
        if toks.iter().any(|t| t.starts_with("*.")) || (has_f && has_b) {
            return PadLayers::Through;
        }
        if has_b {
            return PadLayers::Bottom;
        }
    }
    PadLayers::Top
}

/// Convenience wrapper: read a `.kicad_mod` file from disk and import it.
pub fn import_footprint_file(path: &str) -> Result<PartDef, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    import_footprint(&text)
}

// =============================================================================
// Board outline (.kicad_pcb `Edge.Cuts`) → (outline, cutouts)
// =============================================================================
//
// A `.kicad_pcb` is one big S-expression `(kicad_pcb …)`, so we reuse the
// tokenizer/reader/`Sexp` machinery above (no second parser). This importer lifts
// **only the board boundary**: the top-level `gr_line` / `gr_arc` / `gr_circle`
// graphics on the `Edge.Cuts` layer, stitched into closed loops and classified into
// an outline + cutouts.
//
// **Scope.** Outline + cutouts only. Placed footprints, their positions/rotations,
// nets, tracks, zones, and vias are *not* imported — that is the larger
// board-round-trip feature (see issue 0017) and is deliberately out of scope here.
//
// Coordinates are mm in the file → integer nm via [`mm_to_nm`], matching the
// fixed-point invariant. Disjoint edges are chained by matching endpoints within a
// tiny [`TOUCH_TOL`] slack (KiCad coordinates are exact nm, but the slack tolerates
// any rounding); each closed loop becomes a [`Shape2D::Polygon`] whose edges are
// `Seg::Line`/`Seg::Arc`. The loop of largest area is the `outline`; the rest are
// `cutouts`.

/// Endpoint-match slack for stitching `Edge.Cuts` segments into loops, in nm (1 µm).
/// KiCad writes exact nm so consecutive edges normally share an endpoint exactly;
/// this only absorbs sub-µm rounding noise.
const TOUCH_TOL: Nm = 1_000;

/// One `Edge.Cuts` graphic as an undirected edge: endpoints `a`/`b` plus, for an arc,
/// the on-curve `mid` point. Emitted as a [`Seg`] in whichever direction the stitch
/// walks it (an arc's `mid` stays on the curve when reversed).
struct EdgeSeg {
    a: Point,
    b: Point,
    mid: Option<Point>,
}

impl EdgeSeg {
    /// The [`Seg`] for walking this edge away from endpoint `from` (`~a` ⇒ ends at
    /// `b`, else ends at `a`); also returns the far endpoint reached.
    fn seg_from(&self, from: Point) -> (Seg, Point) {
        let end = if near(from, self.a) { self.b } else { self.a };
        match self.mid {
            Some(mid) => (Seg::Arc { mid, end }, end),
            None => (Seg::Line { end }, end),
        }
    }
}

/// Are two points within [`TOUCH_TOL`] of each other (squared, exact i128)?
fn near(p: Point, q: Point) -> bool {
    let (dx, dy) = ((p.x - q.x) as i128, (p.y - q.y) as i128);
    dx * dx + dy * dy <= (TOUCH_TOL as i128) * (TOUCH_TOL as i128)
}

/// Does a graphic item carry `(layer "Edge.Cuts")` (quoted or bare)?
fn on_edge_cuts(list: &[Sexp]) -> bool {
    list.iter()
        .find_map(|s| s.list_headed("layer"))
        .and_then(|l| l.get(1))
        .and_then(Sexp::as_atom)
        == Some("Edge.Cuts")
}

/// Import a `.kicad_pcb`'s board outline: parse the `Edge.Cuts` `gr_line`/`gr_arc`/
/// `gr_circle` graphics, stitch them into closed loops, and return the authored board
/// geometry as `(outline, cutouts)` — [`geom::Shape2D`]s that become `Board`/`Cutout`
/// directives (largest-area loop = `outline`, the rest = `cutouts`; arcs preserved).
/// The board's *derived* region (outline ∖ cutouts) is [`elaborate::board_region`].
///
/// **Only the board boundary is imported** — no placed footprints, nets, tracks or
/// zones (that full round-trip is a separate, larger feature; see issue 0017). Errors
/// if there is no `Edge.Cuts` geometry or if its segments do not close into a loop.
pub fn import_board_outline(text: &str) -> Result<(geom::Shape2D, Vec<geom::Shape2D>), String> {
    let toks = tokenize(text)?;
    let root = read(&toks)?;
    let items = root.as_list().ok_or("top-level expression is not a list")?;
    if items.first().and_then(Sexp::as_atom) != Some("kicad_pcb") {
        return Err(format!(
            "expected '(kicad_pcb …)', got {:?}",
            items.first().and_then(Sexp::as_atom)
        ));
    }

    // gr_line / gr_arc become open edges to be stitched; gr_circle is already a
    // closed loop and goes straight into the loop list.
    let mut edges: Vec<EdgeSeg> = Vec::new();
    let mut loops: Vec<geom::Path> = Vec::new();
    for item in items {
        let Some(list) = item.as_list() else { continue };
        let head = list.first().and_then(Sexp::as_atom).unwrap_or("");
        if !matches!(head, "gr_line" | "gr_arc" | "gr_circle") || !on_edge_cuts(list) {
            continue;
        }
        match head {
            "gr_line" => {
                let a = prim_xy(list, "start")?.ok_or("gr_line missing (start …)")?;
                let b = prim_xy(list, "end")?.ok_or("gr_line missing (end …)")?;
                edges.push(EdgeSeg { a, b, mid: None });
            }
            "gr_arc" => {
                let (s, m, e) = gr_arc_points(list, Point { x: 0, y: 0 })?;
                edges.push(EdgeSeg {
                    a: s,
                    b: e,
                    mid: Some(m),
                });
            }
            "gr_circle" => loops.push(circle_loop(list)?),
            _ => unreachable!(),
        }
    }

    if edges.is_empty() && loops.is_empty() {
        return Err("no Edge.Cuts graphics found in board".into());
    }
    loops.extend(stitch_loops(edges)?);

    // Classify by area: the largest loop is the board outline, the rest are cutouts.
    // (For real boards the outline both has the largest area and contains the others.)
    let mut indexed: Vec<(i128, geom::Shape2D)> = loops
        .into_iter()
        .map(|path| {
            let shape = geom::Shape2D::polygon_path(path, 0);
            (loop_area(&shape), shape)
        })
        .collect();
    indexed.sort_by_key(|y| std::cmp::Reverse(y.0));
    let mut shapes = indexed.into_iter().map(|(_, s)| s);
    let outline = shapes
        .next()
        .ok_or("Edge.Cuts has no closed loop to use as the board outline")?;
    Ok((outline, shapes.collect()))
}

/// Convenience wrapper: read a `.kicad_pcb` file from disk and import its outline.
pub fn import_board_outline_file(
    path: &str,
) -> Result<(geom::Shape2D, Vec<geom::Shape2D>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path:?}: {e}"))?;
    import_board_outline(&text)
}

/// A `gr_circle (center …)(end …)` → a closed two-semicircle-arc [`geom::Path`]. `end`
/// is a point on the circle, so the radius is `|center − end|`; we walk the circle via
/// the four axis points (cardinal), independent of where `end` sits.
fn circle_loop(list: &[Sexp]) -> Result<geom::Path, String> {
    let c = prim_xy(list, "center")?.ok_or("gr_circle missing (center …)")?;
    let e = prim_xy(list, "end")?.ok_or("gr_circle missing (end …)")?;
    let r = dist_nm(c, e);
    if r <= 0 {
        return Err("gr_circle has zero radius".into());
    }
    let right = Point { x: c.x + r, y: c.y };
    let top = Point { x: c.x, y: c.y + r };
    let left = Point { x: c.x - r, y: c.y };
    let bottom = Point { x: c.x, y: c.y - r };
    Ok(geom::Path {
        start: right,
        segs: vec![
            Seg::Arc {
                mid: top,
                end: left,
            },
            Seg::Arc {
                mid: bottom,
                end: right,
            },
        ],
    })
}

/// Chain undirected [`EdgeSeg`]s into closed loops by matching endpoints within
/// [`TOUCH_TOL`]. Greedy: take any unused edge as a loop seed, then keep appending the
/// edge touching the current open end (in either direction) until it returns to the
/// loop's start. Errors if an edge has no continuation (an open contour, which is not
/// a valid board boundary).
fn stitch_loops(mut edges: Vec<EdgeSeg>) -> Result<Vec<geom::Path>, String> {
    let mut loops = Vec::new();
    while let Some(first) = edges.pop() {
        let loop_start = first.a;
        let (seg0, mut cur) = first.seg_from(loop_start);
        let mut segs = vec![seg0];
        while !near(cur, loop_start) {
            let Some(idx) = edges.iter().position(|e| near(e.a, cur) || near(e.b, cur)) else {
                return Err("Edge.Cuts segments do not form a closed loop (open contour)".into());
            };
            let e = edges.remove(idx);
            let (seg, next) = e.seg_from(cur);
            segs.push(seg);
            cur = next;
        }
        // The loop closes back at `loop_start`. A closing straight edge is the
        // polygon's *implicit* final `Line`, so drop it to avoid a redundant repeated
        // vertex (keep a closing `Arc` — it carries real curvature the implicit line
        // can't). Guard so we never collapse below a triangle.
        if segs.len() >= 3 && matches!(segs.last(), Some(Seg::Line { .. })) {
            segs.pop();
        }
        loops.push(geom::Path {
            start: loop_start,
            segs,
        });
    }
    Ok(loops)
}

/// Signed area ×2 of a closed loop, via the shoelace formula over the polygon's
/// flattened skeleton (arcs subdivided to [`geom::DEFAULT_CHORD_TOL`]). Exact i128;
/// magnitude only is used (orientation is irrelevant to classification).
fn loop_area(shape: &geom::Shape2D) -> i128 {
    let pts = shape.path().flatten(geom::DEFAULT_CHORD_TOL);
    let n = pts.len();
    let mut a2: i128 = 0;
    for i in 0..n {
        let p = pts[i];
        let q = pts[(i + 1) % n];
        a2 += p.x as i128 * q.y as i128 - q.x as i128 * p.y as i128;
    }
    a2.abs()
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
    // (see [`iface_infer`](crate::iface_infer)).
    crate::iface_infer::infer_interfaces(&mut part);
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

#[cfg(test)]
mod tests;
