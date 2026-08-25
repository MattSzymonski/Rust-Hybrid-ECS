//! Tracy Profiling Demo - runs continuously for live profiling.
//!
//! # Responsibilities
//!
//! - Installs the Pill telemetry stack with the Tracy layer enabled.
//! - Registers eight component types and seven systems with mixed component sizes.
//! - Spawns 30 000 entities and drives the engine frame loop at a 30 FPS cap.
//! - Reports live FPS and entity counts to the console every two seconds.
//!
//! # Design
//!
//! The demo keeps a fixed, self-contained workload so frame times, zone
//! counts, and thread distribution stay comparable across runs. Heavy
//! per-entity work (trig, sqrt, and cache-line scattering reads) keeps
//! wake-up latency negligible and stresses parallel distribution.
//!
//! Usage:
//!   1. Start Tracy GUI (Tracy.exe from https://github.com/wolfpld/tracy/releases)
//!   2. cargo run --example tracy_live --release --features profiling
//!   3. Click Connect in Tracy
//!   4. Watch live CPU zones, frame times, and thread work distribution
//!
//! Press Ctrl+C to stop.
//!
//! Reconnecting: after killing and restarting this program, Tracy auto-reconnects.
//! If it doesn't pick up, click the "Connect" button in Tracy GUI again - sometimes
//! the GUI stops listening after an abrupt disconnect.

// Standard library
use std::time::Instant;

// External crates
use pill_engine::*;
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Position
// =============================================================================

/// Two-dimensional position of an entity in world space.
///
/// Updated each frame by `movement_system` and read by `render_system`.
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
/// Consumed by `movement_system` and updated by `gravity_system`.
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
/// Decayed each frame by `health_decay_system`, damaged by
/// `health_decay_system`, and destroyed by `cleanup_system` at zero.
#[derive(Debug, Clone)]
struct Health(f32);
impl Component for Health {}

// =============================================================================
// Enemy
// =============================================================================

/// Marker component tagging entities as enemies.
///
/// Carried by the entities seeded at startup so the world contains a mix of
/// archetypes for the scheduler to batch, which is what this demo profiles.
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

/// 256 B large component - spans 4 cache lines, stresses cache-aware slicing
#[derive(Debug, Clone)]
struct RenderData([[f64; 4]; 8]);
impl Component for RenderData {}

// =============================================================================
// PhysicsData
// =============================================================================

/// 128 B medium component - spans 2 cache lines
#[derive(Debug, Clone)]
struct PhysicsData([[f32; 4]; 8]);
impl Component for PhysicsData {}

// Registers all eight component types with the trait-type map so the ECS can
// type-erase them behind the `Component` trait object.
impl_trait_accessible!(dyn Component; Position, Velocity, Health, Enemy, Mass, GravityForce, RenderData, PhysicsData);

// =============================================================================
// Systems
// =============================================================================

/// Integrates velocity into position for every moving entity.
fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
    }
}

/// Slowly regenerates health towards a zero floor for every entity.
///
/// Uses a labelled, tracked parallel iterator so Tracy reports per-label
/// timings; the deliberately light work keeps the label group meaningful
/// after the first frame.
fn health_decay_system(mut query: Query<&mut Health>) {
    let _zone = crate::profile_scope!("health_decay_systXxxxxxm");

    // Light work - per-label timing will reduce groups to 1 after first frame.
    query
        .par_iter_mut()
        .tracked()
        .label("health_decay_system")
        .for_each(|mut health| {
            health.0 = (health.0 + 0.1).max(0.0);
        });
}
/// Runs a heavy parallel pre-pass, then destroys entities at or below zero health.
///
/// The pre-pass adds CPU work to stress the scheduler's automatic parallelism
/// hint; the second pass performs the actual cleanup through deferred commands.
fn cleanup_system(mut commands: Commands, mut query: Query<(Entity, &Health)>) {
    // Heavy parallel pre-pass - auto-hinted from system EMA, uses full pool.
    query
        .par_iter_mut()
        .label("cleanup_system")
        .tracked()
        .for_each(|(entity, health)| {
            let mut acc = health.0 + entity.id() as f32;
            acc = (acc.sqrt() * acc.cbrt()).clamp(-100.0, 100.0);
            core::hint::black_box(acc);
        });

    // Actual cleanup - destroy entities at or below zero health.
    for (entity, health) in query.iter_mut() {
        if health.0 <= 0.0 {
            commands.destroy_entity(entity);
        }
    }
}

