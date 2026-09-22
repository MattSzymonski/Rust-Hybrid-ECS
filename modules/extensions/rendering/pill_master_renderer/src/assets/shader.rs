//! Shader assets and their material binding declarations.

use super::TextureType;
use pill_engine::{Asset, AssetLoadResult, AssetLoader};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum ShaderParameterType {
    Scalar,
    Bool,
    Color,
}

#[derive(Debug, Clone)]
pub struct ShaderParameterSlot {
    pub parameter_type: ShaderParameterType,
}

impl ShaderParameterSlot {
    pub fn new(parameter_type: ShaderParameterType) -> Self {
        Self { parameter_type }
    }
}

#[derive(Debug, Clone)]
pub struct ShaderTextureSlot {
    pub texture_type: TextureType,
    pub texture_binding: u32,
    pub sampler_binding: u32,
}

impl ShaderTextureSlot {
    pub fn new(texture_type: TextureType, bindings: (u32, u32)) -> Self {
        Self {
            texture_type,
            texture_binding: bindings.0,
            sampler_binding: bindings.1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Shader {
    pub name: String,
    pub vertex_wgsl: String,
    pub fragment_wgsl: String,
    pub parameter_slots: Vec<(String, ShaderParameterSlot)>,
    pub texture_slots: HashMap<String, ShaderTextureSlot>,
    pub pass_engine_parameters: bool,
    pub pass_camera_parameters: bool,
}

impl Shader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        vertex_shader: AssetLoader,
        fragment_shader: AssetLoader,
        parameter_slots: Vec<(String, ShaderParameterSlot)>,
        texture_slots: HashMap<String, ShaderTextureSlot>,
        pass_engine_parameters: bool,
        pass_camera_parameters: bool,
    ) -> AssetLoadResult<Self> {
        Ok(Self {
            name: name.into(),
            vertex_wgsl: vertex_shader.load_string()?,
            fragment_wgsl: fragment_shader.load_string()?,
            parameter_slots,
            texture_slots,
            pass_engine_parameters,
            pass_camera_parameters,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_wgsl(
        name: impl Into<String>,
        vertex_wgsl: impl Into<String>,
        fragment_wgsl: impl Into<String>,
        parameter_slots: Vec<(String, ShaderParameterSlot)>,
        texture_slots: HashMap<String, ShaderTextureSlot>,
        pass_engine_parameters: bool,
        pass_camera_parameters: bool,
    ) -> Self {
        Self {
            name: name.into(),
            vertex_wgsl: vertex_wgsl.into(),
            fragment_wgsl: fragment_wgsl.into(),
            parameter_slots,
            texture_slots,
            pass_engine_parameters,
            pass_camera_parameters,
        }
    }
}

impl Asset for Shader {}
