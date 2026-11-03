//! Runs a fixed, heavy ECS workload for live Tracy profiling.
//!
//! # Responsibilities
//!
//! - Installs the Pill telemetry stack with the Tracy layer enabled.
//! - Registers eight component types and six systems of mixed component sizes.
//! - Spawns 30 000 entities and drives the frame loop with no frame cap, so
//!   the profiler sees the frame times the machine really produces.
//! - Prints the live FPS and entity count every two seconds.
//!
//! # Design
//!
//! The workload is fixed and self-contained, so frame times, zone counts and
//! the spread of work across threads stay comparable between runs. The
//! per-entity work is heavy on purpose (square roots, divisions and reads
//! spread over a large component). That keeps the time to wake a worker small
//! next to the work itself and gives the scheduler real work to split.
//!
//! # Usage
//!
//! 1. Start the Tracy GUI (Tracy.exe from
//!    https://github.com/wolfpld/tracy/releases).
//! 2. Run `cargo run --example tracy_live --release --features profiling`.
//! 3. Click Connect in Tracy.
//! 4. Watch the live CPU zones, frame times and thread work distribution.
//!
//! Press Ctrl+C to stop.
//!
//! # Reconnecting
//!
//! After a restart, Tracy usually reconnects on its own. If it does not, click
//! "Connect" again: the GUI can stop listening after an abrupt disconnect.

// Standard library
use std::cell::Cell;

// External crates
use pill_core::platform::Instant;

// Current crate
use pill_engine::*;

// =============================================================================
// Position
// =============================================================================

/// Two-dimensional position of an entity in world space.
///
/// Updated each frame by `movement_system`.
#[derive(Debug, Clone)]
struct Position {
    /// Horizontal coordinate in world units.
    x: f32,
    /// Vertical coordinate in world units.
    y: f32,
}
impl Component for Position {}

// =============================================================================
// Velocity
// =============================================================================

/// Two-dimensional velocity of an entity in world units per frame.
///
/// Read by `movement_system` and updated by `physics_system`.
#[derive(Debug, Clone)]
struct Velocity {
    /// Horizontal speed in world units per frame.
    x: f32,
    /// Vertical speed in world units per frame.
    y: f32,
}
impl Component for Velocity {}

// =============================================================================
// Health
// =============================================================================

/// Current hit points of an entity.
///
/// Each frame `health_decay_system` adds 0.1 and never lets the value go below
/// zero. `cleanup_system` destroys an entity whose health is at or below zero.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Enemy
// =============================================================================

/// Marker component that tags an entity as an enemy.
///
/// Every entity spawned at startup carries it. No system queries it, so it only
/// adds one more component to the seeded entities.
#[derive(Debug, Clone)]
struct Enemy;
impl Component for Enemy {}

// =============================================================================
// Mass
// =============================================================================

/// Mass of an entity, used by `gravity_system` to scale gravitational force.
#[derive(Debug, Clone)]
struct Mass(f32);
impl Component for Mass {}

// =============================================================================
// GravityForce
// =============================================================================

/// Net gravitational force acting on an entity each frame.
///
/// Computed by `gravity_system` and stored per-entity so the system can run
/// in parallel with `movement_system` and `health_decay_system`.
#[derive(Debug, Clone)]
struct GravityForce {
    /// Horizontal force component.
    x: f32,
    /// Vertical force component.
    y: f32,
}
impl Component for GravityForce {}

// =============================================================================
// RenderData
// =============================================================================

/// A 256-byte component, four 64-byte cache lines wide.
///
/// `render_system` reads values from different rows, so each entity's reads
/// touch different cache lines; that read pressure is what this demo measures.
#[derive(Debug, Clone)]
struct RenderData([[f64; 4]; 8]);
impl Component for RenderData {}

// =============================================================================
// PhysicsData
// =============================================================================

/// A 128-byte component, two 64-byte cache lines wide.
///
/// `physics_system` reads a few values from it and writes a small `Velocity`,
/// which gives the demo a medium-size read next to `RenderData`.
#[derive(Debug, Clone)]
struct PhysicsData([[f32; 4]; 8]);
impl Component for PhysicsData {}

// =============================================================================
// Systems
// =============================================================================

/// Adds each entity's velocity to its position once per frame.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut position, velocity) in query.iter_mut() {
        position.x += velocity.x;
        position.y += velocity.y;
    }
}

