//! `eutectic-gui` — the GUI layer over `eutectic-core` (milestone 1 skeleton).
//!
//! The only crate in the workspace that depends on damascene / wgpu. See
//! `docs/gui-architecture.md` for the design of record; this crate implements
//! its milestone 1: workspace conversion, the `eutectic-gui` skeleton `App`, and
//! the headless fixture-and-lint review loop.

pub mod app;
pub mod canvas;
mod chrome;
pub mod explorer;
pub mod findings;
pub mod fixtures;
pub mod harness;
pub mod highlight;
pub mod host;
pub mod inspector;
mod panels;
mod panes;
pub mod registry;
pub mod reload;
pub mod schematic_view;
pub mod selection;
pub mod tool;

pub use app::{DomainState, EutecticApp, LibSource, PaneId, PaneLayout, PaneState, ViewKind};
pub use registry::{LibNote, Registry};
pub use reload::{SourceMailbox, SourceMsg};
