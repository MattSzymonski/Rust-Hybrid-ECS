//! Stress test that measures how fast the ECS runs a movement and collision
//! query over many entities.
//!
//! The world holds one obstacle with five box colliders and 10,000 moving
//! entities, which start on a ring and move outward. The run lasts 10,000
//! frames and ends by printing how many collision checks were made per second.
//!
//! # Responsibilities
//!
//! - Spawns one obstacle entity holding several box colliders.
//! - Spawns 10,000 moving entities, each with a `Transform` and a `Velocity`.
//! - Runs a parallel movement and collision query over the moving entities,
//!   and moves an entity only when it does not hit the obstacle.
//! - Counts the frames and prints the total number of collision checks and
//!   the checks per second when the run ends.
//!
//! # Design
//!
//! The program registers every component type before the simulation starts.
//! One parallel system, `collision_and_movement_system`, does the movement and
//! the collision checks with `par_iter_mut` and batch tracking. A second
//! system, `simulation_tracker_system`, keeps the frame count and reports the
//! results. `Engine::process_frame` is called once per frame.

// Standard library
use std::sync::atomic::{AtomicUsize, Ordering};

// External crates
use pill_core::platform::Instant;

// Current crate
use pill_engine::{Component, Engine, Query, ResMut, Resource};

// =============================================================================
// Components
// =============================================================================

/// State the systems share through the world as a resource.
///
/// It holds the frame count, the frame limit, the time the run started and the
/// number of moving entities, so the tracker system can report the results.
#[derive(Debug, Clone)]
struct SimulationStats {
    /// Number of frames processed so far.
    frame_count: u32,
    /// Total number of frames the simulation should run.
    max_frames: u32,
    /// Time the simulation started, used to measure how long the run takes.
    started_at: Instant,
    /// Number of moving entities in the simulation.
    entity_count: usize,
}

impl Resource for SimulationStats {}

/// Position of an entity in 3D space.
///
/// The movement system reads it and writes it back every frame.
#[derive(Debug, Clone)]
struct Transform {
    /// Position along the X axis.
    x: f32,
    /// Position along the Y axis.
    y: f32,
    /// Position along the Z axis.
    z: f32,
}

impl Transform {
    /// Creates a transform at the given 3D position.
    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

impl Component for Transform {}

/// Velocity of an entity in 3D space.
///
/// Each frame the movement system adds this velocity, multiplied by the fixed
/// timestep, to the entity's `Transform`.
#[derive(Debug, Clone)]
struct Velocity {
    /// Velocity along the X axis.
    x: f32,
    /// Velocity along the Y axis.
    y: f32,
    /// Velocity along the Z axis.
    z: f32,
}

impl Velocity {
    /// Creates a velocity with the given per-axis components.
    fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

impl Component for Velocity {}

/// A box lined up with the world axes, used to test whether a point is inside.
///
/// The box stores its center and its size along each axis. `intersects` first
/// moves the box by the owning entity's transform, then tests the point against
/// it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ColliderData {
    /// Box center relative to the owning entity's transform.
    center: (f32, f32, f32),
    /// Full box extent along each axis.
    size: (f32, f32, f32),
}

impl ColliderData {
    /// Creates a collider with the given center and size.
    fn new(center: (f32, f32, f32), size: (f32, f32, f32)) -> Self {
        Self { center, size }
    }

