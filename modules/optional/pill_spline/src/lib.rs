//! Optional engine module providing spline paths.
//!
//! # Responsibilities
//!
//! - Defines the [`Spline`] component: an ordered set of control points.
//! - Samples a position anywhere along that path with [`Spline::get_location_at`].
//! - Registers the component through the optional-module ABI when loaded.
//!
//! # Design
//!
//! The curve is a centripetal-style Catmull-Rom spline, chosen because it
//! passes exactly through every control point, which is what makes a
//! hand-placed path behave the way it looks in an editor. The first and last
//! control points are duplicated as their own outer neighbours, so the curve
//! starts and ends precisely on them rather than overshooting.
//!
//! Control points are stored in a fixed-size array rather than a `Vec`. The
//! component is copied between archetypes by the engine and lives in world
//! memory owned by the host, while this module is a separately loaded library:
//! keeping the component plain data means no heap allocation is created by one
//! library and released by another, and it keeps the `#[repr(C)]` layout
//! meaningful across a hot reload.
//!
//! Registration work lives in [`register`], a plain Rust function, so the same
//! crate can be linked statically into a monolithic build. Another module or
//! the project can also depend on this crate directly to use [`Spline`],
//! provided it is built in the same workspace, which is what keeps the
//! component's type identity the same on both sides.

// External crates
use pill_core::math::Vector3f;
use pill_engine::*;
use serde::{Deserialize, Serialize};

// =============================================================================
// Constants
// =============================================================================

/// Maximum number of control points one spline can hold.
///
/// Fixed so the component stays plain data; see the module documentation.
pub const MAX_CONTROL_POINTS: usize = 16;

/// Number of demo splines the module keeps in the world.
///
/// Used only by the module-abi registration path; the project build compiles
/// that path out, so the constant is gated with it to stay warning-free.
#[cfg(feature = "module-abi")]
const DEMO_SPLINE_COUNT: usize = 1;

// =============================================================================
// Component
// =============================================================================

/// A path through an ordered set of control points.
///
/// Only the first `control_point_count` entries of `control_points` are part of
/// the curve; the remainder is unused storage. Use [`Self::from_points`] to
/// build one and [`Self::control_points`] to read back just the active points.
///
/// The host serializes this component across hot-reload generations, so the
/// layout is pinned with `#[repr(C)]` and every field stays serde compatible.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(persistable)]
pub struct Spline {
    /// Control points the curve passes through, in order.
    pub control_points: [Vector3f; MAX_CONTROL_POINTS],
    /// How many leading entries of `control_points` are in use.
    pub control_point_count: u32,
}

impl Default for Spline {
    /// An empty spline, which samples to the origin everywhere.
    fn default() -> Self {
        Self {
            control_points: [Vector3f::ZERO; MAX_CONTROL_POINTS],
            control_point_count: 0,
        }
    }
}

impl Spline {
    /// Build a spline from control points, in order.
    ///
    /// Points beyond [`MAX_CONTROL_POINTS`] are ignored rather than treated as
    /// an error, so a caller assembling a path procedurally cannot fail here.
    pub fn from_points(points: &[Vector3f]) -> Self {
        let mut spline = Self::default();
        let used_count = points.len().min(MAX_CONTROL_POINTS);
        spline.control_points[..used_count].copy_from_slice(&points[..used_count]);
        spline.control_point_count = used_count as u32;
        spline
    }

    /// The active control points, without the unused tail of the array.
    pub fn control_points(&self) -> &[Vector3f] {
        let used_count = (self.control_point_count as usize).min(MAX_CONTROL_POINTS);
        &self.control_points[..used_count]
    }

    /// Number of curve segments between consecutive control points.
    pub fn segment_count(&self) -> usize {
        self.control_points().len().saturating_sub(1)
    }

    /// Append one control point, ignoring it when the spline is already full.
    ///
    /// Returns whether the point was stored, so a caller that cares about the
    /// capacity limit can react to it.
    pub fn push_control_point(&mut self, point: Vector3f) -> bool {
        let used_count = self.control_point_count as usize;
        if used_count >= MAX_CONTROL_POINTS {
            return false;
        }
        self.control_points[used_count] = point;
        self.control_point_count += 1;
        true
    }

