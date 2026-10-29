//! Every subsystem error and the composition root over them.
//!
//! # Responsibilities
//!
//! - Declare the subsystem error each area returns: configuration, build,
//!   library loading, module loading, the file watcher, and the managed side.
//! - Compose them into [`HostError`] through transparent `#[from]` wrappers, so
//!   `?` works across a subsystem boundary and the leaf error plus its source
//!   chain survive.
//!
//! # Design
//!
//! Each variant carries one `#[message(...)]` definition; the `#[engine_error]`
//! macro generates the plain `Display`, a stable `namespace::variant` code and
//! the [`EngineMessage`] implementation that renders the same definition with
//! semantic roles. Nothing here names a presentation medium.

use super::*;

// =============================================================================
// Subsystem Errors
// =============================================================================

/// Configuration-validation failures for project-module setups.
///
/// Raised before any build work begins, so a misconfigured module fails fast
/// with a precise diagnostic instead of a confusing runtime error.
#[engine_error(namespace = host::config)]
pub enum ConfigError {
    /// The configured module name is empty.
    #[message("module name must not be empty")]
    EmptyModuleName,

    /// The configured watch directory is empty.
    #[message("watch directory must not be empty")]
    EmptyWatchDirectory,

    /// The configured build command is empty.
    #[message("build command must not be empty")]
    EmptyBuildCommand,

