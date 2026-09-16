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
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

// External crates
use pill_core::error::{CSharpError, EngineMessage};
use pill_core::info;
use pill_core::telemetry::telemetry_target;
use pill_engine::archetype::{ArchetypeId, Blittability, DynamicFieldPlan, LayoutField};
// The native binding path is windowed-only (its components come from the
// renderer), so these three are unused in a headless build.
#[cfg(feature = "rendering")]
use pill_engine::commands::boxed_component_adder;
use pill_engine::commands::ComponentAdder;
use pill_engine::component_registry::ComponentFieldDescriptor;
#[cfg(feature = "rendering")]
use pill_engine::Component;
use pill_engine::{ComponentId, Engine, World};
use serde::Deserialize;
#[cfg(feature = "rendering")]
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

/// Field types a dynamic component is allowed to contain.
///
/// This is the enforcement behind `DynamicColumn`'s `unsafe impl Send`/`Sync`
/// and its lack of drop glue. That storage is a raw byte buffer: rows are moved
/// with `ptr::copy` and the buffer is freed without running any destructor, so
/// every field must be a blittable value with no ownership, no interior
/// pointer, and nothing to release.
///
/// Before this list existed the only check on a field's type was that its name
/// was non-empty, so a manifest declaring a managed reference passed validation
/// and the resulting column was shared across threads on a promise nothing
/// verified.
///
/// `"struct"` denotes a nested value type; its own fields are validated
/// recursively against this same list, so allowing it does not open a hole.
const BLITTABLE_FIELD_TYPES: &[&str] = &[
    "System.Byte",
    "System.SByte",
    "System.Int16",
    "System.UInt16",
    "System.Int32",
    "System.UInt32",
    "System.Int64",
    "System.UInt64",
    "System.IntPtr",
    "System.UIntPtr",
    "System.Single",
    "System.Double",
    "System.Boolean",
    "System.Char",
    "struct",
];

// =============================================================================
// Types + Impls
// =============================================================================

// The renderer's components, which managed physics writes into directly.
//
// They live in `pill_master_renderer` with the pipeline that draws them, so they
// are reachable only in a windowed build. A headless host registers no native
// binding for them: a managed project that declares a `Sprite` mirror still
// works, falling through to the dynamic byte-level binding like any other
// component the host does not know natively.
#[cfg(feature = "rendering")]
pub(super) use pill_master_renderer::{Color, Position, Sprite};

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

/// The live binding table, replaceable under a running managed project.
///
/// Every managed system closure holds this handle rather than a snapshot of the
/// map. A reload can change a component's layout - or add a component - and the
/// invocation scope a system installs when it runs has to see the layout its
/// columns actually use, not the one that was current when the system was
/// registered. The lock is held for the duration of one system run and taken
/// for writing only between frames, so the two never contend.
pub(super) struct BindingStore {
    /// The table itself.
    bindings: RwLock<ComponentBindings>,
}

impl BindingStore {
    /// Wrap one binding table.
    pub(super) fn new(bindings: ComponentBindings) -> Self {
        Self {
            bindings: RwLock::new(bindings),
        }
    }

