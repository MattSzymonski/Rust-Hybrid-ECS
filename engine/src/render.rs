//! Minimal 2D renderer components, gated behind the `rendering` feature.
//!
//! # Responsibilities
//!
//! - Defines [`Position`] and [`Sprite`] components describing where and how
//!   to draw an entity as a colored rectangle.
//! - Provides [`draw_sprites`], which queries the world and issues macroquad
//!   draw calls.
//!
//! # Design
//!
//! This module intentionally stays tiny: it draws colored rectangles, not a
//! general sprite/texture pipeline. The host application (e.g. `standalone`)
//! owns the macroquad window and frame loop; it calls [`draw_sprites`] once
//! per frame between `clear_background` and `next_frame().await`. Game
//! modules only need to spawn entities with `Position` + `Sprite` - they
//! never touch macroquad directly.

// External crates
use macroquad::prelude::*;
use trait_type_map::impl_trait_accessible;

pub use macroquad::color::Color;

// Current crate
use crate::component::Component;
use crate::query::Query;
use crate::world::World;

// =============================================================================
// Components
// =============================================================================

/// World-space position of an entity's top-left draw origin.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    pub x: f32,
    pub y: f32,
}
impl Component for Position {}
impl_trait_accessible!(dyn Component; Position);

/// Axis-aligned colored rectangle drawn at an entity's [`Position`].
#[derive(Debug, Clone, Copy)]
pub struct Sprite {
    pub width: f32,
    pub height: f32,
    pub color: Color,
}
impl Component for Sprite {}
impl_trait_accessible!(dyn Component; Sprite);

impl Default for Sprite {
    fn default() -> Self {
        Self {
            width: 16.0,
            height: 16.0,
            color: WHITE,
        }
    }
}

// =============================================================================
// Drawing
// =============================================================================

/// Draw every entity that has both a [`Position`] and a [`Sprite`] as a
/// filled rectangle.
///
/// Call this once per frame, after `process_frame()` and after
/// `clear_background(...)`, and before `next_frame().await`.
pub fn draw_sprites(world: &mut World) {
    let mut query = Query::<(&Position, &Sprite)>::new(world);
    for (position, sprite) in query.iter_mut() {
        draw_rectangle(
            position.x,
            position.y,
            sprite.width,
            sprite.height,
            sprite.color,
        );
    }
}
