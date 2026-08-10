//! Native ECS adapter for scheduler-managed C# systems.
//!
//! # Responsibilities
//!
//! - Defines ABI-compatible mirrors of components used by the C# game.
//! - Gives managed queries temporary access to native archetype columns.
//! - Validates every managed read/write against scheduler-declared access.
//! - Registers reflected managed methods as ordinary Rust ECS systems.
//!
//! # Design
//!
//! Managed code never owns ECS storage. During a scheduled system call,
//! [`ActiveSystemGuard`] publishes the current [`World`] and the system's
//! declared access list through thread-local slots. Native callbacks reject
//! requests made outside that scope or requests absent from the declaration.
//! Component keys are stable FNV-1a hashes of the shared short type names.

// Standard library
use std::cell::Cell;
use std::path::Path;

// Current workspace crates
#[cfg(feature = "rendering")]
use ecs_hybrid::{Color, Position, Sprite};
use ecs_hybrid::{Component, ComponentId, ComponentTicks, Engine, SystemAccess, World};
use trait_type_map::{impl_trait_accessible, TraitAccessible};

// Current crate
use crate::CSharpModuleConfig;

use super::cs_runtime::DotnetRuntimeContext;

// =============================================================================
// ABI Component Mirrors
// =============================================================================

// In rendering builds these names resolve to the renderer's components so
// managed physics writes directly into the columns consumed by the renderer.
// Headless builds provide layout-identical local definitions instead.

#[cfg(not(feature = "rendering"))]
/// Headless ABI mirror of `TracyLive.Position`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Position {
    x: f32,
    y: f32,
}
#[cfg(not(feature = "rendering"))]
impl Component for Position {}

/// ABI mirror of `TracyLive.PhysicsState`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PhysicsState {
    delta_time: f32,
    position_x: f32,
    position_y: f32,
    velocity_x: f32,
    velocity_y: f32,
    radius: f32,
    active: u8,
}
impl Component for PhysicsState {}

#[cfg(not(feature = "rendering"))]
/// Headless ABI mirror of the renderer's RGBA color.
#[repr(C)]
#[derive(Clone, Copy)]
struct Color {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[cfg(not(feature = "rendering"))]
/// Headless ABI mirror of `TracyLive.Sprite`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Sprite {
    width: f32,
    height: f32,
    color: Color,
}
#[cfg(not(feature = "rendering"))]
impl Component for Sprite {}

#[cfg(not(feature = "rendering"))]
impl_trait_accessible!(dyn Component; Position, PhysicsState, Sprite);
#[cfg(feature = "rendering")]
impl_trait_accessible!(dyn Component; PhysicsState);

// =============================================================================
// Native API Layout
// =============================================================================

/// Function table copied by `cs_runtime` during managed initialization.
///
/// Field order and calling conventions must match `EngineApi.cs` exactly.
#[repr(C)]
struct CsEngineApi {
    entity_count: extern "C" fn() -> u32,
    get_component_chunk: extern "C" fn(u64, u8, u32, *mut ComponentChunk) -> u8,
    get_entity_chunk: extern "C" fn(u32, *mut ComponentChunk) -> u8,
}

impl CsEngineApi {
    fn new() -> Self {
        Self {
            entity_count: ffi_entity_count,
            get_component_chunk: ffi_get_component_chunk,
            get_entity_chunk: ffi_get_entity_chunk,
        }
    }
}

/// Description of one contiguous component column returned to managed code.
#[repr(C)]
struct ComponentChunk {
    archetype_low: u64,
    archetype_high: u64,
    data: *mut std::ffi::c_void,
    len: u32,
    element_size: u32,
    ticks: *mut ComponentTicks,
    change_tick: u32,
}

/// One reflected scheduler access, where `0` is read and `1` is write.
#[repr(C)]
#[derive(Clone, Copy)]
struct NativeSystemAccess {
    component_key: u64,
    mode: u8,
}

// =============================================================================
// Scheduled Invocation Scope
// =============================================================================

thread_local! {
    /// World available only while the current thread executes a C# system.
    static ACTIVE_WORLD: Cell<*mut World> = const { Cell::new(std::ptr::null_mut()) };
    /// Access declaration belonging to that active C# system.
    static ACTIVE_ACCESS: Cell<(*const NativeSystemAccess, usize)> =
        const { Cell::new((std::ptr::null(), 0)) };
}

