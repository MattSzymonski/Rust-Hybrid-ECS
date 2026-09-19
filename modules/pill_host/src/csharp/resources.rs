//! Managed resource registration and the callback that serves resource bytes.
//!
//! # Responsibilities
//!
//! - Registers every resource a managed manifest declares as a foreign engine
//!   resource, and records the binding managed code reaches it through.
//! - Serves [`ResourceView`]s to `Res<T>` and `ResMut<T>`, under the same scope
//!   and access-declaration rules the query callbacks obey.
//! - Supplies the resource half of the shared reload pipeline, as
//!   [`RESOURCE_SUBJECT`]; the stages it runs through live in
//!   [`manifest_apply`](super::manifest_apply).
//!
//! # Design
//!
//! A managed resource is blittable bytes with a declared name, which is exactly
//! what [`World::register_foreign_resource`] already accepts - so nothing new
//! is invented here. The manifest carries resources in the same array as
//! components, distinguished by a `kind` tag, so one transfer and one
//! transaction cover both.
//!
//! The binding table is a process-wide `RwLock` rather than a per-invocation
//! pointer, unlike [`ComponentBindings`](super::components::ComponentBindings).
//! That is not a shortcut: a resource table is genuinely process-wide - there
//! is one value per world, not one per archetype - and the mirror-method table
//! next door is published the same way for the same reason. It also keeps the
//! scope struct, and every installer that fills it, unchanged.
//!
//! A reload does not have its own apply loop either. The rename/retire/rollback
//! protocol is the components' protocol, run from one place; what is
//! resource-specific is that a resource owns no column, so its migration moves
//! a stored value rather than rewriting a column - and that the move cannot be
//! reversed, which the subject says by journalling no undo for it.

// Standard library
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock, RwLockReadGuard};

// External crates
use pill_core::error::{CSharpError, EngineMessage};
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info, warn};
use pill_engine::archetype::{FieldPlan, LayoutField};
use pill_engine::component_registry::ComponentFieldDescriptor;
use pill_engine::{Engine, ResourceId, World};

// Current crate
use super::abi::ResourceView;
use super::components::{stable_component_id, StableComponentId};
use super::context::{resource_access_is_authorized, with_active_world, ACCESS_KIND_RESOURCE};
use super::manifest::intern;
use super::manifest_apply::{
    apply_manifest, ManifestOutcome, ManifestSubject, RenamePredecessor, Settlement,
};

// =============================================================================
// Types
// =============================================================================

/// One managed resource bound to its engine registration.
///
/// Every managed resource is foreign storage: the host has no Rust type for it,
/// so there is only one variant here where components have three.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(super) struct ResourceBinding {
    /// Engine id the resource was registered under.
    pub(super) resource_id: ResourceId,
    /// Declared name the registration was made under, kept so a retirement
    /// can report what the manifest stopped naming.
    pub(super) name: String,
    /// Width of the declared layout in bytes.
    pub(super) size: usize,
    /// Alignment of the declared layout in bytes.
    ///
    /// Remembered beside the width because a rollback re-registers the
    /// predecessor from this record alone, and the engine needs both.
    pub(super) align: usize,
    /// Hash of the managed field schema the registration was made with.
    ///
    /// Carried here rather than read back from the engine so a reload can tell
    /// a resource whose fields changed from one whose bytes merely moved; size
    /// and alignment can agree while the shape underneath them does not.
    pub(super) schema_hash: u64,
    /// The field layout this registration declared.
    ///
    /// The engine stores no field table for a foreign resource - a column has
    /// one so the editor can show its rows, and a resource has no rows - so the
    /// outgoing layout is remembered here. It is the only record of where each
    /// field used to sit, and a reload's byte plan is measured against it.
    pub(super) fields: Vec<ResourceFieldLayout>,
}

