//! C# component identities, native bindings, and manifest registration.
//!
//! # Responsibilities
//!
//! - Define native ABI mirrors for shared managed components.
//! - Resolve stable managed identities to native or managed bindings.
//! - Register reflected component manifests; the schema model and its
//!   validation live in [`manifest`](super::manifest).
//! - Supply the component half of the shared reload pipeline, as
//!   [`COMPONENT_SUBJECT`]; the stages it runs through live in
//!   [`manifest_apply`](super::manifest_apply).
//!
//! # Design
//!
//! In rendering builds the shared component names ([`Position`], [`Color`],
//! [`MeshRendererComponent`]) resolve to the renderer's own components through a conditional
//! re-export; headless builds provide layout-identical local definitions
//! instead. Every managed component is addressed by a [`StableComponentId`]
//! hashed from its canonical full name, and [`ComponentBinding`] records
//! whether storage is backed by a concrete Rust type or by a managed layout of
//! raw bytes.
//!
//! A reload does not have its own apply loop. It hands the binding table and
//! the arriving declarations to the one pipeline every manifest kind runs
//! through, along with the actions that make a component a component: what its
//! aliases resolve against, which live bindings it may retire, and what
//! registering, migrating, renaming and retiring managed storage do.

// Standard library
use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

// External crates
use pill_core::error::{CSharpError, EngineMessage};
use pill_core::info;
use pill_core::telemetry::telemetry_target;
use pill_engine::archetype::{ArchetypeId, Blittability, FieldPlan, LayoutField};
// The native binding path is windowed-only (its components come from the
// renderer), so these three are unused in a headless build.
#[cfg(feature = "rendering")]
use pill_engine::commands::boxed_component_adder;
use pill_engine::commands::ComponentAdder;
use pill_engine::component_registry::ComponentFieldDescriptor;
#[cfg(feature = "rendering")]
use pill_engine::Component;
use pill_engine::{ComponentId, Engine, World};

// Current crate
use super::manifest::{
    build_field_plan, format_field_layout_line, managed_field_layout, parse_and_validate_manifest,
    plan_tag, split_manifest_kinds, ManagedComponentManifest,
};
use super::manifest_apply::{
    apply_manifest, BindingTable, ManifestSubject, RenamePredecessor, Settlement,
};

// Current crate
use super::abi::ComponentChunk;
// Only the native chunk binders stamp a scope token, and those are
// windowed-only, so a headless build never reaches this.
#[cfg(feature = "rendering")]
use super::context::active_scope_token;

// =============================================================================
// Types + Impls
// =============================================================================

// The components managed physics writes into directly.
//
// `Position` and `Color` are the engine's - universal enough that every
// renderer and most projects want the same two - while `MeshRendererComponent` lives in
// `pill_master_renderer` with the pipeline that draws it. Both imports stay
// gated because it is the *native binding* that is windowed-only, not the
// types: a headless host registers no native binding for any of them, and a
// managed project that declares a `MeshRendererComponent` mirror still works, falling through
// to the managed byte-level binding like any other component the host does not
// know natively.
#[cfg(feature = "rendering")]
pub(super) use pill_engine::common_components::{Color, Position};
#[cfg(feature = "rendering")]
pub(super) use pill_master_renderer::MeshRendererComponent;

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

