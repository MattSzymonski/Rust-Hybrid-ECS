//! Deferred command queue for structural ECS mutations.
//!
//! # Responsibilities
//!
//! - Provides [`Commands`] - a deferred-operation queue for creating/destroying
//!   entities and adding/removing components without holding `&mut World`.
//! - Implements the two-phase frame lifecycle: systems queue commands during
//!   execution, then the engine applies them after all systems finish.
//! - Defines [`CommandError`] for queue-execution failures.
//! - Provides [`DeferredEntityBuilder`] for ergonomic entity construction via commands.
//!
//! # Design
//!
//! The ECS uses a two-phase execution model each frame:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        FRAME N                              │
//! ├─────────────────────────────────┬───────────────────────────┤
//! │      Phase 1: System Execution  │  Phase 2: Command Apply   │
//! │                                 │                           │
//! │  ┌──────────┐  ┌──────────┐     │  Commands from Phase 1    │
//! │  │System A  │  │System B  │     │  are now executed:        │
//! │  │(parallel)│  │(parallel)│     │                           │
//! │  └────┬─────┘  └────┬─────┘     │  - Create entities        │
//! │       │             │           │  - Destroy entities       │
//! │       ▼             ▼           │  - Add/remove components  │
//! │  ┌──────────────────────┐       │                           │
//! │  │   Command Queue      │──────►│  World is now consistent  │
//! │  │   (deferred ops)     │       │  for next frame           │
//! │  └──────────────────────┘       │                           │
//! └─────────────────────────────────┴───────────────────────────┘
//! ```
//!
//! ## Why Deferred?
//!
//! 1. Thread Safety: Multiple systems can queue commands without locks
//! 2. Consistency: World state doesn't change mid-iteration
//! 3. Batching: Commands can be optimized before execution
//!
//! ## Usage Example
//!
//! ```no_run
//! # use pill_engine::*;
//! # #[derive(Debug, Clone)] struct Health { current: f32 }
//! # impl Component for Health {}
//! fn combat_system(mut query: Query<(&Health, Entity)>, mut commands: Commands) {
//!     for (health, entity) in query.iter_mut() {
//!         if health.current <= 0.0 {
//!             // Queue for destruction - doesn't happen immediately!
//!             commands.destroy_entity(entity);
//!         }
//!     }
//! }
//! // After ALL systems run, the engine calls commands.execute_queued_commands()
//! // and the dead entities are actually removed.
//! ```

// Standard library

// External crates
use pill_core::warn;

// Current crate
use crate::archetype::ComponentColumns;
use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::world::World;

// =============================================================================
// ComponentAdder
// =============================================================================

/// Trait for adding a component with its concrete type preserved.
///
/// Must be `Send` to support parallel execution of systems.
pub trait ComponentAdder: Send {
    /// Returns the [`ComponentId`] of the component this adder will insert.
    ///
    /// Used to compute the target archetype before the component value itself
    /// is written into storage.
    fn component_id(&self) -> ComponentId;

    /// Writes the held component into the given type-erased storage.
    ///
    /// The caller is responsible for having created the storage row for this
    /// adder's component type before invoking this method.
    fn add_component_to_storage(self: Box<Self>, new_storage: &mut ComponentColumns);
}

/// Typed component adder that knows the concrete type `T`.
///
/// Wraps the concrete component value so it can be transported through the
/// type-erased [`DeferredCommand`] queue without losing its native type.
struct TypedComponentAdder<T: Component> {
    /// The concrete component value to insert when the command executes.
    component: T,
}

impl<T: Component + Send> ComponentAdder for TypedComponentAdder<T> {
    fn component_id(&self) -> ComponentId {
        ComponentId::of::<T>()
    }

    fn add_component_to_storage(self: Box<Self>, new_storage: &mut ComponentColumns) {
        // The storage row for `T` was allocated by the caller; append the
        // component value to it to finish the insertion.
        new_storage.column_of_mut::<T>().push::<T>(self.component);
    }
}

/// Type-erased component adder that writes raw bytes into a native column.
///
/// Used by the C# backend to create or add components that an extension
///  registered as native, so the host never names the concrete type.
/// The byte payload must match the component's ABI layout exactly; the
/// binding validates the size before this adder is queued.
pub struct ByteComponentAdder {
    /// Native component identity the bytes belong to.
    component_id: ComponentId,
    /// Serialized component value in its native ABI layout.
    bytes: Vec<u8>,
}

impl ComponentAdder for ByteComponentAdder {
    fn component_id(&self) -> ComponentId {
        self.component_id
    }

    fn add_component_to_storage(self: Box<Self>, new_storage: &mut ComponentColumns) {
        // The storage row for the native component was allocated by the
        // caller; copy the raw ABI bytes into it.
        assert!(
            self.component_id.is_native_storage(),
            "byte component adder requires a native component id"
        );
        let column = new_storage
            .get_mut(self.component_id)
            .expect("native column must exist for a registered component");
        column.push_bytes(&self.bytes).ok();
    }
}

impl ByteComponentAdder {
    /// Create a byte adder for a native component and its ABI payload.
    pub fn new(component_id: ComponentId, bytes: Vec<u8>) -> Self {
        Self {
            component_id,
            bytes,
        }
    }
}

/// Deferred command to be executed later
// Every variant ends in `Entity` because every deferred command acts on one;
// the suffix is the subject, not noise. Dropping it would leave `Create`,
// `AddComponentTo` and `Destroy`, which read worse at the match sites.
#[allow(clippy::enum_variant_names)]
enum DeferredCommand {
    /// Create a new entity carrying native and type-erased components.
    CreateEntity {
        /// The pre-allocated entity handle to materialize.
        entity: Entity,
        /// Native components to insert into the new archetype's storage.
        component_adders: Vec<Box<dyn ComponentAdder>>,
        /// Type-erased runtime components to write after the row exists.
        descriptor_components: Vec<(ComponentId, Vec<u8>)>,
    },
    /// Add a native component to an existing entity.
    AddComponentToEntity {
        /// The target entity.
        entity: Entity,
        /// The typed adder that writes the component during execution.
        component_adder: Box<dyn ComponentAdder>,
    },
    /// Add a type-erased runtime component to an existing entity.
    AddDescriptorComponentToEntity {
        /// The target entity.
        entity: Entity,
        /// Runtime ID of the component type to write.
        component_id: ComponentId,
        /// Serialized component blob to store.
        bytes: Vec<u8>,
    },
    /// Remove a component from an existing entity.
    RemoveComponentFromEntity {
        /// The target entity.
        entity: Entity,
        /// Runtime ID of the component type to remove.
        component_id: ComponentId,
    },
    /// Destroy an entity entirely.
    DestroyEntity {
        /// The entity to remove from the world.
        entity: Entity,
    },
}