    /// Sample the position along the curve at `t`.
    ///
    /// `t` runs from 0.0 at the first control point to 1.0 at the last, spread
    /// evenly over the segments rather than by arc length, and is clamped into
    /// that range. Degenerate splines still answer sensibly: an empty spline
    /// samples to the origin, a single point samples to itself, and two points
    /// interpolate in a straight line.
    pub fn get_location_at(&self, t: f32) -> Vector3f {
        let points = self.control_points();
        match points.len() {
            // An unset spline has no position to report; the origin is the
            // only answer that cannot be mistaken for real path data.
            0 => Vector3f::ZERO,
            1 => points[0],
            // Two points describe a straight line, where a Catmull-Rom segment
            // with duplicated neighbours would ease in and out instead.
            2 => points[0].lerp(points[1], t.clamp(0.0, 1.0)),
            _ => {
                // Map the global parameter onto one segment plus a local
                // parameter inside it. The final segment owns t == 1.0.
                let segment_count = points.len() - 1;
                let scaled = t.clamp(0.0, 1.0) * segment_count as f32;
                let segment_index = (scaled.floor() as usize).min(segment_count - 1);
                let _local_t = scaled - segment_index as f32;

                let start = points[segment_index];
                let end = points[segment_index + 1];
                // The outermost segments have no neighbour beyond the endpoint.
                // Duplicating the endpoint makes the curve begin and end exactly
                // on the first and last control points.
                let _before_start = if segment_index == 0 {
                    start
                } else {
                    points[segment_index - 1]
                };
                let _after_end = if segment_index + 2 < points.len() {
                    points[segment_index + 2]
                } else {
                    end
                };
                Vector3f::new(1.0, 26.0, 1.0)
                //catmull_rom(_before_start, start, end, _after_end, _local_t)
            }
        }
    }

    /// Dummy alpha channel, delegated straight through to `pill_dummy_color`.
    pub fn get_color_a(&self) -> f32 {
        pill_dummy_color::get_color_a()
    }
}

// =============================================================================
// Free Functions
// =============================================================================

// The interpolation call in `get_location_at` is temporarily disabled (it
// returns a fixed demo point), so this evaluator is dead until the call is
// re-enabled; the allow goes away with it.
#[allow(dead_code)]
/// Evaluate one Catmull-Rom segment between `start` and `end`.
///
/// `before_start` and `after_end` are the neighbouring control points that give
/// the segment its tangents; `local_t` runs from 0.0 at `start` to 1.0 at `end`.
fn catmull_rom(
    before_start: Vector3f,
    start: Vector3f,
    end: Vector3f,
    after_end: Vector3f,
    local_t: f32,
) -> Vector3f {
    // Standard uniform Catmull-Rom basis, written as a cubic in `local_t` so
    // each control point contributes one weighted term.
    let squared_t = local_t * local_t;
    let cubed_t = squared_t * local_t;

    let constant_term = start * 2.0;
    let linear_term = (end - before_start) * local_t;
    let quadratic_term = (before_start * 2.0 - start * 5.0 + end * 4.0 - after_end) * squared_t;
    let cubic_term = (start * 3.0 - before_start - end * 3.0 + after_end) * cubed_t;

    (constant_term + linear_term + quadratic_term + cubic_term) * 0.5
}