/// Maps every stable managed identity to its native or managed binding.
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
    #[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
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
/// managed bindings keep only the layout facts needed for raw byte storage.
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
    /// A managed layout stored as raw bytes.
    Managed {
        /// Engine ID of the managed component type.
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
    /// A native component registered by an extension, exposed to
    /// managed code through the raw byte view of its column.
    ///
    /// The host never names the concrete Rust type; reads and writes go
    /// through the type-erased native column accessors, exactly like managed
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
    /// by a concrete Rust type or a managed layout.
    pub(super) fn component_id(self) -> ComponentId {
        match self {
            Self::Native { component_id, .. }
            | Self::Managed { component_id, .. }
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
fn get_component_chunk<T: Component>(
    world: &mut World,
    chunk_index: u32,
    output: *mut ComponentChunk,
) -> u8 {
    let change_tick = world.change_tick().get();
    let scope_token = active_scope_token();
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
            scope_token,
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
fn get_component_chunk_in_archetype<T: Component>(
    world: &mut World,
    archetype_id: ArchetypeId,
    output: *mut ComponentChunk,
) -> u8 {
    let change_tick = world.change_tick().get();
    let scope_token = active_scope_token();
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
            scope_token,
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
    T: Component + Copy + Send,
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

/// Register one engine-owned component and bind its managed name and schema to
/// the callbacks required by queries and deferred commands.
#[cfg(feature = "rendering")]
fn register_native_binding<T>(
    engine: &mut Engine,
    bindings: &mut ComponentBindings,
    managed_name: &str,
    managed_schema: &str,
) where
    T: Component + Copy + Send,
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
    register_native_binding::<MeshRendererComponent>(
        engine,
        bindings,
        "TracyLive.MeshRendererComponent",
        "TracyLive.MeshRendererComponent|16|4|Mesh@0:8:System.UInt64|Material@8:8:System.UInt64",
    );
    register_native_binding::<pill_master_renderer::TransformComponent>(engine, bindings,"TracyLive.TransformComponent",
        "TracyLive.TransformComponent|40|4|X@0:4:System.Single|Y@4:4:System.Single|Z@8:4:System.Single|RotationX@12:4:System.Single|RotationY@16:4:System.Single|RotationZ@20:4:System.Single|RotationW@24:4:System.Single|ScaleX@28:4:System.Single|ScaleY@32:4:System.Single|ScaleZ@36:4:System.Single");
    register_native_binding::<pill_master_renderer::DirectionalLightComponent>(engine,bindings,"TracyLive.DirectionalLightComponent","TracyLive.DirectionalLightComponent|16|4|R@0:4:System.Single|G@4:4:System.Single|B@8:4:System.Single|Intensity@12:4:System.Single");
    register_native_binding::<pill_master_renderer::CameraComponent>(engine, bindings,"TracyLive.CameraComponent",
        "TracyLive.CameraComponent|20|4|Enabled@0:1:System.Byte|Priority@4:4:System.Int32|VerticalFov@8:4:System.Single|Near@12:4:System.Single|Far@16:4:System.Single");
    register_native_binding::<Color>(
        engine,
        bindings,
        "TracyLive.Color",
        "TracyLive.Color|16|4|R@0:4:System.Single|G@4:4:System.Single|B@8:4:System.Single|A@12:4:System.Single",
    );
}

/// One native component an extension registered, exposed to managed
/// code under a derived C#-facing name.
///
/// The host aggregates these after the extensions load and hands them to
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
    /// hand-registered or managed components, which keep the opaque ABI-blob
    /// mirror.
    #[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
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

/// Build byte-level bindings for every extension component exposed to
/// managed code, keyed by the stable identity of its derived C# name.
///
/// The bindings are merged into the shared table before the managed manifest
/// is registered, so a `project_cs` mirror whose full name matches a module
/// component resolves to the module's native storage instead of being
/// registered as an unrelated managed component.
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

/// Validate and register all components discovered in the managed assembly.
///
/// Shared components must already have a native engine binding; every other
/// component is registered as managed storage before the bindings are
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
    let (components, resources) = split_manifest_kinds(parse_and_validate_manifest(bytes)?);
    // Resources first: a startup method may already want to read one, and a
    // resource registration touches no column, so nothing here can be undone by
    // a component entry refused afterwards.
    super::resources::register_resource_manifest(engine, &resources)?;
    for component in components {
        let stable_id =
            StableComponentId::from_halves(component.stable_id_low, component.stable_id_high);

        if let Some(binding) = bindings.get(&stable_id).copied() {
            check_binding_against_manifest(binding, &component)?;
            continue;
        }

        // Step 2: Register each remaining component as managed storage.
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
        // `register_component_descriptor` below.
        let component_name = component.full_name.clone();
        let field_layout = managed_field_layout(&component.full_name, &component.fields);
        let id = engine
            .world_mut()
            .register_component_descriptor(
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
            ComponentBinding::Managed {
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
            .register_component_descriptor_with_layout(id, field_layout)
            .map_err(|error| CSharpError::ManifestInvalid {
                message: error.to_string(),
            })?;
    }
    Ok(bindings)
}

/// Check one manifest entry against the binding the host already holds.
///
/// The startup and reload paths share this so they cannot disagree about what
/// "the same component" means: a reload that accepted a disagreement startup
/// would have refused is a reload that changed the meaning of a registered id.
pub(super) fn check_binding_against_manifest(
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
        ComponentBinding::Managed {
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
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
#[derive(Debug, Default)]
pub(super) struct ManifestApplyReport {
    /// Managed components whose layout changed and whose rows were migrated.
    pub(super) migrated: Vec<String>,
    /// Managed components the manifest added.
    pub(super) added: Vec<String>,
    /// Managed components renamed from an earlier declaration, as
    /// `old name -> new name`.
    pub(super) renamed: Vec<String>,
    /// Managed components whose storage was retired because the manifest
    /// stopped naming them.
    pub(super) retired: Vec<String>,
    /// Managed resources the manifest added.
    pub(super) resources_added: Vec<String>,
    /// Managed resources whose layout changed and whose bytes were migrated.
    pub(super) resources_migrated: Vec<String>,
    /// Managed resources renamed from an earlier declaration, as
    /// `old name -> new name`.
    pub(super) resources_renamed: Vec<String>,
    /// Managed resources whose declaration was retired because the manifest
    /// stopped naming it.
    pub(super) resources_retired: Vec<String>,
}

/// Apply a swapped assembly's component manifest to the live world.
///
/// The reload counterpart of [`register_component_manifest`]. Where startup may
/// register what it likes, this one has to *migrate*: a managed component whose
/// layout changed keeps its entities and its rows, and only the bytes move.
///
/// Three cases are refused rather than migrated, each for a reason the caller
/// cannot talk its way out of:
///
/// - a `Native` or `ModuleNative` binding whose layout or schema changed - the
///   Rust side did not change, so the mirror is simply wrong;
/// - a shared component with no native binding, exactly as at startup;
/// - a managed entry whose aliases name more than one previous registration,
///   because picking one would silently drop the other's rows.
///
/// A managed component the manifest stopped naming is not refused: its storage
/// is retired after everything else has applied, so the dev loop does not
/// demand a restart for a removed type.
///
/// The stages themselves - resolve aliases, collect what vanished, resolve
/// every entry before mutating anything, apply with a journal, unwind on a
/// refusal, retire last - are [`super::manifest_apply`]'s, shared with the
/// resource kind. What is component-specific lives in [`COMPONENT_SUBJECT`]:
/// the identity, the refusals above, and what registering, migrating, renaming
/// and retiring a managed column actually do.
///
/// # Errors
///
/// Returns a [`CSharpError`] for any of the refusals above, for a malformed
/// manifest, or when the engine refuses a registration or a relayout.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn apply_component_manifest_on_reload(
    engine: &mut Engine,
    bytes: &[u8],
    store: &BindingStore,
) -> Result<ManifestApplyReport, CSharpError> {
    let (manifest, resources) = split_manifest_kinds(parse_and_validate_manifest(bytes)?);
    // Resources settle first and on their own terms: they own no column, so a
    // component refused afterwards leaves nothing of theirs half-applied, and a
    // resource entry that reached the component pipeline would be resolved as a
    // column to add.
    let resource_outcome = super::resources::apply_resource_manifest_on_reload(engine, &resources)?;

    // The component table is handed to the shared pipeline as a plain map and
    // published whole when it comes back, which is what makes the table half of
    // the rollback all-or-nothing: nothing observes an intermediate state,
    // because there is no intermediate state to observe.
    let previous = store.read().clone();
    let (result, table) = apply_manifest(&COMPONENT_SUBJECT, engine, previous, &manifest);
    *store.write() = table;
    let outcome = result?;

    Ok(ManifestApplyReport {
        migrated: outcome.migrated,
        added: outcome.added,
        renamed: outcome.renamed,
        retired: outcome.retired,
        resources_added: resource_outcome.added,
        resources_migrated: resource_outcome.migrated,
        resources_renamed: resource_outcome.renamed,
        resources_retired: resource_outcome.retired,
    })
}

// =============================================================================
// The component manifest subject
// =============================================================================

/// One applied component entry, recorded so a later refusal can be undone.
///
/// Engine state only: the binding table's half of an undo belongs to the
/// pipeline, which knows exactly what it wrote where.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) enum ComponentUndo {
    /// The entry registered a managed component.
    Added { component_id: ComponentId },
    /// The entry migrated a managed component to a new shape.
    Migrated {
        component_id: ComponentId,
        previous_binding: ComponentBinding,
        previous_fields: Vec<ComponentFieldDescriptor>,
    },
    /// The entry renamed a managed component from an earlier registration.
    Renamed {
        successor_id: ComponentId,
        predecessor_stable_id: StableComponentId,
        predecessor_name: String,
        predecessor_binding: ComponentBinding,
        predecessor_fields: Vec<ComponentFieldDescriptor>,
    },
}

/// The actions the shared apply pipeline needs for managed components.
///
/// Everything a component does differently from a resource, and nothing else:
/// the six stages themselves live in [`super::manifest_apply`].
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) const COMPONENT_SUBJECT: ManifestSubject<
    ManagedComponentManifest,
    ComponentBinding,
    ComponentUndo,
> = ManifestSubject {
    kind: "component",
    identity: |component| {
        StableComponentId::from_halves(component.stable_id_low, component.stable_id_high)
    },
    name: |component| component.full_name.as_str(),
    aliases: |component| &component.aliases,
    binding_name: component_binding_name,
    // Only managed storage is this pipeline's to retire: a native or
    // module-native binding describes a column Rust owns, and a manifest that
    // stops naming it says nothing about that column's lifetime.
    is_governed: |binding| matches!(binding, ComponentBinding::Managed { .. }),
    resolve_alias: resolve_component_alias,
    check_addable: |component| {
        if component.shared {
            return Err(format!(
                "managed shared component {} has no native engine binding",
                component.full_name
            )
            .into());
        }
        Ok(())
    },
    settle: settle_component,
    register: register_component_entry,
    reshape: reshape_component_entry,
    rename: rename_component_entry,
    retire: |engine, bindings| {
        let retired: Vec<ComponentId> = bindings
            .iter()
            .map(|binding| binding.component_id())
            .collect();
        engine.world_mut().retire_component_storage(&retired)
    },
    undo: undo_component_entry,
};

/// The name a live binding's component is registered under.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn component_binding_name(engine: &Engine, binding: &ComponentBinding) -> String {
    let component_id = binding.component_id();
    engine
        .world()
        .registered_components()
        .iter()
        .find(|(_, id)| *id == component_id)
        .map(|(name, _)| name.clone())
        .unwrap_or_default()
}

/// Resolve one alias to the managed binding it names, if any.
///
/// The alias is a name the engine knows, so it resolves through the registry
/// first and the table is searched for whatever binding holds that id. A
/// resolution that lands on a native binding is ignored: it names a column Rust
/// owns, and a managed entry cannot inherit its rows.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn resolve_component_alias(
    engine: &Engine,
    table: &BindingTable<ComponentBinding>,
    alias: &str,
) -> Result<Option<StableComponentId>, CSharpError> {
    let resolved = engine
        .world()
        .resolve_component_id_by_name_any(alias)
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    let Some(component_id) = resolved else {
        return Ok(None);
    };
    Ok(table.iter().find_map(|(stable_id, binding)| {
        (binding.component_id() == component_id
            && matches!(binding, ComponentBinding::Managed { .. }))
        .then_some(*stable_id)
    }))
}

/// Decide what an arriving entry means for the binding already registered.
///
/// A native or module-native binding is never migrated: the Rust side did not
/// change, so a mirror that disagrees is simply wrong and is refused by the
/// same check startup applies.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn settle_component(
    _engine: &Engine,
    binding: &ComponentBinding,
    component: &ManagedComponentManifest,
) -> Result<Settlement, CSharpError> {
    let ComponentBinding::Managed {
        size,
        align,
        schema_hash,
        ..
    } = binding
    else {
        check_binding_against_manifest(*binding, component)?;
        return Ok(Settlement::Unchanged);
    };
    // The same layout means nothing to do; anything else is a migration.
    if *size == component.size
        && *align == component.alignment
        && *schema_hash == component.schema_hash
    {
        return Ok(Settlement::Unchanged);
    }
    Ok(Settlement::Reshaped)
}

/// Register one manifest entry the binding table has no entry for.
///
/// The startup path in one function: anything that reaches here is managed
/// storage with its editor-facing field layout, because a shared entry with no
/// native binding was refused before the plan.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn register_component_entry(
    engine: &mut Engine,
    stable_id: StableComponentId,
    component: &ManagedComponentManifest,
) -> Result<(ComponentBinding, Option<ComponentUndo>), CSharpError> {
    let component_id = register_managed_storage(engine, stable_id, component)?;
    info!(
        target: telemetry_target::HOT_RELOAD,
        component = %component.full_name,
        "managed component registered on reload"
    );
    Ok((
        managed_binding(component_id, component),
        Some(ComponentUndo::Added { component_id }),
    ))
}

