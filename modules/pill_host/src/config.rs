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
use std::sync::OnceLock;

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

/// Cargo profile directory this host was compiled into, recorded by `build.rs`.
///
/// `debug` for an ordinary `cargo build`, `release` for `--release`, and the
/// profile's own name for any custom profile (`release-fast`,
/// `release-with-debug`).
const HOST_PROFILE_DIRECTORY: &str = env!("PILL_HOST_PROFILE_DIRECTORY");

/// Cargo profile *name* matching [`HOST_PROFILE_DIRECTORY`].
///
/// Cargo names the default profile `dev` but writes it into `target/debug`;
/// every other profile's directory is its own name. This is the only place the
/// two vocabularies are reconciled.
///
/// Module builds must use the host's profile. A host and a module compiled
/// into different profiles resolve different crate-metadata hashes, so the
/// module's exports do not match what the host looks up and `LoadLibrary`
/// fails with "The specified procedure could not be found" - an error that
/// names nothing useful and cost a full afternoon the first time it appeared.
pub(crate) fn host_profile_name() -> &'static str {
    profile_name_for_directory(HOST_PROFILE_DIRECTORY)
}

/// Target triple the host was compiled for, recorded by `build.rs`.
///
/// Empty when the host was built natively (an ordinary `cargo build`/`cargo
/// run` without `--target`), which is every host except one launched by the
/// dioxus CLI (`dx serve`). dx always passes `--target <host-triple>` to
/// cargo, and cargo folds the target into each crate's metadata hash - so a
/// module built natively against a `--target` host gets different symbol
/// names than the host's loaded engine dylibs export, and every module load
/// dies with "The specified procedure could not be found" (os error 127).
/// Host-spawned module builds mirror this triple so both sides agree.
const HOST_TARGET_TRIPLE: &str = env!("PILL_HOST_TARGET_TRIPLE");

/// The `--target` triple host-spawned builds must pass, when the host itself
/// was built with one.
pub(crate) fn host_target_triple() -> Option<&'static str> {
    if HOST_TARGET_TRIPLE.is_empty() {
        None
    } else {
        Some(HOST_TARGET_TRIPLE)
    }
}

/// Map a cargo profile *directory* to the profile *name* that produces it.
///
/// Split out from [`host_profile_name`] so the mapping is testable without
/// rebuilding the host under every profile.
fn profile_name_for_directory(directory: &str) -> &str {
    // The one profile whose directory and name differ. `test` and `bench` also
    // build into `debug`, but the host is never compiled under those - they
    // apply to test and benchmark targets, not to a dependency being built.
    if directory == "debug" {
        "dev"
    } else {
        directory
    }
}

/// Whether a cargo profile enables link-time optimization, the one case where
/// a host-spawned build must drop the workspace's `-C prefer-dynamic` rustflag.
///
/// rustc refuses `prefer-dynamic` combined with `lto = "fat"` outright
/// ("cannot prefer dynamic linking when performing LTO"), so a host running
/// under an LTO profile has to clear the flag for every build it spawns.
/// Every other profile - `dev`, and launcher-injected dev-like profiles such
/// as the dioxus CLI's `desktop-dev` - keeps `prefer-dynamic`, which is what
/// makes modules dynamic (importing the one shared `pill_core.dll` instead of
/// embedding a private engine copy).
fn profile_uses_lto(profile: &str) -> bool {
    // The workspace manifest sets `lto = "fat"` on `release` and everything
    // that inherits from it. Custom profiles that inherit `dev` (for example
    // the dioxus CLI's `desktop-dev`) deliberately stay out of this list.
    matches!(profile, "release" | "release-fast" | "release-with-debug")
}

/// Environment variable the dioxus CLI sets on every app it launches.
///
/// Its presence tells the host it is running as an app built and served by
/// `dx`, which matters because dx builds by wrapping every workspace-member
/// rustc invocation through its own binary (see [`dioxus_executable_path`]).
const DIOXUS_CLI_ENABLED_ENVIRONMENT: &str = "DIOXUS_CLI_ENABLED";

/// Cargo's workspace-wrapper environment variable.
///
/// Cargo wraps only workspace member crates through this executable and folds
/// the executable's path into their `-C metadata` hash, which is how dx's
/// build diverges from a plain cargo build. The value is deliberately kept as
/// a constant here (not hard-coded) so the env name and the dx-specific
/// `DX_RUSTC` sibling stay in one place.
const RUSTC_WORKSPACE_WRAPPER_ENVIRONMENT: &str = "RUSTC_WORKSPACE_WRAPPER";

/// Environment variable that makes dx's own binary act as a rustc wrapper.
///
/// Without it dx treats a `dx <rustc> <args>` invocation as a normal CLI call
/// and fails; with it, dx records the invocation into the directory named by
/// this variable and then proxies the real `rustc`. Host-spawned builds that
/// mirror dx's workspace wrapper must set both variables together.
const DX_RUSTC_WRAPPER_ENVIRONMENT: &str = "DX_RUSTC";

/// Whether this host binary is running as an app launched by the dioxus CLI.
///
/// `dx serve`/`dx run` set `DIOXUS_CLI_ENABLED` (plus `DIOXUS_BUILD_ID`, the
/// devserver address, and friends) on the environment of every app it builds
/// and launches (dioxus-cli `build/builder.rs` `child_environment_variables`).
/// A plain `cargo run`/`cargo build` never sets it, which is what separates
/// the dx-built editor - whose engine dylibs carry dx's workspace-wrapper
/// hash - from every other host.
fn running_under_dioxus_cli() -> bool {
    env::var_os(DIOXUS_CLI_ENABLED_ENVIRONMENT).is_some()
}

/// Canonicalize a path without the Windows `\\?\` verbatim prefix.
///
/// `std::fs::canonicalize` returns `\\?\C:\...` on Windows; dx canonicalizes
/// its own executable with `dunce` (which strips the prefix), and the two
/// strings must be byte-identical or cargo's metadata hash will not agree.
fn canonicalize_plain(path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    let text = canonical.to_string_lossy();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
    Some(PathBuf::from(plain))
}

