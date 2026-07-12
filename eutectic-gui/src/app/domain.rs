//! Domain state — the source-of-truth half of `gui-architecture.md` through-line 3
//! (domain state / pane state split): the loaded document, its resolver input
//! ([`LibSource`]), the resolved library + notes, the shared semantic selection, and
//! the revision-keyed [`DerivedCaches`] bundle (board / schematic / explorer /
//! findings). Split out of `app.rs` as pure code motion (facade + submodules).

use crate::explorer::Explorer;
use crate::findings::Findings;
use crate::pick::{self, Candidate, LayerId, SemanticId};
use crate::registry::{self, LibNote, Registry};
use crate::render::scene::PlaneKey;
use crate::selection::SelectionModel;
use damascene_core::prelude::Color;
use eutectic_core::doc::Doc;
use eutectic_core::history::History;
use std::cell::RefCell;

/// Where a [`DomainState`]'s [`PartLib`](eutectic_core::part::PartLib) comes from —
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
    Fixed(eutectic_core::part::PartLib),
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
    /// The `.eut` source text the document was loaded from (empty for the
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
    pub lib: eutectic_core::part::PartLib,
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
    /// when this changes, exactly like `eutectic-core`'s query-engine revision. Load-once
    /// domains sit at revision 0.
    pub revision: u64,
    /// The persistent last-good-load error, if the most recent reload FAILED to
    /// parse/elaborate. Per the permissive philosophy this does NOT replace the
    /// rendered doc (the last-good doc stays on screen); the chrome surfaces it as an
    /// unmissable banner until a good reload clears it. `None` when the current doc is
    /// the freshest source.
    pub reload_error: Option<String>,
    /// The held engine [`History`] behind [`doc`](Self::doc) — the m6 editing
    /// foundation. GUI mutations are engine commands committed against THIS history
    /// ([`commit`](Self::commit)), so command-authored state survives across GUI
    /// edits instead of being rebuilt from source each time. Rebuilt only on a
    /// (re)load / undo / redo (each of which elaborates fresh source text). `None`
    /// exactly when [`doc`](Self::doc) is `Err` (no document / failed load).
    pub(crate) history: Option<History>,
    /// The filesystem path the document was loaded from — where explicit Save
    /// writes. `None` for fixtures / in-memory docs, which therefore have **no
    /// save affordance** (the decided save model). Only `main.rs` sets it.
    pub source_path: Option<std::path::PathBuf>,
    /// The m6 editing state: dirty flag, undo/redo source snapshots, the
    /// last-saved content (the dirty-compare + save-echo baselines), the pending
    /// disk-conflict text, and the last save/commit error.
    pub(crate) edit: EditState,
}

/// The GUI editing state (m6, the decided save model — `docs/gui-architecture.md`,
/// "Save model"): edits live in memory as dirty state; explicit save writes
/// `serialize(doc)`; a disk change while dirty is a conflict banner, never silent
/// last-writer; undo/redo are source snapshots.
#[derive(Debug, Default)]
pub(crate) struct EditState {
    /// True when the doc has commits not yet written to the file. Set by every
    /// GUI commit; cleared by Save and by an applied external reload; recomputed
    /// on undo/redo by comparing the restored snapshot to [`saved_canon`](Self::saved_canon).
    pub(crate) dirty: bool,
    /// Undo stack: the canonical `serialize(doc)` snapshot taken **before** each
    /// GUI commit (newest last). Bounded at [`UNDO_CAP`]; cleared on external reload.
    pub(crate) undo: Vec<String>,
    /// Redo stack: snapshots displaced by undo (newest last). Cleared on every
    /// new GUI commit and on external reload.
    pub(crate) redo: Vec<String>,
    /// The canonical `serialize(doc)` that corresponds to the on-disk state: set
    /// at load / applied reload (serialize of the freshly loaded doc) and at Save
    /// (the exact text written). The undo/redo dirty compare — "an undo/redo does
    /// NOT clear dirty unless the restored snapshot equals the last-saved
    /// content" — is a cheap string compare against this.
    pub(crate) saved_canon: Option<String>,
    /// The exact text of our own last Save write, for **watcher echo
    /// suppression**: a mailbox delivery whose text equals this is our own write
    /// coming back through the file watcher and is consumed silently. Cleared
    /// when a genuinely external change applies (disk has moved on).
    pub(crate) last_saved_write: Option<String>,
    /// A disk change delivered while the doc was dirty — the pending conflict
    /// (the full new source text). Renders as the persistent conflict banner with
    /// explicit Reload / Keep-mine actions; a newer delivery replaces it.
    pub(crate) conflict: Option<String>,
    /// The last save/commit failure, rendered as a persistent destructive chip
    /// until the next successful save/commit.
    pub(crate) error: Option<String>,
}

