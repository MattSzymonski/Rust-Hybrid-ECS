//! The frame the renderer draws: plain data a project's world hands the
//! renderer once per update.
//!
//! # Responsibilities
//!
//! - Define [`RenderFrame`], the per-update renderer input, and the two values
//!   it carries: [`RenderInstance`] for each drawable and [`ResolvedPass`] for
//!   each pass of the chain.
//!
//! # Design
//!
//! Only the types live here; the system that fills them from the world is the
//! renderer module's. The frame crosses the artifact boundary - whatever fills
//! it, the renderer module reads it - so what it carries is plain data: asset
//! keys instead of handles, the camera copied out rather than borrowed. The
//! assets behind those keys are read from the world's `AssetManager` at render
//! time, so the frame never copies the asset store.

// Standard library
use std::collections::HashMap;

// External crates
use pill_engine::Resource;

// Current crate
use crate::{
    assets::{CullMode, MaterialParameter, PassKind, PassTarget},
    components::{CameraComponent, TransformComponent},
};

/// One drawable the frame collected: where it stands and what it draws with.
///
/// Built from a renderer whose mesh and material both still resolve. The
/// renderer turns each instance into GPU work, batching runs that share a
/// shader, material, and mesh into single instanced draws.
#[derive(Clone, Debug)]
pub struct RenderInstance {
    /// World transform the instance is drawn at.
    pub transform: TransformComponent,
    /// Asset key of the mesh to draw.
    pub mesh: u64,
    /// Asset key of the material to draw it with.
    pub material: u64,
    /// The material's rendering order, resolved here so the renderer can sort
    /// draws without going back to the material asset for each one.
    pub rendering_order: u8,
}

/// One pass of the chain the renderer runs, with every handle resolved.
///
/// The renderer only ever sees a [`RenderFrame`], and the manager holding the
/// pipeline is a resource of the world, so the chain crosses into the frame as
/// plain values: `shader` is the asset key the renderer finds its GPU shader
/// by, and `None` leaves the choice to the instances, each drawing with the
/// shader its own material names.
#[derive(Clone, Debug)]
pub struct ResolvedPass {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    pub shader: Option<u64>,
    /// What the pass draws.
    pub kind: PassKind,
    /// Where the pass reads and writes.
    pub target: PassTarget,
    /// Further targets the pass writes in the same draw.
    pub extra_targets: Vec<PassTarget>,
    /// Divisor for the size of the targets this pass writes.
    pub target_scale: u32,
    /// Blend over the target instead of replacing it.
    pub blend: bool,
    /// Write depth, or only read it.
    pub depth_write: bool,
    /// Which faces the pass drops.
    pub cull: CullMode,
    /// Uniform parameters, packed exactly as a material packs its own.
    pub parameters: HashMap<String, MaterialParameter>,
    /// Offscreen targets the pass samples, by the texture slot they bind to.
    pub inputs: HashMap<String, String>,
    /// Committed textures the pass samples, by the texture slot they bind to,
    /// as the asset keys `shader` is a key for.
    pub textures: HashMap<String, u64>,
    /// Ordering key: lower runs first. Equal orders keep the pipeline's order.
    pub order: u8,
}

impl ResolvedPass {
    /// A geometry pass over the swapchain that filters no instance.
    ///
    /// This is the chain a project that sets no pipeline runs, and the shape a
    /// project's own single-pass pipeline takes.
    pub fn builtin() -> Self {
        Self {
            name: "builtin.default".to_owned(),
            shader: None,
            kind: PassKind::Geometry,
            target: PassTarget::Surface,
            extra_targets: Vec::new(),
            target_scale: 1,
            blend: false,
            depth_write: true,
            cull: CullMode::Back,
            parameters: HashMap::new(),
            inputs: HashMap::new(),
            textures: HashMap::new(),
            order: 0,
        }
    }
}

/// One frame's renderer input: the instances, the pass chain, the camera, and
/// the clock, extracted from the world and handed to the renderer.
///
/// Everything here crosses as plain owned values, because the frame is what a
/// project's copy of this crate hands the host's renderer: asset keys instead
/// of handles, the camera copied out rather than borrowed. The assets behind
/// those keys are read from the world's `AssetManager` at render time, so the
/// frame stays small however large the project's assets are.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    /// Every drawable the world offered this update, in traversal order.
    pub instances: Vec<RenderInstance>,
    /// The chain to run this frame, as resolved from the pipeline the game set
    /// in [`RenderingManager`](crate::RenderingManager). Empty asks for a cleared frame and nothing
    /// else, which is what a pipeline with every pass disabled means.
    pub passes: Vec<ResolvedPass>,
    /// What the current `passes` were resolved from: the asset revision and the
    /// pipeline's asset key, when the manager holds one.
    ///
    /// Resolution clones every pass's strings and maps, so it repeats only when
    /// one of these moved - and `chain_generation` lets the renderer tell that
    /// the chain it built its pipelines from is still this very chain.
    pub chain_source: Option<(u64, Option<u64>)>,
    /// Bumped whenever `passes` is resolved again.
    pub chain_generation: u64,
    /// Seconds since the frame loop started.
    ///
    /// Carried into the engine's parameters so a shader can animate without the
    /// game handing it a value through a pass parameter, which is read when the
    /// chain is built rather than once a frame.
    pub seconds: f32,
    /// This frame's own duration, the amount `seconds` advances by.
    pub delta_seconds: f32,
    /// Camera the frame renders through, when one is enabled.
    pub camera: CameraComponent,
    /// World transform of that camera.
    pub camera_transform: TransformComponent,
    /// Whether an enabled camera was found; the renderer skips a frame
    /// without one.
    pub has_camera: bool,
    /// Number of this frame, counting from one; packed into the engine
    /// parameters beside the clocks.
    pub sequence: u64,
}

impl Default for RenderFrame {
    fn default() -> Self {
        Self {
            instances: Vec::new(),
            passes: Vec::new(),
            chain_source: None,
            chain_generation: 0,
            seconds: 0.0,
            delta_seconds: 0.0,
            camera: CameraComponent::default(),
            camera_transform: TransformComponent::default(),
            has_camera: false,
            sequence: 0,
        }
    }
}

impl Resource for RenderFrame {
    fn shared_name() -> Option<&'static str> {
        Some("pill_master_renderer::frame::RenderFrame")
    }
}
