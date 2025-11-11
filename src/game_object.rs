use crate::command_buffer::CommandBuffer;
/// Unity-like GameObject wrapper - provides familiar OOP API
/// This is the "hybrid" solution that wraps ECS with object-oriented interface
use crate::ecs_core::{Entity, World};
use parking_lot::RwLock;
use std::sync::Arc;

/// GameObject - Unity-like wrapper around an Entity
/// Provides immediate-mode API that feels natural to use
#[derive(Clone)]
pub struct GameObject {
    entity: Entity,
    world: Arc<RwLock<World>>,
    command_buffer: Arc<RwLock<CommandBuffer>>,
    is_pending: bool, // True if entity creation is deferred
}

impl GameObject {
    /// Create from existing entity (direct mode)
    pub fn from_entity(
        entity: Entity,
        world: Arc<RwLock<World>>,
        command_buffer: Arc<RwLock<CommandBuffer>>,
    ) -> Self {
        Self {
            entity,
            world,
            command_buffer,
            is_pending: false,
        }
    }

    /// Create new GameObject (deferred mode - queued in command buffer)
    pub fn new(world: Arc<RwLock<World>>, command_buffer: Arc<RwLock<CommandBuffer>>) -> Self {
        // Create entity immediately for simple case
        let entity = world.write().create_entity();

        Self {
            entity,
            world,
            command_buffer,
            is_pending: false,
        }
    }

    /// Get the underlying entity ID
    pub fn entity(&self) -> Entity {
        self.entity
    }

    /// Add a component - Unity-like API: gameObject.add_component(transform)
    pub fn add_component<T: Send + Sync + 'static>(&self, component: T) -> &Self {
        if self.is_pending {
            // Deferred: queue command
            self.command_buffer
                .write()
                .add_component(self.entity, component);
        } else {
            // Immediate: add directly
            self.world.write().add_component(self.entity, component);
        }
        self
    }

    /// Get a component - Unity-like API: gameObject.get_component::<Transform>()
    pub fn get_component<T: 'static>(&self) -> Option<ComponentRef<T>> {
        if self.is_pending {
            // Can't access components of pending entities
            None
        } else {
            // Return a smart reference that holds the lock
            Some(ComponentRef::<T>::new(self.world.clone(), self.entity))
        }
    }

    /// Get a mutable component reference
    pub fn get_component_mut<T: 'static>(&self) -> Option<ComponentRefMut<T>> {
        if self.is_pending {
            None
        } else {
            Some(ComponentRefMut::new(self.world.clone(), self.entity))
        }
    }

    // pub fn get_component_raw_mut<T: 'static>(&self) -> Result<&mut T, ()> {
    //     if self.is_pending {
    //         Err(())
    //     } else {
    //         let mut world = self.world.write();
    //         let comp = world.get_component_mut::<T>(self.entity);
    //         match comp {
    //             Some(v) => Ok(v),
    //             None => Err(()),
    //         }
    //     }
    // }

    /// Remove a component - Unity-like API
    pub fn remove_component<T: 'static>(&self) {
        if self.is_pending {
            self.command_buffer
                .write()
                .remove_component::<T>(self.entity);
        } else {
            self.world.write().remove_component::<T>(self.entity);
        }
    }

    /// Destroy this GameObject - Unity-like API
    pub fn destroy(&self) {
        if self.is_pending {
            // Can't destroy what doesn't exist yet
            return;
        }
        self.command_buffer.write().destroy_entity(self.entity);
    }

    /// Check if component exists
    pub fn has_component<T: 'static>(&self) -> bool {
        if self.is_pending {
            false
        } else {
            self.world.read().get_component::<T>(self.entity).is_some()
        }
    }
}

/// Smart reference to a component - automatically manages read lock
pub struct ComponentRef<T: 'static> {
    world: Arc<RwLock<World>>,
    entity: Entity,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: 'static> ComponentRef<T> {
    fn new(world: Arc<RwLock<World>>, entity: Entity) -> Self {
        Self {
            world,
            entity,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Access the component through a closure
    pub fn with<R, F: FnOnce(&T) -> R>(&self, f: F) -> Option<R> {
        let world = self.world.read();
        world.get_component::<T>(self.entity).map(f)
    }
}

/// Smart mutable reference to a component - automatically manages write lock
pub struct ComponentRefMut<T: 'static> {
    world: Arc<RwLock<World>>,
    entity: Entity,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: 'static> ComponentRefMut<T> {
    fn new(world: Arc<RwLock<World>>, entity: Entity) -> Self {
        Self {
            world,
            entity,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Access the component mutably through a closure
    pub fn with<R, F: FnOnce(&mut T) -> R>(&mut self, f: F) -> Option<R> {
        let mut world = self.world.write();
        world.get_component_mut::<T>(self.entity).map(f)
    }
}

/// Scene - manages GameObjects like Unity's scene
pub struct Scene {
    world: Arc<RwLock<World>>,
    command_buffer: Arc<RwLock<CommandBuffer>>,
}

impl Scene {
    pub fn new() -> Self {
        Self {
            world: Arc::new(RwLock::new(World::new())),
            command_buffer: Arc::new(RwLock::new(CommandBuffer::new())),
        }
    }

    /// Instantiate a new GameObject - Unity-like API
    pub fn instantiate(&self) -> GameObject {
        GameObject::new(self.world.clone(), self.command_buffer.clone())
    }

    /// Get GameObject from entity ID
    pub fn get_game_object(&self, entity: Entity) -> GameObject {
        GameObject::from_entity(entity, self.world.clone(), self.command_buffer.clone())
    }

    /// Access the world directly for system execution
    pub fn world(&self) -> Arc<RwLock<World>> {
        self.world.clone()
    }

    /// Access command buffer
    pub fn command_buffer(&self) -> Arc<RwLock<CommandBuffer>> {
        self.command_buffer.clone()
    }

    /// Apply all pending commands - called at frame boundaries
    pub fn apply_commands(&self) {
        let mut cmd_buffer = self.command_buffer.write();
        let mut world = self.world.write();
        cmd_buffer.execute(&mut world);
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}
