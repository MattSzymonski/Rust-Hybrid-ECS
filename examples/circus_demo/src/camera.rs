//! The free flying camera and its field of view keys.
//!
//! # Responsibilities
//!
//! - Fly the camera with WASD, rise and sink with E and Q, sprint with Shift,
//!   and look around by dragging with the right mouse button
//!   ([`fps_camera_system`]). Both movement and view are smoothed.
//! - Widen and narrow the view with T and G ([`camera_fov_changing_system`]).

// External crates
use glam::{EulerRot, Quat, Vec3};
use pill_engine::{pill_hot, Input, KeyCode, MouseButton, Query, Res, SystemError, Time};
use pill_master_renderer_data::{CameraComponent, TransformComponent};

// Current crate
use crate::game::CameraMovementComponent;

/// Key that widens the view.
const INCREASE_CAMERA_FOV_BUTTON: KeyCode = KeyCode::KeyT;

/// Key that narrows the view.
const DECREASE_CAMERA_FOV_BUTTON: KeyCode = KeyCode::KeyG;

/// Degrees the field of view changes per second while its key is held.
const FOV_DEGREES_PER_SECOND: f32 = 100.0;

/// The narrowest and widest field of view, in degrees, both exclusive.
const FOV_RANGE: (f32, f32) = (10.0, 120.0);

/// The view never tilts this far up or down, so it cannot flip over.
const PITCH_LIMIT_DEGREES: f32 = 89.0;

/// The camera's orientation for a pitch and a yaw, in degrees.
///
/// Yaw turns around the world's up axis and pitch tilts the view up and down;
/// with both at zero the camera looks down -Z.
pub(crate) fn view_rotation(pitch_yaw_degrees: [f32; 3]) -> Quat {
    Quat::from_euler(
        EulerRot::YXZ,
        pitch_yaw_degrees[1].to_radians(),
        pitch_yaw_degrees[0].to_radians(),
        0.0,
    )
}

/// Flies the camera from the keys and turns it from right button drags.
#[pill_hot]
pub(crate) fn fps_camera_system(
    input: Res<Input>,
    time: Res<Time>,
    mut cameras: Query<(
        &mut TransformComponent,
        &CameraComponent,
        &mut CameraMovementComponent,
    )>,
) -> Result<(), SystemError> {
    let (Some(input), Some(time)) = (input.get(), time.get()) else {
        return Ok(());
    };
    let delta_time = time.delta_seconds();

    // The window does not capture the cursor, so the view only follows the
    // mouse while the right button is held.
    let mouse_delta = if input.mouse_button_held(MouseButton::Right) {
        input.mouse_delta()
    } else {
        glam::Vec2::ZERO
    };

    for (mut transform, camera, mut movement) in cameras.iter_mut() {
        if !camera.enabled {
            continue;
        }

        // Moving the mouse right turns right, moving it down looks down.
        movement.target_rotation[1] -= mouse_delta.x * movement.mouse_sensitivity;
        movement.target_rotation[0] -= mouse_delta.y * movement.mouse_sensitivity;
        movement.target_rotation[0] =
            movement.target_rotation[0].clamp(-PITCH_LIMIT_DEGREES, PITCH_LIMIT_DEGREES);

        // Ease the view towards where the mouse points.
        let rotation_blend = 1.0 - (-movement.rotation_lerp_speed * delta_time).exp();
        for axis in 0..2 {
            movement.current_rotation[axis] +=
                (movement.target_rotation[axis] - movement.current_rotation[axis]) * rotation_blend;
        }
        let rotation = view_rotation(movement.current_rotation);
        transform.rotation = rotation.to_array();

        // Walk on the horizontal plane the camera faces; E and Q fly straight
        // up and down.
        let forward = (rotation * Vec3::NEG_Z).with_y(0.0).normalize_or_zero();
        let right = (rotation * Vec3::X).with_y(0.0).normalize_or_zero();
        let mut direction = Vec3::ZERO;
        let held = |key| input.key_held(key);
        if held(KeyCode::KeyW) {
            direction += forward;
        }
        if held(KeyCode::KeyS) {
            direction -= forward;
        }
        if held(KeyCode::KeyD) {
            direction += right;
        }
        if held(KeyCode::KeyA) {
            direction -= right;
        }
        if held(KeyCode::KeyE) {
            direction.y += 1.0;
        }
        if held(KeyCode::KeyQ) {
            direction.y -= 1.0;
        }

        let speed = if held(KeyCode::ShiftLeft) {
            movement.move_speed * movement.sprint_multiplier
        } else {
            movement.move_speed
        };
        let target_velocity = direction.normalize_or_zero() * speed;
        movement.target_velocity = target_velocity.to_array();

        // Ease the velocity, so starting and stopping are smooth.
        let velocity_blend = 1.0 - (-movement.lerp_speed * delta_time).exp();
        let current_velocity = Vec3::from_array(movement.current_velocity);
        let current_velocity =
            current_velocity + (target_velocity - current_velocity) * velocity_blend;
        movement.current_velocity = current_velocity.to_array();

        let position = Vec3::from_array(transform.translation) + current_velocity * delta_time;
        transform.translation = position.to_array();
    }
    Ok(())
}

/// Widens or narrows every camera's view while T or G is held.
#[pill_hot]
pub(crate) fn camera_fov_changing_system(
    input: Res<Input>,
    time: Res<Time>,
    mut cameras: Query<&mut CameraComponent>,
) -> Result<(), SystemError> {
    let (Some(input), Some(time)) = (input.get(), time.get()) else {
        return Ok(());
    };
    let mut change = 0.0;
    if input.key_held(INCREASE_CAMERA_FOV_BUTTON) {
        change += 1.0;
    }
    if input.key_held(DECREASE_CAMERA_FOV_BUTTON) {
        change -= 1.0;
    }
    if change == 0.0 {
        return Ok(());
    }

    let (narrowest, widest) = FOV_RANGE;
    for mut camera in cameras.iter_mut() {
        let fov = camera.vertical_fov + change * FOV_DEGREES_PER_SECOND * time.delta_seconds();
        if fov > narrowest && fov < widest {
            camera.vertical_fov = fov;
        }
    }
    Ok(())
}
