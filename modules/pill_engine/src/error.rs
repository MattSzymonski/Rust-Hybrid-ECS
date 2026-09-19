//! Typed error system for the ECS engine, generated from one semantic
//! message definition per variant.
//!
//! # Responsibilities
//!
//! - Declare the engine's subsystem errors ([`WorldError`],
//!   [`CommandError`], [`AddComponentError`], [`RemoveComponentError`],
//!   [`BuildError`]).
//! - Compose them transparently into the top-level [`EngineError`] together
//!   with the shared host errors from `pill_core`.
//!
//! # Design
//!
//! Every variant carries a single `#[message(...)]` definition rendered
//! through the `pill_core` diagnostics runtime. Error enums never import a
//! styling crate, never convert sources to text, and compose with `?`
//! through transparent `#[from]` wrappers. Frontends receive the composed
//! [`EngineError`] at their boundary and report it exactly once.

// Standard library
use std::fmt;

// External crates
use pill_core::error::{EngineMessage, MessageRenderer, SemanticRole};
use pill_core_macros::engine_error;

// Current crate
use crate::{archetype::ArchetypeId, ComponentId, Entity, ResourceId};

// =============================================================================
// World Errors
// =============================================================================

/// Storage and migration failures of the archetype world.
///
/// Raised when entity or descriptor-component operations violate storage
/// invariants, such as unregistered IDs, malformed byte layouts, or
/// exceeding the component-type limit.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(PartialEq)]
pub enum WorldError {
    /// The stable ID of a descriptor component cannot be zero.
    #[message("descriptor component stable ID cannot be zero")]
    DescriptorStableIdZero,

    /// The size of a descriptor component cannot be zero.
    #[message("descriptor component size cannot be zero")]
    DescriptorSizeZero,

    /// The alignment of a descriptor component must be a non-zero power of two.
    #[message("descriptor component alignment must be a non-zero power of two")]
    DescriptorAlignmentInvalid,

    /// The size and alignment pair does not form a valid memory layout.
    #[message("descriptor component size and alignment do not form a valid layout")]
    DescriptorLayoutInvalid,

    /// The stable ID is already registered with a different name or schema.
    #[message("descriptor component stable ID is already registered with another name or schema")]
    DescriptorAlreadyRegistered,

    /// A descriptor component cannot be remapped onto itself.
    #[message("descriptor component cannot be remapped onto itself")]
    DescriptorRemapSelf,

