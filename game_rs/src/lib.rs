//! Hot-reloadable bouncing-ball game implemented with ECS systems.

use ecs_hybrid::*;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use trait_type_map::impl_trait_accessible;

// =============================================================================
// Ball physics component
// =============================================================================

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

fn update_time_system(mut time: ResMut<SimulationTime>) {
    let now = Instant::now();
    let mut time = time
        .get_mut()
        .expect("SimulationTime is inserted during game initialization");
    time.delta_seconds = now.duration_since(time.last_frame).as_secs_f32().min(0.1);
    time.last_frame = now;
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
        if state.velocity_y.abs() < 10.0 {
            state.velocity_y = 0.0;
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
fn physics_system(time: Res<SimulationTime>, mut query: Query<&mut PhysicsState>) {
    let delta_seconds = time
        .get()
        .expect("SimulationTime is inserted during game initialization")
        .delta_seconds;
    for mut physics in query.iter_mut() {
        physics.delta_time = delta_seconds;
        simulate_ball(&mut physics);
    }
}

#[cfg(feature = "rendering")]
fn physics_system(
    time: Res<SimulationTime>,
    mut query: Query<(&mut PhysicsState, &mut Position, &mut Sprite)>,
) {
    let delta_seconds = time
        .get()
        .expect("SimulationTime is inserted during game initialization")
        .delta_seconds;
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
}

// =============================================================================
// Game module entry points
// =============================================================================

#[no_mangle]
pub extern "C" fn game_init(api: *const EngineApi) {
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

        entity.build().expect("ball components should be registered");
    }
}

#[no_mangle]
pub extern "C" fn game_update(_api: *const EngineApi) {
    // Gameplay is executed entirely by scheduler-managed ECS systems.
}

#[no_mangle]
pub extern "C" fn game_schema_fingerprint() -> u64 {
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
}