/// One field of a managed resource, as the manifest described it.
///
/// Owned strings rather than the interned `&'static str` the component
/// descriptors use: a resource layout is remembered for exactly as long as its
/// binding, so there is nothing to intern it against.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(super) struct ResourceFieldLayout {
    /// Field name, which is what a reload matches the two layouts on.
    pub(super) name: String,
    /// Engine type tag, already folded to what a migration plan compares.
    pub(super) type_tag: String,
    /// Byte offset of the field within the resource.
    pub(super) offset: usize,
    /// Byte width of the field.
    pub(super) size: usize,
    /// Bytes a declared default writes for this field when a reload adds or
    /// resets it, already parsed against the field's declared type.
    pub(super) default: Option<Vec<u8>>,
}

/// Every managed resource, keyed by the stable identity of its full name.
pub(super) type ResourceBindings = HashMap<StableComponentId, ResourceBinding>;

/// The live resource table, shared by the registration path and the callback.
static RESOURCE_BINDINGS: OnceLock<RwLock<ResourceBindings>> = OnceLock::new();

/// Borrow the table, recovering a poisoned lock as the value it guarded.
///
/// A panic while the lock is held cannot leave the map wrong - it is plain data
/// and a writer completes or abandons one whole entry - so poisoning is
/// reported as the map rather than becoming a new failure mode for every later
/// resource access. The same rule `BindingStore` applies.
fn table() -> &'static RwLock<ResourceBindings> {
    RESOURCE_BINDINGS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Read the resource table.
fn read_table() -> RwLockReadGuard<'static, ResourceBindings> {
    table()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// =============================================================================
// Registration
// =============================================================================

/// Register every resource the managed manifest declares.
///
/// Called with the resource entries a manifest parse separated out, at the
/// point the component entries are registered, so one generation's whole
/// declaration lands in one transaction.
///
/// Each resource is registered twice on purpose: once as a foreign resource, so
/// the scheduler and managed code can reach it, and once as persistable, so it
/// appears in a snapshot. A managed resource is the case persistence was least
/// able to reach before - there is no Rust type to derive anything from - and
/// its bytes are exactly what a snapshot can carry.
///
/// # Errors
///
/// Returns a [`CSharpError`] when a declared layout cannot describe an
/// allocation, when the name is already claimed by a Rust resource of a
/// different shape, or when a Rust value is already stored under the identity.
pub(super) fn register_resource_manifest(
    engine: &mut Engine,
    resources: &[ManagedResourceDeclaration],
) -> Result<(), CSharpError> {
    let mut bindings = ResourceBindings::new();
    for resource in resources {
        let resource_id = register_one(engine, resource)?;
        bindings.insert(
            stable_component_id(&resource.full_name),
            binding_for(resource_id, resource),
        );
    }
    publish_resource_bindings(bindings);
    Ok(())
}

/// Replace the live resource table with one generation's bindings.
///
/// Replaced rather than merged: a reload's manifest is the complete list of
/// what the arriving generation declares, so a resource it dropped must stop
/// resolving. The engine keeps the stored value either way - `drop_resources`
/// is what releases a retired one - so this only closes the managed door.
fn publish_resource_bindings(bindings: ResourceBindings) {
    let mut live = table()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *live = bindings;
}

/// Empty the live table, so one test's registrations cannot reach another's.
///
/// The table is process-wide by design - there is one resource value per world,
/// not one per archetype - which is right in a host and wrong under `cargo
/// test`, where every test shares one process and cargo runs them on parallel
/// threads. Tests that touch it take a shared lock and start by calling this;
/// see `RESOURCE_TABLE_LOCK` in the test module, which is gated on `rendering`
/// for the same reason the rest of that module is - its fixtures are the
/// renderer's components.
#[cfg(all(test, feature = "rendering"))]
pub(super) fn reset_resource_bindings_for_test() {
    publish_resource_bindings(ResourceBindings::new());
}

