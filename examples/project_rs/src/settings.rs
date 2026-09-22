//! Scene counts, appearance, and physics tuning.

use pill_engine::common_components::Color;

pub const FIXED_DELTA_TIME: f32 = 1.0 / 60.0;
pub const GRAVITY: f32 = 800.0;
pub const BOUNCE_VELOCITY_Y: f32 = -800.0;
pub const BOUNCE_VELOCITY_X: f32 = 350.0;
pub const RESTITUTION: f32 = 0.6;

/// Upward speed restored when a floor bounce would otherwise decay to rest.
///
/// Keeps every ball visibly bouncing for the whole lifetime of the scene
/// instead of settling on the floor after a few seconds.
pub const MINIMUM_BOUNCE_VELOCITY_Y: f32 = 500.0;

pub const FLOOR_Y: f32 = 580.0;
pub const CEILING_Y: f32 = 20.0;
pub const LEFT_WALL: f32 = 20.0;
pub const RIGHT_WALL: f32 = 780.0;

/// Number of balls in the scene, and with it the number of control points the
/// spline is driven from.
///
/// Must stay at or below `pill_spline::MAX_CONTROL_POINTS`, the length of the
/// control point array a [`pill_spline::Spline`] stores.
pub const BALL_COUNT: usize = 5;

/// Curve parameter between two neighbouring sample dots.
pub const SPLINE_SAMPLE_STEP: f32 = 0.05;

/// Number of sample dots: one every [`SPLINE_SAMPLE_STEP`], from `t = 0.0`
/// through `t = 1.0`, so both endpoints of the curve get a dot of their own.
pub const SPLINE_SAMPLE_COUNT: usize = 21;

/// Edge length of a sample dot, in pixels.
pub const SPLINE_SAMPLE_DOT_SIZE: f32 = 6.0;

/// Fill colour of the ball meshes.
pub const BALL_COLOR: Color = Color::new(1.0, 0.3, 0.3, 1.0);

/// Fill colour of the sample dots.
pub const SAMPLE_DOT_COLOR: Color = Color::new(0.25, 0.85, 1.0, 1.0);