// =============================================================================
// CommandError
// =============================================================================

pub use crate::error::CommandError;

// =============================================================================
// CommandQueue
// =============================================================================

/// Commands queue for deferred operations
///
/// Systems that want to modify entities use Commands to queue changes.
/// These changes are applied in a separate phase after all systems run.
pub struct CommandQueue {
    /// The deferred operations queued this frame, in submission order.
    commands: Vec<DeferredCommand>,
}

impl CommandQueue {
    /// Creates an empty command queue.
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Discard every queued command without applying it.
    ///
    /// Used by transactional startup paths that must roll back a failed
    /// command-producing phase.
    pub(crate) fn clear(&mut self) {
        self.commands.clear();
    }
}

impl CommandQueue {
    /// Queue creating a new entity with components.
    ///
    /// The `entity` must have been pre-allocated from the world's free
    /// list - it won't exist in the world until commands are flushed,
    /// but the caller receives the handle immediately.
    pub fn create_entity(&mut self, entity: Entity, components: Vec<Box<dyn ComponentAdder>>) {
        self.commands.push(DeferredCommand::CreateEntity {
            entity,
            component_adders: components,
            descriptor_components: Vec::new(),
        });
    }

    /// Queue an entity containing both concrete native and type-erased
    /// runtime components.
    pub fn create_mixed_entity(
        &mut self,
        entity: Entity,
        native_components: Vec<Box<dyn ComponentAdder>>,
        descriptor_components: Vec<(ComponentId, Vec<u8>)>,
    ) {
        self.commands.push(DeferredCommand::CreateEntity {
            entity,
            component_adders: native_components,
            descriptor_components,
        });
    }

    /// Queue adding a component to an entity.
    pub fn add_component_to_entity<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + Send,
    {
        self.commands.push(DeferredCommand::AddComponentToEntity {
            entity,
            component_adder: Box::new(TypedComponentAdder { component }),
        });
    }

    /// Queue a concrete native component already erased by an ABI adapter.
    pub fn add_component_adder_to_entity(
        &mut self,
        entity: Entity,
        component_adder: Box<dyn ComponentAdder>,
    ) {
        self.commands.push(DeferredCommand::AddComponentToEntity {
            entity,
            component_adder,
        });
    }

    /// Queue adding a type-erased runtime component.
    pub fn add_descriptor_component_to_entity(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: Vec<u8>,
    ) {
        self.commands
            .push(DeferredCommand::AddDescriptorComponentToEntity {
                entity,
                component_id,
                bytes,
            });
    }

    /// Queue removing a component from an entity
    pub fn remove_component_from_entity<T: Component>(&mut self, entity: Entity) {
        self.commands
            .push(DeferredCommand::RemoveComponentFromEntity {
                entity,
                component_id: ComponentId::of::<T>(),
            });
    }

    /// Queue removing a component selected by its runtime component ID.
    pub fn remove_component_by_id(&mut self, entity: Entity, component_id: ComponentId) {
        self.commands
            .push(DeferredCommand::RemoveComponentFromEntity {
                entity,
                component_id,
            });
    }

    /// Queue destroying (removing) an entity
    pub fn destroy_entity(&mut self, entity: Entity) {
        self.commands
            .push(DeferredCommand::DestroyEntity { entity });
    }

