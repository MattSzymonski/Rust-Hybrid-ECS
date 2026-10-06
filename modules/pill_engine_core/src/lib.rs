//! The engine core: an archetype-based Entity Component System and everything
//! built on it.
//!
//! # Responsibilities
//!
//! - Declares all public modules that compose the engine: world, queries,
//!   scheduler, assets, persistence, hot-patch machinery.
//! - Re-exports the public types at the crate root.
//! - Configures the Tracy profiled allocator when the `profiling-memory`
//!   feature is active.
//!
//! # Design
//!
//! Nothing outside the engine names this crate. Projects, extensions and the
//! macros use `pill_engine`, a facade that re-exports everything here under
//! the same paths and adds the per-DLL `inventory` registries. This crate holds
//! what is the same for every DLL, and never reads a registry: what it needs
//! from one, the calling DLL passes in. It must never depend on `pill_engine`.
//!
//! The module ABI is **Rust-to-Rust by design**: a loaded project or module
//! receives an [`EngineApi`] carrying a pointer to the host's engine and calls
//! the typed API through it. There is no language-neutral plugin table - see
//! [`api`] for why one was removed rather than completed.
// The derive macros expand to `::pill_engine::...` paths, so the crate itself
// needs that name in scope to use its own derives - `Position` and `Color`
// derive `PillLayout` here rather than carrying hand-written offsets. Only
// `PillLayout` can be used here: the other derives submit into registries,
// which live in the facade.
extern crate self as pill_engine;
// ===== Constants =====

/// Tracy profiled allocator that tracks allocations in Tracy's memory view.
///
/// Only active with the opt-in `profiling-memory` feature. The sampling rate is
/// controlled by [`crate::config::ProfilingConfig::MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY`].
///
/// Not part of `profiling`: the allocator is generic, so it is instantiated in
/// every DLL that links this rlib and calls Tracy's C entry points directly.
/// Those live inside `pill_core.dll`, which exports only Rust symbols, so any
/// extension DLL fails to link with "undefined symbol ___tracy_emit_memory_*".
#[cfg(feature = "profiling-memory")]
#[global_allocator]
static ALLOC: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(
        std::alloc::System,
        crate::config::ProfilingConfig::MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY,
    );

// ===== Public Modules =====

/// The module entry-point contract passed to hot-reloadable artifacts.
pub mod api;

/// Archetype-based component storage with structure-of-arrays layout.
pub mod archetype;

/// Many-per-type asset storage addressed by generational handle.
///
/// The counterpart to [`resource`] for data a world holds several of - meshes,
/// textures, materials - where the type alone does not name one value. The
/// store is itself a resource, so it reaches systems through the existing
/// `Res` / `ResMut` parameters.
pub mod asset;

/// The C ABI of importing an asset, shared by the C# bridge's import exports.
pub mod asset_ffi;

/// Importing assets by file extension, for code that does not know their type.
pub mod asset_import_registry;

/// Metadata files beside an asset's source (`<asset_name>.meta`).
pub mod asset_metadata;

/// Saved references to assets, by guid, that resolve in a later run.
pub mod asset_reference;

/// Assets stored as their own file in `res`, with no source file.
pub mod asset_standalone;

/// Where asset paths resolve: mounted packs and the filesystem.
pub mod asset_store;

/// Deferred command queue for structural ECS mutations.
pub mod commands;

/// Component trait, type identification, and change-detection primitives.
pub mod component;

/// Compile-time component registry driven by `#[derive(PillComponent)]`.
pub mod component_registry;

/// Components the engine defines because more than one consumer needs them.
pub mod common_components;

/// Generic type-erased component field access for editor-style tools.
pub mod component_field;

/// Centralised configuration constants and hardware detection.
pub mod config;

/// Periodic ECS state report, registered by the engine and printed every N
/// frames.
pub mod diagnostics;

/// Engine-owned native dynamic buffer, re-exported from `pill_core`.
///
/// The type lives beside the allocation service that owns its blocks, so a
/// module can name it without depending on the whole ECS. Re-exported here
/// because a component field is what it is for, and every existing
/// `pill_engine::DynamicBuffer` import keeps resolving.
pub mod dynamic_buffer {
    pub use pill_core::dynamic_buffer::*;
}

/// System registration, frame execution, and parallel dispatch orchestration.
pub mod engine;

/// Lightweight entity handles with generation-based invalidation.
pub mod entity;

/// Typed error system for the ECS engine.
pub mod error;

/// Constants shared between the host and extensions.
pub mod module_abi;

/// Component persistence and schema migration for hot-reload.
pub mod persistence;

/// Re-exports the profiling API from `pill_core`.
pub mod profiling;

/// Query system for efficient iteration over entities with specific components.
pub mod query;

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

/// Keyboard, mouse and gamepad state maintained by the engine and read through
/// `Res<Input>`.
pub mod input;

/// Frame timing maintained by the engine and read through `Res<Time>`.
pub mod time;

/// Type names as the rest of the engine sees them: `pill_engine::...` for the
/// engine's own types.
pub mod type_names;

/// Central ECS state container - entities, archetypes, components, and resources.
pub mod world;
// ===== Public Re-exports =====

// Core engine types re-exported for single-import usage.
pub use api::EngineApi;
pub use asset::{
    Asset, AssetBindingError, AssetBindingResult, AssetGuid, AssetLoadError, AssetLoadResult,
    AssetLoader, AssetManager, Handle,
};
pub use asset_import_registry::{
    ErasedImportError, ErasedImportOutcome, ImportRegistrationError, ImportRegistry, ScanReport,
};
pub use asset_metadata::{
    AssetImport, AssetImportError, ImportOutcome, ImportedAsset, MetadataPolicy, MetadataSource,
    ReimportOutcome,
};
pub use asset_reference::AssetReference;
pub use asset_standalone::{render_standalone, StandaloneAsset};
pub use commands::{CommandError, Commands};
pub use common_components::{register_common_components, Color, Position, TransformComponent};
pub use component::{Component, ComponentId, ComponentTicks, Tick};
pub use component_field::{ComponentFieldError, FieldValue};
pub use engine::{Engine, SystemOwner, SystemSnapshot};
pub use entity::Entity;
pub use error::{EngineError, SystemError, SystemFailure};
pub use hot_patch::{HotPatchError, HotPatchRegistry, HotSlot, PlainSlot};
pub use input::{
    ButtonState, GamepadAxis, GamepadButton, Input, InputEvent, KeyCode, MouseButton, PlayerId,
    RumbleRequest, ScrollDelta,
};
pub use persistence::{PersistResourceManifestEntry, ResourceSnapshot};
pub use pill_core::DynamicBuffer;
pub use query::{
    Added, BatchStats, Changed, Or, Query, QueryFilter, QueryTarget, Res, ResMut, With, Without,
};
pub use resource::{ResHandle, Resource, ResourceId};
pub use scheduler::{SystemAccess, SystemScheduler, TypeKey};
pub use scripting::{ScriptComponent, ScriptContext};
pub use time::Time;

// Serde derives re-exported so downstream components can derive serialization without a direct dependency.
pub use serde::{Deserialize, Serialize};

// Tracing re-exported to keep telemetry under a single flat namespace.
pub use tracing;

// Registration macros, re-exported here so the facade's glob re-export carries
// them. The `inventory::submit` they expand to is re-exported by the facade,
// which owns the registries.
pub use pill_engine_macros::{
    pill_hot, pill_hot_fn, pill_hot_resolver, pill_mirror_fn, pill_mirror_impl, pill_mirror_method,
    pill_module, pill_project, pill_value_type, PillComponent, PillLayout, PillMirror,
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