/// Clears thread-local native access even if managed execution unwinds.
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
        // SAFETY: ActiveSystemGuard installs this pointer immediately before
        // managed invocation and clears it before the borrowed world expires.
        (!pointer.is_null()).then(|| unsafe { f(&mut *pointer) })
    })
}

/// Check whether the active system declared the requested component mode.
///
/// A write declaration also permits reads; a read declaration never permits
/// writes. `None` means no managed system is currently active on this thread.
fn access_is_authorized(key: u64, requested_mode: u8) -> Option<bool> {
    ACTIVE_ACCESS.with(|slot| {
        let (pointer, len) = slot.get();
        if pointer.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard stores a slice owned by the registered
        // system closure and clears the slot before that invocation returns.
        let accesses = unsafe { std::slice::from_raw_parts(pointer, len) };
        Some(accesses.iter().any(|access| {
            access.component_key == key && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}

// =============================================================================
// Component Registry and Native Query Callbacks
// =============================================================================

/// Produce the same stable FNV-1a key as `Engine.ComponentKey` in C#.
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

/// Resolve a managed component key into the native scheduler identifier.
fn component_id(key: u64) -> Option<ComponentId> {
    match key {
        value if value == component_key("PhysicsState") => Some(ComponentId::of::<PhysicsState>()),
        value if value == component_key("Position") => Some(ComponentId::of::<Position>()),
        value if value == component_key("Sprite") => Some(ComponentId::of::<Sprite>()),
        _ => None,
    }
}

/// Return the `chunk_index`th archetype column containing `T`.
fn get_component_chunk<T: Component + TraitAccessible<dyn Component>>(
    world: &mut World,
    chunk_index: u32,
    output: *mut ComponentChunk,
) -> u8 {
    let change_tick = world.change_tick().get();
    let Some((archetype, slice, ticks)) =
        world.component_chunk_with_ticks_mut::<T>(chunk_index as usize)
    else {
        return 0;
    };
    let bits = archetype.0;
    // SAFETY: `output` was checked by the FFI entry point and the slice stays
    // alive for the duration of the active scheduled system invocation.
    unsafe {
        output.write(ComponentChunk {
            archetype_low: bits as u64,
            archetype_high: (bits >> 64) as u64,
            data: slice.as_mut_ptr().cast(),
            len: slice.len() as u32,
            element_size: std::mem::size_of::<T>() as u32,
            ticks: ticks.as_mut_ptr(),
            change_tick,
        });
    }
    1
}

/// C ABI entry point used by managed query enumerators.
///
/// Status codes are interpreted by `Engine.TryGetChunk`: `0` ends iteration,
/// `1` returns a chunk, `2` is unknown component, `3` is out-of-scope access,
/// and `4` is an undeclared access mode.
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
        value if value == component_key("PhysicsState") => {
            get_component_chunk::<PhysicsState>(world, chunk_index, output)
        }
        value if value == component_key("Position") => {
            get_component_chunk::<Position>(world, chunk_index, output)
        }
        value if value == component_key("Sprite") => {
            get_component_chunk::<Sprite>(world, chunk_index, output)
        }
        _ => 2,
    })
    .unwrap_or(3)
}

/// C ABI entry point returning the `chunk_index`th archetype entity column.
extern "C" fn ffi_get_entity_chunk(chunk_index: u32, output: *mut ComponentChunk) -> u8 {
    if output.is_null() {
        return 0;
    }
    with_active_world(|world| {
        let Some((archetype, entities)) = world.entity_chunk(chunk_index as usize) else {
            return 0;
        };
        let bits = archetype.0;
        // SAFETY: `output` was checked above and the entity slice remains
        // borrowed only for the active managed system invocation.
        unsafe {
            output.write(ComponentChunk {
                archetype_low: bits as u64,
                archetype_high: (bits >> 64) as u64,
                data: entities.as_ptr().cast_mut().cast(),
                len: entities.len() as u32,
                element_size: std::mem::size_of::<ecs_hybrid::Entity>() as u32,
                ticks: std::ptr::null_mut(),
                change_tick: world.change_tick().get(),
            });
        }
        1
    })
    .unwrap_or(3)
}

/// Return the current entity count while a managed system is active.
extern "C" fn ffi_entity_count() -> u32 {
    with_active_world(|world| world.entity_count() as u32).unwrap_or(0)
}

// =============================================================================
// Managed Export Signatures
// =============================================================================

type InitFn = extern "system" fn(*const CsEngineApi) -> u8;
type SystemCountFn = extern "system" fn() -> u32;
type SystemAccessCountFn = extern "system" fn(u32) -> u32;
type GetSystemAccessFn = extern "system" fn(u32, u32, *mut NativeSystemAccess) -> u8;
type RunSystemFn = extern "system" fn(u32);
type PollReloadFn = extern "system" fn();

// =============================================================================
// CSharpRuntime
// =============================================================================

/// Owns the hosted .NET context, stable API table, and reload callback.
///
/// Keeping `_runtime` and `_api` alive guarantees that both the managed
/// runtime and every native function pointer remain valid for registered
/// scheduler closures.
pub struct CSharpRuntime {
    poll_reload: PollReloadFn,
    _runtime: DotnetRuntimeContext,
    _api: Box<CsEngineApi>,
}

impl CSharpRuntime {
    /// Start .NET, load `cs_runtime`, discover managed systems, and register
    /// each system with its reflected read/write access declaration.
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
            // SAFETY: `derive_system_access` has resolved every managed access
            // and the closure exposes the world only under that exact list.
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

    /// Ask the collectible managed loader to reload a changed game assembly.
    pub fn poll_reload(&mut self) {
        (self.poll_reload)();
    }
}

/// Translate managed component modes into native scheduler metadata.
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

// =============================================================================
// Demo World Initialization
// =============================================================================

/// Register ABI-visible components and create the C# bouncing-ball demo.
fn setup_world(engine: &mut Engine) {
    const BALL_COUNT: usize = 100;
    const FIXED_DELTA_TIME: f32 = 1.0 / 60.0;
    const BOUNCE_VELOCITY_Y: f32 = -500.0;
    const BOUNCE_VELOCITY_X: f32 = 150.0;

    engine.world_mut().register_component::<PhysicsState>();
    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Sprite>();
    engine.world_mut().reserve_entities(BALL_COUNT);

    for index in 0..BALL_COUNT {
        let column = (index % 10) as f32;
        let row = (index / 10) as f32;
        let radius = 10.0 + (index % 4) as f32 * 2.0;
        let position_x = 60.0 + column * 72.0;
        let position_y = 60.0 + row * 42.0;
        engine
            .world_mut()
            .create_entity()
            .with(PhysicsState {
                delta_time: FIXED_DELTA_TIME,
                position_x,
                position_y,
                velocity_x: if index % 2 == 0 {
                    BOUNCE_VELOCITY_X + row * 8.0
                } else {
                    -BOUNCE_VELOCITY_X - row * 8.0
                },
                velocity_y: BOUNCE_VELOCITY_Y + column * 18.0,
                radius,
                active: 1,
            })
            .with(Position {
                x: position_x - radius,
                y: position_y - radius,
            })
            .with(Sprite {
                width: radius * 2.0,
                height: radius * 2.0,
                color: Color {
                    r: 1.0,
                    g: 0.3,
                    b: 0.3,
                    a: 1.0,
                },
            })
            .build()
            .expect("C# ball entity should build");
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ecs_hybrid::SystemScheduler;

    fn managed_access(entries: &[(&str, u8)]) -> SystemAccess {
        let native: Vec<_> = entries
            .iter()
            .map(|(name, mode)| NativeSystemAccess {
                component_key: component_key(name),
                mode: *mode,
            })
            .collect();
        derive_system_access(&native).expect("managed access should map to native components")
    }

    fn scheduler_for(accesses: impl IntoIterator<Item = SystemAccess>) -> SystemScheduler {
        let mut scheduler = SystemScheduler::new();
        for access in accesses {
            scheduler.register_system(access);
        }
        scheduler.build_execution_graph();
        scheduler
    }

    fn assert_same_batch(scheduler: &SystemScheduler, systems: &[usize]) {
        assert!(scheduler
            .execution_graph()
            .iter()
            .any(|batch| systems.iter().all(|system| batch.contains(system))));
    }

    fn assert_different_batches(scheduler: &SystemScheduler, first: usize, second: usize) {
        assert!(!scheduler
            .execution_graph()
            .iter()
            .any(|batch| batch.contains(&first) && batch.contains(&second)));
    }

    fn empty_chunk() -> ComponentChunk {
        ComponentChunk {
            archetype_low: 0,
            archetype_high: 0,
            data: std::ptr::null_mut(),
            len: 0,
            element_size: 0,
            ticks: std::ptr::null_mut(),
            change_tick: 0,
        }
    }

    unsafe fn simulate_managed_write(chunk: &ComponentChunk, row: usize) {
        assert!(row < chunk.len as usize);
        assert!(!chunk.ticks.is_null());
        // SAFETY: the chunk callback returns a tick slice parallel to the
        // component data and `row` was checked against that shared length.
        unsafe {
            (*chunk.ticks.add(row)).set_changed(ecs_hybrid::Tick::new(chunk.change_tick));
        }
    }

    #[test]
    fn component_chunk_change_tracking_abi_layout_is_stable() {
        assert_eq!(std::mem::size_of::<ComponentTicks>(), 8);
        assert_eq!(std::mem::offset_of!(ComponentTicks, changed), 4);
        assert_eq!(std::mem::size_of::<ComponentChunk>(), 48);
        assert_eq!(std::mem::offset_of!(ComponentChunk, ticks), 32);
        assert_eq!(std::mem::offset_of!(ComponentChunk, change_tick), 40);
    }

    #[cfg(feature = "rendering")]
    #[test]
    fn csharp_world_supports_the_sprite_renderer_query() {
        let mut engine = Engine::new();
        setup_world(&mut engine);

        let mut query = ecs_hybrid::Query::<(&Position, &Sprite)>::new(engine.world_mut());
        assert_eq!(query.iter_mut().count(), 100);
    }

    #[test]
    fn disjoint_managed_writers_share_a_parallel_batch() {
        let scheduler = scheduler_for([
            managed_access(&[("PhysicsState", 1)]),
            managed_access(&[("Position", 1)]),
            managed_access(&[("Sprite", 1)]),
        ]);

        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_same_batch(&scheduler, &[0, 1, 2]);
    }

    #[test]
    fn managed_readers_of_the_same_component_share_a_parallel_batch() {
        let scheduler = scheduler_for([
            managed_access(&[("Position", 0)]),
            managed_access(&[("Position", 0)]),
        ]);

        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_same_batch(&scheduler, &[0, 1]);
    }

    #[test]
    fn managed_reader_and_writer_are_scheduled_in_different_batches() {
        let scheduler = scheduler_for([
            managed_access(&[("Position", 0)]),
            managed_access(&[("Position", 1)]),
        ]);

        assert_eq!(scheduler.execution_graph().len(), 2);
        assert_different_batches(&scheduler, 0, 1);
    }

    #[test]
    fn managed_writers_of_the_same_component_are_scheduled_in_different_batches() {
        let scheduler = scheduler_for([
            managed_access(&[("Sprite", 1)]),
            managed_access(&[("Sprite", 1)]),
        ]);

        assert_eq!(scheduler.execution_graph().len(), 2);
        assert_different_batches(&scheduler, 0, 1);
    }

    #[test]
    fn entity_only_managed_system_does_not_create_a_scheduler_conflict() {
        // EntityTerm is intentionally omitted from the native component
        // access list exported by IQueryDescriptor.
        let scheduler = scheduler_for([
            managed_access(&[]),
            managed_access(&[("PhysicsState", 1), ("Position", 1), ("Sprite", 1)]),
        ]);

        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_same_batch(&scheduler, &[0, 1]);
    }

    #[test]
    fn optional_managed_access_conflicts_when_the_component_may_be_present() {
        // OptionalWrite<Sprite> exports the same scheduler write as Write<Sprite>;
        // optionality affects matching, never parallel safety.
        let scheduler = scheduler_for([
            managed_access(&[("PhysicsState", 1), ("Sprite", 0)]),
            managed_access(&[("Position", 1)]),
            managed_access(&[("Sprite", 1)]),
        ]);

        assert_eq!(scheduler.execution_graph().len(), 2);
        assert_same_batch(&scheduler, &[0, 1]);
        assert_different_batches(&scheduler, 0, 2);
        assert!(!scheduler
            .get_access(1)
            .unwrap()
            .conflicts_with(scheduler.get_access(2).unwrap()));
    }

    #[test]
    fn one_managed_row_write_is_visible_to_rust_changed_filter() {
        let mut engine = Engine::new();
        setup_world(&mut engine);
        let baseline = engine.world().change_tick();
        engine.world_mut().set_system_last_run(baseline);
        engine.world_mut().increment_change_tick();

        let accesses = [NativeSystemAccess {
            component_key: component_key("Position"),
            mode: 1,
        }];
        let mut component_chunk = empty_chunk();
        let mut entity_chunk = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses);
            assert_eq!(
                ffi_get_component_chunk(component_key("Position"), 1, 0, &mut component_chunk,),
                1
            );
            assert_eq!(ffi_get_entity_chunk(0, &mut entity_chunk), 1);
            unsafe { simulate_managed_write(&component_chunk, 37) };
        }

        let expected = unsafe { *((entity_chunk.data as *const ecs_hybrid::Entity).add(37)) };
        let mut changed =
            ecs_hybrid::Query::<(ecs_hybrid::Entity,), ecs_hybrid::Changed<Position>>::new(
                engine.world_mut(),
            );
        let hits: Vec<_> = changed.iter_mut().map(|(entity,)| entity).collect();
        assert_eq!(hits, vec![expected]);
    }

    #[test]
    fn managed_read_only_chunk_does_not_trigger_changed_filter() {
        let mut engine = Engine::new();
        setup_world(&mut engine);
        let baseline = engine.world().change_tick();
        engine.world_mut().set_system_last_run(baseline);
        engine.world_mut().increment_change_tick();

        let accesses = [NativeSystemAccess {
            component_key: component_key("Position"),
            mode: 0,
        }];
        let mut chunk = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses);
            assert_eq!(
                ffi_get_component_chunk(component_key("Position"), 0, 0, &mut chunk),
                1
            );
            assert!(!chunk.ticks.is_null());
        }

        let mut changed =
            ecs_hybrid::Query::<(ecs_hybrid::Entity,), ecs_hybrid::Changed<Position>>::new(
                engine.world_mut(),
            );
        assert_eq!(changed.iter_mut().count(), 0);
    }

    #[test]
    fn disjoint_managed_writes_mark_the_correct_tick_columns() {
        let mut engine = Engine::new();
        setup_world(&mut engine);
        let baseline = engine.world().change_tick();
        engine.world_mut().set_system_last_run(baseline);
        engine.world_mut().increment_change_tick();

        let accesses = [
            NativeSystemAccess {
                component_key: component_key("Position"),
                mode: 1,
            },
            NativeSystemAccess {
                component_key: component_key("Sprite"),
                mode: 1,
            },
        ];
        let mut positions = empty_chunk();
        let mut sprites = empty_chunk();
        let mut entities = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses);
            assert_eq!(
                ffi_get_component_chunk(component_key("Position"), 1, 0, &mut positions),
                1
            );
            assert_eq!(
                ffi_get_component_chunk(component_key("Sprite"), 1, 0, &mut sprites),
                1
            );
            assert_eq!(ffi_get_entity_chunk(0, &mut entities), 1);
            assert_ne!(positions.ticks, sprites.ticks);
            unsafe {
                simulate_managed_write(&positions, 3);
                simulate_managed_write(&sprites, 7);
            }
        }

        let entity_at = |row| unsafe { *((entities.data as *const ecs_hybrid::Entity).add(row)) };
        let mut changed_positions = ecs_hybrid::Query::<
            (ecs_hybrid::Entity,),
            ecs_hybrid::Changed<Position>,
        >::new(engine.world_mut());
        let position_hits: Vec<_> = changed_positions
            .iter_mut()
            .map(|(entity,)| entity)
            .collect();
        assert_eq!(position_hits, vec![entity_at(3)]);

        let mut changed_sprites = ecs_hybrid::Query::<
            (ecs_hybrid::Entity,),
            ecs_hybrid::Changed<Sprite>,
        >::new(engine.world_mut());
        let sprite_hits: Vec<_> = changed_sprites.iter_mut().map(|(entity,)| entity).collect();
        assert_eq!(sprite_hits, vec![entity_at(7)]);
    }

    #[test]
    fn entity_chunks_are_available_only_during_a_managed_system() {
        let mut engine = Engine::new();
        setup_world(&mut engine);
        let mut chunk = empty_chunk();

        assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 3);
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &[]);
            assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 1);
            assert_eq!(chunk.len, 100);
            assert_eq!(
                chunk.element_size as usize,
                std::mem::size_of::<ecs_hybrid::Entity>()
            );
            assert!(!chunk.data.is_null());
        }
        assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 3);
    }
}
