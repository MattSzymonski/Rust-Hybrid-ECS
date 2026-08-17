//! [`Res`] and [`ResMut`] system parameters for resource access in systems.
//!
//! # Responsibilities
//!
//! - Provides [`Res<T>`] for immutable singleton resource access from system functions.
//! - Provides [`ResMut<T>`] for mutable resource access with change-detection tracking.
//!
//! # Design
//!
//! These types implement [`SystemParam`](crate::system::SystemParam) so they
//! can appear as system function parameters. The scheduler tracks resource
//! reads and writes for conflict detection, ensuring no two systems obtain
//! `&mut` to the same resource simultaneously.

// Current crate
use crate::query::change_detection::Mut;
use crate::resource::Resource;
use crate::world::World;

// =============================================================================
// Res
// =============================================================================

/// Immutable resource access for systems.
///
/// Use `Res<T>` as a system parameter to read a resource without mutation.
/// The scheduler tracks this as a read and allows multiple systems to
/// read the same resource in parallel.
///
/// # Examples
/// ```no_run
/// # use pill_engine::*;
/// # #[derive(Debug)] struct ProjectTime { elapsed: f32 }
/// # impl Resource for ProjectTime {}
/// fn my_system(time: Res<ProjectTime>) {
///     if let Some(time) = time.get() {
///         println!("Elapsed: {}", time.elapsed);
///     }
/// }
/// ```
pub struct Res<'w, T: Resource> {
    /// The world the resource is fetched from.
    world: &'w World,
    /// Marks `T` as the resource type targeted by this wrapper.
    _phantom: std::marker::PhantomData<T>,
}

impl<'w, T: Resource> Res<'w, T> {
    /// Creates a new [`Res`] wrapper around the given [`World`].
    ///
    /// Constructed by the system runner rather than called directly;
    /// [`Res<T>`] parameters are built automatically when a system runs.
    pub fn new(world: &'w World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Returns an immutable reference to the resource.
    ///
    /// Returns `None` if the resource has not been inserted into the World.
    pub fn get(&self) -> Option<&T> {
        self.world.get_resource::<T>()
    }
}

// =============================================================================
// ResMut
// =============================================================================

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
/// # Examples
/// ```no_run
/// # use pill_engine::*;
/// # #[derive(Debug)] struct ProjectTime { elapsed: f32, delta: f32 }
/// # impl Resource for ProjectTime {}
/// fn my_system(mut time: ResMut<ProjectTime>) {
///     if let Some(mut time) = time.get_mut() {
///         time.elapsed += time.delta; // bumps changed tick
///     }
/// }
/// ```
pub struct ResMut<'w, T: Resource> {
    /// The world the resource is fetched from.
    world: &'w mut World,
    /// Marks `T` as the resource type targeted by this wrapper.
    _phantom: std::marker::PhantomData<T>,
}

impl<'w, T: Resource> ResMut<'w, T> {
    /// Creates a new [`ResMut`] wrapper around the given [`World`].
    ///
    /// Constructed by the system runner rather than called directly;
    /// [`ResMut<T>`] parameters are built automatically when a system runs.
    pub fn new(world: &'w mut World) -> Self {
        Self {
            world,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Returns an immutable reference to the resource.
    ///
    /// Returns `None` if the resource has not been inserted into the World.
    pub fn get(&self) -> Option<&T> {
        self.world.get_resource::<T>()
    }

    /// Returns mutable, change-tracking access to the resource.
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
