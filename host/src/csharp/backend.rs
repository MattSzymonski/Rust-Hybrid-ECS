//! High-level C# game startup, discovery, and scheduler registration.

use std::path::Path;
use std::sync::Arc;

use pill_engine::commands::CommandQueue;
use pill_engine::{Engine, SystemAccess, World};

use crate::CSharpModuleConfig;

use super::abi::{CsEngineApi, NativeSystemAccess};
use super::components::{
    register_component_manifest, shared_component_bindings, ComponentBindings, StableComponentId,
};
use super::context::ActiveSystemGuard;
use super::csharp_runtime::DotnetRuntimeContext;

type InitFn = extern "system" fn(*const CsEngineApi) -> u8;
type SystemCountFn = extern "system" fn() -> u32;
type StartupCountFn = extern "system" fn() -> u32;
type SystemUsesCommandsFn = extern "system" fn(u32) -> u8;
type RunStartupFn = extern "system" fn(u32) -> u8;
type ComponentManifestLengthFn = extern "system" fn() -> u32;
type CopyComponentManifestFn = extern "system" fn(*mut u8, u32) -> u8;
type SystemAccessCountFn = extern "system" fn(u32) -> u32;
type GetSystemAccessFn = extern "system" fn(u32, u32, *mut NativeSystemAccess) -> u8;
type RunSystemFn = extern "system" fn(u32);
type PollReloadFn = extern "system" fn();

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
    pub(crate) fn start(
        engine: &mut Engine,
        workspace_root: &Path,
        config: &CSharpModuleConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let shared_bindings = shared_component_bindings(engine);

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

/// Translate managed component modes into native scheduler metadata.
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
