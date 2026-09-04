//! Renderer data contract: the sprite components and the viewport types.
//!
//! # Responsibilities
//!
//! - Defines [`Position`], [`Color`] and [`Sprite`], the components describing
//!   where and how to draw an entity as a colored rectangle.
//! - Publishes their editor field layouts, so a world that registers them by
//!   hand still exposes their fields to the inspector.
//! - Defines [`RenderViewport`] and [`VirtualResolution`], the pure-data
//!   description of where sprites are drawn and in what coordinate space.
//! - Collects a world's drawable entities into [`SpriteInstance`] records
//!   through [`World::sprite_instances`].
//!
//! # Design
//!
//! This module contains no GPU code and no `wgpu` dependency, and that is
//! load-bearing rather than incidental. `pill_engine` is an rlib compiled into
//! the host, into every loaded module, and into every hot patch, so anything
//! reachable from here is linked into all of them. The wgpu pipeline that
//! consumes this data lives in [`crate::sprite_pipeline`], behind the
//! `rendering` feature.
//!
//! The components are a deliberately shared ABI: a hot-loaded project and the
//! host assign different `TypeId`s to the same type, so collection resolves
//! their columns by stable type name and verified `repr(C)` size instead of by
//! a host-typed query.

// External crates
use trait_type_map::impl_trait_accessible;

// Current crate
use crate::component::Component;
use crate::component_registry::ComponentFieldDescriptor;
use crate::world::World;


// =============================================================================
// Components
// =============================================================================

/// World-space position of an entity's top-left draw origin, in pixels.
///
/// Position is a renderer component, resolved by the sprite pipeline through
/// the shared ABI by stable type name rather than by Rust `TypeId`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    /// Horizontal pixel coordinate of the draw origin.
    pub x: f32,
    /// Vertical pixel coordinate of the draw origin.
    pub y: f32,
}
impl Component for Position {}
impl_trait_accessible!(dyn Component; Position);

/// Plain RGBA color, backend-agnostic (0.0-1.0 per channel).
///
/// `#[repr(C)]` with normalized float channels so the byte layout is shared
/// with the C# runtime as part of the renderer ABI.
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
impl_trait_accessible!(dyn Component; Color);

impl Color {
    /// Opaque white, the default fill color of [`Sprite`].
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

/// Axis-aligned colored rectangle drawn at an entity's [`Position`].
///
/// The quad spans `width` by `height` pixels starting at the entity's draw
/// origin and is filled with `color`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Sprite {
    /// Quad width in pixels.
    pub width: f32,
    /// Quad height in pixels.
    pub height: f32,
    /// Fill color of the quad.
    pub color: Color,
}
impl Component for Sprite {}
impl_trait_accessible!(dyn Component; Sprite);

impl Default for Sprite {
    fn default() -> Self {
        Self {
            width: 16.0,
            height: 16.0,
            color: Color::WHITE,
        }
    }
}

// =============================================================================
// Shared component field layouts (editor inspectability)
// =============================================================================

/// Hand-written `repr(C)` offsets mirroring the shared renderer structs, for
/// the editor's generic field API.
///
/// These types are part of the shared renderer ABI and are registered with
/// plain [`World::register_component`] - they cannot carry
/// `#[derive(PillComponent)]` - so without a catalog entry the editor would
/// show them with no fields at all.
///
/// `Sprite::color` is flattened into per-channel scalars at absolute offsets
/// (`color.r` … `color.a`) so the editor can render it as a colour picker over
/// the generic scalar read/write path instead of an opaque byte blob.
const POSITION_FIELD_LAYOUT: &[ComponentFieldDescriptor] = &[
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
const COLOR_FIELD_LAYOUT: &[ComponentFieldDescriptor] = &[
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

/// `Sprite { width, height, color }` with `color` flattened into channels.
const SPRITE_FIELD_LAYOUT: &[ComponentFieldDescriptor] = &[
    ComponentFieldDescriptor {
        name: "width",
        type_tag: "f32",
        offset: 0,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "height",
        type_tag: "f32",
        offset: 4,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "color.r",
        type_tag: "f32",
        offset: 8,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "color.g",
        type_tag: "f32",
        offset: 12,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "color.b",
        type_tag: "f32",
        offset: 16,
        size: 4,
        align: 4,
        element_count: 0,
    },
    ComponentFieldDescriptor {
        name: "color.a",
        type_tag: "f32",
        offset: 20,
        size: 4,
        align: 4,
        element_count: 0,
    },
];

/// Return the editor field layout for a shared renderer component type name.
///
/// `World::register_component` consults this catalog so any world that uses
/// the renderer components (native or C# project) exposes their fields to the
/// editor without the types carrying the derive macro.
pub(crate) fn shared_component_field_layout(
    type_name: &str,
) -> Option<&'static [ComponentFieldDescriptor]> {
    if type_name == std::any::type_name::<Position>() {
        Some(POSITION_FIELD_LAYOUT)
    } else if type_name == std::any::type_name::<Color>() {
        Some(COLOR_FIELD_LAYOUT)
    } else if type_name == std::any::type_name::<Sprite>() {
        Some(SPRITE_FIELD_LAYOUT)
    } else {
        None
    }
}

/// Physical-pixel rectangle within a render target.
///
/// Sprite positions are interpreted relative to this rectangle's top-left
/// corner. The GPU viewport maps their local coordinates into the rectangle,
/// while a matching scissor prevents drawing outside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderViewport {
    /// Left edge of the rectangle, in physical pixels.
    pub x: u32,
    /// Top edge of the rectangle, in physical pixels.
    pub y: u32,
    /// Horizontal extent of the rectangle, in physical pixels.
    pub width: u32,
    /// Vertical extent of the rectangle, in physical pixels.
    pub height: u32,
}

