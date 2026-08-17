//! Math type aliases over `glam` plus the direction vocabulary.
//!
//! # Responsibilities
//!
//! - Provide short, engine-facing aliases for `glam` vectors, matrices, and
//!   colors.
//! - Define the `Direction` vocabulary used by transform and input code.
//!
//! # Design
//!
//! Pure type aliases and one unit enum; `glam` is the only dependency.

// External crates
use glam::{IVec2, Mat3, Mat3A, Mat4, Vec2, Vec3, Vec4};

/// 2D integer vector.
pub type Vector2i = IVec2;

/// 2D float vector.
pub type Vector2f = Vec2;
/// 3D float vector.
pub type Vector3f = Vec3;
/// 4D float vector.
pub type Vector4f = Vec4;

/// RGB color stored as a 3D float vector.
pub type Color = Vec3;

/// 3x3 float matrix.
pub type Matrix3f = Mat3;
/// 3x3 float matrix (column-major variant).
pub type Matrix3fA = Mat3A;

/// 4x4 float matrix.
pub type Matrix4f = Mat4;

/// Axis-aligned direction vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Local forward.
    Forward,
    /// Local backward.
    Backward,
    /// Local right.
    Right,
    /// Local left.
    Left,
    /// Local up.
    Up,
    /// Local down.
    Down,
    /// World forward.
    WorldForward,
    /// World backward.
    WorldBackward,
    /// World right.
    WorldRight,
    /// World left.
    WorldLeft,
    /// World up.
    WorldUp,
    /// World down.
    WorldDown,
}
