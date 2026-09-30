//! The chain's GPU objects, and the plan each frame is recorded from.
//!
//! # Responsibilities
//!
//! - Decide when a chain's pipelines, bind groups, and offscreen targets have
//!   to be rebuilt, from two signatures compared against the frame's chain
//!   generation, the resource epoch, and the surface size.
//! - Build one pass object per pass of the chain, and remember the reason for
//!   each pass it could not build.
//! - Plan each frame - which draws each pass takes, which pass opens each
//!   target - and name a pass that cannot be recorded once, rather than once
//!   per frame.
//! - Write the chain to the log once per change.
//!
//! # Design
//!
//! The chain is read every frame but its GPU objects are not rebuilt every
//! frame: an unchanged frame costs one tuple comparison, and the signature
//! strings are only built once the chain generation, the resource epoch, or the
//! surface size moved. Creation failures are reported and skipped rather than
//! fatal - a shader, texture, or target that would not build is named in the log
//! while the rest of the chain still reaches the GPU.
//!
//! The chain is a script. A game writes it as a [`RenderingPipeline`] of
//! [`RenderPass`]es; this is what that declaration becomes once the device has
//! seen it. Nothing here owns a window, a frame loop, or an asset cache - what
//! it needs of those arrives in a [`ChainContext`], and what it produces is
//! either a pass object or the reason there is none.
//!
//! [`RenderingPipeline`]: crate::RenderingPipeline
//! [`RenderPass`]: crate::RenderPass

// Standard library
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

// External crates
use pill_engine::AssetManager;

// Current crate
use crate::{
    assets::{asset_key, Shader},
    frame::{PassKind, PassTarget, ResolvedPass},
    render_queue::{decompose_render_queue_key, RenderQueueItem},
    renderer::{State, OFFSCREEN_FORMAT},
    resources::{RendererPass, RendererShaderHandle, RendererTextureHandle},
};

/// Where a pass writes.
#[derive(Clone, Copy)]
pub(crate) enum PassOutput<'a> {
    Surface,
    Offscreen(&'a str),
}

/// A pass's GPU object, or the reason there is none.
pub(crate) enum PassSlot {
    /// Built and ready to record: a pipeline of its own, for the target it
    /// writes.
    Drawable(Box<RendererPass>),
    /// A geometry pass with no shader of its own. It draws every instance
    /// through the pipeline each material names, which is the renderer's
    /// built-in chain and needs nothing built per pass.
    Unshaded,
    /// Cannot be recorded, carrying the reason the log reports.
    Unsupported(String),
}

/// One pass the renderer will record.
pub(crate) enum PassPlan<'a> {
    /// Instances, batched by material, into the pass's targets.
    ///
    /// Borrowed when the pass takes the whole queue, which is the built-in
    /// chain's shape; only a shader-filtered pass pays for a collection.
    Geometry {
        label: &'a str,
        items: Cow<'a, [RenderQueueItem]>,
        outputs: Vec<PassOutput<'a>>,
        clear: bool,
        /// The pass's own pipeline, when it built one. `None` leaves each
        /// instance to the pipeline its material's shader carries.
        pass_index: Option<usize>,
    },
    /// Three vertices and no vertex buffers, reading what earlier passes wrote.
    Fullscreen {
        label: &'a str,
        pass_index: usize,
        outputs: Vec<PassOutput<'a>>,
        clear: bool,
    },
}

impl PassPlan<'_> {
    /// Pass name, used as the wgpu render-pass label and in the chain log.
    pub(crate) fn label(&self) -> &str {
        match self {
            PassPlan::Geometry { label, .. } | PassPlan::Fullscreen { label, .. } => label,
        }
    }

    /// Where this pass writes, its own target first.
    pub(crate) fn outputs(&self) -> &[PassOutput<'_>] {
        match self {
            PassPlan::Geometry { outputs, .. } | PassPlan::Fullscreen { outputs, .. } => outputs,
        }
    }

    /// Whether this pass opens the targets it writes.
    pub(crate) fn clears(&self) -> bool {
        match self {
            PassPlan::Geometry { clear, .. } | PassPlan::Fullscreen { clear, .. } => *clear,
        }
    }

    /// How many draws this pass records: one per instance batch, or the single
    /// triangle a fullscreen pass is.
    pub(crate) fn draws(&self) -> usize {
        match self {
            PassPlan::Geometry { items, .. } => items.len(),
            PassPlan::Fullscreen { .. } => 1,
        }
    }
}

