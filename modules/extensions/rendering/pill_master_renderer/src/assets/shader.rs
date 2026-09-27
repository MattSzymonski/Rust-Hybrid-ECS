//! Shader assets and their material binding declarations.

use super::TextureType;
use indexmap::IndexMap;
use pill_engine::{Asset, AssetLoadError, AssetLoadResult, AssetLoader};

#[derive(Debug, Clone)]
pub enum ShaderParameterType {
    Scalar,
    Bool,
    Color,
}

/// One uniform a shader declares, under the name its source gives it.
#[derive(Debug, Clone)]
pub struct ShaderParameterSlot {
    pub name: String,
    pub parameter_type: ShaderParameterType,
}

impl ShaderParameterSlot {
    pub fn new(name: impl Into<String>, parameter_type: ShaderParameterType) -> Self {
        Self {
            name: name.into(),
            parameter_type,
        }
    }
}

/// One texture a shader declares, under the name its source gives it, with the
/// bindings it declares it at.
#[derive(Debug, Clone)]
pub struct ShaderTextureSlot {
    pub name: String,
    pub texture_type: TextureType,
    pub texture_binding: u32,
    pub sampler_binding: u32,
}

impl ShaderTextureSlot {
    pub fn new(name: impl Into<String>, texture_type: TextureType, bindings: (u32, u32)) -> Self {
        Self {
            name: name.into(),
            texture_type,
            texture_binding: bindings.0,
            sampler_binding: bindings.1,
        }
    }
}

/// Key a list of slots by the names they carry, keeping the list's order.
///
/// The shader stores its slots this way: a name answers which slot, and the
/// order is the order the source declares them in - the order a parameter's
/// values pack into the uniform buffer.
pub(crate) fn parameter_slots_by_name(
    slots: impl IntoIterator<Item = ShaderParameterSlot>,
) -> IndexMap<String, ShaderParameterSlot> {
    slots
        .into_iter()
        .map(|slot| (slot.name.clone(), slot))
        .collect()
}

/// [`parameter_slots_by_name`], for texture slots.
pub(crate) fn texture_slots_by_name(
    slots: impl IntoIterator<Item = ShaderTextureSlot>,
) -> IndexMap<String, ShaderTextureSlot> {
    slots
        .into_iter()
        .map(|slot| (slot.name.clone(), slot))
        .collect()
}

#[derive(Clone, Debug)]
pub struct Shader {
    pub name: String,
    pub vertex_wgsl: String,
    pub fragment_wgsl: String,
    /// The uniforms the shader declares, by name, in declaration order.
    pub parameter_slots: IndexMap<String, ShaderParameterSlot>,
    /// The textures the shader declares, by name, each with its bindings.
    pub texture_slots: IndexMap<String, ShaderTextureSlot>,
    pub pass_engine_parameters: bool,
    pub pass_camera_parameters: bool,
}

impl Shader {
    /// Start a shader with its name, the way [`RenderPass::new`](crate::RenderPass::new)
    /// starts a pass: name first, then the parts, and [`ShaderBuilder::build`]
    /// reads the sources and hands the asset back.
    // `clippy::new_ret_no_self`: the chain returns the builder, not the shader,
    // because reading the sources is the one step that can fail.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(name: impl Into<String>) -> ShaderBuilder {
        ShaderBuilder {
            name: name.into(),
            vertex_source: None,
            fragment_source: None,
            parameter_slots: Vec::new(),
            texture_slots: Vec::new(),
            pass_engine_parameters: false,
            pass_camera_parameters: false,
        }
    }

    /// Build a shader from WGSL already in memory, for callers whose sources
    /// are not committed files - the host bridge builds them from what a C#
    /// project handed it.
    #[allow(clippy::too_many_arguments)]
    pub fn from_wgsl(
        name: impl Into<String>,
        vertex_wgsl: impl Into<String>,
        fragment_wgsl: impl Into<String>,
        parameter_slots: impl IntoIterator<Item = ShaderParameterSlot>,
        texture_slots: impl IntoIterator<Item = ShaderTextureSlot>,
        pass_engine_parameters: bool,
        pass_camera_parameters: bool,
    ) -> Self {
        Self {
            name: name.into(),
            vertex_wgsl: vertex_wgsl.into(),
            fragment_wgsl: fragment_wgsl.into(),
            parameter_slots: parameter_slots_by_name(parameter_slots),
            texture_slots: texture_slots_by_name(texture_slots),
            pass_engine_parameters,
            pass_camera_parameters,
        }
    }
}