    /// The world's component type limit has been reached.
    ///
    /// Registration is driven by user data — a project's compile-time registry
    /// and descriptor manifests from the managed runtime — so exceeding 128 types
    /// is a configuration outcome, not a programming error. The diagnostic
    /// carries the offending type name and the current count so the host can
    /// report it as a normal engine error instead of a bare panic.
    #[message(
        "component type limit exceeded: cannot register ",
        name_style(type_name),
        " (max 128 component types, ",
        debug_value(count),
        " already registered)"
    )]
    ComponentTypeLimitExceeded {
        /// The type that could not be registered.
        type_name: String,
        /// How many component types were already registered.
        count: u8,
    },

    /// A descriptor entity must carry at least one component.
    #[message("a descriptor entity must contain at least one component")]
    DescriptorEntityEmpty,

    /// A descriptor entity cannot contain the same component twice.
    #[message("a descriptor entity cannot contain duplicate components")]
    DescriptorDuplicateComponent,

    /// The component ID was never registered as descriptor storage.
    #[message("descriptor component ", debug_value(id), " is not registered")]
    DescriptorComponentNotRegistered { id: ComponentId },

    /// The supplied bytes do not match the component's registered layout.
    #[message(
        "descriptor component ",
        debug_value(id),
        " byte length does not match its manifest"
    )]
    DescriptorByteLengthMismatch { id: ComponentId },

    /// The entity does not exist in the world.
    #[message("entity not found")]
    EntityNotFound,

    /// The entity already carries the descriptor component being added.
    #[message("entity already contains the descriptor component")]
    DescriptorComponentAlreadyPresent,

    /// The entity does not carry the descriptor component being removed or set.
    #[message("entity does not contain the descriptor component")]
    DescriptorComponentMissing,

    /// A byte copy was rejected by the descriptor storage column.
    #[message("descriptor component row or byte length is invalid")]
    DescriptorRowInvalid,

    /// A byte copy length does not match the registered element size.
    #[message("descriptor component byte length does not match its registered size")]
    DescriptorSizeMismatch,

    /// A raw-byte write was attempted on a column whose rows own resources.
    ///
    /// The byte mutators copy and overwrite rows without consulting the
    /// element's drop glue, which is correct only for plain data. On a column
    /// whose type has a real destructor the same calls would leak the
    /// overwritten value, or hand two columns ownership of one allocation.
    #[message("raw-byte writes are not valid on a column whose rows own resources")]
    ColumnRowsAreNotPlainData,

    /// A registered descriptor component has no storage column in the archetype
    /// the entity was placed in.
    ///
    /// Signals that the manifest and the archetype's columns disagree, which
    /// the manifest-driven registration path can produce if a component is
    /// registered without its storage being created.
    #[message(
        "descriptor component ",
        debug_value(component_id),
        " has no storage column in archetype ",
        debug_value(archetype_id)
    )]
    DescriptorStorageMissing {
        /// The descriptor component whose column is absent.
        component_id: ComponentId,
        /// The archetype that was expected to own the column.
        archetype_id: ArchetypeId,
    },

    /// A descriptor column's element size disagrees with the registered layout
    /// the migration plan was validated against.
    ///
    /// Signals that a column and the storage factory describing it have drifted
    /// apart, which a partially applied relayout used to produce.
    #[message(
        "descriptor component ",
        debug_value(component_id),
        " in archetype ",
        debug_value(archetype_id),
        " has ",
        debug_value(actual),
        " byte elements; the registered layout declares ",
        debug_value(expected)
    )]
    ComponentColumnLayoutMismatch {
        /// The component whose column disagrees.
        component_id: ComponentId,
        /// The archetype owning the column.
        archetype_id: ArchetypeId,
        /// The size the registered layout declares.
        expected: usize,
        /// The size the column's elements are stored at.
        actual: usize,
    },

    /// A foreign resource declaration carries no name to be identified by.
    #[message("a foreign resource must declare a name")]
    ForeignResourceNameEmpty,

    /// The declared layout of a foreign resource cannot describe an allocation.
    #[message(
        "foreign resource layout of ",
        debug_value(size),
        " bytes at alignment ",
        debug_value(align),
        " is not a valid allocation"
    )]
    ForeignResourceLayoutInvalid {
        /// Declared size in bytes.
        size: usize,
        /// Declared alignment in bytes.
        align: usize,
    },

    /// The id does not name a registered foreign resource.
    #[message("resource ", debug_value(id), " is not a registered foreign resource")]
    ForeignResourceNotRegistered {
        /// The resource that was asked for.
        id: ResourceId,
    },

    /// A foreign resource cannot be remapped onto itself.
    #[message("foreign resource cannot be remapped onto itself")]
    ForeignResourceRemapSelf,

    /// The id does not name a registered shared resource.
    #[message("resource ", debug_value(id), " is not a registered shared resource")]
    SharedResourceNotRegistered {
        /// The resource that was asked for.
        id: ResourceId,
    },

    /// The id's registered table belongs to a Rust type, so foreign bytes
    /// cannot be stored under it.
    #[message(
        "resource ",
        debug_value(id),
        " is registered by a Rust type; foreign bytes cannot replace its value"
    )]
    ForeignResourceFactoryIsNative {
        /// The resource the payload was offered to.
        id: ResourceId,
    },

    /// A Rust value is stored under the id, and its type fixes its shape.
    #[message(
        "resource ",
        debug_value(id),
        " holds a Rust value; a foreign declaration cannot reshape it"
    )]
    ForeignResourceHoldsRustValue {
        /// The resource whose value refused the migration.
        id: ResourceId,
    },

    /// The payload length does not match the foreign resource's declared size.
    #[message(
        "foreign resource ",
        debug_value(id),
        " takes ",
        debug_value(expected),
        " bytes; the payload has ",
        debug_value(actual)
    )]
    ForeignResourceBytesMismatch {
        /// The resource the payload was offered to.
        id: ResourceId,
        /// Declared size in bytes.
        expected: usize,
        /// Payload length in bytes.
        actual: usize,
    },

    /// A migration plan for a foreign resource falls outside its rows.
    #[message(
        "the migration plan for foreign resource ",
        debug_value(id),
        " reads or writes past the edge of a row"
    )]
    ForeignResourcePlanOutOfBounds {
        /// The resource the plan was built for.
        id: ResourceId,
    },

    /// A shared id holds a value of another shape, so removing it as `T` is
    /// refused instead of destroying what is there.
    ///
    /// A shared id is derived from a written-down name, which lets a Rust type
    /// that never registered reach another language's slot. The box's own
    /// identity check is what refuses the take; the value, its ticks, its
    /// factory and its claim are left in place, so the caller learns why
    /// instead of losing the resource on the way to `None`.
    #[message(
        "shared resource ",
        debug_value(id),
        " does not hold a ",
        value(requested_type),
        "; a shared id is name-derived, so the removal is refused rather than destroying another type's value"
    )]
    SharedResourceHoldsAnotherType {
        /// The shared id the removal targeted.
        id: ResourceId,
        /// The Rust type the removal asked for.
        requested_type: &'static str,
    },

    /// Two different shared resource names hash to one identity.
    ///
    /// The id of a shared resource is a 128-bit hash of its name, and a hash
    /// collision would make one slot answer to two names - each declaration
    /// silently joining the other's resource. The recorded string is the
    /// evidence the id cannot carry, so it is compared on every claim.
    #[message(
        "shared resource names ",
        name_style(shared_name),
        " and ",
        name_style(existing_name),
        " hash to one identity; rename one of them"
    )]
    SharedResourceIdentityCollision {
        /// The name claiming the id now.
        shared_name: String,
        /// The name that already claimed it.
        existing_name: String,
    },

    /// One shared resource name, two different field shapes.
    ///
    /// Size and alignment cannot tell `{u32, u32}` from `{f32, f32}`; when both
    /// declarations carry a schema hash, the shapes can be compared and a
    /// reinterpretation becomes a refusal instead of a silent misread.
    #[message(
        "shared resource ",
        name_style(shared_name),
        " is declared with two different field shapes (existing hash ",
        value(existing_hash),
        ", incoming hash ",
        value(incoming_hash),
        "); rebuild every artifact that links it against one definition"
    )]
    SharedResourceSchemaMismatch {
        /// The shared name both registrations declared.
        shared_name: String,
        /// Schema hash recorded by the registration that got there first.
        existing_hash: u64,
        /// Schema hash of the type being registered now.
        incoming_hash: u64,
    },

    /// Two live registrations claim the same component type name under
    /// different [`ComponentId`]s.
    ///
    /// The persistable-registration path evicts a same-name entry from the
    /// persist maps on the assumption that it is a superseded hot-reload
    /// generation. A generation that has been superseded has no live rows
    /// left, so when the older column still holds entities the two
    /// registrations are concurrent peers - two binaries that each linked the
    /// same component type and therefore each got their own `TypeId` for it -
    /// and evicting one would silently drop its rows at the next reload.
    ///
    /// The fix is to give the type a shared identity with
    /// `#[pill(shared)]`, which makes both binaries resolve to one column
    /// instead of two.
    #[message(
        "component ",
        name_style(type_name),
        " is registered twice with different type identities (",
        debug_value(existing_id),
        " still holds ",
        value(live_rows),
        " live rows, and ",
        debug_value(incoming_id),
        " is registering now); declare it `#[pill(shared)]` so both registrations bind to one column"
    )]
    ComponentNameCollision {
        /// The type name both registrations claim.
        type_name: String,
        /// The already-registered id whose column still holds rows.
        existing_id: ComponentId,
        /// The id being registered now.
        incoming_id: ComponentId,
        /// How many rows the existing column still holds.
        live_rows: usize,
    },

    /// A persistable component was re-registered by a superseding generation
    /// with a layout the previous generation's column cannot host.
    ///
    /// The migration rebuilds the old column after reading the rows the
    /// incoming generation spawned into it through the incoming type, in
    /// slots spaced for the old layout. A wider alignment - or a stride that
    /// is not a multiple of it - makes those reads misaligned, which aborts
    /// debug hosts and is undefined behaviour in release, so the registration
    /// is refused and the host rolls the reload back instead.
    ///
    /// A size change that keeps the alignment is not refused: the old
    /// column's slots stay validly aligned for the incoming type, and the
    /// add-a-field reloads depend on that shape migrating.
    #[message(
        "component ",
        name_style(type_name),
        " was re-registered with a different size or alignment (existing: ",
        value(existing_size),
        " bytes / ",
        value(existing_align),
        " align; incoming: ",
        value(incoming_size),
        " bytes / ",
        value(incoming_align),
        " align); keeping the running generation"
    )]
    ComponentLayoutChanged {
        /// The type name both registrations claim.
        type_name: String,
        /// Size of the layout the existing column was built for.
        existing_size: usize,
        /// Alignment of that layout.
        existing_align: usize,
        /// Size of the type being registered now.
        incoming_size: usize,
        /// Alignment of the type being registered now.
        incoming_align: usize,
    },

    /// Two different Rust types claim the same shared resource name.
    ///
    /// A shared name is a process-wide identity, so two types holding it are
    /// one resource: one slot, and whichever artifact inserts last replaces
    /// the other's value. When their layouts also agree, reads through either
    /// type succeed and quietly return the other resource's data.
    ///
    /// The legitimate case this must not reject is one type compiled into two
    /// artifacts, which is the entire point of a shared name. Those agree on
    /// the type's own name, while two different types do not.
    #[message(
        "shared resource name ",
        name_style(shared_name),
        " is claimed by two different types (",
        name_style(existing_type),
        " and ",
        name_style(incoming_type),
        "); a shared name is a process-wide identity, so give them distinct names"
    )]
    SharedResourceNameConflict {
        /// The shared name both types declared.
        shared_name: String,
        /// Rust type name that claimed the name first.
        existing_type: String,
        /// Rust type name claiming it now.
        incoming_type: String,
    },

    /// One shared resource name, two different memory layouts.
    ///
    /// A shared resource is reached from every artifact through one slot, and
    /// the only thing establishing that they agree about its contents is this
    /// check - the box's own identity check compares layout, so a mismatch
    /// would misread the value rather than refuse it.
    #[message(
        "shared resource ",
        name_style(shared_name),
        " is registered with two different layouts (existing: ",
        value(existing_size),
        " bytes / ",
        value(existing_align),
        " align; incoming: ",
        value(incoming_size),
        " bytes / ",
        value(incoming_align),
        " align); rebuild every artifact that links it against one definition"
    )]
    SharedResourceLayoutMismatch {
        /// The shared name both registrations declared.
        shared_name: String,
        /// Size recorded by the registration that got there first.
        existing_size: usize,
        /// Alignment recorded by that first registration.
        existing_align: usize,
        /// Size of the type being registered now.
        incoming_size: usize,
        /// Alignment of the type being registered now.
        incoming_align: usize,
    },

    /// Two different Rust types claim the same shared component name.
    ///
    /// A shared name is a process-wide identity, so two types holding it are
    /// one component as far as the engine is concerned: one bit, one column,
    /// and every write through either type landing on the other's rows. When
    /// their layouts also agree, nothing downstream can notice - the reads
    /// succeed and silently return another component's data.
    ///
    /// The legitimate case this must not reject is one type compiled into two
    /// binaries, which is the entire point of shared identity. Those are told
    /// apart by their Rust path: the same type compiled twice reports the same
    /// [`std::any::type_name`], while two different types never do.
    ///
    /// Give the two components distinct names. The default derived from
    /// `module_path!()` is already distinct; this is reachable only by
    /// overriding it with `#[pill(shared = "...")]`.
    #[message(
        "shared component name ",
        name_style(shared_name),
        " is claimed by two different types (",
        name_style(existing_type),
        " and ",
        name_style(incoming_type),
        "); a shared name is a process-wide identity, so give them distinct names"
    )]
    SharedComponentNameConflict {
        /// The shared name both types declared.
        shared_name: String,
        /// Rust path of the type that registered the name first.
        existing_type: String,
        /// Rust path of the type claiming it now.
        incoming_type: String,
    },

    /// Two binaries registered the same shared component with different
    /// memory layouts.
    ///
    /// A shared component is reached from every binary that links it through
    /// one column, and the only thing establishing that they agree about what
    /// a row contains is this check. Binding the second registration anyway
    /// would let one binary read another's rows through the wrong field
    /// offsets, so the registration is refused instead.
    ///
    /// Because identity is resolved at load time rather than by the compiler,
    /// a field reorder or type change compiles cleanly in both binaries and
    /// surfaces here. Rebuild both against the same definition.
    #[message(
        "shared component ",
        name_style(shared_name),
        " is registered with two different layouts (existing: ",
        value(existing_size),
        " bytes / ",
        value(existing_align),
        " align; incoming ",
        name_style(type_name),
        ": ",
        value(incoming_size),
        " bytes / ",
        value(incoming_align),
        " align); rebuild every binary that links it against one definition"
    )]
    SharedComponentLayoutMismatch {
        /// The shared name both registrations declared.
        shared_name: String,
        /// The Rust path of the type being registered now.
        type_name: String,
        /// Size recorded by the registration that got there first.
        existing_size: usize,
        /// Alignment recorded by that first registration.
        existing_align: usize,
        /// Size of the type being registered now.
        incoming_size: usize,
        /// Alignment of the type being registered now.
        incoming_align: usize,
    },

    /// A component type name resolved to more than one registered
    /// [`ComponentId`].
    ///
    /// Name resolution is only meaningful when a name identifies one column.
    /// More than one surviving candidate means
    /// [`WorldError::ComponentNameCollision`] was not enforced somewhere
    /// upstream, so it is reported rather than resolved by an arbitrary
    /// tiebreak that would quietly pick the wrong column.
    #[message(
        "component name ",
        name_style(type_name),
        " resolves to ",
        value(count),
        " different registered components; it must resolve to exactly one"
    )]
    ComponentNameAmbiguous {
        /// The ambiguous type name.
        type_name: String,
        /// How many registered components claim it.
        count: usize,
    },

    /// The archetype recorded for an entity is absent from the world.
    ///
    /// Signals that an entity location outlived the archetype it points at,
    /// which a partially applied hot-reload rehome can produce.
    #[message(
        "entity ",
        debug_value(entity),
        " references archetype ",
        debug_value(archetype_id),
        " which is missing from the world"
    )]
    ArchetypeMissing {
        /// The entity whose recorded location is stale.
        entity: Entity,
        /// The archetype ID that could not be resolved.
        archetype_id: ArchetypeId,
    },
}

