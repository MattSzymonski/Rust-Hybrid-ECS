//! Scheduler-aware C# backend for the native game host.
//!
//! # Responsibilities
//!
//! - Starts the .NET runtime used by managed gameplay assemblies.
//! - Exposes native ECS component storage to managed query iterators.
//! - Converts reflected C# query access into Rust scheduler metadata.
//!
//! # Design
//!
//! [`csharp_runtime`] owns the low-level .NET hosting boundary. The remaining
//! modules separate ABI layout, component registration, scheduled invocation
//! scope, queries, commands, and backend lifecycle. Only [`CSharpRuntime`] is
//! exposed to the parent host module.

/// C-compatible data structures and callback table shared with the managed runtime.
mod abi;
/// High-level C# game startup, discovery, and scheduler registration.
mod backend;
/// Native callbacks that translate C# lifecycle requests into deferred ECS commands.
mod commands;
/// C# component identities, native bindings, and manifest registration.
mod components;
/// Thread-local access scope installed around one scheduled C# system.
mod context;
/// Low-level .NET hosting bootstrap used by the C# game backend.
mod csharp_runtime;
/// Native callbacks used by C# query enumerators.
mod queries;

// =============================================================================
// Types + Impls
// =============================================================================

// The full type documentation lives in the `backend` module; this re-export
// exposes the type as `csharp::CSharpRuntime` so the parent host module has a
// single, stable import path.
pub(crate) use backend::CSharpRuntime;

// =============================================================================
// Tests
// =============================================================================

/// Integration-style unit tests for the native/C# ECS boundary.
#[cfg(test)]
mod tests;