/// The engine id and byte width of one bound resource.
///
/// The hot path wants only these two, and cloning a binding to read them would
/// copy the field layout on every `Res<T>` access.
pub(super) fn resource_target(key: StableComponentId) -> Option<(ResourceId, usize)> {
    read_table()
        .get(&key)
        .map(|binding| (binding.resource_id, binding.size))
}

/// Apply a reloaded generation's resource declarations to the live world.
///
/// Three outcomes per resource, decided by comparing the arriving declaration
/// with the binding the previous generation left behind:
///
/// - **unchanged** - same schema hash, so the stored bytes are still valid
///   under the arriving struct and nothing is touched, which is what keeps a
///   resource's value across an ordinary reload;
/// - **added** - no binding yet, so it is registered exactly as at startup;
/// - **reshaped** - the schema hash moved, so the stored bytes are migrated
///   field by field into the new layout, matched by name.
///
/// What this path cannot do at all - a resource entry an alias claims as
/// someone else's past - is refused before anything is applied; a declaration
/// the manifest merely stopped naming is retired after the rest of the apply,
/// through `drop_resources`, which skips a value another subject still claims.
/// What can still fail while
/// applying is handled differently from the component path, which unwinds an
/// undo journal: a relayout moves bytes into a narrower shape, so reversing it
/// would invent the bytes it dropped. Instead the binding table is published on
/// every exit, so it never describes a layout the engine no longer holds, and
/// the failure is logged as one that needs a host restart.
///
/// # Errors
///
/// Returns a [`CSharpError`] when the engine refuses a registration or a
/// relayout, or when one entry collects predecessors through more than one
/// alias.
/// Apply a reloaded generation's resource declarations to the live world.
///
/// Five outcomes per resource, decided by the shared pipeline from the arriving
/// declaration and the binding the previous generation left behind:
///
/// - **unchanged** - the schema hash matches, so the stored bytes are still
///   valid under the arriving struct and nothing is touched, which is what
///   keeps a resource's value across an ordinary reload;
/// - **added** - no binding yet, so it registers exactly as at startup;
/// - **reshaped** - the schema hash moved, so the stored bytes migrate field by
///   field into the new layout, matched by name;
/// - **renamed** - an alias claims a binding still in the table, so the value
///   and the claims move onto the successor and the predecessor is dropped;
/// - **retired** - the manifest stopped naming it and no alias claimed it, so
///   its value and binding go.
///
/// The ordering that makes those safe - aliases first, retirement last - is the
/// pipeline's, and it is the same ordering managed components run through.
///
/// # Errors
///
/// Returns a [`CSharpError`] when an alias is malformed or claims more than one
/// predecessor, and when the engine refuses a registration, a relayout or a
/// remap.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) fn apply_resource_manifest_on_reload(
    engine: &mut Engine,
    resources: &[ManagedResourceDeclaration],
) -> Result<ManifestOutcome, CSharpError> {
    let previous = read_table().clone();
    let (result, table) = apply_manifest(&RESOURCE_SUBJECT, engine, previous, resources);
    // The table is published on every exit, the failing one included. A reshape
    // that already ran cannot be undone - reversing it would invent the bytes
    // it dropped - so the pipeline leaves it applied, and publishing is what
    // keeps the table describing the layout the engine actually holds.
    publish_resource_bindings(table);
    result
}

// =============================================================================
// The resource manifest subject
// =============================================================================

/// One applied resource entry, recorded so a later refusal can be undone.
///
/// Only a rename appears here. An addition leaves a registration the engine
/// keeps and the table correctly describes, and a reshape moves bytes into a
/// possibly narrower shape whose inverse would have to invent what it dropped;
/// neither is reversed, which is why neither has a variant.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) enum ResourceUndo {
    /// The entry renamed a resource from an earlier declaration.
    Renamed {
        /// The registration the rename retired, to be re-registered.
        predecessor: ResourceBinding,
        /// The successor's id, whose value moves back to the predecessor.
        successor_id: ResourceId,
        /// The shape the successor was registered with, which is the source
        /// side of the plan that moves the value back.
        successor_fields: Vec<ResourceFieldLayout>,
    },
}