impl RenderViewport {
    /// Construct a physical-pixel viewport rectangle.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Construct a viewport covering an entire render target.
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }

    /// Clamp this rectangle to a render target, returning `None` when empty.
    pub fn clamped_to(self, target_width: u32, target_height: u32) -> Option<Self> {
        let x = self.x.min(target_width);
        let y = self.y.min(target_height);
        let width = self.width.min(target_width.saturating_sub(x));
        let height = self.height.min(target_height.saturating_sub(y));

        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}

/// Logical coordinate space mapped into a physical [`RenderViewport`].
///
/// Keeping this separate from the swapchain dimensions lets an embedded project
/// keep a stable coordinate system while its dock panel is resized. The GPU
/// viewport performs the final scaling into the panel rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VirtualResolution {
    /// Horizontal extent of the project coordinate space.
    pub width: f32,
    /// Vertical extent of the project coordinate space.
    pub height: f32,
}

impl VirtualResolution {
    /// Construct a logical scene resolution.
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// Return whether both dimensions can safely be used by the projection.
    pub fn is_valid(self) -> bool {
        self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
    }
}

// =============================================================================
// Instance data
// =============================================================================

/// One drawable entity, flattened for the renderer.
///
/// Plain `repr(C)` data with no GPU dependency: [`crate::sprite_pipeline`]
/// keeps its own layout-identical `bytemuck` mirror for the actual upload, so
/// this side of the split needs neither `wgpu` nor `bytemuck`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpriteInstance {
    /// Top-left position in pixels.
    pub position: [f32; 2],
    /// Width/height in pixels.
    pub size: [f32; 2],
    /// RGBA color.
    pub color: [f32; 4],
}

// =============================================================================
// World collection
// =============================================================================

impl World {
    /// Every drawable entity in this world, flattened for a renderer.
    ///
    /// The one public seam the GPU half needs. Collection has to live here
    /// because it reads `archetypes` and `component_registry`, which are
    /// crate-private; exposing those instead would leak the ECS internals to
    /// anything that wanted to draw.
    pub fn sprite_instances(&self) -> Vec<SpriteInstance> {
        collect_sprite_instances(self)
    }
}


// =============================================================================
// Shared-ABI collection
// =============================================================================

/// Collect renderer components across the native project-module ABI boundary.
///
/// A hot-loaded Rust project and the host executable can assign different
/// `TypeId` values to the same `pill_engine` type. Renderer components are a
/// deliberately shared ABI, so resolve their columns by stable type name and
/// verify their C layouts instead of issuing a host-typed ECS query.
fn collect_sprite_instances(world: &World) -> Vec<SpriteInstance> {
    collect_sprite_instances_named(
        world,
        std::any::type_name::<Position>(),
        std::any::type_name::<Sprite>(),
    )
}

