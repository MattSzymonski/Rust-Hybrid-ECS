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

/// Workspace-relative directory holding every optional module crate.
///
/// The workspace manifest globs this directory, so a module is discovered by
/// existing rather than by being listed anywhere.
const OPTIONAL_MODULE_DIRECTORY: &str = "optional";

/// Name prefix of the generated workspace member that builds a native project.
///
/// The member lives under [`OPTIONAL_MODULE_DIRECTORY`], so the existing
/// `optional/*` workspace glob discovers it without any entry in the workspace
/// manifest; only the package name differs per project.
const HOST_PROJECT_MEMBER_PREFIX: &str = "host_project_";

/// Optional host configuration file name, resolved in the engine workspace
/// root. Declares the project and the optional modules; environment variables
/// override it for a single run.
const HOST_CONFIG_FILE: &str = "pill_config.yaml";

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

    /// Workspace-relative path to the project's manifest file.
    ///
    /// The host re-reads it at runtime to answer dependency questions, such as
    /// whether a reloaded optional module is linked by this project.
    pub manifest_path: Option<String>,

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

    /// Directory of the hot-load artifact, relative to the workspace root.
    ///
    /// Cargo writes the freshly built standalone library into the shared
    /// output slot first; the host stages it into this private directory and
    /// loads from there, so a project build overwriting the shared slot
    /// cannot corrupt the loaded module.
    pub output_subdirectory: String,
}

impl OptionalModuleConfig {
    /// Derive the configuration of a module crate from its directory name.
    ///
    /// Optional modules live under [`OPTIONAL_MODULE_DIRECTORY`] inside the
    /// engine workspace, which a glob in the workspace manifest picks up
    /// automatically, so a new module needs no manifest edit. The directory
    /// name determines everything else: sources in `<directory>/<name>/src`,
    /// the loadable artifact staged into the private `target/hot` hot-load
    /// directory, and a plain package selection for the build.
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
            // Emit a cargo timing report so the analytics collector can show
            // per-crate compile+link wall time for every host-driven build.
            "--timings".to_string(),
            // Never touch the registry: every dependency is already cached in
            // the workspace. Skipping the index avoids the ~/.cargo package
            // cache lock (which rust-analyzer's cargo check can hold for long
            // stretches) and halves the fixed per-build cargo overhead.
            "--offline".to_string(),
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
            output_subdirectory: "target/hot".to_string(),
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

    /// Optional modules selected by `pill_config.yaml`, loaded before the project.
    pub optional_modules: Vec<OptionalModuleConfig>,
}

impl HostConfig {
    /// Assemble the host configuration from the environment and the optional
    /// `pill_config.yaml` file in the workspace root.
    ///
    /// The file declares the project and the optional modules. `PROJECT_PATH`
    /// overrides the file's `project` entry for a single run; the optional
    /// module list comes from the file alone, so which modules load is always
    /// visible in one place instead of depending on shell state.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when no project is configured, the project
    /// configuration cannot be derived, the configuration file cannot be
    /// parsed, or a configured module is invalid.
    pub fn from_environment() -> Result<Self, ConfigError> {
        // Step 1: Load the optional host configuration file.
        let file_config = read_host_file_config()?;

        // Step 2: Resolve the effective project path: the environment wins,
        // then the configuration file.
        let project_path = env::var(PROJECT_PATH_ENVIRONMENT_VARIABLE)
            .ok()
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|config| config.project.clone())
            })
            .ok_or(ConfigError::MissingEnvironmentVariable {
                variable: PROJECT_PATH_ENVIRONMENT_VARIABLE,
            })?;
        let project = ProjectModuleConfig::from_path(&project_path)?;

        // Step 3: Resolve the optional modules and validate every configured
        // module before any build work starts.
        let optional_modules: Vec<OptionalModuleConfig> = optional_module_names(&file_config)
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

