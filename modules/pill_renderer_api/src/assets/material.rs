//! Material assets connecting shaders, textures, and uniform parameters.
//!
//! # Responsibilities
//!
//! - Carry what one drawn surface needs: the shader it draws with and the
//!   values that shader reads, both as uniform parameters and as bound
//!   textures.
//! - Keep materials data. A material names its shader by handle and its values
//!   by the slot names the shader declares, so a game describes an appearance
//!   without reaching into the renderer.
//!
//! # Design
//!
//! The fields mirror [`RenderPass`](crate::RenderPass) on purpose: a shader
//! handle, a parameter map and a texture map, keyed by the slot names the
//! shader declares and packed by the same rules. The renderer therefore drives
//! a material and a pass through one code path, and a material's
//! `rendering_order` decides where it sorts within the pass that draws it.

use std::collections::HashMap;

use pill_engine::{Asset, Handle};

use super::{Shader, Texture};
// Part of the frame contract (a resolved pass carries parameters too).
use crate::frame::MaterialParameter;

/// A texture handle bound to one of a material's slots.
///
/// Wraps the handle rather than a GPU texture: the renderer resolves it when
/// the material is built, so the same asset can back any number of slots
/// without being copied.
#[derive(Clone, Debug)]
pub struct MaterialTexture {
    /// Texture asset this binding resolves through.
    pub texture: Handle<Texture>,
}

/// A surface appearance: the shader a draw runs and the values that shader
/// reads.
///
/// A material is an asset, so the renderer rebuilds its bind groups when the
/// asset's version moves rather than expecting a game to mutate renderer state
/// mid frame. [`MaterialBuilder`] is the supported way to make one.
#[derive(Clone, Debug)]
pub struct Material {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Shader the material draws with. [`Handle::INVALID`] selects the
    /// renderer's built-in shader, as does a handle that names no loaded
    /// shader.
    pub shader: Handle<Shader>,
    /// Textures bound to the slots the shader declares, by slot name.
    ///
    /// A map, like [`RenderPass::textures`](crate::RenderPass), so a slot named
    /// twice has one entry rather than a vector the binder reads only its first
    /// match out of. A declared slot the material leaves unbound falls back to
    /// the renderer's default texture for that slot's type.
    pub textures: HashMap<String, MaterialTexture>,
    /// Uniform parameters, packed one 16-byte slot each, in the order the
    /// shader declares them. A declared slot the material leaves unset packs
    /// as zero.
    pub parameters: HashMap<String, MaterialParameter>,
    /// Sort key for materials inside the pass that draws them.
    ///
    /// The queue's composed key stores this byte inverted and sorts the key
    /// ascending, so a *larger* value is drawn earlier: the default `u8::MAX`
    /// draws before any material given an explicit order. Note the split with
    /// passes, where it is the opposite - a lower [`RenderPass::order`] runs
    /// first.
    pub rendering_order: u8,
}

/// Builds a [`Material`] one field at a time.
///
/// A fresh builder starts on the renderer's built-in shader with no textures,
/// no parameters, and the highest rendering order, so a material only mentions
/// the fields the game wants to differ from those defaults.
pub struct MaterialBuilder {
    material: Material,
}

impl Material {
    /// Start building a material under the given name.
    ///
    /// The name labels the material in logs, profiling and error messages,
    /// which is why it is the one field the builder asks for up front.
    pub fn builder(name: impl Into<String>) -> MaterialBuilder {
        MaterialBuilder {
            material: Self {
                name: name.into(),
                shader: Handle::INVALID,
                textures: HashMap::new(),
                parameters: HashMap::new(),
                rendering_order: u8::MAX,
            },
        }
    }
}

impl MaterialBuilder {
    /// Set the shader the material draws with. [`Handle::INVALID`] keeps the
    /// renderer's built-in shader.
    pub fn shader(mut self, shader: &Handle<Shader>) -> Self {
        self.material.shader = *shader;
        self
    }

    /// Bind a texture to one of the shader's declared slots.
    ///
    /// Binding a slot twice replaces the earlier binding: the map keeps one
    /// texture per slot, and the last call is the one that was meant.
    pub fn texture(mut self, slot: impl Into<String>, texture: &Handle<Texture>) -> Self {
        self.material
            .textures
            .insert(slot.into(), MaterialTexture { texture: *texture });
        self
    }

    /// Set one scalar uniform parameter, under the slot the shader declares.
    pub fn scalar_parameter(mut self, slot: impl Into<String>, value: f32) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Scalar(value));
        self
    }

    /// Set one boolean uniform parameter, packed as a `u32` of 0 or 1.
    pub fn bool_parameter(mut self, slot: impl Into<String>, value: bool) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Bool(value));
        self
    }

    /// Set one colour uniform parameter, packed as three `f32`s.
    pub fn color_parameter(mut self, slot: impl Into<String>, value: [f32; 3]) -> Self {
        self.material
            .parameters
            .insert(slot.into(), MaterialParameter::Color(value));
        self
    }

    /// Choose when the material draws relative to the others in the same pass.
    ///
    /// See [`Material::rendering_order`]: a larger value is drawn earlier.
    pub fn rendering_order(mut self, value: u8) -> Self {
        self.material.rendering_order = value;
        self
    }

    /// Finish building and return the material.
    pub fn build(self) -> Material {
        self.material
    }
}

impl Asset for Material {}
