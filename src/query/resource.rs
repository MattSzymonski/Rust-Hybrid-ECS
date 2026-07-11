//! [`Res`] / [`ResMut`] system parameters for resource access.

use crate::query::change_detection::Mut;
use crate::resource::Resource;
use crate::world::World;

/// Immutable resource access for systems.
///
/// Use `Res<T>` as a system parameter to read a resource without mutation.
/// The scheduler tracks this as a read and allows multiple systems to
/// read the same resource in parallel.
///
/// # Example
/// ```no_run
/// # use ecs_hybrid::*;
/// # #[derive(Debug)] struct GameTime { elapsed: f32 }
/// # impl Resource for GameTime {}
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

/// Mutable resource access for systems - with change-detection tracking.
///
/// Use `ResMut<T>` as a system parameter to read and write a resource.
/// The scheduler tracks this as a write and prevents other systems from
/// accessing the same resource in parallel.
///
/// `get_mut()` returns a [`Mut<'_, T>`] that automatically bumps the
/// resource's `changed` tick when mutated through `DerefMut`. This lets
/// other systems (or future frames) detect that the resource was modified.
///
/// # Example
/// ```no_run
/// # use ecs_hybrid::*;
/// # #[derive(Debug)] struct GameTime { elapsed: f32, delta: f32 }
/// # impl Resource for GameTime {}
/// fn my_system(mut time: ResMut<GameTime>) {
///     if let Some(mut time) = time.get_mut() {
///         time.elapsed += time.delta; // bumps changed tick
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

    /// Get mutable, change-tracking access to the resource.
    ///
    /// Returns a [`Mut<'_, T>`] that wraps both the value and its
    /// change-detection ticks. Mutating through `DerefMut` automatically
    /// bumps `ticks.changed` to the current world tick.
    ///
    /// Returns `None` if the resource has not been inserted into the World.
    pub fn get_mut(&mut self) -> Option<Mut<'_, T>> {
        self.world.get_resource_mut_tracked::<T>()
    }
}
