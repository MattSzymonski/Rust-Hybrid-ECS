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
#[cfg(feature = "hot_reload")]
use pill_engine::SystemOwner;
use pill_engine::{Engine, SystemAccess, SystemError, World};

// Current crate
use super::abi::{CsEngineApi, NativeSystemAccess};
use super::aot_runtime::AotRuntimeContext;
#[cfg_attr(not(feature = "hot_reload"), allow(unused_imports))]
use super::components::apply_component_manifest_on_reload;
use super::components::{
    module_native_bindings, register_component_manifest, shared_component_bindings, BindingStore,
    ComponentBindings, ModuleExposedComponent, StableComponentId,
};
use super::context::ActiveSystemGuard;
use super::csharp_runtime::DotnetRuntimeContext;
#[cfg(feature = "hot_reload")]
use super::fast_compile::{FastCompileOutcome, FastCompiler};
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
#[cfg(feature = "hot_reload")]
/// A behaviour-compatible assembly is loaded and waiting on the host's verdict
/// about its component manifest.
///
/// The managed loader cannot decide it: whether a manifest change can be
/// applied depends on what each component is bound to natively, and only the
/// host holds those bindings. It answers within the same poll.
pub(crate) const POLL_MANIFEST_PENDING: u8 = 3;

/// Maximum UTF-8 byte length accepted for a managed system name.
const MAX_SYSTEM_NAME_BYTES: u32 = 1024;

/// Maximum UTF-8 byte length accepted for a managed system error message.
const MAX_SYSTEM_ERROR_BYTES: u32 = 4096;

/// Upper bound on the number of systems one managed assembly may report.
///
/// The count sizes the startup snapshot before a single system is reflected,
/// so it needs the same kind of bound the manifest lengths above have: 4,096
/// systems is far past any real project and keeps the snapshot under a
/// megabyte even with its access lists.
pub(super) const MAX_SYSTEMS_PER_ASSEMBLY: u32 = 4096;

/// Upper bound on the accesses one managed system may declare.
pub(super) const MAX_ACCESSES_PER_SYSTEM: u32 = 1024;

