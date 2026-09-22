//! Shared scene contracts for the PBR renderer.
//!
//! # Responsibilities
//!
//! - Defines transforms, cameras, renderable meshes, and directional lights.
//! - Registers scene components and describes persistable lighting settings.
//! - Provides physical-pixel viewport bounds for host frontends.
//!
//! # Design
//!
//! Scene components contain plain data with a fixed C layout. Shared identities
//! let the host and project artifacts address the same ECS columns, while serde
//! persistence restores scene values across reloads. Stable asset IDs refer to
//! renderer-owned data; no GPU handles or heap-owning fields enter components.
//!
//! Transforms use a right-handed world with +Y up and cameras looking along local
//! -Z. Viewports are host presentation state rather than scene components.

// External crates and shared engine types
pub use pill_engine::common_components::{Color, Position};
use pill_engine::{PillComponent, World};
use serde::{Deserialize, Serialize};

// =============================================================================
// Scene Components
// =============================================================================

/// World-space translation, rotation, and scale for a rendered entity.
///
/// The renderer composes scale, then rotation, then translation. No parent
/// hierarchy is evaluated here; projects supply the resulting world transform.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct TransformComponent {
    /// World-space position in the scene's chosen distance unit.
    pub translation: [f32; 3],
    /// Quaternion in x, y, z, w order.
    pub rotation: [f32; 4],
    /// Scale along the local axes; the identity transform uses one on each axis.
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

/// Perspective camera paired with a transform.
///
/// Extraction chooses the valid enabled camera with the highest priority. Equal
/// priorities use the lowest entity ID so archetype iteration order cannot decide.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct CameraComponent {
    /// Whether this camera participates in selection.
    pub enabled: bool,
    /// Larger values win when several valid cameras are enabled.
    pub priority: i32,
    /// Vertical field of view in degrees; extraction accepts values in (0, 179).
    pub vertical_fov: f32,
    /// Positive distance from the camera to the near clipping plane.
    pub near: f32,
    /// Distance to the far clipping plane; must exceed `near`.
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

/// Mesh and material selection with per-entity appearance controls.
///
/// Zero asset IDs choose built-in defaults. A loaded material supplies metallic
/// and roughness factors; its base color is multiplied by the entity tint.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct PbrRenderableComponent {
    /// Stable IDs; 0 selects the built-in sphere/default material.
    pub mesh: u64,
    /// Stable material ID; zero selects the built-in material.
    pub material: u64,
    /// Linear RGBA tint multiplied by the loaded material and its albedo texture.
    pub base_color: [f32; 4],
    /// Fallback metallic factor when no material is loaded, from zero to one.
    pub metallic: f32,
    /// Fallback perceptual roughness when no material is loaded, from zero to one.
    pub roughness: f32,
    /// Whether extraction includes this object in the frame packet.
    pub visible: bool,
}

impl Default for PbrRenderableComponent {
    fn default() -> Self {
        Self {
            mesh: 0,
            material: 0,
            base_color: [1.0; 4],
            metallic: 0.0,
            roughness: 0.5,
            visible: true,
        }
    }
}

/// Directional radiance emitted along the transform's local -Z axis.
///
/// The current extraction path uses the first light returned by the ECS query.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct DirectionalLightComponent {
    /// Linear RGB light color, multiplied by `intensity` during extraction.
    pub color: [f32; 3],
    /// Radiance multiplier for the directional light.
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

// =============================================================================
// Viewport
// =============================================================================

/// Top-left origin and extent of a scene rectangle in physical surface pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenderViewport {
    /// Horizontal offset from the left edge of the surface.
    pub x: u32,
    /// Vertical offset from the top edge of the surface.
    pub y: u32,
    /// Horizontal extent in physical pixels.
    pub width: u32,
    /// Vertical extent in physical pixels.
    pub height: u32,
}

impl RenderViewport {
    /// Construct a rectangle; use [`Self::clamped_to`] before drawing into a surface.
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Cover a whole surface with a rectangle rooted at the top-left corner.
    pub fn full(width: u32, height: u32) -> Self {
        Self {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    /// Clip to the surface bounds, returning `None` when no pixels remain.
    pub fn clamped_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let width = self.width.min(width - x);
        let height = self.height.min(height - y);
        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }
}

// =============================================================================
// Component Registration
// =============================================================================

/// Register scene layouts and shared identities without installing a system.
///
/// Projects can call this entry point while the host owns full renderer setup.
pub fn register_components(world: &mut World) {
    pill_engine::common_components::register_common_components(world);
    __pill_register_TransformComponent(world);
    __pill_register_CameraComponent(world);
    __pill_register_PbrRenderableComponent(world);
    __pill_register_DirectionalLightComponent(world);
}

// =============================================================================
// Scene Settings
// =============================================================================

/// Persistable scene lighting controls; GPU state remains with the host.
#[repr(C)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RenderSettings {
    /// Linear exposure multiplier applied before tonemapping; extraction clamps to zero.
    pub exposure: f32,
    /// Background panorama ID; zero uses the automatically discovered environment.
    pub environment: u64,
    /// Diffuse irradiance texture ID; zero uses the discovered diffuse IBL map.
    pub diffuse_ibl: u64,
    /// Prefiltered reflection texture ID; zero uses the discovered specular IBL map.
    pub specular_ibl: u64,
    /// Split-sum BRDF lookup ID; zero uses the discovered lookup texture.
    pub brdf_lut: u64,
    /// Whether to draw the environment behind scene geometry.
    pub background: bool,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            exposure: 1.0,
            environment: 0,
            diffuse_ibl: 0,
            specular_ibl: 0,
            brdf_lut: 0,
            background: true,
        }
    }
}

impl pill_engine::Resource for RenderSettings {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::RenderSettings")
    }
}