/// Type-erased implementation separated from the public renderer names so its
/// cross-`TypeId` behavior can be covered by an ordinary unit test.
fn collect_sprite_instances_named(
    world: &World,
    position_name: &str,
    sprite_name: &str,
) -> Vec<SpriteInstance> {
    let registry = &world.component_registry;
    let mut instances = Vec::new();

    for archetype in world.archetypes.values() {
        // Step 1: Resolve each column among the components actually present in
        // this archetype by shared type name and size. This also supports
        // entities retained from older DLL generations whose component IDs
        // differ from the current module.
        let position_id = archetype.component_types.iter().copied().find(|id| {
            registry.get_name(id) == Some(position_name)
                && registry.get_size(id) == Some(std::mem::size_of::<Position>())
        });
        let sprite_id = archetype.component_types.iter().copied().find(|id| {
            registry.get_name(id) == Some(sprite_name)
                && registry.get_size(id) == Some(std::mem::size_of::<Sprite>())
        });
        let (Some(position_id), Some(sprite_id)) = (position_id, sprite_id) else {
            continue;
        };

        // Step 2: Fetch the type-erased trait storage backing each resolved
        // component column so rows can be read without a host-typed query.
        let (Some(position_type_id), Some(sprite_type_id)) =
            (position_id.native_type_id(), sprite_id.native_type_id())
        else {
            continue;
        };
        let Some(position_storage) = archetype
            .component_storages
            .get_trait_storage(position_type_id)
        else {
            continue;
        };
        let Some(sprite_storage) = archetype
            .component_storages
            .get_trait_storage(sprite_type_id)
        else {
            continue;
        };

        // Step 3: Copy each row's layout-validated component data into an
        // instance record for the GPU.
        //
        // SAFETY: Both shared types are `#[repr(C)]` and `Copy`, and their
        // sizes were verified against this crate's `Position`/`Sprite` above,
        // so each read from the type-erased trait storage yields a valid value
        // of the target type even when its native TypeId originated in another
        // DLL. `row_count` caps the loop at every storage length, keeping the
        // `get(row)` accesses in bounds.
        let row_count = archetype
            .entity_count()
            .min(position_storage.len())
            .min(sprite_storage.len());
        for row in 0..row_count {
            // SAFETY: `read_shared_component` dereferences the type-erased
            // storage pointer; `row_count` is capped at every storage length
            // and the layout checks above guarantee the value is a valid
            // `Position` for every row in this loop.
            let position =
                unsafe { read_shared_component::<Position>(position_storage.get_dyn(row)) };
            // SAFETY: as for the `Position` read directly above, for `Sprite`.
            let sprite = unsafe { read_shared_component::<Sprite>(sprite_storage.get_dyn(row)) };
            instances.push(SpriteInstance {
                position: [position.x, position.y],
                size: [sprite.width, sprite.height],
                color: [
                    sprite.color.r,
                    sprite.color.g,
                    sprite.color.b,
                    sprite.color.a,
                ],
            });
        }
    }

    instances
}