/// Resolve the optional modules to load from the host configuration file.
///
/// An absent file or an empty `modules` list both mean no optional modules,
/// so leaving `pill_config.yaml` unwritten is equivalent to opting out.
fn optional_module_names(file_config: &Option<HostFileConfig>) -> Vec<String> {
    file_config
        .as_ref()
        .map(|config| config.modules.clone())
        .unwrap_or_default()
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
        let project_path = required_environment(PROJECT_PATH_ENVIRONMENT_VARIABLE)?;
        Self::from_path(&project_path)
    }

    /// Assemble the module configuration from an explicit workspace-relative
    /// project path.
    ///
    /// Backend detection and path derivation are shared with
    /// [`Self::from_environment`]; this is the entry point used by
    /// [`HostConfig::from_environment`] so a `pill_config.yaml` value can be
    /// honoured without the environment variable.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when the directory cannot be inspected, no
    /// supported manifest exists, or a manifest field cannot be read.
    fn from_path(project_path: &str) -> Result<Self, ConfigError> {
        // Step 1: Resolve the directory against the working directory. The
        // host is launched from the workspace root, so `cwd` matches the root
        // that every stored path is relative to.
        let current_dir = env::current_dir().map_err(|_| ConfigError::ProjectDirectoryMissing {
            path: project_path.to_string(),
        })?;
        let project_root = current_dir.join(project_path);
        if !project_root.is_dir() {
            return Err(ConfigError::ProjectDirectoryMissing {
                path: project_path.to_string(),
            });
        }

        // Step 2: Detect the backend from the manifest files in the directory.
        let cargo_manifest = project_root.join("Cargo.toml");
        if cargo_manifest.is_file() {
            return Self::native_from_manifest(&current_dir, project_path, &cargo_manifest);
        }
        let csproj_manifest = find_csproj_manifest(&project_root)?;
        Self::csharp_from_manifest(project_path, &csproj_manifest)
    }

    /// Derive a native shared-library configuration from a `Cargo.toml`.
    fn native_from_manifest(
        workspace_root: &Path,
        project_path: &str,
        manifest_path: &Path,
    ) -> Result<Self, ConfigError> {
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

        // Step 2: Materialize a workspace member for the project so it compiles
        // against the engine workspace (one Cargo.lock, one target directory,
        // one crate-metadata set). This is what keeps `pill_spline::Spline` the
        // same type in the project DLL and in the optional module DLLs. The
        // member is generated under `optional/` and discovered by the existing
        // `optional/*` glob, so the workspace manifest never names the project.
        materialize_host_project_member(workspace_root, project_path, &package_name)?;

        // Step 3: Derive the source watch directory and build output location.
        // Sources are watched in place at the real project path, while the
        // built artifact lands in the workspace target directory.
        let watch_directory = format!("{project_path}/src");
        let output_subdirectory = "target/debug".to_string();

        // Step 4: Build the Cargo command. The project is a workspace member,
        // so the build selects it by package name from the workspace root. It
        // inherits the workspace's `-C prefer-dynamic` rustflags, matching the
        // optional modules, because a shared crate keeps one metadata identity
        // (and therefore one `TypeId`) only when every side is compiled with
        // the same inputs. When the host renders, the project is built with
        // the same feature so both sides share renderer components.
        let mut build_command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--package".to_string(),
            package_name.clone(),
            // Emit a cargo timing report so the analytics collector can show
            // per-crate compile+link wall time for every host-driven build.
            "--timings".to_string(),
            // Never touch the registry: every dependency is already cached in
            // the workspace. Skipping the index avoids the ~/.cargo package
            // cache lock (which rust-analyzer's cargo check can hold for long
            // stretches) and halves the fixed per-build cargo overhead.
            "--offline".to_string(),
        ];
        if cfg!(feature = "rendering") {
            build_command.push("--features".to_string());
            build_command.push("rendering".to_string());
        }

        Ok(Self {
            name: package_name,
            manifest_path: Some(format!("{project_path}/Cargo.toml")),
            watch_directory,
            build_command,
            build_environment: Vec::new(),
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
            manifest_path: None,
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

/// Whether the project manifest declares a direct dependency on `crate_name`.
///
/// Reads the project's manifest again at runtime because the decision depends
/// on the module that just changed, which is only known during the frame loop.
/// A missing or unreadable manifest counts as "no dependency", so a transient
/// filesystem state can never force the project through an extra reload.
pub(crate) fn project_depends_on_crate(
    workspace_root: &Path,
    project: &ProjectModuleConfig,
    crate_name: &str,
) -> bool {
    // Only a native Rust project can link an optional module crate; a managed
    // project cannot reference it, so it never needs this reload.
    if !matches!(project.backend, ProjectModuleBackend::NativeLibrary { .. }) {
        return false;
    }
    let Some(manifest_path) = &project.manifest_path else {
        return false;
    };
    let Ok(manifest) = std::fs::read_to_string(workspace_root.join(manifest_path)) else {
        return false;
    };
    manifest_depends_on_crate(&manifest, crate_name)
}

/// Scan a `Cargo.toml` for a dependency entry naming `crate_name`.
///
/// Accepts both a direct entry (`pill_spline = { ... }`) and a renamed one
/// (`my_spline = { package = "pill_spline", ... }`). Only dependency sections
/// contribute code to the built library: `[dependencies]`, the dev and build
/// variants, and target-specific tables that end in `.dependencies`.
fn manifest_depends_on_crate(manifest: &str, crate_name: &str) -> bool {
    let mut in_dependency_section = false;
    for raw_line in manifest.lines() {
        let line = raw_line.trim();

        // Any bracketed header selects a section and ends the previous one.
        if line.starts_with('[') {
            let section = line.trim_start_matches('[').trim_end_matches(']').trim();
            // A sub-table like `[dependencies.foo]` declares the dependency
            // `foo` itself; match its key directly and leave its body unscanned.
            if let Some(dependency_key) = dependency_sub_table_key(section) {
                if dependency_key == crate_name {
                    return true;
                }
            }
            in_dependency_section = is_dependency_section(section);
            continue;
        }

        if !in_dependency_section || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // Dependency keys may be quoted when renamed to a non-identifier.
        let key = key.trim().trim_matches('"');
        if key == crate_name {
            return true;
        }
        // A renamed dependency keeps the real crate name in `package`.
        if value.contains(&format!("package = \"{crate_name}\"")) {
            return true;
        }
    }
    false
}

/// Whether `section` is one of the dependency tables that contributes linked
/// code: the plain `[dependencies]` family or a target-specific table ending
/// in `.dependencies`. The workspace-wide `[workspace.dependencies]` table is
/// shared infrastructure rather than a dependency of this project, so it is
/// excluded.
fn is_dependency_section(section: &str) -> bool {
    matches!(
        section,
        "dependencies" | "dev-dependencies" | "build-dependencies"
    ) || (section.ends_with(".dependencies") && !section.starts_with("workspace."))
}

/// The crate name a dependency sub-table declares, for sections shaped like
/// `[dependencies.<name>]`, `[dev-dependencies.<name>]`, or
/// `[build-dependencies.<name>]`. Returns `None` for any other section or for
/// a sub-table nested deeper than one level.
fn dependency_sub_table_key(section: &str) -> Option<&str> {
    for prefix in ["dependencies.", "dev-dependencies.", "build-dependencies."] {
        if let Some(rest) = section.strip_prefix(prefix) {
            if !rest.contains('.') {
                return Some(rest);
            }
        }
    }
    None
}

/// Values declared by the optional `pill_config.yaml` file.
///
/// Every field is optional so a file can override just the entries it cares
/// about; anything left unset falls back to the environment or a default.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct HostFileConfig {
    /// Project directory, workspace-relative; overridden by `PROJECT_PATH`.
    project: Option<String>,
    /// Optional module crate names, in load order. The only source for this
    /// list: there is no environment-variable override, so the file is always
    /// the complete answer to "which modules load".
    modules: Vec<String>,
}

/// Read the optional host configuration file from the workspace root.
///
/// Returns `Ok(None)` when no file exists or the working directory cannot be
/// determined, so a host that has no config file behaves exactly as before.
/// A present but unreadable or malformed file is an error, so a typo cannot
/// silently fall back to a different project.
///
/// # Errors
///
/// Returns a [`ConfigError`] when the file exists but cannot be read or is
/// not valid YAML.
fn read_host_file_config() -> Result<Option<HostFileConfig>, ConfigError> {
    let Ok(workspace_root) = env::current_dir() else {
        return Ok(None);
    };
    let config_path = workspace_root.join(HOST_CONFIG_FILE);
    if !config_path.is_file() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&config_path).map_err(|source| {
        ConfigError::HostConfigFileReadFailed {
            path: config_path.display().to_string(),
            source,
        }
    })?;
    serde_yaml::from_str(&contents).map(Some).map_err(|source| {
        ConfigError::HostConfigFileParseFailed {
            path: config_path.display().to_string(),
            details: source.to_string(),
        }
    })
}

