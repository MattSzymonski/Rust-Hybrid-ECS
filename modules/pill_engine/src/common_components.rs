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
//! [`Sprite`](../../pill_master_renderer/component/struct.Sprite.html) does
//! *not* live here, and the line is deliberate: a quad with a width, a height
//! and a fill is a renderer's idea of a thing to draw, and the ECS core has no
//! business defining one. What is here is what a renderer needs *from* the
//! world rather than what it draws *into* it.
//!
//! The types are a deliberately shared ABI. `pill_engine` is an rlib, so every
//! binary that links it gets its own `TypeId` for these structs; consumers
//! that must reach another binary's rows resolve them by stable type name and
//! verified `repr(C)` size instead of by `TypeId`. Keeping the definition in
//! one crate is what makes that name agree across every artifact.

// Current crate
use crate::component::Component;
use crate::component_registry::ComponentFieldDescriptor;
use crate::world::World;

// =============================================================================
// Components
// =============================================================================

/// World-space position of an entity's draw origin, in pixels.
///
/// Resolved across binaries by stable type name rather than by Rust `TypeId`,
/// so a hot-loaded project and the host agree on one column.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// Opaque white, the default fill color of a sprite.
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
// Shared component field layouts (editor inspectability)
// =============================================================================

/// Hand-written `repr(C)` offsets mirroring [`Position`], for the editor's
/// generic field API.
///
/// These types are part of a shared ABI and cannot carry
/// `#[derive(PillComponent)]` - the derive lives in `pill_engine_macros` and
/// expects to own the type - so without these layouts the editor would show
/// them with no fields at all. [`register_common_components`] attaches them.
///
/// Public because a renderer that flattens a `Color` into a larger component
/// builds its own layout from these offsets rather than restating them.
pub const POSITION_FIELD_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "x",
        type_tag: "f32",
        offset: 0,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "y",
        type_tag: "f32",
        offset: 4,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// `Color` is itself a component; register its channels like the derive would.
pub const COLOR_FIELD_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "r",
        type_tag: "f32",
        offset: 0,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "g",
        type_tag: "f32",
        offset: 4,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "b",
        type_tag: "f32",
        offset: 8,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "a",
        type_tag: "f32",
        offset: 12,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// Register [`Position`] and [`Color`] with their editor layouts.
///
/// Idempotent, so a hot reload re-running `init` is safe, and it goes through
/// `register_component_with_layout` so the components arrive field-editable in
/// the inspector rather than as opaque blobs.
///
/// A renderer calls this from its own registration entry point and then adds
/// whatever it draws with; a project that only wants a position and a colour
/// can call it directly and link no renderer at all.
pub fn register_common_components(world: &mut World) {
    world.register_component_with_layout::<Position>(POSITION_FIELD_LAYOUT);
    world.register_component_with_layout::<Color>(COLOR_FIELD_LAYOUT);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The declared layouts describe the structs they claim to.
    ///
    /// Hand-written offsets are the price of a shared `repr(C)` ABI the derive
    /// cannot own, so the one thing that can go wrong with them - drifting
    /// from the struct - is checked here rather than discovered in an
    /// inspector showing the wrong bytes.
    #[test]
    fn the_field_layouts_match_their_structs() {
        assert_eq!(
            POSITION_FIELD_LAYOUT.len(),
            2,
            "a position has two coordinates"
        );
        assert_eq!(
            std::mem::size_of::<Position>(),
            POSITION_FIELD_LAYOUT
                .iter()
                .map(|field| field.size)
                .sum::<usize>(),
            "the layout covers every byte of Position"
        );

        assert_eq!(COLOR_FIELD_LAYOUT.len(), 4, "a colour has four channels");
        assert_eq!(
            std::mem::size_of::<Color>(),
            COLOR_FIELD_LAYOUT
                .iter()
                .map(|field| field.size)
                .sum::<usize>(),
            "the layout covers every byte of Color"
        );

        for (index, field) in POSITION_FIELD_LAYOUT
            .iter()
            .chain(COLOR_FIELD_LAYOUT)
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

    /// White is the default, because a sprite with no colour set should be
    /// visible rather than invisible.
    #[test]
    fn a_default_colour_is_opaque_white() {
        assert_eq!(Color::default(), Color::WHITE);
        assert_eq!(Color::WHITE, Color::new(1.0, 1.0, 1.0, 1.0));
    }
}