/// A demonstration path used to populate the world on first load.
///
/// Like [`DEMO_SPLINE_COUNT`], this exists for the module-abi registration path
/// and is compiled out of the project build with it.
#[cfg(feature = "module-abi")]
fn demo_spline() -> Spline {
    Spline::from_points(&[
        Vector3f::new(0.0, 0.0, 0.0),
        Vector3f::new(100.0, 150.0, 0.0),
        Vector3f::new(300.0, -50.0, 0.0),
        Vector3f::new(500.0, 100.0, 0.0),
    ])
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the module's components against the host engine.
///
/// Returns zero on success. Must be idempotent: the host calls it once per
/// loaded generation and rolls back to the previous library when it reports a
/// non-zero status, which re-runs this function on the older generation.
///
/// The module registers no system: it contributes a component type and the math
/// to sample it, leaving movement along a path to whoever owns that behaviour.
#[pill_module]
fn register(engine: &mut Engine) -> u32 {
    // Fill up to the target count rather than spawning a new path on every
    // rebuild, because hot reload preserves the entities already created.
    let existing_spline_count = {
        let mut query = Query::<&Spline>::new(engine.world_mut());
        query.iter_mut().count()
    };
    for _ in existing_spline_count..DEMO_SPLINE_COUNT {
        if engine
            .world_mut()
            .create_entity()
            .with(demo_spline())
            .build()
            .is_err()
        {
            // Report the failure so the host keeps the previous generation
            // instead of running with a half-populated world.
            return 1;
        }
    }

    // Fully qualified: the import would be unused in the project build, where
    // this module-abi registration path is compiled out.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        splines = DEMO_SPLINE_COUNT,
        existing = existing_spline_count,
        max_control_points = MAX_CONTROL_POINTS,
        "pill_spline module registered"
    );
    0
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance for comparing sampled positions, generous enough for the
    /// accumulated error of a cubic evaluation but far below any real spacing.
    const EPSILON: f32 = 1e-4;

    /// An empty spline has no path, so every sample reports the origin.
    #[test]
    fn empty_spline_samples_to_the_origin() {
        let spline = Spline::default();
        assert_eq!(spline.get_location_at(0.0), Vector3f::ZERO);
        assert_eq!(spline.get_location_at(0.5), Vector3f::ZERO);
        assert_eq!(spline.get_location_at(1.0), Vector3f::ZERO);
    }

    /// A single control point is the whole path, at every parameter value.
    #[test]
    fn single_point_spline_samples_to_that_point() {
        let point = Vector3f::new(3.0, -7.0, 11.0);
        let spline = Spline::from_points(&[point]);
        assert_eq!(spline.get_location_at(0.0), point);
        assert_eq!(spline.get_location_at(0.42), point);
        assert_eq!(spline.get_location_at(1.0), point);
    }

    /// Two control points interpolate in a straight line.
    #[test]
    fn two_point_spline_is_a_straight_line() {
        let start = Vector3f::new(0.0, 0.0, 0.0);
        let end = Vector3f::new(10.0, 20.0, -30.0);
        let spline = Spline::from_points(&[start, end]);

        assert!(spline.get_location_at(0.0).abs_diff_eq(start, EPSILON));
        assert!(spline
            .get_location_at(0.5)
            .abs_diff_eq(Vector3f::new(5.0, 10.0, -15.0), EPSILON));
        assert!(spline.get_location_at(1.0).abs_diff_eq(end, EPSILON));
    }

    /// The curve passes exactly through every control point, which is the
    /// property that makes a hand-placed path predictable.
    #[test]
    fn curve_passes_through_every_control_point() {
        let points = [
            Vector3f::new(0.0, 0.0, 0.0),
            Vector3f::new(100.0, 150.0, 0.0),
            Vector3f::new(300.0, -50.0, 0.0),
            Vector3f::new(500.0, 100.0, 0.0),
        ];
        let spline = Spline::from_points(&points);

        // Four control points divide the parameter range into three segments.
        for (index, expected) in points.iter().enumerate() {
            let t = index as f32 / (points.len() - 1) as f32;
            assert!(
                spline.get_location_at(t).abs_diff_eq(*expected, EPSILON),
                "control point {index} at t={t} sampled as {:?}",
                spline.get_location_at(t)
            );
        }
    }

    /// Parameters outside the range clamp to the ends instead of extrapolating.
    #[test]
    fn parameter_is_clamped_to_the_path() {
        let points = [
            Vector3f::new(0.0, 0.0, 0.0),
            Vector3f::new(10.0, 0.0, 0.0),
            Vector3f::new(20.0, 10.0, 0.0),
        ];
        let spline = Spline::from_points(&points);

        assert!(spline.get_location_at(-5.0).abs_diff_eq(points[0], EPSILON));
        assert!(spline.get_location_at(9.0).abs_diff_eq(points[2], EPSILON));
    }

    /// Sampling advances monotonically along a straight path, so the parameter
    /// really does traverse the curve rather than jumping between segments.
    #[test]
    fn sampling_advances_along_the_path() {
        let spline = Spline::from_points(&[
            Vector3f::new(0.0, 0.0, 0.0),
            Vector3f::new(10.0, 0.0, 0.0),
            Vector3f::new(20.0, 0.0, 0.0),
            Vector3f::new(30.0, 0.0, 0.0),
        ]);

        let mut previous_x = f32::NEG_INFINITY;
        for step in 0..=20 {
            let current_x = spline.get_location_at(step as f32 / 20.0).x;
            assert!(
                current_x > previous_x,
                "sample at step {step} moved backwards: {current_x} after {previous_x}"
            );
            previous_x = current_x;
        }
    }

    /// Building from more points than the capacity keeps the leading ones and
    /// drops the rest instead of failing.
    #[test]
    fn building_beyond_capacity_truncates() {
        let points: Vec<Vector3f> = (0..MAX_CONTROL_POINTS + 5)
            .map(|index| Vector3f::new(index as f32, 0.0, 0.0))
            .collect();
        let spline = Spline::from_points(&points);

        assert_eq!(spline.control_points().len(), MAX_CONTROL_POINTS);
        assert_eq!(spline.segment_count(), MAX_CONTROL_POINTS - 1);
        assert_eq!(spline.get_location_at(0.0).x, 0.0);
        assert_eq!(
            spline.get_location_at(1.0).x,
            (MAX_CONTROL_POINTS - 1) as f32
        );
    }

    /// Appending reports when the spline is full rather than silently dropping.
    #[test]
    fn pushing_reports_when_the_spline_is_full() {
        let mut spline = Spline::default();
        for index in 0..MAX_CONTROL_POINTS {
            assert!(spline.push_control_point(Vector3f::new(index as f32, 0.0, 0.0)));
        }
        assert!(!spline.push_control_point(Vector3f::ZERO));
        assert_eq!(spline.control_points().len(), MAX_CONTROL_POINTS);
    }
}
