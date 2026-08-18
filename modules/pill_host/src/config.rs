//! Project-module configuration shared by every host frontend.
//!
//! # Responsibilities
//!
//! - Describes how a project module is built, watched, and loaded.
//! - Infers the complete configuration from a single project directory.
//! - Validates the assembled configuration before any build work starts.
//!
//! # Design
//!
//! [`ProjectModuleConfig`] owns every configured value and is built by
//! [`ProjectModuleConfig::from_environment`] from one variable, `PROJECT_PATH`,
//! which names the project directory relative to the workspace root. The
//! backend (native Rust or managed C#) is detected from the manifest files in
//! that directory (`Cargo.toml` or `*.csproj`), and every path, command, and
//! assembly name is derived from the manifest, so no project identity is
//! compiled into the host binary. A missing or unreadable project fails fast
//! with a [`ConfigError`] naming the offending path, and
//! [`ProjectModuleConfig::validate`] reports the first invalid field so
//! callers can correct it directly.

// Standard library
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// Current crate
use pill_core::error::ConfigError;

// =============================================================================
// Constants
// =============================================================================

/// The only variable the host reads: the project directory to run, relative
/// to the workspace root (for example `../examples/project_rs`).
const PROJECT_PATH_ENVIRONMENT_VARIABLE: &str = "PROJECT_PATH";

/// Assembly name of the bundled `csharp_runtime` collectible loader.
const CSHARP_RUNTIME_ASSEMBLY_NAME: &str = "csharp_runtime";

/// Workspace-root-relative output directory of the bundled C# runtime.
const CSHARP_RUNTIME_OUTPUT_SUBDIRECTORY: &str = "pill_csharp_runtime/bin/Release/net8.0";

/// Default target framework used for the managed project output path.
const CSHARP_TARGET_FRAMEWORK: &str = "net8.0";

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
        library_name: String,
        /// Output subdirectory relative to the workspace root.
        output_subdirectory: String,
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
    pub runtime_assembly_name: String,
    /// Output subdirectory for the runtime assembly, relative to the workspace root.
    pub runtime_output_subdirectory: String,
    /// Name of the project assembly loaded by the runtime.
    pub project_assembly_name: String,
    /// Output subdirectory for the project assembly, relative to the workspace root.
    pub project_output_subdirectory: String,
}