/// Canonical path of the `dx` executable that launched this host, if any.
///
/// dx builds by setting `RUSTC_WORKSPACE_WRAPPER` to its own canonical
/// executable path (dioxus-cli `build/request.rs` `cargo_build_command`), and
/// cargo folds that exact path into the `-C metadata` hash of workspace
/// member crates (cargo `compiler/build_runner/compilation_files.rs`
/// `compute_metadata`). Host-spawned builds must mirror the SAME path string
/// for their member crates to hash identically to the editor's, so this
/// resolves dx the same way dx resolves itself: prefer an already-recorded
/// wrapper value from the environment, otherwise look `dx` up on `PATH` and
/// canonicalize. Cached because it is read for every spawned build.
fn dioxus_executable_path() -> Option<&'static Path> {
    static DIOXUS_EXECUTABLE_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIOXUS_EXECUTABLE_PATH
        .get_or_init(|| {
            // A launcher may already have set the wrapper (or dx may have left
            // it in the environment); reuse it rather than guessing on PATH.
            if let Some(recorded) = env::var_os(RUSTC_WORKSPACE_WRAPPER_ENVIRONMENT) {
                let recorded_path = PathBuf::from(recorded);
                return canonicalize_plain(&recorded_path).or(Some(recorded_path));
            }
            let executable_name = if cfg!(windows) { "dx.exe" } else { "dx" };
            for directory in env::split_paths(&env::var_os("PATH")?) {
                let candidate = directory.join(executable_name);
                if candidate.is_file() {
                    return canonicalize_plain(&candidate);
                }
            }
            None
        })
        .as_deref()
}

/// Directory dx's wrapper writes captured member rustc invocations into.
///
/// dx only acts as a rustc wrapper when `DX_RUSTC` names a directory; it
/// records one JSON file per member crate there (never read back by this
/// host) and then proxies the real `rustc`. A private temp directory keeps
/// these host-driven captures out of dx's own `target/dx/.captured-args`
/// scope, which dx serve reads for its own thin-rebuild replay.
fn dioxus_wrapper_args_directory() -> PathBuf {
    std::env::temp_dir().join("pill-host-dx-rustc-args")
}

/// Environment every host-spawned cargo build needs, on top of the host's own.
///
/// Two independent decisions are made here, both about keeping the spawned
/// build's crate-metadata universe byte-identical to the host binary that is
/// running right now:
///
/// 1. `RUSTFLAGS` is cleared only for LTO profiles, the one case where the
///    workspace's `-C prefer-dynamic` cannot survive (rustc rejects the
///    pairing). `RUSTFLAGS` is the only lever that can drop it: cargo MERGES
///    `build.rustflags` arrays, so `--config build.rustflags=[]` joins the
///    existing list and changes nothing. An empty `RUSTFLAGS` in the
///    environment takes precedence over the config file, which is also the
///    mechanism `devops/ci_cd/build_release.sh` uses for the host's own build.
///    Dev-like launcher profiles such as the dioxus CLI's `desktop-dev`
///    (inherits `dev`, no LTO) keep `prefer-dynamic`, so modules stay
///    dynamic - importing the one shared engine instance.
/// 2. Under the dioxus CLI, spawned builds mirror dx's `RUSTC_WORKSPACE_WRAPPER`
///    so workspace member crates hash identically to the editor's. dx wraps
///    every member rustc invocation through its own binary and cargo folds
///    that wrapper path into the member crates' `-C metadata` (registry
///    crates are unaffected, which is why only members ever diverged).
///    Mirroring it through dx's own executable is safe: dx's wrapper only
///    intercepts LINKER-driver invocations (rustc args carrying `.o` files or
///    `-flavor`), and host builds link through `link.exe`, so every member
///    compile is simply proxied to the real `rustc`.
///
/// The dynamic choice is fail-safe: when running under dx but the wrapper path
/// cannot be resolved, `prefer-dynamic` is dropped too, leaving the previous
/// static-module behaviour (static modules embed their own engine and load
/// regardless of the wrapper hash; dynamic ones would fail with os error 127).
pub(crate) fn spawned_build_environment() -> Vec<(String, String)> {
    let mut environment = Vec::new();
    let under_dioxus = running_under_dioxus_cli();
    let wrapper_mirrored = under_dioxus && dioxus_executable_path().is_some();
    if profile_uses_lto(host_profile_name()) || (under_dioxus && !wrapper_mirrored) {
        environment.push(("RUSTFLAGS".to_string(), String::new()));
    }
    if wrapper_mirrored {
        if let Some(executable) = dioxus_executable_path() {
            environment.push((
                RUSTC_WORKSPACE_WRAPPER_ENVIRONMENT.to_string(),
                executable.display().to_string(),
            ));
            environment.push((
                DX_RUSTC_WRAPPER_ENVIRONMENT.to_string(),
                dioxus_wrapper_args_directory().display().to_string(),
            ));
        }
    }
    environment
}

/// Workspace-relative directory cargo wrote this host's profile into.
///
/// Used as the host's own artifact location, so the host looks for the engine
/// dylibs it maps where its own profile put them rather than always in
/// `target/debug`. A host built with `--target` (the dioxus CLI) lives under
/// `target/<triple>/<profile>`; a native one under `target/<profile>`.
pub(crate) fn host_target_directory() -> String {
    match host_target_triple() {
        Some(triple) => format!("target/{triple}/{HOST_PROFILE_DIRECTORY}"),
        None => format!("target/{HOST_PROFILE_DIRECTORY}"),
    }
}

