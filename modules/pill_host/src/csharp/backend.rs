//! High-level C# project startup, discovery, and scheduler registration.
//!
//! # Responsibilities
//!
//! - Start .NET and discover the managed interop exports.
//! - Register reflected component manifests and startup methods.
//! - Translate reflected system accesses into scheduler registrations.
//!
//! # Design
//!
//! Every unmanaged export is resolved once at startup into a plain function
//! pointer held by the [`CSharpRuntime`] host. Scheduled systems never call
//! the runtime directly: each registration captures the resolved pointers,
//! the reflected access list, and a shared [`ComponentBindings`] arc, so the
//! host outlives every scheduler closure it registers. After a hot reload the
//! active assembly is re-reflected and compared against the
//! [`ManagedSystemSnapshot`] captured at startup, so stale index bindings can
//! never run silently.

// Standard library
use std::path::{Path, PathBuf};
use std::sync::Arc;

// External crates
use pill_core::error::CSharpError;
#[cfg(feature = "hot_reload")]
use pill_core::{error, info};
use pill_engine::commands::CommandQueue;
use pill_engine::{Engine, SystemAccess, SystemError, World};

// Current crate
use super::abi::{CsEngineApi, NativeSystemAccess};
use super::aot_runtime::AotRuntimeContext;
use super::components::{
    module_native_bindings, register_component_manifest, shared_component_bindings,
    ComponentBindings, ModuleExposedComponent, StableComponentId,
};
use super::context::ActiveSystemGuard;
use super::csharp_runtime::DotnetRuntimeContext;
use super::ResolvedMirrorMethod;
use crate::CSharpModuleConfig;

// =============================================================================
// Constants
// =============================================================================

/// Upper bound for the serialized managed component manifest.
///
/// Real manifests are a few hundred bytes. The cap keeps a buggy or hostile
/// managed assembly from driving a multi-gigabyte host allocation.
pub(super) const MAX_COMPONENT_MANIFEST_BYTES: u32 = 16 * 1024 * 1024;

#[cfg(feature = "hot_reload")]
/// Poll returned without a reload: nothing was due or the file is unchanged.
pub(crate) const POLL_NO_CHANGE: u8 = 0;
#[cfg(feature = "hot_reload")]
/// Poll swapped in a behavior-compatible assembly.
pub(crate) const POLL_RELOADED: u8 = 1;
#[cfg(feature = "hot_reload")]
/// Poll rejected the new assembly; the old version stays loaded.
pub(crate) const POLL_REJECTED: u8 = 2;

/// Maximum UTF-8 byte length accepted for a managed system name.
const MAX_SYSTEM_NAME_BYTES: u32 = 1024;

/// Maximum UTF-8 byte length accepted for a managed system error message.
const MAX_SYSTEM_ERROR_BYTES: u32 = 4096;

/// Unmanaged ABI contract version shared with `csharp_runtime`.
///
/// Bump whenever any `UnmanagedCallersOnly` export signature changes; the host
/// refuses to start against a runtime built for a different version.
const INTEROP_CONTRACT_VERSION: u32 = 3;

// =============================================================================
// Types + Impls
// =============================================================================

/// Signature of the managed runtime entry point that receives the API table.
type InitFn = extern "system" fn(*const CsEngineApi) -> u8;
/// Signature returning the unmanaged ABI contract version.
type InteropVersionFn = extern "system" fn() -> u32;
/// Signature returning the number of registered scheduler systems.
type SystemCountFn = extern "system" fn() -> u32;
/// Signature returning the number of managed startup methods.
type StartupCountFn = extern "system" fn() -> u32;
/// Signature reporting whether one system declares a Commands parameter.
type SystemUsesCommandsFn = extern "system" fn(u32) -> u8;
/// Signature executing one managed startup method by index.
type RunStartupFn = extern "system" fn(u32) -> u8;
/// Signature returning the serialized component manifest byte length.
type ComponentManifestLengthFn = extern "system" fn() -> u32;
/// Signature copying the serialized component manifest into a caller buffer.
type CopyComponentManifestFn = extern "system" fn(*mut u8, u32) -> u8;
/// Signature returning the UTF-8 byte length of one system's reflected name.
type SystemNameLengthFn = extern "system" fn(u32) -> u32;
/// Signature copying one system's reflected name into a caller buffer.
type CopySystemNameFn = extern "system" fn(u32, *mut u8, u32) -> u8;
/// Signature returning how many accesses one system declared.
type SystemAccessCountFn = extern "system" fn(u32) -> u32;
/// Signature copying one system's reflected accesses into a caller buffer.
type GetSystemAccessFn = extern "system" fn(u32, u32, *mut NativeSystemAccess) -> u8;
/// Signature running one scheduler system by index.
///
/// Returns one on success and zero after the managed side records the
/// failure for [`SystemErrorMessageLengthFn`] retrieval.
type RunSystemFn = extern "system" fn(u32) -> u8;
/// Signature returning the UTF-8 byte length of one system's last error message.
type SystemErrorMessageLengthFn = extern "system" fn(u32) -> u32;
/// Signature copying one system's last error message into a caller buffer.
type CopySystemErrorMessageFn = extern "system" fn(u32, *mut u8, u32) -> u8;
/// Signature polling the collectible loader for a new project assembly and
/// reporting the swap outcome through the status codes below.
type PollReloadFn = extern "system" fn() -> u8;

