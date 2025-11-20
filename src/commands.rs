// ============================================================================
// Commands - Deferred Operation Queue
// ============================================================================
//! Commands provide deferred operations on entities and components.
//!
//! Instead of modifying the world immediately (which would require mutable
//! access), commands queue operations to be executed later. This allows
//! multiple systems to run in parallel without conflicts.

use std::any::Any;

use crate::component::Component;
use crate::entity::Entity;
use crate::world::World;

/// Deferred command to be executed later
enum DeferredCommand {
    AddComponent {
        entity: Entity,
        component: Box<dyn Any>,
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
    pub fn add_component<T: Component>(&mut self, entity: Entity, component: T) {
        self.commands.push(DeferredCommand::AddComponent {
            entity,
            component: Box::new(component),
        });
    }

    /// Execute all queued commands
    ///
    /// This is called by the Engine after all systems have run.
    pub(crate) fn execute(&mut self, world: &mut World) {
        for command in self.commands.drain(..) {
            match command {
                DeferredCommand::AddComponent {
                    entity,
                    component: _,
                } => {
                    // Note: For minimal ECS, we just acknowledge this
                    // A full implementation would move entities between archetypes
                    if world.entity_locations.get(&entity).is_some() {
                        println!("  [Deferred] Would add component to entity {:?}", entity.id);
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
    pub fn add_component<T: Component>(&mut self, entity: Entity, component: T) {
        self.queue.add_component(entity, component);
    }
}
