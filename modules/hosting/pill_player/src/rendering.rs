//! A renderer attached to a window, and the player that draws a statically
//! linked project in one.
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
use pill_engine::{Engine, World};
use pill_renderer_api::{
    AttachFuture, PillRenderer, RawWindowData, RenderFrame, RenderViewport, RendererError,
    SurfaceId, SurfaceOptions,
};

// Current crate
use crate::render_window::{attach_window, attach_window_async, AttachedWindow, RendererWindow};
use crate::{FrameDriver, RenderingError, Runtime, StaticProject, WindowedFrameDriver};

// =============================================================================
// AttachedRenderer
// =============================================================================

/// A renderer backend and the window it draws on.
///
/// The field order is the drop order: the backend's surface is built on the
/// window's raw handles and must not outlive the window.
pub struct AttachedRenderer {
    /// The backend drawing on the primary window; declared first so it drops
    /// first.
    renderer: Box<dyn PillRenderer>,
    /// The primary window: the same record every extra surface keeps, so each
    /// per-surface operation is one lookup and a backend swap replays one
    /// shape. Declared after `renderer`, so the backend drops before the
    /// window it draws on.
    primary: SurfaceRecord,
    /// Whether a frame with draw calls has been presented, for the one-time
    /// first-frame line.
    presented_scene: bool,
    /// The draw count last reported in the log, so a frame that changes how
    /// much it draws is reported once rather than every frame.
    reported_passes_with_draws: u32,
    /// Whether the backend was dropped with [`Self::detach_backend`] and none
    /// has replaced it yet. [`Self::replace_backend`] asserts it, because
    /// assigning over a live backend drops it while its module may be gone.
    detached: bool,
    /// Every surface attached after the primary window, by surface id.
    /// Declared after `renderer`, so the backend drops first.
    extra_surfaces: BTreeMap<u32, SurfaceRecord>,
}

// =============================================================================
// SurfaceRecord
// =============================================================================

