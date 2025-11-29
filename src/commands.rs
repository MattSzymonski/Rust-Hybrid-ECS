// ============================================================================
// Commands - Deferred Operation Queue
// ============================================================================
//! Commands provide deferred operations on entities and components.
//!
//! Instead of modifying the world immediately (which would require mutable
//! access), commands queue operations to be executed later. This allows
//! multiple systems to run in parallel without conflicts.

use std::sync::Arc;

use trait_type_map::{TraitAccessible, TraitTypeMap, VecFamily};

use crate::component::{Component, ComponentId};
use crate::entity::Entity;
use crate::world::World;

/// Trait for adding a component with its concrete type preserved
trait ComponentAdder {
    fn component_id(&self) -> ComponentId;
    fn add_to_storage(self: Box<Self>, new_storage: &mut TraitTypeMap<dyn Component, VecFamily>);
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

    fn add_to_storage(self: Box<Self>, new_storage: &mut TraitTypeMap<dyn Component, VecFamily>) {
        // Add the new component
        new_storage.get_storage_mut::<T>().push(self.component);
    }
}

/// Deferred command to be executed later
enum DeferredCommand {
    AddComponent {
        entity: Entity,
        adder: Box<dyn ComponentAdder>,
    },
    RemoveComponent {
        entity: Entity,
        component_id: ComponentId,
    },
    DespawnEntity {
        entity: Entity,
    },
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

    /// Queue adding a component to an entity
    pub fn add_component<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + TraitAccessible<dyn Component> + Send,
    {
        self.commands.push(DeferredCommand::AddComponent {
            entity,
            adder: Box::new(TypedComponentAdder { component }),
        });
    }

    /// Queue removing a component from an entity
    pub fn remove_component<T: Component>(&mut self, entity: Entity) {
        self.commands.push(DeferredCommand::RemoveComponent {
            entity,
            component_id: ComponentId::of::<T>(),
        });
    }

    /// Queue despawning (removing) an entity
    pub fn despawn(&mut self, entity: Entity) {
        self.commands
            .push(DeferredCommand::DespawnEntity { entity });
    }

    /// Execute all queued commands
    ///
    /// This is called by the Engine after all systems have run.
    pub(crate) fn execute(&mut self, world: &mut World) {
        for command in self.commands.drain(..) {
            match command {
                DeferredCommand::AddComponent { entity, adder } => {
                    // Get current entity location and components
                    let location = match world.entity_locations.get(&entity) {
                        Some(loc) => *loc,
                        None => {
                            println!(
                                "  [Deferred] Entity {:?} not found for add_component",
                                entity.id
                            );
                            continue;
                        }
                    };

                    let old_archetype = world.archetypes.get(&location.archetype_id).unwrap();
                    let mut new_component_ids = old_archetype.component_types.clone();

                    // Add the new component ID
                    let new_comp_id = adder.component_id();
                    if new_component_ids.contains(&new_comp_id) {
                        println!(
                            "  [Deferred] Entity {:?} already has component {:?}",
                            entity.id, new_comp_id
                        );
                        continue;
                    }

                    new_component_ids.push(new_comp_id);
                    new_component_ids.sort();

                    // Copy existing components using the registered copiers
                    let old_component_ids = old_archetype.component_types.clone();

                    // Collect copiers before borrowing world mutably (Arc::clone is cheap)
                    let copiers: Vec<_> = old_component_ids
                        .iter()
                        .filter_map(|comp_id| world.component_copiers.get(comp_id).map(Arc::clone))
                        .collect();

                    // Move entity to new archetype with the additional component
                    world.move_entity_to_archetype(
                        entity,
                        new_component_ids,
                        |old_storage, new_storage, old_index| {
                            // Copy all existing components from old archetype
                            for copier in copiers.iter() {
                                copier(old_storage, new_storage, old_index);
                            }

                            // Add the new component via the adder
                            adder.add_to_storage(new_storage);
                        },
                    );
                }

                DeferredCommand::RemoveComponent {
                    entity,
                    component_id,
                } => {
                    // Get current entity location and components
                    let location = match world.entity_locations.get(&entity) {
                        Some(loc) => *loc,
                        None => {
                            println!(
                                "  [Deferred] Entity {:?} not found for remove_component",
                                entity.id
                            );
                            continue;
                        }
                    };

                    let old_archetype = world.archetypes.get(&location.archetype_id).unwrap();

                    // Check if entity has this component
                    if !old_archetype.component_types.contains(&component_id) {
                        println!(
                            "  [Deferred] Entity {:?} doesn't have component {:?}",
                            entity.id, component_id
                        );
                        continue;
                    }

                    // Build new component list without the removed component
                    let new_component_ids: Vec<ComponentId> = old_archetype
                        .component_types
                        .iter()
                        .filter(|&id| *id != component_id)
                        .cloned()
                        .collect();

                    // If no components left, despawn the entity instead
                    if new_component_ids.is_empty() {
                        world.despawn(entity);
                        continue;
                    }

                    // Collect copiers for remaining components
                    let copiers: Vec<_> = new_component_ids
                        .iter()
                        .filter_map(|comp_id| world.component_copiers.get(comp_id).map(Arc::clone))
                        .collect();

                    // Move entity to new archetype without the removed component
                    world.move_entity_to_archetype(
                        entity,
                        new_component_ids,
                        |old_storage, new_storage, old_index| {
                            // Copy all components except the removed one
                            for copier in copiers.iter() {
                                copier(old_storage, new_storage, old_index);
                            }
                        },
                    );
                }

                DeferredCommand::DespawnEntity { entity } => {
                    if !world.despawn(entity) {
                        println!(
                            "  [Deferred] Failed to despawn entity {:?} (not found)",
                            entity.id
                        );
                    }
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Commands allows systems to perform deferred entity operations
///
/// This is a system parameter that provides access to the command queue.
pub struct Commands<'a> {
    queue: &'a mut CommandQueue,
}

impl<'a> Commands<'a> {
    pub fn new(queue: &'a mut CommandQueue) -> Self {
        Self { queue }
    }

    /// Queue adding a component to an entity (executed later)
    pub fn add_component<T>(&mut self, entity: Entity, component: T)
    where
        T: Component + TraitAccessible<dyn Component> + Send,
    {
        self.queue.add_component(entity, component);
    }

    /// Queue removing a component from an entity (executed later)
    pub fn remove_component<T: Component>(&mut self, entity: Entity) {
        self.queue.remove_component::<T>(entity);
    }

    /// Queue despawning an entity (executed later)
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.despawn(entity);
    }
}
