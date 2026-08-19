//! Hot-reloadable bouncing-ball project implemented with ECS systems.
//!
//! # Responsibilities
//!
//! - Owns the simulation-time resource and the ball-physics systems that run
//!   every frame.
//! - Exposes the `project_init` / `project_update` entry points the host loads
//!   through the project-module ABI.
//!
//! # Design
//!
//! The project is a plain `cdylib` that registers ECS components, resources,
//! and systems through the [`EngineApi`] during `project_init`. Hot reload
//! preserves entities, so initialization fills the world only up to the target
//! ball count instead of spawning fresh entities on every rebuild.

// Standard library
use std::time::Instant;

// External crates
use pill_core::error;
use pill_core::info;
use pill_engine::*;
use pill_spline::Spline;
use serde::{Deserialize, Serialize};
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Ball physics component
// =============================================================================

/// A rigid ball that bounces inside a fixed box, simulated each frame.
///
/// The host serializes this component across hot-reload generations, so the
/// struct layout is pinned with `#[repr(C)]` and every field stays
/// `Serialize` / `Deserialize` compatible.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PhysicsState {
    pub delta_time: f32,
    pub position_x: f32,
    pub position_y: f32,
    pub velocity_x: f32,
    pub velocity_y: f32,
    pub radius: f32,
    pub active: bool,
}

impl Component for PhysicsState {}
impl_trait_accessible!(dyn Component; PhysicsState);

const FIXED_DELTA_TIME: f32 = 1.0 / 60.0;
const GRAVITY: f32 = 800.0;
const BOUNCE_VELOCITY_Y: f32 = -500.0;
const BOUNCE_VELOCITY_X: f32 = 150.0;
const RESTITUTION: f32 = 0.7;
/// Upward speed restored when a floor bounce would otherwise decay to rest.
///
/// Keeps every ball visibly bouncing for the whole lifetime of the scene
/// instead of settling on the floor after a few seconds.
const MINIMUM_BOUNCE_VELOCITY_Y: f32 = 300.0;
const FLOOR_Y: f32 = 580.0;
const CEILING_Y: f32 = 20.0;
const LEFT_WALL: f32 = 20.0;
const RIGHT_WALL: f32 = 780.0;
const BALL_COUNT: usize = 100;

struct SimulationTime {
    last_frame: Instant,
    delta_seconds: f32,
}

impl Resource for SimulationTime {}

fn update_time_system(mut time: ResMut<SimulationTime>) -> Result<(), SystemError> {
    let now = Instant::now();
    // A missing resource means the project module and host disagree about
    // initialization; report it through the system result so the frame
    // continues while the host logs the failure.
    let Some(mut time) = time.get_mut() else {
        return Err(SystemError::MissingResource {
            name: String::from("SimulationTime"),
        });
    };
    time.delta_seconds = now.duration_since(time.last_frame).as_secs_f32().min(0.1);
    time.last_frame = now;
    Ok(())
}

fn initial_physics_state() -> PhysicsState {
    PhysicsState {
        delta_time: FIXED_DELTA_TIME,
        position_x: 400.0,
        position_y: 500.0,
        velocity_x: BOUNCE_VELOCITY_X,
        velocity_y: BOUNCE_VELOCITY_Y,
        radius: 16.0,
        active: true,
    }
}

impl Default for PhysicsState {
    fn default() -> Self {
        initial_physics_state()
    }
}

fn simulate_ball(state: &mut PhysicsState) {
    if !state.active {
        return;
    }

    let delta = state.delta_time.clamp(0.0, 0.1);
    state.velocity_y += GRAVITY * delta;
    state.position_x += state.velocity_x * delta;
    state.position_y += state.velocity_y * delta;

    if state.position_y + state.radius >= FLOOR_Y {
        state.position_y = FLOOR_Y - state.radius;
        state.velocity_y = -state.velocity_y.abs() * RESTITUTION;
        // Restitution alone decays each bounce towards rest; restore a
        // minimum upward speed so balls bounce forever.
        if state.velocity_y.abs() < MINIMUM_BOUNCE_VELOCITY_Y {
            state.velocity_y = -MINIMUM_BOUNCE_VELOCITY_Y;
        }
    }
    if state.position_y - state.radius <= CEILING_Y {
        state.position_y = CEILING_Y + state.radius;
        state.velocity_y = state.velocity_y.abs() * RESTITUTION;
    }
    if state.position_x - state.radius <= LEFT_WALL {
        state.position_x = LEFT_WALL + state.radius;
        state.velocity_x = state.velocity_x.abs() * RESTITUTION;
    }
    if state.position_x + state.radius >= RIGHT_WALL {
        state.position_x = RIGHT_WALL - state.radius;
        state.velocity_x = -state.velocity_x.abs() * RESTITUTION;
    }
}