/// One surface: the window it draws on, and everything re-attaching it to
/// another backend takes.
///
/// The primary window and every surface attached later are kept the same way,
/// so a resize, a viewport or a pacing change is one lookup rather than a
/// branch, and a backend swap replays the same facts for both.
struct SurfaceRecord {
    /// The frontend window, held only to keep it alive while the backend draws
    /// on it.
    #[allow(dead_code)]
    window: AttachedWindow,
    /// The window's handles as data, for re-attaching to another backend.
    window_data: RawWindowData,
    /// The surface size last handed to the backend.
    size: (u32, u32),
    /// The region drawn to, or `None` for the whole surface.
    viewport: Option<RenderViewport>,
    /// How the surface presents; `None` for the primary, which takes the
    /// backend's default.
    options: Option<SurfaceOptions>,
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
        Ok(Self::from_parts(
            renderer,
            window,
            window_data,
            width,
            height,
        ))
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
        Ok(Self::from_parts(
            renderer,
            window,
            window_data,
            width,
            height,
        ))
    }

    /// A renderer over a backend and the parts of the window it was built on.
    ///
    /// The one place the struct is built, so the synchronous and asynchronous
    /// attach paths cannot drift apart.
    fn from_parts(
        renderer: Box<dyn PillRenderer>,
        window: AttachedWindow,
        window_data: RawWindowData,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            renderer,
            primary: SurfaceRecord {
                window,
                window_data,
                size: (width, height),
                viewport: None,
                options: None,
                ready: true,
            },
            presented_scene: false,
            reported_passes_with_draws: 0,
            detached: false,
            extra_surfaces: BTreeMap::new(),
        }
    }

    /// The record for surface `id`: the primary window, or one attached later.
    fn surface_mut(&mut self, id: SurfaceId) -> Option<&mut SurfaceRecord> {
        if id == SurfaceId::PRIMARY {
            Some(&mut self.primary)
        } else {
            self.extra_surfaces.get_mut(&id.0)
        }
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
        // Step 1: Swap in the new backend and window. Renderer first, then
        // window: the old renderer drops while its window is still alive, and
        // only then is that window released. What the old window was told - the
        // viewport and the pacing - survives the swap, because they belong to
        // the surface, not to the backend.
        let viewport = self.primary.viewport;
        let ready = self.primary.ready;
        self.renderer = renderer;
        self.detached = false;
        self.primary = SurfaceRecord {
            window,
            window_data,
            size: (width, height),
            viewport,
            options: None,
            ready,
        };
        // Step 2: Tell the new backend what the old one was told.
        self.replay_surfaces();
    }

    /// Replace the backend on the same window, keeping the viewport, the pacing
    /// and every extra surface.
    ///
    /// # Precondition
    ///
    /// The current backend is detached ([`Self::detach_backend`]). Assigning
    /// over a live backend drops it, and a backend that lives in a module the
    /// host is about to unload must be gone before that unload - which is why
    /// every caller detaches first and this asserts it in debug builds.
    pub fn replace_backend(&mut self, renderer: Box<dyn PillRenderer>) {
        debug_assert!(
            self.detached,
            "replace_backend over a live backend drops it while its module may be gone; call detach_backend first"
        );
        self.renderer = renderer;
        self.detached = false;
        self.replay_surfaces();
    }

    /// Tell a new backend what the one it replaced was told: the primary's
    /// viewport and pacing, then every extra surface.
    ///
    /// The pacing is what a replacement would otherwise forget - a window the
    /// frontend asked to hold would start presenting again on the first frame
    /// after a renderer reload - and it is recorded for the primary now for the
    /// same reason the extra surfaces' always was.
    fn replay_surfaces(&mut self) {
        let viewport = self.primary.viewport;
        let ready = self.primary.ready;
        self.renderer
            .set_surface_viewport(SurfaceId::PRIMARY, viewport);
        self.renderer.set_surface_ready(SurfaceId::PRIMARY, ready);
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
            let options = surface.options.unwrap_or_default();
            // SAFETY: the surface's window is kept alive in `extra_surfaces`
            // until the surface is detached or this renderer is dropped, and
            // the backend - declared first - drops before it either way.
            let attached = unsafe {
                self.renderer.attach_surface(
                    SurfaceId(*id),
                    surface.window_data,
                    surface.size.0,
                    surface.size.1,
                    options,
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
        // Step 1: Take the window's handles as data; the surface is built on
        // them.
        let window_data = RawWindowData::from_window(&window)?;
        // SAFETY: the window is stored in `extra_surfaces` below and kept until
        // the surface is detached or this renderer is dropped; the backend,
        // declared first, drops before it.
        let attached = unsafe {
            self.renderer
                .attach_surface(id, window_data, width, height, options)
        };
        // Step 2: A backend that draws one surface leaves the surface pending;
        // anything else it refuses is an error.
        match attached {
            Ok(()) => {}
            Err(RendererError::UnsupportedSurface { detail }) => info!(
                target: telemetry_target::RENDERING,
                "surface {} waits for a renderer that draws more surfaces ({detail})", id.0
            ),
            Err(error) => return Err(error),
        }
        // Step 3: Keep the window, and what re-attaching the surface needs.
        self.extra_surfaces.insert(
            id.0,
            SurfaceRecord {
                window: Box::new(window),
                window_data,
                size: (width, height),
                viewport: None,
                options: Some(options),
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
    ///
    /// One method serves every surface: the size is recorded in the surface's
    /// own record, so a caller never has to know which form reaches which.
    pub fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32) {
        let Some(record) = self.surface_mut(id) else {
            return;
        };
        record.size = (width, height);
        let viewport = record.viewport;
        self.renderer.resize_surface(id, width, height);
        self.renderer.set_surface_viewport(id, viewport);
    }

    /// Restrict drawing to a region of surface `id`; `None` draws all of it.
    pub fn set_surface_viewport(&mut self, id: SurfaceId, viewport: Option<RenderViewport>) {
        let Some(record) = self.surface_mut(id) else {
            return;
        };
        record.viewport = viewport;
        self.renderer.set_surface_viewport(id, viewport);
    }

    /// Whether surface `id` may present on the next frame; see
    /// [`PillRenderer::set_surface_ready`]. Applies to the primary surface too,
    /// which records it so a backend swap cannot drop it.
    pub fn set_surface_ready(&mut self, id: SurfaceId, ready: bool) {
        if let Some(record) = self.surface_mut(id) {
            record.ready = ready;
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
        self.detached = true;
    }

    /// The current backend; [`Self::detach_backend`] leaves the headless one.
    pub fn renderer(&self) -> &dyn PillRenderer {
        self.renderer.as_ref()
    }

    /// The window's handles as data.
    pub fn window_data(&self) -> RawWindowData {
        self.primary.window_data
    }

    /// The surface size last handed to the backend.
    pub fn surface_size(&self) -> (u32, u32) {
        self.primary.size
    }

    /// The region drawn to, or `None` for the whole window.
    pub fn viewport(&self) -> Option<RenderViewport> {
        self.primary.viewport
    }

    /// Draw the frame the `rendering` system left in `world`, when it did.
    ///
    /// The renderer reads both resources straight out of the world - the
    /// frame, and the store the assets live in - rather than being handed a
    /// copy of the assets. Both borrows last the call, which is what stops the
    /// store changing under a frame that is already being drawn.
    ///
    /// A world holding no frame, or no asset store, draws nothing: that is not
    /// an error.
    ///
    /// # Errors
    ///
    /// Returns what the backend's `render` returns.
    pub fn render(&mut self, world: &World) -> Result<(), RendererError> {
        // Step 1: Take the frame and the store it was built from, if this
        // world has both.
        let (Some(frame), Some(assets)) = (
            world.get_resource::<RenderFrame>(),
            world.get_resource::<pill_engine::AssetManager>(),
        ) else {
            return Ok(());
        };
        let outcome = self.renderer.render(frame, assets)?;
        let metrics = self.renderer.metrics();
        // Step 2: Report a frame whose drawn-pass count moved. The line is how
        // a chain that stopped drawing a pass - a material no pass names, a
        // shader that failed to load - is visible in a run, and how a test
        // reads what a settled frame actually drew without a person watching
        // the window.
        if metrics.passes_with_draws != self.reported_passes_with_draws {
            self.reported_passes_with_draws = metrics.passes_with_draws;
            info!(
                target: telemetry_target::RENDERING,
                passes_with_draws = metrics.passes_with_draws,
                passes = metrics.passes,
                instances = frame.instances.len(),
                "[render] frame draws"
            );
        }
        // Step 3: Report the first frame that presented with draw calls. A
        // window opened before any camera is enabled presents empty frames,
        // and none of them is the start of the scene.
        if !self.presented_scene
            && matches!(outcome, pill_renderer_api::FrameOutcome::Presented)
            && self.renderer.metrics().passes_with_draws > 0
        {
            self.presented_scene = true;
            // Through the log rather than stdout: a browser has no stdout, and
            // the terminal lane prints the same line on native.
            info!(
                target: telemetry_target::RENDERING,
                "[render] First frame: {outcome:?}; camera={}; instances={}; {:?}",
                frame
                    .surface_views
                    .contains_key(&pill_renderer_api::SurfaceId::PRIMARY),
                frame.instances.len(),
                self.renderer.metrics()
            );
        }
        Ok(())
    }

    /// Draw the frame the `rendering` system left in `world`, then hand back
    /// the picks and previews the GPU finished since the last frame.
    ///
    /// The order is the renderer's contract: an answer to a request made this
    /// frame cannot exist before the draw that produces it, so a frontend that
    /// delivered first would read the previous frame's results again. Both
    /// windowed drivers call this one method, which is why neither of them can
    /// get the order wrong.
    ///
    /// # Errors
    ///
    /// Returns what [`Self::render`] returns.
    pub fn present(&mut self, world: &mut World) -> Result<(), RendererError> {
        self.render(world)?;
        self.deliver_pick_results(world);
        self.deliver_preview_results(world);
        Ok(())
    }

    /// Move the picks the renderer has answered since the last call into the
    /// world's [`pill_renderer_api::PickResults`].
    ///
    /// Answers arrive frames after their request, whenever the GPU finishes;
    /// a world without the resource drops them. Reachable through
    /// [`Self::present`], which keeps the delivery after the draw.
    fn deliver_pick_results(&mut self, world: &mut World) {
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
    fn deliver_preview_results(&mut self, world: &mut World) {
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
    /// Returns a [`RendererError`] when the new window cannot be attached,
    /// including when this build links no renderer to move.
    pub async fn retarget_render_window<W: RendererWindow>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<(), RendererError> {
        let Some((renderer, _)) = self.runtime.static_renderer() else {
            return Err(RendererError::Other {
                detail: NO_LINKED_RENDERER.to_owned(),
            });
        };
        self.display
            .retarget_async(window, width, height, |window_data| {
                attach_linked(renderer, window_data, width, height)
            })
            .await
    }

    /// Execute one ECS frame and present its resulting world to the surface.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the renderer fails.
    pub fn run_frame(&mut self) -> Result<(), RendererError> {
        // Step 1: Run the engine's frame, which fills the frame resource.
        self.runtime.run_frame();
        // Step 2: Draw what it left and hand back the answers the GPU finished.
        self.display
            .present(self.runtime.engine_mut().world_mut())?;
        Ok(())
    }

    /// Live frame rate; see [`Runtime::frames_per_second`].
    pub fn frames_per_second(&self) -> f64 {
        self.runtime.frames_per_second()
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

    fn run_frame(&mut self) -> Result<(), RendererError> {
        RenderingRuntime::run_frame(self)
    }

    fn engine_mut(&mut self) -> &mut Engine {
        RenderingRuntime::engine_mut(self)
    }
}

impl WindowedFrameDriver for RenderingRuntime {
    fn display_mut(&mut self) -> &mut AttachedRenderer {
        &mut self.display
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Why a windowed build can end up with no renderer to attach.
///
/// Both ways to reach it are build decisions rather than runtime failures, so the
/// message names them instead of the feature that is already on wherever it can
/// appear: a project whose settings select no `renderer:`, and a shipping bundle
/// generated without the rendering feature.
const NO_LINKED_RENDERER: &str = "this build links no renderer: the project selects no `renderer:`, or its shipping bundle was generated without the rendering feature";

/// Register the linked renderer and attach it to `window`.
///
/// For a frontend that finishes project setup before any window exists, so a
/// slow start never shows a blank surface. Asynchronous because building the
/// renderer is: a native frontend blocks on it once, a web frontend awaits it.
///
/// # Errors
///
/// Returns a [`RenderingError`]: the renderer's registration failed, or the
/// backend could not attach to the window. A build that links no renderer
/// returns the error [`Self`]'s caller reports, naming the two ways a windowed
/// build ends up without one.
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
    // Step 1: A build with no linked renderer stops here, before any window is
    // bound: registering nothing and failing the attach later would report the
    // same fact one step further in, as a renderer error.
    let Some((renderer, owner)) = runtime.static_renderer() else {
        return Err(RenderingError::Renderer(RendererError::Other {
            detail: NO_LINKED_RENDERER.to_owned(),
        }));
    };
    // Step 2: Register its system the way its module entry point would.
    crate::static_project::initialize_linked(
        runtime.engine_mut(),
        renderer.init,
        Some(owner),
        None,
    )
    .map_err(|status| RendererError::Other {
        detail: format!("the renderer failed to initialize with status {status}"),
    })?;
    // Step 3: Build it on the window through the bundle's function pointer.
    let display = AttachedRenderer::attach_async(window, width, height, |window_data| {
        attach_linked(renderer, window_data, width, height)
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
/// # Errors
///
/// The returned future fails when the renderer cannot attach.
fn attach_linked(
    renderer: crate::StaticRenderer,
    window_data: RawWindowData,
    width: u32,
    height: u32,
) -> AttachFuture {
    // SAFETY: every caller stores the backend in an `AttachedRenderer`, whose
    // `renderer` field is declared before `window`: the backend drops while
    // the window it was built on is alive.
    unsafe { (renderer.attach)(window_data, width, height) }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use pill_renderer_api::{FrameOutcome, RawWindowKind};

    use super::*;

    /// What a backend was told, as one line per call.
    type Calls = Arc<Mutex<Vec<String>>>;

    /// A backend that records what it is told, so a test can see what a
    /// replacement replays.
    struct RecordingRenderer {
        calls: Calls,
    }

    impl RecordingRenderer {
        /// A backend recording into `calls`.
        fn new(calls: Calls) -> Self {
            Self { calls }
        }

        /// Append one line to the record.
        fn record(&self, line: String) {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(line);
        }
    }

    impl PillRenderer for RecordingRenderer {
        fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32) {
            self.record(format!("resize {} {width}x{height}", id.0));
        }

        fn set_surface_viewport(&mut self, id: SurfaceId, viewport: Option<RenderViewport>) {
            self.record(format!("viewport {} {viewport:?}", id.0));
        }

        fn set_surface_ready(&mut self, id: SurfaceId, ready: bool) {
            self.record(format!("ready {} {ready}", id.0));
        }

        fn render(
            &mut self,
            _frame: &RenderFrame,
            _assets: &pill_engine::AssetManager,
        ) -> Result<FrameOutcome, RendererError> {
            Ok(FrameOutcome::Skipped)
        }

        fn invalidate_assets(&mut self) {}
    }

    /// A renderer over `renderer` and a synthetic primary window, for the tests
    /// that check what a replacement replays.
    fn attached_over(renderer: Box<dyn PillRenderer>) -> AttachedRenderer {
        AttachedRenderer::from_parts(
            renderer,
            Box::new(()),
            RawWindowData {
                kind: RawWindowKind::Win32,
                window: 0,
                window_aux: 0,
                display: 0,
                screen: 0,
            },
            800,
            600,
        )
    }

    /// A backend replacement replays the primary surface's pacing.
    ///
    /// A window the frontend asked to hold would otherwise present again on the
    /// first frame after a renderer reload - the primary's pacing used to be
    /// the one fact the extra surfaces kept and it did not.
    #[test]
    fn a_replacement_replays_the_primary_surfaces_pacing() {
        let first = Arc::new(Mutex::new(Vec::new()));
        let mut attached = attached_over(Box::new(RecordingRenderer::new(first.clone())));

        // The frontend holds the primary, and sets a viewport while it does.
        attached.set_surface_ready(SurfaceId::PRIMARY, false);
        attached.set_surface_viewport(SurfaceId::PRIMARY, None);
        let held = first
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            held.iter().any(|line| line == "ready 0 false"),
            "the first backend was told to hold: {held:?}"
        );

        // A renderer reload replaces the backend.
        attached.detach_backend();
        let second = Arc::new(Mutex::new(Vec::new()));
        attached.replace_backend(Box::new(RecordingRenderer::new(second.clone())));
        let replayed = second
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            replayed.iter().any(|line| line == "ready 0 false"),
            "the replacement was told to hold the primary: {replayed:?}"
        );
        assert!(
            replayed.iter().any(|line| line.starts_with("viewport 0")),
            "the replacement was told the viewport: {replayed:?}"
        );
    }

    /// A resize reaches the surface's record and the backend, for the primary
    /// and for a surface attached later alike.
    #[test]
    fn a_resize_is_recorded_for_every_surface() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut attached = attached_over(Box::new(RecordingRenderer::new(calls.clone())));

        attached.resize_surface(SurfaceId::PRIMARY, 1024, 768);
        assert_eq!(attached.surface_size(), (1024, 768));
        let recorded = calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            recorded.iter().any(|line| line == "resize 0 1024x768"),
            "the backend was resized: {recorded:?}"
        );
    }

    /// Replacing the backend without detaching it first trips the precondition
    /// in debug builds: assigning over a live backend drops it while the module
    /// it lives in may already be gone.
    #[test]
    #[should_panic(expected = "call detach_backend first")]
    fn replacing_a_live_backend_trips_the_precondition() {
        let mut attached = attached_over(Box::new(RecordingRenderer::new(Arc::new(Mutex::new(
            Vec::new(),
        )))));
        attached.replace_backend(Box::new(RecordingRenderer::new(Arc::new(Mutex::new(
            Vec::new(),
        )))));
    }
}