/// Unmanaged ABI contract version shared with `csharp_runtime`.
///
/// Bump whenever any `UnmanagedCallersOnly` export signature changes, when
/// the [`CsEngineApi`] table's field layout does - the managed runtime copies
/// that struct field by field, so a new slot makes the two sides disagree
/// about every slot after it - or when a struct the exports exchange changes
/// shape. Bumped to 4 by the mirror-epoch slot and to 5 by the chunk's const
/// `entities` pointer, which a stale runtime would otherwise read as a
/// 48-byte struct. The host refuses to start against a runtime built for a
/// different version. Bumped to 10 by `NotifyAssemblyReplaced`, which the
/// in-process compile path calls to collapse the loader's poll interval.
const INTEROP_CONTRACT_VERSION: u32 = 10;

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
/// Signature telling the loader a new assembly is already complete on disk.
#[cfg(feature = "hot_reload")]
type NotifyAssemblyReplacedFn = extern "system" fn();
/// Signature returning the parked manifest's byte length.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
type PendingManifestLengthFn = extern "system" fn() -> u32;
/// Signature copying the parked manifest into a caller buffer.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
type CopyPendingManifestFn = extern "system" fn(*mut u8, u32) -> u8;
/// Signature installing the parked version once its manifest is in force.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
type CommitReloadFn = extern "system" fn() -> u8;
/// Signature discarding the parked version when its manifest is refused.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
type AbortReloadFn = extern "system" fn() -> u8;

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
    /// The name the system was registered under, after the synthetic fallback.
    ///
    /// Recorded because a rename changes nothing else a reload can observe -
    /// not the accesses, not the Commands flag - yet it is the identity the
    /// scheduler, the profiler and every log line report.
    name: Box<str>,
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
    #[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
    Dotnet(DotnetRuntimeContext),
    /// A NativeAOT library loaded directly (self-contained posture).
    #[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
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
    /// Byte length of the manifest a parked version carries.
    #[cfg(feature = "hot_reload")]
    pending_manifest_length: PendingManifestLengthFn,
    /// Copies the parked version's manifest for validation.
    #[cfg(feature = "hot_reload")]
    copy_pending_manifest: CopyPendingManifestFn,
    /// Installs the parked version once its manifest is in force.
    #[cfg(feature = "hot_reload")]
    commit_reload: CommitReloadFn,
    /// Discards the parked version when its manifest cannot be applied.
    #[cfg(feature = "hot_reload")]
    abort_reload: AbortReloadFn,
    /// Unmanaged export reporting the number of registered scheduler systems.
    system_count: SystemCountFn,
    /// Unmanaged export reporting how many accesses one system declared.
    access_count: SystemAccessCountFn,
    /// Unmanaged export copying one system's reflected accesses into a buffer.
    get_access: GetSystemAccessFn,
    /// Unmanaged export reporting whether one system declares a Commands parameter.
    system_uses_commands: SystemUsesCommandsFn,
    /// Every export a re-registration pass needs.
    ///
    /// Held so a reload can rebuild the scheduler's managed systems from the
    /// arriving assembly. The four fields above duplicate members of this
    /// bundle because the reload's cheap comparison path reads them directly,
    /// one system at a time, without cloning anything.
    exports: SystemExports,
    #[cfg(feature = "hot_reload")]
    /// Outcome of the most recent reload poll, for one-shot rejection logging.
    last_poll_status: u8,
    /// Set once the world has taken a manifest whose assembly never loaded.
    ///
    /// The one outcome the commit handshake cannot make safe by ordering: the
    /// manifest is applied while the outgoing assembly still runs, so a refusal
    /// normally costs an unload and nothing else - but if the *commit* fails
    /// after that, the world holds layouts belonging to an assembly that was
    /// never installed. Every later frame would read migrated rows through the
    /// old assembly's expectations, so the project's systems are cleared and
    /// this latches to keep them cleared until the host restarts.
    #[cfg(feature = "hot_reload")]
    manifest_applied_without_assembly: bool,
    /// Metadata snapshot the active assembly is verified against after reload.
    system_snapshot: Vec<ManagedSystemSnapshot>,
    /// Unmanaged export reporting the current component manifest's length.
    manifest_length: ComponentManifestLengthFn,
    /// Unmanaged export copying that manifest into a host buffer.
    copy_manifest: CopyComponentManifestFn,
    /// Serialized manifest the bindings below were built from.
    ///
    /// A reload compares the swapped assembly's manifest against this one before
    /// it considers applying anything, so an ordinary behaviour-only swap costs
    /// one copy and one comparison.
    applied_manifest: Vec<u8>,
    /// The live component-binding table, shared with every registered system.
    ///
    /// Shared rather than cloned per system so a reload can rewrite it in
    /// place: each run locks it, which is what makes a reshaped component's new
    /// layout visible without re-registering the systems that read it.
    bindings: Arc<BindingStore>,
    /// Clears the loader's poll interval after an in-process compile.
    ///
    /// Only useful together with the compiler above: the certainty it reports is
    /// exactly the certainty an in-process compile produces. `None` in the AOT
    /// posture, which never reloads - and which would otherwise oblige every
    /// shipped project to re-export a symbol it can never call, since NativeAOT
    /// exports only from the root assembly.
    #[cfg(feature = "hot_reload")]
    notify_assembly_replaced: Option<NotifyAssemblyReplacedFn>,
    /// The in-process Roslyn compiler, when one could be loaded.
    ///
    /// `None` leaves every reload on the ordinary `dotnet build` path, which is
    /// the AOT posture's permanent state and the reloading posture's fallback
    /// when the compiler cannot be built or loaded.
    #[cfg(feature = "hot_reload")]
    fast_compiler: Option<FastCompiler>,
    /// Keeps the hosted .NET runtime alive for the host's lifetime.
    _runtime: ManagedRuntimeContext,
    /// Keeps the native API table alive so registered closures stay valid.
    _api: Box<CsEngineApi>,
}

/// The managed exports one system-registration pass needs.
///
/// Bundled rather than passed loose because the pass has three callers - the
/// reloading backend's start, the AOT backend's start, and the reload that
/// re-registers a changed system set. Nine loose function pointers per call
/// site is how the two startups came to hold character-identical copies of the
/// same loop, which then had to be kept in step by hand.
#[derive(Clone, Copy)]
struct SystemExports {
    /// Reports how many systems the assembly registered.
    system_count: SystemCountFn,
    /// Reports how many accesses one system declared.
    access_count: SystemAccessCountFn,
    /// Copies one system's reflected accesses into a caller buffer.
    get_access: GetSystemAccessFn,
    /// Reports whether one system declares a Commands parameter.
    system_uses_commands: SystemUsesCommandsFn,
    /// Invokes one system for a frame.
    run_system: RunSystemFn,
    /// Byte length of one system's reflected name.
    system_name_length: SystemNameLengthFn,
    /// Copies that name into a caller buffer.
    copy_system_name: CopySystemNameFn,
    /// Byte length of the message a failed system left.
    system_error_length: SystemErrorMessageLengthFn,
    /// Copies that message into a caller buffer.
    copy_system_error: CopySystemErrorMessageFn,
}