    /// Borrow the table for reading.
    ///
    /// A panic while the table is locked cannot leave it wrong - it is a map of
    /// plain data, and a writer completes or abandons one whole entry - so a
    /// poisoned lock is reported as the value it guarded rather than becoming a
    /// new failure mode for every later system run.
    pub(super) fn read(&self) -> RwLockReadGuard<'_, ComponentBindings> {
        self.bindings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Borrow the table for writing, with the same rule as [`Self::read`].
    pub(super) fn write(&self) -> RwLockWriteGuard<'_, ComponentBindings> {
        self.bindings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Copies one archetype column into an ABI `ComponentChunk` for managed code.
type NativeChunkGetter = fn(&mut World, u32, *mut ComponentChunk) -> u8;

/// Copies one component column of an already-known archetype into an ABI chunk.
type NativeArchetypeChunkGetter = fn(&mut World, ArchetypeId, *mut ComponentChunk) -> u8;

/// Decodes one managed component blob into a deferred command adder.
type NativeBlobDecoder = fn(*const u8, usize) -> Result<Box<dyn ComponentAdder>, String>;

/// Resolves a stable managed identity to native or type-erased ECS storage.
///
/// Native bindings carry typed callbacks for chunk access and blob decoding;
/// dynamic bindings keep only the layout facts needed for raw byte storage.
#[derive(Clone, Copy)]
pub(super) enum ComponentBinding {
    /// A concrete Rust component with chunk access and a typed decoder.
    ///
    /// Constructed only by [`register_native_binding`], which is itself
    /// windowed-only: the sole native components this host binds are the
    /// renderer's. A headless build still *matches* on the variant, so it stays
    /// in the enum rather than being compiled out with the constructor.
    #[cfg_attr(not(feature = "rendering"), allow(dead_code))]
    Native {
        /// Engine ID of the registered Rust component type.
        component_id: ComponentId,
        /// Copies the matching archetype column into an ABI chunk.
        get_chunk: NativeChunkGetter,
        /// Copies one column of an already-known archetype into an ABI chunk.
        get_chunk_in_archetype: NativeArchetypeChunkGetter,
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
        /// Hash of the managed field schema the storage was registered with.
        ///
        /// Carried here rather than read back from the engine because a reload
        /// has to tell a component whose layout changed from one whose bytes
        /// merely moved, and the two can agree on size and alignment while the
        /// fields underneath them do not.
        schema_hash: u64,
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
///
/// Re-exported from the engine rather than reimplemented: the engine derives a
/// shared component's [`ComponentId`] from its declared name with exactly this
/// function, so two copies of the algorithm could drift apart and silently
/// stop agreeing on an identity.
pub(super) use pill_engine::component::component_name_hash as component_hash;

/// Hash a canonical managed full name into its stable 128-bit identity.
///
/// Managed code names a component with dots (`pill_spline.Spline`) where Rust
/// names it with colons, so this identity is not interchangeable with the one
/// the engine derives for a shared component - the two are related by
/// `ModuleExposedComponent`, which carries both. Only the mixing function is
/// shared.
pub(super) const fn stable_component_id(name: &str) -> StableComponentId {
    StableComponentId::from_halves(
        component_hash(name, 0xcbf29ce484222325),
        component_hash(name, 0x84222325cbf29ce4),
    )
}

/// Return the `chunk_index`th archetype column containing native component T.
///
/// Windowed builds only, with the rest of the native binding path: no other
/// build has a native component to bind.
#[cfg(feature = "rendering")]
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
            entities: std::ptr::null(),
            len: slice.len() as u32,
            element_size: std::mem::size_of::<T>() as u32,
            ticks: ticks.as_mut_ptr(),
            change_tick,
        });
    }
    1
}

/// Return one archetype column containing native component T.
///
/// The archetype-scoped twin of [`get_component_chunk`]: managed enumerators
/// that already hold a driver chunk's archetype identity use this to resolve
/// the remaining query terms directly, with no chunk-index scan.
#[cfg(feature = "rendering")]
fn get_component_chunk_in_archetype<T: Component + TraitAccessible<dyn Component>>(
    world: &mut World,
    archetype_id: ArchetypeId,
    output: *mut ComponentChunk,
) -> u8 {
    let change_tick = world.change_tick().get();
    let Some((archetype, slice, ticks)) =
        world.component_chunk_with_ticks_mut_in_archetype::<T>(archetype_id)
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
            entities: std::ptr::null(),
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
#[cfg(feature = "rendering")]
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
#[cfg(feature = "rendering")]
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
            get_chunk_in_archetype: get_component_chunk_in_archetype::<T>,
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
    // Only a windowed host links `pill_master_renderer`, so a headless build has
    // nothing to bind and the table comes back empty. Spelled as two blocks
    // rather than one guarded call so neither configuration carries an unused
    // `mut` or an unused parameter.
    #[cfg(feature = "rendering")]
    {
        let mut bindings = HashMap::new();
        shared_renderer_bindings(engine, &mut bindings);
        bindings
    }
    #[cfg(not(feature = "rendering"))]
    {
        let _ = engine;
        HashMap::new()
    }
}

