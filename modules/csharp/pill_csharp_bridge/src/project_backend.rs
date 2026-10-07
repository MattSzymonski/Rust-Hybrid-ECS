//! A C# project as the runtime's external project backend.
//!
//! # Responsibilities
//!
//! - Implement `pill_runtime::ProjectBackend` for a shipped C# project, in
//!   either posture: CoreCLR through hostfxr, or a NativeAOT library.
//!
//! # Design
//!
//! The runtime never names the bridge: a C# shipping bundle builds a
//! [`CSharpBackend`] and hands it over as
//! `StaticProjectBackend::External`, and the runtime keeps the returned
//! [`CSharpRuntime`] alive beside the engine. A build that runs no C# therefore
//! links none of this.

// Standard library
use std::any::Any;
use std::path::PathBuf;

// External crates
use pill_core::error::HostError;
use pill_engine::Engine;
use pill_runtime::ProjectBackend;

// Current crate
use crate::mirror_calls::static_mirror_methods;
use crate::{exposed_components_from_names, CSharpModuleConfig, CSharpRuntime};

// =============================================================================
// Types
// =============================================================================

/// How the managed project is hosted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Posture {
    /// CoreCLR booted through hostfxr, loading the runtime and project
    /// assemblies (framework-dependent).
    CoreClr,
    /// A NativeAOT library with a trimmed runtime embedded; no .NET install
    /// and no JIT.
    NativeAot,
}

/// A shipped C# project: where its assemblies are, and how to host them.
///
/// There is no static equivalent of a .NET assembly - the runtime loads it
/// either way - so a shipped project differs from a developed one only in what
/// is not done: nothing is compiled, no C# mirror sources are generated, and
/// the assembly is not watched for replacement.
#[derive(Clone, Debug)]
pub struct CSharpBackend {
    /// CoreCLR or NativeAOT.
    posture: Posture,
    /// Assembly names and output directories, exactly as a reloading build
    /// describes them. In the NativeAOT posture only the project side is used,
    /// and it points at the `dotnet publish` output directory.
    config: CSharpModuleConfig,
    /// Directory the subdirectories in `config` are resolved against.
    ///
    /// A distributed build carries its assemblies beside the executable, which
    /// the backend prefers; this root is the fallback, the workspace the
    /// binary was built in.
    root: PathBuf,
}

// =============================================================================
// Impls
// =============================================================================

impl CSharpBackend {
    /// A project hosted by CoreCLR through hostfxr.
    pub fn coreclr(config: CSharpModuleConfig, root: PathBuf) -> Self {
        Self {
            posture: Posture::CoreClr,
            config,
            root,
        }
    }

    /// A project published with NativeAOT and loaded as a native library.
    pub fn native_aot(config: CSharpModuleConfig, root: PathBuf) -> Self {
        Self {
            posture: Posture::NativeAot,
            config,
            root,
        }
    }
}

impl ProjectBackend for CSharpBackend {
    /// Start .NET (or load the AOT library) and register the project.
    ///
    /// Managed code is given byte-level bindings for every component the
    /// extensions registered. The mirrored Rust functions are this binary's
    /// own: every extension is linked in, so the descriptors they submitted
    /// carry the trampolines' addresses with no module to load.
    fn start(
        &self,
        engine: &mut Engine,
        exposed_component_names: &[String],
    ) -> Result<Box<dyn Any>, HostError> {
        let exposed = exposed_components_from_names(engine.world(), exposed_component_names);
        let mirror_methods = static_mirror_methods();
        let runtime = match self.posture {
            // Nothing to load beside a shipped project.
            Posture::CoreClr => CSharpRuntime::start(
                engine,
                &self.root,
                &self.config,
                &exposed,
                &mirror_methods,
                &mut |_| {},
            )?,
            Posture::NativeAot => CSharpRuntime::start_aot(
                engine,
                &self.root,
                &self.config,
                &exposed,
                &mirror_methods,
            )?,
        };
        Ok(Box::new(runtime))
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Each constructor selects its posture and keeps what it was given.
    #[test]
    fn constructors_select_the_posture() {
        let config = CSharpModuleConfig::new("csharp_runtime", "runtime", "project_cs", "project");
        let coreclr = CSharpBackend::coreclr(config.clone(), PathBuf::from("."));
        let aot = CSharpBackend::native_aot(config, PathBuf::from("."));
        assert_eq!(coreclr.posture, Posture::CoreClr);
        assert_eq!(aot.posture, Posture::NativeAot);
        assert_eq!(aot.config.project_assembly_name, "project_cs");
    }

    /// A name the world does not know resolves to no binding rather than a
    /// guessed one; the C#-facing name is the Rust path with `.` separators.
    #[test]
    fn unknown_exposed_names_are_skipped() {
        let engine = Engine::new();
        let bindings =
            exposed_components_from_names(engine.world(), &["pill_spline::Spline".to_string()]);
        assert!(bindings.is_empty());
    }
}