/// What the chain needs from the renderer to build its GPU objects.
///
/// The chain names its shaders and textures by asset key and the renderer caches
/// their GPU objects by the same key, so it is handed the caches rather than a
/// way to look them up. Bundled because the split is what the borrow checker
/// enforces - the pipeline is mutated while all of this is borrowed - and
/// because a chain that has to be handed four things separately at every call
/// site is a chain that is not really a unit yet.
pub(crate) struct ChainContext<'a> {
    /// The device, caches, and surface-sized targets the passes are built from.
    pub(crate) state: &'a mut State,
    /// Shader objects by asset key: a pass names its shader by key.
    pub(crate) shader_handles: &'a HashMap<u64, RendererShaderHandle>,
    /// Texture objects by asset key: a pass's committed textures.
    pub(crate) texture_handles: &'a HashMap<u64, RendererTextureHandle>,
    /// Bumped whenever a shader or texture object is created, recreated, or
    /// dropped. Part of both signatures, because a pass's bind groups reference
    /// those objects by handle.
    pub(crate) resource_epoch: u64,
}

/// The GPU side of a chain: one pass object per pass, and the plan a frame is
/// recorded from.
///
/// Rebuilt only when something the chain depends on moves - its content, the
/// resource epoch, or the surface size - and asked for a plan on every frame.
/// The plan borrows the frame it came from, so a plan never outlives the frame
/// that produced it.
pub(crate) struct ScriptableRenderingPipeline {
    /// One entry per pass of the current chain, in the chain's order.
    passes: Vec<PassSlot>,
    /// The chain those passes were built from.
    signature: Option<String>,
    /// What `signature` was last checked against: the frame's chain generation,
    /// the resource epoch and the surface size. One tuple compare per frame - no
    /// strings built - until the frame's chain or the GPU objects behind it
    /// actually move.
    inputs: Option<(u64, u64, u32, u32)>,
    /// The offscreen layout the current targets were built for.
    offscreen_key: Option<String>,
    /// Passes this pipeline cannot record yet, remembered so each one is named
    /// once in the log instead of once per frame.
    skipped_passes: HashSet<String>,
    /// The chain last written to the log, so a chain that does not change does
    /// not write a line per frame.
    chain_log: Option<String>,
}

impl ScriptableRenderingPipeline {
    /// A chain with nothing built and nothing planned.
    pub(crate) fn new() -> Self {
        Self {
            passes: Vec::new(),
            signature: None,
            inputs: None,
            offscreen_key: None,
            skipped_passes: HashSet::new(),
            chain_log: None,
        }
    }

    /// The built pass objects, in chain order, for the frame recorder to index.
    pub(crate) fn passes(&self) -> &[PassSlot] {
        &self.passes
    }

    /// Build the GPU objects the chain needs, when something they depend on
    /// moved.
    ///
    /// Two keys decide. The offscreen layout - every target and scale the chain
    /// declares, at the surface size - decides the render target textures;
    /// everything else (the chain's serialized content and the resource epoch)
    /// decides the pass pipelines and their bind groups. An unchanged frame
    /// costs one tuple comparison: signature strings are only built when the
    /// chain generation, the resource epoch or the surface size moved.
    ///
    /// Infallible on purpose: a pass that would not build is recorded as
    /// [`PassSlot::Unsupported`] and named in the log, so one bad shader does
    /// not cost the frame the passes that would have drawn.
    pub(crate) fn ensure(
        &mut self,
        chain: &[ResolvedPass],
        assets: &AssetManager,
        chain_generation: u64,
        context: ChainContext<'_>,
    ) {
        let (width, height) = context.state.surface.size();
        let inputs = (chain_generation, context.resource_epoch, width, height);
        if self.inputs == Some(inputs) {
            return;
        }

        let offscreen_key = offscreen_signature(chain, width, height);
        let targets_changed = self.offscreen_key.as_deref() != Some(offscreen_key.as_str());
        let signature = chain_signature(chain, context.resource_epoch);
        if !targets_changed && self.signature.as_deref() == Some(signature.as_str()) {
            self.inputs = Some(inputs);
            return;
        }

        if targets_changed {
            context.state.ensure_offscreen_targets(chain);
            self.offscreen_key = Some(offscreen_key);
        }

        // A pass builds its own pipeline from the shader's sources, and the
        // chain names that shader by key while the manager looks assets up by
        // handle. One walk of the shader column builds the way from one to the
        // other - here rather than once per pass, and only when the chain is
        // actually rebuilt.
        let shaders_by_key: HashMap<u64, &Shader> = assets
            .iter_handles::<Shader>()
            .map(|(handle, shader)| (asset_key(handle), shader))
            .collect();

        // Passes are built in chain order and may only read targets an earlier
        // pass declares: the set grows as the chain is walked, so a pass that
        // reads its own output, or a later pass's, is refused instead of
        // binding whatever the map happens to hold.
        let mut defined_targets: HashSet<String> = HashSet::new();
        let mut passes: Vec<PassSlot> = Vec::with_capacity(chain.len());
        for pass in chain {
            passes.push(build_pass(
                pass,
                &shaders_by_key,
                &defined_targets,
                &context,
            ));
            for target in std::iter::once(&pass.target).chain(pass.extra_targets.iter()) {
                if let PassTarget::Offscreen(name) = target {
                    defined_targets.insert(name.clone());
                }
            }
        }
        self.passes = passes;
        self.signature = Some(signature);
        self.inputs = Some(inputs);
    }

