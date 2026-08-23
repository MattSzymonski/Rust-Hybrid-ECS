//! C# component identities, native bindings, and manifest registration.
//!
//! # Responsibilities
//!
//! - Define native ABI mirrors for shared managed components.
//! - Resolve stable managed identities to native or dynamic bindings.
//! - Validate and register reflected component manifests.
//!
//! # Design
//!
//! In rendering builds the shared component names ([`Position`], [`Color`],
//! [`Sprite`]) resolve to the renderer's own components through a conditional
//! re-export; headless builds provide layout-identical local definitions
//! instead. Every managed component is addressed by a [`StableComponentId`]
//! hashed from its canonical full name, and [`ComponentBinding`] records
//! whether storage is backed by a concrete Rust type or by dynamically
//! registered bytes.

// Standard library
use std::collections::{HashMap, HashSet};

// External crates
use pill_core::error::{CSharpError, EngineMessage};
use pill_engine::commands::{boxed_component_adder, ComponentAdder};
use pill_engine::{Component, ComponentId, Engine, World};
use serde::Deserialize;
#[cfg(not(feature = "rendering"))]
use trait_type_map::impl_trait_accessible;
use trait_type_map::TraitAccessible;

// Current crate
use super::abi::ComponentChunk;

// =============================================================================
// Constants
// =============================================================================

/// Maximum nesting depth accepted in a managed component field tree.
///
/// Real component layouts never exceed a handful of levels. The budget stays
/// below `serde_json`'s own parser recursion limit so this validation, not an
/// opaque parser error, rejects pathological manifests.
const MAX_FIELD_NESTING_DEPTH: usize = 32;

// =============================================================================
// Types + Impls
// =============================================================================

// In rendering builds these names resolve to the renderer's components so
// managed physics writes directly into the columns consumed by the renderer.
// Headless builds provide layout-identical local definitions instead.
#[cfg(feature = "rendering")]
pub(super) use pill_engine::{Color, Position, Sprite};

/// Headless ABI mirror of `TracyLive.Position`.
///
/// Layout-identical to the renderer's component so managed physics writes
/// land in the same columns consumed by rendering builds.
#[cfg(not(feature = "rendering"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Position {
    /// Horizontal position in world units.
    pub(super) x: f32,
    /// Vertical position in world units.
    pub(super) y: f32,
}
#[cfg(not(feature = "rendering"))]
impl Component for Position {}

/// Headless ABI mirror of the renderer's RGBA color.
///
/// Stores the four channels as single-precision floats in the same order the
/// renderer's component uses, keeping the native and managed layouts in sync.
#[cfg(not(feature = "rendering"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Color {
    /// Red channel in the 0.0–1.0 range.
    pub(super) r: f32,
    /// Green channel in the 0.0–1.0 range.
    pub(super) g: f32,
    /// Blue channel in the 0.0–1.0 range.
    pub(super) b: f32,
    /// Alpha channel in the 0.0–1.0 range.
    pub(super) a: f32,
}
#[cfg(not(feature = "rendering"))]
impl Component for Color {}

/// Headless ABI mirror of `TracyLive.Sprite`.
///
/// Carries the sprite's dimensions and tint color; the layout matches the
/// renderer's component so managed code can populate sprite columns directly.
#[cfg(not(feature = "rendering"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Sprite {
    /// Sprite width in world units.
    pub(super) width: f32,
    /// Sprite height in world units.
    pub(super) height: f32,
    /// Tint applied when the sprite is rendered.
    pub(super) color: Color,
}
#[cfg(not(feature = "rendering"))]
impl Component for Sprite {}

// Registers the headless mirrors as trait-accessible so the native binding
// machinery can look them up through `dyn Component` in headless builds.
#[cfg(not(feature = "rendering"))]
impl_trait_accessible!(dyn Component; Position, Sprite, Color);

/// Stable 128-bit identity derived from a managed component's canonical name.
///
/// Produced by [`stable_component_id`] from the canonical full name, so the
/// managed runtime and the host agree on an identity without shared state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct StableComponentId(
    /// The 128-bit canonical identity value.
    pub(super) u128,
);

impl StableComponentId {
    /// Reconstruct the canonical 128-bit ID from the two halves carried by the
    /// C ABI, with the high half restored to its original bit position.
    pub(super) const fn from_halves(low: u64, high: u64) -> Self {
        Self(((high as u128) << 64) | low as u128)
    }
}

/// Maps every stable managed identity to its native or dynamic binding.
pub(super) type ComponentBindings = HashMap<StableComponentId, ComponentBinding>;

/// Copies one archetype column into an ABI `ComponentChunk` for managed code.
type NativeChunkGetter = fn(&mut World, u32, *mut ComponentChunk) -> u8;

/// Decodes one managed component blob into a deferred command adder.
type NativeBlobDecoder = fn(*const u8, usize) -> Result<Box<dyn ComponentAdder>, String>;

