//! View submodules that extend `impl Scratchpad` with cohesive method groups.
//!
//! Declaring them under `app` (rather than as crate-root siblings) lets each
//! `impl Scratchpad` block access the view's private fields and helpers.

mod find;
pub(crate) mod geom;
mod hex;
mod mouse;
mod overlays;
mod paint;
mod palette;
mod scroll;
mod status;
