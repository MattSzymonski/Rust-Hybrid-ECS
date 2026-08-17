//! Re-exports the profiling API from `pill_core`.
//!
//! # Responsibilities
//!
//! - Makes `crate::profiling::*` resolve to [`pill_core::profiling`] so the
//!   engine keeps a single namespace for both its own code and downstream
//!   users.
//!
//! # Design
//!
//! The implementation, feature gating, and all `#[macro_export]` profiling
//! macros live in `pill_core::profiling`. This module is a pure re-export
//! shim; the engine's own root re-exports (`pub use pill_core::{profile_scope,
//! ...}`) additionally surface the macros at `crate::` scope for the many
//! internal call sites.
pub use pill_core::profiling::*;
