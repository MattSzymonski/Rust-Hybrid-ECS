//! The Rust half of the C#/Rust ABI: runs a C# project inside the engine.
//!
//! # Responsibilities
//!
//! - Starts the .NET runtime used by managed gameplay assemblies (CoreCLR
//!   through hostfxr, or a NativeAOT library).
//! - Exposes native ECS component storage to managed query iterators.
//! - Converts reflected C# query access into Rust scheduler metadata.
//! - With `hot_reload`, swaps a rebuilt project assembly into the running
//!   engine.
//!
//! # Design
//!
//! `pill_csharp_runtime` is the managed half of the same boundary, and the
//! two change together: [`INTEROP_CONTRACT_VERSION`] is checked from both
//! sides when the runtime starts. [`csharp_runtime`] owns the low-level .NET
//! hosting boundary. The remaining modules separate ABI layout, component
//! registration, scheduled invocation scope, queries, commands, and backend
//! lifecycle; [`CSharpRuntime`] is the entry point.
//!
//! Development tooling - mirror generation and the in-process Roslyn
//! compiler - lives in `pill_host`, which builds on what is exported here.
//! Nothing in this crate reads project settings, builds or watches anything.
//! A shipped C# project reaches the runtime as a [`CSharpBackend`], which
//! implements `pill_runtime::ProjectBackend`: the bridge depends on the
//! runtime, never the other way round.

/// C-compatible data structures and callback table shared with the managed runtime.
mod abi;
/// NativeAOT library loader used by the C# project backend's AOT posture.
mod aot_runtime;
/// High-level C# project startup, discovery, and scheduler registration.
mod backend;
/// Native callbacks that translate C# lifecycle requests into deferred ECS commands.
mod commands;
/// C# component identities, native bindings, and manifest registration.
mod components;
/// Output locations and assembly names of a managed project.
mod config;
/// Thread-local access scope installed around one scheduled C# system.
mod context;
/// Low-level .NET hosting bootstrap used by the C# project backend.
mod csharp_runtime;
/// Read-only frame state for managed code: input and time.
mod frame_state;
/// The two-call protocol every managed payload crosses the boundary through.
mod managed_buffer;
/// C# component manifest schema, field validation, and engine type mapping.
mod manifest;
/// The one apply pipeline every manifest kind runs through.
mod manifest_apply;
/// The native calls the mirror ABI needs beside the trampolines: text
/// channels, Rust-owned resources, and the asset store.
mod mirror_calls;
/// The rules that name a generated mirror's fields, for generation and binding checks.
mod mirror_naming;
/// Parallel dispatch of managed callbacks onto the shared Rayon pool.
mod parallel;
/// A shipped C# project as the runtime's external project backend.
mod project_backend;
/// Native callbacks used by C# query enumerators.
mod queries;
/// Managed resource registration and the callback that serves resource bytes.
mod resources;

// =============================================================================
// Types + Impls
// =============================================================================

// The full type documentation lives in the `backend` module.
pub use backend::{CSharpRuntime, INTEROP_CONTRACT_VERSION};
/// The reload poll's status codes.
#[cfg(feature = "hot_reload")]
pub use backend::{POLL_MANIFEST_PENDING, POLL_NO_CHANGE, POLL_REJECTED, POLL_RELOADED};
pub use config::CSharpModuleConfig;
/// The booted CoreCLR runtime, which development tooling (the in-process
/// compiler) loads its own assemblies into.
pub use csharp_runtime::DotnetRuntimeContext;
/// The two-call protocol every managed payload crosses the boundary through.
pub use managed_buffer::{fetch_managed_buffer, ManagedBufferError};
/// The naming rules a generated mirror follows, shared by the generator in
/// `pill_host` and the binding check here.
pub use mirror_naming::{is_opaque_container_tag, snake_to_pascal, split_array_tag};
/// A shipped C# project, started through `pill_runtime`'s external backend.
pub use project_backend::CSharpBackend;

/// Resolve registered component names into the layouts managed code binds.
pub use components::exposed_components_from_names;
/// Aggregate of the native components exposed to managed code.
pub use components::ModuleExposedComponent;