#[cfg(not(feature = "rendering"))]
fn physics_system(
    time: Res<SimulationTime>,
    mut query: Query<&mut PhysicsState>,
) -> Result<(), SystemError> {
    let Some(time) = time.get() else {
        return Err(SystemError::MissingResource {
            name: String::from("SimulationTime"),
        });
    };
    let delta_seconds = time.delta_seconds;
    for mut physics in query.iter_mut() {
        physics.delta_time = delta_seconds;
        simulate_ball(&mut physics);
    }
    Ok(())
}

#[cfg(feature = "rendering")]
fn physics_system(
    time: Res<SimulationTime>,
    mut query: Query<(&mut PhysicsState, &mut Position, &mut Sprite)>,
) -> Result<(), SystemError> {
    let Some(time) = time.get() else {
        return Err(SystemError::MissingResource {
            name: String::from("SimulationTime"),
        });
    };
    let delta_seconds = time.delta_seconds;
    for (mut physics, mut position, mut sprite) in query.iter_mut() {
        physics.delta_time = delta_seconds;
        simulate_ball(&mut physics);

        // Physics coordinates describe the center of the ball; the sprite
        // renderer expects the top-left corner.
        position.x = physics.position_x - physics.radius;
        position.y = physics.position_y - physics.radius;
        sprite.width = physics.radius * 2.0;
        sprite.height = physics.radius * 2.0;
        sprite.color = if physics.active {
            Color::new(1.0, 0.3, 0.3, 1.0)
        } else {
            Color::new(0.5, 0.5, 0.5, 1.0)
        };
    }
    Ok(())
}

// =============================================================================
// Spline usage
// =============================================================================

/// Frame interval at which the spline probe reports what it can see.
///
/// Large because the headless loop runs tens of thousands of frames per second
/// and this is only a demonstration of reading the component.
const SPLINE_REPORT_INTERVAL_FRAMES: u64 = 30_000;

/// Counts frames so the spline probe reports at a readable interval.
struct SplineProbeState {
    frame_count: u64,
}

impl Resource for SplineProbeState {}

/// Reads every `Spline` in the world and samples its midpoint.
///
/// Demonstrates the project using a component type defined by the `pill_spline`
/// crate: the type is named through a direct dependency, so the query matches
/// only splines registered by this same compiled copy of the crate.
fn spline_probe_system(
    mut state: ResMut<SplineProbeState>,
    mut splines: Query<&mut Spline>,
) -> Result<(), SystemError> {
    let Some(mut state) = state.get_mut() else {
        return Err(SystemError::MissingResource {
            name: String::from("SplineProbeState"),
        });
    };
    state.frame_count += 1;
    if state.frame_count % SPLINE_REPORT_INTERVAL_FRAMES != 0 {
        return Ok(());
    }

    // Report how many splines this module can see, which is what reveals
    // whether a separately loaded copy of the crate shares the component type.
    let mut visible_spline_count = 0;
    let mut midpoint = pill_core::math::Vector3f::ZERO;
    for spline in splines.iter_mut() {
        visible_spline_count += 1;
        midpoint = spline.get_location_at(0.5);
    }
    // Printed rather than logged through `tracing`: the project links its own
    // copy of `pill_core`, so its tracing dispatcher has no subscriber and log
    // lines emitted here never reach the host's telemetry.
    println!(
        "[project] sees {visible_spline_count} spline(s), midpoint ({:.1}, {:.1})",
        midpoint.x, midpoint.y
    );
    Ok(())
}

// =============================================================================
// Project module entry points
// =============================================================================