/// Bind the renderer's components to their canonical managed mirrors.
///
/// Windowed builds only - the types live in `pill_master_renderer`, which only a
/// windowed host links.
#[cfg(feature = "rendering")]
fn shared_renderer_bindings(engine: &mut Engine, bindings: &mut ComponentBindings) {
    register_native_binding::<Position>(
        engine,
        bindings,
        "TracyLive.Position",
        "TracyLive.Position|8|4|X@0:4:System.Single|Y@4:4:System.Single",
    );
    register_native_binding::<Sprite>(
        engine,
        bindings,
        "TracyLive.Sprite",
        "TracyLive.Sprite|24|4|Width@0:4:System.Single|Height@4:4:System.Single|Color@8:16:struct|R@0:4:System.Single|G@4:4:System.Single|B@8:4:System.Single|A@12:4:System.Single",
    );
    register_native_binding::<Color>(
        engine,
        bindings,
        "TracyLive.Color",
        "TracyLive.Color|16|4|R@0:4:System.Single|G@4:4:System.Single|B@8:4:System.Single|A@12:4:System.Single",
    );
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
    /// Compile-time field layout registered with the component, when the
    /// component was declared with `#[derive(PillComponent)]`. Empty for
    /// hand-registered or dynamic components, which keep the opaque ABI-blob
    /// mirror.
    pub(crate) fields: Vec<ComponentFieldDescriptor>,
}

