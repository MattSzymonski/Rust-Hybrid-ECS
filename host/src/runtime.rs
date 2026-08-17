//! Engine ownership and frontend-facing frame orchestration.
//!
//! # Responsibilities
//!
//! - Own the engine instance and expose safe frontend access.
//! - Assemble the host state shared by all frontends.
//! - Execute one hot-reload-aware frame per [`run_one_frame`] call.
//!
//! # Design
//!
//! This module contains the stable API used by `standalone`, `editor`, and
//! other host binaries. [`Host`] bundles the engine and project module for the
//! headless path, [`RenderingHost`] adds a renderer for windowed frontends,
//! and backend-specific loading stays behind [`LoadedProject`].

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// External crates
use pill_core::error::{EngineMessage, HostError};
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
#[cfg(feature = "rendering")]
use pill_engine::EngineError;
use pill_engine::{Engine, EngineApi};
#[cfg(feature = "rendering")]
use pill_engine::{RenderViewport, Renderer, RendererError, RendererWindow, VirtualResolution};

// Current crate
use crate::project_module::LoadedProject;
use crate::native_library::cleanup_temporary_files;
use crate::watcher::spawn_file_watcher;
use crate::{ProjectModuleBackend, ProjectModuleConfig};

// =============================================================================
// Constants
// =============================================================================

/// Minimum interval between repeated frame-error reports.
const FRAME_ERROR_REPORT_INTERVAL: Duration = Duration::from_secs(1);

// =============================================================================
// Types + Impls
// =============================================================================

/// Everything the frame-step loop needs, assembled once during startup.
///
/// Bundling this state lets headless, windowed, and editor frontends share the
/// same engine lifetime and hot-reload behavior through [`run_one_frame`].
pub struct Host {
    workspace_root: PathBuf,
    module_config: ProjectModuleConfig,
    // Boxed before EngineApi is created so its raw engine pointer remains
    // stable even if Host is moved by a caller.
    engine: Box<Engine>,
    engine_api: EngineApi,
    loaded_project: LoadedProject,
    reload_generation: Arc<AtomicU64>,
    last_processed_generation: u64,
    last_frame_error: Option<String>,
    last_error_report: Instant,
    suppressed_error_count: u64,
    frame_count: u64,
    last_report: Instant,
    last_measured_fps: f64,
}

impl Host {
    /// Read-only engine access for rendering and diagnostics.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable engine access for frontend-owned ad-hoc work.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Snapshot the current frame rate and entity count without resetting the
    /// three-second reporting window used by console frontends.
    pub fn current_frame_report(&self) -> FrameReport {
        let elapsed = self.last_report.elapsed().as_secs_f64();
        let fps = if self.frame_count == 0 || elapsed <= f64::EPSILON {
            self.last_measured_fps
        } else {
            self.frame_count as f64 / elapsed
        };

        FrameReport {
            fps,
            entity_count: self.engine.world().entity_count(),
        }
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
                eprintln!(
                    "[host] Frame error ({} more occurrences): {signature}",
                    self.suppressed_error_count
                );
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
        eprintln!("[host] Frame error: {signature}");
        error!(target: telemetry_target::ENGINE, "frame error: {signature}");
        self.last_frame_error = Some(signature);
        self.suppressed_error_count = 0;
        self.last_error_report = now;
    }
}

/// Host state with the engine renderer attached to one native window surface.
///
/// Keeping the renderer beside [`Host`] makes its creation and lifetime part
/// of host setup. Executable crates never construct or retain GPU resources.
#[cfg(feature = "rendering")]
pub struct RenderingHost {
    host: Host,
    renderer: Renderer,
}

#[cfg(feature = "rendering")]
impl RenderingHost {
    /// Move rendering to a newly created native window surface.
    ///
    /// The existing ECS host and project module remain alive. A replacement is
    /// constructed before the old renderer is dropped, so initialization
    /// failure leaves the current surface untouched.
    pub fn retarget_render_window<W>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<(), RendererError>
    where
        W: RendererWindow + 'static,
    {
        let renderer = Renderer::new(window, width, height)?;
        self.renderer = renderer;
        Ok(())
    }

