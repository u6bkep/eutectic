//! Derived **display** annotation (Decision 14): reference designators, effective
//! parameters, and rendered labels — none of them stored, all recomputed from the
//! component universe and the class registry, exactly as [`features`](crate::elaborate::features)
//! / [`regions`](crate::elaborate::regions) are pure derived views of the source.
//!
//! Identity (`part` + authored `params`) and display are kept apart: the reference
//! designator is a *different namespace* from the [`EntityId`] instance path — flat,
//! prefixed, and consumed by manufacturing-time humans — so it is a query, the classic
//! annotation pass. A single [`registry`] table keys every convention (`prefix`,
//! label `template`, class-default params) by class.
//!
//! # Dependency tracking
//!
//! These are **pure free functions** over `&Source` / `&Doc` / `&PartLib`, matching the
//! `features`/`regions`/`stackup` idiom rather than the memoized [`query::Engine`]
//! (`Netlist`/`Erc`/…). They are cheap and deterministic; the salsa-style memo tier is
//! reserved for the heavier netlist→DRC chain whose `InputId` revisions it already
//! tracks. A source edit re-derives them from scratch.

use crate::doc::{Component, Doc};
use crate::elaborate::GenDirective;
use crate::id::EntityId;
use crate::part::{PartDef, PartLib};
use crate::quantity;
use std::collections::{BTreeMap, BTreeSet};

/// One class-registry entry — the conventions attached to a component class. All fields
/// are optional: a `prefix`/`template` of `None` falls through to the query-level
/// default (the class name / `"{value}"`), and `defaults` may be empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassEntry {
    /// Reference-designator prefix. `None` ⇒ the class name itself (see [`refdes`]).
    pub prefix: Option<String>,
    /// Label template (see [`render_template`]). `None` ⇒ the built-in `"{value}"`.
    pub template: Option<String>,
    /// Class-default parameters, overlaid by instance params (see [`effective_params`]).
    pub defaults: BTreeMap<String, String>,
}

/// The class registry: a `class → ClassEntry` table. Built-in seeds (`R`, `C`, `L`,
/// each `template = "{value}"`) merged **under** authored [`Class`](GenDirective::Class)
/// directives — an authored entry replaces its seed wholesale (last authored wins).
pub fn registry(source: &[GenDirective]) -> BTreeMap<String, ClassEntry> {
    let mut reg = seed_registry();
    for d in source {
        if let GenDirective::Class { name, entry } = d {
            reg.insert(name.clone(), entry.clone());
        }
    }
    reg
}

/// The built-in seed entries. `prefix` is intentionally `None` (the class name is the
/// prefix by the query-level rule, so `R`'s prefix is `R` without stating it here).
fn seed_registry() -> BTreeMap<String, ClassEntry> {
    let seed = |t: &str| ClassEntry {
        prefix: None,
        template: Some(t.to_string()),
        defaults: BTreeMap::new(),
    };
    [("R", "{value}"), ("C", "{value}"), ("L", "{value}")]
        .into_iter()
        .map(|(c, t)| (c.to_string(), seed(t)))
        .collect()
}

/// The class of a part (Decision 14): the explicit [`PartDef::class`] override, else the
/// leading alphabetic run of the part name (`R_0402` → `R`, `LED_0603` → `LED`), else
/// `U` (a part name starting with a digit or symbol).
pub fn class_of(def: &PartDef) -> String {
    if let Some(c) = &def.class {
        return c.clone();
    }
    class_from_name(&def.name)
}

fn class_from_name(name: &str) -> String {
    let run: String = name
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if run.is_empty() { "U".to_string() } else { run }
}

