//! The demo's components, its tuning resource, the level, and the keys that
//! tune the curl noise field live.
//!
//! # Responsibilities
//!
//! - Define [`CurlNoiseComponent`], [`CameraMovementComponent`] and the
//!   [`DemoState`] resource, and register them ([`register`]).
//! - Build the level once ([`create_scene`]): the camera, the pillars, the
//!   ground and [`PILL_COUNT`] pills scattered above it.
//! - Let the player tune the field with the keyboard ([`demo_control_system`]).

// Standard library
use std::f32::consts::PI;

// External crates
use pill_engine::{
    pill_hot, tracing, Input, KeyCode, PillComponent, Query, Res, ResMut, Resource, SystemError,
    World,
};
use pill_master_renderer_data::{CameraComponent, MeshRendererComponent, TransformComponent};
use serde::{Deserialize, Serialize};

// Current crate
use crate::resources::SceneAssets;

/// How many pills drift through the field.
pub(crate) const PILL_COUNT: usize = 200_000;

/// Uniform scale of every pill.
const PILL_SCALE: f32 = 0.35;

/// Where the camera starts: at eye height, ten units in front of the origin.
const CAMERA_START: [f32; 3] = [0.0, 1.6, -10.0];

/// The camera starts turned around to face the origin, because a camera looks
/// down its local -Z axis.
const CAMERA_START_YAW_DEGREES: f32 = 180.0;

/// The keys that tune the field, as (key, field it changes, step per frame).
const TUNING_KEYS: &[(KeyCode, TunedField, f32)] = &[
    (KeyCode::KeyO, TunedField::Scale, 0.0005),
    (KeyCode::KeyP, TunedField::Scale, -0.0005),
    (KeyCode::KeyL, TunedField::Attraction, 5.0),
    (KeyCode::KeyK, TunedField::Attraction, -5.0),
    (KeyCode::KeyM, TunedField::Damping, 0.01),
    (KeyCode::KeyN, TunedField::Damping, -0.01),
    (KeyCode::KeyV, TunedField::Epsilon, 0.001),
    (KeyCode::KeyB, TunedField::Epsilon, -0.001),
];

/// One pill's motion through the curl noise field.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(shared = "circus_demo::CurlNoiseComponent", persistable)]
pub struct CurlNoiseComponent {
    /// Current velocity, in world units per second.
    pub velocity: [f32; 3],
    /// How strongly the field pushes this pill.
    pub curl_strength: f32,
    /// Per-pill noise scale, drawn at spawn; the field uses the shared scale.
    pub noise_scale: f32,
}

/// Settings and smoothed state of the free flying camera.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(shared = "circus_demo::CameraMovementComponent", persistable)]
pub struct CameraMovementComponent {
    /// Flying speed, in world units per second.
    pub move_speed: f32,
    /// Speed factor while Shift is held.
    pub sprint_multiplier: f32,
    /// How quickly the velocity follows the keys.
    pub lerp_speed: f32,
    /// Degrees turned per pixel of mouse movement.
    pub mouse_sensitivity: f32,
    /// How quickly the view follows the mouse.
    pub rotation_lerp_speed: f32,
    /// Velocity this frame, in world units per second.
    pub current_velocity: [f32; 3],
    /// Velocity the keys ask for.
    pub target_velocity: [f32; 3],
    /// View this frame: pitch and yaw in degrees (the third value is unused).
    pub current_rotation: [f32; 3],
    /// View the mouse asks for: pitch and yaw in degrees.
    pub target_rotation: [f32; 3],
}

/// The curl noise field's live tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DemoState {
    /// Spatial frequency of the noise.
    pub curl_scale: f32,
    /// Finite difference step used to take the curl.
    pub curl_epsilon: f32,
    /// Pull towards the middle of the field.
    pub curl_attraction: f32,
    /// Velocity multiplier applied every frame.
    pub curl_damping: f32,
}

impl Default for DemoState {
    /// The values the original demo was tuned to.
    fn default() -> Self {
        Self {
            curl_scale: 0.1005,
            curl_epsilon: 0.0038,
            curl_attraction: 155.0,
            curl_damping: 2.91,
        }
    }
}

impl Resource for DemoState {
    fn shared_name() -> Option<&'static str> {
        Some("circus_demo::DemoState")
    }
}

/// A [`DemoState`] field a tuning key changes.
#[derive(Debug, Clone, Copy)]
enum TunedField {
    Scale,
    Attraction,
    Damping,
    Epsilon,
}

/// Registers the demo's components and its tuning resource, and inserts the
/// resource unless a previous generation left one behind.
pub(crate) fn register(world: &mut World) {
    __pill_register_CurlNoiseComponent(world);
    __pill_register_CameraMovementComponent(world);
    world.register_persistable_resource::<DemoState>();
    if !world.has_resource::<DemoState>() {
        world.insert_resource(DemoState::default());
    }
}

