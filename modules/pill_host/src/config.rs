//! Project-module configuration shared by every host frontend.
//!
//! # Responsibilities
//!
//! - Describes how a project module is built, watched, and loaded.
//! - Provides the standard Rust, C#, and integration-test configurations.
//! - Selects a configuration from the host process environment.
//!
//! # Design
//!
//! All configuration is owned by [`ProjectModuleConfig`], whose `const`
//! constructors produce consistent, valid configurations for each supported
//! backend. [`ProjectModuleConfig::validate`] reports the first invalid field so
//! callers can correct it directly, and [`ProjectModuleConfig::from_environment`]
//! keeps backend selection out of executable crates.

// Current crate
use pill_core::error::ConfigError;

// =============================================================================
// Types
// =============================================================================

/// Backend-specific output information for a hot-reloadable project module.
///
/// The variant determines how the host locates and loads the built output:
/// a native shared library or a managed assembly hosted by `csharp_runtime`.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ProjectModuleBackend {
    /// A native shared library exporting `project_init` and `project_update`.
    NativeLibrary {
        /// Library name without the platform prefix or suffix.
        library_name: &'static str,
        /// Output subdirectory relative to the workspace root.
        output_subdirectory: &'static str,
    },
    /// A managed project assembly loaded through the stable `csharp_runtime` host.
    CSharp(CSharpModuleConfig),
}

/// Output locations and assembly names used by the managed project backend.
///
/// The runtime assembly hosts the collectible loader; the project assembly is
/// loaded by the runtime, so both assemblies and their output directories are
/// needed to start the managed module.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct CSharpModuleConfig {
    /// Name of the runtime assembly that hosts the collectible loader.
    pub runtime_assembly_name: &'static str,
    /// Output subdirectory for the runtime assembly, relative to the workspace root.
    pub runtime_output_subdirectory: &'static str,
    /// Name of the project assembly loaded by the runtime.
    pub project_assembly_name: &'static str,
    /// Output subdirectory for the project assembly, relative to the workspace root.
    pub project_output_subdirectory: &'static str,
}

/// Configuration for a hot-reloadable project module.
///
/// Describes how to build the module, where to find its output, and which
/// source directories to watch. Change the fields here to support Rust, C,
/// C++, Zig, or any other language that produces a compatible project module.
///
/// `#[non_exhaustive]` allows new fields to be added without breaking
/// frontends; prefer the provided constructors over struct literals.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ProjectModuleConfig {
    /// Human-readable name used in log messages.
    pub name: &'static str,

    /// Directory to watch for source changes, relative to the workspace root.
    pub watch_directory: &'static str,

    /// Build command whose first element is the program and rest are arguments.
    pub build_command: &'static [&'static str],

    /// How the built module is loaded and executed.
    pub backend: ProjectModuleBackend,
}

impl ProjectModuleConfig {
    /// Verify that the configuration is internally consistent.
    ///
    /// Checks every field that must be non-empty and returns on the first
    /// violation so callers can fix one problem at a time.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ConfigError`] naming the first invalid field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Step 1: Reject an empty name so log messages never reference a blank module.
        if self.name.is_empty() {
            return Err(ConfigError::EmptyModuleName);
        }
        // Step 2: Reject an empty watch directory so the watcher has a real path to monitor.
        if self.watch_directory.is_empty() {
            return Err(ConfigError::EmptyWatchDirectory);
        }
        // Step 3: Reject an empty build command so the runner never spawns a bare process.
        if self.build_command.is_empty() {
            return Err(ConfigError::EmptyBuildCommand);
        }
        Ok(())
    }

    /// Default configuration for a Rust `cdylib` project module built with Cargo.
    ///
    /// When the host is built with the `rendering` feature, the project module is
    /// built with the same feature so both sides share renderer components.
    #[cfg(not(feature = "rendering"))]
    pub const fn rust_default() -> Self {
        Self {
            name: "project-rs",
            watch_directory: "../examples/project_rs/src",
            build_command: &[
                "cargo",
                "build",
                "--manifest-path",
                "../examples/project_rs/Cargo.toml",
                "--package",
                "project",
            ],
            backend: ProjectModuleBackend::NativeLibrary {
                library_name: "project",
                output_subdirectory: "../examples/project_rs/target/debug",
            },
        }
    }

    /// See the non-`rendering` variant above.
    #[cfg(feature = "rendering")]
    pub const fn rust_default() -> Self {
        Self {
            name: "project-rs",
            watch_directory: "../examples/project_rs/src",
            build_command: &[
                "cargo",
                "build",
                "--manifest-path",
                "../examples/project_rs/Cargo.toml",
                "--package",
                "project",
                "--features",
                "rendering",
            ],
            backend: ProjectModuleBackend::NativeLibrary {
                library_name: "project",
                output_subdirectory: "../examples/project_rs/target/debug",
            },
        }
    }

    /// Default scheduler-integrated C# project loaded through `csharp_runtime`.
    pub const fn csharp_default() -> Self {
        Self {
            name: "project-csharp",
            watch_directory: "../examples/project_cs/src",
            build_command: &[
                "dotnet",
                "build",
                "../examples/project_cs/project_cs.csproj",
                "-c",
                "Release",
                "--nologo",
            ],
            backend: ProjectModuleBackend::CSharp(CSharpModuleConfig {
                runtime_assembly_name: "csharp_runtime",
                runtime_output_subdirectory: "pill_csharp_runtime/bin/Release/net8.0",
                project_assembly_name: "project_cs",
                project_output_subdirectory: "../examples/project_cs/bin/Release/net8.0",
            }),
        }
    }

    /// Configuration for the dedicated integration-test project crate.
    pub const fn tests_project() -> Self {
        Self {
            name: "tests-project",
            watch_directory: "tests/project/src",
            build_command: &[
                "cargo",
                "build",
                "--manifest-path",
                "tests/project/Cargo.toml",
            ],
            backend: ProjectModuleBackend::NativeLibrary {
                library_name: "project",
                output_subdirectory: "tests/project/target/debug",
            },
        }
    }

    /// Pick module configuration from the environment, defaulting to Rust.
    ///
    /// Unrecognized values are reported instead of silently launching the
    /// Rust module, so typos cannot hide behind a working default.
    pub fn from_environment() -> Self {
        match std::env::var("ECS_HOT_RELOAD_MODULE") {
            Ok(value) if value.eq_ignore_ascii_case("tests-project") => Self::tests_project(),
            Ok(value)
                if value.eq_ignore_ascii_case("csharp")
                    || value.eq_ignore_ascii_case("project-csharp") =>
            {
                Self::csharp_default()
            }
            Ok(value)
                if value.eq_ignore_ascii_case("rust")
                    || value.eq_ignore_ascii_case("project-rs") =>
            {
                Self::rust_default()
            }
            Ok(value) => {
                eprintln!(
                    "[host] Unknown ECS_HOT_RELOAD_MODULE value {value:?}; using the Rust module. \
                     Expected one of: rust, project-rs, csharp, project-csharp, tests-project."
                );
                Self::rust_default()
            }
            Err(_) => Self::rust_default(),
        }
    }
}
