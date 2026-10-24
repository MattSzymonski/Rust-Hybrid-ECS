//! The Pill engine, as projects and extensions see it.
//!
//! # Responsibilities
//!
//! - Re-export the engine core (`pill_engine_core`) under the same paths, so
//!   `use pill_engine::*` and every `::pill_engine::...` path the macros emit
//!   resolve as before.
//! - Own the per-DLL `inventory` registries ([`component_registry`],
//!   [`hot_patch`]) and re-export the `submit!` macro the derives expand to.
//!
//! # Design
//!
//! This crate is an `rlib` compiled into every DLL, while the engine core holds
//! what is the same for every DLL. The registries are here because their
//! questions are per DLL ("every component this artifact declares", "the
//! function's address in this artifact"): each DLL's copy of this crate reads
//! that DLL's own lists. The core never reads them, and never depends on this
//! crate; what it needs from a registry, code here passes in.

// The derive macros expand to `::pill_engine::...` paths; this lets the
// crate's own tests use them.
extern crate self as pill_engine;

/// Everything in the engine core, under the paths it has always had.
pub use pill_engine_core::*;

/// Per-DLL component, value-type, method, accessor and export registries.
pub mod component_registry;

/// Per-DLL hot-patch registries over the core's hot-patch machinery.
pub mod hot_patch;

// The descriptor types the hot-patch macros submit, at the crate root as
// before.
pub use hot_patch::{PillHotFunctionDescriptor, PillHotSlotDescriptor};

// The inventory submit macro the derives and attribute macros expand to, so
// module and project crates need no dependency beyond `pill_engine` itself.
pub use inventory::submit;