/// Resolve one exposed component name to its engine [`ComponentId`], reporting
/// an ambiguous name instead of binding managed code to a guessed column.
///
/// A name that resolves to nothing is an ordinary outcome - the module that
/// registered it may be unloaded - and yields `None` quietly. A name claimed by
/// two registrations is not: managed code would be bound to whichever column
/// won an arbitrary tiebreak, and every read and write through that binding
/// would silently address the wrong rows. That case is logged and left unbound.
pub(crate) fn resolve_exposed_component_id(world: &World, type_name: &str) -> Option<ComponentId> {
    match world.resolve_component_id_by_name_any(type_name) {
        Ok(component_id) => component_id,
        Err(error) => {
            pill_core::error!(
                target: telemetry_target::ECS,
                type_name = %type_name,
                error = %error,
                "exposed component name is ambiguous; leaving it unbound for managed code"
            );
            None
        }
    }
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
    // Validate every binding against the live column before it is handed out.
    // An id that is gone yields no binding, and one whose registered layout
    // disagrees with the facts the module forwarded would surface later as a
    // wrong-size chunk served to managed code - which is why a mismatch is
    // logged and dropped here instead.
    bindings.retain(|_, binding| match binding {
        ComponentBinding::ModuleNative {
            component_id,
            size,
            align,
            ..
        } => match engine.world().component_layout(*component_id) {
            Some((live_size, live_align)) if live_size == *size && live_align == *align => true,
            Some((live_size, live_align)) => {
                pill_core::error!(
                    target: telemetry_target::ECS,
                    component_id = ?component_id,
                    forwarded_size = *size,
                    forwarded_align = *align,
                    live_size,
                    live_align,
                    "module component binding disagrees with the live column; dropping it"
                );
                false
            }
            None => false,
        },
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
/// empty field or type, declares a type outside [`BLITTABLE_FIELD_TYPES`], or
/// exceeds the maximum nesting depth.
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
        // Reject anything that is not a blittable value type. `DynamicColumn`
        // copies rows as raw bytes and frees its buffer without running drop
        // glue, so a field owning a resource would be duplicated on move and
        // leaked on free - and sharing such a column across threads, which the
        // engine does, would be unsound.
        if !BLITTABLE_FIELD_TYPES.contains(&field.primitive_type.as_str()) {
            return Err(format!(
                "managed field {} has non-blittable type {}; dynamic components                  must contain only unmanaged value types",
                field.name, field.primitive_type
            ));
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

/// Map a managed primitive type onto the engine's field type-tag vocabulary.
///
/// Returns `None` for blittable types the engine cannot decode (the field is
/// then omitted from the registered layout but keeps its bytes in storage).
fn managed_primitive_tag(primitive_type: &str) -> Option<&'static str> {
    match primitive_type {
        "System.Byte" => Some("u8"),
        "System.SByte" => Some("i8"),
        "System.Int16" => Some("i16"),
        "System.UInt16" => Some("u16"),
        "System.Int32" => Some("i32"),
        "System.UInt32" => Some("u32"),
        "System.Int64" => Some("i64"),
        "System.UInt64" => Some("u64"),
        "System.Single" => Some("f32"),
        "System.Double" => Some("f64"),
        "System.Boolean" => Some("bool"),
        // `System.Char` is a blittable UTF-16 code unit with the same size as
        // the engine's `u16`; exposing it that way keeps the field editable.
        "System.Char" => Some("u16"),
        _ => None,
    }
}

/// Convert a managed component manifest's field tree into engine descriptors.
///
/// Primitive leaves map onto the engine's type-tag vocabulary so the editor
/// can decode and edit them. Nested `struct:` fields stay opaque: the engine
/// has no struct walking, so their bytes are visible but not interpretable.
/// Field names and struct tags are leaked once per registration, bounded by
/// the number of distinct C# component types in the process.
fn managed_field_layout(
    component_name: &str,
    fields: &[ManagedFieldManifest],
) -> Vec<ComponentFieldDescriptor> {
    let mut layout = Vec::new();
    for field in fields {
        if field.primitive_type == "struct" {
            layout.push(ComponentFieldDescriptor {
                name: Box::leak(field.name.clone().into_boxed_str()),
                type_tag: Box::leak(
                    format!("struct:{component_name}::{}", field.name).into_boxed_str(),
                ),
                offset: field.offset,
                size: field.size,
                align: 1,
                element_count: 0,
            });
            continue;
        }
        let Some(type_tag) = managed_primitive_tag(&field.primitive_type) else {
            continue;
        };
        layout.push(ComponentFieldDescriptor {
            name: Box::leak(field.name.clone().into_boxed_str()),
            type_tag,
            offset: field.offset,
            size: field.size,
            align: 1,
            element_count: 0,
        });
    }
    layout
}

/// Render a registered field layout as one stable, parseable line so
/// integration suites can assert the managed manifest reached the engine
/// intact: `name@offset:size:type_tag` entries joined by `|`.
fn format_field_layout_line(layout: &[ComponentFieldDescriptor]) -> String {
    layout
        .iter()
        .map(|field| {
            format!(
                "{}@{}:{}:{}",
                field.name, field.offset, field.size, field.type_tag
            )
        })
        .collect::<Vec<_>>()
        .join("|")
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
    for component in parse_and_validate_manifest(bytes)? {
        let stable_id =
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high);

        if let Some(binding) = bindings.get(&stable_id).copied() {
            check_binding_against_manifest(binding, &component)?;
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
        // The editor layout is computed here, not at the top of the loop: it
        // leaks every field name and struct tag, and entries that already had
        // a binding - or a manifest just refused as shared - must not leak
        // anything. Both reads happen before the name moves into
        // `register_dynamic_component` below.
        let component_name = component.full_name.clone();
        let field_layout = managed_field_layout(&component.full_name, &component.fields);
        let id = engine
            .world_mut()
            .register_dynamic_component(
                stable_id.0,
                component.full_name,
                component.size,
                component.alignment,
                component.schema_hash,
                // `parse_and_validate_manifest` ran `BLITTABLE_FIELD_TYPES`
                // over every field before this point, so the witness is the
                // record of a check rather than a restatement of the promise.
                Blittability::from_manifest_fields(),
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
                schema_hash: component.schema_hash,
            },
        );
        // The binding and the registered column are two records of one layout,
        // and two records can drift. Asserted where both are written, so a
        // future path that relayouts the column without the store fails in
        // debug builds at its source; the chunk path itself serves the live
        // column, so it cannot mis-stride no matter what the store says.
        debug_assert_eq!(
            engine.world().component_layout(id),
            Some((component.size, component.alignment)),
            "the column just registered must match the binding just stored"
        );

        // Slice G: give the editor the same field vocabulary `#[derive(PillComponent)]`
        // produces, so a C# component shows named, editable fields instead of
        // nothing. Re-registration on assembly swap replaces the layout.
        info!(
            target: telemetry_target::HOT_RELOAD,
            component = %component_name,
            fields = %format_field_layout_line(&field_layout),
            "managed component field layout registered"
        );
        engine
            .world_mut()
            .register_dynamic_component_field_layout(id, field_layout)
            .map_err(|error| CSharpError::ManifestInvalid {
                message: error.to_string(),
            })?;
    }
    Ok(bindings)
}

/// Parse a managed manifest and check every entry against its own identity.
///
/// Split out of [`register_component_manifest`] so the reload path validates
/// exactly what the startup path validates. Every check here is a property of
/// the manifest alone - identity, uniqueness, and the shape of each layout - so
/// both callers can run it before either touches the world.
fn parse_and_validate_manifest(bytes: &[u8]) -> Result<Vec<ManagedComponentManifest>, CSharpError> {
    // Step 1: Parse and validate every entry against canonical identities.
    let manifest: Vec<ManagedComponentManifest> = serde_json::from_slice(bytes)?;
    let mut seen = HashSet::new();
    for component in &manifest {
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
    }
    Ok(manifest)
}

/// Check one manifest entry against the binding the host already holds.
///
/// The startup and reload paths share this so they cannot disagree about what
/// "the same component" means: a reload that accepted a disagreement startup
/// would have refused is a reload that changed the meaning of a registered id.
fn check_binding_against_manifest(
    binding: ComponentBinding,
    component: &ManagedComponentManifest,
) -> Result<(), CSharpError> {
    let (size, align, expected_schema) = match binding {
        ComponentBinding::Native {
            size,
            align,
            schema_hash,
            ..
        } => (size, align, Some(schema_hash)),
        ComponentBinding::Dynamic {
            size,
            align,
            schema_hash,
            ..
        } => (size, align, Some(schema_hash)),
        ComponentBinding::ModuleNative { size, align, .. } => {
            // No schema hash to compare: the managed manifest's hash is an FNV
            // over a C#-only schema text (managed type names, nested-struct
            // recursion) that the engine's flat field descriptors cannot
            // reproduce, so a hash invented here would refuse healthy mirrors.
            // The mirror is regenerated from the module's own descriptors on
            // every reload, and `module_native_bindings` validates the binding
            // against the live column before it is handed out.
            (size, align, None)
        }
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
    Ok(())
}

/// What one applied manifest changed, for the caller's log line.
#[derive(Debug, Default)]
pub(super) struct ManifestApplyReport {
    /// Managed components whose layout changed and whose rows were migrated.
    pub(super) migrated: Vec<String>,
    /// Managed components the manifest added.
    pub(super) added: Vec<String>,
}

/// Apply a swapped assembly's component manifest to the live world.
///
/// The reload counterpart of [`register_component_manifest`]. Where startup may
/// register what it likes, this one has to *migrate*: a dynamic component whose
/// layout changed keeps its entities and its rows, and only the bytes move.
///
/// Three cases are refused rather than migrated, each for a reason the caller
/// cannot talk its way out of:
///
/// - a `Native` or `ModuleNative` binding whose layout or schema changed - the
///   Rust side did not change, so the mirror is simply wrong;
/// - a shared component with no native binding, exactly as at startup;
/// - a dynamic component the manifest has stopped naming, which would need its
///   storage retired (a byte-storage analogue of `drop_forgotten_components`,
///   planned as slice 3b).
///
/// The whole manifest is planned before any of it is applied: every entry is
/// resolved to a no-op, a check, an addition or a migration first, so the
/// refusals validation cannot foresee - a shared entry with no binding, an
/// engine registration or relayout error - leave the world and the bindings as
/// they were. Residual failures during application are unwound from a journal,
/// so "as they were" holds for every exit and not only the planned ones.
///
/// # Errors
///
/// Returns a [`CSharpError`] for any of the refusals above, for a malformed
/// manifest, or when the engine refuses a registration or a relayout.
pub(super) fn apply_component_manifest_on_reload(
    engine: &mut Engine,
    bytes: &[u8],
    store: &BindingStore,
) -> Result<ManifestApplyReport, CSharpError> {
    let manifest = parse_and_validate_manifest(bytes)?;
    let live: HashSet<StableComponentId> = manifest
        .iter()
        .map(|component| {
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high)
        })
        .collect();

    // Step 1: Refuse anything this path cannot do, before it does anything.
    // A dynamic component the manifest stopped naming would keep its columns
    // and its registry entry, and a later registration could recycle its bit.
    let retired: Vec<StableComponentId> = store
        .read()
        .iter()
        .filter(|(id, binding)| {
            !live.contains(id) && matches!(binding, ComponentBinding::Dynamic { .. })
        })
        .map(|(id, _)| *id)
        .collect();
    if !retired.is_empty() {
        return Err(CSharpError::ManifestInvalid {
            message: format!(
                "{} managed component(s) disappeared from the manifest; restart the host to retire their storage",
                retired.len()
            ),
        });
    }

    // Step 2: Resolve every entry before the first mutation. The borrowed
    // reads below (`store`, `engine`) are read-only, so a refusal here leaves
    // both untouched.
    let planned = plan_manifest(engine, store, manifest)?;

    // Step 3: Execute the plan, journalling one undo per applied entry. A
    // refusal the plan could not see - the engine rejecting a registration, a
    // relayout meeting a column that drifted beneath it - unwinds the journal
    // in reverse before the error is returned.
    let mut undos: Vec<ManifestUndo> = Vec::with_capacity(planned.len());
    let mut report = ManifestApplyReport::default();
    for entry in planned {
        match apply_planned_entry(engine, store, entry, &mut report) {
            Ok(Some(undo)) => undos.push(undo),
            Ok(None) => {}
            Err(error) => {
                rollback_manifest_apply(engine, store, undos);
                return Err(error);
            }
        }
    }
    Ok(report)
}

/// One manifest entry with everything the engine and the store can tell us
/// resolved up front.
///
/// The apply phase executes these in order and re-decides nothing, which is
/// what keeps the refusals out of the mutated state; what still goes wrong at
/// apply time is handled by the undo journal.
enum PlannedManifestEntry {
    /// The binding table already agrees with the manifest.
    Settled,
    /// A dynamic component the manifest adds.
    Add {
        stable_id: StableComponentId,
        component: ManagedComponentManifest,
    },
    /// A dynamic component whose layout changed and whose rows must migrate.
    Migrate {
        stable_id: StableComponentId,
        component: ManagedComponentManifest,
        component_id: ComponentId,
        plan: DynamicFieldPlan,
        previous_binding: ComponentBinding,
        previous_fields: Vec<ComponentFieldDescriptor>,
    },
}

/// One applied entry, recorded so a later refusal can be undone.
enum ManifestUndo {
    /// The entry registered a dynamic component.
    Added {
        stable_id: StableComponentId,
        component_id: ComponentId,
    },
    /// The entry migrated a dynamic component to a new shape.
    Migrated {
        stable_id: StableComponentId,
        component_id: ComponentId,
        previous_binding: ComponentBinding,
        previous_fields: Vec<ComponentFieldDescriptor>,
    },
}

/// Resolve every manifest entry against the store and the engine.
///
/// The one refusal that lives here rather than in validation is the shared
/// entry with no native binding; `register_manifest_entry` repeats it as a
/// defensive check, but planning means it is raised before anything moves.
fn plan_manifest(
    engine: &Engine,
    store: &BindingStore,
    manifest: Vec<ManagedComponentManifest>,
) -> Result<Vec<PlannedManifestEntry>, CSharpError> {
    let mut planned = Vec::with_capacity(manifest.len());
    for component in manifest {
        let stable_id =
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high);
        let Some(binding) = store.read().get(&stable_id).copied() else {
            if component.shared {
                return Err(format!(
                    "managed shared component {} has no native engine binding",
                    component.full_name
                )
                .into());
            }
            planned.push(PlannedManifestEntry::Add {
                stable_id,
                component,
            });
            continue;
        };

        let ComponentBinding::Dynamic {
            component_id,
            size,
            align,
            schema_hash,
        } = binding
        else {
            check_binding_against_manifest(binding, &component)?;
            planned.push(PlannedManifestEntry::Settled);
            continue;
        };

        // The same layout means nothing to do; anything else is a migration.
        if size == component.size
            && align == component.alignment
            && schema_hash == component.schema_hash
        {
            planned.push(PlannedManifestEntry::Settled);
            continue;
        }

        let plan = build_field_plan(engine, component_id, &component.fields);
        // The fields are captured as owned data here so the inverse plan needs
        // no engine borrow later, when the world is being mutated again.
        let previous_fields = engine
            .world()
            .component_field_layout(component_id)
            .unwrap_or(&[])
            .to_vec();
        planned.push(PlannedManifestEntry::Migrate {
            stable_id,
            component,
            component_id,
            plan,
            previous_binding: binding,
            previous_fields,
        });
    }
    Ok(planned)
}

/// Execute one planned entry, reporting what it changed.
///
/// Returns the undo it journalled, if any: `Settled` entries change nothing
/// and need none.
fn apply_planned_entry(
    engine: &mut Engine,
    store: &BindingStore,
    entry: PlannedManifestEntry,
    report: &mut ManifestApplyReport,
) -> Result<Option<ManifestUndo>, CSharpError> {
    match entry {
        PlannedManifestEntry::Settled => Ok(None),
        PlannedManifestEntry::Add {
            stable_id,
            component,
        } => {
            let added_name = component.full_name.clone();
            register_manifest_entry(engine, store, stable_id, component)?;
            // The undo needs the id the registration minted, and the store
            // entry just written is the only place that knows it.
            let Some(component_id) = store
                .read()
                .get(&stable_id)
                .map(|binding| binding.component_id())
            else {
                return Err(CSharpError::ManifestInvalid {
                    message: format!(
                        "component {added_name} was registered but the binding table has no entry for it"
                    ),
                });
            };
            info!(
                target: telemetry_target::HOT_RELOAD,
                component = %added_name,
                "managed component registered on reload"
            );
            report.added.push(added_name);
            Ok(Some(ManifestUndo::Added {
                stable_id,
                component_id,
            }))
        }
        PlannedManifestEntry::Migrate {
            stable_id,
            component,
            component_id,
            plan,
            previous_binding,
            previous_fields,
        } => {
            let migrated_rows = engine
                .world_mut()
                .relayout_dynamic_component(
                    component_id,
                    component.size,
                    component.alignment,
                    component.schema_hash,
                    &plan,
                )
                .map_err(|error| CSharpError::ManifestInvalid {
                    message: error.to_plain_message(),
                })?;
            store.write().insert(
                stable_id,
                ComponentBinding::Dynamic {
                    component_id,
                    size: component.size,
                    align: component.alignment,
                    schema_hash: component.schema_hash,
                },
            );
            engine
                .world_mut()
                .register_dynamic_component_field_layout(
                    component_id,
                    managed_field_layout(&component.full_name, &component.fields),
                )
                .map_err(|error| CSharpError::ManifestInvalid {
                    message: error.to_string(),
                })?;
            info!(
                target: telemetry_target::HOT_RELOAD,
                component = %component.full_name,
                rows = migrated_rows,
                "managed component layout migrated"
            );
            report.migrated.push(component.full_name);
            Ok(Some(ManifestUndo::Migrated {
                stable_id,
                component_id,
                previous_binding,
                previous_fields,
            }))
        }
    }
}

/// Undo every applied entry, newest first.
///
/// Best effort by design: a rollback that fails leaves the process mixed, so
/// the failure is logged with the component it concerns rather than raised
/// over the original refusal, which is the error the caller needs.
fn rollback_manifest_apply(engine: &mut Engine, store: &BindingStore, undos: Vec<ManifestUndo>) {
    for undo in undos.into_iter().rev() {
        match undo {
            ManifestUndo::Added {
                stable_id,
                component_id,
            } => {
                store.write().remove(&stable_id);
                // `drop_forgotten_component_ids` removes the rows and every
                // registration artifact of the id, which is exactly what an
                // added-then-refused component has to give back.
                let dropped = engine
                    .world_mut()
                    .drop_forgotten_component_ids(&[component_id]);
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    dropped,
                    "rolled back a managed component registered by a refused manifest"
                );
            }
            ManifestUndo::Migrated {
                stable_id,
                component_id,
                previous_binding,
                previous_fields,
            } => {
                let ComponentBinding::Dynamic {
                    size,
                    align,
                    schema_hash,
                    ..
                } = previous_binding
                else {
                    // Only dynamic bindings are ever journalled as migrated.
                    continue;
                };
                let plan = rollback_field_plan(engine, component_id, &previous_fields);
                match engine.world_mut().relayout_dynamic_component(
                    component_id,
                    size,
                    align,
                    schema_hash,
                    &plan,
                ) {
                    Ok(restored_rows) => {
                        store.write().insert(stable_id, previous_binding);
                        if let Err(error) = engine
                            .world_mut()
                            .register_dynamic_component_field_layout(component_id, previous_fields)
                        {
                            // The rows are back but the editor's vocabulary
                            // is not; name the component and keep unwinding.
                            pill_core::error!(
                                target: telemetry_target::HOT_RELOAD,
                                component_id = ?component_id,
                                error = %error,
                                "could not restore the component's field layout during rollback"
                            );
                        }
                        info!(
                            target: telemetry_target::HOT_RELOAD,
                            rows = restored_rows,
                            "rolled back a managed component layout migration"
                        );
                    }
                    Err(error) => {
                        // The rows could not be put back; name the component
                        // so the mixed state is diagnosable rather than silent.
                        pill_core::error!(
                            target: telemetry_target::HOT_RELOAD,
                            component_id = ?component_id,
                            error = %error.to_plain_message(),
                            "could not undo a manifest layout migration"
                        );
                    }
                }
            }
        }
    }
}