/// Reflect every managed system and register it with the scheduler.
///
/// The single registration path: both startups and the reload re-registration
/// call it, which is what keeps "how a system is registered" from drifting
/// between the posture that starts one and the posture that replaces one. The
/// returned snapshot is the metadata a later reload compares against to decide
/// whether the set changed at all.
///
/// # Errors
///
/// Returns [`CSharpError::NoSystems`] for an assembly that registered none,
/// [`CSharpError::SystemAccessFailed`] when an access cannot be read, and
/// whatever [`derive_system_access`] refuses - an unregistered component or
/// resource key, or an unknown access mode.
/// One managed system resolved and validated, but not yet handed to the engine.
///
/// The split this type exists for is the point: resolving a system can fail -
/// an access naming a component nobody registered is exactly what
/// `derive_system_access` refuses - while handing a resolved system to the
/// scheduler cannot. Preparing every system before registering any of them is
/// what lets a reload refuse a bad assembly with the previous generation's
/// systems still installed, instead of clearing them first and discovering the
/// refusal with nothing left to run.
struct PreparedManagedSystem {
    /// Name the system registers under, after the synthetic fallback.
    name: String,
    /// Scheduler access list, already resolved against the binding table.
    access: SystemAccess,
    /// Reflected managed accesses the run closure installs its scope from.
    managed_access: Box<[NativeSystemAccess]>,
    /// Whether the managed system declared a `Commands` parameter.
    uses_commands: bool,
    /// Managed discovery index this system is dispatched by.
    system_index: u32,
    /// Metadata a later reload compares to tell a behaviour swap from a
    /// signature change.
    snapshot: ManagedSystemSnapshot,
}

/// Resolve every managed system without touching the engine.
///
/// This is the half that can fail. It reads each system's reflected accesses,
/// resolves them against the binding table, and captures the metadata the
/// scheduler closure needs - but registers nothing, so a failure here leaves
/// the engine exactly as it was.
///
/// # Errors
///
/// Returns [`CSharpError::NoSystems`] when the assembly declares none, and
/// whatever `derive_system_access` refuses when an access names a component or
/// resource that is not registered.
fn prepare_managed_systems(
    bindings: &Arc<BindingStore>,
    exports: SystemExports,
) -> Result<Vec<PreparedManagedSystem>, CSharpError> {
    let count = (exports.system_count)();
    if count == 0 {
        return Err(CSharpError::NoSystems);
    }
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(checked_system_count(count)?)
        .map_err(|_| CSharpError::SystemSnapshotAllocationFailed)?;
    for system_index in 0..count {
        let system_access_count = (exports.access_count)(system_index);
        let mut managed_access = Vec::with_capacity(checked_access_count(system_access_count)?);
        for access_index in 0..system_access_count {
            let mut item = NativeSystemAccess {
                component_key: 0,
                component_key_high: 0,
                mode: 0,
                kind: 0,
            };
            if (exports.get_access)(system_index, access_index, &mut item) == 0 {
                return Err(CSharpError::SystemAccessFailed {
                    system: system_index,
                    access: access_index,
                });
            }
            managed_access.push(item);
        }
        // The reflected count and the recorded accesses move together: a
        // future early exit inside the fill loop must not leave the snapshot
        // short of what the managed side reported.
        debug_assert_eq!(managed_access.len(), system_access_count as usize);

        let uses_commands = (exports.system_uses_commands)(system_index) != 0;
        let mut access = derive_system_access(&managed_access, &bindings.read())?;
        access.set_uses_commands(uses_commands);
        // Prefer the reflected managed name (type and method) so profiling and
        // scheduler debugging show real identities; fall back to a synthetic
        // index-based name when the export is unavailable.
        let name = resolved_system_name(
            exports.system_name_length,
            exports.copy_system_name,
            system_index,
        );
        // Snapshot the reflected metadata before moving the access list into
        // the scheduler closure, so a later reload can tell a behaviour-only
        // swap from one that changed a signature.
        let snapshot = ManagedSystemSnapshot {
            accesses: managed_access.clone().into_boxed_slice(),
            uses_commands,
            name: name.as_str().into(),
        };
        prepared.push(PreparedManagedSystem {
            name,
            access,
            managed_access: managed_access.into_boxed_slice(),
            uses_commands,
            system_index,
            snapshot,
        });
    }
    Ok(prepared)
}

