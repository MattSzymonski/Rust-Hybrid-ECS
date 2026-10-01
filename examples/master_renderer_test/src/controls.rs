//! Mouse, keyboard and gamepad controls over the helmet and the camera.
//!
//! - Drag with the left mouse button, hold the arrow keys or A/D, or push the
//!   left stick to turn the helmet; dragging up and down tilts it.
//! - Scroll, or push the right stick up and down, to move the camera closer
//!   or farther.
//! - Gamepad A rumbles the pad that pressed it.
//! - Every key press is logged, so a run's input can be read from its log.

use crate::TagHelmet;
use pill_engine::{
    pill_hot, tracing, GamepadAxis, GamepadButton, Input, KeyCode, MouseButton, Query, Res,
    ResMut, SystemError, Time,
};
use pill_master_renderer_data::{CameraComponent, TransformComponent};
use std::time::Duration;

/// Helmet turn per physical pixel dragged.
const DRAG_RADIANS_PER_PIXEL: f32 = 0.01;

/// Helmet turn per second with a key held or the stick pushed fully.
const KEY_TURN_RADIANS_PER_SECOND: f32 = std::f32::consts::PI;

/// Camera travel per wheel line, and per second with the stick pushed fully.
const ZOOM_UNITS_PER_STEP: f32 = 0.25;

/// Touchpads report pixels; this many make one wheel line.
const PIXELS_PER_LINE: f32 = 40.0;

/// How close and how far the camera may go.
const CAMERA_DISTANCE_RANGE: (f32, f32) = (1.0, 8.0);

/// Turns the helmet from mouse drags, keys and the left stick.
#[pill_hot]
pub(crate) fn helmet_control_system(
    input: Res<Input>,
    time: Res<Time>,
    mut helmets: Query<(&mut TransformComponent, &TagHelmet)>,
) -> Result<(), SystemError> {
    let (Some(input), Some(time)) = (input.get(), time.get()) else {
        return Ok(());
    };

    for key in input.keys_pressed() {
        tracing::info!("[master_renderer_test] key pressed: {}", key.code_name());
    }
    for button in MouseButton::ALL {
        if input.mouse_button_pressed(button) {
            tracing::info!("[master_renderer_test] mouse button pressed: {button:?}");
        }
    }

    // Yaw around the world's up axis, pitch around its right axis.
    let mut yaw = 0.0;
    let mut pitch = 0.0;
    if input.mouse_button_held(MouseButton::Left) {
        let drag = input.mouse_delta();
        yaw += drag.x * DRAG_RADIANS_PER_PIXEL;
        pitch += drag.y * DRAG_RADIANS_PER_PIXEL;
    }
    let mut steer = 0.0;
    if input.key_held(KeyCode::ArrowLeft) || input.key_held(KeyCode::KeyA) {
        steer -= 1.0;
    }
    if input.key_held(KeyCode::ArrowRight) || input.key_held(KeyCode::KeyD) {
        steer += 1.0;
    }
    for player in input.connected_gamepads() {
        steer += input.gamepad_axis(player, GamepadAxis::LeftStickX);
    }
    yaw += steer.clamp(-1.0, 1.0) * KEY_TURN_RADIANS_PER_SECOND * time.delta_seconds();
    if yaw == 0.0 && pitch == 0.0 {
        return Ok(());
    }

    let turn = glam::Quat::from_rotation_y(yaw) * glam::Quat::from_rotation_x(pitch);
    for (mut transform, _) in helmets.iter_mut() {
        let current = glam::Quat::from_array(transform.rotation);
        let current = if current.is_finite() && current.length_squared() > 1.0e-8 {
            current.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        transform.rotation = (turn * current).normalize().to_array();
    }
    Ok(())
}

/// Moves the camera along its view axis from the wheel and the right stick.
#[pill_hot]
pub(crate) fn camera_zoom_system(
    input: Res<Input>,
    time: Res<Time>,
    mut cameras: Query<(&mut TransformComponent, &CameraComponent)>,
) -> Result<(), SystemError> {
    let (Some(input), Some(time)) = (input.get(), time.get()) else {
        return Ok(());
    };

    // Scrolling up (positive) brings the camera closer.
    let mut steps = input.scroll_lines().y + input.scroll_pixels().y / PIXELS_PER_LINE;
    for player in input.connected_gamepads() {
        steps += input.gamepad_axis(player, GamepadAxis::RightStickY) * time.delta_seconds() * 8.0;
    }
    if steps == 0.0 {
        return Ok(());
    }

    let (nearest, farthest) = CAMERA_DISTANCE_RANGE;
    for (mut transform, _) in cameras.iter_mut() {
        let distance = transform.translation[2] - steps * ZOOM_UNITS_PER_STEP;
        transform.translation[2] = distance.clamp(nearest, farthest);
        if input.scroll_lines() != glam::Vec2::ZERO || input.scroll_pixels() != glam::Vec2::ZERO {
            tracing::info!(
                "[master_renderer_test] scrolled; camera distance {:.2}",
                transform.translation[2]
            );
        }
    }
    Ok(())
}

/// Rumbles a gamepad when its A button is pressed.
pub(crate) fn rumble_system(mut input: ResMut<Input>) -> Result<(), SystemError> {
    let Some(mut input) = input.get_mut() else {
        return Ok(());
    };
    let players: Vec<_> = input
        .connected_gamepads()
        .filter(|player| input.gamepad_button_pressed(*player, GamepadButton::A))
        .collect();
    for player in players {
        input.request_rumble(player, 0.4, 0.8, Duration::from_millis(250));
    }
    Ok(())
}
