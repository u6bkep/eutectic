//! Findings as data (structural commitment 5, `docs/gui-architecture.md`).
//!
//! A **finding** is one derived issue over the elaborated [`Doc`]: a DRC violation
//! (clearance / min-width / ratsnest / keep-out / edge), an ERC diagnostic
//! (multiple drivers), or a connectivity diagnostic (floating pad). Each carries a
//! severity, a human message, semantic refs (net / entity / pin / trace / via), and
//! — where the GUI can derive it — a board-mm location for a canvas halo.
//!
//! # What refs each engine surface actually carries (reported in `findings_pipeline`)
//!
//! `ecad-core` surfaces findings as **semantic** refs, never board mm:
//!
//! | Surface (query key)        | Diagnostic code(s)          | `Location` the engine attaches |
//! |----------------------------|-----------------------------|--------------------------------|
//! | DRC `Clearance`            | `E_DRC_CLEARANCE`           | `Net(a)` (the lower-sorted net) |
//! | DRC `MinWidth`             | `E_DRC_MIN_WIDTH`           | `Trace(id)`                    |
//! | DRC `Unrouted`             | `E_DRC_UNROUTED`            | `Net(n)`                       |
//! | DRC `Keepout`              | `E_DRC_KEEPOUT`             | `Net(n)`                       |
//! | DRC `EdgeClearance`        | `E_DRC_EDGE_CLEARANCE`      | `Net(n)`                       |
//! | ERC `Key::Erc`             | `E_MULTIPLE_DRIVERS`        | `Net(n)`                       |
//! | Connectivity `Key::Floating` | `E_FLOATING_PAD`          | `Pin(PinRef{comp,pad})`        |
//! | Elaboration (hard fault)   | `E_*` / `W_*`               | `Span{line,col}` / semantic    |
//!
//! Two consequences the deliverable must be honest about:
//!
//! 1. **No engine surface carries a board-mm point.** A clearance violation names
//!    the *net pair*, not the two colliding copper edges; a ratsnest violation names
//!    the net, not the gap. So a halo location is **derived GUI-side** by mapping the
//!    finding's semantic ref to the pick candidates the canvas already built and
//!    taking a representative point (the union bbox centre of the matching copper).
//!    This is the same `FeatureOrigin`-attributed candidate stream the picker walks
//!    (issue 0031); a finding with no on-board candidate (e.g. a net with no copper
//!    yet) simply has no halo and lives only in the panel.
//! 2. **Elaboration diagnostics only exist on a *failed* load** (the hard-fault
//!    channel returns `Err(Vec<Diagnostic>)` and no `Doc`). A committed `Doc` has, by
//!    construction, zero elaboration faults — so on a good doc the findings set is
//!    exactly DRC ∪ ERC ∪ Floating. The failed-load diagnostics surface through the
//!    persistent error banner (`app.rs`), not this per-revision set (there is no
//!    `Doc` to compute findings over).
//!
//! # Computed once per revision, cached like the canvas
//!
//! [`Findings::compute`] runs the three queries once and folds them; the app holds
//! the result across frames and rebuilds it only when the doc revision changes (the
//! same discipline as the layer assets). `build` never recomputes findings.

use crate::canvas::pick::{Candidate, SemanticId};
use ecad_core::coord::{MM, Nm, Point};
use ecad_core::diagnostic::{Diagnose, Diagnostic, Location, Severity};
use ecad_core::doc::{Doc, PinRef};
use ecad_core::id::NetId;
use ecad_core::part::PartLib;
use ecad_core::query::{Engine, Key};

/// One finding row, projected from a DRC violation or a diagnostic. Holds the
/// severity + message for the panel, the semantic refs to fold into the selection on
/// click (so the panes cross-highlight), and an optional board-mm location for the
/// canvas halo (derived GUI-side — the engine surfaces carry no mm; see the module
/// docs).
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// Error or warning — drives the badge colour and the chip counts.
    pub severity: Severity,
    /// The stable diagnostic code (e.g. `"E_DRC_CLEARANCE"`) — the panel row's mono
    /// prefix and a stable test anchor.
    pub code: &'static str,
    /// The human-readable message from the engine.
    pub message: String,
    /// The semantic ids this finding highlights / selects. A clearance violation
    /// carries **both** nets; a floating pad carries the pin (and, derived, its part).
    /// Empty only for a whole-document diagnostic with no semantic anchor.
    pub refs: Vec<SemanticId>,
    /// A representative board-mm point for the canvas halo, derived from the pick
    /// candidates matching `refs`. `None` when no on-board copper matches (panel-only
    /// finding — e.g. a schematic-side ERC net with no laid copper).
    pub board_mm: Option<(f32, f32)>,
}

