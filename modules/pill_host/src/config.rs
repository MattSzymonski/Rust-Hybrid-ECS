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

/// Comma-separated list of optional modules to build, watch and load.
const OPTIONAL_MODULES_ENVIRONMENT_VARIABLE: &str = "PILL_MODULES";

/// Workspace-relative directory holding every optional module crate.
///
/// The workspace manifest globs this directory, so a module is discovered by
/// existing rather than by being listed anywhere.
const OPTIONAL_MODULE_DIRECTORY: &str = "optional";

/// Optional modules loaded when `PILL_MODULES` is not set.
const DEFAULT_OPTIONAL_MODULES: &str = "pill_test";

/// Cargo variable that overrides the workspace-configured compiler flags.
///
/// Setting it takes precedence over `build.rustflags` in a Cargo config file,
/// which the `--config` command-line override does not do.
const RUSTFLAGS_VARIABLE: &str = "RUSTFLAGS";

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

    /// Environment variables applied to the build process.
    ///
    /// Used to override values the build would otherwise inherit from the
    /// host's environment or from the workspace Cargo configuration.
    pub build_environment: Vec<(String, String)>,

    /// How the built module is loaded and executed.
    pub backend: ProjectModuleBackend,
}

/// Build, watch and load description for one optional engine module.
///
/// Optional modules are workspace members of the engine workspace, built as
/// `cdylib` and loaded by the host at runtime. They share the workspace's
/// lockfile and Cargo configuration, which is what lets them link the engine
/// dynamically and share one copy of its statics with the host.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct OptionalModuleConfig {
    /// Crate directory name, also the log field and temporary-copy prefix.
    pub name: String,

    /// Library name without the platform prefix or suffix.
    pub library_name: String,

    /// Directory to watch for source changes, relative to the workspace root.
    pub watch_directory: String,

    /// Build command whose first element is the program and rest are arguments.
    pub build_command: Vec<String>,

    /// Output directory of the built artifact, relative to the workspace root.
    pub output_subdirectory: String,
}

impl OptionalModuleConfig {
    /// Derive the configuration of a module crate from its directory name.
    ///
    /// Optional modules live under [`OPTIONAL_MODULE_DIRECTORY`] inside the
    /// engine workspace, which a glob in the workspace manifest picks up
    /// automatically, so a new module needs no manifest edit. The directory
    /// name determines everything else: sources in `<directory>/<name>/src`,
    /// the artifact in the shared `target/debug`, and a plain package
    /// selection for the build.
    ///
    /// Building inside the workspace is required rather than convenient. It
    /// makes the module resolve the identical dependency graph as the host,
    /// which is what keeps component type identities and the mangled symbol
    /// names of the shared `pill_core` library in agreement.
    pub fn workspace_member(name: &str) -> Self {
        let mut build_command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--package".to_string(),
            name.to_string(),
        ];
        // Mirror the host's engine feature set. A module compiled against a
        // differently configured engine can disagree about type layout.
        if cfg!(feature = "rendering") {
            build_command.push("--features".to_string());
            build_command.push("rendering".to_string());
        }

        Self {
            name: name.to_string(),
            library_name: name.to_string(),
            watch_directory: format!("{OPTIONAL_MODULE_DIRECTORY}/{name}/src"),
            build_command,
            output_subdirectory: "target/debug".to_string(),
        }
    }

    /// Verify that the configuration is internally consistent.
    ///
    /// # Errors
    ///
    /// Returns a typed [`ConfigError`] naming the first invalid field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.name.is_empty() {
            return Err(ConfigError::EmptyModuleName);
        }
        if self.watch_directory.is_empty() {
            return Err(ConfigError::EmptyWatchDirectory);
        }
        if self.build_command.is_empty() {
            return Err(ConfigError::EmptyBuildCommand);
        }
        Ok(())
    }
}

/// Complete host configuration: the project module plus every optional module
/// the host should build, watch and load.
///
/// Assembled by [`Self::from_environment`] so executable crates stay free of
/// any project or module identity.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// The project module selected by `PROJECT_PATH`.
    pub project: ProjectModuleConfig,

    /// Optional modules selected by `PILL_MODULES`, loaded before the project.
    pub optional_modules: Vec<OptionalModuleConfig>,
}

impl HostConfig {
    /// Assemble the host configuration from the environment.
    ///
    /// `PROJECT_PATH` selects the project module and `PILL_MODULES` selects the
    /// optional modules; see [`ProjectModuleConfig::from_environment`] and
    /// [`selected_optional_modules`].
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when the project configuration cannot be
    /// derived or when a configured module is invalid.
    pub fn from_environment() -> Result<Self, ConfigError> {
        let project = ProjectModuleConfig::from_environment()?;
        let optional_modules: Vec<OptionalModuleConfig> = selected_optional_modules()
            .iter()
            .map(|name| OptionalModuleConfig::workspace_member(name))
            .collect();
        for module in &optional_modules {
            module.validate()?;
        }
        Ok(Self {
            project,
            optional_modules,
        })
    }
}

impl From<ProjectModuleConfig> for HostConfig {
    /// Run one project with no optional modules.
    ///
    /// Keeps embedders that already build a `ProjectModuleConfig` compiling.
    fn from(project: ProjectModuleConfig) -> Self {
        Self {
            project,
            optional_modules: Vec::new(),
        }
    }
}

/// Read the selected optional modules from the environment.
///
/// `PILL_MODULES` is a comma-separated list of crate directory names inside the
/// engine workspace, for example `pill_test,pill_physics`. When it is not set
/// the default set is loaded; setting it to an empty value disables optional
/// modules entirely, which is how one run opts out without a rebuild.
pub fn selected_optional_modules() -> Vec<String> {
    env::var(OPTIONAL_MODULES_ENVIRONMENT_VARIABLE)
        .unwrap_or_else(|_| DEFAULT_OPTIONAL_MODULES.to_string())
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
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
        let mut build_command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--manifest-path".to_string(),
            format!("{project_path}/Cargo.toml"),
            "--package".to_string(),
            package_name.clone(),
        ];
        if cfg!(feature = "rendering") {
            build_command.push("--features".to_string());
            build_command.push("rendering".to_string());
        }

        Ok(Self {
            name: package_name,
            watch_directory,
            build_command,
            // Project modules live in their own workspace with their own
            // lockfile, so they link the engine statically. The engine
            // workspace turns on `-C prefer-dynamic` through its Cargo config,
            // and this build inherits that config because it runs from the
            // workspace root, so the flags are cleared explicitly here. Shared
            // dynamic linkage would require both sides to resolve an identical
            // dependency graph: a Rust dylib exports symbols mangled with a
            // metadata hash derived from that graph, and a mismatch fails at
            // load time with a missing-procedure error.
            build_environment: vec![(RUSTFLAGS_VARIABLE.to_string(), String::new())],
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
            // The managed build never invokes rustc, so it needs no overrides.
            build_environment: Vec::new(),
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
