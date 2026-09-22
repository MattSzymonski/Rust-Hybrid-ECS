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
#[cfg(feature = "hot_reload")]
/// In-process Roslyn compilation of the C# project, replacing MSBuild on reload.
mod fast_compile;
/// Thread-local access scope installed around one scheduled C# system.
mod context;
/// Low-level .NET hosting bootstrap used by the C# project backend.
mod csharp_runtime;
/// The two-call protocol every managed payload crosses the boundary through.
mod managed_buffer;
/// C# component manifest schema, field validation, and engine type mapping.
mod manifest;
/// The one apply pipeline every manifest kind runs through.
mod manifest_apply;
/// Native callbacks used by C# query enumerators.
mod queries;
/// Managed resource registration and the callback that serves resource bytes.
mod resources;

// =============================================================================
// Types + Impls
// =============================================================================

// The full type documentation lives in the `backend` module; this re-export
// exposes the type as `csharp::CSharpRuntime` so the parent host module has a
// single, stable import path.
pub(crate) use backend::CSharpRuntime;
#[cfg(feature = "hot_reload")]
pub(crate) use backend::POLL_RELOADED;

/// What one in-process compile attempt produced, reported to the reload path.
#[cfg(feature = "hot_reload")]
pub(crate) use fast_compile::FastCompileOutcome;

/// Aggregate of the native components extensions exposed to managed code.
pub(crate) use components::{resolve_exposed_component_id, ModuleExposedComponent};

/// One mirrored Rust method resolved to a callable address, shared by the
/// host's module loader and the C# backend. Defined here (not in the
/// `hot_reload`-gated `native_library` module) because the C# backend is
/// compiled in every host configuration.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
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
    /// Argument names from the Rust source, in declaration order (`alpha`,
    /// `beta`), so the generated C# mirror names its parameters identically.
    /// Parallel to `arg_tags`.
    pub(crate) arg_names: Vec<String>,
    /// Address of the exported C-ABI trampoline.
    pub(crate) address: usize,
}

/// One heap-field accessor resolved to callable addresses, shared by the
/// host's module loader, the C# mirror codegen, and the managed runtime's
/// method table.
#[derive(Clone, Debug)]
#[cfg(feature = "hot_reload")]
pub(crate) struct ResolvedFieldAccessor {
    /// Fully-qualified component type name the field belongs to.
    pub(crate) type_name: String,
    /// Rust field name, snake_case.
    pub(crate) field_name: String,
    /// Container kind: `"vec"`, `"dynbuf"`, `"string"`, or `"vecstring"`.
    pub(crate) kind: String,
    /// Element type tag of a `vec` field; `string` for a `vecstring` field;
    /// empty for a `string` field.
    pub(crate) element_tag: String,
    /// Address of the view trampoline; `None` for a `vecstring` field, whose
    /// elements are individually allocated and cannot be viewed as one run.
    pub(crate) view_address: Option<usize>,
    /// Address of the resize trampoline for a `vec`, `dynbuf` or `vecstring`
    /// field.
    pub(crate) resize_address: Option<usize>,
    /// Address of the replace-in-place trampoline for a `string` field.
    pub(crate) set_address: Option<usize>,
    /// Address of the per-element view trampoline for a `vecstring` field.
    pub(crate) item_address: Option<usize>,
    /// Address of the per-element replace trampoline for a `vecstring` field.
    pub(crate) set_item_address: Option<usize>,
    /// Address of the append trampoline for a `vecstring` field.
    pub(crate) push_address: Option<usize>,
}

/// The operation name a generated `MirrorMethods.Resolve` call and the host's
/// accessor rows agree on for one operation of a heap field.
///
/// Defined once so the codegen (which writes the name into C#) and the table
/// builder (which registers it) can never drift; `operation` is `view`,
/// `resize`, `set`, `item`, `set_item`, or `push`.
#[cfg(feature = "hot_reload")]
pub(crate) fn accessor_operation_name(field_name: &str, operation: &str) -> String {
    format!("{field_name}_{operation}")
}

/// Convert heap-field accessors into mirror-method rows, so the managed
/// runtime resolves them through the same table — and the same per-reload
/// refresh — as mirrored value-type methods.
#[cfg(feature = "hot_reload")]
pub(crate) fn accessor_rows(accessors: &[ResolvedFieldAccessor]) -> Vec<ResolvedMirrorMethod> {
    let mut rows = Vec::with_capacity(accessors.len() * 6);
    for accessor in accessors {
        let mut push = |operation: &str, address: Option<usize>| {
            if let Some(address) = address {
                rows.push(ResolvedMirrorMethod {
                    type_name: accessor.type_name.clone(),
                    method_name: accessor_operation_name(&accessor.field_name, operation),
                    return_tag: String::new(),
                    arg_tags: Vec::new(),
                    arg_names: Vec::new(),
                    address,
                });
            }
        };
        push("view", accessor.view_address);
        push("resize", accessor.resize_address);
        push("set", accessor.set_address);
        push("item", accessor.item_address);
        push("set_item", accessor.set_item_address);
        push("push", accessor.push_address);
    }
    rows
}

#[cfg(feature = "hot_reload")]
/// Generate the C# mirror file for extension components.
pub(crate) use codegen::generate_module_components_csharp;

/// Rebuild the mirror-method table the managed runtime reads, after an
/// extension reload changes its trampoline addresses or method set.
#[cfg_attr(not(feature = "hot_reload"), allow(unused_imports))]
pub(crate) use abi::publish_mirror_methods;

// =============================================================================
// Tests
// =============================================================================

/// Integration-style unit tests for the native/C# ECS boundary.
///
/// Gated on `rendering` because the fixtures are the renderer's own components
/// (`Position`, `Sprite`, `Color`): they are the shared-ABI types the managed
/// side mirrors, so they are what these tests must exercise, and they live in
/// `pill_master_renderer`, which only a windowed host links. `cargo test` on the
/// editor or any windowed frontend runs them; `--no-default-features` does not.
#[cfg(all(test, feature = "rendering"))]
mod tests;
