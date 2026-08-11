//! C# component identities, native bindings, and manifest registration.

use std::collections::{HashMap, HashSet};

use ecs_hybrid::commands::{boxed_component_adder, ComponentAdder};
use ecs_hybrid::{Component, ComponentId, Engine, World};
use serde::Deserialize;
#[cfg(not(feature = "rendering"))]
use trait_type_map::impl_trait_accessible;
use trait_type_map::TraitAccessible;

use super::abi::ComponentChunk;

// In rendering builds these names resolve to the renderer's components so
// managed physics writes directly into the columns consumed by the renderer.
// Headless builds provide layout-identical local definitions instead.
#[cfg(all(feature = "rendering", test))]
pub(super) use ecs_hybrid::Color;
#[cfg(feature = "rendering")]
pub(super) use ecs_hybrid::{Position, Sprite};

#[cfg(not(feature = "rendering"))]
/// Headless ABI mirror of `TracyLive.Position`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Position {
    pub(super) x: f32,
    pub(super) y: f32,
}
#[cfg(not(feature = "rendering"))]
impl Component for Position {}

#[cfg(not(feature = "rendering"))]
/// Headless ABI mirror of the renderer's RGBA color.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Color {
    pub(super) r: f32,
    pub(super) g: f32,
    pub(super) b: f32,
    pub(super) a: f32,
}

#[cfg(not(feature = "rendering"))]
/// Headless ABI mirror of `TracyLive.Sprite`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Sprite {
    pub(super) width: f32,
    pub(super) height: f32,
    pub(super) color: Color,
}
#[cfg(not(feature = "rendering"))]
impl Component for Sprite {}

#[cfg(not(feature = "rendering"))]
impl_trait_accessible!(dyn Component; Position, Sprite);

/// Stable 128-bit identity derived from a managed component's canonical name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct StableComponentId(pub(super) u128);

impl StableComponentId {
    /// Reconstruct the canonical 128-bit ID from the two halves carried by the
    /// C ABI, with the high half restored to its original bit position.
    pub(super) const fn from_halves(low: u64, high: u64) -> Self {
        Self(((high as u128) << 64) | low as u128)
    }
}

pub(super) type ComponentBindings = HashMap<StableComponentId, ComponentBinding>;
type NativeChunkGetter = fn(&mut World, u32, *mut ComponentChunk) -> u8;
type NativeBlobDecoder = fn(*const u8, usize) -> Result<Box<dyn ComponentAdder>, String>;

/// Resolves a stable managed identity to native or type-erased ECS storage.
#[derive(Clone, Copy)]
pub(super) enum ComponentBinding {
    Native {
        component_id: ComponentId,
        get_chunk: NativeChunkGetter,
        size: usize,
        align: usize,
        schema_hash: u64,
        decode: NativeBlobDecoder,
    },
    Dynamic {
        component_id: ComponentId,
        size: usize,
        align: usize,
    },
}

impl ComponentBinding {
    /// Return the engine component ID regardless of whether storage is backed
    /// by a concrete Rust type or a dynamically registered managed layout.
    pub(super) fn component_id(self) -> ComponentId {
        match self {
            Self::Native { component_id, .. } | Self::Dynamic { component_id, .. } => component_id,
        }
    }
}

/// Produce one half of the stable component identity or a native schema hash.
pub(super) const fn component_hash(name: &str, offset: u64) -> u64 {
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

/// Hash a canonical managed full name into its stable 128-bit identity.
pub(super) const fn stable_component_id(name: &str) -> StableComponentId {
    StableComponentId::from_halves(
        component_hash(name, 0xcbf29ce484222325),
        component_hash(name, 0x84222325cbf29ce4),
    )
}

/// Return the `chunk_index`th archetype column containing native component T.
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

/// Copy one managed component blob into a concrete Rust component adder.
///
/// The decoder is stored in a native binding so deferred commands can recover
/// the correct Rust type without a hardcoded component match at the call site.
fn decode_native_component<T>(
    data: *const u8,
    size: usize,
) -> Result<Box<dyn ComponentAdder>, String>
where
    T: Component + TraitAccessible<dyn Component> + Copy + Send,
{
    if data.is_null() || size != std::mem::size_of::<T>() {
        return Err("native component blob does not match its ABI layout".into());
    }
    // SAFETY: the binding validated the exact type size. `read_unaligned`
    // permits managed pinned buffers with no stronger alignment guarantee.
    let component = unsafe { std::ptr::read_unaligned(data.cast::<T>()) };
    Ok(boxed_component_adder(component))
}

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

/// Register one engine-owned component and bind its managed name and schema to
/// the callbacks required by queries and deferred commands.
fn register_native_binding<T>(
    engine: &mut Engine,
    bindings: &mut ComponentBindings,
    managed_name: &str,
    managed_schema: &str,
) where
    T: Component + TraitAccessible<dyn Component> + Copy + Send,
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
            decode: decode_native_component::<T>,
        },
    );
}

/// Register native shared components and create their managed lookup table.
pub(super) fn shared_component_bindings(engine: &mut Engine) -> ComponentBindings {
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

/// Recursively verify that a field and each nested field fit within the byte
/// range of the struct that directly contains it.
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

/// Validate and register all components discovered in the managed assembly.
pub(super) fn register_component_manifest(
    engine: &mut Engine,
    bytes: &[u8],
    mut bindings: ComponentBindings,
) -> Result<ComponentBindings, Box<dyn std::error::Error>> {
    let manifest: Vec<ManagedComponentManifest> = serde_json::from_slice(bytes)?;
    let mut seen = HashSet::new();
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