    /// Execute all queued commands
    ///
    /// This is called by the Engine after all systems have run.
    ///
    /// When `exit_on_error` is `true`, any command failure causes an
    /// immediate `Err` return.  When `false`, failures are logged to
    /// stderr and execution continues (backward-compatible behaviour).
    pub(crate) fn execute_queued_commands(
        &mut self,
        world: &mut World,
        exit_on_error: bool,
    ) -> Result<(), Vec<CommandError>> {
        // Step 1: Record the pending count for diagnostics and open the profiling scope.
        let pending = self.commands.len();
        world.commands_executed_this_frame = pending;
        let zone = crate::profile_scope!(
            "execute commands",
            [("Deferred commands to execute: {}", pending)]
        );

        // Step 2: Drain the queue and dispatch each command to its executor.
        let mut errors = Vec::new();
        let mut succeeded = 0usize;

        for command in self.commands.drain(..) {
            // Every executor reports failure by appending to `errors`; a
            // command that appended nothing succeeded. Counting from the
            // vector length keeps one source of truth and avoids the
            // double-counting an unconditional `succeeded += 1` used to
            // introduce for the non-create arms.
            let errors_before = errors.len();
            match command {
                DeferredCommand::CreateEntity {
                    entity,
                    component_adders,
                    descriptor_components,
                } => {
                    // Entity creation can fail to write one or more components
                    // after the row exists; every failure is collected rather
                    // than panicking inside the flush, and a creation that did
                    // not fully materialise is no longer counted as succeeded.
                    if let Err(mut failures) = Self::execute_create_entity(
                        world,
                        entity,
                        component_adders,
                        descriptor_components,
                    ) {
                        errors.append(&mut failures);
                    }
                }

                DeferredCommand::AddComponentToEntity {
                    entity,
                    component_adder,
                } => {
                    Self::execute_add_component(world, entity, component_adder, &mut errors);
                }

                DeferredCommand::AddDescriptorComponentToEntity {
                    entity,
                    component_id,
                    bytes,
                } => {
                    Self::execute_add_descriptor_component(
                        world,
                        entity,
                        component_id,
                        bytes,
                        &mut errors,
                    );
                }

                DeferredCommand::RemoveComponentFromEntity {
                    entity,
                    component_id,
                } => {
                    Self::execute_remove_component(world, entity, component_id, &mut errors);
                }

                DeferredCommand::DestroyEntity { entity } => {
                    Self::execute_destroy_entity(world, entity, &mut errors);
                }
            }
            if errors.len() == errors_before {
                succeeded += 1;
            }
        }

        // Step 3: Report success/failure totals to the profiler.
        zone.text(format_args!(
            "{} succeeded, {} errors",
            succeeded,
            errors.len(),
        ));

        // Step 4: Surface failures - hard-stop with `Err` or log-and-continue.
        if errors.is_empty() {
            Ok(())
        } else if exit_on_error {
            Err(errors)
        } else {
            for err in &errors {
                crate::profile_error!("deferred command failed: {}", err);
                warn!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    error = %err,
                    "deferred command failed"
                );
            }
            Ok(())
        }
    }

    /// Executes a queued entity-creation command.
    ///
    /// Creates the archetype row for the entity and writes both its native
    /// and type-erased components. Component data is trusted because managed
    /// command blobs are validated before being queued.
    fn execute_create_entity(
        world: &mut World,
        entity: Entity,
        component_adders: Vec<Box<dyn ComponentAdder>>,
        descriptor_components: Vec<(ComponentId, Vec<u8>)>,
    ) -> Result<(), Vec<CommandError>> {
        // Step 1: Collect the full component-ID set that defines the new archetype.
        let mut component_ids: Vec<ComponentId> = component_adders
            .iter()
            .map(|adder| adder.component_id())
            .collect();
        component_ids.extend(descriptor_components.iter().map(|(id, _)| *id));

        // Step 1b: Refuse a repeated id before the row exists. The set defines
        // the archetype's columns, so a duplicate would push two rows for one
        // entity row - or panic inside the archetype insert when it found the
        // id already present. `create_descriptor_entity` and the C# create path
        // de-duplicate before they get here; this entry point refuses instead
        // of guessing which row was meant.
        component_ids.sort_unstable();
        if let Some(duplicate) = component_ids
            .windows(2)
            .find(|pair| pair[0] == pair[1])
            .map(|pair| pair[0])
        {
            return Err(vec![CommandError::DuplicateComponent {
                entity,
                component_id: duplicate,
            }]);
        }

        // Step 2: Insert the entity row and write its native components into storage.
        world.insert_entity_with_components(entity, component_ids, |storage| {
            for component_adder in component_adders {
                component_adder.add_component_to_storage(storage);
            }
        });

        // Step 3: Write type-erased components once the entity row exists.
        //
        // These were validated when the command was queued, not now. A reload
        // that retires a component type between the queue and the flush leaves
        // this naming storage that no longer exists, so the failure is
        // collected and reported with the rest of the batch. It used to be an
        // `expect`, which panicked inside `process_frame` - and for a managed
        // project that unwinds across the C ABI.
        let mut errors = Vec::new();
        for (component_id, bytes) in descriptor_components {
            if world
                .set_descriptor_component_bytes(entity, component_id, &bytes)
                .is_err()
            {
                errors.push(CommandError::ComponentWriteFailed {
                    entity,
                    component_id,
                });
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Executes a queued type-erased component addition.
    ///
    /// Rejects stale entities and duplicate components, then writes the blob
    /// into the entity's component storage.
    fn execute_add_descriptor_component(
        world: &mut World,
        entity: Entity,
        component_id: ComponentId,
        bytes: Vec<u8>,
        errors: &mut Vec<CommandError>,
    ) {
        // Step 1: Reject commands that reference an entity which no longer exists.
        if !world.is_entity_valid(entity) {
            errors.push(CommandError::EntityNotFound {
                entity,
                operation: "add_component",
            });
            return;
        }
        // Step 2: Reject components the entity already carries.
        let already_present = world
            .entity_locations
            .get(&entity)
            .and_then(|location| world.archetypes.get(&location.archetype_id))
            .is_some_and(|archetype| archetype.component_types.contains(&component_id));
        if already_present {
            errors.push(CommandError::ComponentAlreadyExists {
                entity,
                component_id,
            });
            return;
        }
        // Step 3: Write the serialized blob into the component storage.
        //
        // The blob was validated when the command was queued, not now. A
        // reload that retires a component type between the queue and the
        // flush leaves this naming storage that no longer exists, so the
        // failure is collected with the rest of the batch. It used to be an
        // `expect`, which panicked inside the flush - and for a managed
        // project that unwinds across the C ABI.
        if let Err(_error) = world.add_descriptor_component(entity, component_id, &bytes) {
            errors.push(CommandError::ComponentWriteFailed {
                entity,
                component_id,
            });
        }
    }

    /// Executes a queued native component addition.
    ///
    /// Migrates the entity to a new archetype that includes the component,
    /// copying every existing component and writing the new one.
    fn execute_add_component(
        world: &mut World,
        entity: Entity,
        component_adder: Box<dyn ComponentAdder>,
        errors: &mut Vec<CommandError>,
    ) {
        // Step 1: Locate the entity and reject stale handles.
        let entity_location = match world.entity_locations.get(&entity) {
            Some(location) => *location,
            None => {
                errors.push(CommandError::EntityNotFound {
                    entity,
                    operation: "add_component",
                });
                return;
            }
        };

        // Step 2: Compute the target archetype's component-ID set, rejecting
        // components the entity already has.
        let Some(old_archetype) = world.archetypes.get(&entity_location.archetype_id) else {
            // The entity's location references an archetype that no longer
            // exists - the desync a partially applied hot reload can leave
            // behind. The entity is effectively unreachable, so report it as
            // not found rather than panicking inside the flush.
            errors.push(CommandError::EntityNotFound {
                entity,
                operation: "add_component",
            });
            return;
        };
        let mut new_component_ids = Vec::with_capacity(old_archetype.component_types.len() + 1);
        new_component_ids.extend_from_slice(&old_archetype.component_types);
        let new_component_id = component_adder.component_id();
        if new_component_ids.contains(&new_component_id) {
            errors.push(CommandError::ComponentAlreadyExists {
                entity,
                component_id: new_component_id,
            });
            return;
        }

        new_component_ids.push(new_component_id);
        new_component_ids.sort();

        // Step 3: Migrate the entity row. Every surviving component is carried
        // by the move itself, so the only thing left to write is the component
        // being added. A migration failure (archetype missing, component column
        // missing) is collected rather than panicking inside the flush.
        if let Err(error) =
            world.move_entity_to_archetype(entity, new_component_ids, |new_storage| {
                component_adder.add_component_to_storage(new_storage);
                Ok(())
            })
        {
            errors.push(CommandError::MigrationFailed {
                entity,
                reason: error.to_string(),
            });
        }
    }

    /// Executes a queued component removal.
    ///
    /// Migrates the entity to a new archetype without the component, or
    /// destroys it outright when no components remain.
    fn execute_remove_component(
        world: &mut World,
        entity: Entity,
        component_id: ComponentId,
        errors: &mut Vec<CommandError>,
    ) {
        // Step 1: Locate the entity and reject stale handles.
        let entity_location = match world.entity_locations.get(&entity) {
            Some(location) => *location,
            None => {
                errors.push(CommandError::EntityNotFound {
                    entity,
                    operation: "remove_component",
                });
                return;
            }
        };

        // Step 2: Reject removal of a component the entity does not have, and
        // compute the surviving component-ID set.
        let Some(old_archetype) = world.archetypes.get(&entity_location.archetype_id) else {
            // As in `execute_add_component`: the entity's recorded archetype
            // is gone, so the entity is effectively unreachable. Report it as
            // not found rather than panicking inside the flush.
            errors.push(CommandError::EntityNotFound {
                entity,
                operation: "remove_component",
            });
            return;
        };

        if !old_archetype.component_types.contains(&component_id) {
            errors.push(CommandError::ComponentNotFound {
                entity,
                component_id,
            });
            return;
        }

        let new_component_ids: Vec<ComponentId> = old_archetype
            .component_types
            .iter()
            .filter(|&id| *id != component_id)
            .cloned()
            .collect();

        // Step 3: An entity with no remaining components is destroyed outright.
        if new_component_ids.is_empty() {
            // If destroy_entity fails (entity already gone) we still want to
            // bail out - no components remain to migrate.
            let _ = world.destroy_entity(entity);
            return;
        }

        // Step 4: Migrate surviving components to the new archetype. The move
        // carries them; the removed component is left behind in the source
        // archetype and released there. A migration failure is collected rather
        // than panicking inside the flush, matching `execute_add_component`.
        if let Err(error) = world.move_entity_to_archetype(entity, new_component_ids, |_| Ok(())) {
            errors.push(CommandError::MigrationFailed {
                entity,
                reason: error.to_string(),
            });
        }
    }

    /// Executes a queued entity destruction.
    ///
    /// Records `CommandError::EntityNotFound` when the entity is already gone,
    /// matching the behaviour of the other command executors.
    fn execute_destroy_entity(world: &mut World, entity: Entity, errors: &mut Vec<CommandError>) {
        if !world.destroy_entity(entity) {
            errors.push(CommandError::EntityNotFound {
                entity,
                operation: "destroy_entity",
            });
        }
    }

    /// Returns whether no commands are currently queued.
    ///
    /// # Examples
    ///
    /// ```
    /// # use pill_engine::commands::CommandQueue;
    /// let queue = CommandQueue::new();
    /// assert!(queue.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl Default for CommandQueue {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Commands
// =============================================================================

/// Commands allows systems to perform deferred entity operations
///
/// This is a system parameter that provides access to the command queue
/// and world.  Entity IDs are allocated eagerly from the free list at
/// `build()` time so the caller can track them immediately, even though
/// the entity won't exist in the world until deferred commands are flushed.
pub struct Commands<'a> {
    /// The queue that deferred operations are appended to.
    command_queue: &'a mut CommandQueue,
    /// The world entities are allocated from, and later mutated through, the queue.
    world: &'a mut World,
}

impl<'a> Commands<'a> {
    /// Wraps a command queue and a world for one frame's system execution.
    pub(crate) fn new(command_queue: &'a mut CommandQueue, world: &'a mut World) -> Self {
        Self {
            command_queue,
            world,
        }
    }

    /// Start building a new entity to create (executed later).
    ///
    /// The entity ID is allocated immediately from the free list and
    /// returned by [`.build()`](DeferredEntityBuilder::build).  The entity
    /// won't appear in the world until deferred commands are flushed at
    /// the end of the frame.
    pub fn create_entity(&mut self) -> DeferredEntityBuilder<'_> {
        let entity = self.world.allocate_entity();
        DeferredEntityBuilder {
            command_queue: self.command_queue,
            allocated_entity: entity,
            components: Vec::with_capacity(
                crate::config::EntityBuilderConfig::DEFAULT_COMPONENTS_CAPACITY,
            ),
        }
    }

    /// Queue adding a component to an entity (executed later)
    pub fn add_component_to_entity<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + Send,
    {
        self.command_queue
            .add_component_to_entity(entity, component);
    }

    /// Queue removing a component from an entity (executed later)
    pub fn remove_component_from_entity<T: Component>(&mut self, entity: Entity) {
        self.command_queue.remove_component_from_entity::<T>(entity);
    }

    /// Queue destroying an entity (executed later)
    pub fn destroy_entity(&mut self, entity: Entity) {
        self.command_queue.destroy_entity(entity);
    }
}

// =============================================================================
// DeferredEntityBuilder
// =============================================================================

/// Builder for creating entities with components through the command queue.
///
/// Unlike [`World::EntityBuilder`](crate::world::EntityBuilder) which creates
/// entities immediately, this builder queues the creation for deferred
/// execution.  However, the entity ID is still allocated eagerly from the
/// free list at construction time, so [`build()`](Self::build) can return
/// it immediately - the entity just won't be queryable until after the
/// current frame's deferred commands are flushed.
pub struct DeferredEntityBuilder<'a> {
    /// The queue the finished create command is appended to.
    command_queue: &'a mut CommandQueue,
    /// The entity handle allocated from the free list at construction time.
    allocated_entity: Entity,
    /// Native components accumulated via [`with`](Self::with).
    components: Vec<Box<dyn ComponentAdder>>,
}