    /// Hand each pass the draws it was given.
    ///
    /// A geometry pass that names a shader draws the instances shaded by that
    /// shader and leaves the rest to the other passes; one that names none draws
    /// every instance, each with the pipeline its own material names, which is
    /// what the built-in chain is. A fullscreen pass draws nothing but its
    /// triangle, and reads what earlier passes left in the targets it names.
    pub(crate) fn plan<'a>(
        &mut self,
        chain: &'a [ResolvedPass],
        render_queue: &'a [RenderQueueItem],
        shader_handles: &HashMap<u64, RendererShaderHandle>,
    ) -> Vec<PassPlan<'a>> {
        let mut plan = Vec::with_capacity(chain.len());
        for (index, pass) in chain.iter().enumerate() {
            // The pass's own target first, then the ones it writes beside it.
            let outputs: Vec<PassOutput<'_>> = std::iter::once(&pass.target)
                .chain(pass.extra_targets.iter())
                .map(|target| match target {
                    PassTarget::Surface => PassOutput::Surface,
                    PassTarget::Offscreen(name) => PassOutput::Offscreen(name.as_str()),
                })
                .collect();

            match pass.kind {
                PassKind::Geometry => {
                    let reason = match self.passes.get(index) {
                        Some(PassSlot::Drawable(_)) => None,
                        Some(PassSlot::Unshaded) => None,
                        Some(PassSlot::Unsupported(reason)) => Some(reason.clone()),
                        _ => Some("it has no pipeline".to_owned()),
                    };
                    if let Some(reason) = reason {
                        self.report_skip(&pass.name, &reason);
                        continue;
                    }

                    let items: Cow<'a, [RenderQueueItem]> = match pass.shader {
                        // The pass draws the instances shaded by the shader it
                        // names, when that shader is still loaded. One that no
                        // longer is draws nothing: falling back to the default
                        // shader would hand this pass instances another pass
                        // already took.
                        Some(key) => match shader_handles.get(&key) {
                            Some(handle) => {
                                let index = handle.data().index as u8;
                                Cow::Owned(
                                    render_queue
                                        .iter()
                                        .copied()
                                        .filter(|item| {
                                            decompose_render_queue_key(item.key).shader_index
                                                == index
                                        })
                                        .collect::<Vec<_>>(),
                                )
                            }
                            None => Cow::Borrowed(&[] as &[RenderQueueItem]),
                        },
                        // No shader named: the pass draws every instance, each
                        // with the pipeline its own material names. This is the
                        // built-in chain, and it borrows the queue rather than
                        // copying it.
                        None => Cow::Borrowed(render_queue),
                    };
                    let pass_index = match self.passes.get(index) {
                        Some(PassSlot::Drawable(_)) => Some(index),
                        _ => None,
                    };
                    plan.push(PassPlan::Geometry {
                        label: pass.name.as_str(),
                        items,
                        outputs,
                        clear: plan.is_empty(),
                        pass_index,
                    });
                }
                PassKind::Fullscreen => {
                    let reason = match self.passes.get(index) {
                        Some(PassSlot::Drawable(_)) => None,
                        Some(PassSlot::Unsupported(reason)) => Some(reason.clone()),
                        _ => Some("it has no fullscreen pipeline".to_owned()),
                    };
                    match reason {
                        None => plan.push(PassPlan::Fullscreen {
                            label: pass.name.as_str(),
                            pass_index: index,
                            outputs,
                            clear: plan.is_empty(),
                        }),
                        Some(reason) => self.report_skip(&pass.name, &reason),
                    }
                }
            }
        }

        // A chain with nothing recordable in it still has to open the frame's
        // targets: a swapchain image nobody cleared presents whatever was in it.
        if plan.is_empty() {
            plan.push(PassPlan::Geometry {
                label: "frame.clear",
                items: Cow::default(),
                outputs: vec![PassOutput::Surface],
                clear: true,
                pass_index: None,
            });
        }
        plan
    }

    /// Forget the pass objects and what they were built from, so the next frame
    /// rebuilds them.
    ///
    /// The offscreen targets are deliberately kept: they are keyed by the
    /// chain's layout and the surface size, and an asset edit - which is what
    /// calls this - moves neither. [`invalidate_targets`](Self::invalidate_targets)
    /// is the version for a surface size that did move.
    pub(crate) fn invalidate(&mut self) {
        self.passes.clear();
        self.signature = None;
        self.inputs = None;
    }

    /// Forget the offscreen layout as well, targets included.
    ///
    /// For a surface size that moved: every target is built at the surface size,
    /// so a new one makes all of them wrong.
    pub(crate) fn invalidate_targets(&mut self) {
        self.invalidate();
        self.offscreen_key = None;
    }

    /// Write the chain to the log, once per change.
    ///
    /// Each pass is named with the number of draws it was handed, which is the
    /// one thing that tells a chain doing what the game asked apart from one
    /// quietly falling back to the built-in pass. A line per frame would bury
    /// everything else, and an unchanged chain is the normal case.
    pub(crate) fn log(&mut self, plan: &[PassPlan]) {
        let mut signature = String::new();
        for entry in plan {
            if !signature.is_empty() {
                signature.push(' ');
            }
            signature.push_str(entry.label());
            signature.push('(');
            signature.push_str(&entry.draws().to_string());
            signature.push(')');
        }
        if self.chain_log.as_deref() == Some(signature.as_str()) {
            return;
        }

        // Printed, like the renderer's other once-per-change diagnostics: the
        // log target this would otherwise use is filtered out of the host's log,
        // and this line is what a reader has when the window shows the wrong
        // thing.
        println!("[render] Pass chain: {signature}");
        self.chain_log = Some(signature);
    }

    /// Name a pass the renderer cannot record, once rather than once per frame.
    fn report_skip(&mut self, name: &str, reason: &str) {
        if !self.skipped_passes.insert(name.to_owned()) {
            return;
        }

        // Printed, like the renderer's other once-per-change diagnostics: the
        // log target this would otherwise use is filtered out of the host's log.
        println!("[render] Pass {name} is not drawn: {reason}");
    }
}

