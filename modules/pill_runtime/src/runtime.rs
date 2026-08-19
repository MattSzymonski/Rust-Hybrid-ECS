//! One live engine generation: the world, its project, and its renderer.
//!
//! # Responsibilities
//!
//! - Own the [`Engine`], its stable [`EngineApi`] table, the loaded project
//!   module, and the optional window renderer for one generation.
//! - Execute one hot-reload-aware frame per [`Runtime::run_one_frame`] call.
//! - Track frame statistics, rate-limited frame errors, and the exit request.
//!
//! # Design
//!
//! This is the engine-owning half of what used to be `pill_host::runtime`. It
//! moved into the reloadable dynamic library because everything it holds is
//! invalidated by an engine rebuild: the engine itself, the renderer's GPU
//! state, and the project module whose `EngineApi` function pointers address
//! this binary's code. The host keeps only what survives a swap - the window,
//! the watchers, the build runner, and the telemetry pipeline.
//!
//! The headless and windowed paths are one type rather than two. The former
//! `Host` / `RenderingHost` split existed to keep GPU state out of a host
//! crate's public API; that API no longer exists, and a single type with an
//! optional renderer makes the reload transaction a single object to rebuild.

// Standard library
use std::path::PathBuf;
use std::time::{Duration, Instant};

// External crates
use pill_core::error::{EngineMessage, HostError};
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
use pill_engine::{Engine, EngineApi};
use pill_runtime_api::FrameReport;
#[cfg(feature = "rendering")]
use pill_runtime_api::{PillWindowHandleV1, RenderViewport, VirtualResolution};

// Current crate
use crate::project::ProjectDescriptor;
use crate::project_module::LoadedProject;

// =============================================================================
// Constants
// =============================================================================

/// Minimum interval between repeated frame-error reports.
const FRAME_ERROR_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Interval at which a console frame report is produced.
const FRAME_REPORT_INTERVAL_SECONDS: f64 = 3.0;

// =============================================================================
// Runtime
// =============================================================================

/// Everything one engine generation needs to run frames.
///
/// The host holds this behind an opaque handle and calls into it only between
/// frames on its main thread, so no field needs to be `Send` or `Sync`.
pub struct Runtime {
    /// Workspace root used for temporary project-library copies.
    workspace_root: PathBuf,
    /// Boxed before `EngineApi` is built so its raw engine pointer stays valid
    /// even if the runtime itself is moved.
    engine: Box<Engine>,
    /// Stable function-pointer table handed to the project module.
    engine_api: EngineApi,
    /// The native or managed project module currently loaded.
    loaded_project: LoadedProject,
    /// GPU state bound to the host's window, absent in headless runs.
    #[cfg(feature = "rendering")]
    renderer: Option<pill_engine::Renderer>,
    /// Signature of the most recently reported frame error.
    last_frame_error: Option<String>,
    /// When the most recent frame-error report was printed.
    last_error_report: Instant,
    /// Repeated identical frame errors collapsed since that report.
    suppressed_error_count: u64,
    /// Frames executed since the current reporting window opened.
    frame_count: u64,
    /// When the current reporting window opened.
    last_report: Instant,
    /// Frame rate measured over the previous reporting window.
    last_measured_fps: f64,
    /// Report produced by the last frame, waiting to be taken by the host.
    pending_frame_report: Option<FrameReport>,
    /// Whether the runtime asked the host to stop the frame loop.
    exit_requested: bool,
}

impl Runtime {
    /// Bring up one engine generation and load its project module.
    ///
    /// # Errors
    ///
    /// Returns the typed [`HostError`] naming the failing subsystem: native
    /// library loading, project initialization, or managed backend startup.
    pub fn new(workspace_root: PathBuf, descriptor: ProjectDescriptor) -> Result<Self, HostError> {
        // Step 1: Construct the engine and its stable API table.
        // `EngineApi` stores a raw pointer into this allocation, so the engine
        // must reach its final stable address before the table is built.
        let mut engine = Box::new(Engine::new());
        engine.set_parallel_execution(true);
        let engine_api = EngineApi::new(&mut engine);

        // Step 2: Load and initialize the project module. The host already
        // built it, so this only maps or hosts the finished artifact.
        let loaded_project =
            LoadedProject::start(&mut engine, &engine_api, &workspace_root, &descriptor)?;

        Ok(Self {
            workspace_root,
            engine,
            engine_api,
            loaded_project,
            #[cfg(feature = "rendering")]
            renderer: None,
            last_frame_error: None,
            last_error_report: Instant::now(),
            suppressed_error_count: 0,
            frame_count: 0,
            last_report: Instant::now(),
            last_measured_fps: 0.0,
            pending_frame_report: None,
            exit_requested: false,
        })
    }

