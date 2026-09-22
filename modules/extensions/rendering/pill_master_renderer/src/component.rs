//! ECS contracts consumed by the transferred renderer.

use crate::assets::{Material, Mesh};
use pill_engine::{Handle, PillComponent, World};
use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct TransformComponent {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
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

#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct CameraComponent {
    pub enabled: bool,
    pub priority: i32,
    pub vertical_fov: f32,
    pub near: f32,
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

pub struct MeshRendererComponentBuilder {
    component: MeshRendererComponent,
}

impl MeshRendererComponentBuilder {
    pub fn mesh(mut self, mesh: &Handle<Mesh>) -> Self {
        self.component.mesh = *mesh;
        self
    }

    pub fn material(mut self, material: &Handle<Material>) -> Self {
        self.component.material = *material;
        self
    }

    pub fn build(self) -> MeshRendererComponent {
        self.component
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
#[pill(
    shared = "pill_master_renderer::component::MeshRendererComponent",
    persistable
)]
pub struct MeshRendererComponent {
    pub mesh: Handle<Mesh>,
    pub material: Handle<Material>,
}

impl MeshRendererComponent {
    pub fn builder() -> MeshRendererComponentBuilder {
        MeshRendererComponentBuilder {
            component: Self::default(),
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PillComponent)]
#[pill(shared, persistable)]
pub struct DirectionalLightComponent {
    pub color: [f32; 3],
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RenderViewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl RenderViewport {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }

    pub fn clamped_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let width = self.width.min(width - x);
        let height = self.height.min(height - y);
        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}

pub fn register_components(world: &mut World) {
    pill_engine::common_components::register_common_components(world);
    __pill_register_TransformComponent(world);
    __pill_register_CameraComponent(world);
    __pill_register_MeshRendererComponent(world);
    __pill_register_DirectionalLightComponent(world);
}
