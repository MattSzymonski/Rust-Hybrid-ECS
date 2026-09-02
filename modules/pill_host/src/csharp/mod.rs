//! Scheduler-aware C# backend for the native project host.
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
/// NativeAOT library loader used by the C# project backend's AOT posture.
mod aot_runtime;
/// High-level C# project startup, discovery, and scheduler registration.
mod backend;
#[cfg(feature = "hot_reload")]
/// Host-side generation of the C# mirror structs for exposed module components.
mod codegen;
/// Native callbacks that translate C# lifecycle requests into deferred ECS commands.
mod commands;
/// C# component identities, native bindings, and manifest registration.
mod components;
/// Thread-local access scope installed around one scheduled C# system.
mod context;
/// Low-level .NET hosting bootstrap used by the C# project backend.
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

/// Aggregate of the native components optional modules exposed to managed code.
pub(crate) use components::ModuleExposedComponent;

/// One mirrored Rust method resolved to a callable address, shared by the
/// host's module loader and the C# backend. Defined here (not in the
/// `hot_reload`-gated `native_library` module) because the C# backend is
/// compiled in every host configuration.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedMirrorMethod {
    /// Fully-qualified Rust type name the method belongs to.
    pub(crate) type_name: String,
    /// Rust method name, snake_case.
    pub(crate) method_name: String,
    /// Return type tag; empty for a `()` return.
    pub(crate) return_tag: String,
    /// Argument type tags, in declaration order.
    pub(crate) arg_tags: Vec<String>,
    /// Address of the exported C-ABI trampoline.
    pub(crate) address: usize,
}

#[cfg(feature = "hot_reload")]
/// Generate the C# mirror file for optional-module components.
pub(crate) use codegen::generate_module_components_csharp;

/// Rebuild the mirror-method table the managed runtime reads, after an
/// optional module reload changes its trampoline addresses or method set.
pub(crate) use abi::publish_mirror_methods;

// =============================================================================
// Tests
// =============================================================================

/// Integration-style unit tests for the native/C# ECS boundary.
#[cfg(test)]
mod tests;