/// Private build tree for every cargo build the host spawns.
///
/// The running host binary maps the engine dylibs (`pill_core.dll`) out of the
/// regular target directory ([`host_target_directory`]). A GUI frontend like
/// the editor unions extra features onto crates those dylibs depend on, so the
/// engine the host itself runs can legitimately need different artifacts than
/// a module build does - and a module build that rewrote the shared target
/// directory would make cargo delete a DLL the host has mapped (Windows
/// refuses to replace a loaded image). All host-spawned native builds write
/// here instead; their engine artifacts are staged into the hot-load directory
/// and loaded co-located when the two worlds differ.
///
/// When the host was itself built with `--target` ([`HOST_TARGET_TRIPLE`]),
/// host-spawned builds pass the same `--target`, so cargo inserts the triple
/// between this directory and the profile and the whole subtree lands under
/// `target/hot/build/<triple>/<profile>`.
pub(crate) const MODULE_BUILD_TARGET_DIRECTORY: &str = "target/hot/build";

/// Profile subdirectory inside [`MODULE_BUILD_TARGET_DIRECTORY`] where cargo
/// writes a host-spawned build's artifacts (`target/hot/build/debug` for a
/// native dev host, `target/hot/build/x86_64-pc-windows-msvc/desktop-dev`
/// under the dioxus CLI, which builds with an explicit `--target`).
pub(crate) fn module_build_artifact_directory() -> String {
    match host_target_triple() {
        Some(triple) => {
            format!("{MODULE_BUILD_TARGET_DIRECTORY}/{triple}/{HOST_PROFILE_DIRECTORY}")
        }
        None => format!("{MODULE_BUILD_TARGET_DIRECTORY}/{HOST_PROFILE_DIRECTORY}"),
    }
}

/// Project configuration file name, resolved in the project root. A project
/// that ships one declares its own optional-module list; the project itself is
/// selected by `PROJECT_PATH` alone.
const PROJECT_SETTINGS_FILE: &str = "project_settings.yaml";

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

impl CSharpModuleConfig {
    /// Describe a managed project by its assembly names and output directories.
    ///
    /// A reloading build gets this from
    /// [`ProjectModuleConfig::from_environment`], which reads the project's
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
            // Build into the host's own profile. Stated explicitly rather than
            // left to cargo's default, because the default is only correct for
            // a debug host; a release host that loads a debug module fails
            // inside `LoadLibrary` on mismatched crate-metadata hashes.
            "--profile".to_string(),
            host_profile_name().to_string(),
        ];
        // Enable the module's C-ABI exports explicitly. The feature is opt-in
        // (not a default) so that building every member in one cargo
        // invocation never leaks the `#[no_mangle]` `pill_module_*` exports
        // onto a module's dependency copies via feature unification.
        //
        // The features are package-qualified because the host frontend is
        // selected as an anchor package in the same invocation (see
        // `run_build_command`): a plain `module-abi` would have to exist on
        // every selected package. Qualifying keeps the module's own features
        // on the module while the anchor's presence unifies the shared engine
        // crates with whatever host binary is running - a GUI frontend unions
        // extra features onto those crates, and a module compiled against a
        // differently featured engine cannot resolve its `pill_core.dll`
        // imports against the single instance the host has loaded.
        let mut module_features = vec![format!("{name}/module-abi")];
        // `rendering` used to be mirrored here too, because the engine's
        // renderer feature changed its public type layout. The renderer now
        // lives in `pill_wgpu_renderer`, which only the host links, so the
        // engine a module compiles against is the same either way.
        // Hot patching must be mirrored for a different reason: `pill_engine` is
        // an rlib, so the module links its own copy of `register_system`. Built
        // without the feature, that copy creates no dispatch slot and every
        // patch is refused with "no hot-patchable system registered".
        if cfg!(feature = "hot_patch") {
            module_features.push("pill_engine/hot_patch".to_string());
        }
        build_command.push("--features".to_string());
        build_command.push(module_features.join(","));

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
    /// The project's display name from `project_settings.yaml`; used as the
    /// window title.
    pub name: String,

    /// The artifact file base from `project_settings.yaml`; letters, digits
    /// and underscores only, so it is safe as a file name.
    pub build_binary_name: String,

    /// The project module selected by `PROJECT_PATH`.
    pub project: ProjectModuleConfig,

    /// Optional modules selected by the project's `project_settings.yaml`,
    /// loaded before the project.
    pub optional_modules: Vec<OptionalModuleConfig>,
}