    /// A required configuration environment variable was not set.
    #[message("required environment variable ", value(variable), " is not set")]
    MissingEnvironmentVariable { variable: &'static str },

    /// A configuration environment variable holds a value the host does not
    /// accept.
    #[message(
        "environment variable ",
        value(variable),
        " is set to ",
        value(value),
        "; expected ",
        value(expected)
    )]
    InvalidEnvironmentVariable {
        variable: &'static str,
        value: String,
        expected: &'static str,
    },

    /// The directory the engine workspace lives in could not be determined.
    ///
    /// Every stored path is relative to that directory, and it is derived from
    /// this crate's own build location rather than from the process's working
    /// directory: the two agree only when the host happens to be started from
    /// the workspace root, and a launcher that sets a working directory of its
    /// own would otherwise resolve `extensions/` to a directory that does not
    /// exist.
    #[message("engine workspace root could not be determined from the host build")]
    EngineWorkspaceRootUndetermined,

    /// The configured project directory does not exist.
    #[message("project directory does not exist: ", name_style(path))]
    ProjectDirectoryMissing { path: String },

    /// The project directory contains no Cargo.toml or .csproj manifest.
    #[message(
        "no project manifest (Cargo.toml or .csproj) found in ",
        name_style(path)
    )]
    ProjectManifestMissing { path: String },

    /// The project manifest could not be read.
    #[message("failed to read project manifest ", name_style(path))]
    ProjectManifestReadFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The project manifest is not valid TOML, so no workspace member can be
    /// generated from it.
    #[message(
        "failed to parse project manifest ",
        name_style(path),
        ": ",
        value(details)
    )]
    ProjectManifestParseFailed { path: String, details: String },

    /// The project manifest does not declare a package name.
    #[message(
        "project manifest ",
        name_style(path),
        " does not declare a package name"
    )]
    ProjectPackageNameMissing { path: String },

    /// The generated host workspace member for a native project could not be
    /// created or written.
    #[message("failed to create generated host project member at ", name_style(path))]
    HostProjectMemberCreationFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The generated workspace member that wraps an extension as a loadable
    /// artifact could not be created or written.
    #[message("failed to create generated host module wrapper at ", name_style(path))]
    HostModuleMemberCreationFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A `path` dependency in the project manifest does not resolve to an
    /// existing directory once rewritten for the generated workspace member.
    ///
    /// Reported instead of writing the member, because a member Cargo cannot
    /// load breaks every build in the workspace - including the one that would
    /// replace it.
    #[message(
        "project manifest ",
        name_style(manifest_path),
        " declares dependency ",
        name_style(dependency),
        " at a path that does not exist: ",
        name_style(resolved_path)
    )]
    ProjectDependencyPathMissing {
        manifest_path: String,
        dependency: String,
        resolved_path: String,
    },

    /// The project's settings file does not exist.
    #[message("project settings file not found at ", name_style(path))]
    ProjectSettingsFileMissing { path: String },

    /// The project's settings file could not be read.
    #[message("failed to read project settings file ", name_style(path))]
    ProjectSettingsFileReadFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The project's settings file could not be parsed.
    #[message(
        "failed to parse project settings file ",
        name_style(path),
        ": ",
        value(details)
    )]
    ProjectSettingsFileParseFailed { path: String, details: String },

    /// The project's settings file does not declare a `name`.
    #[message(
        "project settings file ",
        name_style(path),
        " must declare a `name` (used for the window title)"
    )]
    MissingProjectName { path: String },

    /// The project's settings file's `build_binary_name` is missing or not
    /// filename-safe.
    #[message(
        "project settings file ",
        name_style(path),
        " must declare a `build_binary_name` with only letters, digits and underscores (no spaces or special characters)"
    )]
    InvalidBuildBinaryName { path: String },

    /// The settings file's `logging:` section names a level or a target that
    /// does not read.
    #[message(
        "project settings file ",
        name_style(path),
        " has an invalid `logging:` entry: ",
        value(details)
    )]
    InvalidLoggingSettings { path: String, details: String },

    /// The settings file's `startup_scene:` is not a relative `.scene` path
    /// below the asset directory.
    #[message(
        "project settings file ",
        name_style(path),
        " has an invalid `startup_scene:`: ",
        value(details),
        " (use a relative path to a .scene file below `res`)"
    )]
    InvalidStartupScene { path: String, details: String },

    /// An extension name in the settings file is not a usable crate
    /// directory name.
    #[message(
        "project settings file lists extension ",
        value(name),
        ", which is not a crate directory name (letters, digits, `_`, `-`; must start with a letter)"
    )]
    InvalidExtensionName { name: String },

    /// The settings file lists one extension twice.
    #[message("project settings file lists extension ", value(name), " twice")]
    DuplicateExtensionName { name: String },

    /// The settings file lists an editor crate, which only loads with its
    /// extension in an editor host.
    #[message(
        "extension ",
        value(name),
        " is an editor crate: it loads with its extension in an editor host, never from `modules:`"
    )]
    EditorCrateListed { name: String },

    /// An editor crate is tagged for the `pill_ui` editor frontend, which no
    /// host implements.
    #[message(
        "editor crate ",
        value(name),
        " is tagged `editor_frontend = \"pill_ui\"`, the retired editor; tag it `egui` or drop the tag"
    )]
    RetiredEditorFrontend { name: String },

    /// No extension folder holds the configured extension crate.
    #[message(
        "extension ",
        value(name),
        " is in no extension folder under extensions/"
    )]
    ExtensionDirectoryMissing { name: String },

    /// The asset pack a shipping build embeds could not be read.
    #[message(
        "the embedded asset pack of ",
        name_style(project),
        " is damaged: ",
        value(detail)
    )]
    AssetPackInvalid { project: String, detail: String },
}

/// Project-module build execution failures.
///
/// Covers spawn, wait, timeout, cancellation, and missing-artifact failures
/// of the external command that builds a project module.
#[engine_error(namespace = host::build)]
pub enum BuildError {
    /// The build command list is empty.
    #[message("build command must not be empty")]
    EmptyCommand,

    /// The build executable could not be spawned.
    #[message("failed to spawn build command for ", name_style(name))]
    SpawnFailed {
        name: String,
        #[source]
        source: std::io::Error,
    },

    /// The build child could not be waited on.
    #[message("failed to wait for the build command of ", name_style(name))]
    WaitFailed {
        name: String,
        #[source]
        source: std::io::Error,
    },

