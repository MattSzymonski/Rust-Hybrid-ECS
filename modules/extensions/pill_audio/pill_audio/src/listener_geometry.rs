//! Where a listener's ears sit in the world.
//!
//! # Responsibilities
//!
//! - Turns a listener's position and facing into the two ear points rodio
//!   pans against.
//!
//! # Design
//!
//! Split from the system so the geometry is testable without an audio device,
//! and so the change needed when a 3D transform arrives is confined to one
//! function. The world is 2D, so both points are lifted onto z = 0.

// Current crate
use crate::audio_manager::EAR_SEPARATION;

// =============================================================================
// Ear placement
// =============================================================================

/// Where a listener's ears sit, given where it stands and which way it faces.
///
/// Returns `(left, right)` in the 3D space rodio expects, with the world's 2D
/// plane laid on z = 0.
///
/// Split out from [`audio_system`] so the geometry is testable without an
/// audio device, and so the change needed when a 3D transform arrives is
/// confined to one function.
pub fn ear_positions(position: (f32, f32), facing_degrees: f32) -> ([f32; 3], [f32; 3]) {
    let facing = facing_degrees.to_radians();
    // Ears sit on the axis perpendicular to `facing`: rotating the facing
    // vector by a quarter turn gives the left ear's direction.
    let (sin, cos) = facing.sin_cos();
    let left = [
        position.0 - sin * EAR_SEPARATION,
        position.1 + cos * EAR_SEPARATION,
        0.0,
    ];
    let right = [
        position.0 + sin * EAR_SEPARATION,
        position.1 - cos * EAR_SEPARATION,
        0.0,
    ];
    (left, right)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance for comparing ear positions: the geometry is two trig calls,
    /// so the error is far below any audible difference.
    const EPSILON: f32 = 1e-5;

    /// Facing +X puts the left ear at +Y and the right at -Y, which is the
    /// convention every other angle is measured against.
    #[test]
    fn ears_straddle_the_facing_direction() {
        let (left, right) = ear_positions((0.0, 0.0), 0.0);
        assert!((left[0] - 0.0).abs() < EPSILON);
        assert!((left[1] - EAR_SEPARATION).abs() < EPSILON);
        assert!((right[0] - 0.0).abs() < EPSILON);
        assert!((right[1] + EAR_SEPARATION).abs() < EPSILON);
    }

    /// A quarter turn swaps which world axis the ears lie on.
    #[test]
    fn turning_the_listener_rotates_its_ears() {
        let (left, right) = ear_positions((0.0, 0.0), 90.0);
        assert!((left[0] + EAR_SEPARATION).abs() < EPSILON);
        assert!((left[1] - 0.0).abs() < EPSILON);
        assert!((right[0] - EAR_SEPARATION).abs() < EPSILON);
        assert!((right[1] - 0.0).abs() < EPSILON);
    }

    /// Ears are placed relative to where the listener stands.
    #[test]
    fn ears_follow_the_listener_position() {
        let (left, right) = ear_positions((10.0, -5.0), 0.0);
        assert!((left[0] - 10.0).abs() < EPSILON);
        assert!((left[1] - (-5.0 + EAR_SEPARATION)).abs() < EPSILON);
        assert!((right[0] - 10.0).abs() < EPSILON);
        assert!((right[1] - (-5.0 - EAR_SEPARATION)).abs() < EPSILON);
    }

    /// The ears stay one separation either side of the listener at any angle,
    /// which is what keeps panning strength constant as it turns.
    #[test]
    fn ear_separation_is_preserved_under_rotation() {
        for degrees in [0.0, 37.0, 90.0, 180.0, 271.5] {
            let (left, right) = ear_positions((3.0, 4.0), degrees);
            let span = ((left[0] - right[0]).powi(2) + (left[1] - right[1]).powi(2)).sqrt();
            assert!(
                (span - 2.0 * EAR_SEPARATION).abs() < EPSILON,
                "separation drifted at {degrees} degrees"
            );
        }
    }
}
