use crate::command_buffer::CommandBuffer;
/// Unity-like GameObject wrapper - provides familiar OOP API
/// This is the "hybrid" solution that wraps ECS with object-oriented interface
use crate::ecs_core::World;
use parking_lot::RwLock;
use std::sync::Arc;

/// Entity ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Entity(pub u64);

/// GameObject - Unity-like wrapper around an Entity
/// Now stores the world and command buffer pointers directly
#[derive(Clone)]
pub struct GameObject {
    pub entity: Entity,
    world: Arc<RwLock<World>>,
    command_buffer: Arc<RwLock<CommandBuffer>>,
}

impl GameObject {
    /// Create from existing entity
    pub fn from_entity(
        entity: Entity,
        world: Arc<RwLock<World>>,
        command_buffer: Arc<RwLock<CommandBuffer>>,
    ) -> Self {
        Self {
            entity,
            world,
            command_buffer,
        }
    }

    /// Create new GameObject
    pub fn new(world: Arc<RwLock<World>>, command_buffer: Arc<RwLock<CommandBuffer>>) -> Self {
        let entity = Entity(world.write().create_entity_id());
        world.write().register_entity(entity);

        Self {
            entity,
            world,
            command_buffer,
        }
    }

    /// Add a component immediately - executes right away
    ///
    /// Usage:
    /// ```
    /// entity.add_component(Transform::new(0.0, 0.0, 0.0));
    /// // Component is immediately accessible
    /// ```
    pub fn add_component<T: Send + Sync + 'static>(&self, component: T) -> &Self {
        self.world.write().add_component(self.entity, component);
        self
    }

    /// Add a component deferred - queued until apply_commands()
    ///
    /// Usage:
    /// ```
    /// entity.add_component_deferred(Transform::new(0.0, 0.0, 0.0));
    /// // Component NOT accessible yet
    /// scene.apply_commands();
    /// // NOW component is accessible
    /// ```
    pub fn add_component_deferred<T: Send + Sync + 'static>(&self, component: T) {
        self.command_buffer
            .write()
            .add_component(self.entity, component);
    }

    /// Get a component - Unity-like API: gameObject.get_component::<Transform>()
    pub fn get_component<T: 'static>(&self) -> Option<ComponentRef<T>> {
        Some(ComponentRef::<T>::new(self.world.clone(), self.entity))
    }

    /// Get a mutable component reference
    pub fn get_component_mut<T: 'static>(&self) -> Option<ComponentRefMut<T>> {
        Some(ComponentRefMut::new(self.world.clone(), self.entity))
    }

    /// Remove a component immediately
    pub fn remove_component<T: 'static>(&self) {
        self.world.write().remove_component::<T>(self.entity);
    }

    /// Remove a component deferred - queued until apply_commands()
    pub fn remove_component_deferred<T: 'static>(&self) {
        self.command_buffer
            .write()
            .remove_component::<T>(self.entity);
    }

    /// Destroy this GameObject immediately
    pub fn destroy(&self) {
        self.world.write().destroy_entity(self.entity);
    }

    /// Destroy this GameObject deferred - queued until apply_commands()
    pub fn destroy_deferred(&self) {
        self.command_buffer.write().destroy_entity(self.entity);
    }

    /// Check if component exists
    pub fn has_component<T: 'static>(&self) -> bool {
        self.world.read().get_component::<T>(self.entity).is_some()
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