/// Copy one layout-validated shared component out of type-erased storage.
///
/// # Safety
///
/// `component` must point to a value with the same `repr(C)` layout and size
/// as `T`. Callers establish this through the shared component name and size.
unsafe fn read_shared_component<T: Copy>(component: &dyn Component) -> T {
    let data = component as *const dyn Component as *const T;
    // SAFETY: Guaranteed by the caller. `read_unaligned` also avoids relying
    // on alignment information that is not present in ComponentRegistry.
    unsafe { data.read_unaligned() }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod shared_component_tests {
    use super::*;
    use crate::component::ComponentId;
    use trait_type_map::impl_trait_accessible;

    /// Layout-compatible stand-in with a different TypeId than Position.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ForeignPosition {
        x: f32,
        y: f32,
    }

    impl Component for ForeignPosition {}
    impl_trait_accessible!(dyn Component; ForeignPosition);

    /// Layout-compatible stand-in with a different TypeId than Sprite.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ForeignSprite {
        width: f32,
        height: f32,
        color: Color,
    }

    impl Component for ForeignSprite {}
    impl_trait_accessible!(dyn Component; ForeignSprite);

    /// Shared renderer layouts can be read even when their native TypeIds
    /// originate from a different compilation unit.
    #[test]
    fn collects_layout_compatible_components_with_foreign_type_ids() {
        let mut world = World::new();
        world.register_component::<ForeignPosition>();
        world.register_component::<ForeignSprite>();
        world
            .create_entity()
            .with(ForeignPosition { x: 12.0, y: 34.0 })
            .with(ForeignSprite {
                width: 56.0,
                height: 78.0,
                color: Color::new(0.1, 0.2, 0.3, 0.4),
            })
            .build()
            .unwrap();

        let instances = collect_sprite_instances_named(
            &world,
            std::any::type_name::<ForeignPosition>(),
            std::any::type_name::<ForeignSprite>(),
        );

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].position, [12.0, 34.0]);
        assert_eq!(instances[0].size, [56.0, 78.0]);
        assert_eq!(instances[0].color, [0.1, 0.2, 0.3, 0.4]);
    }

    /// Projection dimensions must be finite and strictly positive.
    #[test]
    fn virtual_resolution_rejects_invalid_projection_dimensions() {
        assert!(VirtualResolution::new(800.0, 600.0).is_valid());
        assert!(!VirtualResolution::new(0.0, 600.0).is_valid());
        assert!(!VirtualResolution::new(800.0, f32::NAN).is_valid());
        assert!(!VirtualResolution::new(f32::INFINITY, 600.0).is_valid());
    }

    /// The editor field-layout catalog mirrors the `repr(C)` structs and is
    /// attached by plain `register_component`, so the hand-registered renderer
    /// components become field-editable in the inspector without carrying the
    /// derive macro.
    #[test]
    fn shared_layouts_match_struct_layout_and_attach_on_registration() {
        // Channel order and byte sizes come straight from the compiler.
        assert_eq!(std::mem::size_of::<Position>(), 8);
        assert_eq!(std::mem::size_of::<Color>(), 16);
        assert_eq!(std::mem::size_of::<Sprite>(), 24);

        let assert_matches = |fields: &[ComponentFieldDescriptor],
                              expected: &[(&str, usize, usize)]| {
            assert_eq!(fields.len(), expected.len());
            for (field, (name, offset, size)) in fields.iter().zip(expected) {
                assert_eq!(field.name, *name, "descriptor name for {name}");
                assert_eq!(field.type_tag, "f32", "descriptor tag for {name}");
                assert_eq!(field.offset, *offset, "descriptor offset for {name}");
                assert_eq!(field.size, *size, "descriptor size for {name}");
                assert_eq!(field.align, 4, "descriptor align for {name}");
                assert_eq!(field.element_count, 0);
            }
        };

        let position_layout =
            shared_component_field_layout(std::any::type_name::<Position>()).expect("position");
        assert_matches(
            position_layout,
            &[
                ("x", std::mem::offset_of!(Position, x), 4),
                ("y", std::mem::offset_of!(Position, y), 4),
            ],
        );

        let color_layout =
            shared_component_field_layout(std::any::type_name::<Color>()).expect("color");
        assert_matches(
            color_layout,
            &[
                ("r", std::mem::offset_of!(Color, r), 4),
                ("g", std::mem::offset_of!(Color, g), 4),
                ("b", std::mem::offset_of!(Color, b), 4),
                ("a", std::mem::offset_of!(Color, a), 4),
            ],
        );

        // `Sprite.color` is flattened into per-channel scalars at absolute
        // offsets so the inspector renders it as one colour group.
        let color_offset = std::mem::offset_of!(Sprite, color);
        let sprite_layout =
            shared_component_field_layout(std::any::type_name::<Sprite>()).expect("sprite");
        assert_matches(
            sprite_layout,
            &[
                ("width", std::mem::offset_of!(Sprite, width), 4),
                ("height", std::mem::offset_of!(Sprite, height), 4),
                ("color.r", color_offset + std::mem::offset_of!(Color, r), 4),
                ("color.g", color_offset + std::mem::offset_of!(Color, g), 4),
                ("color.b", color_offset + std::mem::offset_of!(Color, b), 4),
                ("color.a", color_offset + std::mem::offset_of!(Color, a), 4),
            ],
        );

        // Registering the real components attaches the catalog layouts through
        // `World::register_component`, which is what makes them editable.
        let mut world = World::new();
        world.register_component::<Position>();
        world.register_component::<Color>();
        world.register_component::<Sprite>();
        let position = world
            .component_field_layout(ComponentId::of::<Position>())
            .expect("Position registered with a layout");
        assert_eq!(position.len(), 2);
        let color = world
            .component_field_layout(ComponentId::of::<Color>())
            .expect("Color registered with a layout");
        assert_eq!(color.len(), 4);
        let sprite = world
            .component_field_layout(ComponentId::of::<Sprite>())
            .expect("Sprite registered with a layout");
        assert_eq!(sprite.len(), 6);
    }
}