/// The actions the shared apply pipeline needs for managed resources.
///
/// A resource owns no column and no archetype, so its "reshape" is a value
/// migration where a component's is a column rewrite - which is exactly the
/// kind of difference the subject exists to carry.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
pub(super) const RESOURCE_SUBJECT: ManifestSubject<
    ManagedResourceDeclaration,
    ResourceBinding,
    ResourceUndo,
> = ManifestSubject {
    kind: "resource",
    identity: |resource| stable_component_id(&resource.full_name),
    name: |resource| resource.full_name.as_str(),
    aliases: |resource| &resource.aliases,
    binding_name: |_engine, binding| binding.name.clone(),
    // Every entry in the resource table is a managed declaration: unlike
    // components, there is no native lane sharing the map.
    is_governed: |_binding| true,
    // A resource alias names a managed declaration, and the identity of one is
    // the hash of its name, so the table answers directly - no engine lookup is
    // needed or would be meaningful.
    resolve_alias: |_engine, table, alias| {
        let predecessor = stable_component_id(alias);
        Ok(table.contains_key(&predecessor).then_some(predecessor))
    },
    // A declared resource is always registrable: it carries its own layout and
    // needs no counterpart on the Rust side.
    check_addable: |_resource| Ok(()),
    settle: |_engine, binding, resource| {
        Ok(if binding.schema_hash == resource.schema_hash {
            Settlement::Unchanged
        } else {
            Settlement::Reshaped
        })
    },
    register: |engine, _identity, resource| {
        let resource_id = register_one(engine, resource)?;
        Ok((binding_for(resource_id, resource), None))
    },
    reshape: |engine, _identity, existing, resource| match relayout_one(engine, existing, resource)
    {
        Ok(()) => Ok((binding_for(existing.resource_id, resource), None)),
        Err(error) => {
            // The arriving assembly is refused after this returns, so the
            // generation still running holds the old struct for a resource
            // whose engine layout may already have moved. Nothing here can put
            // that back, so it is said out loud rather than left to be
            // discovered as a wrong-looking value.
            error!(
                target: telemetry_target::HOT_RELOAD,
                resource = %resource.full_name,
                error = %error.to_plain_message(),
                "managed resource migration failed part-way; restart the host before trusting resource values"
            );
            Err(error)
        }
    },
    rename: rename_resource_entry,
    retire: retire_resources,
    undo: undo_resource_entry,
};

/// Register the successor of a renamed resource and move its value across.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn rename_resource_entry(
    engine: &mut Engine,
    _identity: StableComponentId,
    predecessor: &RenamePredecessor<'_, ResourceBinding>,
    resource: &ManagedResourceDeclaration,
) -> Result<(ResourceBinding, Option<ResourceUndo>), CSharpError> {
    let resource_id = register_one(engine, resource)?;
    let plan = resource_field_plan(&predecessor.binding.fields, &resource.fields);
    let retyped = plan.retyped_fields().to_vec();
    engine
        .world_mut()
        .remap_foreign_resource(predecessor.binding.resource_id, resource_id, &plan)
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    if !retyped.is_empty() {
        warn!(
            target: telemetry_target::HOT_RELOAD,
            resource = %resource.full_name,
            fields = %retyped.join(", "),
            "managed resource fields changed type and were reset to their zero bytes"
        );
    }
    info!(
        target: telemetry_target::HOT_RELOAD,
        resource = %resource.full_name,
        predecessor = %predecessor.alias,
        "managed resource renamed and its value migrated"
    );
    Ok((
        binding_for(resource_id, resource),
        Some(ResourceUndo::Renamed {
            predecessor: predecessor.binding.clone(),
            successor_id: resource_id,
            successor_fields: resource.fields.clone(),
        }),
    ))
}