    /// The build command exited with a non-zero status.
    #[message(
        "build command failed for ",
        name_style(name),
        " with status ",
        value(status)
    )]
    CommandFailed {
        name: String,
        status: std::process::ExitStatus,
    },

    /// The build command exceeded its wall-clock deadline.
    #[message(
        "build command for ",
        name_style(name),
        " timed out after ",
        value(seconds),
        " seconds"
    )]
    TimedOut { name: String, seconds: u64 },

    /// Sources changed again while the build was running.
    #[message("build cancelled: sources changed again during compilation")]
    Cancelled,

    /// The build succeeded but no artifact appeared at the configured path.
    #[message("shared library not found at expected path: ", name_style(path))]
    #[diagnostic(help("check the selected backend output directory in ProjectModuleConfig"))]
    OutputMissing { path: String },

    /// The built library could not be staged into its hot-load directory.
    #[message(
        "failed to copy built library ",
        name_style(source_path),
        " to hot-load path ",
        name_style(target_path)
    )]
    HotArtifactCopyFailed {
        source_path: String,
        target_path: String,
        #[source]
        source: std::io::Error,
    },

    /// A staged artifact changed after the build that produced it finished.
    #[message(
        "staged artifact ",
        name_style(path),
        " no longer matches what was built for ",
        name_style(name),
        "; another build or a cancelled compiler is writing to the staging directory"
    )]
    #[diagnostic(help("retry the build; check for a stray rustc or linker process"))]
    StagedArtifactChanged { name: String, path: String },
}

/// Native project-library loading and initialization failures.
///
/// Covers the temporary-directory staging, the copy into place, dynamic
/// loading, and the `pill_module_init` entry-point contract.
///
/// The loader's own error is a boxed source: `pill_core` is the shared dylib
/// every posture links, so it may not name `libloading`. The cause is
/// still reported through `Error::source()` and the message chain.
#[engine_error(namespace = host::library)]
pub enum LibraryError {
    /// The per-process temporary directory could not be created.
    #[message(
        "failed to prepare temporary library directory ",
        name_style(directory)
    )]
    TemporaryDirectory {
        directory: String,
        #[source]
        source: std::io::Error,
    },

    /// The built library could not be copied to its temporary path.
    #[message(
        "failed to copy built library ",
        name_style(source_path),
        " to ",
        name_style(target_path)
    )]
    CopyFailed {
        source_path: String,
        target_path: String,
        #[source]
        source: std::io::Error,
    },

    /// The temporary copy is not a loadable native library.
    #[message("failed to load native project library from ", name_style(path))]
    LoadFailed {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The artifact was linked against a different engine dylib than the host
    /// has mapped, so the loader cannot resolve its imports.
    ///
    /// Cargo folds a dependency's resolved features into the dependent's
    /// `-C metadata`, and that hash is part of every symbol name `pill_core`
    /// exports. Two builds that resolve different graphs therefore produce two
    /// `pill_core.dll` files whose symbols do not match, and the artifact built
    /// against one cannot load against the other. Windows reports that as
    /// `LoadLibrary` error 127, "The specified procedure could not be found",
    /// which names neither the dylib nor the feature that moved.
    ///
    /// Staging the matching dylib beside the artifact does not help: the loader
    /// resolves a DLL that is already mapped by its module name, whatever
    /// directory the new one sits in, and the host mapped `pill_core.dll`
    /// before it built anything.
    #[message(
        "native library for ",
        name_style(subject),
        " was built against a different engine dylib than this host runs, so its ",
        "imports cannot resolve. The host maps ",
        name_style(host_engine),
        " and the artifact links ",
        name_style(module_engine),
        ". The two builds resolved different dependency graphs, which changes ",
        "every exported symbol name; the usual cause is a headless host against ",
        "a project that links the renderer. Either run the host with the ",
        "feature set the project implies (`--features rendering` for the ",
        "renderer), or build the host and the project in one cargo invocation ",
        "so both resolve the same graph"
    )]
    EngineDylibMismatch {
        subject: String,
        host_engine: String,
        module_engine: String,
    },

    /// A named import the module needs is not exported by the copy of the
    /// imported DLL the loader would use.
    ///
    /// Windows reports a missing named import as `LoadLibrary` error 127,
    /// "The specified procedure could not be found", naming neither the DLL
    /// nor the symbol. The module's own import tables against the candidate
    /// copy's export table say exactly which symbol is missing and where it
    /// was looked for.
    #[message(
        "native library for ",
        name_style(subject),
        " imports ",
        name_style(symbol),
        " from ",
        name_style(dll),
        ", which does not export it; the copy the loader would use is ",
        name_style(path)
    )]
    MissingImport {
        subject: String,
        dll: String,
        symbol: String,
        path: String,
    },

    /// The library does not export a required entry point.
    ///
    /// Names the subject as well as the symbol: a project and an extension
    /// export the same entry points, so the symbol alone does not say which
    /// artifact is missing it.
    #[message(
        "native library for ",
        name_style(subject),
        " is missing required export ",
        name_style(symbol)
    )]
    MissingExport {
        subject: String,
        symbol: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// `pill_module_init` reported a failed generation.
    #[message("project module initialization failed with status ", value(status))]
    InitializationFailed { status: u32 },
}

