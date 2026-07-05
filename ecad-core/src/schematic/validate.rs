//! Elaboration / validation (tier 1 diagnostics): [`validate`] for the `sym` tree and
//! [`validate_wires`] for the drawn wires, kept apart because wires need the part
//! library and the netlist.

use crate::diagnostic::{Diagnostic, Location};
use crate::id::EntityId;
use crate::part::PartLib;
use crate::schematic::reflow::{MAX_FRAGMENT_DEPTH, fragment_depth};
use crate::schematic::{LayoutNode, SchematicLayout, WireEnd};
use std::collections::{BTreeMap, BTreeSet};

/// Validate an authored layout against the elaborated component universe. Two kinds of
/// finding, split like the rest of the codebase splits them (a fault aborts the commit;
/// a finding rides on a valid doc — see `diagnostic.rs`):
///
///   - **Hard `E_SCHEMATIC` errors** (returned): a `sym` whose comp path the *source*
///     never declares (a typo — unknown path), the same comp path placed by two `sym`
///     leaves (duplicate placement), and two sibling containers sharing a `name`
///     (duplicate sibling name — breaks GUI addressing). Collect-all: every offending
///     node is reported in one pass.
///
///   - **A `W_SCHEMATIC_UNPLACED` warning** (returned separately, for the caller to hang
///     on the [`ReconReport`](crate::doc::ReconReport) — the `W_FONT_LOAD` idiom): every
///     component *not* named by any `sym`, plus every `sym` whose path the source declared
///     but a false `if=` depopulated (Decision 21b DNP). Non-blocking; the view stays
///     total (§20c). Not an error, so it does **not** gate `is_clean`.
///
/// The **DNP distinction** (Decision 20c × 21b): a `sym` path in `dnp_dropped` is a
/// component the source *did* declare but a population conditional turned off — toggling
/// a variant must not hard-abort a commit, so it degrades to the unplaced bin (a warning)
/// exactly like a never-placed part, not an `E_SCHEMATIC`. Only a path the source does not
/// know at all is the typo case that aborts.
///
/// `component_ids` is the elaborated (populated) instance universe (keys of
/// `Doc::components`); `dnp_dropped` is the depopulated-path set from
/// [`crate::elaborate::Elaborated`]; `def_fragments` is the per-instance stamped layout
/// table (Decision 20 embedded in a def, keyed by def-instance path) — a `sym` whose path
/// is a key is **legal** (it expands into the fragment at reflow), NOT an unknown-typo error
/// and NOT a placed component. Its values (the fragments, holding the internal paths each
/// stamps) drive the override-warning channel.
///
/// Returns three channels: the hard errors (empty ⇒ clean), the sorted list of unplaced ids
/// (never-placed populated parts + DNP-dropped placed paths), and a set of NON-BLOCKING
/// `W_SCHEMATIC` override warnings — one per internal path that both the stamped fragment
/// AND a doc-level `sym` place, where the doc-level placement wins (the reflow drops the
/// fragment's copy, so this is never a duplicate error, just a visible "your doc-level sym
/// overrides the fragment" signal).
pub fn validate(
    layout: &SchematicLayout,
    component_ids: &BTreeSet<EntityId>,
    dnp_dropped: &BTreeSet<String>,
    def_fragments: &BTreeMap<String, SchematicLayout>,
) -> (Vec<Diagnostic>, Vec<EntityId>, Vec<Diagnostic>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Duplicate sibling container names, walked over the whole tree (siblings = the
    // children of one container, and the root list). Reported once per collision.
    fn check_names(nodes: &[LayoutNode], errors: &mut Vec<Diagnostic>) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for n in nodes {
            if let LayoutNode::Container(c) = n {
                if let Some(name) = &c.name
                    && !seen.insert(name.as_str())
                {
                    errors.push(Diagnostic::error(
                        "E_SCHEMATIC",
                        format!("duplicate sibling container name `{name}`"),
                        Location::None,
                    ));
                }
                check_names(&c.children, errors);
            }
        }
    }
    check_names(&layout.roots, &mut errors);

    // Symbol paths, in pre-order. Five cases:
    //   - a def-instance path (a `def_fragments` key, Decision 20 embedded in a def): LEGAL
    //     — it is a group that expands into the stamped fragment at reflow, not a placed
    //     component and not a typo. Skip it entirely (it reserves no placement here).
    //   - populated (in `component_ids`): a real placement; duplicate placement is an error.
    //   - DNP-dropped (in `dnp_dropped`): the source declared it but `if=false` turned it
    //     off — NOT an error; collect it as unplaced so it warns and the view degrades.
    //   - unknown to the source entirely: a typo — hard `E_SCHEMATIC` abort.
    let mut placed: BTreeSet<&str> = BTreeSet::new();
    let mut dnp_placed: BTreeSet<EntityId> = BTreeSet::new();
    for path in layout.symbol_paths() {
        if def_fragments.contains_key(path) {
            // A def-instance sym: legal, expands at reflow. Not a component placement.
            continue;
        } else if component_ids.contains(&EntityId::new(path)) {
            if !placed.insert(path) {
                errors.push(Diagnostic::error(
                    "E_SCHEMATIC",
                    format!("component `{path}` is placed by more than one `sym`"),
                    Location::Entity(EntityId::new(path)),
                ));
            }
        } else if dnp_dropped.contains(path) {
            // Depopulated variant: degrade to unplaced, do not abort (§20c × 21b).
            dnp_placed.insert(EntityId::new(path));
        } else {
            errors.push(Diagnostic::error(
                "E_SCHEMATIC",
                format!("`sym {path}` names no component instance"),
                Location::Entity(EntityId::new(path)),
            ));
        }
    }

    // Ignored-attribute warnings (F7): a def-instance `sym` expands to a GROUP, not a leaf
    // box, so an authored `rot`/`dx`/`dy` on it has no v1 meaning and `reflow` silently drops
    // it (see `expand_def_syms`). Surface that as a non-blocking `W_SCHEMATIC` so the drop is
    // never invisible — the author asked for a transform the group model can't honour yet.
    for s in layout.symbols() {
        if def_fragments.contains_key(&s.path)
            && (s.rot != crate::doc::Orient::IDENTITY || s.dx != 0 || s.dy != 0)
        {
            warnings.push(Diagnostic::warning(
                "W_SCHEMATIC",
                format!(
                    "`rot`/`dx`/`dy` on def-instance `sym {}` is ignored (a def instance is a \
                     group, not a symbol box; group-level transforms are a follow-up)",
                    s.path
                ),
                Location::Entity(EntityId::new(s.path.as_str())),
            ));
        }
    }

    // Fragment-nesting depth (guards the same cap `reflow`'s `expand_def_syms` enforces).
    // A def-instance `sym` whose fragment nests other def instances beyond
    // [`MAX_FRAGMENT_DEPTH`] cannot fully render — `reflow` stops expanding at the cap and
    // the over-deep subtree vanishes. A silent drop is against the house rules, so surface
    // it here as a hard `E_SCHEMATIC` (not a warning): the schematic would be genuinely
    // incomplete, exactly the fault class [`crate::elaborate::MAX_DEF_DEPTH`] treats as an
    // error. Practically unreachable (instance paths are distinct and acyclic), but the
    // check makes the cap honest rather than a quiet truncation.
    for path in layout.symbol_paths() {
        if def_fragments.contains_key(path)
            && fragment_depth(path, def_fragments, 0) > MAX_FRAGMENT_DEPTH
        {
            errors.push(Diagnostic::error(
                "E_SCHEMATIC",
                format!(
                    "def-instance `sym {path}` nests fragments beyond the depth cap \
                     ({MAX_FRAGMENT_DEPTH}) — the over-deep subtree would not render"
                ),
                Location::Entity(EntityId::new(path)),
            ));
        }
    }

    // Override warnings (§20, doc-wins precedence): a doc-level `sym <inst.internal>` placed
    // explicitly in the tree overrides the stamped fragment's placement of that same path.
    // The reflow drops the fragment's copy (never a double-placement, so never a duplicate
    // error), but the collision is worth surfacing so the author knows the fragment default
    // was superseded. `placed` holds exactly the doc-level explicit component paths (a
    // def-instance sym was skipped above); a fragment that stamps one of them earns a
    // non-blocking `W_SCHEMATIC` warning. Deterministic order: fragments are a `BTreeMap`
    // and each fragment's paths walk in pre-order.
    for (inst, frag) in def_fragments {
        for fp in frag.symbol_paths() {
            if placed.contains(fp) {
                warnings.push(Diagnostic::warning(
                    "W_SCHEMATIC",
                    format!(
                        "doc-level `sym {fp}` overrides the stamped fragment placement from def \
                         instance `{inst}`"
                    ),
                    Location::Entity(EntityId::new(fp)),
                ));
            }
        }
    }

    // Unplaced (a warning, not an error), deterministic id order: every populated component
    // the tree never names, plus every DNP-dropped path a `sym` did name (so a placed but
    // depopulated part is still visibly accounted for). The union is a `BTreeSet` so the
    // result is sorted and dedup'd.
    //
    // A component placed by a def instance's stamped fragment (Decision 20 embedded in a
    // def) is genuinely placed — the reflow expands the def-instance sym into that
    // fragment's group — even though its `sym` never appears in the doc-level tree. So the
    // fragments' internal paths join the placed set; otherwise every stamped internal would
    // spuriously warn as unplaced despite rendering in its group.
    let mut placed_ids: BTreeSet<EntityId> = placed.iter().map(|p| EntityId::new(*p)).collect();
    for frag in def_fragments.values() {
        for fp in frag.symbol_paths() {
            placed_ids.insert(EntityId::new(fp));
        }
    }
    let mut unplaced: BTreeSet<EntityId> = component_ids
        .iter()
        .filter(|id| !placed_ids.contains(id))
        .cloned()
        .collect();
    unplaced.extend(dnp_placed);

    (errors, unplaced.into_iter().collect(), warnings)
}

