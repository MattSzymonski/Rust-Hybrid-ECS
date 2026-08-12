//! High-level C# game startup, discovery, and scheduler registration.
//!
//! # Responsibilities
//!
//! - Start .NET and discover the managed interop exports.
//! - Register reflected component manifests and startup methods.
//! - Translate reflected system accesses into scheduler registrations.

// Standard library
use std::path::Path;
use std::sync::Arc;

// External crates
use pill_engine::commands::CommandQueue;
use pill_engine::{Engine, SystemAccess, World};

// Current crate
use crate::CSharpModuleConfig;

use super::abi::{CsEngineApi, NativeSystemAccess};
use super::components::{
    register_component_manifest, shared_component_bindings, ComponentBindings, StableComponentId,
};
use super::context::ActiveSystemGuard;
use super::csharp_runtime::DotnetRuntimeContext;

// =============================================================================
// Types
// =============================================================================

/// Signature of the managed runtime entry point that receives the API table.
type InitFn = extern "system" fn(*const CsEngineApi) -> u8;
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
/// Signature returning how many accesses one system declared.
type SystemAccessCountFn = extern "system" fn(u32) -> u32;
/// Signature copying one system's reflected accesses into a caller buffer.
type GetSystemAccessFn = extern "system" fn(u32, u32, *mut NativeSystemAccess) -> u8;
/// Signature running one scheduler system by index.
type RunSystemFn = extern "system" fn(u32);
/// Signature polling the collectible loader for a new game assembly.
type PollReloadFn = extern "system" fn();

// =============================================================================
// Types + Impls
// =============================================================================

/// Owns the hosted .NET context, stable API table, and reload callback.
///
/// Keeping `_runtime` and `_api` alive guarantees that both the managed
/// runtime and every native function pointer remain valid for registered
/// scheduler closures.
pub(crate) struct CSharpRuntime {
    poll_reload: PollReloadFn,
    _runtime: DotnetRuntimeContext,
    _api: Box<CsEngineApi>,
    _bindings: Arc<ComponentBindings>,
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
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shared_bindings = shared_component_bindings(engine);

        // Step 1: Resolve assembly paths, start .NET, and load managed exports.
        let runtime_dir = workspace_root.join(config.runtime_output_subdirectory);
        let game_dir = workspace_root.join(config.game_output_subdirectory);
        let assembly = runtime_dir.join(format!("{}.dll", config.runtime_assembly_name));
        let runtime_config = runtime_dir.join(format!(
            "{}.runtimeconfig.json",
            config.runtime_assembly_name
        ));
        std::env::set_var("ECS_CSHARP_GAME_DIR", &game_dir);
        std::env::set_var(
            "ECS_CSHARP_GAME_ASSEMBLY",
            format!("{}.dll", config.game_assembly_name),
        );

        let runtime = DotnetRuntimeContext::new(&runtime_config)?;
        let type_name = format!(
            "TracyLive.Loader.LoaderInterop, {}",
            config.runtime_assembly_name
        );
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
        let poll_reload =
            runtime.get_unmanaged_fn::<PollReloadFn>(&assembly, &type_name, "PollReload")?;

        // Step 2: Initialize the runtime bridge and register the component
        // manifest copied from the managed assembly.
        let api = Box::new(CsEngineApi::new());
        if init(api.as_ref() as *const CsEngineApi) == 0 {
            return Err("csharp_runtime initialization failed".into());
        }

        let mut manifest = vec![0_u8; manifest_length() as usize];
        if manifest.is_empty() || copy_manifest(manifest.as_mut_ptr(), manifest.len() as u32) == 0 {
            return Err("failed to copy the C# component manifest".into());
        }
        let bindings = Arc::new(register_component_manifest(
            engine,
            &manifest,
            shared_bindings,
        )?);

        // Step 3: Run every reflected managed startup method.
        let startup_bindings = Arc::clone(&bindings);
        let mut startup_failed = None;
        engine
            .run_deferred_commands(|world, queue| {
                let no_accesses = [];
                for startup_index in 0..startup_count() {
                    let _guard = ActiveSystemGuard::set_with_commands(
                        world,
                        queue,
                        &no_accesses,
                        &startup_bindings,
                        true,
                    );
                    if run_startup(startup_index) == 0 {
                        startup_failed = Some(startup_index);
                        break;
                    }
                }
            })
            .map_err(|errors| format!("C# startup commands failed: {errors:?}"))?;
        if let Some(index) = startup_failed {
            return Err(format!("C# startup method {index} failed").into());
        }

        // Step 4: Reflect each system's accesses and register it with the
        // scheduler under the exact resolved read/write list.
        let count = system_count();
        if count == 0 {
            return Err("game_cs contains no [EcsSystem] methods".into());
        }
        for system_index in 0..count {
            let mut managed_access = Vec::with_capacity(access_count(system_index) as usize);
            for access_index in 0..access_count(system_index) {
                let mut item = NativeSystemAccess {
                    component_key: 0,
                    component_key_high: 0,
                    mode: 0,
                };
                if get_access(system_index, access_index, &mut item) == 0 {
                    return Err(format!(
                        "failed to get access {access_index} for C# system {system_index}"
                    )
                    .into());
                }
                managed_access.push(item);
            }

            let uses_commands = system_uses_commands(system_index) != 0;
            let mut access = derive_system_access(&managed_access, &bindings)?;
            access.set_uses_commands(uses_commands);
            let managed_access = managed_access.into_boxed_slice();
            let system_bindings = Arc::clone(&bindings);
            let name = Box::leak(format!("csharp_system_{system_index}").into_boxed_str());
            // SAFETY: `derive_system_access` has resolved every managed access
            // and the closure exposes the world only under that exact list.
            unsafe {
                engine.register_system_with_access(
                    name,
                    access,
                    move |world: &mut World, queue: &mut CommandQueue| {
                        let _guard = ActiveSystemGuard::set_with_commands(
                            world,
                            queue,
                            &managed_access,
                            &system_bindings,
                            uses_commands,
                        );
                        run_system(system_index);
                    },
                );
            }
        }

        Ok(Self {
            poll_reload,
            _runtime: runtime,
            _api: api,
            _bindings: bindings,
        })
    }

    /// Ask the collectible managed loader to reload a changed game assembly.
    pub(crate) fn poll_reload(&mut self) {
        (self.poll_reload)();
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Translate managed component modes into native scheduler metadata.
///
/// # Errors
///
/// Returns an error if a declared component key is not registered or an
/// access mode is neither read nor write.
pub(super) fn derive_system_access(
    accesses: &[NativeSystemAccess],
    bindings: &ComponentBindings,
) -> Result<SystemAccess, Box<dyn std::error::Error>> {
    let mut result = SystemAccess::new();
    for access in accesses {
        let stable_id =
            StableComponentId::from_halves(access.component_key, access.component_key_high);
        let component = bindings
            .get(&stable_id)
            .map(|binding| binding.component_id())
            .ok_or_else(|| {
                format!(
                    "C# system references unregistered component key {:016X}{:016X}",
                    access.component_key_high, access.component_key
                )
            })?;
        match access.mode {
            0 => result.add_read(component),
            1 => result.add_write(component),
            mode => return Err(format!("unknown C# access mode {mode}").into()),
        }
    }
    Ok(result)
}