/// Extension loading and compatibility failures.
///
/// Distinct from [`LibraryError`], which covers the mechanics of mapping a
/// library: these report a broken contract between the host and a module.
#[engine_error(namespace = host::module)]
pub enum ModuleError {
    /// The module was built against a different module ABI revision.
    #[message(
        "extension ",
        name_style(module),
        " reports ABI version ",
        value(module_version),
        " but the host requires ",
        value(host_version)
    )]
    #[diagnostic(help("rebuild the module against the current engine sources"))]
    AbiVersionMismatch {
        module: String,
        module_version: u32,
        host_version: u32,
    },

    /// The module does not export an ABI revision at all.
    #[message(
        "extension ",
        name_style(module),
        " does not export pill_module_abi_version"
    )]
    #[diagnostic(help("an extension must export pill_module_abi_version and pill_module_init"))]
    AbiVersionMissing { module: String },

    /// The module's registration entry point reported a failed generation.
    #[message(
        "extension ",
        name_style(module),
        " initialization failed with status ",
        value(status)
    )]
    InitializationFailed { module: String, status: u32 },

    /// A module whose failures clear only its systems registered data.
    ///
    /// Clearing only systems on a failed load is sound because such a module
    /// owns no data: its components and resources are registered by someone
    /// else. One that registers its own is refused rather than trusted.
    #[message(
        "extension ",
        name_style(module),
        " registered ",
        value(components),
        " component type(s) and ",
        value(resources),
        " resource type(s), but may register no data"
    )]
    #[diagnostic(help(
        "register the data in pill_renderer_api, which the host registers before any module loads"
    ))]
    RegisteredForbiddenData {
        module: String,
        components: usize,
        resources: usize,
    },
}

/// Source-watching startup failures.
///
/// Distinct from build failures: the watcher must start before any build can
/// be triggered by a source change.
///
/// The watcher's own error is a boxed source: `pill_core` is the shared dylib
/// every posture links, so it may not name `notify`. The cause is
/// still reported through `Error::source()` and the message chain.
#[engine_error(namespace = host::watcher)]
pub enum WatcherError {
    /// The configured watch directory does not exist.
    #[message("watch directory does not exist: ", name_style(path))]
    WatchDirectoryMissing { path: String },