/// Reflected metadata of one managed system, captured at startup.
///
/// The snapshot is compared against a re-reflection after every successful
/// reload, so a managed-side bug that silently changes system metadata can
// Every field below is resolved from the loaded assembly in both postures,
// but only `verify_systems_unchanged` reads them, and that runs solely on the
// reload path. The allowance is scoped to the configuration where the reader
// is compiled out rather than applied unconditionally, so a field that becomes
// genuinely unused still warns in a reloading build.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
/// never run stale index bindings unnoticed.
struct ManagedSystemSnapshot {
    /// Reflected native access list resolved at startup.
    accesses: Box<[NativeSystemAccess]>,
    /// Whether the managed system declared a Commands parameter.
    uses_commands: bool,
}

/// Owns one hosted managed runtime: CoreCLR through hostfxr, or a loaded
/// NativeAOT library. Keeping this alive guarantees every resolved function
/// pointer below stays valid for the host's lifetime.
///
/// The AOT variant is only constructed in the shipping posture (no
/// `hot_reload`), so a dev build may not reference it.
#[cfg_attr(feature = "hot_reload", allow(dead_code))]
pub(crate) enum ManagedRuntimeContext {
    /// CoreCLR booted through hostfxr (framework-dependent posture).
    Dotnet(DotnetRuntimeContext),
    /// A NativeAOT library loaded directly (self-contained posture).
    Aot(AotRuntimeContext),
}

/// Owns the hosted .NET context, stable API table, and reload callback.
///
/// Keeping `_runtime` and `_api` alive guarantees that both the managed
/// runtime and every native function pointer remain valid for registered
// Same reasoning as `ManagedSystemSnapshot` above: these are the assembly's
// polling and reflection exports, resolved at startup either way, and read
// only by `poll_reload` and `verify_systems_unchanged`.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
/// scheduler closures.
pub(crate) struct CSharpRuntime {
    /// Unmanaged export polling the collectible loader for a rebuilt assembly.
    poll_reload: PollReloadFn,
    /// Unmanaged export reporting the number of registered scheduler systems.
    system_count: SystemCountFn,
    /// Unmanaged export reporting how many accesses one system declared.
    access_count: SystemAccessCountFn,
    /// Unmanaged export copying one system's reflected accesses into a buffer.
    get_access: GetSystemAccessFn,
    /// Unmanaged export reporting whether one system declares a Commands parameter.
    system_uses_commands: SystemUsesCommandsFn,
    #[cfg(feature = "hot_reload")]
    /// Outcome of the most recent reload poll, for one-shot rejection logging.
    last_poll_status: u8,
    /// Metadata snapshot the active assembly is verified against after reload.
    system_snapshot: Vec<ManagedSystemSnapshot>,
    /// Keeps the hosted .NET runtime alive for the host's lifetime.
    _runtime: ManagedRuntimeContext,
    /// Keeps the native API table alive so registered closures stay valid.
    _api: Box<CsEngineApi>,
    /// Keeps the shared component bindings alive for reload verification.
    _bindings: Arc<ComponentBindings>,
}