impl HostConfig {
    /// Assemble the host configuration from the environment and the project's
    /// own settings file.
    ///
    /// `PROJECT_PATH` selects the project; the project's `project_settings.yaml`
    /// (in the project root) supplies the optional-module list. There is no
    /// host-level configuration file anymore, so which project runs and which
    /// modules load are both visible at the project itself.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when `PROJECT_PATH` is not set, the project
    /// configuration cannot be derived, the project settings file is missing
    /// or cannot be read/parsed, or a configured module is invalid.
    pub fn from_environment() -> Result<Self, ConfigError> {
        // Step 1: Resolve the project from the PROJECT_PATH environment
        // variable; there is no other source for it.
        let project_path = required_environment(PROJECT_PATH_ENVIRONMENT_VARIABLE)?;
        let project = ProjectModuleConfig::from_path(&project_path)?;

        // Step 2: Load the project's own settings file. It is the only source
        // of the optional-module list and of the required `name` /
        // `build_binary_name`, so a missing file is a configuration error
        // reported as such, not masked as a missing field.
        let current_dir = env::current_dir().map_err(|_| ConfigError::ProjectDirectoryMissing {
            path: project_path.clone(),
        })?;
        let project_root = current_dir.join(&project_path);
        let settings_path = project_root.join(PROJECT_SETTINGS_FILE);
        let project_settings = read_project_settings_file(&project_root)?.ok_or(
            ConfigError::ProjectSettingsFileMissing {
                path: settings_path.display().to_string(),
            },
        )?;

        // Step 3: Resolve the required project name and binary name. The name
        // is the window title; the build binary name is the artifact file base
        // and must be filename-safe (letters, digits, underscores only), so a
        // settings file missing either is a configuration error rather than a
        // silent default.
        let project_name = project_settings
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or(ConfigError::MissingProjectName {
                path: settings_path.display().to_string(),
            })?
            .to_string();
        let build_binary_name = project_settings
            .build_binary_name
            .as_deref()
            .map(str::trim)
            .filter(|value| is_valid_build_binary_name(value))
            .ok_or(ConfigError::InvalidBuildBinaryName {
                path: settings_path.display().to_string(),
            })?
            .to_string();

        // Step 4: Resolve the optional modules and validate every configured
        // module before any build work starts.
        let optional_modules: Vec<OptionalModuleConfig> = project_settings
            .modules
            .iter()
            .map(|name| OptionalModuleConfig::workspace_member(name))
            .collect();
        for module in &optional_modules {
            module.validate()?;
        }
        Ok(Self {
            name: project_name,
            build_binary_name,
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
            name: project.name.clone(),
            build_binary_name: project.name.clone(),
            project,
            optional_modules: Vec::new(),
        }
    }
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
    /// [`HostConfig::from_environment`] to honour the `PROJECT_PATH` value.
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
        // Where cargo will put it: the private build tree host-spawned builds
        // write into (see [`module_build_artifact_directory`]), never the
        // shared `target/<profile>` the host binary maps its engine dylibs
        // from.
        let output_subdirectory = module_build_artifact_directory();

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
            // The generated member, not the project package it was copied
            // from: the two are distinct packages so a shipping binary can link
            // the project directly without colliding with this one.
            format!("{HOST_PROJECT_MEMBER_PREFIX}{package_name}"),
            // Emit a cargo timing report so the analytics collector can show
            // per-crate compile+link wall time for every host-driven build.
            "--timings".to_string(),
            // Never touch the registry: every dependency is already cached in
            // the workspace. Skipping the index avoids the ~/.cargo package
            // cache lock (which rust-analyzer's cargo check can hold for long
            // stretches) and halves the fixed per-build cargo overhead.
            "--offline".to_string(),
            // Build into the host's own profile, for the same reason the
            // optional modules do: a profile mismatch across the DLL boundary
            // is a load failure, not a performance difference.
            "--profile".to_string(),
            host_profile_name().to_string(),
        ];
        // Mirror the host's engine feature set into the project build, for the
        // same reason optional modules do: `pill_engine` is an rlib, so the
        // project links its own copy and must be configured identically.
        //
        // `rendering` used to be mirrored here as well, package-qualified onto
        // the generated project member. It is gone because the renderer left
        // `pill_engine`, so the engine a project compiles against no longer
        // depends on it.
        let mut project_features: Vec<String> = Vec::new();
        // Without this the project's copy of `register_system` compiles the
        // no-slot path, and every patch is refused with "no hot-patchable
        // system registered" - which is exactly how this was found.
        if cfg!(feature = "hot_patch") {
            project_features.push("pill_engine/hot_patch".to_string());
        }
        if !project_features.is_empty() {
            build_command.push("--features".to_string());
            build_command.push(project_features.join(","));
        }