impl<'a> DeferredEntityBuilder<'a> {
    /// Create a new DeferredEntityBuilder with a pre-allocated entity ID.
    /// Called by [`Commands::create_entity`] and [`ScriptContext::create_entity`].
    pub fn new(command_queue: &'a mut CommandQueue, world: &mut World) -> Self {
        Self {
            command_queue,
            allocated_entity: world.allocate_entity(),
            components: Vec::with_capacity(
                crate::config::EntityBuilderConfig::DEFAULT_COMPONENTS_CAPACITY,
            ),
        }
    }

    /// Add a component to the entity being created
    pub fn with<T>(mut self, component: T) -> Self
    where
        T: Component + Send,
    {
        self.components
            .push(Box::new(TypedComponentAdder { component }));
        self
    }

    /// Finish building and queue the create command.
    ///
    /// Returns the entity handle immediately.  The entity won't appear
    /// in world queries until deferred commands are flushed at the end
    /// of the frame, but the handle is valid and can be stored for later
    /// use.
    pub fn build(self) -> Entity {
        let entity = self.allocated_entity;
        self.command_queue.create_entity(entity, self.components);
        entity
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Preserve a native component's concrete Rust type in a type-erased command.
///
/// Foreign-language adapters use this after validating and decoding an ABI
/// component blob against an explicitly shared native component binding.
pub fn boxed_component_adder<T>(component: T) -> Box<dyn ComponentAdder>
where
    T: Component + Send,
{
    Box::new(TypedComponentAdder { component })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};

    use super::*;
    use crate::archetype::Blittability;
    use crate::world::World;

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Position {
        x: f32,
        y: f32,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Velocity {
        x: f32,
        y: f32,
    }

    /// A third component, used to force an archetype migration.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Health {
        value: u32,
    }

    /// Counts how many `OwningComponent` values are alive.
    ///
    /// An archetype move must not change it: the row travels bitwise, so no
    /// value is created and none is destroyed on the way.
    static OWNING_COMPONENTS_ALIVE: AtomicUsize = AtomicUsize::new(0);

    /// Serializes the tests that read the process-wide live count, which would
    /// otherwise see each other's values while cargo runs them in parallel.
    static OWNING_COMPONENT_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Take the live-count lock, ignoring a poisoning left by a failed test:
    /// the counter is reset below either way, and a second failure reported as
    /// a poisoned lock hides the assertion that actually matters.
    fn owning_component_test_guard() -> MutexGuard<'static, ()> {
        let guard = OWNING_COMPONENT_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        OWNING_COMPONENTS_ALIVE.store(0, Ordering::SeqCst);
        guard
    }