/// Resolves a stable managed identity to native or type-erased ECS storage.
///
/// Native bindings carry typed callbacks for chunk access and blob decoding;
/// dynamic bindings keep only the layout facts needed for raw byte storage.
#[derive(Clone, Copy)]
pub(super) enum ComponentBinding {
    /// A concrete Rust component with chunk access and a typed decoder.
    Native {
        /// Engine ID of the registered Rust component type.
        component_id: ComponentId,
        /// Copies the matching archetype column into an ABI chunk.
        get_chunk: NativeChunkGetter,
        /// Size of the Rust type in bytes.
        size: usize,
        /// Alignment of the Rust type in bytes.
        align: usize,
        /// Hash of the managed schema the native type must match.
        schema_hash: u64,
        /// Decodes one managed blob into a deferred command adder.
        decode: NativeBlobDecoder,
    },
    /// A dynamically registered layout stored as raw bytes.
    Dynamic {
        /// Engine ID of the dynamically registered component type.
        component_id: ComponentId,
        /// Size of the managed layout in bytes.
        size: usize,
        /// Alignment of the managed layout in bytes.
        align: usize,
    },
    /// A native component registered by an optional Rust module, exposed to
    /// managed code through the raw byte view of its column.
    ///
    /// The host never names the concrete Rust type; reads and writes go
    /// through the type-erased native column accessors, exactly like dynamic
    /// storage, but the column is the module's own native storage so managed
    /// and Rust code share one source of truth.
    ModuleNative {
        /// Engine ID of the module-registered native component type.
        component_id: ComponentId,
        /// Size of the native layout in bytes.
        size: usize,
        /// Alignment of the native layout in bytes.
        align: usize,
    },
}

