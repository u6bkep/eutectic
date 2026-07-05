//! Domain state — the source-of-truth half of `gui-architecture.md` through-line 3
//! (domain state / pane state split): the loaded document, its resolver input
//! ([`LibSource`]), the resolved library + notes, the shared semantic selection, and
//! the revision-keyed [`DerivedCaches`] bundle (board / schematic / explorer /
//! findings). Split out of `app.rs` as pure code motion (facade + submodules).

use crate::canvas::pick::{self, Candidate, SemanticId};
use crate::canvas::{BoardLayer, Canvas};
use crate::explorer::Explorer;
use crate::findings::Findings;
use crate::registry::{self, LibNote, Registry};
use crate::schematic_view::SchematicView;
use crate::selection::SelectionModel;
use ecad_core::doc::Doc;
use std::cell::RefCell;

/// Where a [`DomainState`]'s [`PartLib`](ecad_core::part::PartLib) comes from —
/// the resolver *input* (library packages, slice 2). Resolution re-runs on
/// every (re)load: a reload may add or remove `use` lines, and a registry edit
/// re-resolves the current doc.
///
/// - [`Fixed`](LibSource::Fixed): a pre-resolved library. The fixture path
///   (`from_source_with`) and the toy default (`from_source`) — resolution is
///   the identity, zero notes.
/// - [`Registry`](LibSource::Registry): the per-machine name → path registry
///   (`docs/architecture.md` §9). Each load parses the source's `use` names and
///   resolves them through [`registry::resolve`]; `save_path` is where a
///   Libraries-menu edit persists the registry (`None` = don't persist — tests
///   and fixtures). Only `main.rs` computes the real default location.
pub enum LibSource {
    /// A pre-resolved, load-time-fixed library (fixtures / the toy default).
    Fixed(ecad_core::part::PartLib),
    /// Resolve `use` names through the per-machine registry on every load.
    Registry {
        /// The name → directory registry (mutated by the Libraries menu).
        registry: Registry,
        /// Where menu edits save the registry; `None` = in-memory only.
        save_path: Option<std::path::PathBuf>,
    },
}

/// Domain state: the source-of-truth half of `gui-architecture.md` through-line
/// 3 (domain state / pane state split).
///
/// In v1 this grows to hold the source text, the elaborated [`Doc`], derived
/// caches, the semantic selection set, and findings. Milestone 1 loads a
/// document once at startup and holds only the pieces the skeleton renders.
///
/// The full split — domain state shared across a *tree* of panes, each pane
/// projecting the shared semantic selection into its own overlay — is
/// milestones 3–4. This struct is intentionally the shared, view-independent
/// half so that later panes hang off it without a rewrite.
pub struct DomainState {
    /// The `.ecad` source text the document was loaded from (empty for the
    /// no-document state). Editing this and re-elaborating is the source-first
    /// mutation loop of milestone 5+; here it is load-once and read-only.
    pub source: String,
    /// The elaborated document, or the parse/elaborate error to surface in the
    /// UI. Per the permissive philosophy (`gui-architecture.md`, "Editing
    /// philosophy"), a bad load never crashes — it renders as an alert.
    pub doc: Result<Doc, String>,
    /// Where the library comes from — the resolver input. Resolution re-runs
    /// on every (re)load against this (see [`LibSource`]); the *result* is
    /// cached in [`lib`](Self::lib) + [`lib_notes`](Self::lib_notes) below.
    pub lib_source: LibSource,
    /// The **resolved** part library for the current load — what elaboration
    /// and rendering use. Re-derived from [`lib_source`](Self::lib_source) on
    /// every successful (re)load; for a [`LibSource::Fixed`] domain this is
    /// simply that library.
    pub lib: ecad_core::part::PartLib,
    /// The library-resolution notes for the current load (unregistered `use`
    /// names, packages that failed to load, union collisions) — data, not
    /// errors (§9 permissive rule). Merged into the findings panel.
    pub lib_notes: Vec<LibNote>,
    /// The filename the document was loaded from, for the toolbar badge.
    /// `None` in the no-document state.
    pub filename: Option<String>,
    /// The semantic selection + hover model (structural commitment 2). Lives in
    /// domain state — shared, view-independent — so every pane projects the same
    /// selection into its own overlay (milestone 4's schematic pane reuses it
    /// untouched). `RefCell` for the damascene interior-mutability pattern: written in
    /// `on_event`, read in `build` through `&self`.
    pub selection: RefCell<SelectionModel>,
    /// The doc **revision** counter — bumped on every successful reload (m5). The
    /// derived caches (canvas layers, schematic, explorer, findings) are rebuilt only
    /// when this changes, exactly like `ecad-core`'s query-engine revision. Load-once
    /// domains sit at revision 0.
    pub revision: u64,
    /// The persistent last-good-load error, if the most recent reload FAILED to
    /// parse/elaborate. Per the permissive philosophy this does NOT replace the
    /// rendered doc (the last-good doc stays on screen); the chrome surfaces it as an
    /// unmissable banner until a good reload clears it. `None` when the current doc is
    /// the freshest source.
    pub reload_error: Option<String>,
}

