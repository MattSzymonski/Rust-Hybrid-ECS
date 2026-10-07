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
use std::future::Future;

// External crates
use pill_core::info;
use pill_core::telemetry::telemetry_target;
use pill_engine::{Engine, InputEvent, RumbleRequest, World};
use pill_renderer_api::{
    AttachFuture, PillRenderer, RawWindowData, RenderFrame, RenderViewport, RendererError,
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
    }

    /// Replace the backend on the same window, keeping the viewport.
    pub fn replace_backend(&mut self, renderer: Box<dyn PillRenderer>) {
        self.renderer = renderer;
        self.renderer.set_viewport(self.viewport);
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
