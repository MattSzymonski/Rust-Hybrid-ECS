//! Engine ownership and the frame every build runs.
//!
//! # Responsibilities
//!
//! - Own the [`Engine`] at a stable address, and whatever must outlive it.
//! - Start a statically linked project ([`setup`]).
//! - Run one frame: systems, deferred commands, failure reporting, frame
//!   statistics ([`Runtime::run_frame_with`]).
//! - Define [`FrameDriver`], what a frontend's loop needs from whatever runs
//!   the game.
//!
//! # Design
//!
//! The development host wraps a [`Runtime`] and does its reload work around
//! the frame; a shipping build uses it directly. Both reach the same frame
//! code, so a shipped game runs its systems exactly as the game ran while it
//! was developed.

// Standard library
use std::any::Any;
use std::time::Duration;

// External crates
use pill_core::error::{ConfigError, EngineMessage, HostError};
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
use pill_engine::Engine;
use pill_renderer_api::RenderViewport;

// Current crate
use crate::StaticProject;

// =============================================================================
// Constants
// =============================================================================

/// Minimum interval between repeated frame-error reports.
const FRAME_ERROR_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Length of the window frame statistics are reported over.
const FRAME_REPORT_INTERVAL_SECONDS: f64 = 3.0;

// =============================================================================
// Types
// =============================================================================

/// Result of one frame when the reporting interval elapses.
#[derive(Debug, Clone, Copy)]
pub struct FrameReport {
    /// Frames per second measured over the reporting window.
    pub fps: f64,
    /// Number of live entities at report time.
    pub entity_count: usize,
}

/// What a frontend's loop needs from whatever runs the game.
///
/// Implemented by the [`Runtime`] a shipping build runs, by its windowed
/// counterpart, and by the development host, so one frontend loop drives any
/// of them.
pub trait FrameDriver {
    /// What a frame can fail with; a headless runtime cannot fail at all.
    type Error: std::error::Error + 'static;

    /// Run one frame; `Some` when the reporting interval elapsed.
    ///
    /// # Errors
    ///
    /// Returns the driver's error when the frame could not be completed (a
    /// windowed driver: the renderer failed).
    fn run_frame(&mut self) -> Result<Option<FrameReport>, Self::Error>;

    /// Forward a physical window resize; nothing to do without a window.
    fn resize(&mut self, _width: u32, _height: u32) {}

    /// Restrict drawing to a region of the window; nothing to do without one.
    fn set_render_viewport(&mut self, _viewport: Option<RenderViewport>) {}
}

/// The engine and the frame state every build needs.
pub struct Runtime {
    /// Boxed so its address stays stable when the runtime moves: the
    /// development host hands loaded modules a raw pointer to it.
    engine: Box<Engine>,
    /// What an external project backend returned, kept until the engine is
    /// gone. Declared after `engine`, so the engine - and every system the
    /// backend registered - drops first.
    _project_guard: Option<Box<dyn Any>>,
    /// The renderer a windowed shipping build links, with the owner its
    /// `rendering` system registers under; `None` when there is none.
    #[cfg(feature = "rendering")]
    static_renderer: Option<(crate::StaticRenderer, pill_engine::SystemOwner)>,
    /// The last frame error reported, to collapse repeats.
    last_frame_error: Option<String>,
    /// When a frame error was last printed.
    last_error_report: Instant,
    /// Repeats of the last error since it was last printed.
    suppressed_error_count: u64,
    /// Frames since the reporting window opened.
    frame_count: u64,
    /// When the reporting window opened.
    last_report: Instant,
    /// The rate the last completed window measured.
    last_measured_fps: f64,
}