impl ComponentBinding {
    /// Return the engine component ID regardless of whether storage is backed
    /// by a concrete Rust type or a dynamically registered managed layout.
    pub(super) fn component_id(self) -> ComponentId {
        match self {
            Self::Native { component_id, .. }
            | Self::Dynamic { component_id, .. }
            | Self::ModuleNative { component_id, .. } => component_id,
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Produce one half of the stable component identity or a native schema hash.
pub(super) const fn component_hash(name: &str, offset: u64) -> u64 {
    let bytes = name.as_bytes();
    let mut hash = offset;
    let mut index = 0;
    // FNV-1a mixing keeps the hash stable across runs and runtimes, so the
    // managed side can reproduce the same value from the canonical name.
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
    // alive for the duration of the active scheduled system invocation. The
    // managed side must stay within `len * element_size` and must not retain
    // the returned pointers beyond that invocation. The u32 length ceiling
    // is documented on `ComponentChunk`.
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
///
/// # Errors
///
/// Returns an error when `data` is null or `size` does not match the exact
/// ABI layout of `T`.
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
    // SAFETY: the binding validated the exact type size and the caller keeps
    // the managed pinned buffer alive for the duration of this call.
    // `read_unaligned` permits buffers with no stronger alignment guarantee.
    let component = unsafe { std::ptr::read_unaligned(data.cast::<T>()) };
    Ok(boxed_component_adder(component))
}

/// Deserialized entry from the managed component manifest.
#[derive(Deserialize)]
struct ManagedComponentManifest {
    /// Low 64 bits of the stable component identity.
    stable_id_low: u64,
    /// High 64 bits of the stable component identity.
    stable_id_high: u64,
    /// Canonical full name used to recompute and verify the identity.
    full_name: String,
    /// Total byte size of the component layout.
    size: usize,
    /// Required byte alignment of the component layout.
    alignment: usize,
    /// Hash of the managed field schema used to match native mirrors.
    schema_hash: u64,
    /// Whether the managed side expects a native engine binding.
    shared: bool,
    /// Top-level field descriptions of the component layout.
    fields: Vec<ManagedFieldManifest>,
}

/// Deserialized field entry within a managed component manifest.
#[derive(Deserialize)]
struct ManagedFieldManifest {
    /// Field name as it appears in the managed schema.
    name: String,
    /// Byte offset of the field within its containing struct.
    offset: usize,
    /// Byte size of the field.
    size: usize,
    /// Canonical managed type name of the field.
    primitive_type: String,
    /// Nested field descriptions when this field is a struct.
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
///
/// The schema strings encode the canonical managed layouts; a mismatch with
/// the runtime's own reflection is rejected during manifest registration.
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
    register_native_binding::<Color>(
        engine,
        &mut bindings,
        "TracyLive.Color",
        "TracyLive.Color|16|4|R@0:4:System.Single|G@4:4:System.Single|B@8:4:System.Single|A@12:4:System.Single",
    );
    bindings
}

/// One native component an optional Rust module registered, exposed to managed
/// code under a derived C#-facing name.
///
/// The host aggregates these after the optional modules load and hands them to
/// the C# backend, which creates a byte-level [`ComponentBinding::ModuleNative`]
/// for each one so `project_cs` can query and write the module's real storage.
#[derive(Debug, Clone)]
pub(crate) struct ModuleExposedComponent {
    /// C#-facing name derived from the registered Rust type name
    /// (`pill_spline::Spline` -> `pill_spline.Spline`). Managed code declares
    /// its mirror struct under exactly this full name so the stable 128-bit
    /// identity matches.
    pub(crate) csharp_name: String,
    /// Engine ID of the module-registered native component.
    pub(crate) component_id: ComponentId,
    /// Size in bytes of the native layout.
    pub(crate) size: usize,
    /// Alignment in bytes of the native layout.
    pub(crate) align: usize,
}

/// Build byte-level bindings for every optional-module component exposed to
/// managed code, keyed by the stable identity of its derived C# name.
///
/// The bindings are merged into the shared table before the managed manifest
/// is registered, so a `project_cs` mirror whose full name matches a module
/// component resolves to the module's native storage instead of being
/// registered as an unrelated dynamic component.
pub(super) fn module_native_bindings(
    engine: &mut Engine,
    exposed: &[ModuleExposedComponent],
) -> ComponentBindings {
    let mut bindings = HashMap::new();
    for component in exposed {
        bindings.insert(
            stable_component_id(&component.csharp_name),
            ComponentBinding::ModuleNative {
                component_id: component.component_id,
                size: component.size,
                align: component.align,
            },
        );
    }
    // Validate that every exposed component is still registered in the live
    // engine; an unknown id would surface only later as an empty column.
    bindings.retain(|_, binding| match binding {
        ComponentBinding::ModuleNative { component_id, .. } => engine
            .world()
            .component_layout(*component_id)
            .is_some(),
        _ => true,
    });
    bindings
}

/// Reject sibling fields that share any byte range.
///
/// Conflicting interpretations of the same storage would corrupt data
/// silently, so overlaps and duplicated offsets are invalid layouts.
///
/// # Errors
///
/// Returns an error naming the first pair of sibling fields that overlap.
fn validate_sibling_non_overlap(
    fields: &[ManagedFieldManifest],
    parent_name: &str,
) -> Result<(), String> {
    for (index, left) in fields.iter().enumerate() {
        let left_end = left.offset.saturating_add(left.size);
        for right in &fields[index + 1..] {
            let right_end = right.offset.saturating_add(right.size);
            if left.offset < right_end && right.offset < left_end {
                return Err(format!(
                    "managed fields {} and {} overlap inside {parent_name}",
                    left.name, right.name
                ));
            }
        }
    }
    Ok(())
}

/// Verify that a field and every nested field fit within the byte range of
/// the struct that directly contains it, and that sibling fields never
/// overlap.
///
/// The field tree is walked with an explicit worklist so deeply nested input
/// consumes heap rather than stack, and the depth budget rejects pathological
/// manifests before they cost real work.
///
/// # Errors
///
/// Returns an error when a field overflows its containing struct, names an
/// empty field or type, or exceeds the maximum nesting depth.
fn validate_field_manifest(field: &ManagedFieldManifest, parent_size: usize) -> Result<(), String> {
    // Each entry carries the field to inspect, the size of the struct that
    // directly contains it, and that branch's current nesting depth.
    let mut worklist = vec![(field, parent_size, 0_usize)];
    while let Some((field, parent_size, depth)) = worklist.pop() {
        let end = field
            .offset
            .checked_add(field.size)
            .ok_or("managed field range overflow")?;
        if field.name.is_empty() || field.primitive_type.is_empty() || end > parent_size {
            return Err("managed field lies outside its component layout".into());
        }
        // The depth check runs after the field validates so the error always
        // names a well-formed field.
        if depth >= MAX_FIELD_NESTING_DEPTH {
            return Err(format!(
                "managed field {} exceeds the maximum nesting depth of {MAX_FIELD_NESTING_DEPTH}",
                field.name
            ));
        }
        validate_sibling_non_overlap(&field.fields, &field.name)?;
        for nested in &field.fields {
            worklist.push((nested, field.size, depth + 1));
        }
    }
    Ok(())
}

/// Validate and register all components discovered in the managed assembly.
///
/// Shared components must already have a native engine binding; every other
/// component is registered as dynamic storage before the bindings are
/// returned to the caller.
///
/// # Errors
///
/// Returns a [`CSharpError`] when the manifest is malformed, an identity does
/// not match its canonical name, a layout is invalid or duplicated, a shared
/// component has no native binding, or a managed mirror disagrees with the
/// native component's layout or field schema.
pub(super) fn register_component_manifest(
    engine: &mut Engine,
    bytes: &[u8],
    mut bindings: ComponentBindings,
) -> Result<ComponentBindings, CSharpError> {
    // Step 1: Parse and validate every entry against canonical identities.
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

        // Sibling fields of the component itself must not overlap either.
        validate_sibling_non_overlap(&component.fields, &component.full_name)?;
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
                ComponentBinding::Dynamic { size, align, .. }
                | ComponentBinding::ModuleNative { size, align, .. } => (size, align, None),
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

        // Step 2: Register each remaining component as dynamic storage.
        if component.shared {
            return Err(format!(
                "managed shared component {} has no native engine binding",
                component.full_name
            )
            .into());
        }
        let id = engine
            .world_mut()
            .register_dynamic_component(
                stable_id.0,
                component.full_name,
                component.size,
                component.alignment,
                component.schema_hash,
            )
            .map_err(|error| CSharpError::ManifestInvalid {
                message: error.to_plain_message(),
            })?;
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