/// One pass's GPU object, or the reason there is none.
///
/// `defined_targets` is what earlier passes of the chain declared so far; the
/// pass may only read those.
fn build_pass(
    pass: &ResolvedPass,
    shaders_by_key: &HashMap<u64, &Shader>,
    defined_targets: &HashSet<String>,
    chain: &ChainContext<'_>,
) -> PassSlot {
    // A geometry pass with no shader of its own draws through each material's
    // pipeline, and that is the only path that needs no object.
    let Some(shader_key) = pass.shader else {
        return match pass.kind {
            PassKind::Geometry => PassSlot::Unshaded,
            PassKind::Fullscreen => {
                PassSlot::Unsupported("it names no shader the renderer loaded".to_owned())
            }
        };
    };

    let Some(shader) = shaders_by_key.get(&shader_key).copied() else {
        return PassSlot::Unsupported("it names no shader the renderer loaded".to_owned());
    };
    let Some(renderer_shader) = chain
        .shader_handles
        .get(&shader_key)
        .and_then(|handle| chain.state.renderer_resource_storage.shaders.get(*handle))
    else {
        return PassSlot::Unsupported("its shader has no pipeline".to_owned());
    };
    // One format per target the pass writes, in the order the shader's
    // `SV_TARGET` list names them. A target the pass writes beside its own is
    // what a geometry pass leaves a normal buffer in.
    let target_formats: Vec<wgpu::TextureFormat> = std::iter::once(&pass.target)
        .chain(pass.extra_targets.iter())
        .map(|target| match target {
            PassTarget::Surface => chain.state.surface.format(),
            PassTarget::Offscreen(name) => chain
                .state
                .offscreen
                .get(name)
                .map(|texture| texture.texture.format())
                .unwrap_or(OFFSCREEN_FORMAT),
        })
        .collect();

    // The pass's own committed textures, by slot. They resolve to the same GPU
    // handles a material's textures do, so a pass and a material reach one
    // texture by one key. A slot whose texture is not loaded falls through to
    // the binder's default, which the log names rather than leaving a pass
    // quietly showing the wrong map.
    let mut textures: Vec<(String, RendererTextureHandle)> = Vec::new();
    for (slot, key) in &pass.textures {
        match chain.texture_handles.get(key) {
            Some(handle) => textures.push((slot.clone(), *handle)),
            None => println!(
                "[render] Pass {} binds texture `{slot}`, which is not loaded; the shader's default is used",
                pass.name
            ),
        }
    }

    let storage = &chain.state.renderer_resource_storage;
    match RendererPass::new(
        &chain.state.device,
        &chain.state.queue,
        storage,
        &storage.engine_parameters.bind_group_layout,
        &chain.state.camera_bind_group_layout,
        pass,
        shader,
        renderer_shader,
        &target_formats,
        chain.state.depth_format,
        &chain.state.offscreen,
        &chain.state.depth_texture,
        &textures,
        defined_targets,
    ) {
        Ok(value) => PassSlot::Drawable(Box::new(value)),
        Err(error) => PassSlot::Unsupported(error.to_string()),
    }
}

