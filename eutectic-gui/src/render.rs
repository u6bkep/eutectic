//! The owned-canvas renderer core (WP1 of docs/renderer-spec.md).
//!
//! A renderer that turns realized design geometry into pane textures. It is a
//! pure function of a **scene** ([`Scene`] — typed primitives from a
//! producer), a **camera** ([`Camera`] — per-pane app state, f64 CPU math), a
//! set of **style tables** ([`StyleTables`] — per-plane appearance, resolved
//! through the damascene theme at uniform-write time), and a **semantic state
//! buffer** ([`SemanticStates`] — hover/selection flag words indexed by
//! semantic id). Nothing here knows about documents, tools, or damascene Els:
//! producers lower domain data to scenes, the app layer owns cameras and
//! state, and [`Renderer::render`](gpu::Renderer::render) draws into any
//! caller-provided `wgpu` texture view (WP2 points it at an `AppTexture`;
//! the golden tests point it at an owned readback texture).
//!
//! Module map (renderer-spec section in parentheses):
//! - [`scene`] — the ingest contract (§2): planes, primitives, semantic ids,
//!   style classes, deterministic ordering.
//! - [`board`] — the board producer: `route::world_features` → [`Scene`].
//! - [`schematic`] — the schematic producer (WP3): `schematic_features` →
//!   [`Scene`], the second producer on the same ingest.
//! - [`text`] — the MSDF annotation-text tier (§6, WP3): run layout, the
//!   glyph atlas (damascene machinery), glyph-quad instances.
//! - [`tess`] — polygon-with-holes triangulation (§3, CPU-side, lyon).
//! - [`instance`] — analytic-primitive instance building (§3, CPU-side).
//! - [`gpu`] — buffers, coverage + composite passes, procedural grid /
//!   crosshair, the headless-callable renderer entry (§3–§4).
//! - [`camera`] — f64 camera math, fit/frame, the glide filter (§7).
//! - [`state`] — the semantic state buffer, CPU side (§5).
//! - [`style`] — style tables + theme-token resolution (§8).
//! - [`damage`] — the pure damage-key rule (§7); WP2 wires it.

pub mod board;
pub mod camera;
pub mod damage;
pub mod gpu;
pub mod instance;
pub mod scene;
pub mod schematic;
pub mod state;
pub mod style;
pub mod tess;
pub mod text;

pub use board::board_scene;
pub use camera::{Camera, CameraGlide};
pub use damage::{DamageKey, needs_render};
pub use gpu::{OverlayGpu, RenderArgs, Renderer, SceneBuffers, SceneCache};
pub use scene::{Plane, PlaneKey, Prim, PrimShape, Scene, SemanticKey, StyleClass};
pub use schematic::schematic_scene;
pub use state::SemanticStates;
pub use style::{ResolvedStyles, StyleTables};