/// One mirrored Rust method resolved to a callable address, shared by the
/// host's module loader and the C# backend. Defined here (not in the host's
/// `hot_reload`-gated module loader) because the C# backend is compiled in
/// every configuration.
#[derive(Clone, Debug)]
pub struct ResolvedMirrorMethod {
    /// Fully-qualified Rust type name the method belongs to - or, for a free
    /// function (`is_free_function`), the path of the module declaring it.
    pub type_name: String,
    /// Rust method name, snake_case.
    pub method_name: String,
    /// Return type tag; empty for a `()` return.
    pub return_tag: String,
    /// Argument type tags, in declaration order.
    pub arg_tags: Vec<String>,
    /// Argument names from the Rust source, in declaration order (`alpha`,
    /// `beta`), so the generated C# mirror names its parameters identically.
    /// Parallel to `arg_tags`.
    pub arg_names: Vec<String>,
    /// Address of the C-ABI trampoline; zero for a type row with none.
    pub address: usize,
    /// Whether the declaration is a `#[pill_mirror_fn]` free function; the
    /// codegen then emits a static method on a static class named after the
    /// declaring module instead of an instance method on a struct mirror.
    pub is_free_function: bool,
    /// The package that declared the method, from `env!("CARGO_PKG_NAME")` at
    /// the macro's expansion site. The host keeps only the entries whose
    /// `crate_name` matches the module being generated or published, so one
    /// artifact linking another crate's code cannot claim its declarations.
    pub crate_name: String,
    /// How the function takes its owner: `""`, `"ref"`, `"mut"` or `"value"`.
    pub receiver: String,
    /// What the owning type is, when the declaring attribute knew it:
    /// `"object"`, `"enum"`, `"resource"`, `"module"`, or empty for a method
    /// whose type the codegen resolves from the type rows.
    pub owner_kind: String,
}

/// One heap-field accessor resolved to callable addresses, shared by the
/// host's module loader, the C# mirror codegen, and the managed runtime's
/// method table.
#[derive(Clone, Debug)]
#[cfg(feature = "hot_reload")]
pub struct ResolvedFieldAccessor {
    /// Fully-qualified component type name the field belongs to.
    pub type_name: String,
    /// Rust field name, snake_case.
    pub field_name: String,
    /// Container kind: `"vec"`, `"dynbuf"`, `"string"`, or `"vecstring"`.
    pub kind: String,
    /// Element type tag of a `vec` field; `string` for a `vecstring` field;
    /// empty for a `string` field.
    pub element_tag: String,
    /// Address of the view trampoline; `None` for a `vecstring` field, whose
    /// elements are individually allocated and cannot be viewed as one run.
    pub view_address: Option<usize>,
    /// Address of the resize trampoline for a `vec`, `dynbuf` or `vecstring`
    /// field.
    pub resize_address: Option<usize>,
    /// Address of the replace-in-place trampoline for a `string` field.
    pub set_address: Option<usize>,
    /// Address of the per-element view trampoline for a `vecstring` field.
    pub item_address: Option<usize>,
    /// Address of the per-element replace trampoline for a `vecstring` field.
    pub set_item_address: Option<usize>,
    /// Address of the append trampoline for a `vecstring` field.
    pub push_address: Option<usize>,
}

/// The operation name a generated `MirrorMethods.Resolve` call and the host's
/// accessor rows agree on for one operation of a heap field.
///
/// Defined once so the codegen (which writes the name into C#) and the table
/// builder (which registers it) can never drift; `operation` is `view`,
/// `resize`, `set`, `item`, `set_item`, or `push`.
#[cfg(feature = "hot_reload")]
pub fn accessor_operation_name(field_name: &str, operation: &str) -> String {
    format!("{field_name}_{operation}")
}

/// Convert heap-field accessors into mirror-method rows, so the managed
/// runtime resolves them through the same table — and the same per-reload
/// refresh — as mirrored value-type methods.
#[cfg(feature = "hot_reload")]
pub fn accessor_rows(accessors: &[ResolvedFieldAccessor]) -> Vec<ResolvedMirrorMethod> {
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
                    is_free_function: false,
                    crate_name: String::new(),
                    receiver: String::new(),
                    owner_kind: String::new(),
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

/// Rebuild the mirror-method table the managed runtime reads, after an
/// extension reload changes its trampoline addresses or method set.
pub use abi::publish_mirror_methods;

// =============================================================================
// Tests
// =============================================================================

/// Integration-style unit tests for the native/C# ECS boundary.
///
/// The fixtures are the shared-ABI components the managed side mirrors
/// (`Position`, `MeshRendererComponent`, `Color`). They come from `pill_engine`
/// and the renderer data crate (a dev-dependency), so these run in every
/// build, headless included.
#[cfg(test)]
mod tests;
