//! High-performance Entity Component System with archetype-based storage.
//!
//! # Responsibilities
//!
//! - Re-exports all public types for convenient single-import usage (`use ecs_hybrid::*`).
//! - Declares all public modules that compose the ECS library.
//! - Configures the Tracy profiled allocator when the `profiling` feature is active.
//!
//! # Design
//!
//! The crate root is a thin re-export layer. All implementation lives in
//! submodules (`world`, `query`, `scheduler`, etc.). Users import everything
//! from `ecs_hybrid` without needing deep module paths.

// ----------------------------------------------------------------------------
// Tracy ProfiledAllocator - tracks allocations in Tracy's memory view.
// Only active when the `profiling` feature is enabled.
// Sampling rate controlled by `config::TRACY_ALLOC_SAMPLING_RATE`.
// ----------------------------------------------------------------------------
#[cfg(feature = "profiling")]
#[global_allocator]
static ALLOC: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(
        std::alloc::System,
        crate::config::ProfilingConfig::MEMORY_ALLOCATIONS_SAMPLING_FREQUENCY,
    );

// =============================================================================
// Public Modules
// =============================================================================

pub mod api;
pub mod archetype;
pub mod commands;
pub mod component;
pub mod config;
pub mod engine;
pub mod entity;
pub mod persistence;
pub mod profiling;
pub mod query;
#[cfg(feature = "rendering")]
pub mod render;
#[cfg(feature = "rendering")]
pub mod renderer;
pub mod resource;
pub mod scheduler;
pub mod scripting;
pub mod system;
pub mod world;
// =============================================================================
// Public Re-exports
// =============================================================================

pub use api::EngineApi;
pub use commands::{CommandError, Commands};
pub use component::{Component, ComponentId, ComponentTicks, Tick};
pub use engine::Engine;
pub use entity::Entity;
pub use persistence::ComponentSnapshot;
pub use query::{
    Added, BatchStats, Changed, Or, Query, QueryFilter, QueryTarget, Res, ResMut, With, Without,
};
#[cfg(feature = "rendering")]
pub use render::{Color, Position, Sprite, SpriteRenderer};
#[cfg(feature = "rendering")]
pub use renderer::{Renderer, RendererError, RendererWindow};
pub use resource::{ResHandle, Resource};
pub use scheduler::{SystemAccess, SystemScheduler, TypeKey};
pub use scripting::{ScriptComponent, ScriptContext};
pub use serde::{Deserialize, Serialize};
pub use world::{AddComponentError, BuildError, EntityBuilder, RemoveComponentError, World};