impl Finding {
    /// Is this an error-severity finding?
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// The per-revision findings set: the folded DRC + ERC + connectivity findings plus
/// the error / warning tallies for the DRC chip. Computed once per doc revision by
/// [`Findings::compute`] and cached in app state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Findings {
    /// All findings, sorted errors-first then by code/message (deterministic).
    pub items: Vec<Finding>,
    /// Count of error-severity findings (the chip's red count).
    pub errors: usize,
    /// Count of warning-severity findings (the chip's amber count).
    pub warnings: usize,
}

impl Findings {
    /// Compute the findings over a committed `Doc`: run the DRC, ERC, and Floating
    /// queries through a fresh [`Engine`], render each into a [`Finding`], and derive
    /// board-mm halo locations from `candidates` (the same `FeatureOrigin`-attributed
    /// pick stream the canvas built). Pure over `(doc, lib, candidates)`.
    ///
    /// A fresh engine per call is deliberate: the GUI recomputes only when the doc
    /// *revision* changed (the caller gates on that), at which point the incremental
    /// engine's memo would be invalidated anyway. The engine's value is its early
    /// cutoff *within* a revision; across a reload the doc is a new value.
    pub fn compute(doc: &Doc, lib: &PartLib, candidates: &[Candidate]) -> Findings {
        let mut engine = Engine::new();

        // Collect the raw diagnostics from the three findings queries. DRC violations
        // are a typed domain result rendered through `Diagnose`; ERC / Floating are
        // already `Diagnostic`s.
        let mut diags: Vec<Diagnostic> = Vec::new();
        for v in engine.query(doc, lib, Key::Drc).as_drc() {
            diags.extend(v.diagnostics());
        }
        diags.extend(engine.query(doc, lib, Key::Erc).as_erc().iter().cloned());
        diags.extend(
            engine
                .query(doc, lib, Key::Floating)
                .as_floating()
                .iter()
                .cloned(),
        );

        // But DRC clearance carries only ONE net in its rendered `Location` (the
        // lower-sorted one) even though the violation is a *pair*. Re-run the raw DRC
        // to recover both nets for the clearance refs, so selecting a clearance finding
        // highlights BOTH offending nets. (The typed `Violation` is the honest source;
        // the diagnostic is a lossy text projection.)
        let drc = engine.query(doc, lib, Key::Drc).as_drc().to_vec();

        let mut items: Vec<Finding> = Vec::new();

        // DRC: map each typed violation to a finding with its full ref set.
        for v in &drc {
            let (code, refs) = violation_refs(v);
            let message = v
                .diagnostics()
                .into_iter()
                .next()
                .map(|d| d.message)
                .unwrap_or_default();
            let board_mm = halo_point(&refs, doc, candidates);
            items.push(Finding {
                severity: Severity::Error,
                code,
                message,
                refs,
                board_mm,
            });
        }

        // ERC + Floating: map each diagnostic's `Location` to semantic refs.
        for d in engine.query(doc, lib, Key::Erc).as_erc() {
            items.push(finding_from_diagnostic(d, doc, candidates));
        }
        for d in engine.query(doc, lib, Key::Floating).as_floating() {
            items.push(finding_from_diagnostic(d, doc, candidates));
        }

        // Deterministic order: errors before warnings, then by code, then message.
        items.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| a.code.cmp(b.code))
                .then_with(|| a.message.cmp(&b.message))
        });

        let errors = items.iter().filter(|f| f.is_error()).count();
        let warnings = items.len() - errors;
        Findings {
            items,
            errors,
            warnings,
        }
    }

    /// True when there are no findings at all (the chip reads green).
    pub fn is_clean(&self) -> bool {
        self.items.is_empty()
    }
}

/// The stable code + full semantic ref set for a typed DRC [`Violation`]. This is the
/// honest ref recovery the lossy `Diagnose` text projection can't give (clearance
/// names both nets here, not just the sorted-lower one).
fn violation_refs(v: &ecad_core::route::Violation) -> (&'static str, Vec<SemanticId>) {
    use ecad_core::route::Violation;
    match v {
        Violation::Clearance { a, b, .. } => (
            "E_DRC_CLEARANCE",
            vec![SemanticId::Net(a.clone()), SemanticId::Net(b.clone())],
        ),
        Violation::MinWidth { trace, .. } => ("E_DRC_MIN_WIDTH", vec![SemanticId::Trace(*trace)]),
        Violation::Unrouted { net, .. } => ("E_DRC_UNROUTED", vec![SemanticId::Net(net.clone())]),
        Violation::Keepout { net, .. } => ("E_DRC_KEEPOUT", vec![SemanticId::Net(net.clone())]),
        Violation::EdgeClearance { net } => {
            ("E_DRC_EDGE_CLEARANCE", vec![SemanticId::Net(net.clone())])
        }
    }
}