/// Drop the resources the manifest stopped naming.
///
/// `drop_resources` skips an id another subject still claims, so a shared
/// resource a module keeps declaring loses only this subject's binding; the
/// drop itself runs while the assembly that declared the value is still mapped,
/// which is the timing the engine requires.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn retire_resources(engine: &mut Engine, bindings: &[ResourceBinding]) -> usize {
    let retired: Vec<ResourceId> = bindings.iter().map(|binding| binding.resource_id).collect();
    engine.world_mut().drop_resources(&retired)
}

/// Move a renamed resource's value back onto its predecessor, best effort.
///
/// A rollback that fails leaves the process mixed, so the failure is logged
/// with the resource it concerns rather than raised over the original refusal.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn undo_resource_entry(engine: &mut Engine, undo: ResourceUndo) {
    let ResourceUndo::Renamed {
        predecessor,
        successor_id,
        successor_fields,
    } = undo;
    let declaration = ManagedResourceDeclaration {
        full_name: predecessor.name.clone(),
        size: predecessor.size,
        align: predecessor.align,
        schema_hash: predecessor.schema_hash,
        aliases: Vec::new(),
        fields: predecessor.fields.clone(),
    };
    let restored_id = match register_one(engine, &declaration) {
        Ok(resource_id) => resource_id,
        Err(error) => {
            error!(
                target: telemetry_target::HOT_RELOAD,
                resource = %predecessor.name,
                error = %error.to_plain_message(),
                "could not re-register a renamed resource's predecessor during rollback"
            );
            return;
        }
    };
    // The inverse plan: the successor's declared shape is the source and the
    // predecessor's remembered one is the destination.
    let plan = resource_field_plan(&successor_fields, &predecessor.fields);
    if let Err(error) = engine
        .world_mut()
        .remap_foreign_resource(successor_id, restored_id, &plan)
    {
        error!(
            target: telemetry_target::HOT_RELOAD,
            resource = %predecessor.name,
            error = %error.to_plain_message(),
            "could not move a renamed resource's value back during rollback"
        );
        return;
    }
    info!(
        target: telemetry_target::HOT_RELOAD,
        resource = %predecessor.name,
        "rolled back a managed resource rename"
    );
}

/// Register one declared resource and opt it into snapshots.
fn register_one(
    engine: &mut Engine,
    resource: &ManagedResourceDeclaration,
) -> Result<ResourceId, CSharpError> {
    let resource_id = engine
        .world_mut()
        .register_foreign_resource(
            &resource.full_name,
            &resource.full_name,
            resource.size,
            resource.align,
            resource.schema_hash,
        )
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    // Opting the resource into snapshots here rather than at the manifest site
    // keeps the two records of "this resource exists" together: a registration
    // the engine accepted is the only one worth persisting.
    engine
        .world_mut()
        .register_persistable_foreign_resource(&resource.full_name);
    publish_field_layout(engine, resource_id, resource);

    // Seed the value, because a managed declaration is all there is: nothing
    // on the managed side inserts a resource the way a Rust `init` calls
    // `insert_resource`, so without this the first `Res<T>` access would find
    // the resource registered and empty. Zero is the right seed for the same
    // reason a descriptor component's new field is zeroed - the vocabulary is
    // blittable, so every declared shape has a defined all-zero value, and C#
    // itself gives a fresh struct exactly that.
    //
    // Only when nothing is stored: a reload re-registers what it already owns,
    // and reseeding there would wipe the value the reload exists to preserve.
    if engine.world().foreign_resource_bytes(resource_id).is_none() {
        engine
            .world_mut()
            .insert_foreign_resource_bytes(resource_id, &vec![0_u8; resource.size])
            .map_err(|error| CSharpError::ManifestInvalid {
                message: error.to_plain_message(),
            })?;
    }
    info!(
        target: telemetry_target::HOT_RELOAD,
        resource = %resource.full_name,
        size = resource.size,
        align = resource.align,
        "managed resource registered"
    );
    Ok(resource_id)
}

