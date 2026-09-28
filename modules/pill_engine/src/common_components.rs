//! Components the engine defines because more than one consumer needs them.
//!
//! # Responsibilities
//!
//! - Defines [`Position`] and [`Color`], the two components every renderer and
//!   most projects name, with the byte layout they share across binaries.
//! - Publishes their editor field layouts and the one call that registers both
//!   ([`register_common_components`]), so a world that uses them exposes their
//!   fields to the inspector.
//!
//! # Design
//!
//! These are universal rather than renderer-specific. A position is where a
//! thing is; a colour is what shade it is. Neither names a GPU concept, both
//! are plain `repr(C)` data, and a project, an editor, a rasterizer and a wgpu
//! pipeline all want the same two. They lived in `pill_master_renderer`
//! alongside the pipeline that draws them, which meant naming a `Position`
//! cost a dependency on wgpu - and copies of both types accumulated in every
//! other renderer besides.
//!
//! Renderer-specific mesh, camera and material contracts live in the rendering
//! extension. These general gameplay data types remain usable without it.
//!
//! The types are a deliberately shared ABI. `pill_engine` is an rlib, so every
//! binary that links it gets its own `TypeId` for these structs; consumers
//! that must reach another binary's rows resolve them by stable type name and
//! verified `repr(C)` size instead of by `TypeId`. Keeping the definition in
//! one crate is what makes that name agree across every artifact.

// Current crate
use crate::component::Component;
use crate::world::World;

// Derive macro for the field layout (the layout half of `PillComponent`).
use pill_engine_macros::PillLayout;

// =============================================================================
// Components
// =============================================================================

/// World-space position of an entity's draw origin, in pixels.
///
/// Resolved across binaries by stable type name rather than by Rust `TypeId`,
/// so a hot-loaded project and the host agree on one column.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PillLayout)]
pub struct Position {
    /// Horizontal pixel coordinate of the draw origin.
    pub x: f32,
    /// Vertical pixel coordinate of the draw origin.
    pub y: f32,
}
impl Component for Position {}

/// Plain RGBA color, backend-agnostic (0.0-1.0 per channel).
///
/// `#[repr(C)]` with normalized float channels so the byte layout is shared
/// with the C# runtime as part of the component ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, PillLayout)]
pub struct Color {
    /// Red channel.
    pub r: f32,
    /// Green channel.
    pub g: f32,
    /// Blue channel.
    pub b: f32,
    /// Alpha channel.
    pub a: f32,
}
impl Component for Color {}

impl Color {
    /// Opaque white, the default fill color of a mesh.
    pub const WHITE: Color = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };

    /// Construct a color from its red, green, blue, and alpha channels.
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::WHITE
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Register [`Position`] and [`Color`] with their editor layouts.
///
/// Idempotent, so a hot reload re-running `init` is safe, and it goes through
/// `register_component_with_layout` so the components arrive field-editable in
/// the inspector rather than as opaque blobs. The layouts come from
/// `#[derive(PillLayout)]` on each struct: generated descriptors instead of
/// hand-written offsets. These types are a shared ABI every binary links, so
/// they must not carry `#[derive(PillComponent)]` - that derive also submits
/// an inventory registration, which would put both columns in every world that
/// merely links the engine.
///
/// A renderer calls this from its own registration entry point and then adds
/// whatever it draws with; a project that only wants a position and a colour
/// can call it directly and link no renderer at all.
pub fn register_common_components(world: &mut World) {
    world.register_component_with_layout::<Position>(Position::FIELD_LAYOUT);
    world.register_component_with_layout::<Color>(Color::FIELD_LAYOUT);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared layouts describe the structs they claim to.
    ///
    /// The generated layouts cover each struct byte-for-byte, with the tags
    /// and alignment the inspector relies on.
    #[test]
    fn the_field_layouts_match_their_structs() {
        assert_eq!(
            Position::FIELD_LAYOUT.len(),
            2,
            "a position has two coordinates"
        );
        assert_eq!(
            std::mem::size_of::<Position>(),
            Position::FIELD_LAYOUT
                .iter()
                .map(|field| field.size)
                .sum::<usize>(),
            "the layout covers every byte of Position"
        );

        assert_eq!(Color::FIELD_LAYOUT.len(), 4, "a colour has four channels");
        assert_eq!(
            std::mem::size_of::<Color>(),
            Color::FIELD_LAYOUT
                .iter()
                .map(|field| field.size)
                .sum::<usize>(),
            "the layout covers every byte of Color"
        );

        for (index, field) in Position::FIELD_LAYOUT
            .iter()
            .chain(Color::FIELD_LAYOUT)
            .enumerate()
        {
            assert_eq!(field.type_tag, "f32", "field {index} is a float channel");
            assert_eq!(field.align, 4, "field {index} is four-byte aligned");
        }
    }

    /// Registration attaches the layouts, so the inspector sees fields rather
    /// than an opaque blob.
    #[test]
    fn registration_attaches_the_field_layouts() {
        let mut world = World::new();
        register_common_components(&mut world);

        let position = crate::component::ComponentId::of::<Position>();
        let color = crate::component::ComponentId::of::<Color>();
        assert_eq!(
            world.component_field_layout(position).map(<[_]>::len),
            Some(2)
        );
        assert_eq!(world.component_field_layout(color).map(<[_]>::len), Some(4));
    }

    /// Registering twice is what a hot reload does, and it must not fail.
    #[test]
    fn registration_is_idempotent() {
        let mut world = World::new();
        register_common_components(&mut world);
        register_common_components(&mut world);

        assert!(
            world.take_registration_error().is_none(),
            "re-registering the same types is what every reload does"
        );
    }

    /// White is the default, because a mesh with no colour set should be
    /// visible rather than invisible.
    #[test]
    fn a_default_colour_is_opaque_white() {
        assert_eq!(Color::default(), Color::WHITE);
        assert_eq!(Color::WHITE, Color::new(1.0, 1.0, 1.0, 1.0));
    }
}
