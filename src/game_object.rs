/// Unity-like Entity API - provides familiar OOP interface over ECS
use crate::ecs_core::World;
use crate::{command_buffer::CommandBuffer, Transform};
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------------------------------
pub type RawComponentRef<'a, T> = MappedRwLockReadGuard<'a, T>;

pub struct ComponentRefer<'a, T>(MappedRwLockReadGuard<'a, T>);
pub struct ComponentReferMut<'a, T>(MappedRwLockWriteGuard<'a, T>);

impl<'a, T> Deref for ComponentRefer<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<'a, T> Deref for ComponentReferMut<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
impl<'a, T> DerefMut for ComponentReferMut<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<'a, T: Clone> ComponentRefer<'a, T> {
    pub fn cloned(&self) -> T {
        self.0.clone()
    }
}

/// Entity - combines ID with world/command buffer pointers for convenient API
#[derive(Clone)]
pub struct Entity {
    pub id: u64,
    world: Arc<RwLock<World>>,
    command_buffer: Arc<RwLock<CommandBuffer>>,
}

impl Entity {
    /// Create from existing entity ID
    pub fn from_id(
        id: u64,
        world: Arc<RwLock<World>>,
        command_buffer: Arc<RwLock<CommandBuffer>>,
    ) -> Self {
        Self {
            id,
            world,
            command_buffer,
        }
    }

    /// Create new Entity
    pub fn new(world: Arc<RwLock<World>>, command_buffer: Arc<RwLock<CommandBuffer>>) -> Self {
        let id = world.write().create_entity_id();
        world.write().register_entity(id);

        Self {
            id,
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
        self.world.write().add_component(self.id, component);
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
            .add_component(self.id, component);
    }

    /// Get a component - Unity-like API: entity.get_component::<Transform>()
    pub fn get_component<T: 'static>(&self) -> Option<ComponentRef<T>> {
        Some(ComponentRef::<T>::new(self.world.clone(), self.id))
    }

    // pub fn get_component_raw<T: 'static>(&self) -> Option<RawComponentRef<'_, T>> {
    //     let guard = self.world.read();
    //     RwLockReadGuard::try_map(guard, |world| world.get_component::<T>(self.id)).ok()
    // }

    // pub fn get_component_raww<T: 'static>(&self) -> Option<ComponentRefer<'_, T>> {
    //     let guard = self.world.read();
    //     RwLockReadGuard::try_map(guard, |world| world.get_component::<T>(self.id)).ok()
    // }

    pub fn get_component_raw<T: 'static>(&self) -> Option<ComponentRefer<'_, T>> {
        let guard = self.world.read();
        RwLockReadGuard::try_map(guard, |world| world.get_component::<T>(self.id))
            .ok()
            .map(ComponentRefer)
    }

    pub fn get_component_raw_mut<T: 'static>(&self) -> Option<ComponentReferMut<'_, T>> {
        let guard = self.world.write(); // acquire a write lock
        RwLockWriteGuard::try_map(guard, |world| world.get_component_mut::<T>(self.id))
            .ok()
            .map(ComponentReferMut)
    }

    pub fn with_component<T: 'static, R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        let world = self.world.read(); // or .unwrap() if you prefer
        let comp = world.get_component::<T>(self.id)?;
        Some(f(comp))
    }

    // pub fn get_component_raw<T: 'static>(&self) -> Option<&T> {
    //     let world = self.world.read();
    //     world.get_component(self.id)
    // }
    //Some(ComponentRef::<T>::new(self.world.clone(), self.id))
    // pub fn get_component_raw<T: 'static>(&self) -> Option<&T> {
    //     let world = self.world.read();
    //     world.get_component::<T>(self.id);
    // }

    /// Get a mutable component reference
    pub fn get_component_mut<T: 'static>(&self) -> Option<ComponentRefMut<T>> {
        Some(ComponentRefMut::new(self.world.clone(), self.id))
    }

    /// Access all components of a specific type through a closure (no cloning)
    /// Useful when entity has multiple components of the same type
    pub fn with_components<T: 'static, R, F>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&[T]) -> R,
    {
        let world = self.world.read();
        world.with_components::<T, R, F>(self.id, f)
    }

    /// Remove a component immediately
    pub fn remove_component<T: 'static>(&self) {
        self.world.write().remove_component::<T>(self.id);
    }

    /// Remove a component deferred - queued until apply_commands()
    pub fn remove_component_deferred<T: 'static>(&self) {
        self.command_buffer.write().remove_component::<T>(self.id);
    }

    /// Destroy this Entity immediately
    pub fn destroy(&self) {
        self.world.write().destroy_entity(self.id);
    }

    /// Destroy this Entity deferred - queued until apply_commands()
    pub fn destroy_deferred(&self) {
        self.command_buffer.write().destroy_entity(self.id);
    }

    /// Check if component exists
    pub fn has_component<T: 'static>(&self) -> bool {
        self.world.read().get_component::<T>(self.id).is_some()
    }
}

// Implement PartialEq and Eq based on ID only
impl PartialEq for Entity {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Entity {}

impl std::hash::Hash for Entity {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl std::fmt::Debug for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Entity").field(&self.id).finish()
    }
}

/// Smart reference to a component - automatically manages read lock
pub struct ComponentRef<T: 'static> {
    world: Arc<RwLock<World>>,
    entity_id: u64,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: 'static> ComponentRef<T> {
    fn new(world: Arc<RwLock<World>>, entity_id: u64) -> Self {
        Self {
            world,
            entity_id,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Access the component through a closure
    pub fn with<R, F: FnOnce(&T) -> R>(&self, f: F) -> Option<R> {
        let world = self.world.read();
        world.get_component::<T>(self.entity_id).map(f)
    }
}

/// Smart mutable reference to a component - automatically manages write lock
pub struct ComponentRefMut<T: 'static> {
    world: Arc<RwLock<World>>,
    entity_id: u64,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: 'static> ComponentRefMut<T> {
    fn new(world: Arc<RwLock<World>>, entity_id: u64) -> Self {
        Self {
            world,
            entity_id,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Access the component mutably through a closure
    pub fn with<R, F: FnOnce(&mut T) -> R>(&mut self, f: F) -> Option<R> {
        let mut world = self.world.write();
        world.get_component_mut::<T>(self.entity_id).map(f)
    }
}

/// Scene - manages Entities like Unity's scene
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

    /// Instantiate a new Entity - Unity-like API
    pub fn instantiate(&self) -> Entity {
        Entity::new(self.world.clone(), self.command_buffer.clone())
    }

    /// Get Entity from entity ID
    pub fn get_entity(&self, id: u64) -> Entity {
        Entity::from_id(id, self.world.clone(), self.command_buffer.clone())
    }

    /// Access the world directly for system execution
    pub fn world(&self) -> Arc<RwLock<World>> {
        self.world.clone()
    }

    pub fn get_world(&self) -> Option<ComponentRefer<'_, World>> {
        let guard = self.world.read();
        RwLockReadGuard::try_map(guard, |world| Some(world))
            .ok()
            .map(ComponentRefer)
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

trait HasComponentRef<T> {
    fn get_component_raw(&self) -> Option<ComponentRefer<'_, T>>;
}

impl HasComponentRef<Transform> for Entity {
    fn get_component_raw(&self) -> Option<ComponentRefer<'_, Transform>> {
        let guard = self.world.read();
        RwLockReadGuard::try_map(guard, |world| world.get_component::<Transform>(self.id))
            .ok()
            .map(ComponentRefer)
    }
}