/// Resolves one managed artifact (a runtime assembly, its `runtimeconfig.json`,
/// or the project assembly / AOT library) shipped next to the host executable,
/// falling back to the source-tree location the generated bundle baked in.
///
/// A shipping bundle is self-contained: every managed file the host needs is
/// copied into the same dated output folder as the executable, so a shipped
/// build prefers `current_exe()`'s directory and runs on any machine with no
/// engine source tree present. The developer layout (running straight from
/// `cargo run` against the `dotnet build` / `dotnet publish` outputs) has no
/// sidecars next to the exe, so the `workspace_root`-relative path is the
/// fallback. Only shipping postures reach this code, so `current_exe()` always
/// points at the shipping binary rather than a dev target.
fn shipped_or_baked(workspace_root: &Path, baked_relative_dir: &str, file_name: &str) -> PathBuf {
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        let shipped = exe_dir.join(file_name);
        if shipped.is_file() {
            return shipped;
        }
    }
    workspace_root.join(baked_relative_dir).join(file_name)
}

impl CSharpRuntime {
    /// Start .NET, load `csharp_runtime`, discover managed systems, and register
    /// each system with its reflected read/write access declaration.
    ///
    /// # Errors
    ///
    /// Returns an error if the runtime cannot start, a managed export is
    /// missing, runtime initialization fails, the component manifest cannot be
    /// copied or registered, a startup method fails, or a reflected access
    /// references an unregistered component.
    pub(crate) fn start(
        engine: &mut Engine,
        workspace_root: &Path,
        config: &CSharpModuleConfig,
        module_exposed: &[ModuleExposedComponent],
        mirror_methods: &[ResolvedMirrorMethod],
    ) -> Result<Self, CSharpError> {
        // Step 0: Merge the hardcoded shared renderer bindings with byte-level
        // bindings for every native component the optional modules exposed, so
        // a `project_cs` mirror whose full name matches a module component
        // resolves to the module's native storage.
        let shared_bindings = shared_component_bindings(engine);
        let mut bindings = shared_bindings;
        bindings.extend(module_native_bindings(engine, module_exposed));

        // Step 1: Resolve assembly paths, start .NET, and load managed exports.
        // A shipped bundle keeps the runtime sidecars and the project assembly
        // flat next to the executable, so prefer those copies (portable); the
        // generated bundle's source-tree paths are the developer fallback.
        let runtime_assembly_name = format!("{}.dll", config.runtime_assembly_name);
        let runtime_config_name = format!("{}.runtimeconfig.json", config.runtime_assembly_name);
        let project_assembly_name = format!("{}.dll", config.project_assembly_name);
        let assembly = shipped_or_baked(
            workspace_root,
            &config.runtime_output_subdirectory,
            &runtime_assembly_name,
        );
        let runtime_config = shipped_or_baked(
            workspace_root,
            &config.runtime_output_subdirectory,
            &runtime_config_name,
        );
        let project_assembly = shipped_or_baked(
            workspace_root,
            &config.project_output_subdirectory,
            &project_assembly_name,
        );
        // The managed side resolves the project assembly against this directory
        // plus the assembly file name; keep them in the same folder (shipped:
        // the exe's directory, developer: the baked build output).
        let project_dir = project_assembly
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| workspace_root.join(&config.project_output_subdirectory));
        std::env::set_var("ECS_CSHARP_PROJECT_DIR", &project_dir);
        std::env::set_var("ECS_CSHARP_PROJECT_ASSEMBLY", project_assembly_name);

        let runtime = DotnetRuntimeContext::new(&runtime_config)?;
        let type_name = format!(
            "TracyLive.Loader.LoaderInterop, {}",
            config.runtime_assembly_name
        );

        // Step 1a: Validate the unmanaged ABI contract before resolving any
        // export. A mismatched runtime assembly was built against different
        // export signatures and must be rebuilt before the host can proceed.
        let interop_version = runtime.get_unmanaged_fn::<InteropVersionFn>(
            &assembly,
            &type_name,
            "InteropVersion",
        )?;
        if interop_version() != INTEROP_CONTRACT_VERSION {
            return Err(CSharpError::InteropVersionMismatch {
                expected: INTEROP_CONTRACT_VERSION,
                actual: interop_version(),
            });
        }