/// Validate the drawn wires (§20d) against the elaborated universe — a sibling of
/// [`validate`], kept separate because a wire needs the *part library* (to resolve pin
/// identities) and the *netlist* (to spot a wire drawn across two nets), which `validate`
/// does not. Wires are presentational, so their findings mirror the `sym` gate but never
/// touch the flow:
///
///   - **Hard `E_SCHEMATIC` errors** (returned first): an endpoint whose component path is
///     unknown to the source (a typo, exactly like an unknown `sym` path), or whose pin
///     selector names no pin on that component's part (a typo'd pin). Collect-all.
///   - **`W_SCHEMATIC_WIRE` warnings** (returned second): a wire endpoint on a
///     DNP-dropped component (the wire degrades like a `sym` — non-blocking, §20c × 21b),
///     and a wire whose two endpoints resolve onto *different* nets (a legal but honest
///     "your drawing disagrees with the netlist" signal — the net tag at each pin still
///     tells the truth). Both leave the doc clean.
///
/// `components` is the populated path→part universe; `lib` sizes/enumerates pins;
/// `dnp_dropped` is the depopulated-path set; `pin_net` maps a resolved
/// [`PinRef`](crate::doc::PinRef) to its net name (absent ⇒ the pin joins no net). The
/// wire order is the pre-order [`SchematicLayout::wires`] walk, so output is deterministic.
pub fn validate_wires(
    layout: &SchematicLayout,
    components: &BTreeMap<EntityId, String>,
    lib: &PartLib,
    dnp_dropped: &BTreeSet<String>,
    pin_net: &impl Fn(&crate::doc::PinRef) -> Option<String>,
) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Resolve one wire endpoint to the set of stored pin identities it names, emitting the
    // right diagnostic on the way. Returns `None` when the endpoint should be skipped for
    // the cross-net check (unknown comp/pin already errored, or a DNP-dropped comp warned).
    let resolve_end = |end: &WireEnd,
                       errors: &mut Vec<Diagnostic>,
                       warnings: &mut Vec<Diagnostic>|
     -> Option<Vec<crate::doc::PinRef>> {
        let cid = EntityId::new(end.comp.clone());
        let Some(part) = components.get(&cid) else {
            // Not a populated component: a DNP-dropped path degrades (like a sym); an
            // otherwise-unknown path is a hard typo.
            if dnp_dropped.contains(end.comp.as_str()) {
                warnings.push(
                    Diagnostic::warning(
                        "W_SCHEMATIC_WIRE",
                        format!(
                            "wire endpoint `{}.{}` is on `{}`, which an `if=` variant depopulated; the wire is not drawn",
                            end.comp, end.pin, end.comp
                        ),
                        Location::Entity(cid),
                    ),
                );
            } else {
                errors.push(Diagnostic::error(
                    "E_SCHEMATIC",
                    format!(
                        "wire endpoint `{}.{}` names no component instance",
                        end.comp, end.pin
                    ),
                    Location::Entity(cid),
                ));
            }
            return None;
        };
        let Some(def) = lib.get(part) else {
            // A populated component whose part is missing from the lib: the sym path already
            // renders as a min box; a wire on it can't resolve a pin, so skip it silently
            // (the missing part is its own upstream concern, not a wire error).
            return None;
        };
        let ids = def.resolve_selector(&end.pin);
        if ids.is_empty() {
            errors.push(Diagnostic::error(
                "E_SCHEMATIC",
                format!(
                    "wire endpoint `{}.{}` names no pin on part `{part}`",
                    end.comp, end.pin
                ),
                Location::Entity(cid),
            ));
            return None;
        }
        Some(
            ids.iter()
                .map(|id| crate::doc::PinRef::new(&cid, id))
                .collect(),
        )
    };

    for w in layout.wires() {
        let a = resolve_end(&w.a, &mut errors, &mut warnings);
        let b = resolve_end(&w.b, &mut errors, &mut warnings);
        // Cross-net check only when both endpoints resolved to real pins. Two endpoints
        // "agree" if they share any net (a multi-pad selector fans out; sharing one net is
        // enough to call the drawing honest). A wire where neither side joins any net is
        // silent — there is nothing to disagree with.
        if let (Some(a), Some(b)) = (a, b) {
            let nets_a: BTreeSet<String> = a.iter().filter_map(pin_net).collect();
            let nets_b: BTreeSet<String> = b.iter().filter_map(pin_net).collect();
            if !nets_a.is_empty() && !nets_b.is_empty() && nets_a.is_disjoint(&nets_b) {
                // Deterministic message: name the two nets in sorted order.
                let na = nets_a.iter().next().unwrap();
                let nb = nets_b.iter().next().unwrap();
                warnings.push(
                    Diagnostic::warning(
                        "W_SCHEMATIC_WIRE",
                        format!(
                            "wire `{}.{}` — `{}.{}` connects different nets (`{na}` vs `{nb}`); the drawn wire does not match the netlist",
                            w.a.comp, w.a.pin, w.b.comp, w.b.pin
                        ),
                        Location::None,
                    )
                    .with_help("wires are presentational; the net tag at each pin is the truth"),
                );
            }
        }
    }

    (errors, warnings)
}
