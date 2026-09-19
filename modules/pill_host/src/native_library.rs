//! Native project-library loading and Windows-safe temporary-copy handling.
//!
//! Loaded modules are copied to unique paths before opening. This permits a
//! newly compiled DLL to replace the build output while an older generation
//! remains mapped for outstanding function pointers and vtables.
//!
//! # Responsibilities
//!
//! - Load native project libraries from unique temporary paths.
//! - Validate required exports before returning a loaded library.
//! - Call native registration and per-frame update entry points.
//! - Remove temporary copies left behind by earlier host processes.
//!
//! # Design
//!
//! The native ABI is a fixed export contract, and there is exactly one of it.
//! A project and an optional module are the same loadable artifact - same
//! exports, same loader, same reload transaction, same graveyard - so the
//! loader has nothing to parameterise:
//!
//! - `pill_module_init(*const EngineApi) -> u32` — required. Registers
//!   components and systems before the first frame and returns zero on
//!   success; any other status aborts the load transaction.
//! - `pill_module_update(*const EngineApi)` — optional. Called once per frame
//!   by an artifact that keeps an explicit update hook.
//! - `pill_module_abi_version() -> u32` — optional, read before anything else
//!   is called so an incompatible artifact is rejected rather than invoked.
//!
//! The exports are resolved and cached when the library is loaded, so the
//! frame loop never performs a dynamic lookup or panics on a missing optional
//! export. `pill_module_init` must be idempotent: a failed generation is
//! rolled back by re-initializing the previous artifact.
//!
//! What differs between a project and a module is policy, not contract - a
//! project is singular where modules are an ordered list, and a module's
//! components are mirrored to C# - and that lives in the config and the reload
//! transaction rather than here.

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

// External crates
#[cfg(windows)]
use libloading::os::windows as windows_loader;
use libloading::{Library, Symbol};
use pill_core::error::LibraryError;
use pill_core::{debug, info};
use pill_engine::component_registry::{
    PillFieldAccessorDescriptor, PillMethodDescriptor, PillValueTypeDescriptor,
};
use pill_engine::EngineApi;

// Current crate
use crate::analytics;
use crate::csharp::{ResolvedFieldAccessor, ResolvedMirrorMethod};

// =============================================================================
// Constants
// =============================================================================

/// Directory where temporary native-library copies are stored.
const TEMPORARY_DIRECTORY: &str = "pill_standalone_temp";

/// How long another process's staging directory is left alone after its last
/// write.
///
/// This is the liveness check, and it is deliberately a clock rather than a
/// process probe. A running host stages a fresh copy of every DLL it loads and
/// of every one it reloads, so its directory's modification time keeps moving;
/// one that has not been written to for this long belonged to a process that
/// is gone. The window is generous because the cost of waiting is a few
/// megabytes of disk and the cost of being wrong is deleting the staging of a
/// host that is still running - and on Windows that deletion half-succeeds,
/// taking the files that are not mapped and leaving the ones that are.
const STAGING_GRACE_PERIOD: Duration = Duration::from_secs(60 * 60);

/// Maximum number of other processes' staging directories kept.
///
/// The backstop for the grace period above: without it, a developer who starts
/// many hosts in one hour accumulates one directory per host until the hour is
/// up. Bounded the way the retired-image graveyard is bounded - keep N, evict
/// the oldest, say so - and the oldest-first order is what makes it safe to
/// apply to directories still inside the grace period, because a live host's
/// directory is the most recently written of them all.
const MAX_RETAINED_STAGING_DIRECTORIES: usize = 8;

/// Monotonic suffix ensuring temporary copies never collide, even when the
/// system clock repeats or moves backwards.
static TEMPORARY_COPY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `LOAD_WITH_ALTERED_SEARCH_PATH`: make a module's dependency resolution start
/// from the directory that contains the module, so it loads the engine dylib
/// staged beside it rather than the host's copy from the executable directory.
#[cfg(windows)]
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

/// Whether a module build produced an engine dylib different from the one the
/// host has mapped, which means the module must load against its own copy.
///
/// When the two are byte-identical (a plain CLI host whose feature closure
/// matches the module build) the module keeps loading the host's single
/// instance; only a host whose graph unions different features onto the shared
/// engine crates (a GUI frontend) needs the isolated copy.
fn engine_dylib_needs_isolation(workspace_root: &Path) -> bool {
    let Some(module_engine) = module_world_engine_dylib(workspace_root) else {
        return false;
    };
    let host_engine = workspace_root
        .join(crate::config::host_target_directory())
        .join("pill_core.dll");
    if !host_engine.is_file() {
        return true;
    }
    !files_equal(&module_engine, &host_engine)
}

/// Explain a load failure that an engine-dylib mismatch accounts for.
///
/// A missing export is almost never a missing export. The usual cause is that
/// the artifact was linked against one `pill_core.dll` and is being loaded
/// against another: cargo folds a dependency's resolved features into the
/// dependent's `-C metadata`, that hash is part of every symbol name the dylib
/// exports, and two builds that resolve different dependency graphs therefore
/// disagree about names the loader can only report as "procedure not found".
///
/// Passing the original error through unchanged leaves the reader with
/// `os error 127` and nothing to act on, so a failure that coincides with two
/// differing engine dylibs is re-reported as the mismatch it is - with both
/// paths and the two ways out.
fn diagnose_load_failure(
    error: LibraryError,
    workspace_root: &Path,
    module_name: &str,
) -> LibraryError {
    // Only a load failure can be this; a missing export or a failed copy has
    // already said something specific and true.
    if !matches!(error, LibraryError::LoadFailed { .. }) {
        return error;
    }
    let Some(module_engine) = module_world_engine_dylib(workspace_root) else {
        return error;
    };
    let host_engine = workspace_root
        .join(crate::config::host_target_directory())
        .join("pill_core.dll");
    if host_engine.is_file() && files_equal(&module_engine, &host_engine) {
        // The two agree, so whatever failed is not this.
        return error;
    }
    LibraryError::EngineDylibMismatch {
        subject: module_name.to_string(),
        host_engine: host_engine.display().to_string(),
        module_engine: module_engine.display().to_string(),
    }
}

/// Copy the module-world engine dylib into `directory` so a module loaded from
/// there resolves it co-located instead of the host's copy.
fn stage_module_engine_dylib(workspace_root: &Path, directory: &Path) {
    let Some(source) = module_world_engine_dylib(workspace_root) else {
        return;
    };
    if std::fs::create_dir_all(directory).is_err() {
        return;
    }
    let _ = std::fs::copy(source, directory.join("pill_core.dll"));
}

/// Locate the engine dylib the module build produced: the staged hot-load copy
/// when a build has run, otherwise the one still in the private build tree.
fn module_world_engine_dylib(workspace_root: &Path) -> Option<PathBuf> {
    let staged = workspace_root
        .join(crate::build_runner::PROJECT_HOT_OUTPUT_SUBDIRECTORY)
        .join("pill_core.dll");
    if staged.is_file() {
        return Some(staged);
    }
    let built = workspace_root
        .join(crate::config::module_build_artifact_directory())
        .join("pill_core.dll");
    built.is_file().then_some(built)
}