/// Raises every entity's health by 0.1 each frame, never below zero.
///
/// The loop runs on a labelled, tracked parallel iterator, so Tracy reports a
/// separate timing for this system.
fn health_decay_system(mut query: Query<&mut Health>) {
    let _zone = crate::profile_scope!("health_decay_systXxxxxxm");

    // The work is tiny; the label makes the timeline show this loop as its own
    // group.
    query
        .par_iter_mut()
        .tracked()
        .label("health_decay_system")
        .for_each(|mut health| {
            health.0 = (health.0 + 0.1).max(0.0);
        });
}
/// Runs a heavy parallel pre-pass, then destroys entities at or below zero
/// health.
///
/// The pre-pass adds CPU work, so the scheduler's split has real work to
/// spread. The second pass does the cleanup through deferred commands.
fn cleanup_system(mut commands: Commands, mut query: Query<(Entity, &Health)>) {
    // Step 1: run the heavy parallel work first. The scheduler measures each
    // system's average run time over recent frames and uses it to decide how
    // many tasks to split the loop into, so the work has to be real.
    query
        .par_iter_mut()
        .label("cleanup_system")
        .tracked()
        .for_each(|(entity, health)| {
            let mut work_value = health.0 + entity.id() as f32;
            work_value = (work_value.sqrt() * work_value.cbrt()).clamp(-100.0, 100.0);
            core::hint::black_box(work_value);
        });

    // Step 2: destroy entities at or below zero health.
    for (entity, health) in query.iter_mut() {
        if health.0 <= 0.0 {
            commands.destroy_entity(entity);
        }
    }
}

/// Runs heavy per-entity work (square roots, a division and clamps) to stress
/// the parallel split; against that work, the time to wake a worker is small.
///
/// It writes its own `GravityForce` component, so it can run in parallel with
/// `movement_system` and `health_decay_system`.
fn gravity_system(mut query: Query<(&mut GravityForce, &Mass)>) {
    let _zone = crate::profile_scope!("gravity_system");

    let _stats = query
        .par_iter_mut()
        .tracked()
        .label("gravity_system")
        .for_each(|(mut force, mass)| {
            let distance_squared = force.x * force.x + force.y.sqrt() * force.y + 0.01;
            let distance = distance_squared.sqrt();
            let magnitude = mass.0 / (distance_squared * distance);
            force.x = -force.x * magnitude.sqrt();
            force.y = -force.y * magnitude.sqrt();
            force.x = force.x.clamp(-1.0, 1.0);
            force.y = force.y.clamp(-1.0, 1.0);
        });
    crate::profile_message!("gravity: {}", stats);
}

/// Reads the 256-byte `RenderData` and writes a 4-byte `Health`, to test
/// pressure from mixing component sizes.
fn render_system(mut query: Query<(&RenderData, &mut Health)>) {
    query
        .par_iter_mut()
        .label("render_system")
        .tracked()
        .for_each(|(render, mut health)| {
            // Read four f64 values from different rows of the 256-byte struct,
            // so each entity's reads touch different cache lines.
            let sampled_sum = render.0[0][0] + render.0[2][1] + render.0[4][2] + render.0[6][3];
            health.0 = (health.0 + sampled_sum as f32 * 0.001).clamp(0.0, 200.0);
        });
}

/// Reads the 128-byte `PhysicsData` and writes the 8-byte `Velocity`, to test
/// pressure from medium-size components.
fn physics_system(mut query: Query<(&PhysicsData, &mut Velocity)>) {
    query
        .par_iter_mut()
        .label("physics_system")
        .tracked()
        .for_each(|(physics_data, mut velocity)| {
            let acceleration_x = physics_data.0[0][0] + physics_data.0[1][1];
            let acceleration_y = physics_data.0[2][2] + physics_data.0[3][3];
            velocity.x = (velocity.x + acceleration_x * 0.01).clamp(-5.0, 5.0);
            velocity.y = (velocity.y + acceleration_y * 0.01).clamp(-5.0, 5.0);
        });
}

// =============================================================================
// Random Source
// =============================================================================