/// A signature of everything a chain's GPU objects depend on.
///
/// The chain is read every frame but its pipelines are not rebuilt every frame:
/// this is what decides whether they have to be. It covers the chain's content,
/// including pass parameters and the textures a pass names - both iterated in
/// slot order so a `HashMap`'s arbitrary order cannot make an unchanged chain
/// look new - plus the resource epoch, which moves whenever a shader or texture
/// the bind groups reference was recreated.
fn chain_signature(chain: &[ResolvedPass], resource_epoch: u64) -> String {
    let mut signature = format!("e{resource_epoch}");
    for pass in chain {
        signature.push('|');
        signature.push_str(&pass.name);
        signature.push_str(match pass.kind {
            PassKind::Geometry => " geometry",
            PassKind::Fullscreen => " fullscreen",
        });
        match &pass.target {
            PassTarget::Surface => signature.push_str(" surface"),
            PassTarget::Offscreen(name) => {
                signature.push_str(" offscreen:");
                signature.push_str(name);
            }
        }
        if let Some(shader) = pass.shader {
            signature.push_str(&format!(" shader:{shader}"));
        }
        for extra in &pass.extra_targets {
            match extra {
                PassTarget::Surface => signature.push_str(" also:surface"),
                PassTarget::Offscreen(name) => {
                    signature.push_str(" also:offscreen:");
                    signature.push_str(name);
                }
            }
        }
        signature.push_str(&format!(
            " scale:{} blend:{} depth_write:{} cull:{:?}",
            pass.target_scale, pass.blend, pass.depth_write, pass.cull
        ));
        let mut inputs: Vec<_> = pass.inputs.iter().collect();
        inputs.sort();
        for (slot, target) in inputs {
            signature.push_str(&format!(" input:{slot}={target}"));
        }
        // A pass's textures are part of it for the same reason its parameters
        // are: the handle the bind group reads is resolved here, and it is
        // built once and then kept.
        let mut textures: Vec<_> = pass.textures.iter().collect();
        textures.sort();
        for (slot, texture) in textures {
            signature.push_str(&format!(" texture:{slot}={texture}"));
        }
        // Serialized, not hashed: a pass's parameters live in a uniform buffer
        // the bind group keeps, so this is what makes editing one rebuild it.
        let mut parameters: Vec<_> = pass.parameters.iter().collect();
        parameters.sort_by_key(|(name, _)| name.as_str());
        for (name, parameter) in parameters {
            signature.push_str(&format!(" param:{name}={parameter:?}"));
        }
    }
    signature
}