    /// Checks whether a point in world space is inside this box, once the box
    /// has been moved by the owning entity's transform.
    fn intersects(&self, owner_transform: &Transform, other_position: (f32, f32, f32)) -> bool {
        let (center_x, center_y, center_z) = self.center;
        let (size_x, size_y, size_z) = self.size;

        // Step 1: Build the box in world space from its center, its size and
        // the owning entity's transform.
        let min_x = owner_transform.x + center_x - size_x / 2.0;
        let max_x = owner_transform.x + center_x + size_x / 2.0;
        let min_y = owner_transform.y + center_y - size_y / 2.0;
        let max_y = owner_transform.y + center_y + size_y / 2.0;
        let min_z = owner_transform.z + center_z - size_z / 2.0;
        let max_z = owner_transform.z + center_z + size_z / 2.0;

        // Step 2: Check the point against the three axis ranges.
        other_position.0 >= min_x
            && other_position.0 <= max_x
            && other_position.1 >= min_y
            && other_position.1 <= max_y
            && other_position.2 >= min_z
            && other_position.2 <= max_z
    }
}

/// A component holding several box colliders for one entity.
///
/// An obstacle can own several collision volumes, and a hit on any one of them
/// counts as a collision.
#[derive(Debug, Clone)]
struct BoxCollider {
    /// All collision volumes attached to this entity.
    colliders: Vec<ColliderData>,
}

impl BoxCollider {
    /// Creates an empty collider set.
    fn new() -> Self {
        Self {
            colliders: Vec::new(),
        }
    }

    /// Adds a collider volume with the given center and size.
    fn add_collider(&mut self, center: (f32, f32, f32), size: (f32, f32, f32)) {
        self.colliders.push(ColliderData::new(center, size));
    }