        let init = runtime.get_unmanaged_fn::<InitFn>(&assembly, &type_name, "Init")?;
        let system_count =
            runtime.get_unmanaged_fn::<SystemCountFn>(&assembly, &type_name, "SystemCount")?;
        let startup_count =
            runtime.get_unmanaged_fn::<StartupCountFn>(&assembly, &type_name, "StartupCount")?;
        let system_uses_commands = runtime.get_unmanaged_fn::<SystemUsesCommandsFn>(
            &assembly,
            &type_name,
            "SystemUsesCommands",
        )?;
        let run_startup =
            runtime.get_unmanaged_fn::<RunStartupFn>(&assembly, &type_name, "RunStartup")?;
        let manifest_length = runtime.get_unmanaged_fn::<ComponentManifestLengthFn>(
            &assembly,
            &type_name,
            "ComponentManifestLength",
        )?;
        let copy_manifest = runtime.get_unmanaged_fn::<CopyComponentManifestFn>(
            &assembly,
            &type_name,
            "CopyComponentManifest",
        )?;
        let system_name_length = runtime.get_unmanaged_fn::<SystemNameLengthFn>(
            &assembly,
            &type_name,
            "SystemNameLength",
        )?;
        let copy_system_name = runtime.get_unmanaged_fn::<CopySystemNameFn>(
            &assembly,
            &type_name,
            "CopySystemName",
        )?;
        let access_count = runtime.get_unmanaged_fn::<SystemAccessCountFn>(
            &assembly,
            &type_name,
            "SystemAccessCount",
        )?;
        let get_access = runtime.get_unmanaged_fn::<GetSystemAccessFn>(
            &assembly,
            &type_name,
            "GetSystemAccess",
        )?;
        let run_system =
            runtime.get_unmanaged_fn::<RunSystemFn>(&assembly, &type_name, "RunSystem")?;
        let system_error_length = runtime.get_unmanaged_fn::<SystemErrorMessageLengthFn>(
            &assembly,
            &type_name,
            "SystemErrorMessageLength",
        )?;
        let copy_system_error = runtime.get_unmanaged_fn::<CopySystemErrorMessageFn>(
            &assembly,
            &type_name,
            "CopySystemErrorMessage",
        )?;
        let poll_reload =
            runtime.get_unmanaged_fn::<PollReloadFn>(&assembly, &type_name, "PollReload")?;

        // Step 2: Initialize the runtime bridge and register the component
        // manifest copied from the managed assembly.
        let api = Box::new(CsEngineApi::new(mirror_methods));
        if init(api.as_ref() as *const CsEngineApi) == 0 {
            return Err(CSharpError::RuntimeInitFailed);
        }

        // The manifest length comes from managed code, so it must be bounded
        // before the host allocates anything from it.
        let manifest_length = manifest_length();
        if !is_supported_manifest_length(manifest_length) {
            return Err(CSharpError::ManifestLengthOutOfRange {
                length: manifest_length,
                limit: MAX_COMPONENT_MANIFEST_BYTES,
            });
        }

        // Reserve explicitly so an allocation failure surfaces as a regular
        // error instead of aborting the host process.
        let mut manifest = Vec::new();
        manifest
            .try_reserve_exact(manifest_length as usize)
            .map_err(|_| CSharpError::ManifestAllocationFailed)?;
        manifest.resize(manifest_length as usize, 0);

        // The managed contract rejects any caller buffer smaller than the
        // manifest, so a successful copy guarantees a complete payload.
        if copy_manifest(manifest.as_mut_ptr(), manifest_length) == 0 {
            return Err(CSharpError::ManifestCopyFailed);
        }
        let bindings = Arc::new(register_component_manifest(engine, &manifest, bindings)?);