/// Hand every prepared system to the scheduler.
///
/// The half that cannot fail. Every access was resolved by
/// [`prepare_managed_systems`], so nothing here can refuse, which is what makes
/// the caller's clear-then-register sequence safe.
fn commit_managed_systems(
    engine: &mut Engine,
    bindings: &Arc<BindingStore>,
    exports: SystemExports,
    prepared: Vec<PreparedManagedSystem>,
) -> Vec<ManagedSystemSnapshot> {
    let mut system_snapshot = Vec::with_capacity(prepared.len());
    for system in prepared {
        let PreparedManagedSystem {
            name,
            access,
            managed_access,
            uses_commands,
            system_index,
            snapshot,
        } = system;
        system_snapshot.push(snapshot);
        let system_bindings = Arc::clone(bindings);
        let run_system = exports.run_system;
        let system_error_length = exports.system_error_length;
        let copy_system_error = exports.copy_system_error;
        // SAFETY: `derive_system_access` has resolved every managed access and
        // the closure exposes the world only under that exact list.
        unsafe {
            engine.register_system_with_access(
                name,
                access,
                move |world: &mut World, queue: &mut CommandQueue| -> Result<(), SystemError> {
                    // Without a scope every managed callback this system makes
                    // would fail, so report it as a system error rather than
                    // running it blind. One read of the live table for the
                    // whole run: a reload can add or reshape a component
                    // between frames, and the scope has to describe the
                    // storage this run actually touches.
                    let system_bindings = system_bindings.read();
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
    system_snapshot
}

/// Resolve and register every managed system in one step.
///
/// The startup path's entry point: there is nothing installed yet, so the
/// prepare/commit split buys nothing and the two halves run back to back. A
/// reload calls them separately - see `CSharpRuntime::reregister_systems`.
///
/// # Errors
///
/// Whatever [`prepare_managed_systems`] refuses.
fn register_managed_systems(
    engine: &mut Engine,
    bindings: &Arc<BindingStore>,
    exports: SystemExports,
) -> Result<Vec<ManagedSystemSnapshot>, CSharpError> {
    let prepared = prepare_managed_systems(bindings, exports)?;
    Ok(commit_managed_systems(engine, bindings, exports, prepared))
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
        // Started here rather than at the end of startup so its background
        // warmup - about 1.6 seconds of JIT and metadata reading - overlaps
        // component registration, the startup methods and system registration
        // instead of landing on the developer's first edit.
        #[cfg(feature = "hot_reload")]
        let fast_compiler = FastCompiler::try_new(&runtime, workspace_root, config);
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
        // Kept under its own name: the value below shadows it, and reading the
        // manifest again after a swap needs the export, not the first length.
        let manifest_length_export = manifest_length;
        // The reload handshake: a version whose manifest changed parks until
        // the host has applied it, then commits or aborts.
        #[cfg(feature = "hot_reload")]
        let pending_manifest_length = runtime.get_unmanaged_fn::<PendingManifestLengthFn>(
            &assembly,
            &type_name,
            "PendingManifestLength",
        )?;
        #[cfg(feature = "hot_reload")]
        let copy_pending_manifest = runtime.get_unmanaged_fn::<CopyPendingManifestFn>(
            &assembly,
            &type_name,
            "CopyPendingManifest",
        )?;
        #[cfg(feature = "hot_reload")]
        let commit_reload =
            runtime.get_unmanaged_fn::<CommitReloadFn>(&assembly, &type_name, "CommitReload")?;
        #[cfg(feature = "hot_reload")]
        let abort_reload =
            runtime.get_unmanaged_fn::<AbortReloadFn>(&assembly, &type_name, "AbortReload")?;
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
        #[cfg(feature = "hot_reload")]
        let notify_assembly_replaced = Some(runtime.get_unmanaged_fn::<NotifyAssemblyReplacedFn>(
            &assembly,
            &type_name,
            "NotifyAssemblyReplaced",
        )?);

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
        let bindings = Arc::new(BindingStore::new(register_component_manifest(
            engine, &manifest, bindings,
        )?));

        // Step 3: Run every reflected managed startup method transactionally.
        // Commands are queued first and applied only when every startup
        // method reports success, so a failing generation leaves no partial
        // world state behind.
        let startup_bindings = Arc::clone(&bindings);
        let mut startup_failed = None;
        engine.queue_deferred_commands(|world, queue| {
            let no_accesses = [];
            // One read of the live table for the whole startup batch: every
            // method in it resolves components through the entries the manifest
            // has just registered.
            let startup_bindings = startup_bindings.read();
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
        let exports = SystemExports {
            system_count,
            access_count,
            get_access,
            system_uses_commands,
            run_system,
            system_name_length,
            copy_system_name,
            system_error_length,
            copy_system_error,
        };
        let system_snapshot = register_managed_systems(engine, &bindings, exports)?;

        Ok(Self {
            poll_reload,
            system_count,
            access_count,
            get_access,
            system_uses_commands,
            exports,
            #[cfg(feature = "hot_reload")]
            last_poll_status: POLL_NO_CHANGE,
            system_snapshot,
            manifest_length: manifest_length_export,
            #[cfg(feature = "hot_reload")]
            pending_manifest_length,
            #[cfg(feature = "hot_reload")]
            copy_pending_manifest,
            #[cfg(feature = "hot_reload")]
            commit_reload,
            #[cfg(feature = "hot_reload")]
            abort_reload,
            copy_manifest,
            applied_manifest: manifest,
            bindings,
            #[cfg(feature = "hot_reload")]
            manifest_applied_without_assembly: false,
            #[cfg(feature = "hot_reload")]
            notify_assembly_replaced,
            #[cfg(feature = "hot_reload")]
            fast_compiler,
            _runtime: ManagedRuntimeContext::Dotnet(runtime),
            _api: api,
        })
    }

    /// Recompile the project in-process, when a compiler could be loaded.
    ///
    /// `None` means there is no fast path at all and the caller must build
    /// normally; a [`FastCompileOutcome::Unavailable`] means the fast path
    /// exists but cannot answer this particular reload.
    #[cfg(feature = "hot_reload")]
    pub(crate) fn fast_compile(
        &self,
        workspace_root: &Path,
        watch_directory: &str,
    ) -> Option<FastCompileOutcome> {
        let outcome = self
            .fast_compiler
            .as_ref()?
            .compile(workspace_root, watch_directory);
        // The loader samples the assembly's timestamp on an interval because it
        // cannot otherwise tell a finished build from a half-copied one. This
        // compile wrote the file itself, through an atomic rename, so the next
        // poll can skip that wait entirely.
        if matches!(outcome, FastCompileOutcome::Compiled { .. }) {
            if let Some(notify) = self.notify_assembly_replaced {
                notify();
            }
        }
        Some(outcome)
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
        // Kept under its own name: the value below shadows it, and the runtime
        // handle needs the export to read the manifest again after a swap.
        let manifest_length_export = manifest_length;
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
        #[cfg(feature = "hot_reload")]
        let pending_manifest_length =
            runtime.get_unmanaged_fn::<PendingManifestLengthFn>("pill_pending_manifest_length")?;
        #[cfg(feature = "hot_reload")]
        let copy_pending_manifest =
            runtime.get_unmanaged_fn::<CopyPendingManifestFn>("pill_copy_pending_manifest")?;
        #[cfg(feature = "hot_reload")]
        let commit_reload = runtime.get_unmanaged_fn::<CommitReloadFn>("pill_commit_reload")?;
        #[cfg(feature = "hot_reload")]
        let abort_reload = runtime.get_unmanaged_fn::<AbortReloadFn>("pill_abort_reload")?;

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
        let bindings = Arc::new(BindingStore::new(register_component_manifest(
            engine, &manifest, bindings,
        )?));

        // Step 3: run every managed startup method transactionally.
        let startup_bindings = Arc::clone(&bindings);
        let mut startup_failed = None;
        engine.queue_deferred_commands(|world, queue| {
            let no_accesses = [];
            let startup_bindings = startup_bindings.read();
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
        let exports = SystemExports {
            system_count,
            access_count,
            get_access,
            system_uses_commands,
            run_system,
            system_name_length,
            copy_system_name,
            system_error_length,
            copy_system_error,
        };
        let system_snapshot = register_managed_systems(engine, &bindings, exports)?;

        Ok(Self {
            poll_reload,
            system_count,
            access_count,
            get_access,
            system_uses_commands,
            exports,
            #[cfg(feature = "hot_reload")]
            last_poll_status: POLL_NO_CHANGE,
            system_snapshot,
            manifest_length: manifest_length_export,
            #[cfg(feature = "hot_reload")]
            pending_manifest_length,
            #[cfg(feature = "hot_reload")]
            copy_pending_manifest,
            #[cfg(feature = "hot_reload")]
            commit_reload,
            #[cfg(feature = "hot_reload")]
            abort_reload,
            copy_manifest,
            applied_manifest: manifest,
            bindings,
            #[cfg(feature = "hot_reload")]
            manifest_applied_without_assembly: false,
            // A NativeAOT bundle ships no compiler and never reloads.
            #[cfg(feature = "hot_reload")]
            notify_assembly_replaced: None,
            #[cfg(feature = "hot_reload")]
            fast_compiler: None,
            _runtime: ManagedRuntimeContext::Aot(runtime),
            _api: api,
        })
    }

    /// Poll the collectible loader and report the outcome of any swap attempt.
    ///
    /// The managed loader validates the rebuilt assembly's component manifest
    /// and system signatures before swapping. A rejection is logged once per
    /// attempt so the per-frame poll cannot drown the terminal in messages.
    ///
    /// # Errors
    ///
    /// Returns [`CSharpError::UnknownPollStatus`] for a code outside
    /// [`POLL_NO_CHANGE`], [`POLL_RELOADED`] and [`POLL_REJECTED`]: a status the
    /// host cannot interpret is a failure to report, not a change that never
    /// happened. The currently loaded assembly is kept either way.
    #[cfg(feature = "hot_reload")]
    pub(crate) fn poll_reload(&mut self, engine: &mut Engine) -> Result<u8, CSharpError> {
        // A world that took a manifest whose assembly never loaded cannot be
        // reconciled by another poll: the mismatch is already in the world's
        // layouts. Stay stopped rather than swapping a second assembly on top.
        if self.manifest_applied_without_assembly {
            return Ok(POLL_REJECTED);
        }
        let status = (self.poll_reload)();
        if !poll_status_is_known(status) {
            // Logged once per distinct code: the poll runs every frame and a
            // persistent unknown status must not drown the terminal.
            if self.last_poll_status != status {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    status,
                    "the managed loader reported an unknown reload status; keeping the currently loaded assembly"
                );
            }
            self.last_poll_status = status;
            return Err(CSharpError::UnknownPollStatus { status });
        }
        if status == POLL_REJECTED && self.last_poll_status != POLL_REJECTED {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "C# reload rejected: component or system signatures changed; restart the host to rebuild the native component registry and scheduler"
            );
        }
        #[cfg(feature = "hot_reload")]
        if status == POLL_MANIFEST_PENDING {
            return self.decide_parked_manifest(engine);
        }
        if status == POLL_RELOADED {
            // A changed system set is rebuilt rather than refused. The
            // comparison stays because it is what keeps the ordinary
            // behaviour-only swap cheap: that swap keeps its scheduler graph
            // and pays one metadata comparison, and only a real signature
            // change pays for a clear and a re-registration.
            if !self.verify_systems_unchanged() {
                self.reregister_systems(engine)?;
            }
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "C# hot reload complete"
            );
            // The swap is done and the manifest it carried may differ.
            // Applying it here, before the frame's systems run, is what puts a
            // migrated row in place before the new generation reads one.
            self.apply_manifest_if_changed(engine);
        }
        self.last_poll_status = status;
        Ok(status)
    }

    /// Decide the parked version's fate: apply its manifest, then commit or
    /// abort.
    ///
    /// The whole point of the handshake is this ordering. The manifest is
    /// applied while the *old* assembly is still the running one, so a refusal
    /// costs an unload and nothing else; committing afterwards means the world
    /// and the incoming assembly agree the moment the swap lands. Applying
    /// after a swap would leave a running assembly against a world that never
    /// took its layout.
    ///
    /// Returns the status the caller should report: `POLL_RELOADED` when the
    /// swap happened, `POLL_REJECTED` when it did not.
    ///
    /// # Errors
    ///
    /// Whatever re-registering the arriving assembly's systems refuses, when
    /// the committed version changed them.
    #[cfg(feature = "hot_reload")]
    fn decide_parked_manifest(&mut self, engine: &mut Engine) -> Result<u8, CSharpError> {
        let Some(manifest) = self.read_pending_manifest() else {
            self.abort_parked("its component manifest could not be read");
            return Ok(POLL_REJECTED);
        };

        match apply_component_manifest_on_reload(engine, &manifest, &self.bindings) {
            Ok(report) => {
                if (self.commit_reload)() == 0 {
                    // The manifest is in force but the swap did not happen, so
                    // the running assembly now disagrees with the world. This is
                    // the one outcome the handshake cannot make safe by ordering
                    // alone, and continuing to schedule the project's systems
                    // against a world they no longer describe is the one
                    // response that cannot be right - so they are cleared and
                    // the runtime latches until the host restarts.
                    let removed = engine.clear_systems_owned_by(SystemOwner::PROJECT);
                    self.system_snapshot.clear();
                    self.manifest_applied_without_assembly = true;
                    error!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        cleared_systems = removed,
                        "the managed loader failed to install a version whose manifest was already applied; the project's systems have been stopped - restart the host"
                    );
                    return Ok(POLL_REJECTED);
                }
                // Only the commit swaps the managed system table, so this is
                // the first point at which the exports describe the arriving
                // assembly rather than the outgoing one - which is why the
                // comparison cannot sit beside the manifest check above, where
                // it could only ever compare the running set against itself.
                if !self.verify_systems_unchanged() {
                    self.reregister_systems(engine)?;
                }
                self.applied_manifest = manifest;
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    added = report.added.len(),
                    migrated = report.migrated.len(),
                    renamed = report.renamed.len(),
                    retired = report.retired.len(),
                    resources_added = report.resources_added.len(),
                    resources_migrated = report.resources_migrated.len(),
                    resources_renamed = report.resources_renamed.len(),
                    resources_retired = report.resources_retired.len(),
                    "C# hot reload complete"
                );
                Ok(POLL_RELOADED)
            }
            Err(error) => {
                self.abort_parked(&error.to_string());
                Ok(POLL_REJECTED)
            }
        }
    }

    /// Copy the parked version's manifest, or `None` when it cannot be read.
    #[cfg(feature = "hot_reload")]
    fn read_pending_manifest(&self) -> Option<Vec<u8>> {
        let length = (self.pending_manifest_length)();
        if !is_supported_manifest_length(length) {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                length,
                "the parked assembly reported a component manifest length outside the accepted range"
            );
            return None;
        }
        let mut manifest = Vec::new();
        if manifest.try_reserve_exact(length as usize).is_err() {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                length,
                "could not reserve a buffer for the parked component manifest"
            );
            return None;
        }
        manifest.resize(length as usize, 0);
        if (self.copy_pending_manifest)(manifest.as_mut_ptr(), length) == 0 {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "could not copy the parked component manifest"
            );
            return None;
        }
        Some(manifest)
    }

    /// Tell the managed loader to discard the parked version, and say why.
    #[cfg(feature = "hot_reload")]
    fn abort_parked(&mut self, reason: &str) {
        error!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            reason,
            "C# reload rejected: the component manifest could not be applied; the running assembly keeps its rows"
        );
        if (self.abort_reload)() == 0 {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "the managed loader failed to discard the parked version; restart the host"
            );
        }
    }

    /// Apply the swapped assembly's component manifest when it differs from the
    /// one the bindings were built from.
    ///
    /// The managed loader still refuses a manifest change at the swap itself
    /// (plan §4.4), so today this compares two identical payloads and returns.
    /// It is here so that relaxing that refusal becomes a managed-side change:
    /// the host already knows how to migrate what a new manifest asks for - a
    /// reshaped descriptor component is relaid out, a new one is registered, and a
    /// native mirror or a vanished component is refused with a typed error.
    #[cfg(feature = "hot_reload")]
    fn apply_manifest_if_changed(&mut self, engine: &mut Engine) {
        let length = (self.manifest_length)();
        if !is_supported_manifest_length(length) {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                length,
                "the reloaded assembly reported a component manifest length outside the accepted range; keeping the applied manifest"
            );
            return;
        }
        let mut manifest = Vec::new();
        if manifest.try_reserve_exact(length as usize).is_err() {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                length,
                "could not reserve a buffer for the reloaded component manifest; keeping the applied manifest"
            );
            return;
        }
        manifest.resize(length as usize, 0);
        if (self.copy_manifest)(manifest.as_mut_ptr(), length) == 0 {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                "could not copy the reloaded component manifest; keeping the applied manifest"
            );
            return;
        }
        if manifest == self.applied_manifest {
            return;
        }

        match apply_component_manifest_on_reload(engine, &manifest, &self.bindings) {
            Ok(report) => {
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    added = report.added.len(),
                    migrated = report.migrated.len(),
                    renamed = report.renamed.len(),
                    retired = report.retired.len(),
                    resources_added = report.resources_added.len(),
                    resources_migrated = report.resources_migrated.len(),
                    resources_renamed = report.resources_renamed.len(),
                    resources_retired = report.resources_retired.len(),
                    "applied the reloaded assembly's component and resource manifest"
                );
                self.applied_manifest = manifest;
            }
            Err(error) => error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                error = %error,
                "component manifest refused; the reloaded assembly's component definitions are not in force"
            ),
        }
    }

    /// Rebuild the scheduler's managed systems from the arriving assembly.
    ///
    /// Only the project's systems are cleared, so a module's survive untouched
    /// (the same scoping a module reload relies on). This runs between frames,
    /// at the point the swap is applied, because clearing systems while the
    /// scheduler is walking them is not safe.
    ///
    /// The snapshot is replaced only on success: a failed re-registration
    /// leaves the world with no managed systems, and the snapshot has to
    /// describe that rather than the set that is gone.
    ///
    /// # Errors
    ///
    /// Whatever [`register_managed_systems`] refuses - most often a system
    /// declaring a component or resource the arriving manifest never
    /// registered.
    #[cfg(feature = "hot_reload")]
    fn reregister_systems(&mut self, engine: &mut Engine) -> Result<(), CSharpError> {
        // Resolve everything first. A refusal here - an access naming a
        // component the arriving assembly never registered is the usual one -
        // must leave the running generation's systems in place, because there
        // is nothing to fall back to once they are cleared.
        let prepared = prepare_managed_systems(&self.bindings, self.exports)?;
        let removed = engine.clear_systems_owned_by(SystemOwner::PROJECT);
        self.system_snapshot.clear();
        self.system_snapshot =
            commit_managed_systems(engine, &self.bindings, self.exports, prepared);
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            removed,
            registered = self.system_snapshot.len(),
            "re-registered the project's managed systems after a signature change"
        );
        Ok(())
    }

    /// Re-reflect the active project assembly and report whether its system
    /// metadata still matches the snapshot captured at registration.
    ///
    /// This is no longer a gate but a fork: a match means the swap changed only
    /// behaviour, so the scheduler graph stands and the reload costs one
    /// comparison, while a mismatch sends the reload through
    /// [`CSharpRuntime::reregister_systems`] to rebuild that graph. Comparing
    /// first is worth it because the match is the common case - most reloads
    /// edit a system body, not its signature.
    #[cfg(feature = "hot_reload")]
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
                    kind: 0,
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
            // Resolved the same way registration resolves it, fallback
            // included, so an assembly that exposes no name export compares
            // equal instead of re-registering on every single poll.
            if resolved_system_name(
                self.exports.system_name_length,
                self.exports.copy_system_name,
                system_index,
            ) != *snapshot.name
            {
                return false;
            }
        }
        true
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Whether a managed-reported poll status is one this host understands.
///
/// The three codes are the loader's whole vocabulary; the set is a helper so a
/// test can pin it without a live runtime.
#[cfg(feature = "hot_reload")]
pub(super) fn poll_status_is_known(status: u8) -> bool {
    #[cfg(feature = "hot_reload")]
    {
        matches!(
            status,
            POLL_NO_CHANGE | POLL_RELOADED | POLL_REJECTED | POLL_MANIFEST_PENDING
        )
    }
    #[cfg(not(feature = "hot_reload"))]
    {
        matches!(status, POLL_NO_CHANGE | POLL_RELOADED | POLL_REJECTED)
    }
}