impl DomainState {
    /// The empty state: no document loaded.
    pub fn empty() -> Self {
        DomainState {
            source: String::new(),
            doc: Err("no document".to_string()),
            lib_source: LibSource::Fixed(ecad_core::part::part_library()),
            lib: ecad_core::part::part_library(),
            lib_notes: Vec::new(),
            filename: None,
            selection: RefCell::new(SelectionModel::new()),
            revision: 0,
            reload_error: None,
        }
    }

    /// Replace this domain's [`LibSource`] without re-elaborating — for wiring
    /// the registry into the **empty** (no-document) state, where there is
    /// nothing to resolve yet but the Libraries menu must still edit the real
    /// registry. A loaded document should use
    /// [`from_source_registry`](Self::from_source_registry) instead, which
    /// resolves before elaborating.
    pub fn with_lib_source(mut self, lib_source: LibSource) -> Self {
        self.lib_source = lib_source;
        self
    }

    /// Load a document from `.ecad` source text, parsing + elaborating it
    /// through `ecad-core`'s public command API (the same entry point
    /// `examples/poc_multiprobe.rs` and `examples/schematic.rs` use:
    /// `History` + `Command::LoadText`). Never panics: an elaboration failure
    /// is captured in [`DomainState::doc`] as `Err` for the UI to display.
    pub fn from_source(source: String, filename: Option<String>) -> Self {
        Self::from_source_with(source, filename, ecad_core::part::part_library(), |_| {
            Vec::new()
        })
    }

    /// Load a document from `.ecad` source with an explicit part library and a
    /// post-load command batch — the general path [`from_source`](Self::from_source)
    /// specialises. The `extra` closure sees the loaded [`Doc`] (so it can free
    /// trace / via ids and reference committed nets) and returns commands committed
    /// in one follow-up transaction. Used by the board fixture to add routed copper
    /// (traces / vias), which is command-authored, not source-authored. Never
    /// panics: any failure is captured in [`DomainState::doc`] as `Err`.
    pub fn from_source_with(
        source: String,
        filename: Option<String>,
        lib: ecad_core::part::PartLib,
        extra: impl FnOnce(&Doc) -> Vec<ecad_core::command::Command>,
    ) -> Self {
        let doc = elaborate(&source, &lib, extra);
        DomainState {
            source,
            doc,
            lib_source: LibSource::Fixed(lib.clone()),
            lib,
            lib_notes: Vec::new(),
            filename,
            selection: RefCell::new(SelectionModel::new()),
            revision: 0,
            reload_error: None,
        }
    }

    /// Load a document from `.ecad` source, resolving its `use` names through
    /// the per-machine `registry` (library packages, slice 2 — the windowed
    /// `main.rs` path). Resolution runs first ([`registry::resolve`]: registry
    /// hits unioned in source order, the built-in toy library appended last),
    /// then the source elaborates against the resolved lib. Resolution
    /// failures are notes (`lib_notes`), never load errors.
    pub fn from_source_registry(
        source: String,
        filename: Option<String>,
        registry: Registry,
        save_path: Option<std::path::PathBuf>,
    ) -> Self {
        let (lib, lib_notes) = registry::resolve(&source, &registry);
        let doc = elaborate(&source, &lib, |_| Vec::new());
        DomainState {
            source,
            doc,
            lib_source: LibSource::Registry {
                registry,
                save_path,
            },
            lib,
            lib_notes,
            filename,
            selection: RefCell::new(SelectionModel::new()),
            revision: 0,
            reload_error: None,
        }
    }

