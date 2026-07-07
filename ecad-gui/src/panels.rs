//! The right-sidebar panels — one module per panel region (properties inspector,
//! findings, explorer, layers) plus the sidebar composition over them. These are
//! the `build`-time El builders; the domain snapshot logic they project lives in
//! [`crate::inspector`] / [`crate::explorer`] / [`crate::findings`]. Moved out of
//! `app/panels.rs` as pure code motion (gui-module-split).

pub(crate) mod explorer;
pub(crate) mod findings;
pub(crate) mod layers;
pub(crate) mod properties;
pub(crate) mod sidebar;
