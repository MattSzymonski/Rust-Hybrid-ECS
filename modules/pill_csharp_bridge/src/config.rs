//! Where a managed project's assemblies are, and what they are called.
//!
//! # Responsibilities
//!
//! - Names the runtime and project assemblies and their output directories,
//!   which is everything the backend needs to start a managed project.
//!
//! # Design
//!
//! A reloading host derives these values from the project's `.csproj`; a
//! shipping bundle states them directly. Neither source is read here: this
//! crate never parses project settings.

// =============================================================================
// Types
// =============================================================================

/// Output locations and assembly names used by the managed project backend.
///
/// The runtime assembly hosts the collectible loader; the project assembly is
/// loaded by the runtime, so both assemblies and their output directories are
/// needed to start the managed module.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct CSharpModuleConfig {
    /// Name of the runtime assembly that hosts the collectible loader.
    pub runtime_assembly_name: String,
    /// Output subdirectory for the runtime assembly, relative to the workspace root.
    pub runtime_output_subdirectory: String,
    /// Name of the project assembly loaded by the runtime.
    pub project_assembly_name: String,
    /// Output subdirectory for the project assembly, relative to the workspace root.
    pub project_output_subdirectory: String,
}

impl CSharpModuleConfig {
    /// Describe a managed project by its assembly names and output directories.
    ///
    /// A reloading build gets this from
    /// `pill_host`'s `ProjectModuleConfig::from_environment`, which reads the project's
    /// `.csproj`. A shipping build has no project path to read, so its frontend
    /// states the same four values directly - which is also why this type needs
    /// a constructor at all: it is `#[non_exhaustive]`, so it cannot be built
    /// with a struct expression from another crate.
    ///
    /// The two subdirectories are relative to the root the caller supplies with
    /// them, not to any fixed location.
    pub fn new(
        runtime_assembly_name: impl Into<String>,
        runtime_output_subdirectory: impl Into<String>,
        project_assembly_name: impl Into<String>,
        project_output_subdirectory: impl Into<String>,
    ) -> Self {
        Self {
            runtime_assembly_name: runtime_assembly_name.into(),
            runtime_output_subdirectory: runtime_output_subdirectory.into(),
            project_assembly_name: project_assembly_name.into(),
            project_output_subdirectory: project_output_subdirectory.into(),
        }
    }
}
