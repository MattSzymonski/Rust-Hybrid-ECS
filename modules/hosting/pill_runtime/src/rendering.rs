//! A renderer attached to a window, and the windowed runtime of a shipping
//! build.
//!
//! # Responsibilities
//!
//! - Keep a renderer backend and the window it draws on together, dropping
//!   the backend first ([`AttachedRenderer`]); resize, viewport, re-targeting
//!   and drawing the frame the `rendering` system filled in.
//! - Run a statically linked project in a window ([`RenderingRuntime`]):
//!   register the linked renderer, attach it, and draw every frame.
//!
//! # Design
//!
//! The development host keeps its own windowed host, because its renderer is
//! a reloadable module, but it holds the same [`AttachedRenderer`]: only where
//! a backend comes from differs between the two, never how it is kept and
//! drawn with.

// Standard library
use std::collections::BTreeMap;
use std::future::Future;

// External crates
use pill_core::info;
use pill_core::telemetry::telemetry_target;
use pill_engine::{Engine, InputEvent, RumbleRequest, World};
use pill_renderer_api::{
    AttachFuture, CameraBinding, PillRenderer, RawWindowData, RenderFrame, RenderViewport,
    RendererError, SurfaceId, SurfaceOptions,
};

// Current crate
use crate::render_window::{attach_window, attach_window_async, AttachedWindow, RendererWindow};
use crate::{FrameDriver, FrameReport, RenderingError, Runtime, StaticProject};

// =============================================================================
// AttachedRenderer
// =============================================================================

/// A renderer backend and the window it draws on.
///
/// The field order is the drop order: the backend's surface is built on the
/// window's raw handles and must not outlive the window.
pub struct AttachedRenderer {
    /// The backend drawing on `window`; declared first so it drops first.
    renderer: Box<dyn PillRenderer>,
    /// The frontend window the backend draws on, kept alive for it.
    window: AttachedWindow,
    /// The window's handles as data, kept to attach another backend to the
    /// same window.
    window_data: RawWindowData,
    /// The surface size last handed to the backend, which a replacement
    /// starts from.
    surface_size: (u32, u32),
    /// The region drawn to, or `None` for the whole window.
    viewport: Option<RenderViewport>,
    /// Whether a frame with draw calls has been presented, for the one-time
    /// first-frame line.
    presented_scene: bool,
    /// Every surface attached after the primary window, by surface id: the
    /// window kept alive for it, and what re-attaching it to a new backend
    /// takes. Declared after `renderer`, so the backend drops first.
    extra_surfaces: BTreeMap<u32, ExtraSurface>,
}

/// One surface attached after the primary window.
struct ExtraSurface {
    /// The frontend window the surface draws on, held only to keep it alive
    /// while the backend draws on it; never read.
    _window: AttachedWindow,
    /// The window's handles as data, for re-attaching to another backend.
    window_data: RawWindowData,
    /// The surface size last handed to the backend.
    size: (u32, u32),
    /// The region drawn to, or `None` for the whole surface.
    viewport: Option<RenderViewport>,
    /// How the surface presents.
    options: SurfaceOptions,
    /// Whether the surface may present (the frontend's pacing).
    ready: bool,
}

impl AttachedRenderer {
    /// Attach a backend built by `attach` to `window`.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the window cannot give its handles or
    /// `attach` fails.
    pub fn attach<W: RendererWindow>(
        window: W,
        width: u32,
        height: u32,
        attach: impl FnOnce(RawWindowData) -> Result<Box<dyn PillRenderer>, RendererError>,
    ) -> Result<Self, RendererError> {
        let (renderer, window, window_data) = attach_window(window, attach)?;
        Ok(Self {
            renderer,
            window,
            window_data,
            surface_size: (width, height),
            viewport: None,
            presented_scene: false,
            extra_surfaces: BTreeMap::new(),
        })
    }

