//! Stress test for the ECS archetype-based iteration and collision performance.
//!
//! Runs a 10,000 entity simulation in which entities move toward a single
//! obstacle carrying five box colliders, reporting collision checks per second.
//!
//! # Responsibilities
//!
//! - Spawns one obstacle entity holding multiple box colliders.
//! - Spawns 10,000 moving entities with a `Transform` and a `Velocity`.
//! - Runs a parallel movement and collision query system over the moving
//!   entities, applying movement only when no collision is detected.
//! - Tracks frame progress and prints total collision checks and throughput
//!   once the simulation finishes.
//!
//! # Design
//!
//! All component types are registered on the engine world before the
//! simulation starts. Movement and collision resolution run inside a single
//! parallel system (`collision_and_movement_system`) using `par_iter_mut`
//! with batch tracking; a second system (`simulation_tracker_system`)
//! aggregates frame state and reports the results. The simulation is driven
//! by repeated calls to `Engine::process_frame`.

// Standard library
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

// External crates
use trait_type_map::impl_trait_accessible;

// Current crate
use pill_engine::{Component, Engine, Query, ResMut, Resource};

// =============================================================================
// Components
// =============================================================================

/// Aggregate simulation state shared with systems as a resource.
///
/// Tracks how many frames have run, when the simulation started, and how many
/// entities participate, so the tracker system can report throughput.
#[derive(Debug, Clone)]
struct SimulationStats {
    /// Number of frames processed so far.
    frame_count: u32,
    /// Total number of frames the simulation should run.
    max_frames: u32,
    /// Wall-clock instant the simulation started, used to measure elapsed time.
    start_time: Instant,
    /// Number of moving entities in the simulation.
    entity_count: usize,
}

impl Resource for SimulationStats {}

/// 3D position component for an entity.
///
/// Movement systems read and write these coordinates each frame.
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

/// 3D velocity component for an entity.
///
/// Movement systems advance the entity's `Transform` by this velocity each
/// frame, scaled by the fixed timestep.
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

/// Axis-aligned bounding box used for collision checks.
///
/// Stores the box's center and per-axis size; `intersects` tests a point
/// against the box after offsetting it by the owning entity's transform.
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

    /// Checks whether a world-space point falls inside this collider after
    /// applying the owning entity's transform.
    fn intersects(&self, transform: &Transform, other_pos: (f32, f32, f32)) -> bool {
        let (cx, cy, cz) = self.center;
        let (sx, sy, sz) = self.size;

        // Step 1: Derive the world-space bounding box from the center, size,
        // and the owning entity's transform.
        let min_x = transform.x + cx - sx / 2.0;
        let max_x = transform.x + cx + sx / 2.0;
        let min_y = transform.y + cy - sy / 2.0;
        let max_y = transform.y + cy + sy / 2.0;
        let min_z = transform.z + cz - sz / 2.0;
        let max_z = transform.z + cz + sz / 2.0;

        // Step 2: Test whether the point lies inside all three axis ranges.
        other_pos.0 >= min_x
            && other_pos.0 <= max_x
            && other_pos.1 >= min_y
            && other_pos.1 <= max_y
            && other_pos.2 >= min_z
            && other_pos.2 <= max_z
    }
}

/// Component bundling multiple box colliders for a single entity.
///
/// A collision is reported if any of the contained colliders intersects the
/// tested point, allowing an obstacle to own several collision volumes.
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

    /// Returns true if any collider intersects the given world-space point.
    fn check_any_collision(&self, transform: &Transform, other_pos: (f32, f32, f32)) -> bool {
        self.colliders
            .iter()
            .any(|collider| collider.intersects(transform, other_pos))
    }
}

impl Component for BoxCollider {}

/// Marker component tagging an entity as an obstacle.
#[derive(Debug, Clone)]
struct Obstacle;

impl Component for Obstacle {}

// Make all components accessible via the Component trait for TraitTypeMap
impl_trait_accessible!(dyn Component; Transform, Velocity, BoxCollider, Obstacle);

// =============================================================================
// Systems
// =============================================================================

