//! Scheduler-integrated C# hot-reload support.
//!
//! Managed methods marked `[EcsSystem]` declare exactly one query parameter.
//! The loader derives component reads/writes from that parameter and exposes
//! them here, where each managed method is registered as an ordinary Rust ECS
//! scheduler system.

use std::cell::Cell;
use std::path::PathBuf;
use std::process::Command;

use ecs_hybrid::{ComponentId, Engine, SystemAccess, World};

use crate::cs_components::{GravityForce, Health, Mass, Position, Velocity};
use crate::hostfxr::HostfxrContext;

// ---------------------------------------------------------------------------
// Native EngineApi handed to the stable managed loader.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct EngineApi {
    pub entity_count: extern "C" fn() -> u32,
    pub get_component_chunk: extern "C" fn(u64, u8, u32, *mut ComponentChunk) -> u8,
}

impl EngineApi {
    fn new() -> Self {
        Self {
            entity_count: ffi_entity_count,
            get_component_chunk: ffi_get_component_chunk,
        }
    }
}

#[repr(C)]
pub struct ComponentChunk {
    archetype_low: u64,
    archetype_high: u64,
    data: *mut std::ffi::c_void,
    len: u32,
    element_size: u32,
}

// `Engine::run_systems_parallel` already relies on scheduler-proven disjoint
// access to hand the same World to systems on different Rayon threads. Each
// managed proxy publishes that system's World pointer only on its own thread,
// so an EngineApi query cannot escape into a different system invocation.
thread_local! {
    static ACTIVE_WORLD: Cell<*mut World> = const { Cell::new(std::ptr::null_mut()) };
    static ACTIVE_ACCESS: Cell<(*const NativeSystemAccess, usize)> =
        const { Cell::new((std::ptr::null(), 0)) };
}

struct ActiveWorldGuard;

impl ActiveWorldGuard {
    fn set(world: &mut World, access: &[NativeSystemAccess]) -> Self {
        ACTIVE_WORLD.with(|slot| {
            assert!(slot.get().is_null(), "nested managed ECS system invocation");
            slot.set(world as *mut World);
        });
        ACTIVE_ACCESS.with(|slot| slot.set((access.as_ptr(), access.len())));
        Self
    }
}

impl Drop for ActiveWorldGuard {
    fn drop(&mut self) {
        ACTIVE_WORLD.with(|slot| slot.set(std::ptr::null_mut()));
        ACTIVE_ACCESS.with(|slot| slot.set((std::ptr::null(), 0)));
    }
}