    /// [`Self::attach`] with a backend built asynchronously by the future
    /// `attach` returns.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the window cannot give its handles or
    /// the future fails.
    pub async fn attach_async<W, F>(
        window: W,
        width: u32,
        height: u32,
        attach: impl FnOnce(RawWindowData) -> F,
    ) -> Result<Self, RendererError>
    where
        W: RendererWindow,
        F: Future<Output = Result<Box<dyn PillRenderer>, RendererError>>,
    {
        let (renderer, window, window_data) = attach_window_async(window, attach).await?;
        Ok(Self {
            renderer,
            window,
            window_data,
            surface_size: (width, height),
            viewport: None,
            presented_scene: false,
            extra_surfaces: BTreeMap::new(),
        })
    }

    /// Move rendering to a new window, with a backend built by `attach`.
    ///
    /// The replacement is built before the current backend is dropped, so a
    /// failure leaves the current window drawn.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the new window cannot give its handles
    /// or `attach` fails.
    pub fn retarget<W: RendererWindow>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
        attach: impl FnOnce(RawWindowData) -> Result<Box<dyn PillRenderer>, RendererError>,
    ) -> Result<(), RendererError> {
        let parts = attach_window(window, attach)?;
        self.install(parts, width, height);
        Ok(())
    }

    /// [`Self::retarget`] with a backend built asynchronously by the future
    /// `attach` returns.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the new window cannot give its handles
    /// or the future fails.
    pub async fn retarget_async<W, F>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
        attach: impl FnOnce(RawWindowData) -> F,
    ) -> Result<(), RendererError>
    where
        W: RendererWindow,
        F: Future<Output = Result<Box<dyn PillRenderer>, RendererError>>,
    {
        let parts = attach_window_async(window, attach).await?;
        self.install(parts, width, height);
        Ok(())
    }

    /// Take over a backend and the window it was built on, replacing both.
    fn install(
        &mut self,
        (renderer, window, window_data): (Box<dyn PillRenderer>, AttachedWindow, RawWindowData),
        width: u32,
        height: u32,
    ) {
        // Renderer first, then window: the old renderer drops while its window
        // is still alive, and only then is that window released.
        self.renderer = renderer;
        self.window = window;
        self.window_data = window_data;
        self.surface_size = (width, height);
        self.renderer.set_viewport(self.viewport);
        self.reattach_extra_surfaces();
    }

    /// Replace the backend on the same window, keeping the viewport and every
    /// extra surface.
    pub fn replace_backend(&mut self, renderer: Box<dyn PillRenderer>) {
        self.renderer = renderer;
        self.renderer.set_viewport(self.viewport);
        self.reattach_extra_surfaces();
    }

    /// Attach every extra surface to the current backend again, with its size,
    /// viewport and pacing - after the backend was replaced (a renderer reload)
    /// or retargeted.
    ///
    /// A surface the new backend refuses is kept and reported: its window is
    /// still open, and the next replacement tries again.
    fn reattach_extra_surfaces(&mut self) {
        for (id, surface) in &self.extra_surfaces {
            // SAFETY: the surface's window is kept alive in `extra_surfaces`
            // until the surface is detached or this renderer is dropped, and
            // the backend - declared first - drops before it either way.
            let attached = unsafe {
                self.renderer.attach_surface(
                    SurfaceId(*id),
                    surface.window_data,
                    surface.size.0,
                    surface.size.1,
                    surface.options,
                )
            };
            match attached {
                Ok(()) => {
                    self.renderer
                        .set_surface_viewport(SurfaceId(*id), surface.viewport);
                    self.renderer
                        .set_surface_ready(SurfaceId(*id), surface.ready);
                    info!(
                        target: telemetry_target::RENDERING,
                        "surface {id} reattached to the new renderer"
                    );
                }
                Err(error) => pill_core::warn!(
                    target: telemetry_target::RENDERING,
                    "surface {id} could not be reattached to the new renderer: {error}"
                ),
            }
        }
    }

    /// Draw into another window as surface `id`, sized `width` by `height`
    /// physical pixels; the window is kept alive until [`Self::detach_surface`].
    ///
    /// Which camera draws into it comes from the world's
    /// [`pill_renderer_api::SurfaceCameras`]; a surface with no binding stays
    /// empty.
    ///
    /// A backend that draws one surface only - the placeholder a development
    /// host holds while its renderer module builds or reloads - leaves the
    /// surface pending rather than refused: it is kept, and attached to the
    /// next backend that replaces this one.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the window cannot give its handles or
    /// the backend refuses the surface for another reason.
    pub fn attach_surface<W: RendererWindow>(
        &mut self,
        id: SurfaceId,
        window: W,
        width: u32,
        height: u32,
        options: SurfaceOptions,
    ) -> Result<(), RendererError> {
        let window_data = RawWindowData::from_window(&window)?;
        // SAFETY: the window is stored in `extra_surfaces` below and kept until
        // the surface is detached or this renderer is dropped; the backend,
        // declared first, drops before it.
        let attached = unsafe {
            self.renderer
                .attach_surface(id, window_data, width, height, options)
        };
        match attached {
            Ok(()) => {}
            Err(RendererError::UnsupportedSurface { detail }) => info!(
                target: telemetry_target::RENDERING,
                "surface {} waits for a renderer that draws more surfaces ({detail})", id.0
            ),
            Err(error) => return Err(error),
        }
        self.extra_surfaces.insert(
            id.0,
            ExtraSurface {
                _window: Box::new(window),
                window_data,
                size: (width, height),
                viewport: None,
                options,
                ready: true,
            },
        );
        Ok(())
    }

    /// Stop drawing into surface `id`, then release its window.
    pub fn detach_surface(&mut self, id: SurfaceId) {
        // The backend's surface first, then the window it was built on.
        self.renderer.detach_surface(id);
        self.extra_surfaces.remove(&id.0);
    }

    /// Forward a physical resize of surface `id`.
    pub fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32) {
        if let Some(surface) = self.extra_surfaces.get_mut(&id.0) {
            surface.size = (width, height);
            self.renderer.resize_surface(id, width, height);
            self.renderer.set_surface_viewport(id, surface.viewport);
        }
    }

    /// Restrict drawing to a region of surface `id`; `None` draws all of it.
    pub fn set_surface_viewport(&mut self, id: SurfaceId, viewport: Option<RenderViewport>) {
        if let Some(surface) = self.extra_surfaces.get_mut(&id.0) {
            surface.viewport = viewport;
            self.renderer.set_surface_viewport(id, viewport);
        }
    }

    /// Whether surface `id` may present on the next frame; see
    /// [`PillRenderer::set_surface_ready`]. Applies to the primary surface too.
    pub fn set_surface_ready(&mut self, id: SurfaceId, ready: bool) {
        if let Some(surface) = self.extra_surfaces.get_mut(&id.0) {
            surface.ready = ready;
        }
        self.renderer.set_surface_ready(id, ready);
    }

    /// The ids of the surfaces attached after the primary window.
    pub fn extra_surface_ids(&self) -> Vec<SurfaceId> {
        self.extra_surfaces
            .keys()
            .map(|id| SurfaceId(*id))
            .collect()
    }

    /// Drop the backend and draw nothing until another one replaces it.
    ///
    /// For a host whose backend lives in a module about to be unloaded: the
    /// backend must go while that module is still mapped.
    pub fn detach_backend(&mut self) {
        self.renderer = Box::new(pill_renderer_api::HeadlessRenderer);
    }

    /// The current backend.
    pub fn renderer(&self) -> &dyn PillRenderer {
        self.renderer.as_ref()
    }

    /// The window's handles as data.
    pub fn window_data(&self) -> RawWindowData {
        self.window_data
    }

    /// The surface size last handed to the backend.
    pub fn surface_size(&self) -> (u32, u32) {
        self.surface_size
    }

    /// The region drawn to, or `None` for the whole window.
    pub fn viewport(&self) -> Option<RenderViewport> {
        self.viewport
    }

    /// Forward a physical window resize to the backend.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.surface_size = (width, height);
        self.renderer.resize(width, height);
        self.renderer.set_viewport(self.viewport);
    }

    /// Restrict drawing to a physical region of the window; `None` draws the
    /// whole window.
    pub fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
        self.renderer.set_viewport(viewport);
    }

    /// Draw the frame the `rendering` system left in `world`, when it did.
    ///
    /// The renderer reads both resources straight out of the world - the
    /// frame, and the store the assets live in - rather than being handed a
    /// copy of the assets. Both borrows last the call, which is what stops the
    /// store changing under a frame that is already being drawn.
    ///
    /// # Errors
    ///
    /// Returns what the backend's `render` returns.
    pub fn render(&mut self, world: &World) -> Result<(), RendererError> {
        let (Some(frame), Some(assets)) = (
            world.get_resource::<RenderFrame>(),
            world.get_resource::<pill_engine::AssetManager>(),
        ) else {
            return Ok(());
        };
        let outcome = self.renderer.render(frame, assets)?;
        if !self.presented_scene
            && matches!(outcome, pill_renderer_api::FrameOutcome::Presented)
            && self.renderer.metrics().draw_calls > 0
        {
            self.presented_scene = true;
            // Through the log rather than stdout: a browser has no stdout, and
            // the terminal lane prints the same line on native.
            info!(
                target: telemetry_target::RENDERING,
                "[render] First frame: {outcome:?}; camera={}; instances={}; {:?}",
                frame.has_camera,
                frame.instances.len(),
                self.renderer.metrics()
            );
        }
        Ok(())
    }

    /// Move the picks the renderer has answered since the last call into the
    /// world's [`pill_renderer_api::PickResults`].
    ///
    /// Answers arrive frames after their request, whenever the GPU finishes;
    /// a world without the resource drops them.
    pub fn deliver_pick_results(&mut self, world: &mut World) {
        let results = self.renderer.take_pick_results();
        if results.is_empty() {
            return;
        }
        if let Some(delivered) = world.get_resource_mut::<pill_renderer_api::PickResults>() {
            delivered.results.extend(results);
        }
    }

    /// Move the previews the renderer has read back since the last call into
    /// the world's [`pill_renderer_api::PreviewResults`].
    ///
    /// Like a pick, a preview is answered frames after it was asked; a world
    /// without the resource drops it.
    pub fn deliver_preview_results(&mut self, world: &mut World) {
        let results = self.renderer.take_preview_results();
        if results.is_empty() {
            return;
        }
        if let Some(delivered) = world.get_resource_mut::<pill_renderer_api::PreviewResults>() {
            delivered.results.extend(results);
        }
    }
}