/// Advances moving entities along their velocity and blocks movement that
/// would collide with the obstacle.
///
/// Runs the movement pass in parallel over all moving entities using a fixed
/// batch size and batch tracking, then prints the batch statistics once.
fn collision_and_movement_system(
    mut moving_query: Query<(&mut Transform, &Velocity)>,
    mut obstacle_query: Query<(&Transform, &BoxCollider, &Obstacle)>,
) {
    // Get obstacle data
    let Some((obstacle_transform, obstacle_collider, _)) = obstacle_query.first() else {
        return;
    };

    // Parallel version with batch tracking - shows how work is distributed
    {
        let stats = moving_query
            .par_iter_mut()
            .with_batch_size(200)
            .tracked()
            .for_each(|(mut transform, velocity)| {
                // Step 1: Compute the candidate position from the velocity and
                // the fixed 16 ms timestep.
                let new_x = transform.x + velocity.x * 0.016;
                let new_y = transform.y + velocity.y * 0.016;
                let new_z = transform.z + velocity.z * 0.016;

                // Step 2: Check collision with all colliders on the obstacle.
                let collided = obstacle_collider
                    .check_any_collision(obstacle_transform, (new_x, new_y, new_z));

                // Step 3: Apply movement only if no collision.
                if !collided {
                    transform.x = new_x;
                    transform.y = new_y;
                    transform.z = new_z;
                }
            });

        // Use static counter to only print once per 100 frames
        static FRAME_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let frame = FRAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        if frame == 0 {
            println!("Parallel batch stats: {}", stats);
        }
    }
}

/// Counts simulation frames and reports throughput once the run completes.
///
/// Each frame increments the shared frame counter; when the maximum frame
/// count is reached, the elapsed time and total collision checks are printed.
fn simulation_tracker_system(mut stats: ResMut<SimulationStats>) {
    if let Some(mut stats) = stats.get_mut() {
        stats.frame_count += 1;

        if stats.frame_count >= stats.max_frames {
            let duration = stats.start_time.elapsed();

            // Calculate results
            let total_checks = stats.entity_count * stats.max_frames as usize * 5; // 5 colliders

            println!(
                "Entities: {} Frames: {}, Colliders: {}",
                stats.entity_count, stats.max_frames, 5
            );
            println!(
                "Total collision checks: {}, Checks per second: {:.0}",
                total_checks,
                total_checks as f64 / duration.as_secs_f64()
            );
        }
    }
}

// =============================================================================
// Main
// =============================================================================

/// Runs the full stress-test scenario.
///
/// Registers the component types, spawns the obstacle and moving entities,
/// inserts the simulation resource, and drives the simulation to completion.
fn main() {
    println!("=== Stress Test: Archetype-Based ECS ===\n");

    // Step 1: Create the engine and report the available Rayon threads.
    let mut engine = Engine::new();

    println!("Rayon threads: {}", rayon::current_num_threads());

    // Register all component types before use
    engine.world_mut().register_component::<Transform>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<BoxCollider>();
    engine.world_mut().register_component::<Obstacle>();

    // Register systems
    engine.register_system("collision_and_movement", collision_and_movement_system);
    engine.register_system("simulation_tracker", simulation_tracker_system);

    // Step 2: Create obstacle entity with multiple box colliders
    println!("Creating obstacle with 5 box colliders...");

    // Create a BoxCollider with 5 different colliders
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

    println!("✓ Created obstacle entity with 5 box colliders");

    // Step 3: Create moving entities
    let entity_count = 10_000;
    println!("Creating {} moving entities...", entity_count);

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

    println!("✓ Created {} moving entities", entity_count);
    println!("\nScenario: Entities move toward obstacle with 5 box colliders");
    println!("Collision check: Query-based iteration");
    println!("\nRunning 10,000 frame simulation...\n");

    // Step 4: Set up simulation stats as resource
    let max_frames = 10_000;

    engine.world_mut().insert_resource(SimulationStats {
        frame_count: 0,
        max_frames,
        start_time: Instant::now(),
        entity_count,
    });

    // Step 5: Run simulation with systems
    for _frame in 0..max_frames {
        engine.process_frame().unwrap();
    }
}