/// The derived **reference-designator** map `EntityId → refdes`. Deterministic:
/// components are visited in path order (their [`BTreeMap`] order) and numbered per
/// prefix from 1, formatted `{prefix}{n}`. The prefix is the class's registry `prefix`,
/// or — for any class with none, registered or not — the class name itself.
///
/// **Pins win.** [`Doc::refdes_pins`](crate::doc::Doc::refdes_pins) (Decision 14's
/// stability mechanism) is consulted first: a pinned entity takes its string verbatim,
/// opaque — no validation against the derived class prefix (the user's prerogative). A
/// pinned entity consumes no auto number. To keep an auto-assigned refdes from ever
/// colliding with a pin, any pinned string of the shape `<alpha><digits>` (e.g. `C7`)
/// **reserves** that number under that prefix, and the auto counter skips it — so the
/// visited C-parts here would number `C1..C6, C8` around a pinned `C7`. A pin that does
/// not parse that way (e.g. `SPARE`) reserves nothing numeric.
///
/// A registry `prefix` may itself end in a digit (`class C prefix=X9`), in which case
/// the leading-alpha parse of a pin like `X91` would key the wrong prefix; so the
/// reservation *also* tests each pin against every registry-declared prefix and
/// reserves under that key too. The invariant holds regardless: an auto refdes never
/// equals a pin. Reservation matching is **case-sensitive** — a pin `r7` reserves
/// nothing against the auto `R7` prefix (pins are opaque per Decision 14, so `r7` and
/// `R7` are simply different strings).
///
/// The *auto* numbering is **insertion-unstable** by accepted trade-off (adding a
/// component renumbers its successors); pins are the stability escape hatch.
pub fn refdes(
    doc: &Doc,
    lib: &PartLib,
    reg: &BTreeMap<String, ClassEntry>,
) -> BTreeMap<EntityId, String> {
    // Reserve every pin's number so the auto counter can skip it. Two keys per pin:
    // its own leading-alpha prefix (`C7` → C), and — for a registry prefix that may end
    // in a digit (`X9`) — that prefix directly (`X91` → X9), which the leading-alpha
    // parse would otherwise miskey as `X`. The pin is opaque; reservation is case-
    // sensitive string matching.
    let mut reserved: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for s in doc.refdes_pins.values() {
        if let Some((prefix, n)) = parse_refdes(s) {
            reserved.entry(prefix).or_default().insert(n);
        }
        for (class, e) in reg {
            let p = e.prefix.as_deref().unwrap_or(class);
            if let Some(n) = digits_after_prefix(s, p) {
                reserved.entry(p.to_string()).or_default().insert(n);
            }
        }
    }

    let mut counters: BTreeMap<String, u32> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (id, comp) in &doc.components {
        if let Some(pinned) = doc.refdes_pins.get(id) {
            out.insert(id.clone(), pinned.clone());
            continue; // verbatim; consumes no auto number
        }
        let class = match lib.get(&comp.part) {
            Some(def) => class_of(def),
            None => class_from_name(&comp.part),
        };
        let prefix = reg
            .get(&class)
            .and_then(|e| e.prefix.clone())
            .unwrap_or(class);
        let n = counters.entry(prefix.clone()).or_insert(0);
        let taken = reserved.get(&prefix);
        loop {
            *n += 1;
            if !taken.is_some_and(|t| t.contains(n)) {
                break;
            }
        }
        out.insert(id.clone(), format!("{prefix}{n}"));
    }
    out
}

/// Parse a refdes string as `<alpha-prefix><digits>` (the conventional shape), e.g.
/// `C7` → `("C", 7)`. Returns `None` unless the whole string is a non-empty leading
/// alphabetic run followed by a non-empty run of ASCII digits and nothing else — so
/// `SPARE`, `C7A`, and `7` all reserve nothing numeric.
fn parse_refdes(s: &str) -> Option<(String, u32)> {
    let split = s.find(|c: char| !c.is_ascii_alphabetic())?;
    if split == 0 {
        return None; // no alpha prefix
    }
    let (prefix, digits) = s.split_at(split);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = digits.parse().ok()?;
    Some((prefix.to_string(), n))
}

/// If `s` is `prefix` followed by a non-empty run of ASCII digits and nothing else,
/// return the number — used to reserve a pin against a *known* (possibly digit-suffixed)
/// registry prefix, where the leading-alpha parse of [`parse_refdes`] would miskey.
fn digits_after_prefix(s: &str, prefix: &str) -> Option<u32> {
    let rest = s.strip_prefix(prefix)?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// The set of colliding refdes pins: each group is the `(string, entities)` for one
/// string pinned on more than one entity — a genuine authoring conflict (two parts
/// cannot share `C7`). Entities within a group are in `EntityId` (path) order; groups
/// are in string order. Empty when every pin is unique. Consumed by
/// [`elaborate`](crate::elaborate::elaborate) into the [`ReconReport`](crate::doc::ReconReport).
pub fn duplicate_refdes_pins(pins: &BTreeMap<EntityId, String>) -> Vec<(String, Vec<EntityId>)> {
    let mut by_string: BTreeMap<&str, Vec<EntityId>> = BTreeMap::new();
    for (id, s) in pins {
        by_string.entry(s.as_str()).or_default().push(id.clone());
    }
    by_string
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(s, ids)| (s.to_string(), ids))
        .collect()
}