        // Step 3: Run every reflected managed startup method transactionally.
        // Commands are queued first and applied only when every startup
        // method reports success, so a failing generation leaves no partial
        // world state behind.
        let startup_bindings = Arc::clone(&bindings);
        let mut startup_failed = None;
        engine.queue_deferred_commands(|world, queue| {
            let no_accesses = [];
            for startup_index in 0..startup_count() {
                // A rejected scope means a managed startup method re-entered
                // the host. Running it without a scope would only produce
                // failing FFI calls, so treat it as this startup's failure.
                let Some(_guard) = ActiveSystemGuard::set_with_commands(
                    world,
                    queue,
                    &no_accesses,
                    &startup_bindings,
                    true,
                ) else {
                    startup_failed = Some(startup_index);
                    break;
                };
                if run_startup(startup_index) == 0 {
                    startup_failed = Some(startup_index);
                    break;
                }
            }
        });
        if let Some(index) = startup_failed {
            // Roll back the queued commands so the failure is truly transactional.
            engine.discard_deferred_commands();
            return Err(CSharpError::StartupFailed { index });
        }
        engine
            .flush_deferred_commands()
            .map_err(|errors| CSharpError::StartupCommandsFailed {
                details: format!("{errors:?}"),
            })?;

        // Step 4: Reflect each system's accesses and register it with the
        // scheduler under the exact resolved read/write list.
        let count = system_count();
        if count == 0 {
            return Err(CSharpError::NoSystems);
        }
        let mut system_snapshot = Vec::with_capacity(count as usize);
        for system_index in 0..count {
            let system_access_count = access_count(system_index);
            let mut managed_access = Vec::with_capacity(system_access_count as usize);
            for access_index in 0..system_access_count {
                let mut item = NativeSystemAccess {
                    component_key: 0,
                    component_key_high: 0,
                    mode: 0,
                };
                if get_access(system_index, access_index, &mut item) == 0 {
                    return Err(CSharpError::SystemAccessFailed {
                        system: system_index,
                        access: access_index,
                    });
                }
                managed_access.push(item);
            }

            let uses_commands = system_uses_commands(system_index) != 0;
            let mut access = derive_system_access(&managed_access, &bindings)?;
            access.set_uses_commands(uses_commands);
            // Snapshot the reflected metadata before moving the access list
            // into the scheduler closure, so reloads can verify that the
            // managed side never changes it silently.
            system_snapshot.push(ManagedSystemSnapshot {
                accesses: managed_access.clone().into_boxed_slice(),
                uses_commands,
            });
            let managed_access = managed_access.into_boxed_slice();
            let system_bindings = Arc::clone(&bindings);
            // Prefer the reflected managed name (type and method) so profiling
            // and scheduler debugging show real identities; fall back to a
            // synthetic index-based name when the export is unavailable.
            let name = managed_system_name(system_name_length, copy_system_name, system_index)
                .unwrap_or_else(|| format!("csharp_system_{system_index}"));
            // SAFETY: `derive_system_access` has resolved every managed access
            // and the closure exposes the world only under that exact list.
            unsafe {
                engine.register_system_with_access(
                    name,
                    access,
                    move |world: &mut World, queue: &mut CommandQueue| -> Result<(), SystemError> {
                        // As above: without a scope every managed callback
                        // this system makes would fail, so report it as a
                        // system error rather than running it blind.
                        let Some(_guard) = ActiveSystemGuard::set_with_commands(
                            world,
                            queue,
                            &managed_access,
                            &system_bindings,
                            uses_commands,
                        ) else {
                            return Err(SystemError::Managed {
                                message: "nested managed system invocation".to_string(),
                            });
                        };
                        if run_system(system_index) == 0 {
                            return Err(SystemError::Managed {
                                message: managed_system_error_message(
                                    system_error_length,
                                    copy_system_error,
                                    system_index,
                                ),
                            });
                        }
                        Ok(())
                    },
                );
            }
        }