    /// Resolve `source`'s libraries against **this** domain's [`LibSource`] —
    /// the shared resolution step of the initial load and every reload. A
    /// `Fixed` source is the identity (its lib, zero notes); a `Registry`
    /// source re-runs [`registry::resolve`], because a reload may add or
    /// remove `use` lines and a registry edit changes what a name binds to.
    fn resolve_lib(&self, source: &str) -> (ecad_core::part::PartLib, Vec<LibNote>) {
        match &self.lib_source {
            LibSource::Fixed(lib) => (lib.clone(), Vec::new()),
            LibSource::Registry { registry, .. } => registry::resolve(source, registry),
        }
    }

    /// Re-resolve + re-parse + re-elaborate `source` — the reload entry point.
    /// Returns the freshly-resolved library + notes and the elaboration result
    /// WITHOUT touching `self`; the caller ([`EcadApp::apply_reload`](crate::app::EcadApp::apply_reload))
    /// decides whether to swap them in (success) or keep the last-good doc + lib and
    /// surface the error (failure). Pure over `(source, self.lib_source)`.
    pub(crate) fn elaborate_source(
        &self,
        source: &str,
    ) -> (ecad_core::part::PartLib, Vec<LibNote>, Result<Doc, String>) {
        let (lib, notes) = self.resolve_lib(source);
        let doc = elaborate(source, &lib, |_| Vec::new());
        (lib, notes, doc)
    }
}