/// Project a [`Diagnostic`] (ERC / Floating) into a [`Finding`], mapping its
/// [`Location`] to semantic refs. A `Pin` location also contributes the owning
/// `Part` ref so selecting a floating-pad finding highlights the whole part on the
/// schematic (the pin stub alone is easy to miss).
fn finding_from_diagnostic(d: &Diagnostic, doc: &Doc, candidates: &[Candidate]) -> Finding {
    let refs = location_refs(&d.location);
    let board_mm = halo_point(&refs, doc, candidates);
    Finding {
        severity: d.severity,
        code: d.code,
        message: d.message.clone(),
        refs,
        board_mm,
    }
}

/// Map a diagnostic [`Location`] to the semantic ids to select / highlight.
fn location_refs(loc: &Location) -> Vec<SemanticId> {
    match loc {
        Location::Net(n) => vec![SemanticId::Net(n.clone())],
        Location::Trace(t) => vec![SemanticId::Trace(*t)],
        Location::Via(v) => vec![SemanticId::Via(*v)],
        Location::Entity(e) => vec![SemanticId::Part(e.clone())],
        Location::Pin(pr) => vec![
            SemanticId::Pin {
                comp: pr.comp.clone(),
                pin: pr.pin.clone(),
            },
            // Also the owning part, so the schematic symbol lights up.
            SemanticId::Part(pr.comp.clone()),
        ],
        // A textual span or whole-document issue anchors to no semantic id.
        Location::Span { .. } | Location::None => Vec::new(),
    }
}

/// A representative board-mm point for a finding's halo, derived from the pick
/// candidates that match any of `refs`. The engine surfaces carry no mm (see the
/// module docs), so the location is the **centre of the union bbox** of every
/// candidate whose id — or net — matches a ref. `None` when nothing on the board
/// matches (a panel-only finding).
///
/// Matching mirrors the overlay's [`board_matches`](crate::highlight::HighlightSets::board_matches)
/// logic: a candidate matches a `Net` ref if the candidate's own net is that net (so
/// a clearance on net GND finds all GND copper), and matches a direct copper ref
/// (`Trace`/`Via`/`Pour`/`Pin`) by id.
fn halo_point(refs: &[SemanticId], doc: &Doc, candidates: &[Candidate]) -> Option<(f32, f32)> {
    let ref_nets: Vec<&NetId> = refs
        .iter()
        .filter_map(|r| match r {
            SemanticId::Net(n) => Some(n),
            _ => None,
        })
        .collect();

    let mut min = Point {
        x: Nm::MAX,
        y: Nm::MAX,
    };
    let mut max = Point {
        x: Nm::MIN,
        y: Nm::MIN,
    };
    let mut any = false;

    for c in candidates {
        let matches_id = refs.contains(&c.id);
        let matches_net = !ref_nets.is_empty()
            && candidate_net(&c.id, doc).is_some_and(|n| ref_nets.iter().any(|r| **r == n));
        if !matches_id && !matches_net {
            continue;
        }
        if let Some((lo, hi)) = c.shape.bbox() {
            min.x = min.x.min(lo.x);
            min.y = min.y.min(lo.y);
            max.x = max.x.max(hi.x);
            max.y = max.y.max(hi.y);
            any = true;
        }
    }
    if !any {
        return None;
    }
    // Board-mm centre of the union bbox (y stays board-frame / up; the overlay applies
    // the y-flip when it draws — a halo point is a *board* coordinate).
    let cx = (min.x + max.x) as f32 / 2.0 / MM as f32;
    let cy = (min.y + max.y) as f32 / 2.0 / MM as f32;
    Some((cx, cy))
}

/// The net a board candidate id belongs to, if any — the net-match half of
/// [`halo_point`]. Mirrors `app::EcadApp::candidate_net` (kept local so findings has no
/// dependency on the app struct).
fn candidate_net(id: &SemanticId, doc: &Doc) -> Option<NetId> {
    match id {
        SemanticId::Trace(t) => doc.traces.get(t).map(|t| t.net.clone()),
        SemanticId::Via(v) => doc.vias.get(v).map(|v| v.net.clone()),
        SemanticId::Pour { net, .. } => Some(net.clone()),
        SemanticId::Pin { comp, pin } => {
            let pr = PinRef::new(comp, pin);
            doc.nets
                .iter()
                .find(|(_, n)| n.members.contains(&pr))
                .map(|(nid, _)| nid.clone())
        }
        SemanticId::Net(n) => Some(n.clone()),
        SemanticId::Part(_) => None,
    }
}

#[cfg(test)]
mod tests;