/// Materialize a temporary workspace member for the native project.
///
/// Cross-DLL type identity requires the project to compile as a member of the
/// engine workspace: one Cargo.lock, one target directory, one crate-metadata
/// set. The project source lives outside the workspace, so this writes a
/// generated crate under `optional/` — which the existing `optional/*` glob
/// discovers automatically, so no workspace-manifest entry is ever needed.
///
/// The generated `Cargo.toml` is the project's own manifest with every `path`
/// dependency resolved to an absolute location and the `[lib]` `path` pointed
/// at the real source file. The project source is never copied or moved, and
/// because it is compiled from its real location, edits to it keep triggering
/// rebuilds through the normal source watcher.
///
/// The member is regenerated on every startup, so a change to the project's
/// manifest is picked up without any stale copy.
///
/// # Errors
///
/// Returns a [`ConfigError`] when the project manifest cannot be read or the
/// generated member directory or manifest cannot be written.
fn materialize_host_project_member(
    workspace_root: &Path,
    project_path: &str,
    package_name: &str,
) -> Result<(), ConfigError> {
    let project_root = workspace_root.join(project_path);
    let source_manifest =
        std::fs::read_to_string(project_root.join("Cargo.toml")).map_err(|source| {
            ConfigError::ProjectManifestReadFailed {
                path: project_root.join("Cargo.toml").display().to_string(),
                source,
            }
        })?;

    // Step 1: Resolve every `path = "..."` value against the real project
    // directory so the generated manifest works from any location. Forward
    // slashes keep the TOML strings valid on Windows.
    let mut generated_manifest = String::with_capacity(source_manifest.len() + 512);
    let mut cursor = 0usize;
    while let Some(relative_start) = source_manifest[cursor..].find("path = \"") {
        let value_start = cursor + relative_start + "path = \"".len();
        let Some(relative_end) = source_manifest[value_start..].find('"') else {
            break;
        };
        let relative_end = value_start + relative_end;
        let relative = &source_manifest[value_start..relative_end];
        let absolute = if Path::new(relative).is_absolute() {
            relative.to_string()
        } else {
            project_root
                .join(relative)
                .to_string_lossy()
                .replace('\\', "/")
        };
        generated_manifest.push_str(&source_manifest[cursor..value_start]);
        generated_manifest.push_str(&absolute);
        cursor = relative_end;
    }
    generated_manifest.push_str(&source_manifest[cursor..]);

    // Step 2: Point the generated member's library at the real source file so
    // the project compiles from its actual location.
    let lib_source = project_root.join("src").join("lib.rs");
    if lib_source.is_file() {
        let absolute_lib_path = lib_source.to_string_lossy().replace('\\', "/");
        if let Some(lib_header) = generated_manifest.find("[lib]") {
            // The `[lib]` section ends at the next bracketed header.
            let section_end = generated_manifest[lib_header..]
                .find("\n[")
                .map(|offset| lib_header + offset)
                .unwrap_or(generated_manifest.len());
            let header_line_end = generated_manifest[lib_header..]
                .find('\n')
                .map(|offset| lib_header + offset)
                .unwrap_or(section_end);
            if !generated_manifest[lib_header..section_end].contains("path =") {
                generated_manifest.insert_str(
                    header_line_end,
                    &format!("\npath = \"{absolute_lib_path}\""),
                );
            }
        }
    }

    // Step 3: Write the generated member, replacing any previous generation.
    //
    // When the generated content is unchanged the existing file is left
    // untouched so its modification time survives. Cargo fingerprints the
    // manifest by content, so this is not required for correctness, but the
    // host's own up-to-date build check compares modification times and would
    // otherwise see a freshly rewritten manifest as newer than every artifact.
    let member_directory = workspace_root
        .join(OPTIONAL_MODULE_DIRECTORY)
        .join(format!("{HOST_PROJECT_MEMBER_PREFIX}{package_name}"));
    std::fs::create_dir_all(&member_directory).map_err(|source| {
        ConfigError::HostProjectMemberCreationFailed {
            path: member_directory.display().to_string(),
            source,
        }
    })?;
    let member_manifest = member_directory.join("Cargo.toml");
    let content_changed = std::fs::read_to_string(&member_manifest)
        .map(|existing| existing != generated_manifest)
        .unwrap_or(true);
    if content_changed {
        std::fs::write(&member_manifest, generated_manifest).map_err(|source| {
            ConfigError::HostProjectMemberCreationFailed {
                path: member_manifest.display().to_string(),
                source,
            }
        })?;
    }

    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A native project manifest that links the optional module directly.
    const DEPENDENT_MANIFEST: &str = r#"
[package]
name = "project"

[dependencies]
pill_engine = { path = "../../modules/pill_engine" }
pill_spline = { path = "../../modules/optional/pill_spline" }
"#;

    /// A native project manifest with no optional-module dependency.
    const INDEPENDENT_MANIFEST: &str = r#"
[package]
name = "project"

[dependencies]
pill_engine = { path = "../../modules/pill_engine" }
serde = { version = "1", features = ["derive"] }
"#;

    /// A native `ProjectModuleConfig` for dependency-check tests.
    fn native_config(manifest_path: Option<&str>) -> ProjectModuleConfig {
        ProjectModuleConfig {
            name: "project".to_string(),
            manifest_path: manifest_path.map(str::to_string),
            watch_directory: "project/src".to_string(),
            build_command: vec!["cargo".to_string(), "build".to_string()],
            build_environment: Vec::new(),
            backend: ProjectModuleBackend::NativeLibrary {
                library_name: "project".to_string(),
                output_subdirectory: "target/debug".to_string(),
            },
        }
    }

    /// A managed `ProjectModuleConfig`; it carries no Rust manifest path.
    fn csharp_config() -> ProjectModuleConfig {
        ProjectModuleConfig {
            name: "project_cs".to_string(),
            manifest_path: None,
            watch_directory: "project_cs/src".to_string(),
            build_command: vec!["dotnet".to_string(), "build".to_string()],
            build_environment: Vec::new(),
            backend: ProjectModuleBackend::CSharp(CSharpModuleConfig {
                runtime_assembly_name: "csharp_runtime".to_string(),
                runtime_output_subdirectory: "pill_csharp_runtime/bin/Release/net8.0".to_string(),
                project_assembly_name: "project_cs".to_string(),
                project_output_subdirectory: "project_cs/bin/Release/net8.0".to_string(),
            }),
        }
    }

    /// Root of this test run's temporary files, unique per process so parallel
    /// test binaries never collide.
    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("pill_host_dependency_test_{}", std::process::id()))
    }

    /// Write a manifest into a subdirectory and return its path.
    ///
    /// Each call starts from a clean subdirectory and tests remove only their
    /// own subdirectory when done, so parallel tests never delete a manifest
    /// another test is still reading.
    fn write_manifest(subdirectory: &str, contents: &str) -> PathBuf {
        let directory = temp_root().join(subdirectory);
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("Cargo.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    // =========================================================================
    // manifest_depends_on_crate
    // =========================================================================

    /// A direct dependency declared as an inline table is detected.
    #[test]
    fn direct_dependency_is_detected() {
        assert!(manifest_depends_on_crate(DEPENDENT_MANIFEST, "pill_spline"));
    }

    /// A dependency renamed through `package = "..."` is detected.
    #[test]
    fn renamed_dependency_is_detected() {
        let manifest = r#"
[dependencies]
spline_path = { package = "pill_spline", path = "../../modules/optional/pill_spline" }
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A dependency with a quoted key (renamed to a non-identifier) is detected
    /// through its `package` value.
    #[test]
    fn quoted_key_rename_is_detected() {
        let manifest = r#"
[dependencies]
"spline-path" = { package = "pill_spline", path = "../../modules/optional/pill_spline" }
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// Dev and build dependencies also link the crate into the build.
    #[test]
    fn dev_and_build_dependencies_are_detected() {
        let manifest = r#"
[dev-dependencies]
pill_spline = { path = "../../modules/optional/pill_spline" }

[build-dependencies]
pill_spline = { path = "../../modules/optional/pill_spline" }
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A target-specific dependency table is detected.
    #[test]
    fn target_specific_dependency_is_detected() {
        let manifest = r#"
[target.'cfg(windows)'.dependencies]
pill_spline = { path = "../../modules/optional/pill_spline" }
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A dependency written as its own sub-table is detected.
    #[test]
    fn sub_table_dependency_is_detected() {
        let manifest = r#"
[dependencies.pill_spline]
path = "../../modules/optional/pill_spline"
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A manifest that never mentions the crate reports no dependency.
    #[test]
    fn missing_dependency_is_not_detected() {
        assert!(!manifest_depends_on_crate(
            INDEPENDENT_MANIFEST,
            "pill_spline"
        ));
    }

    /// A crate listed only under `[workspace.dependencies]` is shared
    /// infrastructure, not something this project links directly.
    #[test]
    fn workspace_shared_dependency_is_not_a_project_dependency() {
        let manifest = r#"
[workspace.dependencies]
pill_spline = { path = "../../modules/optional/pill_spline" }

[dependencies]
pill_engine = { path = "../../modules/pill_engine" }
"#;
        assert!(!manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A crate name appearing outside any dependency section is not a link.
    #[test]
    fn same_name_outside_dependency_sections_is_ignored() {
        let manifest = r#"
[package]
name = "pill_spline"

[features]
pill_spline = []

[dependencies]
pill_engine = { path = "../../modules/pill_engine" }
"#;
        assert!(!manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A longer crate name sharing the prefix must not match.
    #[test]
    fn shared_prefix_does_not_match() {
        let manifest = r#"
[dependencies]
pill_spline_extra = { path = "../../modules/optional/pill_spline_extra" }
"#;
        assert!(!manifest_depends_on_crate(manifest, "pill_spline"));
    }

    // =========================================================================
    // project_depends_on_crate
    // =========================================================================

    /// The project manifest is read from disk and a direct dependency found.
    #[test]
    fn reads_the_project_manifest() {
        let manifest = write_manifest("dependent", DEPENDENT_MANIFEST);
        let project = native_config(Some(manifest.to_str().unwrap()));
        assert!(project_depends_on_crate(
            Path::new("."),
            &project,
            "pill_spline"
        ));
        let _ = std::fs::remove_dir_all(temp_root().join("dependent"));
    }

    /// A project that does not depend on the module is not triggered.
    #[test]
    fn independent_manifest_is_not_triggered() {
        let manifest = write_manifest("independent", INDEPENDENT_MANIFEST);
        let project = native_config(Some(manifest.to_str().unwrap()));
        assert!(!project_depends_on_crate(
            Path::new("."),
            &project,
            "pill_spline"
        ));
        let _ = std::fs::remove_dir_all(temp_root().join("independent"));
    }

    /// A managed project is never triggered, even with a manifest path set,
    /// because it cannot link a Rust optional-module crate.
    #[test]
    fn managed_project_is_never_triggered() {
        let manifest = write_manifest("managed", "# csproj contents\n");
        let mut project = csharp_config();
        project.manifest_path = Some(manifest.to_string_lossy().into_owned());
        assert!(!project_depends_on_crate(
            Path::new("."),
            &project,
            "pill_spline"
        ));
        let _ = std::fs::remove_dir_all(temp_root().join("managed"));
    }

    /// A missing manifest counts as "no dependency" rather than an error.
    #[test]
    fn missing_manifest_file_is_not_an_error() {
        let project = native_config(Some("does/not/exist/Cargo.toml"));
        assert!(!project_depends_on_crate(
            Path::new("."),
            &project,
            "pill_spline"
        ));
    }

    /// A config without a manifest path counts as "no dependency".
    #[test]
    fn missing_manifest_path_is_not_an_error() {
        let project = native_config(None);
        assert!(!project_depends_on_crate(
            Path::new("."),
            &project,
            "pill_spline"
        ));
    }
}
