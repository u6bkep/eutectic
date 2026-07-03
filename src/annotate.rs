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
use std::collections::BTreeMap;

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
/// The numbering is **insertion-unstable** by accepted trade-off (adding a component
/// renumbers its successors); the reserved stability mechanism is an `EntityId`-keyed
/// override, deliberately not built here.
pub fn refdes(
    doc: &Doc,
    lib: &PartLib,
    reg: &BTreeMap<String, ClassEntry>,
) -> BTreeMap<EntityId, String> {
    let mut counters: BTreeMap<String, u32> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for (id, comp) in &doc.components {
        let class = match lib.get(&comp.part) {
            Some(def) => class_of(def),
            None => class_from_name(&comp.part),
        };
        let prefix = reg
            .get(&class)
            .and_then(|e| e.prefix.clone())
            .unwrap_or(class);
        let n = counters.entry(prefix.clone()).or_insert(0);
        *n += 1;
        out.insert(id.clone(), format!("{prefix}{n}"));
    }
    out
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