/// Migrate one managed component's rows into the arriving layout.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn reshape_component_entry(
    engine: &mut Engine,
    _stable_id: StableComponentId,
    existing: &ComponentBinding,
    component: &ManagedComponentManifest,
) -> Result<(ComponentBinding, Option<ComponentUndo>), CSharpError> {
    let component_id = existing.component_id();
    let plan = build_field_plan(engine, component_id, &component.fields);
    // The fields are captured as owned data before the migration, so the
    // inverse plan needs no engine borrow later, when the world is being
    // mutated again.
    let previous_fields = engine
        .world()
        .component_field_layout(component_id)
        .unwrap_or(&[])
        .to_vec();
    let migrated_rows = engine
        .world_mut()
        .relayout_descriptor_component(
            component_id,
            component.size,
            component.alignment,
            component.schema_hash,
            &plan,
        )
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    engine
        .world_mut()
        .register_component_descriptor_with_layout(
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
    // A field that changed type keeps its name but loses its value, so saying
    // nothing would leave a reset looking like a migration that worked.
    // Reported per component rather than per field so the line stays readable
    // when a rewrite touches several at once.
    report_retyped_fields(&component.full_name, &plan);
    Ok((
        managed_binding(component_id, component),
        Some(ComponentUndo::Migrated {
            component_id,
            previous_binding: *existing,
            previous_fields,
        }),
    ))
}

/// Register the successor of a renamed component and move the rows across.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn rename_component_entry(
    engine: &mut Engine,
    stable_id: StableComponentId,
    predecessor: &RenamePredecessor<'_, ComponentBinding>,
    component: &ManagedComponentManifest,
) -> Result<(ComponentBinding, Option<ComponentUndo>), CSharpError> {
    let predecessor_id = predecessor.binding.component_id();
    let predecessor_name = predecessor.alias.to_string();
    let predecessor_fields = engine
        .world()
        .component_field_layout(predecessor_id)
        .unwrap_or(&[])
        .to_vec();
    // The plan is measured from the predecessor's live field layout, which
    // registering the successor neither touches nor moves a row of; both happen
    // below, in this order, so a failure in either leaves the undo's record
    // describing what was there.
    let plan = build_field_plan(engine, predecessor_id, &component.fields);
    let successor_id = register_managed_storage(engine, stable_id, component)?;
    let migrated_rows = engine
        .world_mut()
        .remap_descriptor_component(predecessor_id, successor_id, &plan)
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    info!(
        target: telemetry_target::HOT_RELOAD,
        component = %component.full_name,
        predecessor = %predecessor_name,
        rows = migrated_rows,
        "managed component renamed and rows migrated"
    );
    report_retyped_fields(&component.full_name, &plan);
    Ok((
        managed_binding(successor_id, component),
        Some(ComponentUndo::Renamed {
            successor_id,
            predecessor_stable_id: predecessor.identity,
            predecessor_name,
            predecessor_binding: *predecessor.binding,
            predecessor_fields,
        }),
    ))
}

