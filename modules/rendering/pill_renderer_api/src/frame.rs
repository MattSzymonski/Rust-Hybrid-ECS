//! The frame the renderer draws: plain data a project's world hands the
//! renderer once per update.
//!
//! # Responsibilities
//!
//! - Define [`RenderFrame`], the per-update renderer input, and the two values
//!   it carries: [`RenderInstance`] for each drawable and [`ResolvedPass`] for
//!   each pass of the chain.
//! - Define the vocabulary a resolved pass is written in: [`PassSettings`],
//!   [`PassKind`], [`PassTarget`], [`CullMode`] and [`MaterialParameter`].
//!
//! # Design
//!
//! Only the types live here; the system that fills them from the world is the
//! renderer module's. The frame crosses the artifact boundary, so what it
//! carries is plain data: asset keys instead of handles, the camera copied out
//! rather than borrowed. The assets behind those keys are read from the
//! world's `AssetManager` at render time, so the frame never copies the store.
//!
//! **This module is renderer contract.** `PillRenderer::render` takes a
//! [`RenderFrame`], so everything the frame names must be available to every
//! renderer and to the host, whichever renderer's data crate a project uses.
//! It therefore depends on nothing in the renderer's own data crate: the pass
//! and parameter types it needs are defined here, and the asset types that use
//! them import them from here. The camera is contract for the same reason; the
//! transform is the engine's.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_engine::common_components::TransformComponent;
use pill_engine::Resource;

/// What a pass draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PassKind {
    /// Draws the scene's meshes, one instance batch per material.
    #[default]
    Geometry,
    /// Draws one fullscreen triangle: the shape post-processing passes take.
    Fullscreen,
    /// Draws one fullscreen triangle at the far plane, depth-tested against
    /// what the geometry passes left: a background that shows only where no
    /// mesh was drawn. Its shader (usually from the pass's material) turns each
    /// pixel into a view direction and samples a sky by it.
    Skybox,
}

/// Where a pass reads from and writes to.
#[derive(Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum PassTarget {
    /// The swapchain image. A pass writing here reads nothing and ends the
    /// frame, so a pipeline has at most one such pass and it runs last.
    #[default]
    Surface,
    /// An offscreen colour target, named so later passes can sample it.
    ///
    /// The renderer owns these: a name that no earlier pass declares is an
    /// error when the pipeline is built, not a silently blank frame.
    Offscreen(String),
}

/// Which faces a pass drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum CullMode {
    /// Back faces: the default for solid geometry.
    #[default]
    Back,
    /// Front faces, for looking at the inside of a shape.
    Front,
    /// Nothing: a fullscreen triangle has no outside worth dropping, and
    /// two-sided geometry has no inside.
    None,
}

/// How one pass draws: what it renders, where it writes, and how the draw is
/// set up.
///
/// This is the pass settings block, shared by all three places a pass is
/// written down - the `RenderPass` asset a game builds, the document its
/// `.render_pass` file holds and the [`ResolvedPass`] the frame carries - so a
/// new setting is one field here and one line in [`Self::default`], not a field
/// in each type with a default in each crate.
///
/// Serializable, and flattened into the pass document: a `.render_pass` file
/// carries these fields at its top level, beside the shader and parameter
/// maps.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PassSettings {
    /// What the pass draws.
    pub kind: PassKind,
    /// Where the pass reads and writes.
    pub target: PassTarget,
    /// Further targets the pass writes in the same draw, in the order the
    /// shader's `SV_TARGET1`, `SV_TARGET2` ... name them.
    ///
    /// This is how a geometry pass leaves a normal buffer behind for a later
    /// pass to read: one draw, several pictures.
    pub extra_targets: Vec<PassTarget>,
    /// Divisor for the size of the targets this pass writes: 2 is half the
    /// surface, 1 (the default) is all of it.
    ///
    /// The first pass to name a target decides its size, because the target
    /// belongs to the chain rather than to any one pass. A pass at half size is
    /// how the expensive, blurry steps - ambient occlusion, depth of field, a
    /// bloom level - cost a quarter of the pixels. Only a fullscreen pass can be
    /// scaled: a pass's attachments must all be one size, and a geometry or
    /// skybox pass pairs its colour target with the surface's depth buffer, so
    /// the renderer does not draw such a pass at a scale.
    pub target_scale: u32,
    /// Whether the pass blends its output over what the target already holds.
    pub blend: bool,
    /// Whether the pass writes depth. A translucent pass reads depth and does
    /// not write it, so what is behind it still passes the test.
    pub depth_write: bool,
    /// Which faces the pass drops.
    pub cull: CullMode,
    /// Ordering key: lower runs first. Equal orders keep the pipeline's order.
    pub order: u8,
}