    /// A component that owns a heap allocation, so a clone-and-drop move is
    /// distinguishable from a bitwise one by more than a counter.
    #[derive(Debug)]
    struct OwningComponent {
        payload: Vec<u32>,
    }

    impl OwningComponent {
        fn new(payload: Vec<u32>) -> Self {
            OWNING_COMPONENTS_ALIVE.fetch_add(1, Ordering::SeqCst);
            Self { payload }
        }
    }

    impl Clone for OwningComponent {
        fn clone(&self) -> Self {
            Self::new(self.payload.clone())
        }
    }

    impl Drop for OwningComponent {
        fn drop(&mut self) {
            OWNING_COMPONENTS_ALIVE.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Component for OwningComponent {}

    impl Component for Position {}
    impl Component for Velocity {}
    impl Component for Health {}

    /// Tests basic entity creation through the deferred command queue.
    ///
    /// This test verifies that:
    /// - Commands can queue entity creation without immediate execution
    /// - The entity is only created when execute_queued_commands() is called
    /// - The created entity is properly tracked in the world
    /// - Components are correctly added to the entity's archetype
    ///
    /// Expected results:
    /// - Before execution: 0 entities exist in the world
    /// - After execution: 1 entity exists with 2 components (Position+Velocity)
    /// - 1 archetype is created to store the entity
    /// - The archetype contains the correct component types
    #[test]
    fn test_create_command() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let mut queue = CommandQueue::new();
        let mut commands = Commands::new(&mut queue, &mut world);

        // Queue creating a new entity
        commands
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build();

        assert_eq!(
            world.entity_locations.len(),
            0,
            "Entity should not exist yet"
        );

        // Execute commands
        queue.execute_queued_commands(&mut world, false).unwrap();

        assert_eq!(world.entity_locations.len(), 1, "Entity should be created");
        assert_eq!(world.archetypes.len(), 1, "Should have 1 archetype");

        let archetype = world.archetypes.values().next().unwrap();
        assert_eq!(
            archetype.entities.len(),
            1,
            "Archetype should have 1 entity"
        );
        assert_eq!(
            archetype.component_types.len(),
            2,
            "Entity should have 2 components"
        );
    }

    /// Tests the EntityBuilder fluent API for creating entities.
    ///
    /// This test verifies that:
    /// - EntityBuilder provides a fluent interface for entity creation
    /// - Multiple components can be chained using .with() method
    /// - The .build() method queues the creation command
    /// - Entity creation is deferred until execute_queued_commands() is called
    ///
    /// Expected results:
    /// - Before execution: World contains 0 entities
    /// - After execution: World contains 1 entity with both components
    /// - The fluent API works correctly without errors
    #[test]
    fn test_entity_builder() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let mut queue = CommandQueue::new();
        let mut commands = Commands::new(&mut queue, &mut world);

