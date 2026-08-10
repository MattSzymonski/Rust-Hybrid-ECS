//! Native ECS adapter for scheduler-managed C# systems.
//!
//! # Responsibilities
//!
//! - Defines ABI-compatible mirrors of engine-owned components visible to C#.
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
//! Component IDs are stable 128-bit hashes of canonical managed full names.

// Standard library
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// Current workspace crates
#[cfg(feature = "rendering")]
use ecs_hybrid::{Color, Position, Sprite};
use ecs_hybrid::{Component, ComponentId, ComponentTicks, Engine, SystemAccess, World};
use serde::Deserialize;
use trait_type_map::TraitAccessible;
#[cfg(not(feature = "rendering"))]
use trait_type_map::impl_trait_accessible;

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
impl_trait_accessible!(dyn Component; Position, Sprite);

// =============================================================================
// Native API Layout
// =============================================================================

/// Function table copied by `cs_runtime` during managed initialization.
///
/// Field order and calling conventions must match `EngineApi.cs` exactly.
#[repr(C)]
struct CsEngineApi {
    entity_count: extern "C" fn() -> u32,
    get_component_chunk: extern "C" fn(u64, u64, u8, u32, *mut ComponentChunk) -> u8,
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
    component_key_high: u64,
    mode: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct StableComponentId(u128);

impl StableComponentId {
    const fn from_halves(low: u64, high: u64) -> Self {
        Self(((high as u128) << 64) | low as u128)
    }
}

type NativeChunkGetter = fn(&mut World, u32, *mut ComponentChunk) -> u8;

#[derive(Clone, Copy)]
enum ComponentBinding {
    Native {
        component_id: ComponentId,
        get_chunk: NativeChunkGetter,
        size: usize,
        align: usize,
        schema_hash: u64,
    },
    Dynamic {
        component_id: ComponentId,
        size: usize,
        align: usize,
    },
}

impl ComponentBinding {
    fn component_id(self) -> ComponentId {
        match self {
            Self::Native { component_id, .. } | Self::Dynamic { component_id, .. } => component_id,
        }
    }
}

type ComponentBindings = HashMap<StableComponentId, ComponentBinding>;

// =============================================================================
// Scheduled Invocation Scope
// =============================================================================

thread_local! {
    /// World available only while the current thread executes a C# system.
    static ACTIVE_WORLD: Cell<*mut World> = const { Cell::new(std::ptr::null_mut()) };
    /// Access declaration belonging to that active C# system.
    static ACTIVE_ACCESS: Cell<(*const NativeSystemAccess, usize)> =
        const { Cell::new((std::ptr::null(), 0)) };
    /// Stable component bindings belonging to the active C# runtime.
    static ACTIVE_BINDINGS: Cell<*const ComponentBindings> = const { Cell::new(std::ptr::null()) };
}

/// Clears thread-local native access even if managed execution unwinds.
struct ActiveSystemGuard;

impl ActiveSystemGuard {
    fn set(world: &mut World, access: &[NativeSystemAccess], bindings: &ComponentBindings) -> Self {
        ACTIVE_WORLD.with(|slot| {
            assert!(slot.get().is_null(), "nested managed ECS system invocation");
            slot.set(world as *mut World);
        });
        ACTIVE_ACCESS.with(|slot| slot.set((access.as_ptr(), access.len())));
        ACTIVE_BINDINGS.with(|slot| slot.set(bindings));
        Self
    }
}

impl Drop for ActiveSystemGuard {
    fn drop(&mut self) {
        ACTIVE_ACCESS.with(|slot| slot.set((std::ptr::null(), 0)));
        ACTIVE_BINDINGS.with(|slot| slot.set(std::ptr::null()));
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

fn with_active_context<R>(f: impl FnOnce(&mut World, &ComponentBindings) -> R) -> Option<R> {
    ACTIVE_WORLD.with(|world_slot| {
        ACTIVE_BINDINGS.with(|bindings_slot| {
            let world = world_slot.get();
            let bindings = bindings_slot.get();
            if world.is_null() || bindings.is_null() {
                None
            } else {
                // SAFETY: ActiveSystemGuard installs and clears both pointers
                // for exactly the managed invocation.
                Some(unsafe { f(&mut *world, &*bindings) })
            }
        })
    })
}

/// Check whether the active system declared the requested component mode.
///
/// A write declaration also permits reads; a read declaration never permits
/// writes. `None` means no managed system is currently active on this thread.
fn access_is_authorized(key: StableComponentId, requested_mode: u8) -> Option<bool> {
    ACTIVE_ACCESS.with(|slot| {
        let (pointer, len) = slot.get();
        if pointer.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard stores a slice owned by the registered
        // system closure and clears the slot before that invocation returns.
        let accesses = unsafe { std::slice::from_raw_parts(pointer, len) };
        Some(accesses.iter().any(|access| {
            StableComponentId::from_halves(access.component_key, access.component_key_high) == key
                && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}

// =============================================================================
// Component Registry and Native Query Callbacks
// =============================================================================

const fn component_hash(name: &str, offset: u64) -> u64 {
    let bytes = name.as_bytes();
    let mut hash = offset;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += 1;
    }
    hash
}

const fn stable_component_id(name: &str) -> StableComponentId {
    StableComponentId::from_halves(
        component_hash(name, 0xcbf29ce484222325),
        component_hash(name, 0x84222325cbf29ce4),
    )
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
    key_low: u64,
    key_high: u64,
    mode: u8,
    chunk_index: u32,
    output: *mut ComponentChunk,
) -> u8 {
    if output.is_null() {
        return 0;
    }
    let stable_id = StableComponentId::from_halves(key_low, key_high);
    match access_is_authorized(stable_id, mode) {
        None => return 3,
        Some(false) => return 4,
        Some(true) => {}
    }

    with_active_context(|world, bindings| match bindings.get(&stable_id).copied() {
        Some(ComponentBinding::Native { get_chunk, .. }) => get_chunk(world, chunk_index, output),
        Some(ComponentBinding::Dynamic {
            component_id, size, ..
        }) => {
            let change_tick = world.change_tick().get();
            let Some((archetype, data, len, ticks)) =
                world.dynamic_component_chunk_mut(component_id, chunk_index as usize)
            else {
                return 0;
            };
            let bits = archetype.0;
            // SAFETY: output is non-null and all pointers remain owned by the
            // active world's archetype for the managed invocation.
            unsafe {
                output.write(ComponentChunk {
                    archetype_low: bits as u64,
                    archetype_high: (bits >> 64) as u64,
                    data: data.cast(),
                    len: len as u32,
                    element_size: size as u32,
                    ticks: ticks.as_mut_ptr(),
                    change_tick,
                });
            }
            1
        }
        None => 2,
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
// Managed Component Manifest
// =============================================================================

#[derive(Deserialize)]
struct ManagedComponentManifest {
    stable_id_low: u64,
    stable_id_high: u64,
    full_name: String,
    size: usize,
    alignment: usize,
    schema_hash: u64,
    shared: bool,
    fields: Vec<ManagedFieldManifest>,
}

#[derive(Deserialize)]
struct ManagedFieldManifest {
    name: String,
    offset: usize,
    size: usize,
    primitive_type: String,
    fields: Vec<ManagedFieldManifest>,
}

fn register_native_binding<T>(
    engine: &mut Engine,
    bindings: &mut ComponentBindings,
    managed_name: &str,
    managed_schema: &str,
) where
    T: Component + TraitAccessible<dyn Component> + Clone,
{
    engine.world_mut().register_component::<T>();
    bindings.insert(
        stable_component_id(managed_name),
        ComponentBinding::Native {
            component_id: ComponentId::of::<T>(),
            get_chunk: get_component_chunk::<T>,
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            schema_hash: component_hash(managed_schema, 0xcbf29ce484222325),
        },
    );
}

fn shared_component_bindings(engine: &mut Engine) -> ComponentBindings {
    let mut bindings = HashMap::new();
    register_native_binding::<Position>(
        engine,
        &mut bindings,
        "TracyLive.Position",
        "TracyLive.Position|8|4|X@0:4:System.Single|Y@4:4:System.Single",
    );
    register_native_binding::<Sprite>(
        engine,
        &mut bindings,
        "TracyLive.Sprite",
        "TracyLive.Sprite|24|4|Width@0:4:System.Single|Height@4:4:System.Single|Color@8:16:struct|R@0:4:System.Single|G@4:4:System.Single|B@8:4:System.Single|A@12:4:System.Single",
    );
    bindings
}

fn validate_field_manifest(field: &ManagedFieldManifest, parent_size: usize) -> Result<(), String> {
    let end = field
        .offset
        .checked_add(field.size)
        .ok_or("managed field range overflow")?;
    if field.name.is_empty() || field.primitive_type.is_empty() || end > parent_size {
        return Err("managed field lies outside its component layout".into());
    }
    for nested in &field.fields {
        validate_field_manifest(nested, field.size)?;
    }
    Ok(())
}

fn register_component_manifest(
    engine: &mut Engine,
    bytes: &[u8],
    mut bindings: ComponentBindings,
) -> Result<ComponentBindings, Box<dyn std::error::Error>> {
    let manifest: Vec<ManagedComponentManifest> = serde_json::from_slice(bytes)?;
    let mut seen = std::collections::HashSet::new();
    for component in manifest {
        let stable_id =
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high);
        if stable_component_id(&component.full_name) != stable_id {
            return Err(format!(
                "managed component {} has an ID that does not match its canonical full name",
                component.full_name
            )
            .into());
        }
        if !seen.insert(stable_id) {
            return Err(format!(
                "duplicate component {} in managed manifest",
                component.full_name
            )
            .into());
        }
        if component.size == 0
            || u32::try_from(component.size).is_err()
            || component.alignment == 0
            || !component.alignment.is_power_of_two()
            || std::alloc::Layout::from_size_align(component.size, component.alignment).is_err()
        {
            return Err(format!(
                "invalid layout for managed component {}",
                component.full_name
            )
            .into());
        }
        for field in &component.fields {
            validate_field_manifest(field, component.size)?;
        }

        if let Some(binding) = bindings.get(&stable_id).copied() {
            let (size, align, expected_schema) = match binding {
                ComponentBinding::Native {
                    size,
                    align,
                    schema_hash,
                    ..
                } => (size, align, Some(schema_hash)),
                ComponentBinding::Dynamic { size, align, .. } => (size, align, None),
            };
            if size != component.size || align != component.alignment {
                return Err(format!(
                    "managed mirror {} has layout size/alignment {}/{} but native component uses {}/{}",
                    component.full_name, component.size, component.alignment, size, align
                )
                .into());
            }
            if expected_schema.is_some_and(|hash| hash != component.schema_hash) {
                return Err(format!(
                    "managed mirror {} does not match the native component field schema",
                    component.full_name
                )
                .into());
            }
            continue;
        }
        if component.shared {
            return Err(format!(
                "managed shared component {} has no native engine binding",
                component.full_name
            )
            .into());
        }
        let id = engine.world_mut().register_dynamic_component(
            stable_id.0,
            component.full_name,
            component.size,
            component.alignment,
            component.schema_hash,
        )?;
        bindings.insert(
            stable_id,
            ComponentBinding::Dynamic {
                component_id: id,
                size: component.size,
                align: component.alignment,
            },
        );
    }
    Ok(bindings)
}

// =============================================================================
// Managed Export Signatures
// =============================================================================

type InitFn = extern "system" fn(*const CsEngineApi) -> u8;
type SystemCountFn = extern "system" fn() -> u32;
type ComponentManifestLengthFn = extern "system" fn() -> u32;
type CopyComponentManifestFn = extern "system" fn(*mut u8, u32) -> u8;
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
    _bindings: Arc<ComponentBindings>,
}

impl CSharpRuntime {
    /// Start .NET, load `cs_runtime`, discover managed systems, and register
    /// each system with its reflected read/write access declaration.
    pub fn start(
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
            return Err("cs_runtime initialization failed".into());
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
        setup_world(engine, &bindings)?;

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

            let access = derive_system_access(&managed_access, &bindings)?;
            let managed_access = managed_access.into_boxed_slice();
            let system_bindings = Arc::clone(&bindings);
            let name = Box::leak(format!("csharp_system_{system_index}").into_boxed_str());
            // SAFETY: `derive_system_access` has resolved every managed access
            // and the closure exposes the world only under that exact list.
            unsafe {
                engine.register_system_with_access(
                    name,
                    access,
                    move |world: &mut World, _queue: &mut ecs_hybrid::commands::CommandQueue| {
                        let _guard =
                            ActiveSystemGuard::set(world, &managed_access, &system_bindings);
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
    pub fn poll_reload(&mut self) {
        (self.poll_reload)();
    }
}

/// Translate managed component modes into native scheduler metadata.
fn derive_system_access(
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

// =============================================================================
// Demo World Initialization
// =============================================================================

/// Create the bouncing-ball demo after the managed manifest is registered.
///
/// Every game-defined component is default-constructed through type-erased
/// storage, without the host knowing its name or fields. `Position` and
/// `Sprite` remain native renderer components exposed through the explicit
/// shared-component registry.
fn setup_world(
    engine: &mut Engine,
    bindings: &ComponentBindings,
) -> Result<(), Box<dyn std::error::Error>> {
    const BALL_COUNT: usize = 100;

    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Sprite>();
    engine.world_mut().reserve_entities(BALL_COUNT);
    let dynamic_components: Vec<_> = bindings
        .values()
        .filter_map(|binding| match binding {
            ComponentBinding::Dynamic { component_id, .. } => Some(*component_id),
            ComponentBinding::Native { .. } => None,
        })
        .collect();

    for _ in 0..BALL_COUNT {
        let entity = engine
            .world_mut()
            .create_entity()
            .with(Position { x: 0.0, y: 0.0 })
            .with(Sprite {
                width: 0.0,
                height: 0.0,
                color: Color {
                    r: 1.0,
                    g: 0.3,
                    b: 0.3,
                    a: 1.0,
                },
            })
            .build()
            .expect("C# ball entity should build");
        for &component_id in &dynamic_components {
            engine
                .world_mut()
                .add_dynamic_component_default(entity, component_id)?;
        }
    }
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ecs_hybrid::SystemScheduler;

    fn test_stable_id(name: &str) -> StableComponentId {
        stable_component_id(&format!("TracyLive.{name}"))
    }

    fn native_access(name: &str, mode: u8) -> NativeSystemAccess {
        let id = test_stable_id(name).0;
        NativeSystemAccess {
            component_key: id as u64,
            component_key_high: (id >> 64) as u64,
            mode,
        }
    }

    fn get_test_chunk(name: &str, mode: u8, index: u32, output: *mut ComponentChunk) -> u8 {
        let id = test_stable_id(name).0;
        ffi_get_component_chunk(id as u64, (id >> 64) as u64, mode, index, output)
    }

    fn managed_access(entries: &[(&str, u8)]) -> SystemAccess {
        let native: Vec<_> = entries
            .iter()
            .map(|(name, mode)| native_access(name, *mode))
            .collect();
        let mut engine = Engine::new();
        let mut bindings = shared_component_bindings(&mut engine);
        for (name, _) in entries {
            let stable_id = test_stable_id(name);
            if bindings.contains_key(&stable_id) {
                continue;
            }
            let component_id = engine
                .world_mut()
                .register_dynamic_component(stable_id.0, format!("TracyLive.{name}"), 4, 4, 1)
                .unwrap();
            bindings.insert(
                stable_id,
                ComponentBinding::Dynamic {
                    component_id,
                    size: 4,
                    align: 4,
                },
            );
        }
        derive_system_access(&native, &bindings)
            .expect("managed access should map to native components")
    }

    fn setup_test_world(engine: &mut Engine) -> ComponentBindings {
        let shared = shared_component_bindings(engine);
        let stable_id = stable_component_id("TracyLive.PhysicsState");
        let manifest = serde_json::json!([{
            "stable_id_low": stable_id.0 as u64,
            "stable_id_high": (stable_id.0 >> 64) as u64,
            "full_name": "TracyLive.PhysicsState",
            "size": 28,
            "alignment": 4,
            "schema_hash": 1,
            "shared": false,
            "fields": []
        }]);
        let bindings =
            register_component_manifest(engine, &serde_json::to_vec(&manifest).unwrap(), shared)
                .unwrap();
        setup_world(engine, &bindings).unwrap();
        bindings
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
        assert!(
            scheduler
                .execution_graph()
                .iter()
                .any(|batch| systems.iter().all(|system| batch.contains(system)))
        );
    }

    fn assert_different_batches(scheduler: &SystemScheduler, first: usize, second: usize) {
        assert!(
            !scheduler
                .execution_graph()
                .iter()
                .any(|batch| batch.contains(&first) && batch.contains(&second))
        );
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

    #[test]
    fn managed_manifest_registers_and_queries_a_new_dynamic_component() {
        let mut engine = Engine::new();
        let shared = shared_component_bindings(&mut engine);
        let stable_id = stable_component_id("Game.CustomOnlyInCSharp");
        let manifest = serde_json::json!([{
            "stable_id_low": stable_id.0 as u64,
            "stable_id_high": (stable_id.0 >> 64) as u64,
            "full_name": "Game.CustomOnlyInCSharp",
            "size": 4,
            "alignment": 4,
            "schema_hash": 12345,
            "shared": false,
            "fields": [{
                "name": "Value",
                "offset": 0,
                "size": 4,
                "primitive_type": "System.UInt32",
                "fields": []
            }]
        }]);
        let bindings = register_component_manifest(
            &mut engine,
            &serde_json::to_vec(&manifest).unwrap(),
            shared,
        )
        .unwrap();
        let component_id = bindings[&stable_id].component_id();
        engine
            .world_mut()
            .create_dynamic_entity(&[(component_id, 77_u32.to_ne_bytes().to_vec())])
            .unwrap();

        let accesses = [NativeSystemAccess {
            component_key: stable_id.0 as u64,
            component_key_high: (stable_id.0 >> 64) as u64,
            mode: 1,
        }];
        let mut chunk = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
            assert_eq!(
                ffi_get_component_chunk(
                    stable_id.0 as u64,
                    (stable_id.0 >> 64) as u64,
                    1,
                    0,
                    &mut chunk,
                ),
                1
            );
            assert_eq!(chunk.len, 1);
            assert_eq!(chunk.element_size, 4);
            assert_eq!(unsafe { *(chunk.data as *const u32) }, 77);
        }
    }

    #[test]
    fn managed_shared_component_schema_mismatch_is_rejected() {
        let mut engine = Engine::new();
        let shared = shared_component_bindings(&mut engine);
        let stable_id = stable_component_id("TracyLive.Position");
        let manifest = serde_json::json!([{
            "stable_id_low": stable_id.0 as u64,
            "stable_id_high": (stable_id.0 >> 64) as u64,
            "full_name": "TracyLive.Position",
            "size": 8,
            "alignment": 4,
            "schema_hash": 0,
            "shared": true,
            "fields": [
                { "name": "Y", "offset": 0, "size": 4, "primitive_type": "System.Single", "fields": [] },
                { "name": "X", "offset": 4, "size": 4, "primitive_type": "System.Single", "fields": [] }
            ]
        }]);

        let error = register_component_manifest(
            &mut engine,
            &serde_json::to_vec(&manifest).unwrap(),
            shared,
        )
        .err()
        .expect("an equal-sized but incompatible shared schema must fail");
        assert!(error.to_string().contains("field schema"));
    }

    #[cfg(feature = "rendering")]
    #[test]
    fn csharp_world_supports_the_sprite_renderer_query() {
        let mut engine = Engine::new();
        setup_test_world(&mut engine);

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
        assert!(
            !scheduler
                .get_access(1)
                .unwrap()
                .conflicts_with(scheduler.get_access(2).unwrap())
        );
    }

    #[test]
    fn one_managed_row_write_is_visible_to_rust_changed_filter() {
        let mut engine = Engine::new();
        let bindings = setup_test_world(&mut engine);
        let baseline = engine.world().change_tick();
        engine.world_mut().set_system_last_run(baseline);
        engine.world_mut().increment_change_tick();

        let accesses = [native_access("Position", 1)];
        let mut component_chunk = empty_chunk();
        let mut entity_chunk = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
            assert_eq!(get_test_chunk("Position", 1, 0, &mut component_chunk), 1);
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
        let bindings = setup_test_world(&mut engine);
        let baseline = engine.world().change_tick();
        engine.world_mut().set_system_last_run(baseline);
        engine.world_mut().increment_change_tick();

        let accesses = [native_access("Position", 0)];
        let mut chunk = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
            assert_eq!(get_test_chunk("Position", 0, 0, &mut chunk), 1);
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
        let bindings = setup_test_world(&mut engine);
        let baseline = engine.world().change_tick();
        engine.world_mut().set_system_last_run(baseline);
        engine.world_mut().increment_change_tick();

        let accesses = [native_access("Position", 1), native_access("Sprite", 1)];
        let mut positions = empty_chunk();
        let mut sprites = empty_chunk();
        let mut entities = empty_chunk();
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &accesses, &bindings);
            assert_eq!(get_test_chunk("Position", 1, 0, &mut positions), 1);
            assert_eq!(get_test_chunk("Sprite", 1, 0, &mut sprites), 1);
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
        let bindings = setup_test_world(&mut engine);
        let mut chunk = empty_chunk();

        assert_eq!(ffi_get_entity_chunk(0, &mut chunk), 3);
        {
            let _guard = ActiveSystemGuard::set(engine.world_mut(), &[], &bindings);
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