/// Configuration for a hot-reloadable project module.
///
/// Describes how to build the module, where to find its output, and which
/// source directories to watch. The struct is assembled by
/// [`Self::from_environment`] from the `PROJECT_PATH` variable plus the
/// project's manifest, and owns every value, so no project identity is
/// compiled into the host binary.
///
/// `#[non_exhaustive]` allows new fields to be added without breaking
/// frontends; prefer [`Self::from_environment`] over struct literals.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ProjectModuleConfig {
    /// Human-readable name used in log messages.
    pub name: String,

    /// Directory to watch for source changes, relative to the workspace root.
    pub watch_directory: String,

    /// Build command whose first element is the program and rest are arguments.
    pub build_command: Vec<String>,

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

    /// Assemble the module configuration from the single `PROJECT_PATH` variable.
    ///
    /// `PROJECT_PATH` names the project directory relative to the workspace
    /// root. The backend is detected from the directory contents (`Cargo.toml`
    /// selects the native Rust backend, a `*.csproj` selects the managed C#
    /// backend), and every path, command, and assembly name is derived from
    /// the project manifest, so no project identity is compiled into the host.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when the variable is missing, the directory
    /// cannot be inspected, no supported manifest exists, or a manifest field
    /// cannot be read.
    pub fn from_environment() -> Result<Self, ConfigError> {
        // Step 1: Read the single project directory variable.
        let project_path = required_environment(PROJECT_PATH_ENVIRONMENT_VARIABLE)?;

        // Step 2: Resolve the directory against the working directory. The
        // host is launched from the workspace root, so `cwd` matches the root
        // that every stored path is relative to.
        let project_root = env::current_dir()
            .map_err(|_| ConfigError::ProjectDirectoryMissing {
                path: project_path.clone(),
            })?
            .join(&project_path);
        if !project_root.is_dir() {
            return Err(ConfigError::ProjectDirectoryMissing { path: project_path });
        }

        // Step 3: Detect the backend from the manifest files in the directory.
        let cargo_manifest = project_root.join("Cargo.toml");
        if cargo_manifest.is_file() {
            return Self::native_from_manifest(&project_path, &cargo_manifest);
        }
        let csproj_manifest = find_csproj_manifest(&project_root)?;
        Self::csharp_from_manifest(&project_path, &csproj_manifest)
    }

    /// Derive a native shared-library configuration from a `Cargo.toml`.
    fn native_from_manifest(project_path: &str, manifest_path: &Path) -> Result<Self, ConfigError> {
        // Step 1: Read the manifest to discover the package and library names.
        let manifest = std::fs::read_to_string(manifest_path).map_err(|source| {
            ConfigError::ProjectManifestReadFailed {
                path: manifest_path.display().to_string(),
                source,
            }
        })?;
        let package_name =
            manifest_string_value(&manifest, "package", "name").ok_or_else(|| {
                ConfigError::ProjectPackageNameMissing {
                    path: manifest_path.display().to_string(),
                }
            })?;
        let library_name =
            manifest_string_value(&manifest, "lib", "name").unwrap_or_else(|| package_name.clone());

        // Step 2: Derive the source watch directory and build output location.
        let watch_directory = format!("{project_path}/src");
        let output_subdirectory = format!("{project_path}/target/debug");

        // Step 3: Build the Cargo command. When the host renders, the project
        // module is built with the same feature so both sides share renderer
        // components.
        #[cfg(not(feature = "rendering"))]
        let build_command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--manifest-path".to_string(),
            format!("{project_path}/Cargo.toml"),
            "--package".to_string(),
            package_name.clone(),
        ];
        #[cfg(feature = "rendering")]
        let build_command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--manifest-path".to_string(),
            format!("{project_path}/Cargo.toml"),
            "--package".to_string(),
            package_name.clone(),
            "--features".to_string(),
            "rendering".to_string(),
        ];

        Ok(Self {
            name: package_name,
            watch_directory,
            build_command,
            backend: ProjectModuleBackend::NativeLibrary {
                library_name,
                output_subdirectory,
            },
        })
    }

    /// Derive a managed C# configuration from a `.csproj` manifest.
    fn csharp_from_manifest(project_path: &str, csproj_path: &Path) -> Result<Self, ConfigError> {
        // Step 1: The assembly name follows the project file name.
        let project_assembly_name = csproj_path
            .file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| ConfigError::ProjectManifestMissing {
                path: csproj_path.display().to_string(),
            })?
            .to_string();

        // Step 2: Derive the source watch directory, build command, and output.
        let watch_directory = format!("{project_path}/src");
        let project_output_subdirectory =
            format!("{project_path}/bin/Release/{CSHARP_TARGET_FRAMEWORK}");
        let build_command = vec![
            "dotnet".to_string(),
            "build".to_string(),
            format!("{project_path}/{project_assembly_name}.csproj"),
            "-c".to_string(),
            "Release".to_string(),
            "--nologo".to_string(),
        ];

        Ok(Self {
            name: project_assembly_name.clone(),
            watch_directory,
            build_command,
            backend: ProjectModuleBackend::CSharp(CSharpModuleConfig {
                runtime_assembly_name: CSHARP_RUNTIME_ASSEMBLY_NAME.to_string(),
                runtime_output_subdirectory: CSHARP_RUNTIME_OUTPUT_SUBDIRECTORY.to_string(),
                project_assembly_name,
                project_output_subdirectory,
            }),
        })
    }
}

/// Read a required configuration variable or report it as missing.
///
/// # Errors
///
/// Returns [`ConfigError::MissingEnvironmentVariable`] when the variable is
/// absent, so callers can fix the launch environment in one step.
fn required_environment(variable: &'static str) -> Result<String, ConfigError> {
    env::var(variable).map_err(|_| ConfigError::MissingEnvironmentVariable { variable })
}

/// Locate the single `.csproj` manifest inside a project directory.
///
/// # Errors
///
/// Returns [`ConfigError::ProjectDirectoryMissing`] when the directory cannot
/// be listed and [`ConfigError::ProjectManifestMissing`] when no `.csproj`
/// file is found.
fn find_csproj_manifest(project_root: &Path) -> Result<PathBuf, ConfigError> {
    let entries =
        std::fs::read_dir(project_root).map_err(|_| ConfigError::ProjectDirectoryMissing {
            path: project_root.display().to_string(),
        })?;
    for entry in entries {
        let entry = entry.map_err(|_| ConfigError::ProjectDirectoryMissing {
            path: project_root.display().to_string(),
        })?;
        let path = entry.path();
        if path.extension() == Some(OsStr::new("csproj")) {
            return Ok(path);
        }
    }
    Err(ConfigError::ProjectManifestMissing {
        path: project_root.display().to_string(),
    })
}

/// Extract a quoted string value from one section of a TOML manifest.
///
/// Scans for the top-level `[section]` header, then reads `key = "value"`
/// lines inside it until a nested section starts. Used only for the handful
/// of fields the host infers from a `Cargo.toml`, so the host stays free of
/// a full TOML dependency.
fn manifest_string_value(manifest: &str, section: &str, key: &str) -> Option<String> {
    let section_header = format!("[{section}]");
    let mut in_section = false;
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        // Any bracketed header selects the section and ends the previous one.
        if line.starts_with('[') {
            in_section = line == section_header;
            continue;
        }
        if !in_section || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        if left.trim() != key {
            continue;
        }
        // Accept `key = "value"` with an optional trailing comment.
        let value = right.trim();
        if let Some(inner) = value.strip_prefix('"') {
            if let Some(end) = inner.find('"') {
                return Some(inner[..end].to_string());
            }
        }
    }
    None
}