/// Heavy per-entity work - trig, sqrt, mul - designed to stress
/// parallel distribution and make wake-up latency negligible.
/// Writes to its own `GravityForce` component so it can run in
/// parallel with `movement` and `health_decay`.
fn gravity_system(mut query: Query<(&mut GravityForce, &Mass)>) {
    let _zone = crate::profile_scope!("gravity_system");

    let _stats = query
        .par_iter_mut()
        .tracked()
        .label("gravity_system")
        .for_each(|(mut force, mass)| {
            let distance_sq = force.x * force.x + force.y.sqrt() * force.y + 0.01;
            let distance = distance_sq.sqrt();
            let magnitude = mass.0 / (distance_sq * distance);
            force.x = -force.x * magnitude.sqrt();
            force.y = -force.y * magnitude.sqrt();
            force.x = force.x.clamp(-1.0, 1.0);
            force.y = force.y.clamp(-1.0, 1.0);
        });
    crate::profile_message!("gravity: {}", stats);
}

/// Reads 256 B RenderData + writes 4 B Health - tests mixed-size cache pressure
fn render_system(mut query: Query<(&RenderData, &mut Health)>) {
    query
        .par_iter_mut()
        .label("render_system")
        .tracked()
        .for_each(|(render, mut health)| {
            // Touch every 8th f64 in the 256 B struct to simulate scattering reads
            let acc = render.0[0][0] + render.0[2][1] + render.0[4][2] + render.0[6][3];
            health.0 = (health.0 + acc as f32 * 0.001).clamp(0.0, 200.0);
        });
}

/// Reads 128 B PhysicsData + writes 8 B Velocity - tests medium-size cache pressure
fn physics_system(mut query: Query<(&PhysicsData, &mut Velocity)>) {
    query
        .par_iter_mut()
        .label("physics_system")
        .tracked()
        .for_each(|(phys, mut vel)| {
            let ax = phys.0[0][0] + phys.0[1][1];
            let ay = phys.0[2][2] + phys.0[3][3];
            vel.x = (vel.x + ax * 0.01).clamp(-5.0, 5.0);
            vel.y = (vel.y + ay * 0.01).clamp(-5.0, 5.0);
        });
}
/// Fast LCG random f32 - seeded from CPU counter, no syscalls.
fn lcg() -> f32 {
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
        // Fallback: wall clock microseconds
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    use std::cell::Cell;
    thread_local! {
        static S: Cell<u64> = {
            let _zone = crate::profile_scope!("lcg_init");
            Cell::new(seed().wrapping_mul(6364136223846793005).wrapping_add(1))
        };
    }
    S.with(|s| {
        let mut x = s.get();
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.set(x);
        (x >> 32) as f32 / u32::MAX as f32
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
    // Step 1: Install the telemetry stack: terminal logs plus the Tracy layer
    // that routes `profile::*` tracing spans (the `profile_scope!` zones
    // below) into Tracy. The direct client is also started so frame marks,
    // plots, and messages keep working.
    let _ = pill_core::telemetry::TelemetryBuilder::new()
        .with_tracy(true)
        .init();
    crate::profile_init!();
    crate::profile_thread!("main");

    // Step 2: Pause briefly so Tracy's background connection thread can
    // establish the TCP link before we start flooding it with frame data.
    // Also avoids TIME_WAIT collisions on Windows after a restart.
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Step 3: Build the engine, register every component type, and install
    // the seven systems that make up the demo workload.
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

    // Set 30 FPS cap using the engine's built-in limiter
    // engine.set_fps_limit(200.0);
    engine.trace_frame_wait = false;

    // Step 4: Spawn the initial 30 000 entity population with mixed component
    // sizes so cache pressure and parallel distribution are representative.
    engine.world_mut().reserve_entities(32000);
    for _ in 0..30000 {
        let _ = engine
            .world_mut()
            .create_entity()
            .with(Position {
                x: (lcg() - 0.5) * 1000.0,
                y: (lcg() - 0.5) * 1000.0,
            })
            .with(Velocity {
                x: (lcg() - 0.5) * 0.2,
                y: (lcg() - 0.5) * 0.2,
            })
            .with(Health(100.0))
            .with(Mass(1.0 + lcg() * 9.0))
            .with(GravityForce { x: 0.0, y: 0.0 })
            .with(RenderData([[0.0f64; 4]; 8]))
            .with(PhysicsData([[0.0f32; 4]; 8]))
            .with(Enemy)
            .build();
    }

    println!("=== Tracy Live Profiling Demo ===");
    println!("7 systems, 30000 entities, small+large components, parallel ON");
    println!("Target: 30 FPS (engine limiter)");
    println!();
    println!("Connect Tracy now. Press Ctrl+C to stop.");
    println!();

    // Step 5: Run the frame loop, reporting FPS and entity counts every two
    // seconds so the workload's live behaviour stays visible in the console.
    let mut count: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        engine.process_frame().unwrap();
        count += 1;

        // Report every 2 seconds
        let dt = last_report.elapsed().as_secs_f64();
        if dt >= 2.0 {
            let fps = count as f64 / dt;
            let entities = engine.world().entity_count();
            println!("  {:>6.0} FPS | {:>5} entities", fps, entities);
            count = 0;
            last_report = Instant::now();
        }
    }
}