/// Undo-stack bound: the oldest snapshot is dropped beyond this depth.
pub(crate) const UNDO_CAP: usize = 100;

impl DomainState {
    /// The empty state: no document loaded.
    pub fn empty() -> Self {
        DomainState {
            source: String::new(),
            doc: Err("no document".to_string()),
            lib_source: LibSource::Fixed(eutectic_core::part::part_library()),
            lib: eutectic_core::part::part_library(),
            lib_notes: Vec::new(),
            filename: None,
            selection: RefCell::new(SelectionModel::new()),
            revision: 0,
            reload_error: None,
            history: None,
            source_path: None,
            edit: EditState::default(),
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

    /// Load a document from `.eut` source text, parsing + elaborating it
    /// through `eutectic-core`'s public command API (the same entry point
    /// `examples/poc_multiprobe.rs` and `examples/schematic.rs` use:
    /// `History` + `Command::LoadText`). Never panics: an elaboration failure
    /// is captured in [`DomainState::doc`] as `Err` for the UI to display.
    pub fn from_source(source: String, filename: Option<String>) -> Self {
        Self::from_source_with(
            source,
            filename,
            eutectic_core::part::part_library(),
            |_| Vec::new(),
        )
    }

    /// Load a document from `.eut` source with an explicit part library and a
    /// post-load command batch — the general path [`from_source`](Self::from_source)
    /// specialises. The `extra` closure sees the loaded [`Doc`] (so it can free
    /// trace / via ids and reference committed nets) and returns commands committed
    /// in one follow-up transaction. Used by the board fixture to add routed copper
    /// (traces / vias), which is command-authored, not source-authored. Never
    /// panics: any failure is captured in [`DomainState::doc`] as `Err`.
    pub fn from_source_with(
        source: String,
        filename: Option<String>,
        lib: eutectic_core::part::PartLib,
        extra: impl FnOnce(&Doc) -> Vec<eutectic_core::command::Command>,
    ) -> Self {
        let history = elaborate(&source, &lib, extra);
        let (history, doc) = split_history(history);
        DomainState {
            source,
            edit: EditState {
                saved_canon: doc.as_ref().ok().map(eutectic_core::text::serialize),
                ..EditState::default()
            },
            doc,
            lib_source: LibSource::Fixed(lib.clone()),
            lib,
            lib_notes: Vec::new(),
            filename,
            selection: RefCell::new(SelectionModel::new()),
            revision: 0,
            reload_error: None,
            history,
            source_path: None,
        }
    }

    /// Load a document from `.eut` source, resolving its `use` names through
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
        let history = elaborate(&source, &lib, |_| Vec::new());
        let (history, doc) = split_history(history);
        DomainState {
            source,
            edit: EditState {
                saved_canon: doc.as_ref().ok().map(eutectic_core::text::serialize),
                ..EditState::default()
            },
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
            history,
            source_path: None,
        }
    }

    /// Resolve `source`'s libraries against **this** domain's [`LibSource`] —
    /// the shared resolution step of the initial load and every reload. A
    /// `Fixed` source is the identity (its lib, zero notes); a `Registry`
    /// source re-runs [`registry::resolve`], because a reload may add or
    /// remove `use` lines and a registry edit changes what a name binds to.
    fn resolve_lib(&self, source: &str) -> (eutectic_core::part::PartLib, Vec<LibNote>) {
        match &self.lib_source {
            LibSource::Fixed(lib) => (lib.clone(), Vec::new()),
            LibSource::Registry { registry, .. } => registry::resolve(source, registry),
        }
    }

    /// Re-resolve + re-parse + re-elaborate `source` — the reload entry point.
    /// Returns the freshly-resolved library + notes and the elaboration result (a
    /// fresh [`History`] whose head is the new doc) WITHOUT touching `self`; the
    /// caller ([`EutecticApp::apply_reload`](crate::app::EutecticApp::apply_reload) /
    /// `swap_source`) decides whether to swap them in (success) or keep the
    /// last-good doc + lib and surface the error (failure). Pure over
    /// `(source, self.lib_source)`.
    pub(crate) fn elaborate_source(
        &self,
        source: &str,
    ) -> (
        eutectic_core::part::PartLib,
        Vec<LibNote>,
        Result<History, String>,
    ) {
        let (lib, notes) = self.resolve_lib(source);
        let history = elaborate(source, &lib, |_| Vec::new());
        (lib, notes, history)
    }

    /// Commit a GUI-authored transaction against the held [`History`] — **the
    /// command-commit path** (m6). On success the head doc swaps in and
    /// [`source`](Self::source) is refreshed to the canonical `serialize(doc)`
    /// projection, so every "re-elaborate the current source" path (registry
    /// edits, undo snapshots) sees the command-authored state. On failure the
    /// history head is unchanged (engine atomicity) and the diagnostics come back
    /// as the `Err` string. Derived-cache rebuild / revision bump / dirty
    /// bookkeeping are the caller's job ([`EutecticApp::commit_edit`](crate::app::EutecticApp::commit_edit)).
    pub(crate) fn commit(
        &mut self,
        txn: eutectic_core::command::Transaction,
        label: &str,
    ) -> Result<(), String> {
        let Some(history) = self.history.as_mut() else {
            return Err("no document loaded".to_string());
        };
        history.commit(txn, &self.lib, label).map_err(fmt_diags)?;
        let doc = history.doc().clone();
        self.source = eutectic_core::text::serialize(&doc);
        self.doc = Ok(doc);
        Ok(())
    }
}

/// Split an elaboration result into the stored `(history, doc)` pair: the held
/// history (when the load succeeded) and the head-doc clone the render tier reads.
fn split_history(history: Result<History, String>) -> (Option<History>, Result<Doc, String>) {
    match history {
        Ok(h) => {
            let doc = h.doc().clone();
            (Some(h), Ok(doc))
        }
        Err(e) => (None, Err(e)),
    }
}

/// Render engine diagnostics as the one-string error the UI displays.
fn fmt_diags(diags: Vec<eutectic_core::diagnostic::Diagnostic>) -> String {
    diags
        .iter()
        .map(|d| format!("[{}] {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse + elaborate `source` against `lib` through `eutectic-core`'s public
/// command API (`History` + `Command::LoadText` — the same entry point the
/// `eutectic-core` examples use), then commit the `extra` post-load command batch
/// (fixture-routed copper) if non-empty. Returns the **whole `History`** (head =
/// the loaded doc) so the caller can hold it for GUI command commits (m6). Never
/// panics: any failure is the `Err` string for the UI to display.
fn elaborate(
    source: &str,
    lib: &eutectic_core::part::PartLib,
    extra: impl FnOnce(&Doc) -> Vec<eutectic_core::command::Command>,
) -> Result<History, String> {
    use eutectic_core::command::{Command, Transaction};

    let mut history = History::new(Doc::default());
    history
        .commit(
            Transaction::one(Command::LoadText(source.to_string())),
            lib,
            "load",
        )
        .map_err(fmt_diags)?;
    let cmds = extra(history.doc());
    if !cmds.is_empty() {
        history
            .commit(Transaction(cmds), lib, "fixture-route")
            .map_err(fmt_diags)?;
    }
    Ok(history)
}

/// Write `text` to `path` **atomically**: a temp file in the same directory,
/// then rename over the target — a crash mid-save never leaves a half-written
/// document (the same pattern as `registry::Registry::save`). Used by explicit
/// Save; the GUI never writes the user's file through any other path.
pub(crate) fn atomic_write(path: &std::path::Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("renaming {} over {}: {e}", tmp.display(), path.display()))
}

/// One board layer-panel row: identity, display name, swatch color. Derived
/// per doc revision from the renderer scene's plane list + the style tables
/// (WP3: the old `Canvas::build_layers` tessellation pass is gone — the
/// panel never needed the assets, only these rows).
#[derive(Clone, Debug)]
pub(crate) struct BoardLayer {
    /// Stable layer identity (also the visibility-toggle key source).
    pub(crate) id: LayerId,
    /// Display name for the layer panel (the slab name, or "Board outline").
    pub(crate) name: String,
    /// Default swatch colour (the style tables' dark-canvas defaults).
    pub(crate) color: Color,
}

/// The board projection held in app state: the per-layer rows the layer
/// panel lists, and the pre-built pick candidates (folded from the
/// `world_features` stream via each feature's `FeatureOrigin` — see
/// [`crate::pick`]). All built once per (doc revision) load.
pub(crate) struct BoardView {
    pub(crate) layers: Vec<BoardLayer>,
    /// Pickable candidates (pins / traces / vias / pours), folded from the same
    /// `world_features` stream the renderer scene lowers and rebuilt only when
    /// the doc loads — the hit-test input.
    pub(crate) candidates: Vec<Candidate>,
}

/// Derive the layer panel's rows from a board scene's plane list: one row
/// for the outline (the substrate shares its toggle), one per slab plane
/// (a slab's pour plane shares its copper row). Scene order is draw order
/// (bottom-first — the panel reverses for display, like the old canvas
/// projection).
fn layer_rows(scene: &crate::render::Scene) -> Vec<BoardLayer> {
    let tables = crate::render::StyleTables::board_defaults(true);
    let mut out: Vec<BoardLayer> = Vec::new();
    for p in &scene.planes {
        let (id, name) = match &p.key {
            PlaneKey::Outline => (LayerId::Outline, "Board outline".to_string()),
            PlaneKey::Copper(n) | PlaneKey::Mask(n) | PlaneKey::Silk(n) | PlaneKey::Fab(n) => {
                (LayerId::Slab(n.clone()), n.clone())
            }
            // Substrate follows the outline row; a pour follows its copper
            // row; drills/overlay/schematic tiers have no panel row.
            _ => continue,
        };
        let color = tables.plane_appearance(&p.key, scene).color;
        out.push(BoardLayer { id, name, color });
    }
    out
}

/// Everything derived from the elaborated [`Doc`], rebuilt together on a reload (the
/// revision-keyed cache). Holding these as one bundle behind a single `RefCell` means
/// a reload swaps the *whole* derived tier atomically — no half-updated frame where a
/// new board pairs with old findings.
pub(crate) struct DerivedCaches {
    /// The board projection (layer-panel rows + pick candidates), or `None`
    /// when no document is loaded / the load failed / projection failed.
    pub(crate) board: Option<BoardView>,
    /// The owned-canvas board scene: the board lowered through
    /// [`crate::render::board_scene`], rebuilt per doc revision like every
    /// other derived cache. `None` when the board didn't project. Pure CPU —
    /// the GPU buffers ([`crate::render::SceneCache`]) key off the revision
    /// and live in the windowed-only GPU bundle.
    pub(crate) scene: Option<crate::render::Scene>,
    /// The board semantic state buffer (hover/selection flag words) sized to
    /// [`scene`](Self::scene)'s semantic table. `RefCell`: mutated per frame
    /// by one-word diffs under a shared `derived` borrow.
    pub(crate) states: RefCell<crate::render::SemanticStates>,
    /// The owned-canvas schematic scene (WP3): the drawing lowered through
    /// [`crate::render::schematic_scene`] over `schematic_features`. `None`
    /// when the doc has no components (the pane shows a placeholder).
    pub(crate) schematic_scene: Option<crate::render::Scene>,
    /// The schematic semantic state buffer, sized to
    /// [`schematic_scene`](Self::schematic_scene)'s table.
    pub(crate) schematic_states: RefCell<crate::render::SemanticStates>,
    /// Schematic pick candidates, folded from the same `schematic_features`
    /// stream the scene lowers ([`crate::schematic_pick`]).
    pub(crate) schematic_picks: Vec<crate::schematic_pick::Candidate>,
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
            scene: None,
            states: RefCell::new(crate::render::SemanticStates::new(1)),
            schematic_scene: None,
            schematic_states: RefCell::new(crate::render::SemanticStates::new(1)),
            schematic_picks: Vec::new(),
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
        lib: &eutectic_core::part::PartLib,
        lib_notes: &[LibNote],
    ) -> DerivedCaches {
        // The board scene (owned canvas): a lowering failure means no board
        // pane, not a crash. The layer rows derive from its plane list and
        // the pick candidates from the same `world_features` stream it
        // lowered — one producer, no drift.
        let scene = crate::render::board_scene(doc, lib)
            .inspect_err(|e| log::warn!("board scene lowering failed: {e}"))
            .ok();
        let board = scene.as_ref().map(|s| {
            let su = eutectic_core::elaborate::stackup(&doc.source);
            BoardView {
                layers: layer_rows(s),
                candidates: pick::candidates(doc, lib, &su),
            }
        });
        let states = RefCell::new(match &scene {
            Some(s) => crate::render::SemanticStates::for_scene(s),
            None => crate::render::SemanticStates::new(1),
        });
        // The schematic scene + pick candidates, both over the one
        // `schematic_features` stream (Decision 23).
        let schematic_scene = crate::render::schematic_scene(doc, lib);
        let schematic_states = RefCell::new(match &schematic_scene {
            Some(s) => crate::render::SemanticStates::for_scene(s),
            None => crate::render::SemanticStates::new(1),
        });
        let schematic_picks = if doc.components.is_empty() {
            Vec::new()
        } else {
            crate::schematic_pick::candidates(&eutectic_core::schematic::schematic_features(
                doc, lib,
            ))
        };
        let explorer = Explorer::project(doc, lib);
        // Findings are derived from the board pick candidates (the halo-location
        // source) — empty candidate list when the board didn't project, which is fine
        // (findings then carry no board-mm point, panel-only).
        let candidates: &[Candidate] = board.as_ref().map_or(&[], |b| &b.candidates);
        let findings = Findings::compute(doc, lib, candidates, lib_notes);
        DerivedCaches {
            board,
            scene,
            states,
            schematic_scene,
            schematic_states,
            schematic_picks,
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
            let on_schem = derived.schematic_picks.iter().any(|c| &c.id == id);
            on_board || on_schem
        }
    }
}

/// Cheap summary stats over an elaborated [`Doc`], for the skeleton's status
/// card. Everything here is read straight off the public `eutectic-core` API — no
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
        let stackup = eutectic_core::elaborate::stackup(&doc.source);
        // Layer count = copper slabs (the meaningful "layers" a board has).
        let layers = stackup.copper_slabs().len();
        let board_mm = eutectic_core::elaborate::board_region(&doc.source)
            .and_then(|region| region.bbox())
            .map(|(min, max)| {
                let mm = eutectic_core::doc::MM as f64;
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
