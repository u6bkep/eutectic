//! ecad-core — M1 engine prototype.
//!
//! A vertical slice of the architecture in docs/architecture.md:
//!
//! - `doc` — the immutable three-tier document (source/overrides -> materialized
//!   instances/nets; derived tier lives in `query`).
//! - `command` — the sole mutation surface: atomic transactions.
//! - `history` — the version DAG (undo / branch / replay).
//! - `query` — hand-rolled incremental query engine (Netlist, ERC, DRC).
//! - `route` — routed copper representation (trace/via/layer) + the DRC kernel.
//! - `autoroute` — basic deterministic grid/maze autorouter (transaction-proposer).
//! - `elaborate` — generative source -> instances + ID-keyed override reconcile.
//! - `part` — typed pins & interfaces (makes the serial swap unrepresentable).
//! - `project` — deterministic text projection (agent/git view).
//! - `text` — canonical serializer + parser for tier-1 truth (the text front-end).
//! - `export` — deterministic output artifacts (netlist / pick-and-place / SVG).

pub mod annotate;
pub mod autoroute;
pub mod command;
pub mod coord;
pub mod diagnostic;
pub mod doc;
pub mod elaborate;
pub mod export;
pub mod font;
pub mod geom;
pub mod history;
pub mod id;
pub mod ir;
pub mod kicad;
pub mod part;
pub mod project;
pub mod quantity;
pub mod query;
pub mod region;
pub mod route;
pub mod schematic;
pub mod schematic_svg;
pub mod solve;
pub mod svg_import;
pub mod text;
pub mod ttf;

/// Build a root document from a generative source by elaborating it once.
pub fn boot(
    source: elaborate::Source,
    lib: &part::PartLib,
) -> Result<doc::Doc, Vec<diagnostic::Diagnostic>> {
    let mut h = history::History::new(doc::Doc::default());
    h.commit(
        command::Transaction::one(command::Command::SetSource(source)),
        lib,
        "boot",
    )?;
    Ok(h.doc().clone())
}
