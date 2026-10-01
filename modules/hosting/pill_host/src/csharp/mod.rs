//! C# development tooling over the `pill_csharp_bridge` backend.
//!
//! # Responsibilities
//!
//! - Generates the C# mirror structs for the components extensions expose.
//! - Recompiles the project in-process with Roslyn on a hot reload.
//! - Pairs the bridge's [`CSharpRuntime`] with that compiler as the host's
//!   managed project, [`CSharpProject`].
//! - Re-exports the bridge items the rest of the host names, under one path.
//!
//! # Design
//!
//! Running C# - hosting .NET, the ABI, components, systems and the reload
//! poll - is the bridge's, and a C# shipping bundle links it without this
//! crate. What stays here only exists while developing: generated mirrors and
//! the compiler. The whole module is `hot_reload` only.

// Standard library
use std::path::Path;

// External crates
use pill_core::error::CSharpError;
use pill_engine::Engine;

/// Host-side generation of the C# mirror structs for exposed module components.
mod codegen;
/// In-process Roslyn compilation of the C# project, replacing MSBuild on reload.
mod fast_compile;

// =============================================================================
// Re-exports
// =============================================================================

pub(crate) use pill_csharp_bridge::{
    accessor_operation_name, accessor_rows, exposed_components_from_names, publish_asset_exports,
    publish_mirror_methods, CSharpRuntime, ModuleExposedComponent, ResolvedFieldAccessor,
    ResolvedMirrorMethod, POLL_RELOADED,
};

/// Generate the C# mirror file for extension components.
pub(crate) use codegen::generate_module_components_csharp;
/// What one in-process compile attempt produced, reported to the reload path.
pub(crate) use fast_compile::FastCompileOutcome;

// =============================================================================
// CSharpProject
// =============================================================================

/// A running C# project in the reloading host: the bridge's runtime plus the
/// in-process compiler that rebuilds its assembly.
pub(crate) struct CSharpProject {
    /// The managed runtime running the project's systems.
    runtime: CSharpRuntime,
    /// The in-process Roslyn compiler, when one could be loaded.
    ///
    /// `None` leaves every reload on the ordinary `dotnet build` path, the
    /// fallback when the compiler cannot be built or loaded.
    fast_compiler: Option<fast_compile::FastCompiler>,
}

impl CSharpProject {
    /// Start the project through the bridge, loading the in-process compiler
    /// into the same .NET runtime as soon as it boots.
    ///
    /// The compiler is loaded before the project starts, not after, so its
    /// background warmup overlaps startup; see [`CSharpRuntime::start`].
    ///
    /// # Errors
    ///
    /// Returns what [`CSharpRuntime::start`] returns. A compiler that cannot be
    /// loaded is not an error: it is logged, and reloads run a full build.
    pub(crate) fn start(
        engine: &mut Engine,
        workspace_root: &Path,
        config: &pill_csharp_bridge::CSharpModuleConfig,
        module_exposed: &[ModuleExposedComponent],
        mirror_methods: &[ResolvedMirrorMethod],
    ) -> Result<Self, CSharpError> {
        let mut fast_compiler = None;
        let runtime = CSharpRuntime::start(
            engine,
            workspace_root,
            config,
            module_exposed,
            mirror_methods,
            &mut |dotnet| {
                fast_compiler = fast_compile::FastCompiler::try_new(dotnet, workspace_root, config);
            },
        )?;
        Ok(Self {
            runtime,
            fast_compiler,
        })
    }

    /// Poll the collectible loader; see [`CSharpRuntime::poll_reload`].
    ///
    /// # Errors
    ///
    /// Returns what [`CSharpRuntime::poll_reload`] returns.
    pub(crate) fn poll_reload(&mut self, engine: &mut Engine) -> Result<u8, CSharpError> {
        self.runtime.poll_reload(engine)
    }

    /// Recompile the project in-process, when a compiler could be loaded.
    ///
    /// `None` means there is no fast path at all and the caller must build
    /// normally; a [`FastCompileOutcome::Unavailable`] means the fast path
    /// exists but cannot answer this particular reload.
    pub(crate) fn fast_compile(
        &self,
        workspace_root: &Path,
        watch_directory: &str,
    ) -> Option<FastCompileOutcome> {
        let outcome = self
            .fast_compiler
            .as_ref()?
            .compile(workspace_root, watch_directory);
        // This compile wrote the assembly itself, so the loader's next poll
        // need not wait for the file to settle.
        if matches!(outcome, FastCompileOutcome::Compiled { .. }) {
            self.runtime.notify_assembly_replaced();
        }
        Some(outcome)
    }
}