fn access_is_authorized(key: u64, requested_mode: u8) -> Option<bool> {
    ACTIVE_ACCESS.with(|slot| {
        let (ptr, len) = slot.get();
        if ptr.is_null() {
            return None;
        }
        let accesses = unsafe { std::slice::from_raw_parts(ptr, len) };
        Some(accesses.iter().any(|access| {
            access.component_key == key
                && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}

fn with_active_world<R>(f: impl FnOnce(&mut World) -> R) -> Option<R> {
    ACTIVE_WORLD.with(|slot| {
        let ptr = slot.get();
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { f(&mut *ptr) })
        }
    })
}

const fn component_key(name: &str) -> u64 {
    let bytes = name.as_bytes();
    let mut hash = 0xcbf29ce484222325u64;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

fn component_id(key: u64) -> Option<ComponentId> {
    match key {
        k if k == component_key("Position") => Some(ComponentId::of::<Position>()),
        k if k == component_key("Velocity") => Some(ComponentId::of::<Velocity>()),
        k if k == component_key("Health") => Some(ComponentId::of::<Health>()),
        k if k == component_key("Mass") => Some(ComponentId::of::<Mass>()),
        k if k == component_key("GravityForce") => Some(ComponentId::of::<GravityForce>()),
        _ => None,
    }
}

fn get_component_chunk<T: ecs_hybrid::Component>(
    world: &mut World,
    chunk_index: u32,
    out: *mut ComponentChunk,
) -> u8 {
    let Some((archetype_id, slice)) = world.component_chunk_mut::<T>(chunk_index as usize) else {
        return 0;
    };

    let bits = archetype_id.0;
    unsafe {
        out.write(ComponentChunk {
            archetype_low: bits as u64,
            archetype_high: (bits >> 64) as u64,
            data: slice.as_mut_ptr().cast(),
            len: slice.len() as u32,
            element_size: std::mem::size_of::<T>() as u32,
        });
    }
    1
}

extern "C" fn ffi_get_component_chunk(
    key: u64,
    mode: u8,
    chunk_index: u32,
    out: *mut ComponentChunk,
) -> u8 {
    if out.is_null() {
        return 0;
    }

    match access_is_authorized(key, mode) {
        None => return 3,
        Some(false) => return 4,
        Some(true) => {}
    }

    let Some(result) = with_active_world(|world| match key {
        k if k == component_key("Position") => {
            get_component_chunk::<Position>(world, chunk_index, out)
        }
        k if k == component_key("Velocity") => {
            get_component_chunk::<Velocity>(world, chunk_index, out)
        }
        k if k == component_key("Health") => {
            get_component_chunk::<Health>(world, chunk_index, out)
        }
        k if k == component_key("Mass") => get_component_chunk::<Mass>(world, chunk_index, out),
        k if k == component_key("GravityForce") => {
            get_component_chunk::<GravityForce>(world, chunk_index, out)
        }
        _ => 2,
    }) else {
        return 3;
    };
    result
}

extern "C" fn ffi_entity_count() -> u32 {
    with_active_world(|world| world.entity_count() as u32).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Build + hostfxr bootstrap and stable loader exports.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeSystemAccess {
    component_key: u64,
    mode: u8,
}

fn derive_system_access(accesses: &[NativeSystemAccess]) -> Result<SystemAccess, String> {
    let mut result = SystemAccess::new();
    for access in accesses {
        let id = component_id(access.component_key).ok_or_else(|| {
            format!(
                "managed system references an unregistered component key {}",
                access.component_key
            )
        })?;
        match access.mode {
            0 => result.add_read(id),
            1 => result.add_write(id),
            mode => return Err(format!("unknown managed component access mode {mode}")),
        }
    }
    Ok(result)
}

type InitFn = extern "system" fn(*const EngineApi) -> u8;
type SystemCountFn = extern "system" fn() -> u32;
type SystemAccessCountFn = extern "system" fn(u32) -> u32;
type GetSystemAccessFn = extern "system" fn(u32, u32, *mut NativeSystemAccess) -> u8;
type RunSystemFn = extern "system" fn(u32);
type PollReloadFn = extern "system" fn();

struct ManagedExports {
    init: InitFn,
    system_count: SystemCountFn,
    system_access_count: SystemAccessCountFn,
    get_system_access: GetSystemAccessFn,
    run_system: RunSystemFn,
    poll_reload: PollReloadFn,
}

fn workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn build_dotnet_project(name: &str) -> Result<(), String> {
    let project = workspace_dir().join("examples").join(name);
    let status = Command::new("dotnet")
        .args(["build", "-c", "Release", "--nologo"])
        .arg(&project)
        .status()
        .map_err(|e| format!("failed to run `dotnet build {name}`: {e}"))?;
    if !status.success() {
        return Err(format!("`dotnet build {name} -c Release` failed"));
    }
    Ok(())
}

fn assembly_dir(name: &str) -> PathBuf {
    workspace_dir()
        .join("examples")
        .join(name)
        .join("bin")
        .join("Release")
        .join("net8.0")
}

fn load_managed() -> Result<(HostfxrContext, ManagedExports), Box<dyn std::error::Error>> {
    build_dotnet_project("tracy_live_game_cs_loader").map_err(Box::<dyn std::error::Error>::from)?;
    build_dotnet_project("tracy_live_game_cs").map_err(Box::<dyn std::error::Error>::from)?;

    let loader_dir = assembly_dir("tracy_live_game_cs_loader");
    let assembly = loader_dir.join("tracy_live_game_cs_loader.dll");
    let runtime_config = loader_dir.join("tracy_live_game_cs_loader.runtimeconfig.json");
    std::env::set_var("TRACY_LIVE_MANAGED_DIR", assembly_dir("tracy_live_game_cs"));

    let hostfxr = HostfxrContext::new(&runtime_config)?;
    let type_name = "TracyLive.Loader.LoaderInterop, tracy_live_game_cs_loader";
    let exports = ManagedExports {
        init: hostfxr.get_unmanaged_fn::<InitFn>(&assembly, type_name, "Init")?,
        system_count: hostfxr
            .get_unmanaged_fn::<SystemCountFn>(&assembly, type_name, "SystemCount")?,
        system_access_count: hostfxr.get_unmanaged_fn::<SystemAccessCountFn>(
            &assembly,
            type_name,
            "SystemAccessCount",
        )?,
        get_system_access: hostfxr.get_unmanaged_fn::<GetSystemAccessFn>(
            &assembly,
            type_name,
            "GetSystemAccess",
        )?,
        run_system: hostfxr
            .get_unmanaged_fn::<RunSystemFn>(&assembly, type_name, "RunSystem")?,
        poll_reload: hostfxr
            .get_unmanaged_fn::<PollReloadFn>(&assembly, type_name, "PollReload")?,
    };
    Ok((hostfxr, exports))
}

/// Keeps the CLR and API table alive and performs behavior-only hot reloads.
pub struct CsGame {
    poll_reload: PollReloadFn,
    _hostfxr: HostfxrContext,
    _api: Box<EngineApi>,
}

impl CsGame {
    /// Poll before `Engine::process_frame`, while no scheduled system runs.
    pub fn poll_reload(&mut self) {
        (self.poll_reload)();
    }
}

/// Loads managed systems, derives their access patterns from query parameters,
/// and registers scheduler proxies in the Rust engine.
pub fn start(engine: &mut Engine) -> Result<CsGame, String> {
    let (hostfxr, exports) = load_managed().map_err(|e| e.to_string())?;
    let api = Box::new(EngineApi::new());
    if (exports.init)(api.as_ref() as *const EngineApi) == 0 {
        return Err("managed loader initialization failed".into());
    }

    let system_count = (exports.system_count)();
    if system_count == 0 {
        return Err("managed assembly exported no ECS systems".into());
    }

    for system_index in 0..system_count {
        let access_count = (exports.system_access_count)(system_index);
        let mut managed_access = Vec::with_capacity(access_count as usize);
        for access_index in 0..access_count {
            let mut native = NativeSystemAccess {
                component_key: 0,
                mode: 0,
            };
            if (exports.get_system_access)(system_index, access_index, &mut native) == 0 {
                return Err(format!(
                    "failed to read access {access_index} for managed system {system_index}"
                ));
            }
            managed_access.push(native);
        }

        let access = derive_system_access(&managed_access)
            .map_err(|error| format!("managed system {system_index}: {error}"))?;

        let run_system = exports.run_system;
        let managed_access = managed_access.into_boxed_slice();
        let name: &'static str =
            Box::leak(format!("csharp_system_{system_index}").into_boxed_str());
        unsafe {
            engine.register_system_with_access(
                name,
                access,
                move |world: &mut World, _queue: &mut ecs_hybrid::commands::CommandQueue| {
                    let _active_world = ActiveWorldGuard::set(world, &managed_access);
                    run_system(system_index);
                },
            );
        }
    }

    Ok(CsGame {
        poll_reload: exports.poll_reload,
        _hostfxr: hostfxr,
        _api: api,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native<T>(mode: u8) -> NativeSystemAccess
    where
        T: ecs_hybrid::Component,
    {
        let name = std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .unwrap();
        NativeSystemAccess {
            component_key: component_key(name),
            mode,
        }
    }

    #[test]
    fn managed_write_query_derives_one_write() {
        let access = derive_system_access(&[native::<Position>(1)]).unwrap();
        assert!(access.reads.is_empty());
        assert_eq!(access.writes, [ComponentId::of::<Position>()].into());
    }

    #[test]
    fn managed_write_read_query_derives_both_accesses() {
        let access = derive_system_access(&[
            native::<Position>(1),
            native::<Velocity>(0),
        ])
        .unwrap();
        assert_eq!(access.reads, [ComponentId::of::<Velocity>()].into());
        assert_eq!(access.writes, [ComponentId::of::<Position>()].into());
    }

    #[test]
    fn managed_accesses_drive_scheduler_conflicts() {
        let movement = derive_system_access(&[
            native::<Position>(1),
            native::<Velocity>(0),
        ])
        .unwrap();
        let position_reader = derive_system_access(&[native::<Position>(0)]).unwrap();
        let health_writer = derive_system_access(&[native::<Health>(1)]).unwrap();

        assert!(movement.conflicts_with(&position_reader));
        assert!(position_reader.conflicts_with(&movement));
        assert!(!movement.conflicts_with(&health_writer));
    }

    #[test]
    fn managed_access_rejects_unknown_component_and_mode() {
        assert!(derive_system_access(&[NativeSystemAccess {
            component_key: component_key("NotRegistered"),
            mode: 0,
        }])
        .is_err());
        assert!(derive_system_access(&[native::<Position>(9)]).is_err());
    }
}