/// Registers components, resources, and systems; returns zero on success.
///
/// The host treats a non-zero status as a failed generation and rolls back to
/// the previously loaded module. Initialization must therefore be idempotent:
/// re-registering the same components and systems is safe, and entities are
/// only filled up to a target count.
///
/// # Safety
///
/// `api` must be a valid [`EngineApi`] pointer owned by the host for the
/// complete duration of this call.
#[no_mangle]
pub unsafe extern "C" fn project_init(api: *const EngineApi) -> u32 {
    let api = unsafe { &*api };
    let engine = unsafe { &mut *(api.engine_handle as *mut Engine) };

    engine
        .world_mut()
        .register_persistable_component::<PhysicsState>();

    #[cfg(feature = "rendering")]
    {
        engine.world_mut().register_component::<Position>();
        engine.world_mut().register_component::<Sprite>();
    }

    engine.world_mut().insert_resource(SimulationTime {
        last_frame: Instant::now(),
        delta_seconds: FIXED_DELTA_TIME,
    });
    engine.register_system("simulation_time", update_time_system);
    engine.register_system("ball_physics", physics_system);

    // Components defined by another crate are registered by whoever links that
    // crate. Registration is keyed by the type, so this is the same component
    // the crate itself would register only when both sides were compiled in one
    // workspace; a separately built copy is a distinct type with its own
    // storage. Keep `pill_spline` out of PILL_MODULES while the project links
    // it directly, so there is exactly one registration of the type.
    engine.world_mut().register_persistable_component::<Spline>();
    engine
        .world_mut()
        .insert_resource(SplineProbeState { frame_count: 0 });
    engine.register_system("spline_probe", spline_probe_system);

    // Fill up to one project-owned spline instead of adding another on every
    // reload, matching how the ball entities are kept at a target count.
    let existing_splines = {
        let mut query = Query::<&Spline>::new(engine.world_mut());
        query.iter_mut().count()
    };
    if existing_splines == 0 {
        let path = Spline::from_points(&[
            pill_core::math::Vector3f::new(20.0, 300.0, 0.0),
            pill_core::math::Vector3f::new(260.0, 120.0, 0.0),
            pill_core::math::Vector3f::new(540.0, 460.0, 0.0),
            pill_core::math::Vector3f::new(780.0, 300.0, 0.0),
        ]);
        if engine.world_mut().create_entity().with(path).build().is_err() {
            error!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "failed to build the project spline entity; aborting this generation"
            );
            return 1;
        }
    }

    // Hot reload preserves entities, so only fill the world up to the target
    // instead of adding another 100 balls on every rebuild.
    let existing_balls = {
        let mut query = Query::<&PhysicsState>::new(engine.world_mut());
        query.iter_mut().count()
    };
    for index in existing_balls..BALL_COUNT {
        let column = (index % 10) as f32;
        let row = (index / 10) as f32;
        let mut physics = initial_physics_state();
        physics.position_x = 60.0 + column * 72.0;
        physics.position_y = 60.0 + row * 42.0;
        physics.velocity_x = if index % 2 == 0 {
            BOUNCE_VELOCITY_X + row * 8.0
        } else {
            -BOUNCE_VELOCITY_X - row * 8.0
        };
        physics.velocity_y = BOUNCE_VELOCITY_Y + column * 18.0;
        physics.radius = 10.0 + (index % 4) as f32 * 2.0;

        let entity = engine.world_mut().create_entity().with(physics);

        #[cfg(feature = "rendering")]
        let entity = entity
            .with(Position {
                x: physics.position_x - physics.radius,
                y: physics.position_y - physics.radius,
            })
            .with(Sprite {
                width: physics.radius * 2.0,
                height: physics.radius * 2.0,
                color: Color::new(1.0, 0.3, 0.3, 1.0),
            });

        match entity.build() {
            Ok(_) => {}
            Err(error) => {
                // Report the failure and abort the generation: the host keeps
                // the previously loaded module when project_init returns non-zero.
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    error = %error,
                    "failed to build a ball entity; aborting this project generation"
                );
                return 1;
            }
        }
    }

    // Report successful registration so the host keeps this generation.
    0
}

#[no_mangle]
pub extern "C" fn project_update(_api: *const EngineApi) {
    // Gameplay is executed entirely by scheduler-managed ECS systems.
}

#[no_mangle]
pub extern "C" fn project_schema_fingerprint() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    std::any::TypeId::of::<PhysicsState>().hash(&mut hasher);
    std::mem::size_of::<PhysicsState>().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ball_bounces_off_floor() {
        let mut state = initial_physics_state();
        state.position_y = FLOOR_Y - state.radius;
        state.velocity_y = 100.0;

        simulate_ball(&mut state);

        assert_eq!(state.position_y, FLOOR_Y - state.radius);
        assert!(state.velocity_y < 0.0);
    }

    #[test]
    fn inactive_ball_does_not_move() {
        let mut state = initial_physics_state();
        state.active = false;
        let before = state;

        simulate_ball(&mut state);

        assert_eq!(state.position_x, before.position_x);
        assert_eq!(state.position_y, before.position_y);
        assert_eq!(state.velocity_x, before.velocity_x);
        assert_eq!(state.velocity_y, before.velocity_y);
    }

    /// A weak floor bounce is restored so balls never settle on the floor.
    #[test]
    fn ball_never_rests_on_the_floor() {
        let mut state = initial_physics_state();
        state.position_y = FLOOR_Y - state.radius;
        state.velocity_y = 5.0;

        simulate_ball(&mut state);

        assert!(state.velocity_y <= -MINIMUM_BOUNCE_VELOCITY_Y);
    }
}