/// Move one resource's stored bytes from the layout it has to the one the
/// arriving manifest declares.
///
/// Fields are matched by name, so a field that moved follows its name, a field
/// the new shape adds starts zeroed rather than reading whatever was next in
/// memory, and a field the new shape dropped is left behind. Exactly the rule a
/// reshaped managed component's rows follow, because it is the same plan.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
/// Build the byte plan from one resource shape to another, with declared
/// defaults applied to the fields the diff leaves empty.
///
/// Shared by a reshape and a rename because the two differ only in which id
/// the plan runs against; `set_default` is a no-op on a copied field, which is
/// what keeps a changed default from masking a carried value.
fn resource_field_plan(
    previous: &[ResourceFieldLayout],
    arriving: &[ResourceFieldLayout],
) -> FieldPlan {
    let previous_fields: Vec<LayoutField<'_>> = previous.iter().map(layout_field).collect();
    let next_fields: Vec<LayoutField<'_>> = arriving.iter().map(layout_field).collect();
    let mut plan = FieldPlan::between(&previous_fields, &next_fields);
    for field in arriving {
        let Some(bytes) = &field.default else {
            continue;
        };
        plan.set_default(field.offset, bytes)
            .expect("the field default was validated against the planned resource field");
    }
    plan
}

fn relayout_one(
    engine: &mut Engine,
    existing: &ResourceBinding,
    arriving: &ManagedResourceDeclaration,
) -> Result<(), CSharpError> {
    let plan = resource_field_plan(&existing.fields, &arriving.fields);
    let retyped = plan.retyped_fields().to_vec();
    engine
        .world_mut()
        .relayout_foreign_resource(
            existing.resource_id,
            arriving.size,
            arriving.align,
            arriving.schema_hash,
            &plan,
        )
        .map_err(|error| CSharpError::ManifestInvalid {
            message: error.to_plain_message(),
        })?;
    // A retyped field keeps its name and changes its meaning, so its old bytes
    // are reset rather than reinterpreted. Reported because that is data the
    // project loses, and a silent reset is how a reload quietly zeroes a value
    // someone was watching.
    if !retyped.is_empty() {
        warn!(
            target: telemetry_target::HOT_RELOAD,
            resource = %arriving.full_name,
            fields = ?retyped,
            "managed resource fields changed type; their values were reset"
        );
    }
    // The engine dropped the stored layout with the shape it described, so the
    // arriving one is published here rather than left to the next startup.
    publish_field_layout(engine, existing.resource_id, arriving);
    info!(
        target: telemetry_target::HOT_RELOAD,
        resource = %arriving.full_name,
        size = arriving.size,
        "managed resource migrated to a new layout"
    );
    Ok(())
}

/// Hand the engine the field layout this declaration carries.
///
/// A foreign resource's bytes are opaque without it, so this is what makes one
/// inspectable - the resource twin of the field layout a managed component's
/// registration publishes. Reported rather than propagated when the engine
/// refuses it: inspectability is not worth failing a reload over, and the
/// resource itself is already registered and usable by then.
///
/// Called again after a relayout, because the engine drops the stored layout
/// when the shape moves rather than serve the old offsets over new bytes.
fn publish_field_layout(
    engine: &mut Engine,
    resource_id: ResourceId,
    resource: &ManagedResourceDeclaration,
) {
    if resource.fields.is_empty() {
        return;
    }
    let descriptors: Vec<ComponentFieldDescriptor> = resource
        .fields
        .iter()
        .map(|field| ComponentFieldDescriptor {
            name: intern(&field.name),
            type_tag: intern(&field.type_tag),
            offset: field.offset,
            size: field.size,
            // A managed resource's fields are leaves of the blittable
            // vocabulary, so alignment is the field's own width and there is
            // no array arity to carry.
            align: field.size.max(1),
            element_count: 0,
        })
        .collect();
    if let Err(error) = engine
        .world_mut()
        .register_resource_field_layout(resource_id, descriptors)
    {
        warn!(
            target: telemetry_target::HOT_RELOAD,
            resource = %resource.full_name,
            error = %error,
            "managed resource field layout refused; the resource works but is not inspectable"
        );
    }
}

