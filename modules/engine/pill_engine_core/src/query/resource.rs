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
        // Take the debug write lock here rather than in `get_mut`, so acquire
        // and release share one granularity: this guard's lifetime, which is
        // the system's. See `World::debug_acquire_resource_lock`.
        #[cfg(debug_assertions)]
        world.debug_acquire_resource_lock(crate::resource::ResourceId::of::<T>());
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

/// Releasing the debug write lock is what scopes it to one system.
///
/// A `ResMut` is built when a system's parameters are fetched and dropped when
/// that system returns, so the lock it holds spans exactly the window in which
/// concurrent access would be a bug.
#[cfg(debug_assertions)]
impl<T: Resource> Drop for ResMut<'_, T> {
    fn drop(&mut self) {
        self.world
            .debug_release_resource_lock(crate::resource::ResourceId::of::<T>());
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Resource};

    #[derive(Debug, Default)]
    struct Counter {
        count: u32,
    }
    impl Resource for Counter {}

    fn add_one(mut counter: ResMut<Counter>) {
        if let Some(mut counter) = counter.get_mut() {
            counter.count += 1;
        }
    }

    fn add_ten(mut counter: ResMut<Counter>) {
        if let Some(mut counter) = counter.get_mut() {
            counter.count += 10;
        }
    }

    /// Two systems writing one resource is an ordinary pattern: the scheduler
    /// detects the conflict and runs them in separate batches. The debug write
    /// lock must scope to a system, not to the whole frame, or the second one
    /// panics on an assertion about concurrency that is not happening.
    #[test]
    fn two_systems_may_write_one_resource_in_the_same_frame() {
        let mut engine = Engine::new();
        engine.world_mut().insert_resource(Counter::default());
        engine.register_system("add_one", add_one);
        engine.register_system("add_ten", add_ten);

        engine.process_frame().unwrap();

        assert_eq!(
            engine.world().get_resource::<Counter>().unwrap().count,
            11,
            "both systems must have run"
        );
    }

    /// The same system taking the resource repeatedly across frames is fine
    /// too: the lock is released when its `ResMut` drops, every frame.
    #[test]
    fn a_writer_may_run_every_frame() {
        let mut engine = Engine::new();
        engine.world_mut().insert_resource(Counter::default());
        engine.register_system("add_one", add_one);

        for _ in 0..3 {
            engine.process_frame().unwrap();
        }
        assert_eq!(engine.world().get_resource::<Counter>().unwrap().count, 3);
    }

    /// One system taking the resource mutably more than once, in sequence, is
    /// ordinary code. The lock is per guard, not per `get_mut()` call, so this
    /// must not look like overlapping access.
    #[test]
    fn one_system_may_take_the_resource_twice() {
        fn writes_twice(mut counter: ResMut<Counter>) {
            {
                let mut first = counter.get_mut().expect("resource is present");
                first.count += 1;
            }
            let mut second = counter.get_mut().expect("resource is present");
            second.count += 1;
        }

        let mut engine = Engine::new();
        engine.world_mut().insert_resource(Counter::default());
        engine.register_system("writes_twice", writes_twice);
        engine.process_frame().unwrap();

        assert_eq!(engine.world().get_resource::<Counter>().unwrap().count, 2);
    }

    /// Releasing on drop must not blunt the check: two `ResMut` guards alive at
    /// once over one resource is what it exists to catch, and still panics.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "already mutably borrowed")]
    fn two_live_guards_over_one_resource_still_assert() {
        let mut world = crate::World::new();
        world.insert_resource(Counter::default());

        let first = ResMut::<Counter>::new(&mut world);
        let world_ptr: *mut crate::World = first.world;
        // SAFETY: the alias is deliberate and is the point of the test - a
        // second guard built while the first is alive is exactly the overlap
        // the debug lock reports. The pointer comes from a live borrow, and
        // this thread is the only one touching the world, so the aliasing
        // `&mut` is never used to read or write through both handles.
        let _second = ResMut::<Counter>::new(unsafe { &mut *world_ptr });
    }

    /// A resource released by one guard can be taken again by the next, which
    /// is what lets two systems write it in one frame.
    #[test]
    fn a_released_lock_can_be_retaken() {
        let mut world = crate::World::new();
        world.insert_resource(Counter::default());

        for _ in 0..3 {
            let mut guard = ResMut::<Counter>::new(&mut world);
            guard.get_mut().expect("resource is present").count += 1;
        }
        assert_eq!(world.get_resource::<Counter>().unwrap().count, 3);
    }
}