/// Returns the next pseudo-random `f32` between 0 and 1.
///
/// The sequence is a linear congruential generator: a multiply and add on a
/// running value. It is fast and good enough for workload data, but not for
/// anything that needs real randomness. The running value is kept per thread
/// and seeded from the CPU cycle counter, or from the wall clock on targets
/// that have no counter.
fn next_random_unit_f32() -> f32 {
    #[cfg(target_arch = "x86_64")]
    fn seed() -> u64 {
        // RDTSC - fast, non-crypto seed. No syscall, no blocking.
        //
        // SAFETY: `_rdtsc` executes the read-only RDTSC CPU instruction: it
        // takes no arguments, touches no memory, and has no preconditions or
        // aliasing obligations, so calling it is always sound.
        unsafe { std::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn seed() -> u64 {
        // Fallback for other targets: the wall clock, in microseconds.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    thread_local! {
        static RANDOM_STATE: Cell<u64> = {
            let _zone = crate::profile_scope!("lcg_init");
            Cell::new(seed().wrapping_mul(6364136223846793005).wrapping_add(1))
        };
    }
    RANDOM_STATE.with(|state| {
        let mut lcg_state = state.get();
        lcg_state = lcg_state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state.set(lcg_state);
        (lcg_state >> 32) as f32 / u32::MAX as f32
    })
}

// =============================================================================
// Main
// =============================================================================

/// Entry point for the live-profiling demo.
///
/// Installs telemetry, builds the engine, spawns the initial population, and
/// runs the frame loop until the process is interrupted.
fn main() {
    // Step 1: Install the telemetry stack: terminal logs, plus the Tracy layer
    // that sends the `profile::*` spans (the `profile_scope!` zones below) to
    // Tracy. The Tracy client is started here too, so frame marks, plots and
    // messages keep working.
    let _ = pill_core::telemetry::TelemetryBuilder::new()
        .with_tracy(true)
        .init();
    crate::profile_init!();
    crate::profile_thread!("main");

    // Step 2: Pause briefly so Tracy's background connection thread can open
    // the TCP link before the loop floods it with frame data. On Windows this
    // also avoids TIME_WAIT collisions after a restart.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Step 3: Build the engine, register every component type, and register the
    // six systems that make up the workload.
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    engine.world_mut().register_component::<Position>();
    engine.world_mut().register_component::<Velocity>();
    engine.world_mut().register_component::<Health>();
    engine.world_mut().register_component::<Enemy>();
    engine.world_mut().register_component::<Mass>();
    engine.world_mut().register_component::<GravityForce>();
    engine.world_mut().register_component::<RenderData>();
    engine.world_mut().register_component::<PhysicsData>();

    engine.register_system("movement", movement_system);
    engine.register_system("health_decay", health_decay_system);
    engine.register_system("gravity", gravity_system);
    engine.register_system("render", render_system);
    engine.register_system("physics", physics_system);
    engine.register_system("cleanup", cleanup_system);

    // No frame cap is set, so the loop runs as fast as the machine allows. The
    // limiter's Tracy zone is off to keep the timeline readable.
    engine.set_trace_frame_wait(false);

    // Step 4: Spawn the initial 30 000 entities with mixed component sizes, so
    // cache pressure and the parallel split are representative.
    engine.world_mut().reserve_entities(32000);
    for _ in 0..30000 {
        let _ = engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: (next_random_unit_f32() - 0.5) * 1000.0,
                y: (next_random_unit_f32() - 0.5) * 1000.0,
            })
            .with(Velocity {
                x: (next_random_unit_f32() - 0.5) * 0.2,
                y: (next_random_unit_f32() - 0.5) * 0.2,
            })
            .with(Health(100.0))
            .with(Mass(1.0 + next_random_unit_f32() * 9.0))
            .with(GravityForce { x: 0.0, y: 0.0 })
            .with(RenderData([[0.0f64; 4]; 8]))
            .with(PhysicsData([[0.0f32; 4]; 8]))
            .with(Enemy)
            .build();
    }

    pill_core::info!("=== Tracy Live Profiling Demo ===");
    pill_core::info!("7 systems, 30000 entities, small+large components, parallel ON");
    pill_core::info!("Target: 30 FPS (engine limiter)");
    pill_core::info!("Connect Tracy now. Press Ctrl+C to stop.");

    // Step 5: Run the frame loop and report the FPS and entity count every two
    // seconds, so the workload's live behaviour stays visible in the console.
    let mut frame_count_since_report: u64 = 0;
    let mut last_report_at = Instant::now();

    loop {
        engine.process_frame().unwrap();
        frame_count_since_report += 1;

        let delta_time = last_report_at.elapsed().as_secs_f64();
        if delta_time >= 2.0 {
            let fps = frame_count_since_report as f64 / delta_time;
            let entity_count = engine.world().entity_count();
            pill_core::info!("  {:>6.0} FPS | {:>5} entities", fps, entity_count);
            frame_count_since_report = 0;
            last_report_at = Instant::now();
        }
    }
}