// =============================================================================
// RenderingRuntime
// =============================================================================

/// A statically linked project running in a window.
///
/// The field order is the drop order: the engine first, as in every host,
/// then the backend, then the window it drew on.
pub struct RenderingRuntime {
    /// The engine and its frame state.
    runtime: Runtime,
    /// The linked renderer's backend on the window.
    display: AttachedRenderer,
}

impl RenderingRuntime {
    /// Move rendering to a newly created window; see
    /// [`AttachedRenderer::retarget_async`].
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the new window cannot be attached.
    pub async fn retarget_render_window<W: RendererWindow>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<(), RendererError> {
        let linked = self.runtime.static_renderer();
        self.display
            .retarget_async(window, width, height, |window_data| {
                attach_linked(linked, window_data, width, height)
            })
            .await
    }

    /// Forward a physical window resize to the renderer.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.display.resize(width, height);
    }

    /// Restrict drawing to a physical region of the window; `None` draws the
    /// whole window.
    pub fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.display.set_viewport(viewport);
    }

    /// Run one frame and draw its world.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the renderer fails.
    pub fn run_one_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        let report = self.runtime.run_frame();
        self.display.render(self.runtime.engine().world())?;
        self.display
            .deliver_pick_results(self.runtime.engine_mut().world_mut());
        self.display
            .deliver_preview_results(self.runtime.engine_mut().world_mut());
        Ok(report)
    }

    /// Live frame statistics; see [`Runtime::current_frame_report`].
    pub fn current_frame_report(&self) -> FrameReport {
        self.runtime.current_frame_report()
    }

    /// Read-only engine access.
    pub fn engine(&self) -> &Engine {
        self.runtime.engine()
    }

    /// Mutable engine access for frame-boundary work.
    pub fn engine_mut(&mut self) -> &mut Engine {
        self.runtime.engine_mut()
    }
}

