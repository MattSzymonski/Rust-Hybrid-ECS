//! Components the engine defines because more than one consumer needs them.
//!
//! # Responsibilities
//!
//! - Defines [`Position`] and [`Color`], the two components every renderer and
//!   most projects name, with the byte layout they share across binaries.
//! - Defines [`TransformComponent`], an entity's placement in the scene, which
//!   rendering, physics, audio and gameplay all read.
//! - Publishes their editor field layouts and the one call that registers them
//!   ([`register_common_components`]), so a world that uses them exposes their
//!   fields to the inspector.
//!
//! # Design
//!
//! These are universal rather than renderer-specific: a position is where a
//! thing is, a colour is what shade it is, and neither names a GPU concept.
//! Renderer-specific mesh, camera and material contracts live in the rendering
//! extension; these gameplay data types stay usable without it.
//!
//! The byte layout is a shared ABI. Another language binds these rows by the
//! name they register under and the `repr(C)` size it verifies, because a
//! `TypeId` is computed per compilation unit. [`TransformComponent`] also
//! declares a shared name, so every binary that registers it resolves to one
//! column. All three are declared by hand rather than with
//! `#[derive(PillComponent)]`, whose inventory entry would make every artifact
//! linking the engine register the component - and fold it into its
//! persistable-schema fingerprint - whether it used it or not.

// External crates
use serde::{Deserialize, Serialize};

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
/// Plain `repr(C)` float pairs, so its byte layout is what another language
/// binds against rather than a `TypeId`.
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
    #[pill(color)]
    pub r: f32,
    /// Green channel.
    #[pill(color)]
    pub g: f32,
    /// Blue channel.
    #[pill(color)]
    pub b: f32,
    /// Alpha channel.
    #[pill(color)]
    pub a: f32,
}
impl Component for Color {}

impl Color {
    /// Opaque white, and what [`Default`] returns for a colour.
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

/// The shared name [`TransformComponent`] registers under.
///
/// Pinned, not derived from the module path: a derived name would register a
/// different component and orphan every column a live world holds for this
/// one.
pub const TRANSFORM_SHARED_NAME: &str = "pill_master_renderer::component::TransformComponent";

/// Placement of an entity in the scene: position, orientation and scale.
///
/// Renderers read it for every drawable and for the camera they render
/// through; any other system may read or write it too. It is shared and
/// persistable, so one write is visible to the managed side and survives a
/// project reload.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillLayout)]
pub struct TransformComponent {
    /// Position of the entity's origin, in world units.
    pub translation: [f32; 3],
    /// Orientation as a unit quaternion `[x, y, z, w]`; identity is
    /// `[0, 0, 0, 1]`. A non-finite or zero-length value is read as identity
    /// instead of being used as-is.
    pub rotation: [f32; 4],
    /// Per-axis scale applied to the mesh when it is drawn.
    pub scale: [f32; 3],
}

impl Component for TransformComponent {
    fn shared_name() -> Option<&'static str> {
        Some(TRANSFORM_SHARED_NAME)
    }

    fn shared_identity() -> Option<u128> {
        // A `const`, as the derive emits it, so the name is hashed at compile
        // time rather than on every `ComponentId::of` call.
        const IDENTITY: u128 = crate::component::shared_component_identity(TRANSFORM_SHARED_NAME);
        Some(IDENTITY)
    }

    fn declared_schema_hash() -> Option<u64> {
        // What the derive would emit: the hash of the layout registration uses.
        Some(crate::component::component_schema_hash(
            TransformComponent::FIELD_LAYOUT,
        ))
    }
}