/// Byte equality for two DLL files.
///
/// Gated on length first, because both files are multi-megabyte engine
/// dylibs compared on every `load_copy` and a length mismatch is a proof of
/// inequality. Everything else goes through [`files_equal_streaming`]:
/// timestamps cannot answer either way - a copied file keeps or renews them
/// while staying byte-identical, and two files can share a timestamp while
/// differing - and this comparison decides whether a module shares the host's
/// engine instance or gets its own staged copy. A false "different" would map
/// `pill_core.dll` twice and break the one-instance contract the engine's raw
/// pointers rely on, so only exact bytes may answer "equal".
fn files_equal(left: &Path, right: &Path) -> bool {
    if let (Ok(left_metadata), Ok(right_metadata)) =
        (std::fs::metadata(left), std::fs::metadata(right))
    {
        if left_metadata.len() != right_metadata.len() {
            return false;
        }
    }
    // Unreadable metadata falls through rather than answering "different":
    // the streaming compare still answers, a locked file included.
    files_equal_streaming(left, right)
}

/// Exact byte compare of two files through two reused buffers.
///
/// The fallback for [`files_equal`] once metadata has not already answered;
/// exits on the first differing chunk, so a differing pair costs one 64 KiB
/// read per side instead of a whole-file read on each.
fn files_equal_streaming(left: &Path, right: &Path) -> bool {
    use std::io::Read;

    let (Ok(mut left_file), Ok(mut right_file)) =
        (std::fs::File::open(left), std::fs::File::open(right))
    else {
        return false;
    };
    let mut left_buffer = vec![0u8; 64 * 1024];
    let mut right_buffer = vec![0u8; 64 * 1024];
    loop {
        let left_read = match left_file.read(&mut left_buffer) {
            Ok(read) => read,
            Err(_) => return false,
        };
        let right_read = match right_file.read(&mut right_buffer) {
            Ok(read) => read,
            Err(_) => return false,
        };
        if left_read != right_read {
            return false;
        }
        if left_read == 0 {
            return true;
        }
        if left_buffer[..left_read] != right_buffer[..right_read] {
            return false;
        }
    }
}

// =============================================================================
// Types + Impls
// =============================================================================

/// Signature of the required registration entry point.
///
/// Returns zero on success; any non-zero status reports a registration
/// failure and keeps the previous generation loaded.
type ModuleInitFn = unsafe extern "C" fn(*const EngineApi) -> u32;

/// Signature of the optional per-frame entry point.
///
/// Modules that omit this export run entirely through their registered
/// scheduler systems instead of an explicit per-frame hook.
type ModuleUpdateFn = unsafe extern "C" fn(*const EngineApi);

/// Signature of the `pill_hot_resolve_install` export generated by the
/// `#[pill_module]` and `#[pill_project]` macros.
///
/// Takes the function's qualified path, the replacement's address and the
/// signature text the replacement was compiled with. Returns 0 when installed,
/// 1 when this artifact declares no such function, and 2 when the signature no
/// longer matches - a non-zero result always leaves the running implementation
/// untouched.
#[cfg(feature = "hot_patch")]
type PlainFunctionInstallFn =
    unsafe extern "C" fn(*const std::ffi::c_char, usize, *const std::ffi::c_char) -> u32;

/// Signature of the `pill_hot_resolve_reset` export generated by the same
/// macros.
///
/// Takes the function's qualified path and returns 0 when this artifact
/// emptied its slot, 1 when it declares no such function.
#[cfg(feature = "hot_patch")]
type PlainFunctionResetFn = unsafe extern "C" fn(*const std::ffi::c_char) -> u32;

/// Export a loadable artifact provides so a patch can reach its redirect slots.
#[cfg(feature = "hot_patch")]
const PLAIN_FUNCTION_INSTALL_SYMBOL: &[u8] = b"pill_hot_resolve_install";

/// SPIKE: signature of the build-script-backed address resolver.
#[cfg(feature = "hot_patch")]
type FunctionAddressFn =
    unsafe extern "C" fn(*const std::ffi::c_char, *mut *const u8, *mut usize) -> usize;

/// Export that reports any function address in this artifact.
#[cfg(feature = "hot_patch")]
const FUNCTION_ADDRESS_SYMBOL: &[u8] = b"pill_hot_resolve_address";

/// Signature of the extent-coverage diagnostic export.
#[cfg(feature = "hot_patch")]
type ExtentCoverageFn = unsafe extern "C" fn() -> u64;

/// Export reporting how many of an artifact's functions have a known length.
#[cfg(feature = "hot_patch")]
const EXTENT_COVERAGE_SYMBOL: &[u8] = b"pill_hot_resolve_extent_coverage";

/// Export that returns one redirect slot to the artifact's own body.
#[cfg(feature = "hot_patch")]
const PLAIN_FUNCTION_RESET_SYMBOL: &[u8] = b"pill_hot_resolve_reset";

/// Signature of the optional ABI revision export.
type ModuleAbiVersionFn = unsafe extern "C" fn() -> u32;

/// Signature of the optional value-type manifest exports.
///
/// The module artifact reports how many `#[derive(PillMirror)]` descriptors it
/// carries, then copies that many into a host-owned buffer. Both sides share
/// the `pill_engine` rlib, so `PillValueTypeDescriptor` has one layout.
type ValueTypeCountFn = unsafe extern "C" fn() -> u32;
type CopyValueTypesFn = unsafe extern "C" fn(*mut PillValueTypeDescriptor, u32) -> u32;

/// Signature of the optional mirrored-method manifest exports.
///
/// Same shape as the value-type manifest: the artifact reports how many
/// `#[pill_mirror_method]` descriptors it carries, then copies that many into
/// a host-owned buffer. Each descriptor names a `#[no_mangle]` trampoline the
/// artifact exports, which the host resolves by symbol for the callable
/// address.
type MirrorMethodCountFn = unsafe extern "C" fn() -> u32;
type CopyMirrorMethodsFn = unsafe extern "C" fn(*mut PillMethodDescriptor, u32) -> u32;

/// Signature of the optional heap-field accessor manifest exports.
///
/// Same shape again: the artifact reports how many accessor descriptors it
/// carries, then copies that many into a host-owned buffer. Each descriptor
/// names the `#[no_mangle]` trampolines the artifact exports for one `Vec` or
/// `String` component field, which the host resolves by symbol for the
/// callable addresses.
type FieldAccessorCountFn = unsafe extern "C" fn() -> u32;
type CopyFieldAccessorsFn = unsafe extern "C" fn(*mut PillFieldAccessorDescriptor, u32) -> u32;

/// Export reporting how many value-type descriptors an artifact declares.
const VALUE_TYPE_COUNT_SYMBOL: &[u8] = b"pill_value_type_descriptor_count";

/// Export copying an artifact's value-type descriptors into a host buffer.
const VALUE_TYPE_COPY_SYMBOL: &[u8] = b"pill_copy_value_type_descriptors";

/// Export reporting how many mirrored-method descriptors an artifact declares.
const MIRROR_METHOD_COUNT_SYMBOL: &[u8] = b"pill_mirror_method_descriptor_count";

/// Export copying an artifact's mirrored-method descriptors into a host buffer.
const MIRROR_METHOD_COPY_SYMBOL: &[u8] = b"pill_copy_mirror_method_descriptors";

/// Export reporting how many heap-field accessor descriptors an artifact
/// declares.
const FIELD_ACCESSOR_COUNT_SYMBOL: &[u8] = b"pill_field_accessor_descriptor_count";