/// The refusal logged when two live registrations claim one component type
/// name.
///
/// One source of truth for the sentence the host prints: the
/// [`WorldError::ComponentNameCollision`] record and the log line beside it
/// both point at this constant, so a second guard site cannot drift into its
/// own wording - or its own source-wrap spaces.
pub(crate) const COMPONENT_NAME_COLLISION_REFUSAL: &str =
    "two live registrations claim one component type name; refusing to evict the peer's persist entries";

/// The refusal logged when a reloading generation re-registers a persistable
/// component with a layout the previous generation's column cannot host.
///
/// One source of truth for the sentence the host prints and the migration
/// suite greps for, following [`COMPONENT_NAME_COLLISION_REFUSAL`]: the
/// [`WorldError::ComponentLayoutChanged`] record and the log line beside it
/// both point here.
pub(crate) const COMPONENT_LAYOUT_CHANGED_REFUSAL: &str =
    "was re-registered with a different size or alignment; keeping the running generation";

// =============================================================================
// Command Errors
// =============================================================================

/// Error returned when a deferred command cannot be executed.
///
/// Command errors are non-fatal by default — the engine logs them and
/// continues. Set `Engine::should_exit_on_error` to `true` for strict mode
/// where any command failure stops the frame immediately.
#[engine_error(namespace = engine::commands, runtime = ::pill_core::error)]
pub enum CommandError {
    /// The target entity no longer exists in the world.
    #[message("entity ", debug_value(entity), " not found for ", value(operation))]
    EntityNotFound {
        entity: Entity,
        operation: &'static str,
    },