/// Whether a managed-reported manifest length lies within the supported range.
///
/// Rejects zero and any value above [`MAX_COMPONENT_MANIFEST_BYTES`], so a
/// buggy managed assembly can never drive an unbounded host allocation.
pub(super) fn is_supported_manifest_length(length: u32) -> bool {
    (1..=MAX_COMPONENT_MANIFEST_BYTES).contains(&length)
}

/// Validates a managed-reported system count before it sizes a host allocation.
///
/// `Vec::with_capacity` aborts the process when the allocation fails, so the
/// count is checked against [`MAX_SYSTEMS_PER_ASSEMBLY`] first and refused as
/// a typed error instead.
pub(super) fn checked_system_count(count: u32) -> Result<usize, CSharpError> {
    if count > MAX_SYSTEMS_PER_ASSEMBLY {
        return Err(CSharpError::SystemCountOutOfRange {
            count,
            limit: MAX_SYSTEMS_PER_ASSEMBLY,
        });
    }
    Ok(count as usize)
}

/// Validates a managed-reported access count; see [`checked_system_count`].
pub(super) fn checked_access_count(count: u32) -> Result<usize, CSharpError> {
    if count > MAX_ACCESSES_PER_SYSTEM {
        return Err(CSharpError::SystemCountOutOfRange {
            count,
            limit: MAX_ACCESSES_PER_SYSTEM,
        });
    }
    Ok(count as usize)
}

