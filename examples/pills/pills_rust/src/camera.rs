//! The free flying camera: movement, view and view width, in one system.
//!
//! # Responsibilities
//!
//! - Fly with WASD, rise and sink with E and Q, sprint with Shift, look around
//!   by dragging with the right mouse button, and widen or narrow the view with
//!   T and G ([`camera_system`]).
//!
//! # Design
//!
//! It is one system because it is one camera: a separate view-width system
//! would need a second query over the same rows and a second place the camera's
//! limits live. Both the view and the velocity are eased towards their target,
//! so the keys and the mouse feel the same at any frame rate.
//!
//! A drag is the camera's unless the overlay claims the pointer: a drag that
//! starts on a panel belongs to that panel, so the view stays put while the UI
//! is being used.

// External crates
use glam::{EulerRot, Quat, Vec3};
use pill_engine::{pill_hot, Input, KeyCode, MouseButton, Query, Res, SystemError, Time};
use pill_master_renderer_data::{CameraComponent, TransformComponent};
use pill_project_ui::GameUi;

// Current crate
use pills_rust::CameraMovementComponent;

/// The view never tilts this far up or down, so it cannot flip over.
const PITCH_LIMIT_DEGREES: f32 = 89.0;

/// The narrowest and widest field of view, in degrees, both exclusive.
const FOV_RANGE: std::ops::Range<f32> = 10.0..120.0;

/// Degrees the field of view changes per second while T or G is held.
const FOV_DEGREES_PER_SECOND: f32 = 100.0;

/// Flies the camera from the keys, turns it from right button drags and widens
/// or narrows its view.
#[pill_hot]
pub(crate) fn camera_system(
    input: Res<Input>,
    game_ui: Res<GameUi>,
    time: Res<Time>,
    mut cameras: Query<(
        &mut TransformComponent,
        &mut CameraComponent,
        &mut CameraMovementComponent,
    )>,
) -> Result<(), SystemError> {
    let (Some(input), Some(time)) = (input.get(), time.get()) else {
        return Ok(());
    };
    let delta_time = time.delta_seconds();
    let ui_wants_pointer = game_ui.get().is_some_and(GameUi::wants_pointer);

    // The window does not capture the cursor, so the view only follows the
    // mouse while the right button is held over the scene.
    let mouse_delta = if input.mouse_button_held(MouseButton::Right) && !ui_wants_pointer {
        input.mouse_delta()
    } else {
        glam::Vec2::ZERO
    };

    // T widens the view and G narrows it; holding both leaves it alone.
    let fov_change = match (input.key_held(KeyCode::KeyT), input.key_held(KeyCode::KeyG)) {
        (true, false) => FOV_DEGREES_PER_SECOND,
        (false, true) => -FOV_DEGREES_PER_SECOND,
        _ => 0.0,
    };

    for (mut transform, mut camera, mut movement) in cameras.iter_mut() {
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
        // Yaw turns around the world's up axis and pitch tilts the view; with
        // both at zero the camera looks down its local -Z.
        let rotation = Quat::from_euler(
            EulerRot::YXZ,
            movement.current_rotation[1].to_radians(),
            movement.current_rotation[0].to_radians(),
            0.0,
        );
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

        let fov = camera.vertical_fov + fov_change * delta_time;
        if FOV_RANGE.contains(&fov) {
            camera.vertical_fov = fov;
        }
    }
    Ok(())
}