    /// A queued entity creation could not write one of its components.
    ///
    /// The component was validated when the command was queued, but the world
    /// can change before the queue is flushed - a hot reload that retires a
    /// component type between a managed `CreateEntity` call and the end of the
    /// frame leaves the queued command naming storage that no longer exists.
    /// Reported rather than raised, because a panic here would unwind through
    /// the C ABI for a managed project.
    #[message(
        "entity ",
        debug_value(entity),
        " was created but component ",
        debug_value(component_id),
        " could not be written"
    )]
    ComponentWriteFailed {
        entity: Entity,
        component_id: ComponentId,
    },

    /// The queued creation listed one component id more than once.
    ///
    /// The id set defines the archetype's columns, so a duplicate would push
    /// two rows for one entity row - or panic inside the archetype insert when
    /// it found the id already present. Both are refused here, before the
    /// entity row exists, so a rejected command leaves no partial entity.
    #[message(
        "entity ",
        debug_value(entity),
        " was asked to carry component ",
        debug_value(component_id),
        " twice"
    )]
    DuplicateComponent {
        entity: Entity,
        component_id: ComponentId,
    },

    /// The entity already possesses the component being added.
    #[message(
        "entity ",
        debug_value(entity),
        " already has component ",
        debug_value(component_id)
    )]
    ComponentAlreadyExists {
        entity: Entity,
        component_id: ComponentId,
    },

    /// The entity does not have the component being removed.
    #[message(
        "entity ",
        debug_value(entity),
        " does not have component ",
        debug_value(component_id)
    )]
    ComponentNotFound {
        entity: Entity,
        component_id: ComponentId,
    },

    /// A queued command's archetype migration failed partway through.
    ///
    /// The entity's recorded location referenced an archetype that no longer
    /// exists, or a descriptor component named by an archetype had no storage
    /// column. Those inconsistencies are exactly what a partially applied hot
    /// reload can leave behind, so they are collected like any other command
    /// failure rather than panicking inside the flush - which for a managed
    /// project would unwind across the C ABI.
    #[message("entity ", debug_value(entity), " migration failed: ", value(reason))]
    MigrationFailed { entity: Entity, reason: String },
}