impl Default for PassSettings {
    /// A geometry pass writing the surface at full size, back-face culled,
    /// writing depth, not blending, running first.
    fn default() -> Self {
        Self {
            kind: PassKind::Geometry,
            target: PassTarget::Surface,
            extra_targets: Vec::new(),
            target_scale: 1,
            blend: false,
            depth_write: true,
            cull: CullMode::Back,
            order: 0,
        }
    }
}

/// One named value a shader reads.
///
/// Parameters are keyed by the slot name the shader declares, and the packer
/// walks the shader's slots rather than the map, so a name the shader never
/// declares is ignored instead of shifting anything after it. Each value pads
/// out to one 16-byte uniform slot.
///
/// Serializable because a material file stores its parameters as written:
/// `{"Scalar": 1.0}`, `{"Bool": true}` or `{"Color": [1.0, 0.5, 0.0]}`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MaterialParameter {
    /// One `f32`, in the first four bytes of the slot.
    Scalar(f32),
    /// A boolean, packed as a `u32` of 0 or 1.
    Bool(bool),
    /// An RGB colour: three `f32`s with the rest of the slot as padding.
    Color([f32; 3]),
}

/// The key of one asset, as a frame and a renderer name it.
///
/// A key packs the asset's generation in its high 32 bits and its index in the
/// low 32, so a key handed out before the store carried a new generation never
/// equals one from the live store: a renderer keys its GPU objects by this value
/// and knows when to rebuild them. The asset layer computes a key from a handle;
/// the contract only carries it, which is why the type lives here beside the
/// frame rather than with the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetKey(pub u64);

/// One drawable the frame collected: where it stands and what it draws with.
///
/// Built from a renderer whose mesh and material both still resolve. The
/// renderer turns each instance into GPU work, batching runs that share a
/// shader, material, and mesh into single instanced draws.
#[derive(Clone, Debug)]
pub struct RenderInstance {
    /// The entity the instance belongs to: what outlining and picking address.
    pub entity: pill_engine::Entity,
    /// World transform the instance is drawn at.
    pub transform: TransformComponent,
    /// Key of the mesh to draw.
    pub mesh: AssetKey,
    /// Key of the material to draw it with.
    pub material: AssetKey,
    /// The material's rendering order, resolved here so the renderer can sort
    /// draws without going back to the material asset for each one.
    pub rendering_order: u8,
}

/// One pass of the chain the renderer runs, with every handle resolved.
///
/// Its maps are `BTreeMap`s, never `HashMap`s. An empty `HashMap` does not
/// allocate: it points at a static inside the binary that created it. This
/// value lives in the world and outlives that binary - a project or module
/// that built it is reloaded and its retired image eventually unmapped - and
/// reading such a map afterwards faults. A `BTreeMap` holds no such pointer.
///
/// Equality is the renderer's rebuild test: a chain is rebuilt when a pass is
/// no longer equal to the one its GPU objects were built from, so a field added
/// here is compared without anything else having to learn about it.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPass {
    /// Label used in logs, profiling and error messages.
    pub name: String,
    /// Key of the shader the pass draws with; `None` draws each instance
    /// with the shader its own material names.
    pub shader: Option<AssetKey>,
    /// What the pass draws, where it writes, and how the draw is set up.
    pub settings: PassSettings,
    /// Uniform parameters, packed exactly as a material packs its own.
    pub parameters: BTreeMap<String, MaterialParameter>,
    /// Offscreen targets the pass samples, by the texture slot they bind to.
    pub inputs: BTreeMap<String, String>,
    /// Committed textures the pass samples, by the texture slot they bind to,
    /// as the asset keys `shader` is a key for.
    pub textures: BTreeMap<String, AssetKey>,
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
            settings: PassSettings::default(),
            parameters: BTreeMap::new(),
            inputs: BTreeMap::new(),
            textures: BTreeMap::new(),
        }
    }
}