// =============================================================================
// Impls
// =============================================================================

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// A runtime around an empty engine, for a host that registers modules and
    /// the project itself (the development host).
    pub fn new() -> Self {
        Self {
            engine: Box::new(Engine::new()),
            _project_guard: None,
            #[cfg(feature = "rendering")]
            static_renderer: None,
            last_frame_error: None,
            last_error_report: Instant::now(),
            suppressed_error_count: 0,
            frame_count: 0,
            last_report: Instant::now(),
            last_measured_fps: 0.0,
        }
    }

    /// Read-only engine access for rendering and diagnostics.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable engine access for frontend-owned, frame-boundary work.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// The linked renderer and its owner, when a windowed shipping build has
    /// one.
    #[cfg(feature = "rendering")]
    pub(crate) fn static_renderer(
        &self,
    ) -> Option<(crate::StaticRenderer, pill_engine::SystemOwner)> {
        self.static_renderer
    }

    /// Run one frame: see [`Self::run_frame_with`].
    pub fn run_frame(&mut self) -> Option<FrameReport> {
        self.run_frame_with(Instant::now(), |_| {})
    }

    /// Run one scheduler frame and its reporting: systems, deferred commands,
    /// `after_systems`, then the frame statistics.
    ///
    /// `after_systems` runs once the systems have: the development host calls
    /// its modules' per-frame export there. `frame_start` is when the caller's
    /// frame began, for the frame-time metric. Returns a report roughly every
    /// three seconds for a frontend to print or display; all other frames
    /// return `None`.
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    pub fn run_frame_with(
        &mut self,
        frame_start: Instant,
        after_systems: impl FnOnce(&mut Engine),
    ) -> Option<FrameReport> {
        // Step 1: Execute one scheduler frame and report its failures.
        if let Err(errors) = self.engine.process_frame() {
            // Deferred command failures arrive as a batch; flatten them into one
            // rate-limited report using each error's plain semantic message.
            let summary = errors
                .iter()
                .map(EngineMessage::to_plain_message)
                .collect::<Vec<_>>()
                .join("; ");
            self.report_frame_error(summary);
        }

        // Systems can also fail mid-frame. Each failure carries the system name
        // and its semantic message; the rate limiter collapses repeated
        // identical failures across frames.
        for failure in self.engine.drain_system_failures() {
            self.report_frame_error(failure.to_plain_message());
        }

        // Step 2: Whatever the caller runs after the systems.
        after_systems(&mut self.engine);

        // Step 3: Track and report the frame rate over the reporting window.
        self.frame_count += 1;
        let elapsed = self.last_report.elapsed().as_secs_f64();
        if elapsed < FRAME_REPORT_INTERVAL_SECONDS {
            // Repeated numerical state is recorded every frame through metrics,
            // independent of the low-frequency console report.
            #[cfg(feature = "metrics")]
            record_frame_metrics(
                self.engine.world().entity_count(),
                frame_start.elapsed().as_secs_f64() * 1000.0,
                self.last_measured_fps,
            );
            return None;
        }

        let fps = self.frame_count as f64 / elapsed;
        let report = FrameReport {
            fps,
            entity_count: self.engine.world().entity_count(),
        };
        self.last_measured_fps = fps;
        self.frame_count = 0;
        self.last_report = Instant::now();

        #[cfg(feature = "metrics")]
        record_frame_metrics(
            report.entity_count,
            frame_start.elapsed().as_secs_f64() * 1000.0,
            fps,
        );

        Some(report)
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

impl FrameDriver for Runtime {
    type Error = std::convert::Infallible;

    fn run_frame(&mut self) -> Result<Option<FrameReport>, Self::Error> {
        Ok(Runtime::run_frame(self))
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Create the engine and initialize a statically linked project.
///
/// Nothing is built, watched or loaded: the project and its extensions were
/// linked into this binary, and the frontend passes their entry points in.
///
/// # Errors
///
/// Returns a typed [`HostError`] when an extension's or the project's entry
/// point reports a non-zero initialization status, or an external backend
/// fails to start. There is no previous generation to fall back to on this
/// path, so the first failure is fatal.
pub fn setup(project: StaticProject) -> Result<Runtime, HostError> {
    let mut runtime = Runtime::new();

    info!(
        target: telemetry_target::ENGINE,
        module = project.name,
        modules = project.modules.len(),
        "ECS host starting (statically linked)"
    );

    // Mount the embedded assets first: a module's or the project's init may
    // already load a `res` file, and a shipped game must read the copy it was
    // built with.
    if let Some(bytes) = project.asset_pack {
        let pack = pill_engine::asset_store::AssetPack::parse(bytes).map_err(|error| {
            ConfigError::AssetPackInvalid {
                project: project.name.to_owned(),
                detail: error.to_string(),
            }
        })?;
        info!(
            target: telemetry_target::ENGINE,
            files = pack.len(),
            "mounted the embedded asset pack"
        );
        pill_engine::asset_store::mount_pack(pack);
    }

    // Register every module, then the project, in the order and under the
    // owners the reloading host uses.
    runtime._project_guard = project.initialize(runtime.engine_mut())?;
    // The renderer is registered when a window attaches it, like the loaded
    // module; it takes the next owner after the modules, as that module does.
    #[cfg(feature = "rendering")]
    {
        runtime.static_renderer = project.renderer.map(|renderer| {
            (
                renderer,
                crate::registration::renderer_owner(project.modules.len()),
            )
        });
    }
    Ok(runtime)
}

/// Run one frame of `runtime`; see [`Runtime::run_frame`].
pub fn run_one_frame(runtime: &mut Runtime) -> Option<FrameReport> {
    runtime.run_frame()
}

/// Record one frame's numerical state into the shared metrics recorder.
#[cfg(feature = "metrics")]
fn record_frame_metrics(entity_count: usize, frame_time_ms: f64, fps: f64) {
    metrics::gauge!("ecs.entities").set(entity_count as f64);
    metrics::histogram!("engine.frame_time_ms").record(frame_time_ms);
    metrics::gauge!("engine.fps").set(fps);
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A project that registers nothing.
    fn no_op(_engine: &mut Engine) -> u32 {
        0
    }

    /// A statically linked project starts and runs frames; the first ones
    /// report nothing until the reporting window has elapsed.
    #[test]
    fn a_static_project_starts_and_runs_frames() {
        let mut runtime = setup(StaticProject {
            name: "project",
            backend: crate::StaticProjectBackend::Native { init: no_op },
            modules: &[],
            renderer: None,
            asset_pack: None,
        })
        .expect("it starts");
        assert!(run_one_frame(&mut runtime).is_none());
        assert_eq!(runtime.current_frame_report().entity_count, 0);
    }

    /// The hook a host passes runs once per frame, after the systems.
    #[test]
    fn the_after_systems_hook_runs_once_per_frame() {
        let mut runtime = Runtime::new();
        let mut calls = 0;
        runtime.run_frame_with(Instant::now(), |_| calls += 1);
        runtime.run_frame_with(Instant::now(), |_| calls += 1);
        assert_eq!(calls, 2);
    }
}