/// Export copying an artifact's heap-field accessor descriptors into a host
/// buffer.
const FIELD_ACCESSOR_COPY_SYMBOL: &[u8] = b"pill_copy_field_accessor_descriptors";

/// Required registration entry point of a loadable artifact.
///
/// One name for both kinds. A project and an optional module export the same
/// symbols, load the same way, reload through the same transaction and retire
/// into the same graveyard; what used to separate them was a prefix on these
/// three names, which is a difference in a string rather than in a contract.
/// The differences that are real - a project is singular where modules are an
/// ordered list, a module's components are mirrored to C# - are policy, and
/// live in the config and the reload transaction rather than in the loader.
const MODULE_INIT_SYMBOL: &[u8] = b"pill_module_init";

/// Optional per-frame entry point of a loadable artifact.
const MODULE_UPDATE_SYMBOL: &[u8] = b"pill_module_update";

/// Optional ABI revision export, read at load time when present.
const MODULE_ABI_VERSION_SYMBOL: &[u8] = b"pill_module_abi_version";

/// Owns one loaded native library, either the project or an optional module.
///
/// Export symbols are resolved once during loading and stored as raw function
/// pointers. The pointers stay valid for as long as the `library` field keeps
/// the module mapped, so frame-loop calls never perform a dynamic lookup or
/// panic on a missing optional export.
pub(crate) struct NativeLibrary {
    /// Loaded module handle; keeps the native library mapped in memory.
    library: Option<Library>,
    /// Required registration entry point, resolved once at load time.
    module_init: ModuleInitFn,
    /// Optional per-frame entry point; `None` when the module has none.
    module_update: Option<ModuleUpdateFn>,
    /// ABI revision the module reports, when it exports one.
    abi_version: Option<u32>,
    /// Optional value-type manifest exports (`#[derive(PillMirror)]`), used by
    /// the C# mirror codegen to resolve nested struct tags.
    value_type_count: Option<ValueTypeCountFn>,
    copy_value_types: Option<CopyValueTypesFn>,
    /// Optional mirrored-method manifest exports (`#[pill_mirror_impl]`),
    /// used by the C# mirror codegen and runtime method table.
    mirror_method_count: Option<MirrorMethodCountFn>,
    copy_mirror_methods: Option<CopyMirrorMethodsFn>,
    /// Optional heap-field accessor manifest exports (`#[derive(PillComponent)]`
    /// components with `Vec`/`String` fields), used by the C# mirror codegen
    /// and the managed runtime's method table.
    field_accessor_count: Option<FieldAccessorCountFn>,
    copy_field_accessors: Option<CopyFieldAccessorsFn>,
    /// Temporary copy backing this library; deleted when the library drops.
    temporary_path: PathBuf,
}

/// A copied artifact path that deletes itself if the load never takes it over.
///
/// `load_copy` writes the copy before the library exists, so the `?` on a
/// rejected load used to leave the file behind for the life of the process:
/// the `Drop` that removes it lives on `NativeLibrary`, which the failure path
/// never constructs. The guard owns the path from the moment the copy succeeds
/// and is disarmed only once the library owns the file.
struct TemporaryCopy {
    /// The copied file, or `None` once the library has taken it over.
    path: Option<PathBuf>,
}

impl TemporaryCopy {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// The copied file, which the guard still owns.
    fn path(&self) -> &Path {
        self.path.as_deref().expect("the copy is disarmed")
    }

    /// Hand the file to the library that just mapped it.
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryCopy {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        remove_temporary_file(&path);
    }
}

/// Fetch the artifact's `#[pill_mirror_method]` descriptors, each with the
/// exported address of its `#[no_mangle]` trampoline.
///
/// Empty when the artifact predates the exports or declares no mirrored
/// methods. A descriptor whose trampoline symbol cannot be resolved is
/// skipped rather than surfaced, so a stale descriptor can never hand C#
/// a null callable.
impl NativeLibrary {
    /// Copy and load the built shared library from a unique temporary path.
    ///
    /// # Errors
    ///
    /// Returns an error if the temporary directory cannot be created, the
    /// built library cannot be copied, or the copy is not a valid native
    /// library exporting the required `pill_module_init` symbol.
    pub(crate) fn load_copy(
        build_output: &Path,
        workspace_root: &Path,
        module_name: &str,
    ) -> Result<Self, LibraryError> {
        // Step 1: Prepare this process's temporary directory and a unique
        // target path. Scoping the directory per process id keeps concurrent
        // host instances from touching each other's copies.
        let temporary_directory = process_temporary_directory(workspace_root);
        std::fs::create_dir_all(&temporary_directory).map_err(|source| {
            LibraryError::TemporaryDirectory {
                directory: temporary_directory.display().to_string(),
                source,
            }
        })?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let counter = TEMPORARY_COPY_COUNTER.fetch_add(1, Ordering::Relaxed);
        // One directory per generation rather than one uniquely named file in
        // a directory shared by all of them.
        //
        // The image names its PDB by bare file name, so a debugger resolves it
        // against the directory the module was loaded from. Generations
        // sharing a directory would therefore have to share one PDB file, and
        // the reload after the first would find that file held open by the
        // debugger and impossible to replace - leaving the new generation with
        // the previous one's symbols, which do not match it.
        //
        // The module name still prefixes the directory so several modules,
        // each reloading on its own schedule, never collide inside one process
        // directory.
        let generation_directory =
            temporary_directory.join(format!("{module_name}_{timestamp}_{counter}"));
        std::fs::create_dir_all(&generation_directory).map_err(|source| {
            LibraryError::TemporaryDirectory {
                directory: generation_directory.display().to_string(),
                source,
            }
        })?;
        // Keeping the build output's own file name lets the PDB beside it keep
        // the name the image records, which is what makes the lookup work.
        let temporary_path = generation_directory.join(
            build_output
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("module.dll")),
        );