impl FrameDriver for RenderingRuntime {
    type Error = RendererError;

    fn run_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        self.run_one_frame()
    }

    fn resize(&mut self, width: u32, height: u32) {
        RenderingRuntime::resize(self, width, height);
    }

    fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        RenderingRuntime::set_render_viewport(self, viewport);
    }

    fn push_input(&mut self, event: InputEvent) {
        self.engine_mut().push_input_event(event);
    }

    fn take_rumble_requests(&mut self) -> Vec<RumbleRequest> {
        self.engine_mut().take_rumble_requests()
    }

    fn attach_surface<W: RendererWindow>(
        &mut self,
        id: SurfaceId,
        window: W,
        width: u32,
        height: u32,
        options: SurfaceOptions,
    ) -> Result<(), RendererError> {
        self.display
            .attach_surface(id, window, width, height, options)
    }

    fn detach_surface(&mut self, id: SurfaceId) {
        self.display.detach_surface(id);
    }

    fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32) {
        self.display.resize_surface(id, width, height);
    }

    fn set_surface_viewport(&mut self, id: SurfaceId, viewport: Option<RenderViewport>) {
        self.display.set_surface_viewport(id, viewport);
    }

    fn set_surface_ready(&mut self, id: SurfaceId, ready: bool) {
        self.display.set_surface_ready(id, ready);
    }

    fn set_surface_camera(&mut self, id: SurfaceId, binding: Option<CameraBinding>) {
        crate::runtime::set_surface_camera(self.engine_mut().world_mut(), id, binding);
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Register the linked renderer and attach it to `window`.
///
/// For a frontend that finishes project setup before any window exists, so a
/// slow start never shows a blank surface. Asynchronous because building the
/// renderer is: a native frontend blocks on it once, a web frontend awaits it.
///
/// # Errors
///
/// Returns a [`RenderingError`]: the renderer's registration failed, or it
/// could not attach to the window - including when this build links none.
pub async fn attach_renderer<W: RendererWindow>(
    mut runtime: Runtime,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingRuntime, RenderingError> {
    info!(
        target: telemetry_target::RENDERING,
        width,
        height,
        "attaching the engine renderer to the window surface"
    );
    // Register its system the way its module entry point would, then attach
    // through the bundle's function pointer. A build without one reports that
    // from `attach_linked`.
    let linked = runtime.static_renderer();
    if let Some((renderer, owner)) = linked {
        crate::static_project::initialize_static_renderer(runtime.engine_mut(), renderer, owner)
            .map_err(|status| RendererError::Other {
                detail: format!("the renderer failed to initialize with status {status}"),
            })?;
    }
    let display = AttachedRenderer::attach_async(window, width, height, |window_data| {
        attach_linked(linked, window_data, width, height)
    })
    .await?;
    Ok(RenderingRuntime { runtime, display })
}

/// Start a statically linked project and attach its renderer to `window`.
///
/// # Errors
///
/// Returns a [`RenderingError`] carrying the setup failure or the renderer's.
pub async fn setup_rendering<W: RendererWindow>(
    project: StaticProject,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingRuntime, RenderingError> {
    let runtime = crate::setup(project)?;
    attach_renderer(runtime, window, width, height).await
}

/// Start building a backend on `window_data` from the linked renderer.
///
/// The returned future fails with a [`RendererError`] when the renderer cannot
/// attach, or when this build links none.
fn attach_linked(
    linked: Option<(crate::StaticRenderer, pill_engine::SystemOwner)>,
    window_data: RawWindowData,
    width: u32,
    height: u32,
) -> AttachFuture {
    let Some((renderer, _)) = linked else {
        return Box::pin(std::future::ready(Err(RendererError::Other {
            detail: "this shipping build links no renderer; build it with `--features rendering`"
                .to_owned(),
        })));
    };
    // SAFETY: every caller stores the backend in an `AttachedRenderer`, whose
    // `renderer` field is declared before `window`: the backend drops while
    // the window it was built on is alive.
    unsafe { (renderer.attach)(window_data, width, height) }
}