/// Build the plan that puts a component back to a recorded field layout.
///
/// The inverse of [`build_field_plan`] for the rollback path: the engine's
/// current layout is the source, and the descriptors captured before the
/// migration are the destination.
fn rollback_field_plan(
    engine: &Engine,
    component_id: ComponentId,
    previous_fields: &[ComponentFieldDescriptor],
) -> DynamicFieldPlan {
    let current: Vec<LayoutField<'_>> = engine
        .world()
        .component_field_layout(component_id)
        .unwrap_or(&[])
        .iter()
        .map(|field| LayoutField {
            name: field.name,
            offset: field.offset,
            size: field.size,
        })
        .collect();
    let target: Vec<LayoutField<'_>> = previous_fields
        .iter()
        .map(|field| LayoutField {
            name: field.name,
            offset: field.offset,
            size: field.size,
        })
        .collect();
    DynamicFieldPlan::between(&current, &target)
}

/// Register one manifest entry that the bindings table has no entry for.
///
/// The startup path in one function: a shared component needs a native binding
/// the manifest cannot conjure, and anything else becomes dynamic storage with
/// its editor-facing field layout.
fn register_manifest_entry(
    engine: &mut Engine,
    store: &BindingStore,
    stable_id: StableComponentId,
    component: ManagedComponentManifest,
) -> Result<(), CSharpError> {
    if component.shared {
        return Err(format!(
            "managed shared component {} has no native engine binding",
            component.full_name
        )
        .into());
    }
    let field_layout = managed_field_layout(&component.full_name, &component.fields);
    let id = engine
        .world_mut()
        .register_dynamic_component(
            stable_id.0,
            component.full_name.clone(),
            component.size,
            component.alignment,
            component.schema_hash,
            // The entry was parsed by `parse_and_validate_manifest`, whose
            // `BLITTABLE_FIELD_TYPES` check is what earns this witness.
            Blittability::from_manifest_fields(),
        )
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    store.write().insert(
        stable_id,
        ComponentBinding::Dynamic {
            component_id: id,
            size: component.size,
            align: component.alignment,
            schema_hash: component.schema_hash,
        },
    );
    engine
        .world_mut()
        .register_dynamic_component_field_layout(id, field_layout)
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_string(),
        })?;
    Ok(())
}

/// Build the byte plan from the layout a component has now to the one a
/// manifest asks for.
///
/// The old side comes from the world rather than from the previous manifest:
/// the field layout registered with a dynamic component is the layout its
/// columns actually use, which is the only thing a byte copy can be measured
/// against.
fn build_field_plan(
    engine: &Engine,
    component_id: ComponentId,
    fields: &[ManagedFieldManifest],
) -> DynamicFieldPlan {
    let previous: Vec<LayoutField<'_>> = engine
        .world()
        .component_field_layout(component_id)
        .unwrap_or(&[])
        .iter()
        .map(|field| LayoutField {
            name: field.name,
            offset: field.offset,
            size: field.size,
        })
        .collect();
    let next: Vec<LayoutField<'_>> = fields
        .iter()
        .map(|field| LayoutField {
            name: field.name.as_str(),
            offset: field.offset,
            size: field.size,
        })
        .collect();
    DynamicFieldPlan::between(&previous, &next)
}