        // Step 2: Copy the built library to the unique temporary path.
        std::fs::copy(build_output, &temporary_path).map_err(|source| {
            LibraryError::CopyFailed {
                source_path: build_output.display().to_string(),
                target_path: temporary_path.display().to_string(),
                source,
            }
        })?;
        // Step 2a: Place the module's debug symbols beside the copy.
        //
        // `rust-lld` records the PDB in the image as a bare file name rather
        // than an absolute path, so a debugger resolves it against whatever
        // directory the module was loaded from - this temporary one, not the
        // build output it was copied from. Without this the loaded module has
        // no symbols at all, and no breakpoint in module source can bind.
        //
        // Hard linked rather than copied: a module PDB runs to tens of
        // megabytes and both paths are under the workspace root, so the link
        // costs nothing while a copy would be paid on every reload of every
        // module.
        //
        // Best effort by design. A build carrying no debug info has no PDB,
        // and a link that cannot be made costs symbols rather than the module,
        // so nothing here may fail the load.
        let symbol_source = build_output.with_extension("pdb");
        if let Some(symbol_name) = symbol_source.file_name() {
            if symbol_source.is_file() {
                // The image names the PDB by bare file name, so the copy has to
                // keep that name and sit in the directory the module loads
                // from - this generation's own, so reloads never contend for
                // one file the debugger may already hold open.
                let symbol_target = generation_directory.join(symbol_name);
                let staged = std::fs::hard_link(&symbol_source, &symbol_target)
                    .or_else(|_| std::fs::copy(&symbol_source, &symbol_target).map(|_| ()));
                match staged {
                    Ok(()) => debug!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        path = %symbol_target.display(),
                        "staged module debug symbols beside the loaded copy"
                    ),
                    Err(error) => debug!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        path = %symbol_source.display(),
                        %error,
                        "module debug symbols not staged; module code will have no debugger symbols"
                    ),
                }
            }
        }

        // From here the copy exists, and every path that does not hand it to a
        // `NativeLibrary` must delete it again; the guard owns that duty.
        let mut temporary_copy = TemporaryCopy::new(temporary_path);
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            path = %temporary_copy.path().display(),
            "copied project DLL"
        );

        // Step 2b: Decide whether this module must load against its own engine
        // dylib rather than the host's.
        //
        // The host binary maps `pill_core.dll` from the regular target
        // directory. When the module build produced a different engine dylib
        // than the host runs (a GUI frontend unions extra features onto shared
        // crates), the module must resolve ITS copy or symbol lookup fails at
        // load time. The matching copy is staged into the hot-load directory
        // by the build; when the two copies are byte-identical (a plain CLI
        // host) the module keeps loading the host's single instance exactly as
        // before.
        //
        // Staging is a best effort, not a guarantee, and Step 3 says so when it
        // does not pay off: the loader resolves an already-mapped DLL by module
        // name, so a second `pill_core.dll` only wins where the host has not
        // mapped one - which on Windows is never.
        let isolated_engine = engine_dylib_needs_isolation(workspace_root);
        if isolated_engine {
            stage_module_engine_dylib(workspace_root, &temporary_directory);
        }

        // Step 3: Load the copy and validate its required exports.
        let load_started = Instant::now();
        // SAFETY: `temporary_copy.path()` is a complete native module on
        // disk - it was written by `std::fs::copy` immediately above - and
        // `Self::load` validates the required exports before returning; see
        // the fuller justification above the copy.
        let native_library = unsafe {
            Self::load(
                temporary_copy.path(),
                temporary_copy.path().to_path_buf(),
                module_name,
                isolated_engine,
            )
        }
        .inspect_err(|_| {
            // The guard removes the copy when this function returns the error.
            // The staged engine dylib it would have loaded against needs the
            // same treatment when this attempt asked for isolation, because
            // nothing else deletes it for the life of the process. A dylib a
            // live library maps cannot be deleted on Windows, so a removal
            // failure is ignored: another module may already be using it, and
            // the next attempt stages a fresh copy either way.
            if isolated_engine {
                let _ = std::fs::remove_file(temporary_directory.join("pill_core.dll"));
            }
        })
        .map_err(|error| diagnose_load_failure(error, workspace_root, module_name))?;
        temporary_copy.disarm();
        analytics::record_load(module_name, load_started.elapsed().as_secs_f64() * 1000.0);
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = module_name,
            abi_version = ?native_library.abi_version,
            "module DLL loaded successfully"
        );
        Ok(native_library)
    }

    /// ABI revision reported by the module, when it exports one.
    ///
    /// `None` means the library predates the versioned contract; the caller
    /// decides whether that is acceptable for its module kind.
    pub(crate) fn abi_version(&self) -> Option<u32> {
        self.abi_version
    }

    /// Fetch the artifact's `#[derive(PillMirror)]` value-type descriptors.
    ///
    /// Empty when the artifact predates the exports or declares no value
    /// types. The returned descriptors reference static data inside this
    /// loaded library, which stays mapped for the library's lifetime.
    pub(crate) fn value_type_descriptors(&self) -> Vec<PillValueTypeDescriptor> {
        let (Some(count), Some(copy)) = (self.value_type_count, self.copy_value_types) else {
            return Vec::new();
        };
        // SAFETY: the count export takes no arguments and returns a plain
        // integer while the library is mapped.
        let total = unsafe { count() } as usize;
        if total == 0 {
            return Vec::new();
        }
        let mut descriptors = vec![
            PillValueTypeDescriptor {
                type_name: "",
                size: 0,
                align: 0,
                fields: &[],
            };
            total
        ];
        // SAFETY: the copy export was validated to take a host-owned buffer of
        // `total` slots; the slice's pointer and length satisfy that contract,
        // and the library stays mapped for the call.
        let copied = unsafe { copy(descriptors.as_mut_ptr(), total as u32) } as usize;
        descriptors.truncate(copied.min(total));
        descriptors
    }

    /// Fetch the artifact's `#[pill_mirror_method]` descriptors, each with the
    /// exported address of its `#[no_mangle]` trampoline.
    ///
    /// Empty when the artifact predates the exports or declares no mirrored
    /// methods. A descriptor whose trampoline symbol cannot be resolved is
    /// skipped rather than surfaced, so a stale descriptor can never hand C#
    /// a null callable.
    pub(crate) fn mirror_methods(&self) -> Vec<ResolvedMirrorMethod> {
        let Some(library) = self.library.as_ref() else {
            return Vec::new();
        };
        let (Some(count), Some(copy)) = (self.mirror_method_count, self.copy_mirror_methods) else {
            return Vec::new();
        };
        // SAFETY: the count export takes no arguments and returns a plain
        // integer while the library is mapped.
        let total = unsafe { count() } as usize;
        if total == 0 {
            return Vec::new();
        }
        let mut descriptors = vec![
            PillMethodDescriptor {
                type_name: "",
                name: "",
                symbol: "",
                return_tag: "",
                arg_tags: &[],
                arg_names: &[],
            };
            total
        ];
        // SAFETY: the copy export was validated to take a host-owned buffer of
        // `total` slots; the slice's pointer and length satisfy that contract,
        // and the library stays mapped for the call.
        let copied = unsafe { copy(descriptors.as_mut_ptr(), total as u32) } as usize;
        descriptors.truncate(copied.min(total));

        let mut resolved: Vec<ResolvedMirrorMethod> = Vec::new();
        for descriptor in descriptors {
            // SAFETY: `library.get` maps the exported trampoline symbol; the
            // module stays mapped for this `NativeLibrary`'s lifetime.
            let address = match unsafe { library.get::<usize>(descriptor.symbol.as_bytes()) } {
                Ok(symbol) => *symbol,
                Err(_) => continue,
            };
            resolved.push(ResolvedMirrorMethod {
                type_name: descriptor.type_name.to_string(),
                method_name: descriptor.name.to_string(),
                return_tag: descriptor.return_tag.to_string(),
                arg_tags: descriptor
                    .arg_tags
                    .iter()
                    .map(|tag| tag.to_string())
                    .collect(),
                arg_names: descriptor
                    .arg_names
                    .iter()
                    .map(|name| name.to_string())
                    .collect(),
                address,
            });
        }
        resolved
    }

    /// Fetch the artifact's heap-field accessor descriptors, each with the
    /// exported addresses of its `#[no_mangle]` trampolines.
    ///
    /// Empty when the artifact predates the exports or declares no heap
    /// fields. Each operation's trampoline resolves to `None` when the
    /// artifact does not export it, and a descriptor whose every operation
    /// failed to resolve is skipped entirely rather than surfaced as a row
    /// nothing could call; which operations a mirror actually needs is
    /// checked by the codegen, which knows the field's kind.
    pub(crate) fn field_accessors(&self) -> Vec<ResolvedFieldAccessor> {
        let Some(library) = self.library.as_ref() else {
            return Vec::new();
        };
        let (Some(count), Some(copy)) = (self.field_accessor_count, self.copy_field_accessors)
        else {
            return Vec::new();
        };
        // SAFETY: the count export takes no arguments and returns a plain
        // integer while the library is mapped.
        let total = unsafe { count() } as usize;
        if total == 0 {
            return Vec::new();
        }
        let mut descriptors = vec![
            PillFieldAccessorDescriptor {
                type_name: "",
                field_name: "",
                kind: "",
                element_tag: "",
                view_symbol: "",
                resize_symbol: "",
                set_symbol: "",
                item_symbol: "",
                set_item_symbol: "",
                push_symbol: "",
            };
            total
        ];
        // SAFETY: the copy export was validated to take a host-owned buffer of
        // `total` slots; the slice's pointer and length satisfy that contract,
        // and the library stays mapped for the call.
        let copied = unsafe { copy(descriptors.as_mut_ptr(), total as u32) } as usize;
        descriptors.truncate(copied.min(total));

        let resolve_optional = |symbol: &str| -> Option<usize> {
            if symbol.is_empty() {
                return None;
            }
            // SAFETY: `library.get` maps the exported trampoline symbol; the
            // module stays mapped for this `NativeLibrary`'s lifetime.
            unsafe { library.get::<usize>(symbol.as_bytes()) }
                .ok()
                .map(|symbol| *symbol)
        };

        let mut resolved: Vec<ResolvedFieldAccessor> = Vec::new();
        for descriptor in descriptors {
            let view_address = resolve_optional(descriptor.view_symbol);
            let resize_address = resolve_optional(descriptor.resize_symbol);
            let set_address = resolve_optional(descriptor.set_symbol);
            let item_address = resolve_optional(descriptor.item_symbol);
            let set_item_address = resolve_optional(descriptor.set_item_symbol);
            let push_address = resolve_optional(descriptor.push_symbol);
            // A descriptor whose every operation failed to resolve is not a
            // usable accessor; publish nothing rather than a row nothing could
            // call. Which operations a generated mirror needs is checked by
            // the codegen, which knows the field's kind.
            let callable = view_address.is_some()
                || resize_address.is_some()
                || set_address.is_some()
                || item_address.is_some()
                || set_item_address.is_some()
                || push_address.is_some();
            if !callable {
                continue;
            }
            resolved.push(ResolvedFieldAccessor {
                type_name: descriptor.type_name.to_string(),
                field_name: descriptor.field_name.to_string(),
                kind: descriptor.kind.to_string(),
                element_tag: descriptor.element_tag.to_string(),
                view_address,
                resize_address,
                set_address,
                item_address,
                set_item_address,
                push_address,
            });
        }
        resolved
    }

    /// Load a module and verify its required exports.
    ///
    /// # Safety
    ///
    /// `path` must point to a valid native library whose `pill_module_init`
    /// export uses the expected C ABI.
    unsafe fn load(
        path: &Path,
        temporary_path: PathBuf,
        module_name: &str,
        isolated_engine: bool,
    ) -> Result<Self, LibraryError> {
        // Step 1: Open the native library and map it into this process.
        // SAFETY: The `# Safety` contract of `load` guarantees `path` names a
        // valid native library. Mapping runs the module's constructors; the
        // returned handle keeps it mapped and is stored in the
        // `ProjectLibrary` for the module's whole lifetime.
        let library = if isolated_engine {
            // The module was compiled against an engine dylib that differs from
            // the host's; load with the module's own directory first in the
            // dependency search order so it resolves the copy staged beside it.
            #[cfg(windows)]
            {
                unsafe {
                    // SAFETY: `path` is the complete native module on disk
                    // (copied by the caller immediately before this load), and
                    // the matching engine dylib was copied into the same
                    // directory just above; the altered search path therefore
                    // only resolves known, self-owned DLLs.

                    // `load_with_flags` returns the platform-level handle; the
                    // crate-root `Library` used by `NativeLibrary` is the safe
                    // wrapper over it, which `From` provides.
                    Library::from(
                        windows_loader::Library::load_with_flags(
                            path,
                            LOAD_WITH_ALTERED_SEARCH_PATH,
                        )
                        .map_err(|source| LibraryError::LoadFailed {
                            path: path.display().to_string(),
                            source,
                        })?,
                    )
                }
            }
            // Non-Windows loaders do not pin dependency resolution the way
            // Windows does; fall back to the default load, which resolves the
            // module's dependencies the same way it always has.
            #[cfg(not(windows))]
            {
                unsafe {
                    Library::new(path).map_err(|source| LibraryError::LoadFailed {
                        path: path.display().to_string(),
                        source,
                    })?
                }
            }
        } else {
            // SAFETY: `path` names a complete native module on disk (validated
            // by the caller), and the module was built against the engine dylib
            // the host already has mapped, so the default search resolves that
            // shared single instance.
            unsafe {
                Library::new(path).map_err(|source| LibraryError::LoadFailed {
                    path: path.display().to_string(),
                    source,
                })?
            }
        };

        // Step 2: Resolve the required `pill_module_init` export. The subject's
        // name travels with the refusal, because one contract means the symbol
        // no longer says which artifact is missing it.
        // SAFETY: `pill_module_init` is a mandatory export of the native ABI
        // contract, so every supported artifact provides it, and it is resolved
        // here as a pointer with the statically known C ABI signature. The
        // pointer stays valid because the `library` handle keeps the module
        // mapped for the lifetime of the returned `NativeLibrary`.
        let module_init: Symbol<ModuleInitFn> = unsafe {
            library
                .get(MODULE_INIT_SYMBOL)
                .map_err(|source| LibraryError::MissingExport {
                    subject: module_name.to_string(),
                    symbol: String::from_utf8_lossy(MODULE_INIT_SYMBOL).to_string(),
                    source,
                })?
        };

        // Step 3: Resolve the optional `pill_module_update` export.
        // SAFETY: the export is optional; when present it is resolved as a
        // pointer with the statically known C ABI signature, and when absent
        // the lookup fails and the error is discarded, leaving the hook as
        // `None`. The pointer stays valid because the `library` handle keeps
        // the module mapped.
        let module_update: Option<Symbol<ModuleUpdateFn>> =
            unsafe { library.get(MODULE_UPDATE_SYMBOL) }.ok();

        // Step 4: Read the optional ABI revision before any other call, so a
        // caller can reject an incompatible module without ever handing it a
        // pointer into engine memory.
        // SAFETY: The export is optional; when present it is resolved with the
        // statically known C ABI signature and called immediately while the
        // library is mapped. It takes no arguments and returns a plain integer,
        // so the call cannot touch host state.
        let abi_version: Option<u32> =
            unsafe { library.get::<ModuleAbiVersionFn>(MODULE_ABI_VERSION_SYMBOL) }
                .ok()
                .map(|symbol| unsafe { symbol() });

        // Step 4b: Resolve the optional value-type manifest exports. Optional,
        // because the project contract predates them; a library that does not
        // export them simply contributes no typed value types.
        // SAFETY: Both exports, when present, are resolved with their statically
        // known C ABI signatures and called with a host-owned buffer; the
        // pointer stays valid because the `library` handle keeps the module
        // mapped for the lifetime of the returned `NativeLibrary`.
        // SAFETY: `library.get` maps a function pointer from the loaded module
        // with the statically known signature; the mapped module stays alive
        // for as long as the `library` handle held by this `NativeLibrary`.
        let value_type_count: Option<ValueTypeCountFn> =
            unsafe { library.get(VALUE_TYPE_COUNT_SYMBOL) }
                .ok()
                .map(|symbol| *symbol);
        // SAFETY: same as above; `library.get` maps the copy export from the
        // module that is kept mapped by the `library` handle.
        let copy_value_types: Option<CopyValueTypesFn> =
            unsafe { library.get(VALUE_TYPE_COPY_SYMBOL) }
                .ok()
                .map(|symbol| *symbol);
        // Step 4c: Resolve the optional mirrored-method manifest exports.
        // Optional, exactly like the value-type manifest: a library built
        // before the exports simply contributes no mirrored methods.
        // SAFETY: `library.get` maps each export with its statically known C
        // ABI signature; the module stays mapped for the lifetime of the
        // returned `NativeLibrary`.
        let mirror_method_count: Option<MirrorMethodCountFn> =
            unsafe { library.get(MIRROR_METHOD_COUNT_SYMBOL) }
                .ok()
                .map(|symbol| *symbol);
        // SAFETY: `library.get` maps the copy export from the module that is
        // kept mapped by the `library` handle; the descriptor array stays
        // inside this artifact's static data for the artifact's lifetime.
        let copy_mirror_methods: Option<CopyMirrorMethodsFn> =
            unsafe { library.get(MIRROR_METHOD_COPY_SYMBOL) }
                .ok()
                .map(|symbol| *symbol);

        // Step 4d: Resolve the optional heap-field accessor exports. Optional
        // like the manifests above: a library built before the exports simply
        // contributes no container accessors.
        // SAFETY: `library.get` maps each export with its statically known C
        // ABI signature; the module stays mapped for the lifetime of the
        // returned `NativeLibrary`.
        let field_accessor_count: Option<FieldAccessorCountFn> =
            unsafe { library.get(FIELD_ACCESSOR_COUNT_SYMBOL) }
                .ok()
                .map(|symbol| *symbol);
        // SAFETY: same as above; the descriptor array stays inside this
        // artifact's static data for the artifact's lifetime.
        let copy_field_accessors: Option<CopyFieldAccessorsFn> =
            unsafe { library.get(FIELD_ACCESSOR_COPY_SYMBOL) }
                .ok()
                .map(|symbol| *symbol);

        // Step 5: Copy the resolved pointers out of the borrowed Symbol
        // wrappers. The `library` field keeps the module mapped, so these raw
        // pointers remain valid for the complete lifetime of the returned
        // `NativeLibrary`.
        let module_init_pointer = *module_init;
        let module_update_pointer = module_update.map(|symbol| *symbol);
        Ok(Self {
            library: Some(library),
            module_init: module_init_pointer,
            module_update: module_update_pointer,
            abi_version,
            value_type_count,
            copy_value_types,
            mirror_method_count,
            copy_mirror_methods,
            field_accessor_count,
            copy_field_accessors,
            temporary_path,
        })
    }

    /// Call the module's registration entry point.
    ///
    /// Returns the module's status code: zero reports successful registration;
    /// any non-zero value means the module failed to initialize and the
    /// previous generation must remain active.
    pub(crate) fn call_init(&self, api: &EngineApi) -> u32 {
        // SAFETY: The init export was validated to exist and to use this C ABI
        // signature when the library was loaded, and the `library` field
        // keeps the module mapped for as long as this `NativeLibrary` lives.
        // `api` is borrowed immutably for the whole call and outlives it
        // because the host creates the engine API before loading the module.
        unsafe { (self.module_init)(api as *const EngineApi) }
    }

    /// Call the optional native per-frame update entry point, when exported.
    ///
    /// Artifacts that omit `pill_module_update` run entirely through their registered
    /// scheduler systems, so a missing export is a no-op rather than an error.
    pub(crate) fn call_update(&self, api: &EngineApi) {
        if let Some(module_update) = self.module_update {
            // SAFETY: The update export was validated when the library was
            // loaded and the `library` field keeps the module mapped for the
            // lifetime of this `NativeLibrary`. `api` is borrowed immutably for
            // the whole call, matching the read-only access the native update
            // hook expects.
            unsafe { module_update(api as *const EngineApi) };
        }
    }

    /// Offer a replacement implementation to this artifact's copy of one
    /// `#[pill_hot_fn]`.
    ///
    /// A crate linked into several artifacts is compiled into each of them, so
    /// one edited function has as many independent redirect slots as there are
    /// artifacts linking it. The host therefore offers the same replacement to
    /// every loaded library rather than assuming one of them owns the function.
    ///
    /// Returns `Ok(true)` when this artifact installed the replacement,
    /// `Ok(false)` when it declares no such function (including when it exports
    /// no resolver at all, as a library built before this ABI does), and `Err`
    /// when it refused because the signature no longer matches.
    #[cfg(feature = "hot_patch")]
    pub(crate) fn install_plain_function(
        &self,
        qualified_name: &str,
        address: usize,
        signature: &str,
    ) -> Result<bool, String> {
        let Some(library) = self.library.as_ref() else {
            return Ok(false);
        };
        // Resolved on demand rather than at load time: the export only exists
        // on artifacts built from a crate that opted into hot patching, and a
        // patch is rare enough that one lookup costs nothing.
        //
        // SAFETY: the symbol is looked up by the name the `#[pill_module]` and
        // `#[pill_project]` macros generate, with the signature those macros
        // define. The returned pointer stays valid because `library` keeps the
        // module mapped for this `NativeLibrary`'s lifetime.
        let install: Symbol<PlainFunctionInstallFn> =
            match unsafe { library.get(PLAIN_FUNCTION_INSTALL_SYMBOL) } {
                Ok(symbol) => symbol,
                Err(_) => return Ok(false),
            };

        let name = std::ffi::CString::new(qualified_name)
            .map_err(|_| "the function path contains an interior NUL".to_string())?;
        let signature_text = std::ffi::CString::new(signature)
            .map_err(|_| "the signature contains an interior NUL".to_string())?;

        // SAFETY: both strings are NUL-terminated and live until the call
        // returns, and `address` points into a patch library the host keeps
        // mapped for the process lifetime.
        let status = unsafe { install(name.as_ptr(), address, signature_text.as_ptr()) };
        match status {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(format!(
                "`{qualified_name}` changed shape; the running implementation was kept"
            )),
        }
    }

    /// How many of this artifact's functions have a length the exception
    /// directory records, and how many it declares in total.
    ///
    /// Only a function with a known extent can be prologue-patched, so this
    /// turns a refusal into a diagnosis: whether one function happens to be a
    /// leaf, or the whole artifact is out of reach.
    #[cfg(feature = "hot_patch")]
    pub(crate) fn extent_coverage(&self) -> Option<(usize, usize)> {
        let library = self.library.as_ref()?;
        // SAFETY: the symbol is looked up by the name the ABI macros generate,
        // with the signature they define, in a module this handle keeps mapped.
        let coverage: Symbol<ExtentCoverageFn> =
            unsafe { library.get(EXTENT_COVERAGE_SYMBOL) }.ok()?;
        // SAFETY: the export takes no arguments and only reads static tables.
        let packed = unsafe { coverage() };
        Some(((packed >> 32) as usize, (packed & 0xFFFF_FFFF) as usize))
    }

    /// Report the address and recorded declaration of any function in this
    /// artifact, from the build-script-generated inventory rather than an
    /// attribute.
    ///
    /// The declaration is the compatibility gate for the prologue route: it is
    /// the only chance to refuse a reshaped function, because overwriting the
    /// first bytes of a function can check nothing about what it jumps to. An
    /// artifact whose inventory carries no declaration reports `None` for it,
    /// which the gate refuses rather than exempts.
    ///
    /// Returns `None` overall when this artifact exports no address resolver or
    /// has no entry for the name.
    #[cfg(feature = "hot_patch")]
    pub(crate) fn function_address(&self, qualified_name: &str) -> Option<(usize, Option<String>)> {
        let library = self.library.as_ref()?;
        // SAFETY: the symbol is looked up by the name the ABI macros generate,
        // with the signature they define. The pointer stays valid because
        // `library` keeps the module mapped for this `NativeLibrary`'s lifetime.
        let resolve: Symbol<FunctionAddressFn> =
            unsafe { library.get(FUNCTION_ADDRESS_SYMBOL) }.ok()?;
        let name = std::ffi::CString::new(qualified_name).ok()?;
        let mut signature_pointer: *const u8 = std::ptr::null();
        let mut signature_length: usize = 0;
        // SAFETY: the string is NUL-terminated and lives until the call returns,
        // and both out-parameters are writable locals. On success the artifact
        // writes a pointer into its own static storage, which stays valid while
        // it is loaded.
        let address =
            unsafe { resolve(name.as_ptr(), &mut signature_pointer, &mut signature_length) };
        if address == 0 {
            return None;
        }
        let signature = if signature_pointer.is_null() {
            None
        } else {
            // SAFETY: the artifact wrote a pointer and length describing a
            // `&'static str` inside its own image.
            let bytes = unsafe { std::slice::from_raw_parts(signature_pointer, signature_length) };
            Some(String::from_utf8_lossy(bytes).into_owned())
        };
        Some((address, signature))
    }

    /// Return this artifact's copy of one `#[pill_hot_fn]` to its own body.
    ///
    /// The counterpart of [`Self::install_plain_function`], used to roll a patch
    /// back to generation zero. Each artifact restores its own compiled-in code
    /// rather than being handed an address that belongs to another.
    ///
    /// Returns `Ok(true)` when this artifact emptied its slot and `Ok(false)`
    /// when it declares no such function or exports no resolver.
    #[cfg(feature = "hot_patch")]
    pub(crate) fn reset_plain_function(&self, qualified_name: &str) -> Result<bool, String> {
        let Some(library) = self.library.as_ref() else {
            return Ok(false);
        };
        // SAFETY: the symbol is looked up by the name the `#[pill_module]` and
        // `#[pill_project]` macros generate, with the signature those macros
        // define. The returned pointer stays valid because `library` keeps the
        // module mapped for this `NativeLibrary`'s lifetime.
        let reset: Symbol<PlainFunctionResetFn> =
            match unsafe { library.get(PLAIN_FUNCTION_RESET_SYMBOL) } {
                Ok(symbol) => symbol,
                Err(_) => return Ok(false),
            };

        let name = std::ffi::CString::new(qualified_name)
            .map_err(|_| "the function path contains an interior NUL".to_string())?;
        // SAFETY: the string is NUL-terminated and lives until the call returns.
        let status = unsafe { reset(name.as_ptr()) };
        Ok(status == 0)
    }
}

