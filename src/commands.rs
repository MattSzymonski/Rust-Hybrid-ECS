// ----------------------------------------------------------------------------
// Commands - Deferred Operation Queue
// ----------------------------------------------------------------------------
//! Commands provide deferred operations on entities and components.
//!
//! Instead of modifying the world immediately (which would require mutable
//! access), commands queue operations to be executed later. This allows
//! component iterators and multiple systems to run in parallel without conflicts.
//!
//! ## Frame Lifecycle (Two-Phase Approach)
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
//! # use ecs_hybrid::*;
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

use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::world::World;
use trait_type_map::{TraitAccessible, TraitTypeMap, VecFamily};

/// Trait for adding a component with its concrete type preserved
///
/// Must be Send to support parallel execution of systems.
pub trait ComponentAdder: Send {
    fn component_id(&self) -> ComponentId;
    fn add_component_to_storage(
        self: Box<Self>,
        new_storage: &mut TraitTypeMap<dyn Component, VecFamily>,
    );
}

/// Typed component adder that knows the concrete type T
struct TypedComponentAdder<T: Component + TraitAccessible<dyn Component>> {
    component: T,
}

impl<T: Component + TraitAccessible<dyn Component> + Send> ComponentAdder
    for TypedComponentAdder<T>
{
    fn component_id(&self) -> ComponentId {
        ComponentId::of::<T>()
    }

    fn add_component_to_storage(
        self: Box<Self>,
        new_storage: &mut TraitTypeMap<dyn Component, VecFamily>,
    ) {
        // Add the new component
        new_storage.get_storage_mut::<T>().push(self.component);
    }
}

/// Deferred command to be executed later
enum DeferredCommand {
    CreateEntity {
        entity: Entity,
        component_adders: Vec<Box<dyn ComponentAdder>>,
    },
    AddComponentToEntity {
        entity: Entity,
        component_adder: Box<dyn ComponentAdder>,
    },
    RemoveComponentFromEntity {
        entity: Entity,
        component_id: ComponentId,
    },
    DestroyEntity {
        entity: Entity,
    },
}

/// Error returned when a deferred command cannot be executed.
///
/// Command errors are non-fatal by default - the engine logs them and
/// continues.  Set `Engine::should_exit_on_error` to `true` for strict
/// mode where any command failure stops the frame immediately.
#[derive(Debug, Clone)]
pub enum CommandError {
    /// The target entity no longer exists in the world.
    EntityNotFound {
        entity: Entity,
        operation: &'static str,
    },
    /// The entity already possesses the component being added.
    ComponentAlreadyExists {
        entity: Entity,
        component_id: ComponentId,
    },
    /// The entity does not have the component being removed.
    ComponentNotFound {
        entity: Entity,
        component_id: ComponentId,
    },
}

impl std::fmt::Display for CommandError {
    #[cold]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EntityNotFound { entity, operation } => {
                write!(f, "entity {:?} not found for {operation}", entity.id())
            }
            Self::ComponentAlreadyExists {
                entity,
                component_id,
            } => {
                write!(
                    f,
                    "entity {:?} already has component {:?}",
                    entity.id(),
                    component_id
                )
            }
            Self::ComponentNotFound {
                entity,
                component_id,
            } => {
                write!(
                    f,
                    "entity {:?} doesn't have component {:?}",
                    entity.id(),
                    component_id
                )
            }
        }
    }
}

/// Commands queue for deferred operations
///
/// Systems that want to modify entities use Commands to queue changes.
/// These changes are applied in a separate phase after all systems run.
pub struct CommandQueue {
    commands: Vec<DeferredCommand>,
}

impl CommandQueue {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }
}

