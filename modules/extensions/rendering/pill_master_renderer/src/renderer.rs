//! The master renderer: draws a frame's resolved chain and instances, over the
//! device, surface, and frame state [`State`] owns.
//!
//! # Responsibilities
//!
//! - Keep the GPU caches level with each frame's asset store, one asset
//!   type at a time, rebuilding only what moved ([`Renderer`] and its
//!   per-type sync helpers).
//! - Keep the GPU caches level with each frame's asset store, one asset
//!   type at a time, rebuilding only what moved ([`Renderer`] and its
//!   per-type sync helpers).
//! - Hold the chain's GPU objects in a `ScriptableRenderingPipeline`: ask it to
//!   rebuild them when the frame says they moved, and to plan each frame's
//!   passes.
//! - Submit the frame through [`State`]: one command encoder, one wgpu render
//!   pass per planned pass, then the present.
//!
//! # Design
//!
//! The renderer is the two halves a frame needs: an asset store made level with
//! the GPU, and a chain made into passes. Both are gated on something cheap -
//! the store's revision, the chain's generation - so an unchanged frame costs
//! two comparisons and no rebuilds. Creation failures are reported and skipped
//! rather than fatal: a shader, texture, or target that would not build is
//! named in the log while the rest of the revision still reaches the GPU.

#![allow(clippy::too_many_arguments)]

// Standard library
use std::{
    collections::HashMap,
    time::Instant,
};

// External crates
use pill_core::{info, PillStyle};
use pill_engine::AssetManager;

// Current crate
use crate::{
    api::{FrameOutcome, PillRenderer, RenderCapabilities, RenderMetrics},
    asset_mirror::AssetMirror,
    assets::PassTarget,
    components::RenderViewport,
    config::{
        CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX, ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        INSTANCE_BATCH_SIZE, MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
        MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
    },
    drawers::mesh_drawer::MeshDrawer,
    error::{capturing_validation, RendererError, Result},
    frame::{RenderFrame, ResolvedPass},
    pipeline::{PassOutput, PassPlan, PassSlot, ScriptableRenderingPipeline},
    resource_handles::RendererCameraHandle,
    resources::{RendererCamera, RendererResourceStorage, RendererTexture},
    surface::{RendererWindow, Surface},
    Instance,
};

/// Colour every frame starts from, whatever the first pass is.
const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.15,
    g: 0.15,
    b: 0.15,
    a: 1.0,
};

/// Format of every offscreen colour target a chain declares.
///
/// Half-float rather than the surface's `Unorm`: the values a lit frame
/// produces run well past 1.0, and a target that cannot hold them clips the
/// picture before the pass that was going to bring it back into range runs.
pub(crate) const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// The master renderer: the asset caches, the frame's chain, and the submission
/// behind the [`PillRenderer`] contract.
///
/// One renderer owns one window's surface and everything drawn to it: the GPU
/// state, the asset mirror that keeps the store level with the GPU, and the
/// chain that turns a frame into passes.
pub struct Renderer {
    /// The wgpu device, surface, and frame state this renderer drives.
    pub state: State,
    /// The GPU object behind every live asset, and the version it was built
    /// from: the frame-to-GPU diff, one asset type at a time.
    assets: AssetMirror,
    camera: RendererCameraHandle,
    viewport: Option<RenderViewport>,
    minimized: bool,
    /// The frame's chain as GPU objects: a pass pipeline each, and the plan a
    /// frame is recorded from.
    pipeline: ScriptableRenderingPipeline,
    metrics: RenderMetrics,
}

