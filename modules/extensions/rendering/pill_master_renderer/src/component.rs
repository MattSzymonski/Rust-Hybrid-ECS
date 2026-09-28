//! ECS contracts consumed by the transferred renderer.
//!
//! # Responsibilities
//!
//! - Define the scene's renderer components: transform, camera and mesh
//!   renderer, plus the directional light a scene declares and the viewport
//!   rectangle a frame is drawn through.
//! - Keep them shared and persistable, so the managed mirror binds the same
//!   values and the host carries them across reload generations.
//! - Register them with the world through [`register_components`], field
//!   layout included, for the managed codegen and the editor.
//!
//! # Design
//!
//! The four component structs are `#[repr(C)]`, `Serialize` and
//! `Deserialize`, because they are the renderer's public scene contract:
//! projects set them, the managed mirror binds their fields, and the host
//! serialises them when a project reloads. [`RenderViewport`] is a plain value
//! the renderer owns, so it carries no ECS derives.

use crate::assets::{Material, Mesh};
use pill_engine::{Handle, PillComponent, World};
use serde::{Deserialize, Serialize};

/// Placement of an entity in the scene: position, orientation and scale.
///
/// The renderer reads it for every drawable and for the camera it renders
/// through. Because the component is shared and persistable, one write here is
/// visible to the managed side and survives a project reload.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
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

impl Default for TransformComponent {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

/// A perspective camera, described so the renderer can build its projection
/// matrix from it and its view matrix from the entity's transform.
///
/// A frame renders through one enabled camera: the highest `priority` wins,
/// and equal priorities go to the lowest entity id, so the choice cannot
/// wander with traversal order. Impossible projection values fall back to the
/// defaults when read, which keeps a mistyped field of view or near/far pair
/// from becoming NaNs in the matrix.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct CameraComponent {
    /// Whether the camera may be picked for a frame; a disabled one stays in
    /// the world without ever rendering.
    pub enabled: bool,
    /// Selection weight among the scene's cameras; the highest wins.
    pub priority: i32,
    /// Vertical field of view in degrees; values outside `(0, 180)` fall back
    /// to the default when read.
    pub vertical_fov: f32,
    /// Distance to the near clip plane, in world units.
    pub near: f32,
    /// Distance to the far clip plane, in world units.
    pub far: f32,
}

impl Default for CameraComponent {
    fn default() -> Self {
        Self {
            enabled: true,
            priority: 0,
            vertical_fov: 60.0,
            near: 0.1,
            far: 1000.0,
        }
    }
}

/// Assembles a [`MeshRendererComponent`] one handle at a time.
///
/// The chain starts from the component's `Default`, so a caller names only the
/// handles it actually has and `build` returns a value ready to attach.
pub struct MeshRendererComponentBuilder {
    component: MeshRendererComponent,
}

impl MeshRendererComponentBuilder {
    /// Sets the mesh the entity draws with.
    pub fn mesh(mut self, mesh: &Handle<Mesh>) -> Self {
        self.component.mesh = *mesh;
        self
    }

    /// Sets the material that shades the mesh.
    pub fn material(mut self, material: &Handle<Material>) -> Self {
        self.component.material = *material;
        self
    }

    /// Finishes the chain and returns the assembled component.
    pub fn build(self) -> MeshRendererComponent {
        self.component
    }
}

/// What an entity draws: a mesh paired with the material that shades it.
///
/// The extraction system snapshots every entity carrying this and emits one
/// instance per surviving pair, skipping any whose handles no longer resolve
/// in the asset manager, so a dangling handle draws nothing instead of failing
/// the frame. The shared name is pinned explicitly instead of being derived
/// from the module path, keeping the identity stable if this module is ever
/// re-organised.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(
    shared = "pill_master_renderer::component::MeshRendererComponent",
    persistable
)]
pub struct MeshRendererComponent {
    /// Mesh this entity draws.
    pub mesh: Handle<Mesh>,
    /// Material its instances are shaded with; each batch finds its pipeline
    /// through this material's shader.
    pub material: Handle<Material>,
}