    /// The platform file watcher could not be created.
    #[message("failed to create file watcher")]
    CreationFailed {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The watch path could not be registered.
    #[message("failed to register watch path ", name_style(path))]
    RegistrationFailed {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Managed (C#) backend startup and registration failures.
///
/// Covers hostfxr discovery and loading, runtime delegates, the component
/// manifest exchange, and reflected managed startup.
///
/// The loader's own error is a boxed source: `pill_core` is the shared dylib
/// every posture links, so it may not name `libloading`. The cause is
/// still reported through `Error::source()` and the message chain.
#[engine_error(namespace = host::csharp)]
pub enum CSharpError {
    /// No hostfxr installation was found in any configured dotnet root.
    #[message("no .NET hostfxr installation found; searched the configured dotnet roots")]
    HostfxrNotFound,

    /// The `ECS_DOTNET_HOSTFXR` override does not point at a file.
    #[message("ECS_DOTNET_HOSTFXR does not point at a file: ", name_style(path))]
    HostfxrOverrideNotAFile { path: String },

    /// The selected hostfxr library could not be loaded.
    #[message("failed to load hostfxr from ", name_style(path))]
    HostfxrLoadFailed {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The NativeAOT library could not be found on disk.
    #[message("no NativeAOT library at ", name_style(path))]
    AotLibraryNotFound { path: String },

    /// The NativeAOT library could not be loaded.
    #[message("failed to load NativeAOT library from ", name_style(path))]
    AotLibraryLoadFailed {
        path: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A required export is missing from the NativeAOT library.
    #[message("failed to resolve NativeAOT export ", name_style(symbol))]
    AotExportMissing {
        symbol: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A required hostfxr export is missing.
    #[message("failed to resolve hostfxr export ", name_style(symbol))]
    HostfxrExportMissing {
        symbol: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Runtime initialization failed with a negative hostfxr status.
    #[message("hostfxr_initialize_for_runtime_config failed with ", value(status))]
    RuntimeInitializationFailed { status: String },

    /// The runtime delegate request failed with a negative hostfxr status.
    #[message("hostfxr_get_runtime_delegate failed with ", value(status))]
    RuntimeDelegateFailed { status: String },

    /// The managed export type is not a pointer-sized function pointer.
    #[message("managed export type is not pointer-sized")]
    ManagedExportNotPointerSized,

    /// A managed entry point could not be resolved or returned no pointer.
    #[message(
        "load_assembly_and_get_function_pointer failed for ",
        name_style(method),
        " with ",
        value(status)
    )]
    ManagedEntryPointFailed { method: String, status: String },

    /// The managed loader rejected host initialization.
    #[message("csharp_runtime initialization failed")]
    RuntimeInitFailed,

    /// The host-side C# mirror codegen step failed.
    #[message("C# component mirror generation failed: ", value(message))]
    CodegenFailed { message: String },

    /// The managed runtime was built for a different ABI contract version.
    #[message(
        "csharp_runtime interop version mismatch: host expects ",
        value(expected),
        " but the runtime reports ",
        value(actual)
    )]
    #[diagnostic(help("rebuild csharp_runtime and the host together"))]
    InteropVersionMismatch { expected: u32, actual: u32 },

    /// The managed-reported manifest length lies outside the supported range.
    #[message(
        "component manifest length ",
        value(length),
        " is outside the supported range (1..=",
        value(limit),
        ")"
    )]
    ManifestLengthOutOfRange { length: u32, limit: u32 },

    /// A managed-reported system or access count exceeds what the host reflects.
    ///
    /// The counts size host allocations before any item is read, so they are
    /// checked against fixed caps; without them a buggy assembly could ask for
    /// an allocation that aborts the process instead of failing startup.
    #[message(
        "managed-reported count ",
        value(count),
        " exceeds the supported limit of ",
        value(limit)
    )]
    SystemCountOutOfRange { count: u32, limit: u32 },

    /// The reflected system snapshot could not be allocated.
    #[message("out of memory allocating the reflected system snapshot")]
    SystemSnapshotAllocationFailed,