        // Queue creating a new entity
        commands
            .create_entity()
            .with(Position { x: 10.0, y: 20.0 })
            .with(Velocity { x: 1.0, y: 2.0 })
            .build();

        assert_eq!(
            world.entity_locations.len(),
            0,
            "Entity should not exist yet"
        );

        // Execute commands
        queue.execute_queued_commands(&mut world, false).unwrap();

        assert_eq!(world.entity_locations.len(), 1, "Entity should be created");
    }

    #[test]
    fn type_erased_commands_create_add_remove_and_destroy_through_migrations() {
        let mut world = World::new();
        world.register_component::<Position>();
        let descriptor_a = world
            .register_component_descriptor(
                0xA1,
                "Project.DescriptorA",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();
        let descriptor_b = world
            .register_component_descriptor(
                0xB2,
                "Project.DescriptorB",
                4,
                4,
                2,
                Blittability::engine_verified(),
            )
            .unwrap();
        let entity = world.reserve_entity();
        let mut queue = CommandQueue::new();

        queue.create_mixed_entity(
            entity,
            vec![boxed_component_adder(Position { x: 3.0, y: 4.0 })],
            vec![(descriptor_a, 11_u32.to_ne_bytes().to_vec())],
        );
        assert!(
            !world.is_entity_valid(entity),
            "creation must remain deferred"
        );
        queue.execute_queued_commands(&mut world, true).unwrap();
        assert_eq!(world.entity_count(), 1);
        assert_eq!(world.get_component::<Position>(entity).unwrap().x, 3.0);
        assert_eq!(
            world
                .descriptor_component_bytes(entity, descriptor_a)
                .unwrap(),
            11_u32.to_ne_bytes()
        );

        queue.add_descriptor_component_to_entity(
            entity,
            descriptor_b,
            22_u32.to_ne_bytes().to_vec(),
        );
        queue.remove_component_by_id(entity, descriptor_a);
        queue.execute_queued_commands(&mut world, true).unwrap();
        assert!(world
            .descriptor_component_bytes(entity, descriptor_a)
            .is_none());
        assert_eq!(
            world
                .descriptor_component_bytes(entity, descriptor_b)
                .unwrap(),
            22_u32.to_ne_bytes()
        );
        assert_eq!(world.get_component::<Position>(entity).unwrap().y, 4.0);

        queue.destroy_entity(entity);
        assert!(
            world.is_entity_valid(entity),
            "destruction must remain deferred"
        );
        queue.execute_queued_commands(&mut world, true).unwrap();
        assert!(!world.is_entity_valid(entity));
    }

    #[test]
    fn type_erased_command_rejects_a_stale_entity_generation() {
        let mut world = World::new();
        world.register_component::<Position>();
        let entity = world
            .create_entity()
            .with(Position { x: 1.0, y: 2.0 })
            .build()
            .unwrap();
        assert!(world.destroy_entity(entity));
        let replacement = world.reserve_entity();
        assert_eq!(replacement.id(), entity.id());
        assert_ne!(replacement.generation(), entity.generation());

        let mut queue = CommandQueue::new();
        queue.destroy_entity(entity);
        let errors = queue.execute_queued_commands(&mut world, true).unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [CommandError::EntityNotFound {
                operation: "destroy_entity",
                ..
            }]
        ));
    }

    /// A queued creation that lists one component twice is refused before the
    /// entity row exists, naming the component and leaving no partial entity.
    #[test]
    fn mixed_create_with_duplicate_id_reports_error() {
        let mut world = World::new();
        world.register_component::<Position>();
        let entity = world.reserve_entity();
        let archetypes_before = world.archetypes.len();

        let mut queue = CommandQueue::new();
        queue.create_mixed_entity(
            entity,
            vec![
                boxed_component_adder(Position { x: 1.0, y: 2.0 }),
                boxed_component_adder(Position { x: 3.0, y: 4.0 }),
            ],
            Vec::new(),
        );
        let errors = queue.execute_queued_commands(&mut world, true).unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [CommandError::DuplicateComponent { component_id, .. }]
                if *component_id == ComponentId::of::<Position>()
        ));

        assert!(
            !world.entity_locations.contains_key(&entity),
            "a refused creation leaves no entity behind"
        );
        assert_eq!(
            world.archetypes.len(),
            archetypes_before,
            "no archetype was created for the refused set"
        );
    }

    /// An archetype migration moves a component that owns memory; it does not
    /// clone it and drop the original.
    ///
    /// This is the property the copier used to break. A row travelled by deep
    /// clone followed by a deep drop, so a component holding a `Vec` paid an
    /// allocation and a free on every archetype change, and any `Clone` impl
    /// with side effects ran during what the engine calls a relocation. The
    /// live count is the witness: it may not move while the entity does, and it
    /// must reach zero exactly once when the entity is destroyed.
    #[test]
    fn a_migration_moves_an_owning_component_rather_than_cloning_it() {
        let _guard = owning_component_test_guard();
        let mut world = World::new();
        world.register_component::<OwningComponent>();
        world.register_component::<Health>();

        let entity = world
            .create_entity()
            .with(OwningComponent::new(vec![7, 8, 9]))
            .build()
            .unwrap();
        assert_eq!(OWNING_COMPONENTS_ALIVE.load(Ordering::SeqCst), 1);

        // Adding a component migrates the entity to another archetype, which is
        // what carries the owning component across.
        let mut queue = CommandQueue::new();
        queue.add_component_adder_to_entity(entity, boxed_component_adder(Health { value: 5 }));
        queue.execute_queued_commands(&mut world, true).unwrap();

        assert_eq!(
            OWNING_COMPONENTS_ALIVE.load(Ordering::SeqCst),
            1,
            "a move creates no second value and destroys no first one"
        );
        assert_eq!(
            world
                .get_component::<OwningComponent>(entity)
                .expect("the component came across")
                .payload,
            vec![7, 8, 9],
            "the moved row still owns its allocation"
        );

        // Removing it migrates the entity back, again carrying the row.
        let mut queue = CommandQueue::new();
        queue.remove_component_from_entity::<Health>(entity);
        queue.execute_queued_commands(&mut world, true).unwrap();
        assert_eq!(
            OWNING_COMPONENTS_ALIVE.load(Ordering::SeqCst),
            1,
            "the return migration is a move as well"
        );

        // Destroying the entity is the one place the value is released, and it
        // is released exactly once.
        assert!(world.destroy_entity(entity));
        assert_eq!(
            OWNING_COMPONENTS_ALIVE.load(Ordering::SeqCst),
            0,
            "the moved row is dropped exactly once, by whoever owns it last"
        );
    }

    /// A component the migration leaves behind is released, not leaked.
    ///
    /// The counterpart of the test above: the source archetype forgets the rows
    /// it handed over and drops the ones it did not, so removing a component
    /// that owns memory frees it there and then.
    #[test]
    fn a_removed_owning_component_is_dropped_by_the_archetype_it_leaves() {
        let _guard = owning_component_test_guard();
        let mut world = World::new();
        world.register_component::<OwningComponent>();
        world.register_component::<Health>();

        let entity = world
            .create_entity()
            .with(OwningComponent::new(vec![1, 2]))
            .with(Health { value: 3 })
            .build()
            .unwrap();
        assert_eq!(OWNING_COMPONENTS_ALIVE.load(Ordering::SeqCst), 1);

        let mut queue = CommandQueue::new();
        queue.remove_component_from_entity::<OwningComponent>(entity);
        queue.execute_queued_commands(&mut world, true).unwrap();

        assert_eq!(
            OWNING_COMPONENTS_ALIVE.load(Ordering::SeqCst),
            0,
            "the component that did not travel is released by the source archetype"
        );
        assert!(world.get_component::<OwningComponent>(entity).is_none());
        assert_eq!(world.get_component::<Health>(entity).unwrap().value, 3);
        assert!(world.destroy_entity(entity));
    }

    /// Tests entity archetype migration and automatic cleanup when components are removed.
    ///
    /// This test verifies that:
    /// - An entity can be created with multiple components through the command queue
    /// - Components can be removed from an entity after creation
    /// - Removing a component migrates the entity to a new archetype
    /// - The old archetype is automatically cleaned up when it becomes empty
    /// - The entity remains valid and properly tracked after component removal
    /// - Scope-based borrow management allows sequential command queueing and execution
    ///
    /// Expected results:
    /// - Initially: Entity created with Position+Velocity components
    /// - After removing Velocity: Entity migrates to Position-only archetype
    /// - Old Position+Velocity archetype is automatically deleted
    /// - Only 1 archetype remains in the world (Position-only)
    /// - Entity continues to exist with correct component set
    #[test]
    fn test_entity_builder_archetype_deletion() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let mut queue = CommandQueue::new();

        // Queue creating a new entity
        {
            let mut commands = Commands::new(&mut queue, &mut world);
            commands
                .create_entity()
                .with(Position { x: 10.0, y: 20.0 })
                .with(Velocity { x: 1.0, y: 2.0 })
                .build();
        }

        assert_eq!(
            world.entity_locations.len(),
            0,
            "Entity should not exist yet"
        );

        // Execute commands
        queue.execute_queued_commands(&mut world, false).unwrap();

        assert_eq!(world.entity_locations.len(), 1, "Entity should be created");
        let archetype = world.archetypes.values().next().unwrap();
        assert!(
            archetype
                .component_types
                .contains(&ComponentId::of::<Position>()),
            "Archetype should contain Position component"
        );
        assert!(
            archetype
                .component_types
                .contains(&ComponentId::of::<Velocity>()),
            "Archetype should contain Velocity component"
        );

        // Get the entity and queue remove component command
        let entity = *world.entity_locations.keys().next().unwrap();
        {
            let mut commands = Commands::new(&mut queue, &mut world);
            commands.remove_component_from_entity::<Velocity>(entity);
        }

        queue.execute_queued_commands(&mut world, false).unwrap();
        assert_eq!(world.entity_locations.len(), 1, "Entity should still exist");

        let archetype = world.archetypes.values().next().unwrap();
        assert!(
            !archetype
                .component_types
                .contains(&ComponentId::of::<Velocity>()),
            "Archetype should not contain Velocity component"
        );

        archetype.print_info(&world.component_registry);
    }

    /// Tests creating multiple entities with different component combinations through commands.
    ///
    /// This test verifies that:
    /// - Multiple entity creation commands can be queued before execution
    /// - Entities with different component sets are created in separate archetypes
    /// - All queued commands are executed correctly in a single execute_queued_commands() call
    /// - The command queue properly handles entities with varying component combinations
    /// - Archetype system correctly categorizes entities based on their components
    ///
    /// Expected results:
    /// - 3 entities are created in total
    /// - 3 different archetypes are created:
    ///   1. Position+Velocity archetype for entity 1
    ///   2. Position-only archetype for entity 2
    ///   3. Velocity-only archetype for entity 3
    /// - All entities are properly tracked in the world
    #[test]
    fn test_multiple_create_commands() {
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Velocity>();

        let mut queue = CommandQueue::new();
        let mut commands = Commands::new(&mut queue, &mut world);

        // Queue creating multiple entities
        commands
            .create_entity()
            .with(Position { x: 1.0, y: 2.0 })
            .with(Velocity { x: 0.5, y: 1.0 })
            .build();

        commands
            .create_entity()
            .with(Position { x: 5.0, y: 10.0 })
            .build();

        commands
            .create_entity()
            .with(Velocity { x: 2.0, y: 3.0 })
            .build();

        // Execute commands
        queue.execute_queued_commands(&mut world, false).unwrap();

        assert_eq!(world.entity_locations.len(), 3, "Should have 3 entities");
        assert_eq!(
            world.archetypes.len(),
            3,
            "Should have 3 different archetypes"
        );
    }

    /// A queued create that cannot write one of its components is reported,
    /// not counted as a success and not panicked on.
    ///
    /// The component blob was validated when the command was queued, but the
    /// world can change before the flush - here the storage column vanishes
    /// (a partially applied reload rehome). `execute_create_entity` must
    /// surface `ComponentWriteFailed` through the error list, because a panic
    /// here would unwind through `process_frame` and, for a managed project,
    /// across the C ABI.
    #[test]
    fn a_queued_create_that_cannot_write_a_component_is_reported() {
        let mut world = World::new();
        let descriptor_a = world
            .register_component_descriptor(
                0xA1,
                "Project.DescriptorA",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();

        // First flush materialises entity A, which creates the archetype with
        // the descriptor storage column.
        let entity_a = world.reserve_entity();
        let mut queue = CommandQueue::new();
        queue.create_mixed_entity(
            entity_a,
            vec![],
            vec![(descriptor_a, 11_u32.to_ne_bytes().to_vec())],
        );
        queue.execute_queued_commands(&mut world, false).unwrap();
        assert!(world.is_entity_valid(entity_a));

        // Simulate a partially applied reload rehome: the archetype keeps the
        // component in `component_types` but loses its storage column.
        world
            .archetypes
            .values_mut()
            .next()
            .unwrap()
            .component_storages
            .remove(descriptor_a);

        // A second queued create reuses the existing archetype, so the row is
        // materialised but the component write fails.
        let entity_b = world.reserve_entity();
        queue.create_mixed_entity(
            entity_b,
            vec![],
            vec![(descriptor_a, 22_u32.to_ne_bytes().to_vec())],
        );
        let errors = queue
            .execute_queued_commands(&mut world, true)
            .expect_err("a component that cannot be written must fail the flush");

        assert!(matches!(
            errors.as_slice(),
            [CommandError::ComponentWriteFailed {
                entity,
                component_id,
            }] if *entity == entity_b && *component_id == descriptor_a
        ));
    }

    /// A queued removal of the very component whose storage vanished completes
    /// and leaves the entity consistent.
    ///
    /// The migration used to refuse this, because the source release walked
    /// every component the source archetype named and demanded a column for
    /// each. Now a component the destination does not take has nothing left to
    /// release, so the removal is exactly the repair the desynced entity needs:
    /// it lands in a well-formed archetype and the broken one is retired.
    #[test]
    fn a_queued_remove_of_a_component_whose_storage_vanished_completes() {
        let mut world = World::new();
        world.register_component::<Position>();
        let descriptor_a = world
            .register_component_descriptor(
                0xA1,
                "Project.DescriptorA",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();

        // Create an entity carrying both a native and a descriptor component,
        // so removing the descriptor one migrates instead of destroying.
        let entity = world.reserve_entity();
        let mut queue = CommandQueue::new();
        queue.create_mixed_entity(
            entity,
            vec![boxed_component_adder(Position { x: 1.0, y: 2.0 })],
            vec![(descriptor_a, 11_u32.to_ne_bytes().to_vec())],
        );
        queue.execute_queued_commands(&mut world, false).unwrap();
        assert!(world.is_entity_valid(entity));

        // Simulate the desync: the descriptor column vanishes from the archetype
        // while its `component_types` entry survives.
        world
            .archetypes
            .values_mut()
            .next()
            .unwrap()
            .component_storages
            .remove(descriptor_a);

        queue.remove_component_by_id(entity, descriptor_a);
        queue
            .execute_queued_commands(&mut world, true)
            .expect("removing the component whose column is gone has nothing to release");

        assert!(world.is_entity_valid(entity));
        assert_eq!(world.get_component::<Position>(entity).unwrap().x, 1.0);
        assert!(
            world
                .descriptor_component_bytes(entity, descriptor_a)
                .is_none(),
            "the removed component is gone from the entity"
        );
        assert_eq!(
            world.archetypes.len(),
            1,
            "the archetype whose column vanished was emptied and retired"
        );
    }

    /// A migration that cannot carry a column across is reported as
    /// `MigrationFailed`, never panicked on.
    ///
    /// The migration path used to `expect("descriptor storage missing")`, which
    /// aborted the frame inside the flush. The desync is collected as a typed
    /// `WorldError` and surfaced through the command error list instead - and
    /// it is refused before anything moves, so the entity stays where it was.
    #[test]
    fn a_queued_migration_that_cannot_carry_a_column_is_reported() {
        let mut world = World::new();
        world.register_component::<Position>();
        let descriptor_a = world
            .register_component_descriptor(
                0xA1,
                "Project.DescriptorA",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();
        let descriptor_b = world
            .register_component_descriptor(
                0xA2,
                "Project.DescriptorB",
                4,
                4,
                1,
                Blittability::engine_verified(),
            )
            .unwrap();

        let entity = world.reserve_entity();
        let mut queue = CommandQueue::new();
        queue.create_mixed_entity(
            entity,
            vec![boxed_component_adder(Position { x: 1.0, y: 2.0 })],
            vec![
                (descriptor_a, 11_u32.to_ne_bytes().to_vec()),
                (descriptor_b, 22_u32.to_ne_bytes().to_vec()),
            ],
        );
        queue.execute_queued_commands(&mut world, false).unwrap();

        // The desync, this time on a component the migration has to carry:
        // removing `descriptor_b` leaves `descriptor_a` to travel, and it has
        // no column to travel from.
        world
            .archetypes
            .values_mut()
            .next()
            .unwrap()
            .component_storages
            .remove(descriptor_a);

        queue.remove_component_by_id(entity, descriptor_b);
        let errors = queue
            .execute_queued_commands(&mut world, true)
            .expect_err("a migration that cannot complete must fail the flush");

        assert!(matches!(
            errors.as_slice(),
            [CommandError::MigrationFailed { entity: failed_entity, .. }]
                if *failed_entity == entity
        ));
        assert_eq!(
            world.descriptor_component_bytes(entity, descriptor_b),
            Some(&22_u32.to_ne_bytes()[..]),
            "a refused migration leaves the entity where it was, with the              component the removal targeted still on it"
        );
        assert_eq!(world.get_component::<Position>(entity).unwrap().x, 1.0);
    }
}