/// Borrow one recorded field as the plan's layout view.
#[cfg_attr(not(feature = "hot_reload"), allow(dead_code))]
fn layout_field(field: &ResourceFieldLayout) -> LayoutField<'_> {
    LayoutField {
        name: field.name.as_str(),
        type_tag: field.type_tag.as_str(),
        offset: field.offset,
        size: field.size,
    }
}

/// Build the table entry for one accepted declaration.
fn binding_for(resource_id: ResourceId, resource: &ManagedResourceDeclaration) -> ResourceBinding {
    ResourceBinding {
        resource_id,
        name: resource.full_name.clone(),
        size: resource.size,
        align: resource.align,
        schema_hash: resource.schema_hash,
        fields: resource.fields.clone(),
    }
}

// =============================================================================
// Manifest Entry
// =============================================================================

/// One resource entry taken from the managed manifest.
///
/// A resource's manifest entry is a component's minus the parts that only mean
/// something for a column, so it is carried as its own small struct rather than
/// reusing the component entry and leaving fields unread.
#[derive(Debug, Clone)]
pub(super) struct ManagedResourceDeclaration {
    /// Full managed type name, which is also the resource's engine name.
    pub(super) full_name: String,
    /// Width of the declared layout in bytes.
    pub(super) size: usize,
    /// Alignment of the declared layout in bytes.
    pub(super) align: usize,
    /// Hash of the managed field schema.
    pub(super) schema_hash: u64,
    /// Names this resource used to be declared under; a rename migrates the
    /// stored value instead of refusing the manifest.
    pub(super) aliases: Vec<String>,
    /// The declared field layout, used to plan a reload's byte migration.
    pub(super) fields: Vec<ResourceFieldLayout>,
}
// =============================================================================
// FFI
// =============================================================================

/// Serve one resource's bytes to a managed `Res<T>` or `ResMut<T>`.
///
/// Status codes, which the managed side turns into messages:
/// `0` filled the view, `1` the identity names no registered resource,
/// `2` the active system did not declare this access, `3` no managed system is
/// running on this thread, `4` the world holds no value for the resource yet,
/// and `5` the caller passed no output buffer.
///
/// The order matters: scope is checked before the table, so a call from outside
/// a system reports that rather than reporting a resource it could not have
/// reached anyway.
fn ffi_get_resource_view_guarded(
    stable_id_low: u64,
    stable_id_high: u64,
    mode: u8,
    output: *mut ResourceView,
) -> u8 {
    if output.is_null() {
        return 5;
    }
    let key = StableComponentId::from_halves(stable_id_low, stable_id_high);
    // A write declaration permits reads as well; the authorization check owns
    // that rule, and an unknown mode is refused by it rather than here.
    match resource_access_is_authorized(key, mode) {
        None => return 3,
        Some(false) => return 2,
        Some(true) => {}
    }
    let Some((resource_id, declared_size)) = resource_target(key) else {
        return 1;
    };
    let status = with_active_world(|world| {
        fill_resource_view(world, resource_id, declared_size, mode, output)
    });
    status.unwrap_or(3)
}