// =============================================================================
// Component Add/Remove/Builder Errors
// =============================================================================

/// Error type for `add_component` operations.
///
/// Returned by the typed component insertion paths when the target entity
/// is missing or already carries the component being added.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AddComponentError {
    /// The entity does not exist (was destroyed or never created).
    #[message("entity not found")]
    EntityNotFound,

    /// The entity already has a component of this type.
    #[message("component already exists on entity")]
    ComponentAlreadyExists,
}

/// Error type for `remove_component` operations.
///
/// Returned by the typed component removal paths when the target entity
/// is missing or does not carry the component being removed.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RemoveComponentError {
    /// The entity does not exist (was destroyed or never created).
    #[message("entity not found")]
    EntityNotFound,

    /// The entity does not have a component of this type.
    #[message("component not found on entity")]
    ComponentNotFound,
}

/// Error type for `EntityBuilder::build` when a component was not registered.
///
/// Returned when the builder writes a component type the world has never
/// seen; each offending type must first be registered with
/// `world.register_component::<T>()`.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// One or more component types were not registered with the world.
    /// Call `world.register_component::<T>()` for each type first.
    #[message(
        "component ",
        debug_value(id),
        " not registered — call world.register_component::<T>() first"
    )]
    ComponentNotRegistered { id: ComponentId },
}