/// Fetch the reflected managed name for one system.
///
/// Returns `None` when the name is missing, oversized, or not valid UTF-8;
/// callers fall back to a synthetic index-based name.
/// The name a system is registered under: the reflected one, or a synthetic
/// fallback when the assembly exposes none.
///
/// Separate from [`managed_system_name`] because two callers must agree on the
/// fallback - registration records what this returns, and the reload compares
/// against it. A fallback applied in only one of them would make every poll of
/// a nameless assembly look like a rename.
fn resolved_system_name(
    name_length: SystemNameLengthFn,
    copy_name: CopySystemNameFn,
    system_index: u32,
) -> String {
    managed_system_name(name_length, copy_name, system_index)
        .unwrap_or_else(|| format!("csharp_system_{system_index}"))
}

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
        // A resource access resolves through its own table: the two key spaces
        // are both name hashes, so only the declared kind tells them apart, and
        // recording a resource as a component would let two writers of one
        // resource run in the same batch.
        if access.kind == super::resources::RESOURCE_ACCESS_KIND {
            let resource = super::resources::resolve_resource_access(
                access.component_key,
                access.component_key_high,
            )?;
            match access.mode {
                0 => result.add_resource_read(resource),
                1 => result.add_resource_write(resource),
                mode => return Err(CSharpError::UnknownAccessMode { mode }),
            }
            continue;
        }
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
