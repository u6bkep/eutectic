//! The `ecad-gui` application shell — milestone 1 skeleton.
//!
//! This is the *workspace-conversion + skeleton* milestone (see
//! `docs/gui-architecture.md`, "v1 scope", milestone 1): the crate compiles,
//! a window can open, and the headless fixture/lint review loop is in place.
//! The interactive machinery — layered canvas, semantic selection, split-tree
//! panes, tools, findings — is milestones 2–6 and is deliberately *absent*
//! here. Where a future struct belongs, a stub with a doc-comment points at the
//! architecture through-line it will implement.

use damascene_core::prelude::*;
use ecad_core::doc::Doc;

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
    /// The part library used to elaborate and (later) render. The built-in
    /// library is enough for the skeleton; a real project supplies its own.
    pub lib: ecad_core::part::PartLib,
    /// The filename the document was loaded from, for the toolbar badge.
    /// `None` in the no-document state.
    pub filename: Option<String>,
}

impl DomainState {
    /// The empty state: no document loaded.
    pub fn empty() -> Self {
        DomainState {
            source: String::new(),
            doc: Err("no document".to_string()),
            lib: ecad_core::part::part_library(),
            filename: None,
        }
    }

    /// Load a document from `.ecad` source text, parsing + elaborating it
    /// through `ecad-core`'s public command API (the same entry point
    /// `examples/poc_multiprobe.rs` and `examples/schematic.rs` use:
    /// `History` + `Command::LoadText`). Never panics: an elaboration failure
    /// is captured in [`DomainState::doc`] as `Err` for the UI to display.
    pub fn from_source(source: String, filename: Option<String>) -> Self {
        use ecad_core::command::{Command, Transaction};
        use ecad_core::history::History;

        let lib = ecad_core::part::part_library();
        let mut history = History::new(Doc::default());
        let doc = history
            .commit(
                Transaction::one(Command::LoadText(source.clone())),
                &lib,
                "load",
            )
            .map(|_| history.doc().clone())
            .map_err(|diags| {
                diags
                    .iter()
                    .map(|d| format!("[{}] {}", d.code, d.message))
                    .collect::<Vec<_>>()
                    .join("\n")
            });

        DomainState {
            source,
            doc,
            lib,
            filename,
        }
    }
}

/// Per-pane view state: the *view-dependent* half of through-line 3.
///
/// A pane is one view (board / schematic / source) over the shared
/// [`DomainState`], with its own camera keyed by the pane's El key. v1 renders
/// a single pane; milestone 4 grows this into a Blender-style split tree
/// (`resize_handle`) of panes over the same domain state, and the semantic
/// selection projects into each pane's own highlight overlay.
///
/// Milestone 1 needs none of that machinery, so this is a placeholder: it names
/// the seam without building the split-tree / camera / canvas state that
/// milestones 2–4 own.
pub struct PaneState {
    /// The El key this pane's camera state lives under in damascene's
    /// `UiState`. Distinct per pane so two panes on the same doc get
    /// independent cameras (through-line 3). Unused until the viewport canvas
    /// arrives in milestone 2.
    pub key: String,
}

impl Default for PaneState {
    fn default() -> Self {
        PaneState {
            key: "pane:main".to_string(),
        }
    }
}

/// The milestone-1 application: a [`DomainState`] and a single [`PaneState`].
///
/// Implements [`App`] as a pure projection from state to a widget tree — the
/// shape `gui-architecture.md` calls out as matching the engine's source →
/// derived-views model.
pub struct EcadApp {
    pub domain: DomainState,
    #[allow(dead_code)] // wired into the canvas in milestone 2.
    pub pane: PaneState,
}

impl EcadApp {
    pub fn new(domain: DomainState) -> Self {
        EcadApp {
            domain,
            pane: PaneState::default(),
        }
    }
}

/// Cheap summary stats over an elaborated [`Doc`], for the skeleton's status
/// card. Everything here is read straight off the public `ecad-core` API — no
/// routing, no export — so it is safe to compute every frame.
struct DocStats {
    parts: usize,
    nets: usize,
    layers: usize,
    /// Board outline extent in mm (width, height), if the source authored a
    /// board outline.
    board_mm: Option<(f64, f64)>,
}

impl DocStats {
    fn of(doc: &Doc) -> Self {
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

impl App for EcadApp {
    fn build(&self, _cx: &BuildCx) -> El {
        let doc_badge = match (&self.domain.doc, &self.domain.filename) {
            (Ok(_), Some(name)) => badge(name.clone()).info(),
            (Ok(_), None) => badge("untitled").info(),
            (Err(_), _) => badge("no document").muted(),
        };

        let chrome = toolbar([toolbar_title("ecad"), spacer(), doc_badge])
            .padding(Sides::xy(tokens::SPACE_4, tokens::SPACE_2));

        let body = match &self.domain.doc {
            Ok(doc) => stats_card(&DocStats::of(doc)),
            Err(message) => error_card(message),
        };

        page([column([chrome, body])
            .gap(tokens::SPACE_4)
            .height(Size::Fill(1.0))])
    }
}

/// The document-loaded body: a card of cheap doc stats.
fn stats_card(stats: &DocStats) -> El {
    let board = match stats.board_mm {
        Some((w, h)) => format!("{w:.1} x {h:.1} mm"),
        None => "no board outline".to_string(),
    };
    titled_card(
        "Document",
        [
            field_row("Parts", text(stats.parts.to_string())),
            field_row("Nets", text(stats.nets.to_string())),
            field_row("Copper layers", text(stats.layers.to_string())),
            field_row("Board outline", text(board)),
        ],
    )
    .width(Size::Fixed(420.0))
}

/// The parse/elaborate-failure body: surface the error, never crash (the
/// permissive philosophy starts here).
fn error_card(message: &str) -> El {
    // The empty state uses the same path — "no document" is just an `Err`.
    if message == "no document" {
        return titled_card(
            "No document",
            [text("Pass a path to a .ecad file to load a document.").muted()],
        )
        .width(Size::Fixed(420.0));
    }
    alert([
        alert_title("Could not load document"),
        alert_description(message.to_string()),
    ])
    .destructive()
    .width(Size::Fixed(420.0))
}