// =============================================================================
// Persistence Errors
// =============================================================================

/// Migration failures of persistable component columns.
///
/// Raised while moving persisted component data between schemas: missing
/// deserializers or storage factories, a column that cannot hand its rows to
/// the destination archetype, or bytes that fail to decode into the new
/// layout.
#[engine_error(namespace = engine::persistence, runtime = ::pill_core::error)]
pub enum PersistenceError {
    /// The component type is not registered in the current world.
    #[message(
        "component type ",
        name_style(type_name),
        " is not registered in the current world"
    )]
    ComponentTypeUnregistered { type_name: String },

    /// The component type name resolves to more than one registered
    /// component, so migration cannot tell which column owns the rows.
    ///
    /// The persistable-registration path evicts superseded same-name entries
    /// precisely so this cannot happen; reaching it means two concurrent peers
    /// registered the name and the collision guard was bypassed.
    #[message(
        "component type ",
        name_style(type_name),
        " resolves to ",
        value(count),
        " registered components, so the rows to migrate are ambiguous"
    )]
    ComponentTypeAmbiguous {
        /// The ambiguous type name.
        type_name: String,
        /// How many registered components claim it.
        count: usize,
    },

    /// No deserializer is registered for the component type.
    #[message(
        "no deserializer is registered for component type ",
        name_style(type_name)
    )]
    DeserializerMissing { type_name: String },

    /// No current serializer is registered for the component type.
    ///
    /// Migration needs it to carry entities the incoming generation spawned
    /// during `init` across a rebuilt column without reinterpreting their
    /// bytes through the retiring generation's layout.
    #[message(
        "no serializer is registered for component type ",
        name_style(type_name)
    )]
    SerializerMissing { type_name: String },

    /// No inserter is registered for the component type.
    #[message("no inserter is registered for component type ", name_style(type_name))]
    InserterMissing { type_name: String },

    /// The persisted bytes could not be decoded into the new schema.
    #[message("deserialization failed for component ", debug_value(component_id))]
    DeserializationFailed { component_id: ComponentId },

    /// The old component column could not be removed from its archetype.
    #[message(
        "removing the old storage of component ",
        debug_value(component_id),
        " failed"
    )]
    StorageRemovalFailed { component_id: ComponentId },

    /// No storage factory is registered for the component.
    #[message(
        "no storage factory is registered for component ",
        debug_value(component_id)
    )]
    StorageFactoryMissing { component_id: ComponentId },

    /// The component has no native Rust storage to migrate.
    #[message(
        "component ",
        debug_value(component_id),
        " has no native Rust storage to migrate"
    )]
    NativeStorageExpected { component_id: ComponentId },

    /// An unchanged column could not hand its row to the destination.
    ///
    /// The cross-archetype migration moves every column it does not rewrite
    /// straight into the destination archetype. A destination that has no
    /// column for one of them, or one whose rows are a different width, is the
    /// registry/storage desync a partially applied reload leaves behind, and
    /// the migration reports it rather than leaving the entity split between
    /// two archetypes.
    #[message(
        "component ",
        debug_value(component_id),
        " could not be moved into the destination archetype"
    )]
    ColumnMoveFailed { component_id: ComponentId },

    /// The destination archetype vanished immediately after creation.
    #[message("destination archetype missing after creation")]
    DestinationArchetypeMissing,
}