impl Default for CommandQueue {
    fn default() -> Self {
        Self::new()
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
        });
    }

    /// Queue adding a component to an entity
    pub fn add_component_to_entity<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + TraitAccessible<dyn Component> + Send,
    {
        self.commands.push(DeferredCommand::AddComponentToEntity {
            entity,
            component_adder: Box::new(TypedComponentAdder { component }),
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
        let pending = self.commands.len();
        world.commands_executed_this_frame = pending;
        let _zone = crate::profile_scope!(
            "execute commands",
            [("Deferred commands to execute: {}", pending)]
        );
        let mut errors = Vec::new();

        for command in self.commands.drain(..) {
            match command {
                DeferredCommand::CreateEntity {
                    entity,
                    component_adders,
                } => {
                    Self::execute_create_entity(world, entity, component_adders);
                }

                DeferredCommand::AddComponentToEntity {
                    entity,
                    component_adder,
                } => {
                    Self::execute_add_component(world, entity, component_adder, &mut errors);
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
        }

        if errors.is_empty() {
            Ok(())
        } else if exit_on_error {
            Err(errors)
        } else {
            for err in &errors {
                eprintln!("  [Deferred] {err}");
            }
            Ok(())
        }
    }

    fn execute_create_entity(
        world: &mut World,
        entity: Entity,
        component_adders: Vec<Box<dyn ComponentAdder>>,
    ) {
        let component_ids: Vec<ComponentId> = component_adders
            .iter()
            .map(|adder| adder.component_id())
            .collect();

        world.insert_entity_with_components(entity, component_ids, |storage| {
            for component_adder in component_adders {
                component_adder.add_component_to_storage(storage);
            }
        });
    }

    fn execute_add_component(
        world: &mut World,
        entity: Entity,
        component_adder: Box<dyn ComponentAdder>,
        errors: &mut Vec<CommandError>,
    ) {
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

        let old_archetype = world
            .archetypes
            .get(&entity_location.archetype_id)
            .expect("archetype must exist for entity at its recorded location");
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

        // Collect copiers by iterating old_archetype.component_types directly
        let component_copiers: Vec<_> = old_archetype
            .component_types
            .iter()
            .filter_map(|component_id| world.component_copiers.get(component_id).copied())
            .collect();

        world.move_entity_to_archetype(
            entity,
            new_component_ids,
            |old_storage, new_storage, old_index| {
                for component_copier in component_copiers.iter() {
                    component_copier(old_storage, new_storage, old_index);
                }
                component_adder.add_component_to_storage(new_storage);
            },
        );
    }

    fn execute_remove_component(
        world: &mut World,
        entity: Entity,
        component_id: ComponentId,
        errors: &mut Vec<CommandError>,
    ) {
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

        let old_archetype = world.archetypes.get(&entity_location.archetype_id).unwrap();

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

        if new_component_ids.is_empty() {
            // If destroy_entity fails (entity already gone) we still want to
            // bail out - no components remain to migrate.
            let _ = world.destroy_entity(entity);
            return;
        }

        let component_copiers: Vec<_> = new_component_ids
            .iter()
            .filter_map(|component_id| world.component_copiers.get(component_id).copied())
            .collect();

        world.move_entity_to_archetype(
            entity,
            new_component_ids,
            |old_storage, new_storage, old_index| {
                for component_copier in component_copiers.iter() {
                    component_copier(old_storage, new_storage, old_index);
                }
            },
        );
    }

    fn execute_destroy_entity(world: &mut World, entity: Entity, errors: &mut Vec<CommandError>) {
        if !world.destroy_entity(entity) {
            errors.push(CommandError::EntityNotFound {
                entity,
                operation: "destroy_entity",
            });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Commands allows systems to perform deferred entity operations
///
/// This is a system parameter that provides access to the command queue
/// and world.  Entity IDs are allocated eagerly from the free list at
/// `build()` time so the caller can track them immediately, even though
/// the entity won't exist in the world until deferred commands are flushed.
pub struct Commands<'a> {
    command_queue: &'a mut CommandQueue,
    world: &'a mut World,
}

impl<'a> Commands<'a> {
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
            components: Vec::with_capacity(8),
        }
    }

    /// Queue adding a component to an entity (executed later)
    pub fn add_component_to_entity<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + TraitAccessible<dyn Component> + Send,
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

/// Builder for creating entities with components through the command queue.
///
/// Unlike [`World::EntityBuilder`](crate::world::EntityBuilder) which creates
/// entities immediately, this builder queues the creation for deferred
/// execution.  However, the entity ID is still allocated eagerly from the
/// free list at construction time, so [`build()`](Self::build) can return
/// it immediately - the entity just won't be queryable until after the
/// current frame's deferred commands are flushed.
pub struct DeferredEntityBuilder<'a> {
    command_queue: &'a mut CommandQueue,
    allocated_entity: Entity,
    components: Vec<Box<dyn ComponentAdder>>,
}

impl<'a> DeferredEntityBuilder<'a> {
    /// Create a new DeferredEntityBuilder with a pre-allocated entity ID.
    /// Called by [`Commands::create_entity`] and [`ScriptContext::create_entity`].
    pub fn new(command_queue: &'a mut CommandQueue, world: &mut World) -> Self {
        Self {
            command_queue,
            allocated_entity: world.allocate_entity(),
            components: Vec::with_capacity(8),
        }
    }

    /// Add a component to the entity being created
    pub fn with<T>(mut self, component: T) -> Self
    where
        T: Component + TraitAccessible<dyn Component> + Send,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;
    use trait_type_map::impl_trait_accessible;

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

    impl Component for Position {}
    impl Component for Velocity {}

    impl_trait_accessible!(dyn Component; Position, Velocity);

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
}