/// What a frame's resolved pass chain was read from, the two values whose
/// movement means the chain has to be resolved again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainSource {
    /// The asset manager's revision when the chain was resolved.
    pub revision: u64,
    /// Key of the pipeline asset the chain came from, when one is selected.
    pub pipeline: Option<AssetKey>,
}

/// One frame's renderer input: the instances, the pass chain, the camera, and
/// the clock, extracted from the world and handed to the renderer.
///
/// Everything crosses as plain owned values, so a frame outlives the artifact
/// that filled it.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    /// Every drawable the world offered this update, in traversal order.
    pub instances: Vec<RenderInstance>,
    /// The chain to run this frame, as resolved from the pipeline the game set
    /// in the renderer's pipeline-selection resource. Empty asks for a cleared
    /// frame and nothing else, which is what a pipeline with every pass disabled
    /// means.
    pub passes: Vec<ResolvedPass>,
    /// What the current `passes` were resolved from.
    ///
    /// Resolution clones every pass's strings and maps, so it repeats only when
    /// this value moved - and `chain_generation` lets the renderer tell that
    /// the chain it built its pipelines from is still this very chain.
    pub chain_source: Option<ChainSource>,
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
    /// The camera each surface draws through this frame, by surface id.
    ///
    /// Every surface is in here or it is not drawn - the primary one
    /// ([`crate::SurfaceId::PRIMARY`]) included, whose camera is the one
    /// [`crate::SurfaceCameras`] binds to it when it has a binding and the
    /// frame's chosen world camera otherwise. Resolved once per frame by the
    /// renderer's frame writer.
    pub surface_views: BTreeMap<crate::SurfaceId, crate::SurfaceView>,
    /// What systems debug-drew for this frame: lines, labels and gizmo
    /// handles, empty while debug drawing is off.
    pub debug: pill_engine::DebugFrame,
    /// The entities to outline this frame, and the outline colour.
    ///
    /// The [`crate::SelectionOutlines`] resource's value, carried as it is
    /// rather than copied field by field, so an editor writes one resource and
    /// the renderer reads one value: an empty `entities` means no outline, the
    /// same way it does for the resource.
    pub outlines: crate::SelectionOutlines,
    /// The distance between the scene grid's minor lines, in world units, when
    /// a frontend turned the grid on; `None` draws no grid.
    ///
    /// The surfaces it is drawn on are [`Self::editor_views`] - the same
    /// view-bound surfaces that carry the editor's other chrome - so the frame
    /// names that set once instead of twice.
    pub grid_cell_size: Option<f32>,
    /// The surfaces this frame marks as editor views, ascending.
    ///
    /// A surface bound to a view camera is an editor's own viewport: it draws
    /// the grid and carries the editor's chrome - the transform gizmo and the
    /// selection outlines. A surface drawn through a world camera is a game's
    /// window and never gets that chrome. Empty means no editor is looking.
    pub editor_views: Vec<crate::SurfaceId>,
    /// The picks asked for since the last frame; the renderer answers them
    /// through [`crate::PillRenderer::take_pick_results`].
    pub pick_requests: Vec<crate::PickRequest>,
    /// The previews asked for since the last frame; the
    /// renderer draws at most one per frame and answers them through
    /// [`crate::PillRenderer::take_preview_results`].
    pub preview_requests: Vec<crate::PreviewRequest>,
    /// Number of this frame, counting from one; packed into the engine
    /// parameters beside the clocks.
    pub sequence: u64,
    /// The UI to draw over the frame: the draw lists a game's or tool's UI
    /// pass built this update, per surface, and the textures they sample.
    /// Empty means no UI.
    pub ui: pill_ui_frame::UiFrame,
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
            surface_views: BTreeMap::new(),
            debug: pill_engine::DebugFrame::default(),
            outlines: crate::SelectionOutlines::default(),
            grid_cell_size: None,
            editor_views: Vec::new(),
            pick_requests: Vec::new(),
            preview_requests: Vec::new(),
            sequence: 0,
            ui: pill_ui_frame::UiFrame::default(),
        }
    }
}

impl Resource for RenderFrame {
    fn shared_name() -> Option<&'static str> {
        // A fixed identity, not a path: it is how the host and the renderer
        // module resolve the same resource across the artifact boundary, so the
        // name stays what it has always been even though the path it spells is
        // the renderer module's rather than this crate's. Keep it verbatim.
        Some("pill_master_renderer::frame::RenderFrame")
    }
}