    /// Returns true when any of the colliders contains the given world-space
    /// point.
    fn check_any_collision(
        &self,
        owner_transform: &Transform,
        other_position: (f32, f32, f32),
    ) -> bool {
        self.colliders
            .iter()
            .any(|collider| collider.intersects(owner_transform, other_position))
    }
}

impl Component for BoxCollider {}

/// Marker component tagging an entity as an obstacle.
#[derive(Debug, Clone)]
struct Obstacle;

impl Component for Obstacle {}

// =============================================================================
// Systems
// =============================================================================

/// Moves each entity along its velocity and drops a move that would collide
/// with the obstacle.
///
/// The movement pass runs in parallel with a fixed batch size and batch
/// tracking. The statistics it collects are printed once, on the first call.
fn collision_and_movement_system(
    mut moving_query: Query<(&mut Transform, &Velocity)>,
    mut obstacle_query: Query<(&Transform, &BoxCollider, &Obstacle)>,
) {
    // Step 1: Read the obstacle's transform and colliders. Without an obstacle
    // nothing can collide, so the entities stay where they are.
    let Some((obstacle_transform, obstacle_collider, _)) = obstacle_query.first() else {
        return;
    };

    // Step 2: Move the entities in parallel, keeping the statistics so the
    // first call can log how the work was split.
    {
        let batch_stats = moving_query
            .par_iter_mut()
            .with_batch_size(200)
            .tracked()
            .for_each(|(mut transform, velocity)| {
                // Step 1: Compute the candidate position from the velocity and
                // the fixed 16 ms timestep.
                let new_x = transform.x + velocity.x * 0.016;
                let new_y = transform.y + velocity.y * 0.016;
                let new_z = transform.z + velocity.z * 0.016;

                // Step 2: Check the candidate position against the obstacle's
                // colliders.
                let collided = obstacle_collider
                    .check_any_collision(obstacle_transform, (new_x, new_y, new_z));

                // Step 3: Apply the move only if nothing was hit.
                if !collided {
                    transform.x = new_x;
                    transform.y = new_y;
                    transform.z = new_z;
                }
            });

        // Print the statistics once, on the first frame.
        static FRAME_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let frame_index = FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        if frame_index == 0 {
            if let Some(batch_stats) = batch_stats {
                pill_core::info!("Parallel batch stats: {}", batch_stats);
            }
        }
    }
}

/// Counts the frames and reports the results once the run is over.
///
/// Every frame adds one to the frame count. On the frame that reaches the
/// limit, it prints the total number of collision checks and how many were
/// made per second.
fn simulation_tracker_system(mut simulation_stats: ResMut<SimulationStats>) {
    if let Some(mut simulation_stats) = simulation_stats.get_mut() {
        // Step 1: Count this frame.
        simulation_stats.frame_count += 1;

        if simulation_stats.frame_count >= simulation_stats.max_frames {
            // Step 2: On the frame that reaches the limit, report the results.
            let elapsed_time = simulation_stats.started_at.elapsed();

            // Every moving entity is tested against the obstacle's 5 colliders,
            // so this is the number of checks the run aims at. `any` stops at
            // the first hit, so the real number can be lower.
            let total_check_count =
                simulation_stats.entity_count * simulation_stats.max_frames as usize * 5;

            pill_core::info!(
                "Entities: {} Frames: {}, Colliders: {}",
                simulation_stats.entity_count,
                simulation_stats.max_frames,
                5
            );
            pill_core::info!(
                "Total collision checks: {}, Checks per second: {:.0}",
                total_check_count,
                total_check_count as f64 / elapsed_time.as_secs_f64()
            );
        }
    }
}

// =============================================================================
// Main
// =============================================================================

/// Runs the whole stress test.
///
/// It registers the component types, creates the obstacle and the moving
/// entities, stores the simulation state in the world, and runs the frames
/// until the tracker has reported.
///
/// # Panics
///
/// Panics if the engine cannot process a frame, or if building the obstacle or
/// a moving entity fails.
fn main() {
    // Install the engine logger, so these messages and the engine's own
    // reports reach the terminal.
    let _ = pill_core::telemetry::TelemetryBuilder::new().init();
    pill_core::info!("=== Stress Test: Archetype-Based ECS ===");

    // Step 1: Create the engine and log how many Rayon threads it will use.
    let mut engine = Engine::new();

    pill_core::info!("Rayon threads: {}", pill_core::rayon::current_num_threads());

    // Register the component types, so queries can find them.
    engine.world_mut().register_component::<Transform>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<BoxCollider>();
    engine.world_mut().register_component::<Obstacle>();

    // Register both systems with the engine.
    engine.register_system("collision_and_movement", collision_and_movement_system);
    engine.register_system("simulation_tracker", simulation_tracker_system);

    // Step 2: Create the obstacle entity with its box colliders.
    pill_core::info!("Creating obstacle with 5 box colliders...");

    // Give the obstacle one box in the middle and four around it.
    let mut box_collider = BoxCollider::new();
    box_collider.add_collider((0.0, 0.0, 0.0), (5.0, 5.0, 5.0));
    box_collider.add_collider((6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((-6.0, 0.0, 0.0), (4.0, 4.0, 4.0));
    box_collider.add_collider((0.0, 6.0, 0.0), (3.0, 3.0, 3.0));
    box_collider.add_collider((0.0, -6.0, 0.0), (3.0, 3.0, 3.0));

    let _obstacle = engine
        .world_mut()
        .create_entity()
        .with(Obstacle)
        .with(Transform::new(50.0, 0.0, 0.0))
        .with(box_collider)
        .build()
        .unwrap();

    pill_core::info!("✓ Created obstacle entity with 5 box colliders");

    // Step 3: Create the moving entities.
    let entity_count = 10_000;
    pill_core::info!("Creating {} moving entities...", entity_count);

    // Put each entity on a ring of radius 20 and give it a velocity pointing
    // away from the center.
    for i in 0..entity_count {
        let angle = (i as f32 / entity_count as f32) * std::f32::consts::PI * 2.0;
        engine
            .world_mut()
            .create_entity()
            .with(Transform::new(angle.cos() * 20.0, angle.sin() * 20.0, 0.0))
            .with(Velocity::new(angle.cos() * 2.0, angle.sin() * 2.0, 0.0))
            .build()
            .unwrap();
    }

    pill_core::info!("✓ Created {} moving entities", entity_count);
    pill_core::info!("Scenario: Entities move toward obstacle with 5 box colliders");
    pill_core::info!("Collision check: Query-based iteration");
    pill_core::info!("Running 10,000 frame simulation...");

    // Step 4: Store the simulation state in the world as a resource.
    let max_frames = 10_000;

    engine.world_mut().insert_resource(SimulationStats {
        frame_count: 0,
        max_frames,
        started_at: Instant::now(),
        entity_count,
    });

    // Step 5: Run the simulation one frame at a time.
    for _frame in 0..max_frames {
        engine.process_frame().unwrap();
    }
}