impl Renderer {
    /// Creates the renderer synchronously, blocking on [`Renderer::new_async`].
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Renderer::new_async`], which does the work.
    pub fn new<W: RendererWindow + 'static>(window: W, width: u32, height: u32) -> Result<Self> {
        pollster::block_on(Self::new_async(window, width, height))
    }

    /// Creates the renderer: device and surface, the default material, and the
    /// camera.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when a creation step fails: surface or
    /// adapter acquisition, device creation, surface configuration, the depth
    /// buffer, the resource storage, the default material, or the camera.
    pub async fn new_async<W: RendererWindow + 'static>(
        window: W,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        info!(target: pill_core::telemetry::telemetry_target::RENDERING, "Initializing {}", "Renderer".module_object_style());
        let mut state = State::new(window, width, height).await?;
        let assets = AssetMirror::new(&mut state)?;
        let camera = state
            .renderer_resource_storage
            .cameras
            .insert(RendererCamera::new(
                &state.device,
                state.camera_bind_group_layout.clone(),
            )?);
        Ok(Self {
            state,
            assets,
            camera,
            viewport: None,
            minimized: width == 0 || height == 0,
            pipeline: ScriptableRenderingPipeline::new(),
            metrics: RenderMetrics::default(),
        })
    }
}

impl PillRenderer for Renderer {
    fn capabilities(&self) -> RenderCapabilities {
        let limits = self.state.device.limits();
        RenderCapabilities {
            max_texture_size: limits.max_texture_dimension_2d,
            max_buffer_bytes: limits.max_buffer_size,
            hdr: false,
        }
    }

    fn metrics(&self) -> RenderMetrics {
        self.metrics
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.minimized = width == 0 || height == 0;
        if self.minimized {
            return;
        }
        if let Err(error) = self
            .state
            .resize(winit::dpi::PhysicalSize::new(width, height))
        {
            // Warned instead of panicking: a size the driver will not take is
            // not worth killing a running game over, and the next resize event
            // tries again.
            pill_core::warn!(
                target: pill_core::telemetry::telemetry_target::RENDERING,
                "surface resize to {width}x{height} failed: {error}"
            );
            return;
        }
        // An offscreen target is sized to the surface, so a new surface size
        // makes every one of them wrong; the chain is rebuilt with them on
        // the next frame.
        self.state.offscreen.clear();
        self.pipeline.invalidate_targets();
    }

    fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
    }

    fn render(&mut self, frame: &RenderFrame, assets: &AssetManager) -> Result<FrameOutcome> {
        if self.minimized || !frame.has_camera {
            return Ok(FrameOutcome::Skipped);
        }
        let prepare = Instant::now();
        self.assets.sync(assets, &mut self.state);
        self.pipeline.ensure(
            &frame.passes,
            assets,
            frame.chain_generation,
            self.assets.chain_context(&mut self.state),
        );
        let render_queue = self.assets.build_queue(frame, &self.state);
        self.metrics.prepare_micros = prepare.elapsed().as_micros() as u64;
        self.metrics.instance_bytes = (render_queue.len() * std::mem::size_of::<Instance>()) as u64;

        // The frame's chain generation is what makes reading it every frame
        // cheap: the frame resolves the chain only when it changes, and the
        // chain rebuild restarts only when the generation it built from moves.
        // An empty chain is a game that asked for nothing, not a game that
        // asked for the built-in pass: the frame's writer puts that pass in the
        // chain itself.
        let plan = self
            .pipeline
            .plan(&frame.passes, &render_queue, self.assets.shader_handles());
        self.pipeline.log(&plan);
        self.metrics.draw_calls = plan.iter().filter(|entry| entry.draws() > 0).count() as u32;
        self.metrics.passes = plan.len() as u32;

        let submitted = Instant::now();
        self.state.render(
            self.camera,
            &plan,
            self.pipeline.passes(),
            frame,
            self.viewport,
        )?;
        self.metrics.submit_micros = submitted.elapsed().as_micros() as u64;
        Ok(FrameOutcome::Presented)
    }

    fn invalidate_assets(&mut self) {
        // Forget which versions the GPU objects were built from, so the next
        // sync rebuilds them all, and drop the pass objects so the rebuild
        // reaches the bind groups that reference the old ones.
        self.assets.invalidate();
        self.pipeline.invalidate();
    }
}

/// The wgpu objects behind a [`Renderer`]: device, surface, and frame state.
///
/// Owns the device and queue, the [`Surface`] built from them, the depth
/// texture, and the offscreen colour targets the current chain declares. Kept
/// beside [`Renderer`] rather than inside it so the GPU lifetime - new, resize,
/// reconfigure - has a home independent of the asset caches.
pub struct State {
    /// The per-type cache of every GPU resource - shaders, textures, meshes,
    /// materials - and the engine parameters table.
    pub(crate) renderer_resource_storage: RendererResourceStorage,
    /// The window's swapchain, and the colour format every pipeline that
    /// renders to it declares.
    pub(crate) surface: Surface,
    /// The device every GPU object in this module is created from.
    pub(crate) device: wgpu::Device,
    /// The queue every upload and frame submission goes through.
    pub(crate) queue: wgpu::Queue,
    /// Format of the depth buffer shared by the geometry passes.
    pub(crate) depth_format: wgpu::TextureFormat,
    /// The depth buffer every geometry pass writes. Rebuilt when the surface
    /// is, because it is sized the same way.
    pub(crate) depth_texture: RendererTexture,
    /// The offscreen colour targets the current chain names, by that name.
    ///
    /// Owned here rather than by the passes that write them: two passes name
    /// the same target - one writes it, the next reads it - and a texture owned
    /// by either would be a lifetime the chain cannot express.
    pub(crate) offscreen: HashMap<String, RendererTexture>,
    mesh_drawer: MeshDrawer,
    /// Layout every camera bind group is built from.
    pub(crate) camera_bind_group_layout: wgpu::BindGroupLayout,
}

impl State {
    async fn new<W: RendererWindow + 'static>(window: W, width: u32, height: u32) -> Result<Self> {
        let (surface, device, queue) = Surface::create(window, width, height).await?;
        let depth_format = wgpu::TextureFormat::Depth32Float;
        let depth_texture =
            RendererTexture::new_depth_texture(&device, surface.configuration(), "depth_texture")?;
        // Scoped like every other creation: a refused layout would otherwise
        // reach the uncaptured-error handler and take the host down before a
        // `RendererError` exists to explain it.
        let camera_bind_group_layout = capturing_validation(&device, || {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("camera_parameters_bind_group_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            })
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("camera bind group layout: {detail}"),
        })?;
        let renderer_resource_storage = RendererResourceStorage::new(&device, &queue)?;
        let mesh_drawer = MeshDrawer::new(&device, INSTANCE_BATCH_SIZE as u32);
        Ok(Self {
            renderer_resource_storage,
            surface,
            device,
            queue,
            depth_format,
            depth_texture,
            offscreen: HashMap::new(),
            mesh_drawer,
            camera_bind_group_layout,
        })
    }

    /// Resize the surface and rebuild the depth buffer that matches it.
    ///
    /// Only committed once the surface accepted the new size, so a refusal
    /// leaves the old, consistent surface and depth pair in place instead of a
    /// configuration that disagrees with the swapchain. What used to be an
    /// `expect` here killed the host on a resize it could not honour.
    fn resize(&mut self, new_window_size: winit::dpi::PhysicalSize<u32>) -> Result<()> {
        self.surface
            .resize(&self.device, new_window_size.width, new_window_size.height)?;
        self.depth_texture = RendererTexture::new_depth_texture(
            &self.device,
            self.surface.configuration(),
            "depth_texture",
        )?;
        Ok(())
    }

    /// Create a target for every offscreen output the chain declares.
    ///
    /// Rebuilt whenever the chain is, and sized to the surface: one that no
    /// longer matches the window would be sampled at a different scale from the
    /// pass that wrote it, which shows up as a picture that shrinks with the
    /// window rather than one that resizes with it.
    pub(crate) fn ensure_offscreen_targets(&mut self, chain: &[ResolvedPass]) {
        let (width, height) = self.surface.size();
        let device = &self.device;
        let targets = &mut self.offscreen;
        targets.clear();

        for pass in chain {
            // The pass's own target first, then the ones it writes beside it.
            // The scale is the first pass's: a target belongs to the chain, and
            // the pass that names it first is asking on everyone's behalf.
            let scale = pass.target_scale.max(1);
            let written = std::iter::once(&pass.target).chain(pass.extra_targets.iter());
            for target in written {
                let PassTarget::Offscreen(name) = target else {
                    continue;
                };
                if targets.contains_key(name) {
                    continue;
                }
                match RendererTexture::new_render_target(
                    device,
                    name,
                    (width / scale).max(1),
                    (height / scale).max(1),
                    OFFSCREEN_FORMAT,
                ) {
                    Ok(target) => {
                        targets.insert(name.clone(), target);
                    }
                    // Reported instead of fatal: the passes that read it each
                    // say they are not drawn, which is the same outcome their
                    // own failure path would produce.
                    Err(error) => {
                        pill_core::warn!(
                            target: pill_core::telemetry::telemetry_target::RENDERING,
                            "offscreen target `{name}` was not created: {error}"
                        );
                    }
                }
            }
        }
    }

    fn render(
        &mut self,
        camera_handle: RendererCameraHandle,
        plan: &[PassPlan],
        passes: &[PassSlot],
        frame: &RenderFrame,
        viewport: Option<RenderViewport>,
    ) -> Result<()> {
        // Both borrows are of separate fields, so the surface can reconfigure
        // itself against the device that created it.
        let surface_frame = self.surface.acquire(&self.device)?;
        let (width, height) = self.surface.size();
        let view = surface_frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.renderer_resource_storage.engine_parameters.update(
            &self.queue,
            0.0,
            [0.0; 3],
            [width, height],
            [
                frame.seconds,
                frame.delta_seconds,
                frame.sequence as f32,
                0.0,
            ],
        );
        let camera = self
            .renderer_resource_storage
            .cameras
            .get_mut(camera_handle)
            .ok_or(RendererError::RendererResourceNotFound)?;
        let viewport = viewport
            .and_then(|value| value.clamped_to(width, height))
            .unwrap_or_else(|| RenderViewport::full(width, height));
        camera.update(
            &self.queue,
            &frame.camera,
            &frame.camera_transform,
            viewport.width as f32 / viewport.height as f32,
        );
        let camera = self
            .renderer_resource_storage
            .cameras
            .get(camera_handle)
            .ok_or(RendererError::RendererResourceNotFound)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("render_encoder"),
            });
        // One wgpu render pass per pass of the chain, into the same encoder: the
        // first clears the targets it writes, the rest add to what is there.
        for entry in plan {
            // Every target the pass writes, in the order the shader's
            // `SV_TARGET` list names them.
            let mut views = Vec::with_capacity(entry.outputs().len());
            let mut missing_target = false;
            for output in entry.outputs() {
                match output {
                    PassOutput::Surface => views.push(&view),
                    // A pass whose target was never created draws nothing rather
                    // than into the wrong one; the chain log already named the
                    // pass that could not be built.
                    PassOutput::Offscreen(name) => match self.offscreen.get(*name) {
                        Some(texture) => views.push(&texture.texture_view),
                        None => missing_target = true,
                    },
                }
            }
            if missing_target {
                continue;
            }

            let clear = entry.clears();
            let color_attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = views
                .iter()
                .map(|target| {
                    Some(wgpu::RenderPassColorAttachment {
                        view: target,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: color_load(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    })
                })
                .collect();

            match entry {
                PassPlan::Geometry {
                    label,
                    items,
                    pass_index,
                    ..
                } => {
                    let depth_stencil_attachment = wgpu::RenderPassDepthStencilAttachment {
                        view: &self.depth_texture.texture_view,
                        depth_ops: Some(wgpu::Operations {
                            load: if clear {
                                wgpu::LoadOp::Clear(1.0)
                            } else {
                                wgpu::LoadOp::Load
                            },
                            store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    };
                    // A pass that built a pipeline draws through it: the
                    // pipeline holds the targets' formats and the pass's depth
                    // and culling, none of which an instance may choose.
                    let pipeline = pass_index.and_then(|index| match passes.get(index) {
                        Some(PassSlot::Drawable(pass)) => Some(&pass.pipeline),
                        _ => None,
                    });
                    self.mesh_drawer.record_draw_commands(
                        &self.device,
                        &self.queue,
                        &mut encoder,
                        &self.renderer_resource_storage,
                        label,
                        pipeline,
                        &color_attachments,
                        depth_stencil_attachment,
                        camera,
                        items,
                        &frame.instances,
                        viewport,
                    )?;
                }
                PassPlan::Fullscreen {
                    label, pass_index, ..
                } => {
                    let Some(PassSlot::Drawable(pass)) = passes.get(*pass_index) else {
                        continue;
                    };
                    let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some(label),
                        color_attachments: &color_attachments,
                        // No depth: a fullscreen pass overwrites every pixel it
                        // covers, and the depth left behind describes geometry
                        // that is not what this triangle is.
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                    });
                    render_pass.set_pipeline(&pass.pipeline);
                    if pass.pass_engine_parameters {
                        render_pass.set_bind_group(
                            ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                            &self.renderer_resource_storage.engine_parameters.bind_group,
                            &[],
                        );
                    }
                    if pass.pass_camera_parameters {
                        render_pass.set_bind_group(
                            CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                            &camera.bind_group,
                            &[],
                        );
                    }
                    if let Some(bind_group) = &pass.parameters_bind_group {
                        render_pass.set_bind_group(
                            MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX,
                            bind_group,
                            &[],
                        );
                    }
                    if let Some(bind_group) = &pass.textures_bind_group {
                        render_pass.set_bind_group(
                            MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX,
                            bind_group,
                            &[],
                        );
                    }
                    render_pass.draw(0..3, 0..1);
                }
            }
        }
        // The last creation-class failure a frame can hit: wgpu validates a
        // command buffer when it is submitted, and a validation failure would
        // otherwise reach the uncaptured-error handler as a panic.
        capturing_validation(&self.device, || {
            self.queue.submit(std::iter::once(encoder.finish()));
        })
        .map_err(|detail| RendererError::Other {
            detail: format!("frame submission: {detail}"),
        })?;
        surface_frame.present();
        Ok(())
    }
}

/// The colour a pass opens its target with, or the values already in it.
fn color_load(clear: bool) -> wgpu::LoadOp<wgpu::Color> {
    if clear {
        wgpu::LoadOp::Clear(CLEAR_COLOR)
    } else {
        wgpu::LoadOp::Load
    }
}
