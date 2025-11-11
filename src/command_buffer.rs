/// Command buffer - deferred operations for thread-safe entity manipulation
/// This solves the "inconsistent state" problem mentioned in the conversation
use crate::ecs_core::World;
use crate::game_object::Entity;

/// Commands that can be deferred and executed later
pub enum Command {
    CreateEntity(Box<dyn FnOnce(&mut World) -> Entity + Send>),
    AddComponent(Entity, Box<dyn FnOnce(&mut World, Entity) + Send>),
    RemoveComponent(Entity, Box<dyn FnOnce(&mut World, Entity) + Send>),
    DestroyEntity(Entity),
}

/// Buffer for deferred commands - allows "Unity-like" immediate operations
/// while maintaining ECS thread-safety
pub struct CommandBuffer {
    commands: Vec<Command>,
}

impl CommandBuffer {
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Schedule entity creation - returns a "future" entity ID
    pub fn create_entity<F>(&mut self, setup: F)
    where
        F: FnOnce(&mut World) -> Entity + Send + 'static,
    {
        self.commands.push(Command::CreateEntity(Box::new(setup)));
    }

    /// Schedule adding a component
    pub fn add_component<T: Send + Sync + 'static>(&mut self, entity: Entity, component: T) {
        self.commands.push(Command::AddComponent(
            entity,
            Box::new(move |world, entity| {
                world.add_component(entity, component);
            }),
        ));
    }

    /// Schedule removing a component
    pub fn remove_component<T: 'static>(&mut self, entity: Entity) {
        self.commands.push(Command::RemoveComponent(
            entity,
            Box::new(|world, entity| {
                world.remove_component::<T>(entity);
            }),
        ));
    }

    /// Schedule entity destruction
    pub fn destroy_entity(&mut self, entity: Entity) {
        self.commands.push(Command::DestroyEntity(entity));
    }

    /// Execute all buffered commands - called at safe synchronization points
    pub fn execute(&mut self, world: &mut World) {
        for command in self.commands.drain(..) {
            match command {
                Command::CreateEntity(func) => {
                    func(world);
                }
                Command::AddComponent(entity, func) => {
                    func(world, entity);
                }
                Command::RemoveComponent(entity, func) => {
                    func(world, entity);
                }
                Command::DestroyEntity(entity) => {
                    world.destroy_entity(entity);
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

impl Default for CommandBuffer {
    fn default() -> Self {
        Self::new()
    }
}