    /// Read-only engine access for diagnostics and state capture.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable engine access for state restore.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Whether the runtime asked the host to stop the frame loop.
    pub fn is_exit_requested(&self) -> bool {
        self.exit_requested
    }

    /// Take the periodic console report produced by the last frame.
    pub fn take_frame_report(&mut self) -> Option<FrameReport> {
        self.pending_frame_report.take()
    }

    /// Snapshot the current frame rate and entity count without resetting the
    /// reporting window used by console frontends.
    pub fn current_frame_report(&self) -> FrameReport {
        let elapsed = self.last_report.elapsed().as_secs_f64();
        let fps = if self.frame_count == 0 || elapsed <= f64::EPSILON {
            self.last_measured_fps
        } else {
            self.frame_count as f64 / elapsed
        };

        FrameReport {
            fps,
            entity_count: self.engine.world().entity_count() as u64,
        }
    }

    /// Swap in a rebuilt project module, preserving world state.
    ///
    /// `module_path` names the artifact the host just built; the managed
    /// backend ignores it and polls its collectible loader instead.
    pub fn reload_project(&mut self, module_path: Option<&std::path::Path>) {
        info!(
            target: telemetry_target::HOT_RELOAD,
            "project hot reload triggered"
        );
        self.loaded_project.reload(
            &mut self.engine,
            &self.engine_api,
            &self.workspace_root,
            module_path,
        );
    }

    /// Execute one frame: project reload polling, systems, and presentation.
    ///
    /// # Errors
    ///
    /// Returns a message when presentation fails for a fatal reason. Frame
    /// errors raised by systems and deferred commands are rate-limited and
    /// reported rather than returned, because a single broken system must not
    /// stop the loop.
    pub fn run_one_frame(&mut self) -> Result<(), String> {
        #[cfg(feature = "metrics")]
        let frame_start = Instant::now();

        // Step 1: Poll the managed loader for an assembly swap. The managed
        // loader watches the built assembly instead of source files, so a
        // successful build is observed even between host reload signals.
        self.loaded_project.poll_managed_reload();

        // Step 2: Execute one scheduler frame and report its failures.
        if let Err(errors) = self.engine.process_frame() {
            // Deferred command failures arrive as a batch; flatten them into
            // one rate-limited report using each error's semantic message.
            let summary = errors
                .iter()
                .map(EngineMessage::to_plain_message)
                .collect::<Vec<_>>()
                .join("; ");
            self.report_frame_error(summary);

            // The engine only surfaces command errors here when it is
            // configured to treat them as fatal, so honour that by asking the
            // host to stop instead of spinning on a broken world.
            if self.engine.should_exit_on_error {
                self.exit_requested = true;
            }
        }

        // Systems can also fail mid-frame. Each failure carries the system
        // name and its semantic message; the rate limiter collapses repeated
        // identical failures across frames.
        for failure in self.engine.drain_system_failures() {
            self.report_frame_error(failure.to_plain_message());
        }

        // Step 3: Invoke the native compatibility update after scheduler
        // systems. Managed games run entirely as scheduler systems; native
        // games retain this update hook after their scheduled work.
        self.loaded_project.update(&self.engine_api);

        // Step 4: Present the world to the surface when one is attached.
        #[cfg(feature = "rendering")]
        if let Some(renderer) = self.renderer.as_mut() {
            if let Err(error) = renderer.render(&mut self.engine) {
                // Presentation failures are fatal: the surface is unusable and
                // every later frame would fail the same way.
                self.exit_requested = true;
                return Err(error.to_plain_message());
            }
        }

        // Step 5: Track frame rate and publish a report once per window.
        self.frame_count += 1;
        let elapsed = self.last_report.elapsed().as_secs_f64();
        if elapsed < FRAME_REPORT_INTERVAL_SECONDS {
            // Repeated numerical state is recorded every frame through
            // metrics, independent of the low-frequency console report.
            #[cfg(feature = "metrics")]
            record_frame_metrics(
                self.engine.world().entity_count(),
                frame_start.elapsed().as_secs_f64() * 1000.0,
                self.last_measured_fps,
            );
            return Ok(());
        }

        let fps = self.frame_count as f64 / elapsed;
        let report = FrameReport {
            fps,
            entity_count: self.engine.world().entity_count() as u64,
        };
        self.last_measured_fps = fps;
        self.frame_count = 0;
        self.last_report = Instant::now();
        self.pending_frame_report = Some(report);

        #[cfg(feature = "metrics")]
        record_frame_metrics(
            report.entity_count as usize,
            frame_start.elapsed().as_secs_f64() * 1000.0,
            fps,
        );

        Ok(())
    }