    /// The managed loader reported a reload status the host does not know.
    ///
    /// The poll answers `0` (nothing happened), `1` (swapped) or `2`
    /// (rejected). Anything else is an ABI drift or a loader-level failure, and
    /// filing it as "nothing happened" would leave the previous assembly
    /// running with no indication anywhere.
    #[message("the managed loader reported unknown reload status ", value(status))]
    UnknownPollStatus { status: u8 },

    /// The manifest buffer could not be allocated.
    #[message("out of memory allocating the component manifest buffer")]
    ManifestAllocationFailed,

    /// The manifest copy from the managed runtime failed.
    #[message("failed to copy the component manifest from the managed runtime")]
    ManifestCopyFailed,

    /// The managed component manifest failed host-side JSON parsing.
    #[message("failed to parse the component manifest JSON")]
    ManifestParseFailed {
        #[source]
        source: serde_json::Error,
    },

    /// The managed component manifest failed host-side validation.
    #[message("invalid component manifest: ", value(message))]
    ManifestInvalid { message: String },

    /// A reflected managed startup method failed.
    #[message("startup method ", value(index), " failed")]
    StartupFailed { index: u32 },

    /// Queued startup commands failed to apply.
    #[message("startup commands failed: ", value(details))]
    StartupCommandsFailed { details: String },

    /// The managed assembly contains no schedulable systems.
    #[message(
        "C# assembly ",
        name_style(subject),
        " declares no [EcsSystem] methods"
    )]
    NoSystems {
        /// Assembly that was inspected, so the report names the artifact that
        /// failed rather than a fixed example project.
        subject: String,
    },

    /// The managed runtime refused to copy one system access declaration.
    #[message(
        "failed to get access ",
        value(access),
        " for C# system ",
        value(system)
    )]
    SystemAccessFailed { system: u32, access: u32 },

    /// A reflected access mode is neither read nor write.
    #[message("unknown C# access mode ", value(mode))]
    UnknownAccessMode { mode: u8 },

    /// A reflected access references a component the host never registered.
    #[message("C# system references unregistered component key ", value(key))]
    UnregisteredComponent { key: String },

    /// A reflected access references a resource the host never registered.
    ///
    /// Almost always a resource struct that a system declares through
    /// `Res<T>`/`ResMut<T>` without carrying `[EcsResource]`, so the manifest
    /// never told the host it existed.
    #[message("C# system references unregistered resource key ", value(key))]
    UnregisteredResource { key: String },

    /// A managed string passed to hostfxr contains an interior NUL.
    #[message(".NET host string contains an interior NUL")]
    InteriorNul,
}

impl From<String> for CSharpError {
    /// Route free-form manifest validation messages into the typed
    /// `ManifestInvalid` variant.
    fn from(message: String) -> Self {
        Self::ManifestInvalid { message }
    }
}

impl From<serde_json::Error> for CSharpError {
    /// Preserve the real JSON parse error instead of converting it to text.
    fn from(source: serde_json::Error) -> Self {
        Self::ManifestParseFailed { source }
    }
}

// =============================================================================
// Host Error Composition
// =============================================================================

/// Transparent composition of every host subsystem error.
///
/// Subsystem functions return their narrowest meaningful error; [`HostError`]
/// exists only as the composition boundary where `?` crosses subsystems.
#[engine_error(namespace = host)]
pub enum HostError {
    /// Configuration validation failed.
    #[transparent]
    Config(#[from] ConfigError),

    /// The project-module build failed.
    #[transparent]
    Build(#[from] BuildError),

    /// Native library loading or initialization failed.
    #[transparent]
    Library(#[from] LibraryError),

    /// The source watcher could not start.
    #[transparent]
    Watcher(#[from] WatcherError),

    /// An extension failed to load or is incompatible.
    #[transparent]
    Module(#[from] ModuleError),

    /// The managed backend failed to start.
    #[transparent]
    CSharp(#[from] CSharpError),
}