impl Drop for NativeLibrary {
    /// Unmap the module and delete its temporary copy.
    ///
    /// The library handle is dropped explicitly before the file is removed
    /// because Windows refuses to delete a file that is still mapped into the
    /// process. Cleanup failures are reported rather than swallowed.
    fn drop(&mut self) {
        // This is the drop in which a stale call into already-freed code shows
        // up, so the image is named before it goes: the last line printed is
        // what identifies the image when the process dies inside this drop.
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            path = %self.temporary_path.display(),
            "unmapping module copy"
        );
        drop(self.library.take());
        remove_temporary_file(&self.temporary_path);
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Delete one temporary copy, reporting failures rather than swallowing them.
///
/// Shared by [`NativeLibrary`]'s `Drop` and [`TemporaryCopy`]'s, which remove
/// the same kind of file for the same reason and must say the same thing when
/// the removal fails.
fn remove_temporary_file(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        eprintln!(
            "[host] Failed to remove temporary DLL {}: {error}",
            path.display()
        );
    }
}

/// Directory used by this host process for temporary native-library copies.
///
/// Scoping the directory per process id means one host instance can never
/// delete or overwrite the copies of another instance running against the
/// same workspace.
pub(crate) fn process_temporary_directory(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(TEMPORARY_DIRECTORY)
        .join(std::process::id().to_string())
}