impl MeshRendererComponent {
    /// Starts a builder for an entity that should draw.
    pub fn builder() -> MeshRendererComponentBuilder {
        MeshRendererComponentBuilder {
            component: Self::default(),
        }
    }
}

/// A directional light, described by the colour it emits and its strength.
///
/// Projects declare one to light the scene, and the editor edits its fields
/// live through the shared layout. The shipped passes do not read it yet -
/// their light rigs are baked into the shaders - so today it travels the
/// scene contract and nothing more.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct DirectionalLightComponent {
    /// Linear RGB colour the light emits, one channel per element.
    pub color: [f32; 3],
    /// Scalar multiplier applied to each colour channel.
    pub intensity: f32,
}

impl Default for DirectionalLightComponent {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 3.0,
        }
    }
}

/// A rectangle of the target a frame draws into.
///
/// The renderer holds one as its optional split-screen viewport, and falls
/// back to drawing the full target when none is set or the one it has clamps
/// away to nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenderViewport {
    /// Left edge of the rectangle within the target, in pixels.
    pub x: u32,
    /// Top edge of the rectangle within the target, in pixels.
    pub y: u32,
    /// Width of the rectangle, in pixels.
    pub width: u32,
    /// Height of the rectangle, in pixels.
    pub height: u32,
}

impl RenderViewport {
    /// Creates a viewport from explicit pixel bounds.
    ///
    /// The bounds are taken as given; use [`RenderViewport::clamped_to`] to
    /// fit the rectangle to a target that may be smaller.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// The whole target, from the origin, sized `width` by `height`.
    ///
    /// This is what the renderer substitutes when no viewport is set or an
    /// explicit one leaves nothing on screen.
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }

    /// Clips the rectangle to a target of the given pixel size.
    ///
    /// Returns `None` when nothing of the rectangle survives, meaning an
    /// origin at or past the target edge or a visible width or height of zero;
    /// the renderer treats that as "draw the full target".
    pub fn clamped_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let width = self.width.min(width - x);
        let height = self.height.min(height - y);
        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}

/// Registers every renderer component with the world, field layouts included.
///
/// The engine's common components are registered first, then the four declared
/// above, through the `__pill_register_*` functions the `PillComponent`
/// derives generate. Those registrations are what give the managed codegen and
/// the editor each field's name and offset; without one a component is an
/// opaque blob to both.
pub fn register_components(world: &mut World) {
    pill_engine::common_components::register_common_components(world);
    __pill_register_TransformComponent(world);
    __pill_register_CameraComponent(world);
    __pill_register_MeshRendererComponent(world);
    __pill_register_DirectionalLightComponent(world);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pill_engine::ComponentId;

    /// Registration records the field layouts the managed codegen and the
    /// editor read. Without them a component is an opaque blob to both, which
    /// is a silent failure everywhere except here.
    #[test]
    fn registering_the_components_records_their_field_layouts() {
        let mut world = World::new();
        register_components(&mut world);

        let layouts = [
            (
                "TransformComponent",
                ComponentId::of::<TransformComponent>(),
            ),
            ("CameraComponent", ComponentId::of::<CameraComponent>()),
            (
                "MeshRendererComponent",
                ComponentId::of::<MeshRendererComponent>(),
            ),
            (
                "DirectionalLightComponent",
                ComponentId::of::<DirectionalLightComponent>(),
            ),
        ];
        for (name, component) in layouts {
            let fields = world
                .component_field_layout(component)
                .unwrap_or_else(|| panic!("{name} has no field layout after registration"));
            assert!(!fields.is_empty(), "{name} has an empty field layout");
        }

        // One row per declared field, in declaration order, which is what the
        // managed mirror is generated from - an empty or blob-like layout would
        // leave it with nothing to bind by name.
        let count = |component| {
            world
                .component_field_layout(component)
                .map(<[_]>::len)
                .unwrap_or(0)
        };
        assert_eq!(count(ComponentId::of::<TransformComponent>()), 3);
        assert_eq!(count(ComponentId::of::<CameraComponent>()), 5);
        assert_eq!(count(ComponentId::of::<MeshRendererComponent>()), 2);
        assert_eq!(count(ComponentId::of::<DirectionalLightComponent>()), 2);
    }
}