    /// Forward a physical window resize to the engine renderer.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.renderer.resize(width, height);
    }

    /// Restrict engine drawing to a physical region of the native surface.
    ///
    /// Use `None` for full-window rendering. Embedded frontends can leave the
    /// corresponding WebView region transparent and keep surrounding UI
    /// panels opaque.
    pub fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.renderer.set_viewport(viewport);
    }

    /// Map a stable project coordinate space into the current physical viewport.
    ///
    /// Pass `None` to make logical renderer units match physical pixels again.
    pub fn set_render_virtual_resolution(&mut self, resolution: Option<VirtualResolution>) {
        self.renderer.set_virtual_resolution(resolution);
    }

    /// Execute one ECS frame and present its resulting world to the surface.
    pub fn run_one_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        let report = run_one_frame(&mut self.host);
        self.renderer.render(self.host.engine_mut())?;
        Ok(report)
    }

    /// Read live frame statistics for UI overlays without affecting the
    /// lower-frequency report returned by [`Self::run_one_frame`].
    pub fn current_frame_report(&self) -> FrameReport {
        self.host.current_frame_report()
    }
}

/// Result of one [`run_one_frame`] call when the reporting interval elapses.
#[derive(Debug, Clone, Copy)]
pub struct FrameReport {
    /// Frames per second measured over the reporting window.
    pub fps: f64,
    /// Number of live entities at report time.
    pub entity_count: usize,
}

// =============================================================================
// Free Functions
// =============================================================================

/// Build/load the project module, create the engine, and start its source watcher.
///
/// # Errors
///
/// Returns a typed [`HostError`] naming the failing subsystem: configuration,
/// build, library loading, watcher startup, or managed backend startup.
pub fn setup(module_config: ProjectModuleConfig) -> Result<Host, HostError> {
    // Step 1: Reject inconsistent configurations before any build or load.
    module_config.validate()?;

    // Step 2: Resolve the workspace root and print the selected configuration.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or(HostError::WorkspaceRootUndetermined)?
        .to_path_buf();

    print_startup_configuration(&workspace_root, &module_config);

    if matches!(
        module_config.backend,
        ProjectModuleBackend::NativeLibrary { .. }
    ) {
        cleanup_temporary_files(&workspace_root);
    }

    // Step 3: Construct the engine and its stable API table.
    // EngineApi stores a raw pointer into this allocation, so the engine must
    // reach its final stable address before the API table is constructed.
    let mut engine = Box::new(Engine::new());
    engine.set_parallel_execution(true);
    let engine_api = EngineApi::new(&mut engine);

    // Step 4: Build and load the project module, then start its source watcher.
    let loaded_project = LoadedProject::start(&mut engine, &engine_api, &workspace_root, &module_config)?;

    let reload_generation = Arc::new(AtomicU64::new(0));
    spawn_file_watcher(
        workspace_root.clone(),
        &module_config,
        Arc::clone(&reload_generation),
    )?;

    println!();
    println!(
        "[host] Entering project loop. Edit {}/**/* to hot-reload.",
        module_config.watch_directory
    );
    println!();

    Ok(Host {
        workspace_root,
        module_config,
        engine,
        engine_api,
        loaded_project,
        reload_generation,
        last_processed_generation: 0,
        last_frame_error: None,
        last_error_report: Instant::now(),
        suppressed_error_count: 0,
        frame_count: 0,
        last_report: Instant::now(),
        last_measured_fps: 0.0,
    })
}