/// Whether a process with the given id is still running.
///
/// An exact answer where one is cheap, and only there: Linux probes `/proc`
/// directly, and every other platform answers "not alive" rather than paying
/// for a process-table walk on a path that runs once at startup.
///
/// That is safe because it is not the only check. A negative answer here does
/// not authorise a removal - it hands the decision to
/// [`STAGING_GRACE_PERIOD`], which asks when the directory was last written to
/// instead of who owns it. A live host writes to its staging on every load, so
/// the clock recognises it on every platform; this function only lets Linux
/// skip the wait.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// Remove temporary copies left by earlier runs of this host process.
///
/// Three rules, in order. This process's own directory always goes - a
/// previous run under the same process id left it, and nothing of ours is
/// mapped yet. Another process's directory goes once it has been quiet for
/// [`STAGING_GRACE_PERIOD`], which is how a dead host is told from a live one
/// without probing the process table. And whatever survives both is capped at
/// [`MAX_RETAINED_STAGING_DIRECTORIES`], oldest evicted first, so a burst of
/// host runs inside one grace period cannot accumulate without bound.
///
/// Removal failures are reported rather than swallowed, because on Windows a
/// failure is informative: a live host keeps its mapped modules locked, so a
/// refusal is the operating system saying the directory is still in use.
pub(crate) fn cleanup_temporary_files(workspace_root: &Path) {
    let temporary_root = workspace_root.join(TEMPORARY_DIRECTORY);
    let Ok(entries) = std::fs::read_dir(&temporary_root) else {
        return;
    };

    let own_pid = std::process::id();
    let mut candidates: Vec<(u32, SystemTime, PathBuf)> = Vec::new();

    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();

        if pid == own_pid {
            remove_staging_directory(&path, pid, "left by an earlier run of this process");
            continue;
        }

        // The Linux probe is exact and answers first; everywhere else it
        // reports "not alive" by design and the clock decides instead.
        if process_is_alive(pid) {
            continue;
        }

        candidates.push((pid, staging_last_write(&path), path));
    }

    let ages: Vec<(u32, SystemTime)> = candidates
        .iter()
        .map(|(pid, modified, _)| (*pid, *modified))
        .collect();
    for (index, reason) in staging_directories_to_evict(&ages, SystemTime::now()) {
        let (pid, _, path) = &candidates[index];
        remove_staging_directory(path, *pid, reason);
    }
}