// =============================================================================
// System Errors
// =============================================================================

/// Failure reported by one scheduler system during a frame.
///
/// Systems return `Result<(), SystemError>`; the engine records every `Err`
/// with the failing system's name and exposes the batch through
/// [`Engine::drain_system_failures`](crate::Engine::drain_system_failures)
/// for the reporting boundary. A failed system never aborts the remaining
/// batch or the frame.
#[engine_error(namespace = engine::systems, runtime = ::pill_core::error)]
pub enum SystemError {
    /// A resource the system requested is missing from the world.
    #[message("resource ", name_style(name), " is missing from the world")]
    MissingResource { name: String },

    /// A managed-language system reported failure through the interop bridge.
    #[message("managed failure: ", value(message))]
    Managed { message: String },

    /// A system aborted with an arbitrary reported message.
    #[message(value(message))]
    Failure { message: String },
}

/// One failed system execution recorded during a frame.
///
/// Wraps the failing system's name with its [`SystemError`] and renders the
/// pair through the shared semantic protocol, so the host boundary can log
/// `system "name" failed: <error>` with role-aware styling.
#[derive(Debug)]
pub struct SystemFailure {
    /// Registered name of the system that failed.
    pub system: String,
    /// The error the system returned.
    pub error: SystemError,
}

impl SystemFailure {
    /// Pair one failing system name with its error.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_engine::{SystemError, SystemFailure};
    ///
    /// let failure = SystemFailure::new(
    ///     String::from("ball_physics"),
    ///     SystemError::MissingResource {
    ///         name: String::from("SimulationTime"),
    ///     },
    /// );
    /// assert_eq!(
    ///     failure.to_string(),
    ///     "system ball_physics failed: resource SimulationTime is missing from the world"
    /// );
    /// ```
    pub fn new(system: String, error: SystemError) -> Self {
        Self { system, error }
    }
}