/// A signature of the offscreen targets a chain needs: every target name, the
/// scale of the pass that names it, and the surface size it is built at.
///
/// Kept apart from the pipeline signature on purpose: editing an asset moves
/// the pipeline signature (a pass's parameters are part of it) but changes none
/// of this, so the offscreen textures survive an asset edit.
fn offscreen_signature(chain: &[ResolvedPass], width: u32, height: u32) -> String {
    let mut signature = format!("{width}x{height}");
    for pass in chain {
        let scale = pass.target_scale.max(1);
        for target in std::iter::once(&pass.target).chain(pass.extra_targets.iter()) {
            if let PassTarget::Offscreen(name) = target {
                signature.push('|');
                signature.push_str(name);
                signature.push_str(&format!("@{scale}"));
            }
        }
    }
    signature
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(kind: PassKind, target: PassTarget) -> ResolvedPass {
        ResolvedPass {
            name: "p".to_owned(),
            shader: None,
            kind,
            target,
            extra_targets: Vec::new(),
            target_scale: 1,
            blend: false,
            depth_write: true,
            cull: crate::frame::CullMode::Back,
            parameters: std::collections::BTreeMap::new(),
            inputs: std::collections::BTreeMap::new(),
            textures: std::collections::BTreeMap::new(),
            order: 0,
        }
    }

    #[test]
    fn a_chain_signature_covers_everything_the_gpu_objects_depend_on() {
        let surface = pass(PassKind::Geometry, PassTarget::Surface);
        let offscreen = pass(PassKind::Geometry, PassTarget::Offscreen("hdr".to_owned()));
        let mut reading = pass(PassKind::Geometry, PassTarget::Surface);
        reading.inputs.insert("hdr".to_owned(), "hdr".to_owned());

        let base = chain_signature(std::slice::from_ref(&surface), 1);

        assert_eq!(base, chain_signature(std::slice::from_ref(&surface), 1));
        assert_ne!(base, chain_signature(std::slice::from_ref(&offscreen), 1));
        assert_ne!(base, chain_signature(std::slice::from_ref(&reading), 1));
        assert_ne!(
            base,
            chain_signature(std::slice::from_ref(&surface), 2),
            "a pass's parameters live in the asset, so the asset revision is part of it"
        );
    }

    #[test]
    fn a_chain_with_nothing_recordable_still_clears_the_frame() {
        // Nothing was built for this chain, which is every pass's fate on the
        // frame a chain first arrives: none of them may be skipped without the
        // frame targets being opened anyway.
        let chain = vec![pass(PassKind::Geometry, PassTarget::Surface)];
        let mut pipeline = ScriptableRenderingPipeline::new();

        let plan = pipeline.plan(&chain, &[], &HashMap::new());

        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].label(), "frame.clear");
        assert!(plan[0].clears());
        assert_eq!(plan[0].draws(), 0);
        assert_eq!(plan[0].outputs().len(), 1);
    }

    #[test]
    fn a_geometry_pass_with_no_shader_takes_the_whole_queue() {
        let chain = vec![pass(PassKind::Geometry, PassTarget::Surface)];
        let queue = vec![
            RenderQueueItem {
                key: 0,
                entity_index: 0,
            },
            RenderQueueItem {
                key: 0,
                entity_index: 1,
            },
        ];
        let mut pipeline = ScriptableRenderingPipeline::new();
        pipeline.passes.push(PassSlot::Unshaded);

        let plan = pipeline.plan(&chain, &queue, &HashMap::new());

        assert_eq!(plan.len(), 1);
        match &plan[0] {
            PassPlan::Geometry {
                items, pass_index, ..
            } => {
                // Borrowed, not copied: the built-in chain pays for nothing.
                assert_eq!(items.len(), 2);
                assert!(matches!(items, Cow::Borrowed(_)));
                // No pass pipeline of its own: each instance draws through the
                // pipeline its material names.
                assert!(pass_index.is_none());
            }
            PassPlan::Fullscreen { .. } => panic!("a geometry pass planned as fullscreen"),
        }
    }
}