/// Choose which of the other processes' staging directories to remove.
///
/// Two rules. A directory quiet for at least [`STAGING_GRACE_PERIOD`] belonged
/// to a process that is gone, so it goes. Whatever is left is capped at
/// [`MAX_RETAINED_STAGING_DIRECTORIES`], least recently written first - which
/// is both the least likely to belong to a running host and the one whose disk
/// is most worth reclaiming.
///
/// Pure, and separated from the removal for that reason: the policy is the part
/// worth pinning, and pinning it here needs neither a filesystem nor a second
/// host process.
fn staging_directories_to_evict(
    candidates: &[(u32, SystemTime)],
    now: SystemTime,
) -> Vec<(usize, &'static str)> {
    let mut evictions: Vec<(usize, &'static str)> = Vec::new();
    let mut retained: Vec<(SystemTime, usize)> = Vec::new();

    for (index, (_, modified)) in candidates.iter().enumerate() {
        let quiet_for = now
            .duration_since(*modified)
            .unwrap_or_else(|_| Duration::from_secs(0));
        if quiet_for >= STAGING_GRACE_PERIOD {
            evictions.push((index, "stale"));
        } else {
            retained.push((*modified, index));
        }
    }

    if retained.len() > MAX_RETAINED_STAGING_DIRECTORIES {
        retained.sort_by_key(|(modified, _)| *modified);
        let excess = retained.len() - MAX_RETAINED_STAGING_DIRECTORIES;
        for (_, index) in retained.iter().take(excess) {
            evictions.push((*index, "over the retained-staging bound"));
        }
    }

    evictions
}

/// When a staging directory was last written to.
///
/// Falls back to the epoch when the timestamp cannot be read, which makes an
/// unreadable directory look maximally stale: it is the one state in which
/// leaving it forever is worse than attempting a removal that will simply fail
/// if the files are in use.
fn staging_last_write(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Remove one staging directory, saying which one and why.
///
/// A failure for another process's directory is expected rather than
/// exceptional - a live host holds its mapped modules - so it is reported as a
/// note instead of an error.
fn remove_staging_directory(path: &Path, pid: u32, reason: &str) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {
            if pid != std::process::id() {
                println!("[host] Cleaned up temporary files from process {pid} ({reason}).");
            }
        }
        Err(error) => {
            if pid == std::process::id() {
                eprintln!(
                    "[host] Could not remove temporary directory {}: {error}",
                    path.display()
                );
            } else {
                // On platforms without process probing the removal may fail
                // simply because another live host holds the files.
                println!(
                    "[host] Temporary directory {} left in place (possibly still in use): {error}",
                    path.display()
                );
            }
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::{
        files_equal, staging_directories_to_evict, TemporaryCopy, MAX_RETAINED_STAGING_DIRECTORIES,
        STAGING_GRACE_PERIOD,
    };
    use std::time::{Duration, SystemTime};

    /// A directory that has been quiet past the grace period is stale; one
    /// written to recently is left alone.
    ///
    /// The grace period is the liveness check: a running host stages a copy on
    /// every load and every reload, so a recent write is the evidence that its
    /// process is still there. Deleting it would take whatever of its staging
    /// is not currently mapped and leave the rest.
    #[test]
    fn a_quiet_staging_directory_is_stale_and_a_recent_one_is_not() {
        let now = SystemTime::now();
        let quiet = now - STAGING_GRACE_PERIOD - Duration::from_secs(1);
        let recent = now - Duration::from_secs(1);

        let evictions = staging_directories_to_evict(&[(101, quiet), (202, recent)], now);

        assert_eq!(evictions, vec![(0, "stale")]);
    }

    /// Past the retained bound, the least recently written go first.
    ///
    /// Oldest-first is what makes the bound safe to apply inside the grace
    /// period at all: a live host's directory is the most recently written of
    /// them, so it is the last one the bound would ever reach.
    #[test]
    fn the_retained_bound_evicts_the_least_recently_written_first() {
        let now = SystemTime::now();
        // One more than the bound allows, all well inside the grace period,
        // each a second older than the next.
        let count = MAX_RETAINED_STAGING_DIRECTORIES + 2;
        let candidates: Vec<(u32, SystemTime)> = (0..count)
            .map(|index| {
                let age = Duration::from_secs((count - index) as u64);
                (index as u32, now - age)
            })
            .collect();

        let evictions = staging_directories_to_evict(&candidates, now);

        assert_eq!(
            evictions,
            vec![
                (0, "over the retained-staging bound"),
                (1, "over the retained-staging bound")
            ],
            "the two oldest go, and the most recently written - a live host's - stays"
        );
    }

    /// Inside the bound and inside the grace period, nothing is evicted.
    #[test]
    fn staging_directories_within_both_rules_are_kept() {
        let now = SystemTime::now();
        let candidates: Vec<(u32, SystemTime)> = (0..MAX_RETAINED_STAGING_DIRECTORIES)
            .map(|index| (index as u32, now - Duration::from_secs(index as u64)))
            .collect();

        assert!(staging_directories_to_evict(&candidates, now).is_empty());
    }

    /// Stamp one fixed modification time onto a file, so the metadata gate in
    /// `files_equal` is decided by this test rather than by clock resolution.
    fn set_modified(path: &std::path::Path, stamp: SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open to set the timestamp");
        file.set_modified(stamp).expect("set modification time");
    }

    /// The length gate and the streaming compare answer one question: equal
    /// bytes are equal, and both a length mismatch and a same-length
    /// difference are not. Timestamps must not decide: the identical pair
    /// below deliberately carries different modification times.
    #[test]
    fn files_equal_gates_length_and_compares_bytes() {
        let directory = std::env::temp_dir().join("pill_files_equal");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create directory");

        let first = directory.join("first.dll");
        let second = directory.join("second.dll");
        let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        std::fs::write(&first, b"engine bytes").expect("write first");
        std::fs::write(&second, b"engine bytes").expect("write second");
        set_modified(&first, stamp);
        // A copied dylib keeps its bytes but not its timestamp; comparing the
        // two as "different" would stage an isolated engine and map
        // `pill_core.dll` twice.
        set_modified(&second, stamp + Duration::from_secs(1));
        assert!(
            files_equal(&first, &second),
            "identical bytes compare equal regardless of timestamps"
        );

        // Same length, one differing byte: the streaming compare must catch
        // what the length gate cannot.
        std::fs::write(&second, b"engine ByteS").expect("write one differing byte");
        set_modified(&second, stamp);
        assert!(
            !files_equal(&first, &second),
            "same length but different bytes compare unequal"
        );

        // Shorter file: the length gate answers.
        std::fs::write(&second, b"engine").expect("truncate second");
        set_modified(&second, stamp);
        assert!(
            !files_equal(&first, &second),
            "a length mismatch compares unequal"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A load that never happens still removes the copy the guard owns, and a
    /// disarmed guard leaves the file for the library's own `Drop`.
    #[test]
    fn an_abandoned_copy_is_removed() {
        let directory = std::env::temp_dir().join("pill_temporary_copy");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create directory");

        let abandoned = directory.join("abandoned.dll");
        std::fs::write(&abandoned, b"copy").expect("write abandoned copy");
        {
            let _guard = TemporaryCopy::new(abandoned.clone());
        }
        assert!(
            !abandoned.exists(),
            "the guard removes the copy when the load never took it over"
        );

        let kept = directory.join("kept.dll");
        std::fs::write(&kept, b"copy").expect("write kept copy");
        {
            let mut guard = TemporaryCopy::new(kept.clone());
            let _ = guard.path();
            guard.disarm();
        }
        assert!(
            kept.exists(),
            "a disarmed guard leaves the file to the library that mapped it"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }
}