/// Reverse one applied component entry, best effort.
///
/// A rollback that fails leaves the process mixed, so the failure is logged
/// with the component it concerns rather than raised over the original refusal,
/// which is the error the caller needs.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn undo_component_entry(engine: &mut Engine, undo: ComponentUndo) {
    match undo {
        ComponentUndo::Added { component_id } => {
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
        ComponentUndo::Migrated {
            component_id,
            previous_binding,
            previous_fields,
        } => {
            let ComponentBinding::Managed {
                size,
                align,
                schema_hash,
                ..
            } = previous_binding
            else {
                // Only managed bindings are ever journalled as migrated.
                return;
            };
            let plan = rollback_field_plan(engine, component_id, &previous_fields);
            match engine.world_mut().relayout_descriptor_component(
                component_id,
                size,
                align,
                schema_hash,
                &plan,
            ) {
                Ok(restored_rows) => {
                    if let Err(error) = engine
                        .world_mut()
                        .register_component_descriptor_with_layout(component_id, previous_fields)
                    {
                        // The rows are back but the editor's vocabulary is not;
                        // name the component and keep unwinding.
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
                    // The rows could not be put back; name the component so the
                    // mixed state is diagnosable rather than silent.
                    pill_core::error!(
                        target: telemetry_target::HOT_RELOAD,
                        component_id = ?component_id,
                        error = %error.to_plain_message(),
                        "could not undo a manifest layout migration"
                    );
                }
            }
        }
        ComponentUndo::Renamed {
            successor_id,
            predecessor_stable_id,
            predecessor_name,
            predecessor_binding,
            predecessor_fields,
        } => {
            let ComponentBinding::Managed {
                size,
                align,
                schema_hash,
                ..
            } = predecessor_binding
            else {
                // Only managed bindings are ever journalled as renamed.
                return;
            };
            // The predecessor's id derives from its stable id, so re-registering
            // the old name restores the same id the remap retired. The rows move
            // back through the inverse plan, and that remap retires the
            // successor's registration in turn.
            let restored = engine.world_mut().register_component_descriptor(
                predecessor_stable_id.0,
                predecessor_name.clone(),
                size,
                align,
                schema_hash,
                Blittability::from_manifest_fields(),
            );
            let restored_id = match restored {
                Ok(id) => id,
                Err(error) => {
                    pill_core::error!(
                        target: telemetry_target::HOT_RELOAD,
                        component = %predecessor_name,
                        error = %error.to_plain_message(),
                        "could not re-register a renamed component's predecessor during rollback"
                    );
                    return;
                }
            };
            let plan = rollback_field_plan(engine, successor_id, &predecessor_fields);
            match engine
                .world_mut()
                .remap_descriptor_component(successor_id, restored_id, &plan)
            {
                Ok(rows) => {
                    if let Err(error) = engine
                        .world_mut()
                        .register_component_descriptor_with_layout(restored_id, predecessor_fields)
                    {
                        pill_core::error!(
                            target: telemetry_target::HOT_RELOAD,
                            component = %predecessor_name,
                            error = %error,
                            "could not restore the renamed component's field layout during rollback"
                        );
                    }
                    info!(
                        target: telemetry_target::HOT_RELOAD,
                        component = %predecessor_name,
                        rows,
                        "rolled back a managed component rename"
                    );
                }
                Err(error) => {
                    pill_core::error!(
                        target: telemetry_target::HOT_RELOAD,
                        component = %predecessor_name,
                        error = %error.to_plain_message(),
                        "could not move a renamed component's rows back during rollback"
                    );
                }
            }
        }
    }
}

/// Register one manifest entry as managed descriptor storage with its layout.
///
/// Shared by the add and rename actions, which differ only in what happens to
/// the rows afterwards.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn register_managed_storage(
    engine: &mut Engine,
    stable_id: StableComponentId,
    component: &ManagedComponentManifest,
) -> Result<ComponentId, CSharpError> {
    let field_layout = managed_field_layout(&component.full_name, &component.fields);
    let component_id = engine
        .world_mut()
        .register_component_descriptor(
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
    engine
        .world_mut()
        .register_component_descriptor_with_layout(component_id, field_layout)
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_string(),
        })?;
    Ok(component_id)
}