/// The effective identity parameters of a component: the class `defaults` overlaid by
/// the instance's own `params` (instance wins). This is the parameter set BOM identity
/// and label rendering both read.
pub fn effective_params(
    comp: &Component,
    def: &PartDef,
    reg: &BTreeMap<String, ClassEntry>,
) -> BTreeMap<String, String> {
    let class = class_of(def);
    let mut out = reg
        .get(&class)
        .map(|e| e.defaults.clone())
        .unwrap_or_default();
    for (k, v) in &comp.params {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// The rendered display **label** of a component. Template cascade: the instance
/// `label` (itself a template) → the class registry `template` → the built-in
/// `"{value}"`. If the rendered result is empty/whitespace (an IC with no params, say),
/// fall back to the part name — one rule covering passives and ICs alike.
pub fn label(comp: &Component, def: &PartDef, reg: &BTreeMap<String, ClassEntry>) -> String {
    let class = class_of(def);
    let params = effective_params(comp, def, reg);
    let template = comp
        .label
        .clone()
        .or_else(|| reg.get(&class).and_then(|e| e.template.clone()))
        .unwrap_or_else(|| "{value}".to_string());
    let rendered = render_template(&template, &params);
    if rendered.trim().is_empty() {
        def.name.clone()
    } else {
        rendered
    }
}

/// Render a label template against effective params. Field syntax:
///
///   - `{key}` — the param value, verbatim (authored spelling wins); a missing key is
///     the empty string.
///   - `{key:si:UNIT}` — parse the value and render SI engineering notation with `UNIT`
///     (`2600`/`2.6k` → `2.6kΩ` for `UNIT = Ω`).
///   - `{key:iec}` — IEC 60062 letter-as-decimal-point (`4700` → `4k7`).
///
/// Any parse failure or unknown format spec degrades to the **raw value verbatim**,
/// never an error. Text outside braces is copied through; an unterminated `{` is copied
/// literally.
pub fn render_template(template: &str, params: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            // Unterminated field: emit the rest literally and stop.
            out.push('{');
            out.push_str(after);
            return out;
        };
        let field = &after[..close];
        out.push_str(&render_field(field, params));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

fn render_field(field: &str, params: &BTreeMap<String, String>) -> String {
    let mut parts = field.splitn(3, ':');
    let key = parts.next().unwrap_or("");
    let raw = params.get(key).map(String::as_str).unwrap_or("");
    match parts.next() {
        None => raw.to_string(),
        Some("si") => {
            let unit = parts.next().unwrap_or("");
            match quantity::parse(raw) {
                Some(q) => q.format_si(unit),
                None => raw.to_string(),
            }
        }
        Some("iec") => match quantity::parse(raw) {
            Some(q) => q.format_iec(),
            None => raw.to_string(),
        },
        // Unknown format spec → raw value verbatim.
        Some(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Orient;
    use crate::doc::{Dof, Point, Provenance};

    fn pd(name: &str, class: Option<&str>) -> PartDef {
        PartDef {
            name: name.to_string(),
            pins: vec![],
            interfaces: BTreeMap::new(),
            graphics: vec![],
            texts: vec![],
            courtyard: None,
            class: class.map(String::from),
        }
    }

    fn comp(id: &str, part: &str, params: &[(&str, &str)], label: Option<&str>) -> Component {
        Component {
            id: EntityId::new(id),
            part: part.to_string(),
            pos: Dof {
                value: Point { x: 0, y: 0 },
                prov: Provenance::Free,
            },
            orient: Orient::default(),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            label: label.map(String::from),
        }
    }

    fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ---- registry / seeds / merge ----

    #[test]
    fn seeds_present_and_authored_replaces_wholesale() {
        let src = vec![GenDirective::Class {
            name: "R".to_string(),
            entry: ClassEntry {
                prefix: Some("RES".to_string()),
                template: Some("{value} {tol}".to_string()),
                defaults: BTreeMap::new(),
            },
        }];
        let reg = registry(&src);
        // Seeds C and L survive; R is replaced wholesale by the authored entry.
        assert_eq!(reg["C"].template.as_deref(), Some("{value}"));
        assert_eq!(reg["L"].template.as_deref(), Some("{value}"));
        assert_eq!(reg["R"].prefix.as_deref(), Some("RES"));
        assert_eq!(reg["R"].template.as_deref(), Some("{value} {tol}"));
    }

    #[test]
    fn authored_class_with_no_template_kills_seed_template_wholesale() {
        // `class R` authored with `template: None` must REPLACE the R seed wholesale, not
        // field-merge — so the seed's `template = "{value}"` is gone (`None`), leaving
        // only the query-level `"{value}"` fallback. The label is behaviourally unchanged
        // *only* because the R seed template happens to equal that fallback; the
        // wholesale-replace semantics are observable here at the registry map. C's seed
        // is untouched.
        let src = vec![GenDirective::Class {
            name: "R".to_string(),
            entry: ClassEntry {
                prefix: None,
                template: None,
                defaults: params(&[("tol", "1%")]),
            },
        }];
        let reg = registry(&src);
        assert_eq!(
            reg["R"].template, None,
            "seed template replaced wholesale, not merged"
        );
        assert_eq!(reg["R"].defaults["tol"], "1%");
        assert_eq!(
            reg["C"].template.as_deref(),
            Some("{value}"),
            "C seed untouched"
        );
    }

    // ---- class heuristic ----

    #[test]
    fn class_override_then_name_run_then_u() {
        assert_eq!(class_of(&pd("R_0402", None)), "R");
        assert_eq!(class_of(&pd("LED_0603", None)), "LED");
        assert_eq!(class_of(&pd("74HC00", None)), "U"); // starts with a digit
        assert_eq!(class_of(&pd("R_0402", Some("C"))), "C"); // explicit override wins
    }

    // ---- refdes determinism / per-class counters / heuristic fallback ----

    #[test]
    fn refdes_per_class_counters_in_path_order() {
        let mut doc = Doc::default();
        for (id, part) in [
            ("r1", "R_0402"),
            ("c1", "C_0603"),
            ("r2", "R_0402"),
            ("x1", "74HC00"), // starts with a digit → class U
            ("led", "LED_0603"),
        ] {
            doc.components
                .insert(EntityId::new(id), comp(id, part, &[], None));
        }
        let mut lib = PartLib::new();
        for p in ["R_0402", "C_0603", "74HC00", "LED_0603"] {
            lib.insert(p.to_string(), pd(p, None));
        }
        let rd = refdes(&doc, &lib, &registry(&[]));
        // BTreeMap path order is c1, led, r1, r2, x1 → counters advance per prefix.
        assert_eq!(rd[&EntityId::new("r1")], "R1");
        assert_eq!(rd[&EntityId::new("r2")], "R2");
        assert_eq!(rd[&EntityId::new("c1")], "C1");
        assert_eq!(rd[&EntityId::new("led")], "LED1"); // unregistered class → name is prefix
        assert_eq!(rd[&EntityId::new("x1")], "U1"); // digit-leading name → U fallback
    }

    // ---- refdes pins (Decision 14 stability override) ----

    /// A pinned entity takes its string verbatim, consumes no auto number, and the
    /// pinned number is reserved so the auto counter skips it: pinning one of eight
    /// C-parts to `C7` yields `C1..C6, C8` across the other seven.
    #[test]
    fn refdes_pin_respected_and_reserves_its_number() {
        let mut doc = Doc::default();
        for i in 0..8 {
            let id = format!("c{i}");
            doc.components
                .insert(EntityId::new(&id), comp(&id, "C_0603", &[], None));
        }
        doc.refdes_pins.insert(EntityId::new("c3"), "C7".into());
        let mut lib = PartLib::new();
        lib.insert("C_0603".to_string(), pd("C_0603", None));

        let rd = refdes(&doc, &lib, &registry(&[]));
        assert_eq!(rd[&EntityId::new("c3")], "C7", "pinned entity verbatim");
        // Auto assignment in path order c0,c1,c2,(c3 pinned),c4,c5,c6,c7 → 7 gets skipped.
        assert_eq!(rd[&EntityId::new("c0")], "C1");
        assert_eq!(rd[&EntityId::new("c2")], "C3");
        assert_eq!(rd[&EntityId::new("c4")], "C4");
        assert_eq!(rd[&EntityId::new("c6")], "C6");
        assert_eq!(
            rd[&EntityId::new("c7")],
            "C8",
            "C7 reserved by the pin, skipped"
        );
    }

    /// A pin whose string is opaque to the number parser (`SPARE`) reserves nothing:
    /// the auto counter proceeds C1, C2, C3 uninterrupted.
    #[test]
    fn non_numeric_pin_reserves_nothing() {
        let mut doc = Doc::default();
        for i in 0..4 {
            let id = format!("c{i}");
            doc.components
                .insert(EntityId::new(&id), comp(&id, "C_0603", &[], None));
        }
        doc.refdes_pins.insert(EntityId::new("c0"), "SPARE".into());
        let mut lib = PartLib::new();
        lib.insert("C_0603".to_string(), pd("C_0603", None));

        let rd = refdes(&doc, &lib, &registry(&[]));
        assert_eq!(rd[&EntityId::new("c0")], "SPARE");
        assert_eq!(rd[&EntityId::new("c1")], "C1");
        assert_eq!(rd[&EntityId::new("c2")], "C2");
        assert_eq!(rd[&EntityId::new("c3")], "C3");
    }

    /// The reservation keys off the *pinned string's* prefix, not the entity's class:
    /// pinning a resistor to `C7` reserves `C7` (skipping it among C-parts) but leaves
    /// the R counter untouched — the pin is opaque.
    #[test]
    fn pin_reserves_under_its_own_prefix_not_the_entitys_class() {
        let mut doc = Doc::default();
        doc.components
            .insert(EntityId::new("r0"), comp("r0", "R_0402", &[], None));
        for i in 0..8 {
            let id = format!("c{i}");
            doc.components
                .insert(EntityId::new(&id), comp(&id, "C_0603", &[], None));
        }
        // Pin the resistor to a C-shaped string.
        doc.refdes_pins.insert(EntityId::new("r0"), "C7".into());
        let mut lib = PartLib::new();
        lib.insert("R_0402".to_string(), pd("R_0402", None));
        lib.insert("C_0603".to_string(), pd("C_0603", None));

        let rd = refdes(&doc, &lib, &registry(&[]));
        assert_eq!(rd[&EntityId::new("r0")], "C7");
        // Eight auto C-parts (c0..c7) with 7 reserved: C1..C6, C8, C9 — the skip lands
        // between c5 and c6. No R is auto-assigned (the sole R was pinned to a C string),
        // so no R numbering is disturbed.
        assert_eq!(rd[&EntityId::new("c5")], "C6");
        assert_eq!(rd[&EntityId::new("c6")], "C8");
        assert_eq!(rd[&EntityId::new("c7")], "C9");
    }

    /// The reviewer's scenario: a registry prefix ending in a digit (`class C
    /// prefix=X9`) plus a pin `X91`. The auto counter for prefix `X9` must NOT also
    /// emit `X91` — the pin reserves `1` under `X9` even though its leading-alpha run
    /// is `X`, so the auto parts skip to `X92`.
    #[test]
    fn digit_suffixed_registry_prefix_reservation() {
        let src = vec![GenDirective::Class {
            name: "C".to_string(),
            entry: ClassEntry {
                prefix: Some("X9".to_string()),
                template: None,
                defaults: BTreeMap::new(),
            },
        }];
        let mut doc = Doc::default();
        // c0 pinned to X91 (verbatim); c1, c2 auto under the X9 prefix.
        for i in 0..3 {
            let id = format!("c{i}");
            doc.components
                .insert(EntityId::new(&id), comp(&id, "C_0603", &[], None));
        }
        doc.refdes_pins.insert(EntityId::new("c0"), "X91".into());
        let mut lib = PartLib::new();
        lib.insert("C_0603".to_string(), pd("C_0603", Some("C")));

        let rd = refdes(&doc, &lib, &registry(&src));
        assert_eq!(rd[&EntityId::new("c0")], "X91");
        // Without the digit-suffixed-prefix reservation, c1 would collide at X91.
        assert_eq!(rd[&EntityId::new("c1")], "X92");
        assert_eq!(rd[&EntityId::new("c2")], "X93");
    }

    #[test]
    fn parse_refdes_shape() {
        assert_eq!(parse_refdes("C7"), Some(("C".to_string(), 7)));
        assert_eq!(parse_refdes("LED12"), Some(("LED".to_string(), 12)));
        assert_eq!(parse_refdes("SPARE"), None); // no digits
        assert_eq!(parse_refdes("C7A"), None); // trailing non-digit
        assert_eq!(parse_refdes("7"), None); // no alpha prefix
        assert_eq!(parse_refdes("C"), None); // no digits
    }

    /// Duplicate detection groups entities by identical pinned string, in path order.
    #[test]
    fn duplicate_refdes_pins_groups_collisions() {
        let mut pins: BTreeMap<EntityId, String> = BTreeMap::new();
        pins.insert(EntityId::new("a"), "C7".into());
        pins.insert(EntityId::new("b"), "C7".into());
        pins.insert(EntityId::new("c"), "R1".into()); // unique, not reported
        let dups = duplicate_refdes_pins(&pins);
        assert_eq!(
            dups,
            vec![(
                "C7".to_string(),
                vec![EntityId::new("a"), EntityId::new("b")]
            )]
        );
    }

    #[test]
    fn refdes_prefix_from_registry_override() {
        let mut doc = Doc::default();
        doc.components
            .insert(EntityId::new("r1"), comp("r1", "R_0402", &[], None));
        let mut lib = PartLib::new();
        lib.insert("R_0402".to_string(), pd("R_0402", None));
        let src = vec![GenDirective::Class {
            name: "R".to_string(),
            entry: ClassEntry {
                prefix: Some("RES".to_string()),
                template: None,
                defaults: BTreeMap::new(),
            },
        }];
        let rd = refdes(&doc, &lib, &registry(&src));
        assert_eq!(rd[&EntityId::new("r1")], "RES1");
    }

    // ---- effective params merge (seed / authored / instance) ----

    #[test]
    fn effective_params_layers_defaults_under_instance() {
        let reg = registry(&[GenDirective::Class {
            name: "R".to_string(),
            entry: ClassEntry {
                prefix: None,
                template: Some("{value}".to_string()),
                defaults: params(&[("tol", "5%"), ("value", "1k")]),
            },
        }]);
        let c = comp("r1", "R_0402", &[("value", "4.7k")], None);
        let eff = effective_params(&c, &pd("R_0402", None), &reg);
        assert_eq!(eff["value"], "4.7k"); // instance overrides default
        assert_eq!(eff["tol"], "5%"); // default retained
    }

    // ---- template rendering ----

    #[test]
    fn template_verbatim_and_missing_key() {
        let p = params(&[("value", "4.7k")]);
        assert_eq!(render_template("{value}", &p), "4.7k");
        assert_eq!(render_template("R={value} T={tol}", &p), "R=4.7k T="); // missing → empty
        assert_eq!(render_template("no fields", &p), "no fields");
    }

    #[test]
    fn template_si_and_iec_specs() {
        let p = params(&[("value", "2600"), ("c", "4700")]);
        assert_eq!(render_template("{value:si:Ω}", &p), "2.6kΩ");
        assert_eq!(render_template("{c:iec}", &p), "4k7");
    }

    #[test]
    fn template_parse_failure_and_unknown_spec_are_verbatim() {
        let p = params(&[("value", "abc")]);
        assert_eq!(render_template("{value:si:Ω}", &p), "abc"); // parse fail → raw
        assert_eq!(render_template("{value:iec}", &p), "abc");
        assert_eq!(render_template("{value:bogus}", &p), "abc"); // unknown spec → raw
    }

    #[test]
    fn template_unterminated_brace_is_literal() {
        let p = params(&[]);
        assert_eq!(render_template("a {value", &p), "a {value");
    }

    // ---- label cascade ----

    #[test]
    fn label_cascade_instance_registry_then_partname_fallback() {
        let reg = registry(&[GenDirective::Class {
            name: "R".to_string(),
            entry: ClassEntry {
                prefix: None,
                template: Some("{value:si:Ω}".to_string()),
                defaults: BTreeMap::new(),
            },
        }]);
        // Instance label (a template itself) wins.
        let c = comp("r1", "R_0402", &[("value", "4700")], Some("{value:iec}"));
        assert_eq!(label(&c, &pd("R_0402", None), &reg), "4k7");
        // No instance label → registry template.
        let c2 = comp("r2", "R_0402", &[("value", "2600")], None);
        assert_eq!(label(&c2, &pd("R_0402", None), &reg), "2.6kΩ");
        // An IC with no params and no table entry → part name fallback.
        let c3 = comp("u1", "MCU", &[], None);
        assert_eq!(label(&c3, &pd("MCU", None), &reg), "MCU");
    }
}
