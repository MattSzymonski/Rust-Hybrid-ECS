//! The hot-reloadable engine runtime, loaded by the host as a dynamic library.
//!
//! # Responsibilities
//!
//! - Own the [`pill_engine`] instance, its renderer, and the loaded project
//!   module for one engine generation.
//! - Export the C-ABI table declared by [`pill_runtime_api`] so the host can
//!   drive frames without ever linking the engine.
//! - Capture and restore world state so a generation swap preserves entities,
//!   persistable components, and persistable resources.
//! - Route its own logs, metrics, and profiling zones back into the host's
//!   single telemetry pipeline.
//!
//! # Design
//!
//! This crate is the reload unit. Everything an engine rebuild invalidates
//! lives here: the ECS, the GPU state, the project module whose `EngineApi`
//! function pointers address this binary, and the managed loader that hosts a
//! C# project. Everything that must survive a swap stays in the host: the
//! window, the file watchers, the build runner, and the telemetry subscriber.
//!
//! The project module is loaded *by the runtime* rather than by the host,
//! because a project holds function pointers into this binary's code. Engine
//! and project therefore always move together and no reload can ever mix two
//! generations.
//!
//! The library is built as a `cdylib` for that hot-reload path and as an
//! `rlib` so tests can drive the same code in-process, without `libloading`,
//! and observe it directly.

// ===== Module Declarations =====

/// The exported C-ABI surface of the engine runtime dynamic library.
mod abi;
/// Scheduler-aware C# backend for the project host.
mod csharp;
/// Native project-library loading and Windows-safe temporary-copy handling.
mod native_library;
/// Runtime-side description of the project module to load.
mod project;
/// Lifecycle management for the active native or managed project module.
mod project_module;
/// One live engine generation: the world, its project, and its renderer.
mod runtime;
/// World-state capture and restore across an engine runtime swap.
mod state;
/// Routing of runtime telemetry back into the host's single pipeline.
mod telemetry;

// ===== Re-exports =====

// The exported accessor is public so an in-process build can call it directly
// instead of resolving it through the dynamic loader.
pub use abi::get_pill_runtime_api_v1;