/// Set up the engine, project module, hot reload, and renderer together.
///
/// A frontend owns its platform event loop and supplies its cloneable window
/// handle. The engine creates exactly one surface for that window, while the
/// returned [`RenderingHost`] owns the renderer for the rest of its lifetime.
///
/// # Errors
///
/// Returns the composed [`EngineError`], which transparently carries either
/// a [`HostError`] from setup or a [`RendererError`] from surface creation.
#[cfg(feature = "rendering")]
pub fn setup_rendering<W>(
    module_config: ProjectModuleConfig,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, EngineError>
where
    W: RendererWindow + 'static,
{
    let host = setup(module_config)?;
    let renderer = Renderer::new(window, width, height)?;
    Ok(RenderingHost { host, renderer })
}

/// Process hot reloads, execute one scheduler frame, and update FPS tracking.
///
/// Returns a report roughly every three seconds for a frontend to print or
/// display; all other frames return `None`.
pub fn run_one_frame(host: &mut Host) -> Option<FrameReport> {
    #[cfg(feature = "metrics")]
    let frame_start = Instant::now();

    // Step 1: Process a pending hot reload before running systems.
    // The watcher bumps a generation counter; reloading while it differs from
    // the last processed value means events that arrive during a reload are
    // never lost.
    let generation = host.reload_generation.load(Ordering::Acquire);
    if generation != host.last_processed_generation {
        info!(
            target: telemetry_target::HOT_RELOAD,
            generation,
            "hot reload triggered"
        );
        host.loaded_project.reload(
            &mut host.engine,
            &host.engine_api,
            &host.workspace_root,
            &host.module_config,
            // A save during the build advances the generation beyond this
            // baseline, which cancels the in-flight compilation; the next
            // frame observes the newer generation and rebuilds.
            Some((&host.reload_generation, generation)),
        );
        host.last_processed_generation = host.reload_generation.load(Ordering::Acquire);
    }

    // Step 2: Poll the managed loader for an assembly swap.
    // The managed loader watches the built assembly instead of source files.
    host.loaded_project.poll_managed_reload();

    // Step 3: Execute one scheduler frame and report its failures.
    if let Err(errors) = host.engine.process_frame() {
        // Deferred command failures arrive as a batch; flatten them into one
        // rate-limited report using each error's plain semantic message.
        let summary = errors
            .iter()
            .map(pill_core::error::EngineMessage::to_plain_message)
            .collect::<Vec<_>>()
            .join("; ");
        host.report_frame_error(summary);
    }

    // Systems can also fail mid-frame. Each failure carries the system name
    // and its semantic message; the rate limiter collapses repeated identical
    // failures across frames.
    for failure in host.engine.drain_system_failures() {
        host.report_frame_error(failure.to_plain_message());
    }

    // Step 4: Invoke the native compatibility update after scheduler systems.
    // Managed games run entirely as scheduler systems. Native games retain
    // this compatibility update hook after their scheduled work.
    host.loaded_project.update(&host.engine_api);

    // Step 5: Track and report FPS over the three-second window.
    host.frame_count += 1;
    let elapsed = host.last_report.elapsed().as_secs_f64();
    if elapsed < 3.0 {
        // Repeated numerical state is recorded every frame through metrics,
        // independent of the low-frequency console report.
        #[cfg(feature = "metrics")]
        record_frame_metrics(
            host.engine.world().entity_count(),
            frame_start.elapsed().as_secs_f64() * 1000.0,
            host.last_measured_fps,
        );
        return None;
    }

    let fps = host.frame_count as f64 / elapsed;
    let report = FrameReport {
        fps,
        entity_count: host.engine.world().entity_count(),
    };
    host.last_measured_fps = fps;
    host.frame_count = 0;
    host.last_report = Instant::now();

    #[cfg(feature = "metrics")]
    record_frame_metrics(
        report.entity_count,
        frame_start.elapsed().as_secs_f64() * 1000.0,
        fps,
    );

    Some(report)
}

/// Record one frame's numerical state into the shared metrics recorder.
#[cfg(feature = "metrics")]
fn record_frame_metrics(entity_count: usize, frame_time_ms: f64, fps: f64) {
    metrics::gauge!("ecs.entities").set(entity_count as f64);
    metrics::histogram!("engine.frame_time_ms").record(frame_time_ms);
    metrics::gauge!("engine.fps").set(fps);
}

/// Print the selected backend before any build output starts streaming.
fn print_startup_configuration(workspace_root: &Path, module_config: &ProjectModuleConfig) {
    info!(
        target: telemetry_target::ENGINE,
        workspace = %workspace_root.display(),
        module = module_config.name,
        backend = ?module_config.backend,
        build_command = %module_config.build_command.join(" "),
        watch_directory = module_config.watch_directory,
        "ECS host starting"
    );
}