/// Panic-guarded entry point for [`ffi_get_resource_view_guarded`].
///
/// A panic crossing an `extern "C"` boundary aborts the process. The
/// managed side already has a per-system error channel built for a
/// misbehaving system, so an engine assertion reached through this
/// callback reports `5` - the status this callback already
/// uses for a call it could not serve - and lets the frame's error path
/// name the system, rather than killing the host mid-frame.
pub(super) extern "C" fn ffi_get_resource_view(
    stable_id_low: u64,
    stable_id_high: u64,
    mode: u8,
    output: *mut ResourceView,
) -> u8 {
    super::context::guard_managed_callback(
        "ffi_get_resource_view",
        5,
        || ffi_get_resource_view_guarded(stable_id_low, stable_id_high, mode, output),
    )
}

/// Write one resource's live bytes into the caller's view.
///
/// A read takes the immutable byte view; a write takes the mutable one, which
/// stamps the resource's change tick as it is handed out. That is deliberate
/// and matches the engine's own rule: managed code holds raw bytes with no
/// wrapper that could notice a mutation, so the borrow is the last moment a
/// change can be recorded.
fn fill_resource_view(
    world: &mut World,
    resource_id: ResourceId,
    declared_size: usize,
    mode: u8,
    output: *mut ResourceView,
) -> u8 {
    let scope_token = super::context::active_scope_token();
    let view = if mode == 0 {
        let Some(bytes) = world.foreign_resource_bytes(resource_id) else {
            return 4;
        };
        // The binding and the engine's allocation are two records of one
        // layout, and two records can drift. Asserted where both are in hand,
        // so a path that relayouts the resource without updating the table
        // fails in debug builds at its source; the view itself always carries
        // the live length, so managed code cannot mis-stride either way.
        debug_assert_eq!(
            bytes.len(),
            declared_size,
            "resource binding matches storage"
        );
        ResourceView {
            // The cast drops the immutability the engine granted, which is the
            // one place this file relies on the managed contract rather than on
            // the type system: a `Res<T>` hands out `ref readonly`, and the
            // analyzer refuses a system that writes through one. The same
            // trade-off the component chunk path already makes for a read term.
            data: bytes.as_ptr().cast_mut(),
            length: bytes.len() as u32,
            scope_token,
        }
    } else {
        let Some((bytes, _ticks)) = world.foreign_resource_bytes_mut(resource_id) else {
            return 4;
        };
        debug_assert_eq!(
            bytes.len(),
            declared_size,
            "resource binding matches storage"
        );
        ResourceView {
            data: bytes.as_mut_ptr(),
            length: bytes.len() as u32,
            scope_token,
        }
    };
    // SAFETY: `output` was checked non-null by the FFI entry point and points
    // at a caller-owned `ResourceView` for the duration of this call; the
    // pointer written into it borrows engine storage that stays put for the
    // rest of this managed invocation, which is exactly as long as the scope
    // token the managed side validates against remains current.
    unsafe { output.write(view) };
    0
}

// =============================================================================
// Access Reflection
// =============================================================================

/// Translate one reflected resource access into scheduler metadata.
///
/// Returns the engine id the access refers to, so the caller can add it to a
/// [`SystemAccess`](pill_engine::SystemAccess) as a read or a write.
///
/// # Errors
///
/// Returns [`CSharpError::UnregisteredResource`] when the key names no
/// registered resource, which means a system declared `Res<T>` for a type the
/// manifest never carried - almost always a missing `[EcsResource]`.
pub(super) fn resolve_resource_access(
    stable_id_low: u64,
    stable_id_high: u64,
) -> Result<ResourceId, CSharpError> {
    let key = StableComponentId::from_halves(stable_id_low, stable_id_high);
    resource_target(key)
        .map(|(resource_id, _size)| resource_id)
        .ok_or_else(|| CSharpError::UnregisteredResource {
            key: format!("{stable_id_high:016X}{stable_id_low:016X}"),
        })
}

/// The discriminator a resource access carries in the reflected list.
///
/// Re-exported here so the backend reads it from the module that owns the
/// meaning rather than from the scope context it happens to be defined in.
pub(super) const RESOURCE_ACCESS_KIND: u8 = ACCESS_KIND_RESOURCE;
