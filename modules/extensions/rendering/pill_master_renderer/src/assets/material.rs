//! Material assets connecting shaders, textures, and uniform parameters.

use super::{Shader, Texture};
use pill_engine::{Asset, Handle};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum MaterialParameter {
    Scalar(f32),
    Bool(bool),
    Color([f32; 3]),
}

#[derive(Clone, Debug)]
pub struct MaterialTexture {
    pub texture: Handle<Texture>,
}

#[derive(Clone, Debug)]
pub struct Material {
    pub name: String,
    pub shader: Handle<Shader>,
    pub textures: Vec<(String, MaterialTexture)>,
    pub parameters: HashMap<String, MaterialParameter>,
    pub rendering_order: u8,
}

pub struct MaterialBuilder {
    material: Material,
}

impl Material {
    pub fn builder(name: impl Into<String>) -> MaterialBuilder {
        MaterialBuilder {
            material: Self {
                name: name.into(),
                shader: Handle::INVALID,
                textures: Vec::new(),
                parameters: HashMap::new(),
                rendering_order: u8::MAX,
            },
        }
    }
}

impl MaterialBuilder {
    pub fn shader(mut self, shader: &Handle<Shader>) -> Self {
        self.material.shader = *shader;
        self
    }

    pub fn texture(mut self, slot: impl Into<String>, texture: &Handle<Texture>) -> Self {
        self.material
            .textures
            .push((slot.into(), MaterialTexture { texture: *texture }));
        self
    }

    pub fn scalar_parameter(mut self, slot: impl Into<String>, value: f32) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Scalar(value));
        self
    }

    pub fn bool_parameter(mut self, slot: impl Into<String>, value: bool) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Bool(value));
        self
    }

    pub fn color_parameter(mut self, slot: impl Into<String>, value: [f32; 3]) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Color(value));
        self
    }

    pub fn rendering_order(mut self, value: u8) -> Self {
        self.material.rendering_order = value;
        self
    }

    pub fn build(self) -> Material {
        self.material
    }
}

impl Asset for Material {}
