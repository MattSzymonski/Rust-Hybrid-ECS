//! Rust side of the scheduler-aware C# runtime bridge.

use std::cell::Cell;
use std::path::Path;

use ecs_hybrid::{Component, ComponentId, Engine, SystemAccess, World};
#[cfg(feature = "rendering")]
use ecs_hybrid::{Color, Position, Sprite};
use trait_type_map::{impl_trait_accessible, TraitAccessible};

use crate::CSharpModuleConfig;

use super::cs_runtime::DotnetRuntimeContext;

#[cfg(not(feature = "rendering"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct Position {
    x: f32,
    y: f32,
}
#[cfg(not(feature = "rendering"))]
impl Component for Position {}

#[repr(C)]
#[derive(Clone, Copy)]
struct Velocity {
    x: f32,
    y: f32,
}
impl Component for Velocity {}

#[repr(C)]
#[derive(Clone, Copy)]
struct Health {
    value: f32,
}
impl Component for Health {}

#[repr(C)]
#[derive(Clone, Copy)]
struct Mass {
    value: f32,
}
impl Component for Mass {}

#[repr(C)]
#[derive(Clone, Copy)]
struct GravityForce {
    x: f32,
    y: f32,
}
impl Component for GravityForce {}

#[cfg(not(feature = "rendering"))]
impl_trait_accessible!(dyn Component; Position, Velocity, Health, Mass, GravityForce);
#[cfg(feature = "rendering")]
impl_trait_accessible!(dyn Component; Velocity, Health, Mass, GravityForce);

#[repr(C)]
struct CsEngineApi {
    entity_count: extern "C" fn() -> u32,
    get_component_chunk: extern "C" fn(u64, u8, u32, *mut ComponentChunk) -> u8,
}

impl CsEngineApi {
    fn new() -> Self {
        Self {
            entity_count: ffi_entity_count,
            get_component_chunk: ffi_get_component_chunk,
        }
    }
}

