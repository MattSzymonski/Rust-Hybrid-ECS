//! [`Res`] / [`ResMut`] system parameters for resource access.

use crate::resource::Resource;
use crate::world::World;

/// Immutable resource access for systems.
///
/// Use `Res<T>` as a system parameter to read a resource without mutation.
/// The scheduler tracks this as a read and allows multiple systems to
/// read the same resource in parallel.
///
/// # Example
/// ```ignore
/// fn my_system(time: Res<GameTime>) {
///     if let Some(time) = time.get() {
///         println!("Elapsed: {}", time.elapsed);
///     }
/// }
/// ```
pub struct Res<'w, T: Resource> {
    world: &'w World,
    _phantom: std::marker::PhantomData<T>,
}

impl<'w, T: Resource> Res<'w, T> {
    pub fn new(world: &'w World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get immutable reference to the resource.
    ///
    /// Returns `None` if the resource has not been inserted into the World.
    pub fn get(&self) -> Option<&T> {
        self.world.get_resource::<T>()
    }
}

/// Mutable resource access for systems.
///
/// Use `ResMut<T>` as a system parameter to read and write a resource.
/// The scheduler tracks this as a write and prevents other systems from
/// accessing the same resource in parallel.
///
/// # Example
/// ```ignore
/// fn my_system(mut time: ResMut<GameTime>) {
///     if let Some(time) = time.get_mut() {
///         time.elapsed += time.delta;
///     }
/// }
/// ```
pub struct ResMut<'w, T: Resource> {
    world: &'w mut World,
    _phantom: std::marker::PhantomData<T>,
}

impl<'w, T: Resource> ResMut<'w, T> {
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Get immutable reference to the resource.
    ///
    /// Returns `None` if the resource has not been inserted into the World.
    pub fn get(&self) -> Option<&T> {
        self.world.get_resource::<T>()
    }

    /// Get mutable reference to the resource.
    ///
    /// Returns `None` if the resource has not been inserted into the World.
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.world.get_resource_mut::<T>()
    }
}