impl EngineMessage for SystemFailure {
    fn render_message(&self, renderer: &mut dyn MessageRenderer) -> fmt::Result {
        renderer.text("system ")?;
        renderer.styled(SemanticRole::Name, &self.system)?;
        renderer.text(" failed: ")?;
        self.error.render_message(renderer)
    }
}

impl fmt::Display for SystemFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.to_plain_message())
    }
}

// =============================================================================
// Engine Error Composition
// =============================================================================

/// Transparent composition of every engine subsystem error plus the shared
/// host errors from `pill_core`.
///
/// Subsystem functions return their narrowest meaningful error;
/// [`EngineError`] exists as the composition boundary where `?` crosses
/// subsystems and frontends.
#[engine_error(namespace = engine, runtime = ::pill_core::error)]
pub enum EngineError {
    /// A world storage or migration operation failed.
    #[transparent]
    World(#[from] WorldError),

    /// A deferred command could not be applied.
    #[transparent]
    Commands(#[from] CommandError),

    /// A persisted component migration failed.
    #[transparent]
    Persistence(#[from] PersistenceError),

    /// A subsystem of the shared host crate failed.
    #[transparent]
    Host(#[from] ::pill_core::error::HostError),
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use miette::Diagnostic as _;

    /// Plain rendering preserves the semantic values of a world error.
    #[test]
    fn world_error_renders_plain_with_values() {
        let error = WorldError::DescriptorComponentNotRegistered {
            id: ComponentId::descriptor(7),
        };
        assert!(error.to_plain_message().contains("descriptor component"));
        assert!(error.to_plain_message().contains("is not registered"));
    }

    /// Command errors derive their diagnostic code from namespace and variant.
    #[test]
    fn command_error_code_derives_from_namespace_and_variant() {
        let error = CommandError::EntityNotFound {
            entity: Entity::new_for_test(3, 1),
            operation: "destroy",
        };
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("engine::commands::entity_not_found")
        );
    }

    /// Transparent composition preserves the leaf message end to end.
    #[test]
    fn engine_error_composition_keeps_the_leaf_message() {
        let world_error = WorldError::EntityNotFound;
        let engine_error: EngineError = world_error.into();
        assert_eq!(engine_error.to_string(), "entity not found");
        assert_eq!(engine_error.to_plain_message(), "entity not found");
    }

    /// The entity builder error keeps its Copy semantics across composition.
    #[test]
    fn build_error_remains_copy() {
        let error = BuildError::ComponentNotRegistered {
            id: ComponentId::descriptor(9),
        };
        let copied = error;
        assert_eq!(error, copied);
    }

    /// System failures render the failing system's name with its error.
    #[test]
    fn system_failure_renders_name_and_leaf_message() {
        let failure = SystemFailure::new(
            String::from("ball_physics"),
            SystemError::MissingResource {
                name: String::from("SimulationTime"),
            },
        );
        assert_eq!(
            failure.to_plain_message(),
            "system ball_physics failed: resource SimulationTime is missing from the world"
        );
        assert_eq!(failure.to_string(), failure.to_plain_message());
    }

    /// Managed failures keep the managed-reported message verbatim.
    #[test]
    fn managed_system_error_keeps_the_managed_message() {
        let error = SystemError::Managed {
            message: String::from("index out of range"),
        };
        assert_eq!(
            error.to_plain_message(),
            "managed failure: index out of range"
        );
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("engine::systems::managed")
        );
    }
}