#[repr(C)]
struct ComponentChunk {
    archetype_low: u64,
    archetype_high: u64,
    data: *mut std::ffi::c_void,
    len: u32,
    element_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NativeSystemAccess {
    component_key: u64,
    mode: u8,
}

thread_local! {
    static ACTIVE_WORLD: Cell<*mut World> = const { Cell::new(std::ptr::null_mut()) };
    static ACTIVE_ACCESS: Cell<(*const NativeSystemAccess, usize)> =
        const { Cell::new((std::ptr::null(), 0)) };
}

struct ActiveSystemGuard;

impl ActiveSystemGuard {
    fn set(world: &mut World, access: &[NativeSystemAccess]) -> Self {
        ACTIVE_WORLD.with(|slot| {
            assert!(slot.get().is_null(), "nested managed ECS system invocation");
            slot.set(world as *mut World);
        });
        ACTIVE_ACCESS.with(|slot| slot.set((access.as_ptr(), access.len())));
        Self
    }
}

impl Drop for ActiveSystemGuard {
    fn drop(&mut self) {
        ACTIVE_ACCESS.with(|slot| slot.set((std::ptr::null(), 0)));
        ACTIVE_WORLD.with(|slot| slot.set(std::ptr::null_mut()));
    }
}

fn with_active_world<R>(f: impl FnOnce(&mut World) -> R) -> Option<R> {
    ACTIVE_WORLD.with(|slot| {
        let pointer = slot.get();
        (!pointer.is_null()).then(|| unsafe { f(&mut *pointer) })
    })
}

fn access_is_authorized(key: u64, requested_mode: u8) -> Option<bool> {
    ACTIVE_ACCESS.with(|slot| {
        let (pointer, len) = slot.get();
        if pointer.is_null() {
            return None;
        }
        let accesses = unsafe { std::slice::from_raw_parts(pointer, len) };
        Some(accesses.iter().any(|access| {
            access.component_key == key && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}

const fn component_key(name: &str) -> u64 {
    let bytes = name.as_bytes();
    let mut hash = 0xcbf29ce484222325u64;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += 1;
    }
    hash
}

fn component_id(key: u64) -> Option<ComponentId> {
    match key {
        value if value == component_key("Position") => Some(ComponentId::of::<Position>()),
        value if value == component_key("Velocity") => Some(ComponentId::of::<Velocity>()),
        value if value == component_key("Health") => Some(ComponentId::of::<Health>()),
        value if value == component_key("Mass") => Some(ComponentId::of::<Mass>()),
        value if value == component_key("GravityForce") => Some(ComponentId::of::<GravityForce>()),
        _ => None,
    }
}

fn get_component_chunk<T: Component + TraitAccessible<dyn Component>>(
    world: &mut World,
    chunk_index: u32,
    output: *mut ComponentChunk,
) -> u8 {
    let Some((archetype, slice)) = world.component_chunk_mut::<T>(chunk_index as usize) else {
        return 0;
    };
    let bits = archetype.0;
    unsafe {
        output.write(ComponentChunk {
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
    output: *mut ComponentChunk,
) -> u8 {
    if output.is_null() {
        return 0;
    }
    match access_is_authorized(key, mode) {
        None => return 3,
        Some(false) => return 4,
        Some(true) => {}
    }

    with_active_world(|world| match key {
        value if value == component_key("Position") => {
            get_component_chunk::<Position>(world, chunk_index, output)
        }
        value if value == component_key("Velocity") => {
            get_component_chunk::<Velocity>(world, chunk_index, output)
        }
        value if value == component_key("Health") => {
            get_component_chunk::<Health>(world, chunk_index, output)
        }
        value if value == component_key("Mass") => {
            get_component_chunk::<Mass>(world, chunk_index, output)
        }
        value if value == component_key("GravityForce") => {
            get_component_chunk::<GravityForce>(world, chunk_index, output)
        }
        _ => 2,
    })
    .unwrap_or(3)
}

extern "C" fn ffi_entity_count() -> u32 {
    with_active_world(|world| world.entity_count() as u32).unwrap_or(0)
}

type InitFn = extern "system" fn(*const CsEngineApi) -> u8;
type SystemCountFn = extern "system" fn() -> u32;
type SystemAccessCountFn = extern "system" fn(u32) -> u32;
type GetSystemAccessFn = extern "system" fn(u32, u32, *mut NativeSystemAccess) -> u8;
type RunSystemFn = extern "system" fn(u32);
type PollReloadFn = extern "system" fn();

pub struct CSharpRuntime {
    poll_reload: PollReloadFn,
    _runtime: DotnetRuntimeContext,
    _api: Box<CsEngineApi>,
}

impl CSharpRuntime {
    pub fn start(
        engine: &mut Engine,
        workspace_root: &Path,
        config: &CSharpModuleConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        setup_world(engine);

        let runtime_dir = workspace_root.join(config.runtime_output_subdirectory);
        let game_dir = workspace_root.join(config.game_output_subdirectory);
        let assembly = runtime_dir.join(format!("{}.dll", config.runtime_assembly_name));
        let runtime_config = runtime_dir.join(format!(
            "{}.runtimeconfig.json",
            config.runtime_assembly_name
        ));
        std::env::set_var("ECS_CS_GAME_DIR", &game_dir);
        std::env::set_var(
            "ECS_CS_GAME_ASSEMBLY",
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
            return Err("cs_runtime initialization failed".into());
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

            let access = derive_system_access(&managed_access)?;
            let managed_access = managed_access.into_boxed_slice();
            let name = Box::leak(format!("csharp_system_{system_index}").into_boxed_str());
            unsafe {
                engine.register_system_with_access(
                    name,
                    access,
                    move |world: &mut World, _queue: &mut ecs_hybrid::commands::CommandQueue| {
                        let _guard = ActiveSystemGuard::set(world, &managed_access);
                        run_system(system_index);
                    },
                );
            }
        }

        Ok(Self {
            poll_reload,
            _runtime: runtime,
            _api: api,
        })
    }

    pub fn poll_reload(&mut self) {
        (self.poll_reload)();
    }
}

fn derive_system_access(
    accesses: &[NativeSystemAccess],
) -> Result<SystemAccess, Box<dyn std::error::Error>> {
    let mut result = SystemAccess::new();
    for access in accesses {
        let component = component_id(access.component_key).ok_or_else(|| {
            format!(
                "C# system references unregistered component key {}",
                access.component_key
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

fn setup_world(engine: &mut Engine) {
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    engine.world_mut().register_component::<Mass>();
    engine.world_mut().register_component::<GravityForce>();
    #[cfg(feature = "rendering")]
    engine.world_mut().register_component::<Sprite>();
    engine.world_mut().reserve_entities(30_000);

    let mut state = 0x1234_5678_9abc_def0u64;
    let mut random = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (state >> 32) as f32 / u32::MAX as f32
    };
    for _ in 0..30_000 {
        let entity = engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: (random() - 0.5) * 1000.0,
                y: (random() - 0.5) * 1000.0,
            })
            .with(Velocity {
                x: (random() - 0.5) * 0.2,
                y: (random() - 0.5) * 0.2,
            })
            .with(Health { value: 100.0 })
            .with(Mass {
                value: 1.0 + random() * 9.0,
            })
            .with(GravityForce { x: 0.0, y: 0.0 });
        #[cfg(feature = "rendering")]
        let entity = entity.with(Sprite {
            width: 3.0,
            height: 3.0,
            color: Color::new(0.2, 0.7, 1.0, 1.0),
        });
        entity.build().expect("C# demo entity should build");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "rendering")]
    #[test]
    fn csharp_world_supports_the_sprite_renderer_query() {
        let mut engine = Engine::new();
        setup_world(&mut engine);

        let mut query = ecs_hybrid::Query::<(&Position, &Sprite)>::new(engine.world_mut());
        assert_eq!(query.iter_mut().count(), 30_000);
    }

    #[test]
    fn managed_accesses_map_to_scheduler_conflicts() {
        let movement = derive_system_access(&[
            NativeSystemAccess {
                component_key: component_key("Position"),
                mode: 1,
            },
            NativeSystemAccess {
                component_key: component_key("Velocity"),
                mode: 0,
            },
        ])
        .unwrap();
        let position_reader = derive_system_access(&[NativeSystemAccess {
            component_key: component_key("Position"),
            mode: 0,
        }])
        .unwrap();
        let health_writer = derive_system_access(&[NativeSystemAccess {
            component_key: component_key("Health"),
            mode: 1,
        }])
        .unwrap();

        assert!(movement.conflicts_with(&position_reader));
        assert!(!movement.conflicts_with(&health_writer));
    }
}