        Ok(Self {
            name: package_name,
            manifest_path: Some(format!("{project_path}/Cargo.toml")),
            watch_directory,
            build_command,
            build_environment: spawned_build_environment(),
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
#[cfg(feature = "hot_reload")]
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
#[cfg(feature = "hot_reload")]
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
#[cfg(feature = "hot_reload")]
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
#[cfg(feature = "hot_reload")]
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

/// Values declared by the project's own `project_settings.yaml`.
///
/// `name` and `build_binary_name` are required (the display/window title and
/// the artifact file base respectively); the rest is optional classic package
/// metadata.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ProjectSettingsFile {
    /// Project display name; required, used as the window title.
    name: Option<String>,
    /// Artifact file base; required, letters/digits/underscores only.
    build_binary_name: Option<String>,
    /// Optional package version.
    version: Option<String>,
    /// Optional package author.
    author: Option<String>,
    /// Optional one-line package description.
    description: Option<String>,
    /// Optional module crate names, in load order. The only source for this
    /// list: there is no environment-variable override, so the file is always
    /// the complete answer to "which modules load".
    modules: Vec<String>,
}

/// Whether a value is a safe artifact file base: letters, digits, underscores.
fn is_valid_build_binary_name(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Read the project's own settings file from the project root.
///
/// Returns `Ok(None)` when no file exists; the caller decides whether that is
/// a missing-file error ([`HostConfig::from_environment`] treats it as one,
/// because `name` and `build_binary_name` are required) or a no-op. A present
/// but unreadable or malformed file is an error, so a typo cannot silently
/// fall back to a different module set.
///
/// # Errors
///
/// Returns a [`ConfigError`] when the file exists but cannot be read or is
/// not valid YAML.
fn read_project_settings_file(
    project_root: &Path,
) -> Result<Option<ProjectSettingsFile>, ConfigError> {
    let config_path = project_root.join(PROJECT_SETTINGS_FILE);
    if !config_path.is_file() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&config_path).map_err(|source| {
        ConfigError::ProjectSettingsFileReadFailed {
            path: config_path.display().to_string(),
            source,
        }
    })?;
    serde_yaml::from_str(&contents).map(Some).map_err(|source| {
        ConfigError::ProjectSettingsFileParseFailed {
            path: config_path.display().to_string(),
            details: source.to_string(),
        }
    })
}

/// Remove the project's own `[workspace]` (and `[workspace.*]`) tables from a
/// generated member manifest.
///
/// A standalone project declares itself a workspace root; a member of the
/// engine workspace cannot be one too, so every `[workspace...]` header —
/// including `[workspace.dependencies]` — is dropped before the member is
/// written. The header scan is line-oriented so table names inside string
/// values are never touched.
fn strip_workspace_tables(manifest: &str) -> String {
    let mut result = String::with_capacity(manifest.len());
    let mut cursor = 0usize;
    loop {
        // Find the next `[workspace...]` header at the start of a line.
        let relative = if manifest[cursor..].starts_with("[workspace") {
            Some(0)
        } else {
            manifest[cursor..]
                .find("\n[workspace")
                .map(|offset| offset + 1)
        };
        let Some(header) = relative else { break };
        let header = cursor + header;
        // The whole table runs from its header to the next bracketed header
        // (or the end of the manifest); drop it wholesale.
        let table_end = manifest[header..]
            .find("\n[")
            .map(|offset| header + offset)
            .unwrap_or(manifest.len());
        result.push_str(&manifest[cursor..header]);
        cursor = table_end;
    }
    result.push_str(&manifest[cursor..]);
    result
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
    let mut rewritten_paths: Vec<(String, String)> = Vec::new();
    while let Some(relative_start) = source_manifest[cursor..].find("path = \"") {
        let key_offset = cursor + relative_start;
        let value_start = key_offset + "path = \"".len();
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
        rewritten_paths.push((
            manifest_entry_name(&source_manifest, key_offset),
            absolute.clone(),
        ));
        generated_manifest.push_str(&source_manifest[cursor..value_start]);
        generated_manifest.push_str(&absolute);
        cursor = relative_end;
    }
    generated_manifest.push_str(&source_manifest[cursor..]);

    // Step 1b: Refuse to write a member Cargo cannot load. The generated member
    // is picked up by the `optional/*` glob, so a single unresolvable path in
    // it stops Cargo loading the workspace at all - which breaks every build,
    // test and lint in the repository, including the build that would replace
    // the member. Report the offending path instead, and clear any member an
    // earlier run left behind so the workspace stays loadable either way.
    if let Some((dependency, resolved_path)) = rewritten_paths
        .into_iter()
        .find(|(_, absolute)| !Path::new(absolute).exists())
    {
        let member_directory = workspace_root
            .join(OPTIONAL_MODULE_DIRECTORY)
            .join(format!("{HOST_PROJECT_MEMBER_PREFIX}{package_name}"));
        let _ = std::fs::remove_dir_all(&member_directory);
        return Err(ConfigError::ProjectDependencyPathMissing {
            manifest_path: project_root.join("Cargo.toml").display().to_string(),
            dependency,
            resolved_path,
        });
    }

    // Step 2: Drop the project's own `[workspace]` tables. A standalone
    // project declares itself a workspace root; as a member of the engine
    // workspace that table would make Cargo report "multiple workspace roots
    // found in the same workspace" and fail every rebuild.
    #[allow(unused_mut)]
    let mut generated_manifest = strip_workspace_tables(&generated_manifest);

    // Step 2b: Give the generated member its own package name.
    //
    // It used to keep the project's, which made two packages in one workspace
    // claim the same name: this member, and the project crate itself for any
    // build that links the project directly. Cargo rejects that outright
    // ("package collision in the lockfile"), and a statically linked shipping
    // binary has to link the project directly - that is the whole point of it.
    //
    // The `[lib] name` set below keeps the built artifact called `project.dll`,
    // so nothing downstream of the build changes; only the package cargo
    // selects does. The build command is generated from the same prefix, so
    // the two cannot drift.
    let member_package_name = format!("{HOST_PROJECT_MEMBER_PREFIX}{package_name}");
    generated_manifest = rewrite_package_name(&generated_manifest, &member_package_name);

    // Step 3: Point the generated member's library at the real source file so
    // the project compiles from its actual location, and pin the library name
    // so the artifact keeps the name the host looks for.
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

    // Step 3a: Name the library after the project, not after this member.
    //
    // Without this the artifact would follow the renamed package and land as
    // `host_project_project.dll`, which is not what the host looks for.
    if let Some(lib_header) = generated_manifest.find("[lib]") {
        let section_end = generated_manifest[lib_header..]
            .find("\n[")
            .map(|offset| lib_header + offset)
            .unwrap_or(generated_manifest.len());
        if !generated_manifest[lib_header..section_end].contains("name =") {
            let header_line_end = generated_manifest[lib_header..]
                .find('\n')
                .map(|offset| lib_header + offset)
                .unwrap_or(section_end);
            generated_manifest.insert_str(header_line_end, &format!("\nname = \"{package_name}\""));
        }
    }

    // Step 3b: Add an `rlib` artifact when the host can hot patch.
    //
    // A generated patch does `use <project>::*` so it gets the SAME types the
    // running world holds - identical layout and identical `TypeId` - and that
    // needs the crate as an rlib. It is added here rather than in the project's
    // own manifest because it costs a measured ~109 ms on every project rebuild
    // (1241 ms vs 1132 ms, best of three), which no one should pay for a
    // development feature they have not enabled.
    #[cfg(feature = "hot_patch")]
    {
        generated_manifest = add_rlib_crate_type(&generated_manifest);
    }

    // Step 4: Write the generated member, replacing any previous generation.
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
    prune_stale_host_project_members(workspace_root, &member_directory);
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

/// Replace the `name` of a manifest's `[package]` section.
///
/// Only the `[package]` table is touched: a `name` under `[lib]` or inside a
/// dependency entry means something else entirely, and rewriting one of those
/// would silently retarget the build.
///
/// Returns the manifest unchanged when it declares no `[package]` name, which
/// cannot happen for a project the host accepted - [`ProjectModuleConfig`]
/// rejects that earlier with `ProjectPackageNameMissing` - but is handled here
/// rather than assumed.
fn rewrite_package_name(manifest: &str, package_name: &str) -> String {
    let Some(section) = manifest.find("[package]") else {
        return manifest.to_string();
    };
    let section_end = manifest[section..]
        .find("\n[")
        .map(|offset| section + offset)
        .unwrap_or(manifest.len());
    let Some(name_offset) = manifest[section..section_end]
        .find("name")
        .map(|offset| section + offset)
    else {
        return manifest.to_string();
    };
    let line_end = manifest[name_offset..]
        .find('\n')
        .map(|offset| name_offset + offset)
        .unwrap_or(manifest.len());

    let mut rewritten = String::with_capacity(manifest.len() + package_name.len());
    rewritten.push_str(&manifest[..name_offset]);
    rewritten.push_str(&format!("name = \"{package_name}\""));
    rewritten.push_str(&manifest[line_end..]);
    rewritten
}

/// Names the manifest entry that owns the `path = "..."` at `key_offset`.
///
/// Used only to make an error message actionable: it reports which dependency
/// (or which section, for a `[lib]` path) has an unresolvable path, rather than
/// leaving the reader to work that out from the resolved path alone. Falls back
/// to the enclosing section header when the line carries no key of its own, and
/// to `"unknown"` when neither can be determined.
fn manifest_entry_name(manifest: &str, key_offset: usize) -> String {
    let line_start = manifest[..key_offset]
        .rfind('\n')
        .map_or(0, |index| index + 1);

    // `pill_engine = { path = "..." }` names the dependency on the same line.
    if let Some(name) = manifest[line_start..key_offset].split('=').next() {
        let name = name.trim().trim_start_matches('[');
        if !name.is_empty() {
            return name.to_string();
        }
    }

    // A bare `path = "..."` belongs to whatever section it sits under.
    manifest[..key_offset]
        .rfind('[')
        .and_then(|open| {
            manifest[open..]
                .find(']')
                .map(|close| &manifest[open..=open + close])
        })
        .map_or_else(|| "unknown".to_string(), str::to_string)
}

/// Removes host-generated workspace members left behind by earlier runs.
///
/// Every directory under `optional/` whose name carries
/// [`HOST_PROJECT_MEMBER_PREFIX`] was written by this function, so any one that
/// is not the member being materialized now belongs to a project the host is no
/// longer pointed at. Leaving it in place is not harmless: the `optional/*`
/// glob still picks it up as a workspace member, and its dependency paths point
/// into the previous project's directory. Once that directory moves or is
/// deleted, Cargo fails to load the workspace at all, so every build, test and
/// lint in the repository stops working with an error naming a crate nobody
/// asked for.
///
/// Failures are ignored rather than propagated. A stale member the host cannot
/// remove (a file lock, a permission problem) is a housekeeping problem, not a
/// reason to refuse to start; the build that follows either succeeds or reports
/// the real error itself.
fn prune_stale_host_project_members(workspace_root: &Path, keep: &Path) {
    let optional_root = workspace_root.join(OPTIONAL_MODULE_DIRECTORY);
    let Ok(entries) = std::fs::read_dir(&optional_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let is_generated = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(HOST_PROJECT_MEMBER_PREFIX));
        if is_generated {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
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
    // project settings file
    // =========================================================================

    /// A project settings file yields its module list in order.
    #[test]
    fn project_settings_file_lists_modules() {
        let directory = temp_root().join("project_settings_parse");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("project_settings.yaml"),
            "name: \"Bouncing Balls\"\nbuild_binary_name: \"BouncingBalls\"\nmodules:\n  - \"pill_spline\"\n  - \"pill_dummy_math\"\n",
        )
        .unwrap();
        let settings = read_project_settings_file(&directory)
            .unwrap()
            .expect("the settings file should parse");
        assert_eq!(settings.name.as_deref(), Some("Bouncing Balls"));
        assert_eq!(settings.build_binary_name.as_deref(), Some("BouncingBalls"));
        assert_eq!(settings.modules, vec!["pill_spline", "pill_dummy_math"]);
    }

    /// A missing project settings file selects no optional modules.
    #[test]
    fn missing_project_settings_file_selects_no_modules() {
        let directory = temp_root().join("project_settings_missing");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        assert!(read_project_settings_file(&directory).unwrap().is_none());
    }

    // =========================================================================
    // manifest_depends_on_crate
    // =========================================================================

    /// A direct dependency declared as an inline table is detected.
    #[cfg(feature = "hot_reload")]
    #[test]
    fn direct_dependency_is_detected() {
        assert!(manifest_depends_on_crate(DEPENDENT_MANIFEST, "pill_spline"));
    }

    /// A dependency renamed through `package = "..."` is detected.
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
    #[test]
    fn quoted_key_rename_is_detected() {
        let manifest = r#"
[dependencies]
"spline-path" = { package = "pill_spline", path = "../../modules/optional/pill_spline" }
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// Dev and build dependencies also link the crate into the build.
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
    #[test]
    fn target_specific_dependency_is_detected() {
        let manifest = r#"
[target.'cfg(windows)'.dependencies]
pill_spline = { path = "../../modules/optional/pill_spline" }
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A dependency written as its own sub-table is detected.
    #[cfg(feature = "hot_reload")]
    #[test]
    fn sub_table_dependency_is_detected() {
        let manifest = r#"
[dependencies.pill_spline]
path = "../../modules/optional/pill_spline"
"#;
        assert!(manifest_depends_on_crate(manifest, "pill_spline"));
    }

    /// A manifest that never mentions the crate reports no dependency.
    #[cfg(feature = "hot_reload")]
    #[test]
    fn missing_dependency_is_not_detected() {
        assert!(!manifest_depends_on_crate(
            INDEPENDENT_MANIFEST,
            "pill_spline"
        ));
    }

    /// A crate listed only under `[workspace.dependencies]` is shared
    /// infrastructure, not something this project links directly.
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
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

    /// A project manifest with its own workspace tables, like `tests/project`.
    const WORKSPACE_MANIFEST: &str = r#"
[package]
name = "project"
edition = "2021"

[dependencies]
pill_engine = { path = "../../modules/pill_engine" }
serde = { version = "1", features = ["derive"] }

[workspace.dependencies]
shared = { version = "1" }

[workspace]
"#;

    /// The `[workspace]` and `[workspace.dependencies]` tables are dropped so
    /// the generated member is not mistaken for a nested workspace root.
    #[test]
    fn strips_workspace_tables_from_project_manifest() {
        let stripped = strip_workspace_tables(WORKSPACE_MANIFEST);
        assert!(
            !stripped.contains("[workspace"),
            "workspace table still present"
        );
        assert!(stripped.contains("[package]"), "package table was dropped");
        assert!(stripped.contains("serde ="), "dependency was dropped");
    }

    /// The project manifest is read from disk and a direct dependency found.
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
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
    #[cfg(feature = "hot_reload")]
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

/// Ensure the generated member's `[lib]` section builds an `rlib` as well.
///
/// A hot patch links the project crate to name its types, which requires an
/// rlib. Only called when the host was built with `hot_patch`, so a project
/// that never gets patched keeps building one artifact.
///
/// Rewrites an existing `crate-type` list in place and leaves a manifest that
/// already asks for `rlib` untouched, so the transform is idempotent.
#[cfg(feature = "hot_patch")]
fn add_rlib_crate_type(manifest: &str) -> String {
    let Some(lib_header) = manifest.find("[lib]") else {
        return manifest.to_string();
    };
    // The `[lib]` section ends at the next bracketed header.
    let section_end = manifest[lib_header..]
        .find("\n[")
        .map(|offset| lib_header + offset)
        .unwrap_or(manifest.len());
    let section = &manifest[lib_header..section_end];

    let Some(key_offset) = section.find("crate-type") else {
        // No crate-type at all: the default is `rlib` already.
        return manifest.to_string();
    };
    let Some(open) = section[key_offset..].find('[').map(|o| key_offset + o) else {
        return manifest.to_string();
    };
    let Some(close) = section[open..].find(']').map(|o| open + o) else {
        return manifest.to_string();
    };

    let list = &section[open + 1..close];
    if list.contains("rlib") {
        return manifest.to_string();
    }

    let mut rewritten = String::with_capacity(manifest.len() + 8);
    rewritten.push_str(&manifest[..lib_header + close]);
    rewritten.push_str(", \"rlib\"");
    rewritten.push_str(&manifest[lib_header + close..]);
    rewritten
}

#[cfg(test)]
mod profile_tests {
    use super::{
        host_profile_name, host_target_directory, profile_name_for_directory, profile_uses_lto,
    };

    /// The one profile whose cargo name and target directory differ.
    #[test]
    fn the_default_profile_is_named_dev_but_builds_into_debug() {
        assert_eq!(profile_name_for_directory("debug"), "dev");
    }

    /// Only the release family enables LTO, which is what forces spawned builds
    /// to drop `-C prefer-dynamic`. Dev-like launcher profiles (for example
    /// the dioxus CLI's `desktop-dev`) must keep it so modules stay dynamic.
    #[test]
    fn only_the_release_family_uses_lto() {
        for lto_profile in ["release", "release-fast", "release-with-debug"] {
            assert!(
                profile_uses_lto(lto_profile),
                "{lto_profile} must be treated as an LTO profile"
            );
        }
        for dynamic_profile in ["dev", "desktop-dev", "shipping", "test", "bench"] {
            assert!(
                !profile_uses_lto(dynamic_profile),
                "{dynamic_profile} must keep `-C prefer-dynamic`"
            );
        }
    }

    /// Every other profile's directory is its own name, including custom ones.
    #[test]
    fn every_other_profile_directory_is_its_own_name() {
        for directory in ["release", "release-fast", "release-with-debug", "shipping"] {
            assert_eq!(
                profile_name_for_directory(directory),
                directory,
                "{directory} should map to itself"
            );
        }
    }

    /// Whatever the build script recorded has to be usable as a cargo
    /// `--profile` value, or every module build this host runs fails.
    #[test]
    fn the_recorded_profile_is_a_usable_cargo_argument() {
        let name = host_profile_name();
        assert!(!name.is_empty(), "the profile name must not be empty");
        assert!(
            !name.starts_with('-'),
            "{name:?} would be parsed as a flag, not a profile"
        );
        assert!(
            !name.contains(char::is_whitespace),
            "{name:?} would split into two arguments"
        );
    }

    /// The output directory must be the workspace-relative `target/<profile>`
    /// cargo actually writes into, not an absolute or parent-relative path.
    #[test]
    fn the_target_directory_is_workspace_relative() {
        let directory = host_target_directory();
        assert!(
            directory.starts_with("target/"),
            "{directory:?} must live under the workspace target directory"
        );
        assert!(
            !directory.contains(".."),
            "{directory:?} must not escape it"
        );
    }

    /// Under `cargo test` the host is a debug build, which is the one case
    /// where the two vocabularies differ - so this pins the pairing end to end.
    #[test]
    fn the_name_and_the_directory_describe_the_same_profile() {
        let expected_name = profile_name_for_directory(
            host_target_directory()
                .strip_prefix("target/")
                .expect("the directory is workspace-relative"),
        )
        .to_string();
        assert_eq!(host_profile_name(), expected_name);
    }
}

#[cfg(test)]
mod host_project_member_validation_tests {
    use super::{
        manifest_entry_name, materialize_host_project_member, HOST_PROJECT_MEMBER_PREFIX,
        OPTIONAL_MODULE_DIRECTORY,
    };
    use pill_core::error::ConfigError;

    /// A project manifest whose one path dependency is filled in per test, so
    /// each case differs only in whether that path resolves.
    fn manifest_with_dependency(dependency_path: &str) -> String {
        format!(
            "[package]\nname = \"project\"\nversion = \"0.1.0\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\npill_engine = {{ path = \"{dependency_path}\" }}\n"
        )
    }

    /// Lays out a workspace with a project at `project/` and returns its root.
    ///
    /// The project directory is a sibling of `optional/`, matching the real
    /// layout closely enough that relative paths behave the same way.
    fn workspace(test_name: &str, dependency_path: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("pill_member_{test_name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(OPTIONAL_MODULE_DIRECTORY)).unwrap();
        std::fs::create_dir_all(root.join("project").join("src")).unwrap();
        std::fs::write(
            root.join("project").join("Cargo.toml"),
            manifest_with_dependency(dependency_path),
        )
        .unwrap();
        std::fs::write(root.join("project").join("src").join("lib.rs"), "").unwrap();
        root
    }

    /// Path of the member `materialize_host_project_member` would generate.
    fn member_directory(root: &std::path::Path) -> std::path::PathBuf {
        root.join(OPTIONAL_MODULE_DIRECTORY)
            .join(format!("{HOST_PROJECT_MEMBER_PREFIX}project"))
    }

    #[test]
    fn writes_the_member_when_every_path_resolves() {
        let root = workspace("resolves", "../real_dependency");
        std::fs::create_dir_all(root.join("real_dependency")).unwrap();

        materialize_host_project_member(&root, "project", "project").unwrap();

        assert!(member_directory(&root).join("Cargo.toml").is_file());
    }

    #[test]
    fn refuses_to_write_a_member_with_an_unresolvable_path() {
        let root = workspace("unresolvable", "../../modules/pill_engine");

        let error = materialize_host_project_member(&root, "project", "project")
            .expect_err("a dependency path that does not exist must be reported");

        match error {
            ConfigError::ProjectDependencyPathMissing {
                ref dependency,
                ref resolved_path,
                ..
            } => {
                assert_eq!(dependency, "pill_engine");
                assert!(
                    resolved_path.contains("pill_engine"),
                    "the message must name the path that failed, got {resolved_path}"
                );
            }
            other => panic!("expected ProjectDependencyPathMissing, got {other:?}"),
        }
        assert!(
            !member_directory(&root).join("Cargo.toml").exists(),
            "a member Cargo cannot load must never reach the workspace"
        );
    }

    #[test]
    fn clears_a_member_an_earlier_run_left_behind() {
        // The failure this guards against: a member written when the paths did
        // resolve, then invalidated by the project moving. Cargo then refuses
        // to load the workspace, so the build that would rewrite the member
        // cannot run - the host has to clear it on the way out.
        let root = workspace("clears_stale", "../gone");
        let stale = member_directory(&root);
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("Cargo.toml"), "[package]\nname = \"stale\"\n").unwrap();

        let _ = materialize_host_project_member(&root, "project", "project")
            .expect_err("the dependency path does not exist");

        assert!(!stale.exists(), "the unloadable member must be removed");
    }

    #[test]
    fn names_the_dependency_that_owns_a_path() {
        let manifest = manifest_with_dependency("../x");
        let offset = manifest.find("path = \"").unwrap();
        assert_eq!(manifest_entry_name(&manifest, offset), "pill_engine");
    }

    #[test]
    fn falls_back_to_the_section_for_a_bare_path() {
        let manifest = "[lib]\npath = \"src/lib.rs\"\n";
        let offset = manifest.find("path = \"").unwrap();
        assert_eq!(manifest_entry_name(manifest, offset), "[lib]");
    }
}

#[cfg(test)]
mod host_project_member_pruning_tests {
    use super::{prune_stale_host_project_members, OPTIONAL_MODULE_DIRECTORY};

    /// Creates `optional/<name>/Cargo.toml` under `root` and returns its
    /// directory, so a test can assert on the directory rather than the file.
    fn seed_member(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let directory = root.join(OPTIONAL_MODULE_DIRECTORY).join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("Cargo.toml"),
            "[package]
name = \"x\"
",
        )
        .unwrap();
        directory
    }

    /// Builds an empty temporary workspace root, removing any previous run's.
    fn workspace(test_name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("pill_prune_{test_name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(OPTIONAL_MODULE_DIRECTORY)).unwrap();
        root
    }

    #[test]
    fn removes_a_member_left_by_a_previous_project() {
        let root = workspace("previous");
        let stale = seed_member(&root, "host_project_old");
        let keep = seed_member(&root, "host_project_current");

        prune_stale_host_project_members(&root, &keep);

        assert!(
            !stale.exists(),
            "the previous project's member must be gone"
        );
        assert!(keep.exists(), "the member being materialized must survive");
    }

    #[test]
    fn leaves_real_optional_modules_alone() {
        let root = workspace("real_modules");
        let module = seed_member(&root, "pill_spline");
        let keep = seed_member(&root, "host_project_current");

        prune_stale_host_project_members(&root, &keep);

        assert!(
            module.exists(),
            "only host-generated members carry the prefix; a real module must not be deleted"
        );
    }

    #[test]
    fn tolerates_a_missing_optional_directory() {
        let root = std::env::temp_dir().join("pill_prune_missing");
        let _ = std::fs::remove_dir_all(&root);

        // Must not panic: a workspace with no `optional/` directory yet is
        // valid, and refusing to start over it would be worse than the stale
        // member this function exists to clean up.
        prune_stale_host_project_members(&root, &root.join("nothing"));
    }
}

#[cfg(all(test, feature = "hot_patch"))]
mod hot_patch_manifest_tests {
    use super::add_rlib_crate_type;

    #[test]
    fn adds_rlib_to_an_existing_crate_type_list() {
        let manifest =
            "[package]\nname = \"project\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n";
        let rewritten = add_rlib_crate_type(manifest);
        assert!(rewritten.contains("crate-type = [\"cdylib\", \"rlib\"]"));
        assert!(rewritten.contains("[dependencies]"));
    }

    #[test]
    fn is_idempotent() {
        let manifest = "[lib]\ncrate-type = [\"cdylib\", \"rlib\"]\n";
        assert_eq!(add_rlib_crate_type(manifest), manifest);
        assert_eq!(
            add_rlib_crate_type(&add_rlib_crate_type(manifest)),
            manifest
        );
    }

    #[test]
    fn leaves_a_manifest_without_crate_type_alone() {
        // No `crate-type` means the default, which already includes `rlib`.
        let manifest = "[lib]\npath = \"src/lib.rs\"\n";
        assert_eq!(add_rlib_crate_type(manifest), manifest);
    }

    #[test]
    fn leaves_a_manifest_without_a_lib_section_alone() {
        let manifest = "[package]\nname = \"project\"\n";
        assert_eq!(add_rlib_crate_type(manifest), manifest);
    }
}