        Ok(Self {
            poll_reload,
            system_count,
            access_count,
            get_access,
            system_uses_commands,
            #[cfg(feature = "hot_reload")]
            last_poll_status: POLL_NO_CHANGE,
            system_snapshot,
            _runtime: ManagedRuntimeContext::Dotnet(runtime),
            _api: api,
            _bindings: bindings,
        })
    }

    /// Start a NativeAOT-published library, resolve the loader exports by
    /// symbol, discover managed systems, and register each system with its
    /// declared read/write access.
    ///
    /// Mirrors [`Self::start`] but replaces the hostfxr bootstrap with a direct
    /// `libloading` load of the AOT native library (which embeds a trimmed
    /// runtime, so no .NET install and no JIT are involved). Everything from
    /// the component-manifest exchange onward is identical.
    ///
    /// # Errors
    ///
    /// Returns an error if the library cannot be loaded, an export is missing,
    /// the ABI contract mismatches, initialization fails, the component
    /// manifest cannot be registered, a startup method fails, or a reflected
    /// access references an unregistered component.
    #[cfg_attr(feature = "hot_reload", allow(dead_code))]
    pub(crate) fn start_aot(
        engine: &mut Engine,
        workspace_root: &Path,
        config: &CSharpModuleConfig,
        module_exposed: &[ModuleExposedComponent],
        mirror_methods: &[ResolvedMirrorMethod],
    ) -> Result<Self, CSharpError> {
        // Step 0: merge the shared renderer bindings with byte-level bindings
        // for every native component the optional modules exposed, exactly as
        // the hostfxr path does.
        let shared_bindings = shared_component_bindings(engine);
        let mut bindings = shared_bindings;
        bindings.extend(module_native_bindings(engine, module_exposed));

        // Step 1: load the AOT native library and resolve every export by its
        // `pill_*` symbol. A shipped bundle carries the library next to the
        // executable (portable); the `dotnet publish` output the generated
        // bundle describes is the developer fallback.
        let library_path = shipped_or_baked(
            workspace_root,
            &config.project_output_subdirectory,
            &format!("{}.dll", config.project_assembly_name),
        );
        let runtime = AotRuntimeContext::new(&library_path)?;

        let interop_version =
            runtime.get_unmanaged_fn::<InteropVersionFn>("pill_interop_version")?;
        if interop_version() != INTEROP_CONTRACT_VERSION {
            return Err(CSharpError::InteropVersionMismatch {
                expected: INTEROP_CONTRACT_VERSION,
                actual: interop_version(),
            });
        }
        let init = runtime.get_unmanaged_fn::<InitFn>("pill_init")?;
        let system_count = runtime.get_unmanaged_fn::<SystemCountFn>("pill_system_count")?;
        let startup_count = runtime.get_unmanaged_fn::<StartupCountFn>("pill_startup_count")?;
        let system_uses_commands =
            runtime.get_unmanaged_fn::<SystemUsesCommandsFn>("pill_system_uses_commands")?;
        let run_startup = runtime.get_unmanaged_fn::<RunStartupFn>("pill_run_startup")?;
        let manifest_length = runtime
            .get_unmanaged_fn::<ComponentManifestLengthFn>("pill_component_manifest_length")?;
        let copy_manifest =
            runtime.get_unmanaged_fn::<CopyComponentManifestFn>("pill_copy_component_manifest")?;
        let system_name_length =
            runtime.get_unmanaged_fn::<SystemNameLengthFn>("pill_system_name_length")?;
        let copy_system_name =
            runtime.get_unmanaged_fn::<CopySystemNameFn>("pill_copy_system_name")?;
        let access_count =
            runtime.get_unmanaged_fn::<SystemAccessCountFn>("pill_system_access_count")?;
        let get_access = runtime.get_unmanaged_fn::<GetSystemAccessFn>("pill_get_system_access")?;
        let run_system = runtime.get_unmanaged_fn::<RunSystemFn>("pill_run_system")?;
        let system_error_length = runtime
            .get_unmanaged_fn::<SystemErrorMessageLengthFn>("pill_system_error_message_length")?;
        let copy_system_error = runtime
            .get_unmanaged_fn::<CopySystemErrorMessageFn>("pill_copy_system_error_message")?;
        let poll_reload = runtime.get_unmanaged_fn::<PollReloadFn>("pill_poll_reload")?;

        // Step 2: initialize the bridge and register the component manifest.
        let api = Box::new(CsEngineApi::new(mirror_methods));
        if init(api.as_ref() as *const CsEngineApi) == 0 {
            return Err(CSharpError::RuntimeInitFailed);
        }

        let manifest_length = manifest_length();
        if !is_supported_manifest_length(manifest_length) {
            return Err(CSharpError::ManifestLengthOutOfRange {
                length: manifest_length,
                limit: MAX_COMPONENT_MANIFEST_BYTES,
            });
        }
        let mut manifest = Vec::new();
        manifest
            .try_reserve_exact(manifest_length as usize)
            .map_err(|_| CSharpError::ManifestAllocationFailed)?;
        manifest.resize(manifest_length as usize, 0);
        if copy_manifest(manifest.as_mut_ptr(), manifest_length) == 0 {
            return Err(CSharpError::ManifestCopyFailed);
        }
        let bindings = Arc::new(register_component_manifest(engine, &manifest, bindings)?);

        // Step 3: run every managed startup method transactionally.
        let startup_bindings = Arc::clone(&bindings);
        let mut startup_failed = None;
        engine.queue_deferred_commands(|world, queue| {
            let no_accesses = [];
            for startup_index in 0..startup_count() {
                let Some(_guard) = ActiveSystemGuard::set_with_commands(
                    world,
                    queue,
                    &no_accesses,
                    &startup_bindings,
                    true,
                ) else {
                    startup_failed = Some(startup_index);
                    break;
                };
                if run_startup(startup_index) == 0 {
                    startup_failed = Some(startup_index);
                    break;
                }
            }
        });
        if let Some(index) = startup_failed {
            engine.discard_deferred_commands();
            return Err(CSharpError::StartupFailed { index });
        }
        engine
            .flush_deferred_commands()
            .map_err(|errors| CSharpError::StartupCommandsFailed {
                details: format!("{errors:?}"),
            })?;

        // Step 4: register each system with the scheduler under its accesses.
        let count = system_count();
        if count == 0 {
            return Err(CSharpError::NoSystems);
        }
        let mut system_snapshot = Vec::with_capacity(count as usize);
        for system_index in 0..count {
            let system_access_count = access_count(system_index);
            let mut managed_access = Vec::with_capacity(system_access_count as usize);
            for access_index in 0..system_access_count {
                let mut item = NativeSystemAccess {
                    component_key: 0,
                    component_key_high: 0,
                    mode: 0,
                };
                if get_access(system_index, access_index, &mut item) == 0 {
                    return Err(CSharpError::SystemAccessFailed {
                        system: system_index,
                        access: access_index,
                    });
                }
                managed_access.push(item);
            }

            let uses_commands = system_uses_commands(system_index) != 0;
            let mut access = derive_system_access(&managed_access, &bindings)?;
            access.set_uses_commands(uses_commands);
            system_snapshot.push(ManagedSystemSnapshot {
                accesses: managed_access.clone().into_boxed_slice(),
                uses_commands,
            });
            let managed_access = managed_access.into_boxed_slice();
            let system_bindings = Arc::clone(&bindings);
            let name = managed_system_name(system_name_length, copy_system_name, system_index)
                .unwrap_or_else(|| format!("csharp_system_{system_index}"));
            // SAFETY: `derive_system_access` has resolved every managed access
            // and the closure exposes the world only under that exact list.
            unsafe {
                engine.register_system_with_access(
                    name,
                    access,
                    move |world: &mut World, queue: &mut CommandQueue| -> Result<(), SystemError> {
                        let Some(_guard) = ActiveSystemGuard::set_with_commands(
                            world,
                            queue,
                            &managed_access,
                            &system_bindings,
                            uses_commands,
                        ) else {
                            return Err(SystemError::Managed {
                                message: "nested managed system invocation".to_string(),
                            });
                        };
                        if run_system(system_index) == 0 {
                            return Err(SystemError::Managed {
                                message: managed_system_error_message(
                                    system_error_length,
                                    copy_system_error,
                                    system_index,
                                ),
                            });
                        }
                        Ok(())
                    },
                );
            }
        }

        Ok(Self {
            poll_reload,
            system_count,
            access_count,
            get_access,
            system_uses_commands,
            #[cfg(feature = "hot_reload")]
            last_poll_status: POLL_NO_CHANGE,
            system_snapshot,
            _runtime: ManagedRuntimeContext::Aot(runtime),
            _api: api,
            _bindings: bindings,
        })
    }

    /// Poll the collectible loader and report the outcome of any swap attempt.
    ///
    /// The managed loader validates the rebuilt assembly's component manifest
    /// and system signatures before swapping. A rejection is logged once per
    #[cfg(feature = "hot_reload")]
    /// attempt so the per-frame poll cannot drown the terminal in messages.
    pub(crate) fn poll_reload(&mut self) -> u8 {
        let status = (self.poll_reload)();
        if status == POLL_REJECTED && self.last_poll_status != POLL_REJECTED {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "C# reload rejected: component or system signatures changed; restart the host to rebuild the native component registry and scheduler"
            );
        }
        if status == POLL_RELOADED {
            if self.verify_systems_unchanged() {
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "C# hot reload complete"
                );
            } else {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "reloaded assembly exposes different system metadata than the registered snapshot; restart the host to re-register systems"
                );
            }
        }
        self.last_poll_status = status;
        status
    }

    /// Re-reflect the active project assembly and verify that its system metadata
    /// still matches the snapshot captured at startup.
    ///
    /// The managed loader already rejects swaps whose system signatures
    /// changed, so this is defense in depth: a mismatch means the loader
    /// validation and the host snapshot disagree, and continuing would run
    #[cfg(feature = "hot_reload")]
    /// stale index bindings.
    fn verify_systems_unchanged(&self) -> bool {
        let count = (self.system_count)();
        if count as usize != self.system_snapshot.len() {
            return false;
        }
        for system_index in 0..count {
            let snapshot = &self.system_snapshot[system_index as usize];
            let access_count = (self.access_count)(system_index);
            if access_count as usize != snapshot.accesses.len() {
                return false;
            }
            for access_index in 0..access_count {
                let mut item = NativeSystemAccess {
                    component_key: 0,
                    component_key_high: 0,
                    mode: 0,
                };
                if (self.get_access)(system_index, access_index, &mut item) == 0 {
                    return false;
                }
                if item != snapshot.accesses[access_index as usize] {
                    return false;
                }
            }
            if ((self.system_uses_commands)(system_index) != 0) != snapshot.uses_commands {
                return false;
            }
        }
        true
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Whether a managed-reported manifest length lies within the supported range.
///
/// Rejects zero and any value above [`MAX_COMPONENT_MANIFEST_BYTES`], so a
/// buggy managed assembly can never drive an unbounded host allocation.
pub(super) fn is_supported_manifest_length(length: u32) -> bool {
    (1..=MAX_COMPONENT_MANIFEST_BYTES).contains(&length)
}

/// Fetch the reflected managed name for one system.
///
/// Returns `None` when the name is missing, oversized, or not valid UTF-8;
/// callers fall back to a synthetic index-based name.
fn managed_system_name(
    name_length: SystemNameLengthFn,
    copy_name: CopySystemNameFn,
    system_index: u32,
) -> Option<String> {
    let length = name_length(system_index);
    if length == 0 || length > MAX_SYSTEM_NAME_BYTES {
        return None;
    }
    let mut buffer = vec![0_u8; length as usize];
    if copy_name(system_index, buffer.as_mut_ptr(), length) == 0 {
        return None;
    }
    String::from_utf8(buffer).ok()
}

/// Copy the failure message one managed system reported after a failed run.
///
/// Falls back to a neutral message when the managed side reports no text,
/// an oversized message, or a failed copy.
fn managed_system_error_message(
    length: SystemErrorMessageLengthFn,
    copy: CopySystemErrorMessageFn,
    system_index: u32,
) -> String {
    const NEUTRAL_MESSAGE: &str = "managed system reported failure";
    let length = length(system_index);
    if length == 0 || length > MAX_SYSTEM_ERROR_BYTES {
        return String::from(NEUTRAL_MESSAGE);
    }
    let mut buffer = vec![0_u8; length as usize];
    if copy(system_index, buffer.as_mut_ptr(), length) == 0 {
        return String::from(NEUTRAL_MESSAGE);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Translate managed component modes into native scheduler metadata.
///
/// # Errors
///
/// Returns an error if a declared component key is not registered or an
/// access mode is neither read nor write.
pub(super) fn derive_system_access(
    accesses: &[NativeSystemAccess],
    bindings: &ComponentBindings,
) -> Result<SystemAccess, CSharpError> {
    let mut result = SystemAccess::new();
    for access in accesses {
        let stable_id =
            StableComponentId::from_halves(access.component_key, access.component_key_high);
        let component = bindings
            .get(&stable_id)
            .map(|binding| binding.component_id())
            .ok_or_else(|| CSharpError::UnregisteredComponent {
                key: format!(
                    "{:016X}{:016X}",
                    access.component_key_high, access.component_key
                ),
            })?;
        match access.mode {
            0 => result.add_read(component),
            1 => result.add_write(component),
            mode => return Err(CSharpError::UnknownAccessMode { mode }),
        }
    }
    Ok(result)
}