/// Builds the level into a world that does not hold it yet.
///
/// # Errors
///
/// Returns the engine's entity creation error as text.
pub(crate) fn create_scene(world: &mut World, assets: &SceneAssets) -> Result<(), String> {
    if Query::<&CameraMovementComponent>::new(world)
        .iter_mut()
        .next()
        .is_none()
    {
        spawn_camera(world)?;
    }
    if Query::<&CurlNoiseComponent>::new(world)
        .iter_mut()
        .next()
        .is_none()
    {
        spawn_level(world, assets)?;
        spawn_pills(world, assets, PILL_COUNT)?;
    }
    Ok(())
}

/// Spawns the free flying camera.
fn spawn_camera(world: &mut World) -> Result<(), String> {
    let start_rotation = [0.0, CAMERA_START_YAW_DEGREES, 0.0];
    world
        .create_entity()
        .with(TransformComponent {
            translation: CAMERA_START,
            rotation: crate::camera::view_rotation(start_rotation).to_array(),
            ..Default::default()
        })
        .with(CameraComponent {
            vertical_fov: 75.0,
            ..Default::default()
        })
        .with(CameraMovementComponent {
            move_speed: 25.0,
            sprint_multiplier: 2.0,
            lerp_speed: 8.0,
            mouse_sensitivity: 0.1,
            rotation_lerp_speed: 25.0,
            current_rotation: start_rotation,
            target_rotation: start_rotation,
            ..Default::default()
        })
        .build()
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Spawns the pillars and the ground, both at the origin.
fn spawn_level(world: &mut World, assets: &SceneAssets) -> Result<(), String> {
    let pieces = [
        (&assets.pillars_mesh, &assets.pillars_material),
        (&assets.ground_mesh, &assets.ground_material),
    ];
    for (mesh, material) in pieces {
        world
            .create_entity()
            .with(TransformComponent::default())
            .with(
                MeshRendererComponent::builder()
                    .mesh(mesh)
                    .material(material)
                    .build(),
            )
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Scatters `count` pills above the ground, each turned and pushed at random.
fn spawn_pills(world: &mut World, assets: &SceneAssets, count: usize) -> Result<(), String> {
    let mut random = SmallRandom::new(0x00C1_4C05);
    let renderer = MeshRendererComponent::builder()
        .mesh(&assets.pill_mesh)
        .material(&assets.pill_material)
        .build();

    for _ in 0..count {
        let rotation = glam::Quat::from_euler(
            glam::EulerRot::XYZ,
            random.range(0.0, 2.0 * PI),
            random.range(0.0, 2.0 * PI),
            random.range(0.0, 2.0 * PI),
        );
        world
            .create_entity()
            .with(CurlNoiseComponent {
                velocity: [0.0; 3],
                curl_strength: random.range(70.0, 120.0),
                noise_scale: random.range(0.1, 0.2),
            })
            .with(TransformComponent {
                translation: [
                    random.range(-20.0, 20.0),
                    random.range(0.0, 40.0),
                    random.range(-20.0, 20.0),
                ],
                rotation: rotation.to_array(),
                scale: [PILL_SCALE; 3],
            })
            .with(renderer)
            .build()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Tunes the curl noise field while its keys are held, and logs the new
/// values on every frame that changed them.
#[pill_hot]
pub(crate) fn demo_control_system(
    input: Res<Input>,
    mut demo_state: ResMut<DemoState>,
) -> Result<(), SystemError> {
    let Some(input) = input.get() else {
        return Ok(());
    };
    let Some(mut demo_state) = demo_state.get_mut() else {
        return Err(SystemError::MissingResource {
            name: String::from("DemoState"),
        });
    };

    let mut changed = false;
    for (key, field, step) in TUNING_KEYS {
        if !input.key_held(*key) {
            continue;
        }
        let value = match field {
            TunedField::Scale => &mut demo_state.curl_scale,
            TunedField::Attraction => &mut demo_state.curl_attraction,
            TunedField::Damping => &mut demo_state.curl_damping,
            TunedField::Epsilon => &mut demo_state.curl_epsilon,
        };
        *value += step;
        changed = true;
    }

    if changed {
        tracing::info!(
            "[circus_demo] curl scale: {:.4}, attraction: {:.2}, damping: {:.4}, epsilon: {:.4}",
            demo_state.curl_scale,
            demo_state.curl_attraction,
            demo_state.curl_damping,
            demo_state.curl_epsilon
        );
    }
    Ok(())
}

/// A small deterministic random number generator (SplitMix64).
///
/// Used instead of a thread-local generator: a thread-local value owned by a
/// project DLL would register a destructor in that DLL, which a hot reload
/// unmaps while the thread lives on.
struct SmallRandom {
    state: u64,
}

impl SmallRandom {
    /// A generator seeded with `seed`; equal seeds give equal sequences.
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next value, uniform in `[minimum, maximum)`.
    fn range(&mut self, minimum: f32, maximum: f32) -> f32 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^= mixed >> 31;
        // The top 24 bits fill an f32 mantissa exactly.
        let unit = (mixed >> 40) as f32 / (1u64 << 24) as f32;
        minimum + unit * (maximum - minimum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_values_stay_in_range() {
        let mut random = SmallRandom::new(7);
        for _ in 0..10_000 {
            let value = random.range(-20.0, 40.0);
            assert!((-20.0..40.0).contains(&value), "{value}");
        }
    }
}