/// A [`Shader`] under construction, started by [`Shader::new`].
///
/// The parts arrive in any order and [`Self::build`] reads the two sources, so
/// a half-built shader is never alive.
#[derive(Clone, Debug)]
pub struct ShaderBuilder {
    name: String,
    vertex_source: Option<AssetLoader>,
    fragment_source: Option<AssetLoader>,
    parameter_slots: Vec<ShaderParameterSlot>,
    texture_slots: Vec<ShaderTextureSlot>,
    pass_engine_parameters: bool,
    pass_camera_parameters: bool,
}

impl ShaderBuilder {
    /// The WGSL vertex stage, read from this source when the shader is built.
    pub fn with_vertex_source(mut self, source: AssetLoader) -> Self {
        self.vertex_source = Some(source);
        self
    }

    /// The WGSL fragment stage, read from this source when the shader is built.
    pub fn with_fragment_source(mut self, source: AssetLoader) -> Self {
        self.fragment_source = Some(source);
        self
    }

    /// The uniform slots the fragment stage declares, each carrying the name it
    /// binds under. Declaration order is the order their values pack in.
    pub fn with_parameter_slots(
        mut self,
        parameter_slots: impl IntoIterator<Item = ShaderParameterSlot>,
    ) -> Self {
        self.parameter_slots = parameter_slots.into_iter().collect();
        self
    }

    /// The texture slots the fragment stage declares, each carrying the name it
    /// binds under and the bindings the shader declares it at.
    pub fn with_texture_slots(
        mut self,
        texture_slots: impl IntoIterator<Item = ShaderTextureSlot>,
    ) -> Self {
        self.texture_slots = texture_slots.into_iter().collect();
        self
    }

    /// Whether the shader reads the engine's parameters, at set 0.
    pub fn with_engine_parameters(mut self, pass_engine_parameters: bool) -> Self {
        self.pass_engine_parameters = pass_engine_parameters;
        self
    }

    /// Whether the shader reads the camera's parameters, at set 1.
    pub fn with_camera_parameters(mut self, pass_camera_parameters: bool) -> Self {
        self.pass_camera_parameters = pass_camera_parameters;
        self
    }

    /// Read the sources and produce the shader.
    ///
    /// # Errors
    ///
    /// Fails when one of the sources was never set, or when one of them cannot
    /// be read as UTF-8 text.
    pub fn build(self) -> AssetLoadResult<Shader> {
        // A source that was never set is a mistake in the chain, not a missing
        // file, and is refused rather than read from a default path.
        let vertex_source = self.vertex_source.ok_or_else(|| AssetLoadError::Decode {
            label: format!("shader `{}`", self.name),
            detail: "it was built without a vertex source".to_owned(),
        })?;
        let fragment_source = self.fragment_source.ok_or_else(|| AssetLoadError::Decode {
            label: format!("shader `{}`", self.name),
            detail: "it was built without a fragment source".to_owned(),
        })?;

        Ok(Shader {
            name: self.name,
            vertex_wgsl: vertex_source.load_string()?,
            fragment_wgsl: fragment_source.load_string()?,
            parameter_slots: parameter_slots_by_name(self.parameter_slots),
            texture_slots: texture_slots_by_name(self.texture_slots),
            pass_engine_parameters: self.pass_engine_parameters,
            pass_camera_parameters: self.pass_camera_parameters,
        })
    }
}

impl Asset for Shader {}