impl Default for TransformComponent {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

/// Squared length below which a rotation quaternion is read as identity.
///
/// Normalizing a near-zero quaternion would amplify rounding noise rather
/// than an angle, so anything at or under this is not a usable orientation.
pub const ROTATION_IDENTITY_EPSILON: f32 = 1.0e-8;

/// Read a [`TransformComponent::rotation`] value as a normalize-or-identity
/// quaternion array.
///
/// The one implementation of the rule the field's documentation states, so
/// systems, projects and the renderer share it and its tolerance rather than
/// each spelling their own.
pub fn rotation_or_identity(rotation: [f32; 4]) -> [f32; 4] {
    let length_squared: f32 = rotation.iter().map(|value| value * value).sum();
    if rotation.iter().all(|value| value.is_finite()) && length_squared > ROTATION_IDENTITY_EPSILON
    {
        let inverse_length = length_squared.sqrt().recip();
        rotation.map(|value| value * inverse_length)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Register [`Position`], [`Color`] and [`TransformComponent`] with their
/// editor layouts.
///
/// [`TransformComponent`] is registered as persistable, so its values are
/// migrated across reloads. Idempotent, so a hot reload re-running `init` is
/// safe, and every type arrives in the inspector field-editable rather than as
/// an opaque blob: the layouts come from `#[derive(PillLayout)]` on each
/// struct.
///
/// A renderer calls this from its registration entry point before adding
/// whatever it draws with.
pub fn register_common_components(world: &mut World) {
    world.register_component_with_layout::<Position>(Position::FIELD_LAYOUT);
    world.register_component_with_layout::<Color>(Color::FIELD_LAYOUT);
    world.register_persistable_component_with_layout::<TransformComponent>(
        TransformComponent::FIELD_LAYOUT,
    );
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

    /// The transform's pinned string is its identity across binaries, so the
    /// test holds the name in place.
    #[test]
    fn the_transform_keeps_its_pinned_shared_name() {
        assert_eq!(
            TransformComponent::shared_name(),
            Some("pill_master_renderer::component::TransformComponent")
        );
        assert_eq!(
            TransformComponent::shared_identity(),
            Some(crate::component::shared_component_identity(
                "pill_master_renderer::component::TransformComponent"
            ))
        );
    }

    /// A world registering only the common components has the transform, with
    /// its three described fields and a persistence entry.
    #[test]
    fn registration_includes_a_persistable_transform() {
        let mut world = World::new();
        register_common_components(&mut world);

        let transform = crate::component::ComponentId::of::<TransformComponent>();
        assert_eq!(
            world.component_field_layout(transform).map(<[_]>::len),
            Some(3)
        );
        assert!(
            world
                .persist_schema_hashes
                .contains_key(TRANSFORM_SHARED_NAME),
            "the transform is registered for migration under its shared name"
        );
        assert!(world.take_registration_error().is_none());
    }

    /// The layout covers the struct byte for byte: three positions, a
    /// quaternion and three scales, all `f32` arrays.
    #[test]
    fn the_transform_layout_matches_its_struct() {
        let fields = TransformComponent::FIELD_LAYOUT;
        let names: Vec<&str> = fields.iter().map(|field| field.name).collect();
        assert_eq!(names, ["translation", "rotation", "scale"]);
        assert_eq!(
            fields.iter().map(|field| field.size).sum::<usize>(),
            std::mem::size_of::<TransformComponent>()
        );
        assert_eq!(
            fields
                .iter()
                .map(|field| field.element_count)
                .collect::<Vec<_>>(),
            [3, 4, 3]
        );
    }

    /// The default transform is the identity placement.
    #[test]
    fn the_default_transform_is_the_identity() {
        let transform = TransformComponent::default();
        assert_eq!(transform.translation, [0.0; 3]);
        assert_eq!(transform.rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(transform.scale, [1.0; 3]);
    }

    /// White is the default, because a mesh with no colour set should be
    /// visible rather than invisible.
    #[test]
    fn a_default_colour_is_opaque_white() {
        assert_eq!(Color::default(), Color::WHITE);
        assert_eq!(Color::WHITE, Color::new(1.0, 1.0, 1.0, 1.0));
    }
}
