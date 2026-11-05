//! Draws every collider as an outline, for looking at what the simulation sees.
//!
//! # Responsibilities
//!
//! - Draws the shape of each simulated collider through the engine's
//!   [`DebugDraw`] collection, at the pose the entity's transform holds.
//! - Draws nothing at all unless the settings document turns outlines on.

// External crates
use pill_engine::{
    system, DebugDraw, DebugStyle, Query, Res, ResMut, SystemError, TransformComponent,
};
use rapier3d::math::{Rotation, Vector};

// Current crate
use crate::components::{ColliderComponent, ColliderShape};
use crate::manager::PhysicsManager;

/// The green every collider outline is drawn in.
const OUTLINE_COLOR: [f32; 4] = [0.35, 0.95, 0.55, 1.0];

// =============================================================================
// physics_debug_draw
// =============================================================================

/// Draws the outline of every collider while the settings document asks for it.
///
/// # Errors
///
/// Returns [`SystemError::MissingResource`] when the physics manager or the
/// engine's debug collection is absent, which only happens in a world this
/// module was not registered in.
#[system(name = "physics_debug_draw", order = 901)]
pub fn physics_debug_draw(
    manager: Res<PhysicsManager>,
    mut debug_draw: ResMut<DebugDraw>,
    mut colliders: Query<(&ColliderComponent, &TransformComponent)>,
) -> Result<(), SystemError> {
    let Some(manager) = manager.get() else {
        return Err(SystemError::MissingResource {
            name: String::from("PhysicsManager"),
        });
    };
    if !manager.debug_draw_enabled() {
        return Ok(());
    }
    let Some(mut debug_draw) = debug_draw.get_mut() else {
        return Err(SystemError::MissingResource {
            name: String::from("DebugDraw"),
        });
    };
    let style = DebugStyle::colored(OUTLINE_COLOR);
    for (collider, transform) in colliders.iter_mut() {
        let [first, second, third] = collider.dimensions;
        match collider.shape {
            ColliderShape::Ball => {
                debug_draw.sphere(transform.translation, first, style);
            }
            ColliderShape::Cuboid => {
                draw_box(&mut debug_draw, transform, [first, second, third], style);
            }
            ColliderShape::Capsule | ColliderShape::Cylinder => {
                // Both stand on their y axis: the ends and the shaft between
                // them show where the shape is without drawing every curve.
                let half_height = first;
                let radius = second;
                let [x, y, z] = transform.translation;
                let bottom = [x, y - half_height, z];
                let top = [x, y + half_height, z];
                debug_draw.line(bottom, top, style);
                debug_draw.sphere(bottom, radius, style);
                debug_draw.sphere(top, radius, style);
            }
        }
    }
    Ok(())
}

// =============================================================================
// Box outlines
// =============================================================================

/// Draws the twelve edges of a box that is offset by the entity's pose.
pub fn draw_box(
    debug_draw: &mut DebugDraw,
    transform: &TransformComponent,
    half_extents: [f32; 3],
    style: DebugStyle,
) {
    let rotation = rotation_of(transform);
    let center = transform.translation;
    // Step 1: The eight corners, each rotated into world space.
    let mut corners = [[0.0_f32; 3]; 8];
    for (index, corner) in corners.iter_mut().enumerate() {
        let offset = [
            if index & 1 == 0 {
                -half_extents[0]
            } else {
                half_extents[0]
            },
            if index & 2 == 0 {
                -half_extents[1]
            } else {
                half_extents[1]
            },
            if index & 4 == 0 {
                -half_extents[2]
            } else {
                half_extents[2]
            },
        ];
        let turned = rotation.mul_vec3(Vector::new(offset[0], offset[1], offset[2]));
        *corner = [
            center[0] + turned.x,
            center[1] + turned.y,
            center[2] + turned.z,
        ];
    }
    // Step 2: A corner joins the three whose index differs in exactly one bit.
    for (index, corner) in corners.iter().enumerate() {
        for bit in [1, 2, 4] {
            let other = index ^ bit;
            if other < index {
                debug_draw.line(*corner, corners[other], style);
            }
        }
    }
}

/// The entity's rotation, treating a value that is not usable as the identity.
pub fn rotation_of(transform: &TransformComponent) -> Rotation {
    let [x, y, z, w] = transform.rotation;
    let length_squared = x * x + y * y + z * z + w * w;
    if !length_squared.is_finite() || length_squared < f32::EPSILON {
        return Rotation::IDENTITY;
    }
    Rotation::from_xyzw(x, y, z, w).normalize()
}