    /// Report one per-frame engine error with rate limiting.
    ///
    /// Repeated identical errors are collapsed: they print at most once per
    /// [`FRAME_ERROR_REPORT_INTERVAL`] together with the number of suppressed
    /// occurrences, so a broken system cannot flood the terminal at frame rate.
    fn report_frame_error(&mut self, signature: String) {
        let now = Instant::now();
        if self.last_frame_error.as_deref() == Some(signature.as_str()) {
            self.suppressed_error_count += 1;
            if now.duration_since(self.last_error_report) >= FRAME_ERROR_REPORT_INTERVAL {
                error!(
                    target: telemetry_target::ENGINE,
                    suppressed = self.suppressed_error_count,
                    "frame error: {signature}"
                );
                self.suppressed_error_count = 0;
                self.last_error_report = now;
            }
            return;
        }
        error!(target: telemetry_target::ENGINE, "frame error: {signature}");
        self.last_frame_error = Some(signature);
        self.suppressed_error_count = 0;
        self.last_error_report = now;
    }
}

// =============================================================================
// Runtime - Rendering
// =============================================================================

#[cfg(feature = "rendering")]
impl Runtime {
    /// Create the GPU surface for the host's window.
    ///
    /// Headless descriptors are accepted and leave the runtime without a
    /// renderer, which is how the windowed host starts before its window
    /// exists and how the headless host runs permanently.
    ///
    /// # Errors
    ///
    /// Returns a message when the descriptor names a platform this build
    /// cannot bind, or when surface, adapter, or device creation fails.
    ///
    /// # Safety
    ///
    /// The window described by `window` must outlive this runtime. The host
    /// guarantees it by owning the window for the whole process lifetime and
    /// destroying every runtime generation before it.
    pub unsafe fn attach_renderer(
        &mut self,
        window: &PillWindowHandleV1,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if !window.describes_window() {
            self.renderer = None;
            return Ok(());
        }

        let (window_handle, display_handle) = window.to_raw_handles().ok_or_else(|| {
            String::from("the supplied window descriptor names an unsupported platform")
        })?;

        // SAFETY: The caller guarantees the described window outlives this
        // runtime, which is the contract the raw-handle surface requires.
        let renderer = unsafe {
            pill_engine::Renderer::from_raw_window_handle(
                display_handle,
                window_handle,
                width,
                height,
            )
        }
        .map_err(|error| error.to_plain_message())?;
        self.renderer = Some(renderer);
        Ok(())
    }

    /// Move rendering to a different native window.
    ///
    /// A replacement renderer is constructed before the old one is dropped, so
    /// a failure leaves the current surface untouched.
    ///
    /// # Errors
    ///
    /// Returns a message when the new surface cannot be created.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::attach_renderer`].
    pub unsafe fn retarget_render_window(
        &mut self,
        window: &PillWindowHandleV1,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let (window_handle, display_handle) = window.to_raw_handles().ok_or_else(|| {
            String::from("the supplied window descriptor names an unsupported platform")
        })?;

        // SAFETY: The caller guarantees the described window outlives this
        // runtime, which is the contract the raw-handle surface requires.
        let renderer = unsafe {
            pill_engine::Renderer::from_raw_window_handle(
                display_handle,
                window_handle,
                width,
                height,
            )
        }
        .map_err(|error| error.to_plain_message())?;
        self.renderer = Some(renderer);
        Ok(())
    }

    /// Forward a physical window resize to the renderer.
    pub fn resize(&mut self, width: u32, height: u32) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.resize(width, height);
        }
    }

    /// Restrict engine drawing to a physical region of the native surface.
    ///
    /// `None` restores full-surface rendering.
    pub fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_viewport(viewport);
        }
    }

    /// Map a stable project coordinate space into the current viewport.
    ///
    /// `None` makes logical renderer units match physical pixels again.
    pub fn set_virtual_resolution(&mut self, resolution: Option<VirtualResolution>) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_virtual_resolution(resolution);
        }
    }
}

// =============================================================================
// Runtime - Headless stubs
// =============================================================================

#[cfg(not(feature = "rendering"))]
impl Runtime {
    /// Resizing is a no-op without a renderer, which keeps the host's window
    /// event handling identical across both builds.
    pub fn resize(&mut self, _width: u32, _height: u32) {}
}

// =============================================================================
// Free Functions
// =============================================================================

/// Record one frame's numerical state into the runtime metrics recorder.
///
/// The recorder forwards through the ABI metrics sink, so these samples reach
/// the host's process-global store rather than the dylib's own copy.
#[cfg(feature = "metrics")]
fn record_frame_metrics(entity_count: usize, frame_time_ms: f64, fps: f64) {
    metrics::gauge!("ecs.entities").set(entity_count as f64);
    metrics::histogram!("engine.frame_time_ms").record(frame_time_ms);
    metrics::gauge!("engine.fps").set(fps);
}