/// Parse + elaborate `source` against `lib` through `ecad-core`'s public
/// command API (`History` + `Command::LoadText` — the same entry point the
/// `ecad-core` examples use), then commit the `extra` post-load command batch
/// (fixture-routed copper) if non-empty. Never panics: any failure is the
/// `Err` string for the UI to display.
fn elaborate(
    source: &str,
    lib: &ecad_core::part::PartLib,
    extra: impl FnOnce(&Doc) -> Vec<ecad_core::command::Command>,
) -> Result<Doc, String> {
    use ecad_core::command::{Command, Transaction};
    use ecad_core::history::History;

    let fmt = |diags: Vec<ecad_core::diagnostic::Diagnostic>| {
        diags
            .iter()
            .map(|d| format!("[{}] {}", d.code, d.message))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut history = History::new(Doc::default());
    history
        .commit(
            Transaction::one(Command::LoadText(source.to_string())),
            lib,
            "load",
        )
        .map_err(fmt)
        .and_then(|_| {
            let cmds = extra(history.doc());
            if cmds.is_empty() {
                Ok(history.doc().clone())
            } else {
                history
                    .commit(Transaction(cmds), lib, "fixture-route")
                    .map(|_| history.doc().clone())
                    .map_err(fmt)
            }
        })
}

/// The board projection held in app state: the [`Canvas`] (for coordinate
/// inversion), the tessellated per-layer assets it built once, and the pre-built
/// pick candidates (folded from the `world_features` stream via each feature's
/// `FeatureOrigin` — see [`crate::canvas::pick`]). All built once per (doc
/// revision) load.
pub(crate) struct BoardView {
    pub(crate) canvas: Canvas,
    pub(crate) layers: Vec<BoardLayer>,
    /// Pickable candidates (pins / traces / vias / pours), folded from the same
    /// `world_features` stream the canvas renders and rebuilt only when the doc
    /// loads — the hit-test input.
    pub(crate) candidates: Vec<Candidate>,
}

/// Everything derived from the elaborated [`Doc`], rebuilt together on a reload (the
/// revision-keyed cache). Holding these as one bundle behind a single `RefCell` means
/// a reload swaps the *whole* derived tier atomically — no half-updated frame where a
/// new board pairs with old findings.
pub(crate) struct DerivedCaches {
    /// The board projection + cached per-layer assets, or `None` when no document is
    /// loaded / the load failed / projection failed.
    pub(crate) board: Option<BoardView>,
    /// The schematic projection + cached asset + pick candidates, or `None` when the
    /// doc has no components.
    pub(crate) schematic: Option<SchematicView>,
    /// The projected explorer rows (components / nets).
    pub(crate) explorer: Explorer,
    /// The per-revision findings (DRC + ERC + connectivity), computed once here and
    /// read every frame by the panel / chip / overlays.
    pub(crate) findings: Findings,
}

impl DerivedCaches {
    /// Empty caches (no document / failed load).
    pub(crate) fn empty() -> DerivedCaches {
        DerivedCaches {
            board: None,
            schematic: None,
            explorer: Explorer::default(),
            findings: Findings::default(),
        }
    }

    /// Build every derived cache from a document + resolved library (+ the
    /// load's library-resolution notes, which merge into the findings). A
    /// projection failure (unreachable for a committed doc) degrades to "no
    /// board view" rather than crashing — the permissive philosophy.
    pub(crate) fn build(
        doc: &Doc,
        lib: &ecad_core::part::PartLib,
        lib_notes: &[LibNote],
    ) -> DerivedCaches {
        // The layered canvas + pick candidates, from the one `world_features` stream.
        let board = Canvas::new(doc, lib)
            .and_then(|canvas| {
                let layers = canvas.build_layers(doc, lib)?;
                let su = ecad_core::elaborate::stackup(&doc.source);
                let candidates = pick::candidates(doc, lib, &su);
                Ok(BoardView {
                    canvas,
                    layers,
                    candidates,
                })
            })
            .ok();
        let schematic = SchematicView::build(doc, lib);
        let explorer = Explorer::project(doc, lib);
        // Findings are derived from the board pick candidates (the halo-location
        // source) — empty candidate list when the board didn't project, which is fine
        // (findings then carry no board-mm point, panel-only).
        let candidates: &[Candidate] = board.as_ref().map_or(&[], |b| &b.candidates);
        let findings = Findings::compute(doc, lib, candidates, lib_notes);
        DerivedCaches {
            board,
            schematic,
            explorer,
            findings,
        }
    }
}

/// Does a [`SemanticId`] resolve against the new doc + derived caches? A board copper
/// id must still be a pick candidate; a schematic id a schematic candidate; a `Net` /
/// `Part` must be present in the doc's maps. This is the "prune dangling selection"
/// predicate the reload contract requires.
pub(crate) fn resolves_in(id: &SemanticId, doc: &Doc, derived: &DerivedCaches) -> bool {
    match id {
        SemanticId::Net(n) => doc.nets.contains_key(n),
        SemanticId::Part(e) => doc.components.contains_key(e),
        SemanticId::Trace(t) => doc.traces.contains_key(t),
        SemanticId::Via(v) => doc.vias.contains_key(v),
        SemanticId::Pour { .. } | SemanticId::Pin { .. } => {
            // Pours and pins have no top-level doc map; a pin resolves iff its owning
            // component still exists AND the pad is a live pick candidate. Fall back to
            // the candidate sets (board + schematic), which are exactly "what can be
            // selected in the new doc".
            let on_board = derived
                .board
                .as_ref()
                .is_some_and(|b| b.candidates.iter().any(|c| &c.id == id));
            let on_schem = derived
                .schematic
                .as_ref()
                .is_some_and(|s| s.candidates().iter().any(|c| &c.id == id));
            on_board || on_schem
        }
    }
}

/// Cheap summary stats over an elaborated [`Doc`], for the skeleton's status
/// card. Everything here is read straight off the public `ecad-core` API — no
/// routing, no export — so it is safe to compute every frame.
pub(crate) struct DocStats {
    pub(crate) parts: usize,
    pub(crate) nets: usize,
    pub(crate) layers: usize,
    /// Board outline extent in mm (width, height), if the source authored a
    /// board outline.
    pub(crate) board_mm: Option<(f64, f64)>,
}

impl DocStats {
    pub(crate) fn of(doc: &Doc) -> Self {
        let stackup = ecad_core::elaborate::stackup(&doc.source);
        // Layer count = copper slabs (the meaningful "layers" a board has).
        let layers = stackup.copper_slabs().len();
        let board_mm = ecad_core::elaborate::board_region(&doc.source)
            .and_then(|region| region.bbox())
            .map(|(min, max)| {
                let mm = ecad_core::doc::MM as f64;
                ((max.x - min.x) as f64 / mm, (max.y - min.y) as f64 / mm)
            });
        DocStats {
            parts: doc.components.len(),
            nets: doc.nets.len(),
            layers,
            board_mm,
        }
    }
}