/// The binding one arriving manifest entry describes.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn managed_binding(
    component_id: ComponentId,
    component: &ManagedComponentManifest,
) -> ComponentBinding {
    ComponentBinding::Managed {
        component_id,
        size: component.size,
        align: component.alignment,
        schema_hash: component.schema_hash,
    }
}

/// Report the fields a migration reset, once per component.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn report_retyped_fields(component_name: &str, plan: &FieldPlan) {
    if plan.retyped_fields().is_empty() {
        return;
    }
    info!(
        target: telemetry_target::HOT_RELOAD,
        component = %component_name,
        fields = %plan.retyped_fields().join(", "),
        "managed component fields changed type and were reset to their default bytes"
    );
}

/// Build the plan that puts a component back to a recorded field layout.
///
/// The inverse of [`super::manifest::build_field_plan`] for the rollback path:
/// the engine's current layout is the source, and the descriptors captured
/// before the migration are the destination.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn rollback_field_plan(
    engine: &Engine,
    component_id: ComponentId,
    previous_fields: &[ComponentFieldDescriptor],
) -> FieldPlan {
    let current: Vec<LayoutField<'_>> = engine
        .world()
        .component_field_layout(component_id)
        .unwrap_or(&[])
        .iter()
        .map(|field| LayoutField {
            name: field.name,
            type_tag: plan_tag(field.type_tag),
            offset: field.offset,
            size: field.size,
        })
        .collect();
    let target: Vec<LayoutField<'_>> = previous_fields
        .iter()
        .map(|field| LayoutField {
            name: field.name,
            type_tag: plan_tag(field.type_tag),
            offset: field.offset,
            size: field.size,
        })
        .collect();
    FieldPlan::between(&current, &target)
}
