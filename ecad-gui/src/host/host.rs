// ECAD provenance: copied from damascene-winit-wgpu @ eef1630 (src/host.rs); see
// ecad-gui/src/host.rs for the full provenance + license note. Local changes
// are marked with `ECAD:` comments.

//! Building blocks for writing a custom winit host.
//!
//! This crate's [`run`](crate::host::run) family owns the whole event loop and
//! is the right entry point for almost every app. A few integrations
//! can't hand over loop ownership — a resident multi-window process
//! spinning windows off one warm instance, portal dialogs, embedding in
//! an existing `ApplicationHandler` — and have to translate winit
//! events and drive `damascene_wgpu::Runner` themselves.
//!
//! The submodules here expose the host's reusable layers so such a
//! custom host doesn't fork-and-drift this crate:
//!
//! - [`input`] — the pure winit → damascene event mappers.
//! - [`color`] — the color-negotiation stack: startup negotiation,
//!   the live `wp_color_management_v1` driver, and the pure
//!   format/luminance helpers.
//! - [`gfx`] — per-window GPU bring-up: [`WindowGfx`] bundles the
//!   surface, swapchain config, `Runner`, color driver, and MSAA
//!   target, built per window on a shared device/queue.
//!
//! The built-in run loop calls through these same functions, so the
//! public surface is the tested path.

pub mod color;
pub mod gfx;
pub mod input;

pub use gfx::WindowGfx;
