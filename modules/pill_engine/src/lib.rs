//! High-performance Entity Component System with archetype-based storage.
//!
//! # Responsibilities
//!
//! - Re-exports all public types for convenient single-import usage (`use pill_engine::*`).
//! - Declares all public modules that compose the ECS library.
//! - Configures the Tracy profiled allocator when the `profiling` feature is active.
//!
//! # Design
//!
//! The crate root is a thin re-export layer. All implementation lives in
//! submodules ([`world`], [`query`], [`scheduler`], etc.). Users import
//! everything from `pill_engine` without needing deep module paths.

// ===== Constants =====

/// Tracy profiled allocator that tracks allocations in Tracy's memory view.
///
/// Only active when the `profiling` feature is enabled. The sampling rate is
/// controlled by [`crate::config::ProfilingConfig::MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY`].
#[cfg(feature = "profiling")]
#[global_allocator]
static ALLOC: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(
        std::alloc::System,
        crate::config::ProfilingConfig::MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY,
    );

// ===== Public Modules =====

/// Language-agnostic engine API for external hot-reloadable project consumers.
pub mod api;

/// Archetype-based component storage with structure-of-arrays layout.
pub mod archetype;

/// Deferred command queue for structural ECS mutations.
pub mod commands;

/// Component trait, type identification, and change-detection primitives.
pub mod component;

/// Compile-time component registry driven by `#[derive(PillComponent)]`.
pub mod component_registry;

/// Generic type-erased component field access for editor-style tools.
pub mod component_field;

/// Centralised configuration constants and hardware detection.
pub mod config;

/// System registration, frame execution, and parallel dispatch orchestration.
pub mod engine;

/// Lightweight entity handles with generation-based invalidation.
pub mod entity;

/// Typed error system for the ECS engine.
pub mod error;

/// Constants shared between the host and optional engine modules.
pub mod module_abi;

/// Component persistence and schema migration for hot-reload.
pub mod persistence;

/// Re-exports the profiling API from `pill_core`.
pub mod profiling;

/// Query system for efficient iteration over entities with specific components.
pub mod query;

/// Minimal 2D sprite renderer (wgpu-backed), gated behind the `rendering` feature.
#[cfg(feature = "rendering")]
pub mod render;

/// Window-surface renderer owned by the engine's optional rendering feature.
#[cfg(feature = "rendering")]
pub mod renderer;

/// Singleton resources stored in the [`World`], not attached to entities.
pub mod resource;

/// Dependency analysis and parallel batch scheduling for system execution.
pub mod scheduler;

/// Script components with deferred structural mutation safety.
pub mod scripting;

/// Per-function hot patching: stable dispatch slots for registered systems.
pub mod hot_patch;

/// Advanced system parameter infrastructure with automatic parameter resolution.
pub mod system;

/// Central ECS state container - entities, archetypes, components, and resources.
pub mod world;
// ===== Public Re-exports =====

// Core engine types re-exported for single-import usage.
pub use api::EngineApi;
pub use commands::{CommandError, Commands};
pub use component::{Component, ComponentId, ComponentTicks, Tick};
pub use component_field::{ComponentFieldError, FieldValue};
pub use engine::{Engine, SystemOwner, SystemSnapshot};
pub use entity::Entity;
pub use error::{EngineError, SystemError, SystemFailure};
pub use hot_patch::{
    HotPatchError, HotPatchRegistry, HotSlot, PillHotFunctionDescriptor, PillHotSlotDescriptor,
    PlainSlot,
};
pub use persistence::ComponentSnapshot;
pub use query::{
    Added, BatchStats, Changed, Or, Query, QueryFilter, QueryTarget, Res, ResMut, With, Without,
};
#[cfg(feature = "rendering")]
pub use render::{Color, Position, RenderViewport, Sprite, SpriteRenderer, VirtualResolution};
#[cfg(feature = "rendering")]
pub use renderer::{Renderer, RendererError, RendererWindow};
pub use resource::{ResHandle, Resource};
pub use scheduler::{SystemAccess, SystemScheduler, TypeKey};
pub use scripting::{ScriptComponent, ScriptContext};

// Serde derives re-exported so downstream components can derive serialization without a direct dependency.
pub use serde::{Deserialize, Serialize};

// Tracing re-exported to keep telemetry under a single flat namespace.
pub use tracing;

// Registration macros + the inventory submit macro they expand to, re-exported
// so module/project crates need no dependency beyond `pill_engine` itself.
pub use inventory::submit;
pub use pill_engine_macros::{
    pill_hot, pill_hot_fn, pill_hot_resolver, pill_mirror_impl, pill_mirror_method, pill_module,
    pill_project, PillComponent, PillMirror,
};

// World container and its entity-builder and error types.
pub use world::{
    AddComponentError, BuildError, EntityBuilder, EntityRow, RemoveComponentError, World,
};

// ----------------------------------------------------------------------------
// Profiling macro re-exports
//
// The profiling implementation and all its `#[macro_export]` macros live in
// `pill_core::profiling`. Re-exporting the macros at the crate root keeps the
// 100+ `crate::profile_scope!` call sites inside this crate compiling while
// preserving a single flat namespace for downstream users.
// ----------------------------------------------------------------------------
pub use pill_core::{
    profile_error, profile_frame_mark, profile_init, profile_message, profile_non_continuous_frame,
    profile_plot, profile_plot_config, profile_scope, profile_scope_detail, profile_scope_fine,
    profile_secondary_frame_mark, profile_thread, profile_warn,
};
