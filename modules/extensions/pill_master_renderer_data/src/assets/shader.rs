//! Shader assets and their material binding declarations.
//!
//! # Responsibilities
//!
//! - Carry a shader's two WGSL stages plus the uniform and texture slots its
//!   source declares, under the names the source gives them.
//! - Keep those slots in declaration order: that order is the order their
//!   values pack into the uniform buffer, and reordering them would leave the
//!   packed layout disagreeing with the shader's own declarations.
//! - Offer both ways in: [`ShaderBuilder::with_vertex_source`] reads the stages
//!   from files when the shader is built, and [`ShaderBuilder::with_wgsl`] takes
//!   sources already in memory, which is how a chain embedding its own stages
//!   and the host bridge building them from what a C# project handed it both
//!   work.
//!
//! # Design
//!
//! [`ShaderBuilder`] exists because reading the sources is the one step that
//! can fail: the builder collects the parts in any order and
//! [`ShaderBuilder::build`] reads the two stages, so a half-built shader is
//! never alive. The slots are the meeting point with the material side of the
//! renderer, which walks them in declaration order, by name, to size the
//! uniform buffer and place each value.

// External crates
use indexmap::IndexMap;
use pill_engine::{Asset, AssetLoadError, AssetLoadResult, AssetLoader};

// Current crate
use super::TextureType;

/// The kind of value a uniform parameter carries.
///
/// The renderer writes a slot's bytes by this kind rather than by the kind of
/// value it was handed, so the two have to agree or the value packs as zero.
#[derive(Debug, Clone)]
pub enum ShaderParameterType {
    /// A single float.
    Scalar,
    /// A flag, written as the integer it converts to.
    Bool,
    /// An RGB color.
    Color,
}

/// One uniform a shader declares, under the name its source gives it.
///
/// Slots are how a value finds the uniform meant for it: a material carries
/// its parameters by name, and the shader's slots say which names it accepts
/// and what the matching value packs as.
#[derive(Debug, Clone)]
pub struct ShaderParameterSlot {
    /// The name the source binds the uniform under.
    pub name: String,
    /// The kind of value the uniform carries, which decides how it packs.
    pub parameter_type: ShaderParameterType,
}

impl ShaderParameterSlot {
    /// A slot for `name`, carrying `parameter_type`.
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
    /// The name the source binds the texture under.
    pub name: String,
    /// The kind of texture the slot expects.
    pub texture_type: TextureType,
    /// The binding the shader declares the texture at.
    pub texture_binding: u32,
    /// The binding the shader declares the sampler at.
    pub sampler_binding: u32,
}

impl ShaderTextureSlot {
    /// A slot for `name`, of `texture_type`, at the texture and sampler
    /// bindings the shader declares it at.
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
/// values pack into the uniform buffer. A name declared twice keeps its first
/// declaration and reports the later one: collapsing the pair silently would
/// leave the packed layout disagreeing with the shader's own declarations.
pub fn parameter_slots_by_name(
    shader_name: &str,
    slots: impl IntoIterator<Item = ShaderParameterSlot>,
) -> IndexMap<String, ShaderParameterSlot> {
    let mut keyed = IndexMap::new();
    for slot in slots {
        if keyed.contains_key(&slot.name) {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "shader `{shader_name}` declares parameter slot `{}` twice; the later declaration is ignored",
                slot.name
            );
            continue;
        }
        keyed.insert(slot.name.clone(), slot);
    }
    keyed
}

/// [`parameter_slots_by_name`], for texture slots.
pub fn texture_slots_by_name(
    shader_name: &str,
    slots: impl IntoIterator<Item = ShaderTextureSlot>,
) -> IndexMap<String, ShaderTextureSlot> {
    let mut keyed = IndexMap::new();
    for slot in slots {
        if keyed.contains_key(&slot.name) {
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "shader `{shader_name}` declares texture slot `{}` twice; the later declaration is ignored",
                slot.name
            );
            continue;
        }
        keyed.insert(slot.name.clone(), slot);
    }
    keyed
}

/// A shader asset: its two WGSL stages plus the slots the source declares.
///
/// The slots travel with the sources because they describe the same shader:
/// the sources go to the pipeline, and the slots say how the uniform buffer
/// and the texture bindings have to be laid out for that pipeline.
#[derive(Clone, Debug)]
pub struct Shader {
    /// Name used in logs and error messages.
    pub name: String,
    /// The vertex stage, as WGSL text.
    pub vertex_wgsl: String,
    /// The fragment stage, as WGSL text.
    pub fragment_wgsl: String,
    /// The uniforms the shader declares, by name, in declaration order.
    pub parameter_slots: IndexMap<String, ShaderParameterSlot>,
    /// The textures the shader declares, by name, each with its bindings.
    pub texture_slots: IndexMap<String, ShaderTextureSlot>,
    /// Whether the shader reads the engine's parameters, at set 0.
    pub pass_engine_parameters: bool,
    /// Whether the shader reads the camera's parameters, at set 1.
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
}

/// A [`Shader`] under construction, started by [`Shader::new`].
///
/// The parts arrive in any order and [`Self::build`] reads the two sources, so
/// a half-built shader is never alive.
#[derive(Clone, Debug)]
pub struct ShaderBuilder {
    /// The name the finished shader will carry.
    name: String,
    /// The vertex stage's source, read when the shader is built.
    vertex_source: Option<AssetLoader>,
    /// The fragment stage's source, read when the shader is built.
    fragment_source: Option<AssetLoader>,
    /// The uniform slots collected so far, in the order they were declared.
    parameter_slots: Vec<ShaderParameterSlot>,
    /// The texture slots collected so far, in the order they were declared.
    texture_slots: Vec<ShaderTextureSlot>,
    /// Whether the finished shader will read the engine's parameters.
    pass_engine_parameters: bool,
    /// Whether the finished shader will read the camera's parameters.
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

    /// The two WGSL stages, already in hand.
    ///
    /// For sources that are not files: `include_str!` gives a `&'static str`,
    /// and the host bridge has whatever a C# project handed it. Nothing is
    /// resolved or read, so [`Self::build`] has the stages already - where
    /// [`Self::with_vertex_source`] has a path that may turn out not to exist.
    pub fn with_wgsl(mut self, vertex: impl Into<String>, fragment: impl Into<String>) -> Self {
        self.vertex_source = Some(AssetLoader::Bytes(vertex.into().into_bytes().into()));
        self.fragment_source = Some(AssetLoader::Bytes(fragment.into().into_bytes().into()));
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
    /// Fails when one of the sources was never set, or when reading one of
    /// them fails: a path that does not resolve, an I/O error, or bytes that
    /// are not UTF-8 text.
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

        let name = self.name;
        let parameter_slots = parameter_slots_by_name(&name, self.parameter_slots);
        let texture_slots = texture_slots_by_name(&name, self.texture_slots);
        Ok(Shader {
            name,
            vertex_wgsl: vertex_source.load_string()?,
            fragment_wgsl: fragment_source.load_string()?,
            parameter_slots,
            texture_slots,
            pass_engine_parameters: self.pass_engine_parameters,
            pass_camera_parameters: self.pass_camera_parameters,
        })
    }
}

// Shared across binaries: the data module, the GPU module and every project
// compile their own copy of this crate, each with its own `TypeId`. The pinned
// name makes them one asset column (see `Asset::shared_name`); keep it
// verbatim when moving the type.
impl Asset for Shader {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::assets::Shader")
    }
}
