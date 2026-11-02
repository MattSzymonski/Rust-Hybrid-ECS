//! Engine ownership and the frame every build runs.
//!
//! # Responsibilities
//!
//! - Own the [`Engine`] at a stable address, and whatever must outlive it.
//! - Start a statically linked project ([`setup`]).
//! - Mount a project's asset pack and index it ([`mount_asset_pack`]).
//! - Run one frame: systems, deferred commands, failure reporting, frame
//!   statistics ([`Runtime::run_frame_from`]).
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
use std::collections::HashMap;
use std::time::Duration;

// External crates
use pill_core::error::{ConfigError, EngineMessage, HostError};
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
use pill_engine::{Engine, InputEvent, RumbleRequest, World};
use pill_renderer_api::{CameraBinding, SurfaceCameras, SurfaceId};
#[cfg(feature = "rendering")]
use pill_renderer_api::{RenderViewport, RendererError, SurfaceOptions};

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
// FrameDriver
// =============================================================================

/// What a frontend's loop needs from whatever runs the game.
///
/// Implemented by the [`Runtime`] a shipping build runs, by its windowed
/// counterpart, and by the development host, so one frontend loop drives any
/// of them.
pub trait FrameDriver {
    /// What a frame can fail with; a headless runtime cannot fail at all.
    type Error: std::error::Error + 'static;

    /// Run one frame.
    ///
    /// The frame statistics are logged by the driver itself, once per
    /// reporting window, so no frontend has to carry them.
    ///
    /// # Errors
    ///
    /// Returns the driver's error when the frame could not be completed (a
    /// windowed driver: the renderer failed).
    fn run_frame(&mut self) -> Result<(), Self::Error>;

    /// The engine this driver runs.
    ///
    /// Required, and the one accessor the forwarders below are written in
    /// terms of: a driver that reached its engine another way would have to
    /// re-implement them, which is how four impls came to repeat the same five
    /// bodies.
    fn engine_mut(&mut self) -> &mut Engine;

    /// Queue an input event for the next frame.
    ///
    /// Defaulted through [`Self::engine_mut`]: input reaches the engine the
    /// same way in every driver, so a frontend cannot forget to forward it.
    fn push_input(&mut self, event: InputEvent) {
        self.engine_mut().push_input_event(event);
    }

    /// Take the rumble requests the last frame's systems queued, for the
    /// frontend that owns the gamepads to play.
    fn take_rumble_requests(&mut self) -> Vec<RumbleRequest> {
        self.engine_mut().take_rumble_requests()
    }

    /// Bind the camera surface `id` draws through, or remove its binding with
    /// `None`; see [`pill_renderer_api::SurfaceCameras`].
    fn set_surface_camera(&mut self, id: SurfaceId, binding: Option<CameraBinding>) {
        crate::set_surface_camera(self.engine_mut().world_mut(), id, binding);
    }
}

/// What a frontend needs from a driver that draws through a renderer.
///
/// Every surface method lives here rather than on [`FrameDriver`]: a headless
/// driver has no surface to forward to, and a no-op default on the base trait
/// would let a windowed frontend drop every resize without a word.
///
/// A sub-trait rather than a required method on [`FrameDriver`]: the display
/// accessor has to be required, and gating a required method on
/// `pill_player/rendering` is not possible in a crate that cannot see that
/// feature.
#[cfg(feature = "rendering")]
pub trait WindowedFrameDriver: FrameDriver {
    /// The renderer this driver draws through.
    fn display_mut(&mut self) -> &mut crate::AttachedRenderer;

    /// Draw into another window as surface `id`; see
    /// [`crate::AttachedRenderer::attach_surface`].
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the window cannot give its handles or
    /// the renderer refuses the surface.
    fn attach_surface<W: crate::RendererWindow>(
        &mut self,
        id: SurfaceId,
        window: W,
        width: u32,
        height: u32,
        options: SurfaceOptions,
    ) -> Result<(), RendererError> {
        self.display_mut()
            .attach_surface(id, window, width, height, options)
    }

    /// Stop drawing into surface `id`, then release its window.
    fn detach_surface(&mut self, id: SurfaceId) {
        self.display_mut().detach_surface(id);
    }

    /// Forward a physical resize of surface `id` to the renderer.
    fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32) {
        self.display_mut().resize_surface(id, width, height);
    }

    /// Restrict drawing to a region of surface `id`; `None` draws all of it.
    fn set_surface_viewport(&mut self, id: SurfaceId, viewport: Option<RenderViewport>) {
        self.display_mut().set_surface_viewport(id, viewport);
    }

    /// Whether surface `id` may present on the next frame; see
    /// [`crate::AttachedRenderer::set_surface_ready`].
    fn set_surface_ready(&mut self, id: SurfaceId, ready: bool) {
        self.display_mut().set_surface_ready(id, ready);
    }
}

// =============================================================================
// Runtime
// =============================================================================

/// The engine and the frame state every build needs.
pub struct Runtime {
    /// Boxed so its address stays stable when the `Runtime` moves: the
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
    /// Collapses repeated frame errors, one message at a time.
    frame_errors: FrameErrorReporter,
    /// Frames per second, measured over a reporting window.
    statistics: FrameStatistics,
    /// The `metrics` handles a frame records through, resolved on the first
    /// frame this runtime runs.
    #[cfg(feature = "metrics")]
    frame_metrics: Option<FrameMetricHandles>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// A runtime around an empty engine, for a host that registers modules and
    /// the project itself (the development host).
    pub fn new() -> Self {
        // Report detected system hardware so users can tune config.
        crate::system_specs::print_machine_info();
        Self {
            engine: Box::new(Engine::new()),
            _project_guard: None,
            #[cfg(feature = "rendering")]
            static_renderer: None,
            frame_errors: FrameErrorReporter::new(),
            statistics: FrameStatistics::new(),
            #[cfg(feature = "metrics")]
            frame_metrics: None,
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

    /// Run one scheduler frame and its reporting: systems, deferred commands,
    /// failure reporting, then the frame statistics.
    ///
    /// `frame_start` is when the caller's frame began; the frame-time metric
    /// measures from there, so a frontend that did work before the frame can
    /// report it. A caller with nothing before the frame passes
    /// `Instant::now()`. The statistics are logged here, roughly every three
    /// seconds, so every frontend reports them the same way; the line is
    /// `N FPS | M entities`, which the devops suites wait for.
    #[cfg_attr(not(feature = "metrics"), allow(unused_variables))]
    pub fn run_frame_from(&mut self, frame_start: Instant) {
        // Step 1: Execute one scheduler frame and report its failures.
        if let Err(errors) = self.engine.process_frame() {
            // Deferred command failures arrive as a batch; flatten them into one
            // message, which the reporter then rate-limits like any other.
            let summary = errors
                .iter()
                .map(EngineMessage::to_plain_message)
                .collect::<Vec<_>>()
                .join("; ");
            self.report_frame_error(&summary);
        }

        // Systems can also fail mid-frame. Each failure carries the system name
        // and its semantic message, so the text is a stable key: the reporter
        // collapses repeats of each one without letting a second failing system
        // silence or flood the first.
        for failure in self.engine.drain_system_failures() {
            self.report_frame_error(&failure.to_plain_message());
        }

        // Step 2: Track and report the frame rate over the reporting window.
        let fps = match self.statistics.count_frame(Instant::now()) {
            Some(fps) => fps,
            None => {
                // The same numbers flow through the shared `metrics` recorder,
                // which forwards them to Tracy when a profiler is attached; the
                // recorder keeps no copy, so the console report stays the
                // printed record.
                #[cfg(feature = "metrics")]
                self.record_frame_metrics(
                    frame_start.elapsed().as_secs_f64() * 1000.0,
                    self.statistics.last_measured_fps(),
                );
                return;
            }
        };
        let entity_count = self.engine.world().entity_count();

        #[cfg(feature = "metrics")]
        self.record_frame_metrics(frame_start.elapsed().as_secs_f64() * 1000.0, fps);

        // The frame-statistics line the devops suites wait for. It is logged
        // from here so every build reports it the same way, under the target
        // and level the project's `logging:` settings control.
        info!(
            target: telemetry_target::ENGINE,
            "{:.0} FPS | {} entities",
            fps,
            entity_count
        );
    }

    /// The rate the current reporting window measures.
    ///
    /// For a frontend that displays a live rate - the browser's editor canvas;
    /// the console line is logged by [`Self::run_frame_from`] itself.
    pub fn frames_per_second(&self) -> f64 {
        self.statistics.frames_per_second(Instant::now())
    }

    /// Report one per-frame engine error; see [`FrameErrorReporter::report`].
    fn report_frame_error(&mut self, message: &str) {
        for line in self.frame_errors.report(message, Instant::now()) {
            line.log();
        }
    }

    /// Record one frame's numerical state into the shared metrics recorder.
    ///
    /// The span measured here is the caller's: `frame_start` is when the
    /// frontend began the frame, so it covers whatever it drew after the systems
    /// ran. The engine's own pass records its span as `ecs.frame_time_ms` from
    /// inside `Engine::process_frame` - one name per span, so neither histogram
    /// mixes two meanings. The entity count is the engine's to report, for the
    /// same reason: it describes the world that pass produced, and the value is
    /// the same.
    #[cfg(feature = "metrics")]
    fn record_frame_metrics(&mut self, frame_time_ms: f64, fps: f64) {
        let handles = self
            .frame_metrics
            .get_or_insert_with(FrameMetricHandles::resolve);
        handles.frame_time.record(frame_time_ms);
        handles.frames_per_second.set(fps);
    }
}

// =============================================================================
// Frame Metric Handles
// =============================================================================

/// The two `metrics` handles a frame records through, resolved once.
///
/// A `metrics::histogram!(..)` or `metrics::gauge!(..)` call resolves the name
/// in the global registry every time it runs - a key built and hashed per frame,
/// for a handle that never changes. Both names are resolved on the first frame
/// the `Runtime` runs, by which time a frontend has installed its recorder, and
/// the handles are kept for the rest of its life.
#[cfg(feature = "metrics")]
struct FrameMetricHandles {
    /// The frame the frontend owns, start to start.
    frame_time: metrics::Histogram,
    /// The rate the reporting window measured.
    frames_per_second: metrics::Gauge,
}

#[cfg(feature = "metrics")]
impl FrameMetricHandles {
    /// Resolve both handles against the recorder installed right now.
    fn resolve() -> Self {
        Self {
            frame_time: metrics::histogram!("engine.frame_time_ms"),
            frames_per_second: metrics::gauge!("engine.fps"),
        }
    }
}

// =============================================================================
// Frame Error Reporter
// =============================================================================

/// Collapses repeated frame errors per message.
///
/// A frame can report several failures - the scheduler pass and any number of
/// failing systems - so the state is kept per message rather than per frame.
/// Each distinct message reaches the log at most once per
/// [`FRAME_ERROR_REPORT_INTERVAL`], carrying the repeats it swallowed, so one
/// broken system can neither flood the terminal nor silence another. Only the
/// messages reported within the last interval are remembered: an entry whose
/// repeats stopped arriving is logged and dropped by the next report that
/// arrives, because nothing else would ever report its count.
struct FrameErrorReporter {
    /// One entry per message reported within the last interval.
    entries: HashMap<String, FrameErrorEntry>,
}

/// What the reporter remembers about one message.
struct FrameErrorEntry {
    /// When this message last reached the reporter.
    last_seen: Instant,
    /// When it last reached the log.
    printed_at: Instant,
    /// Repeats swallowed since then.
    suppressed: u64,
}

impl FrameErrorReporter {
    /// A reporter that has not seen a message yet.
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Report `message`, returning the lines to log.
    ///
    /// A message that printed less than [`FRAME_ERROR_REPORT_INTERVAL`] ago only
    /// counts its repeat and needs no allocation; a message that returns after
    /// the interval prints once, with everything it swallowed, instead of once
    /// for the old window and once for the new.
    fn report(&mut self, message: &str, now: Instant) -> Vec<FrameErrorLine> {
        // Step 1: the messages that went quiet reach the log first, with the
        // repeats they swallowed.
        let mut lines = self.flush_quiet_messages(message, now);
        // Step 2: this message either collapses into its entry or prints.
        if let Some(entry) = self.entries.get_mut(message) {
            entry.last_seen = now;
            if now.duration_since(entry.printed_at) < FRAME_ERROR_REPORT_INTERVAL {
                entry.suppressed += 1;
                return lines;
            }
            let suppressed = std::mem::replace(&mut entry.suppressed, 0);
            entry.printed_at = now;
            lines.push(FrameErrorLine::new(message, suppressed));
            return lines;
        }
        self.entries.insert(
            message.to_owned(),
            FrameErrorEntry {
                last_seen: now,
                printed_at: now,
                suppressed: 0,
            },
        );
        lines.push(FrameErrorLine::new(message, 0));
        lines
    }

    /// Take the entries but `except` whose repeats stopped arriving at least
    /// [`FRAME_ERROR_REPORT_INTERVAL`] ago, returning the lines their swallowed
    /// counts still owe the log.
    fn flush_quiet_messages(&mut self, except: &str, now: Instant) -> Vec<FrameErrorLine> {
        let mut lines = Vec::new();
        let mut quiet = Vec::new();
        for (message, entry) in &self.entries {
            if message == except {
                continue;
            }
            if now.duration_since(entry.last_seen) < FRAME_ERROR_REPORT_INTERVAL {
                continue;
            }
            quiet.push(message.clone());
            if entry.suppressed > 0 {
                lines.push(FrameErrorLine::new(message, entry.suppressed));
            }
        }
        for message in quiet {
            self.entries.remove(&message);
        }
        lines
    }
}

/// One `frame error:` line the reporter decided to log.
struct FrameErrorLine {
    /// The message, as the engine wrote it.
    message: String,
    /// Repeats swallowed since it last printed; `0` for a first sighting.
    suppressed: u64,
}

impl FrameErrorLine {
    /// A line for `message` that swallowed `suppressed` repeats.
    fn new(message: &str, suppressed: u64) -> Self {
        Self {
            message: message.to_owned(),
            suppressed,
        }
    }

    /// Write the line: the text keeps the `frame error: <message>` shape, with
    /// the repeat count as a field only when there is one.
    fn log(&self) {
        if self.suppressed == 0 {
            error!(
                target: telemetry_target::ENGINE,
                "frame error: {}",
                self.message
            );
        } else {
            error!(
                target: telemetry_target::ENGINE,
                suppressed = self.suppressed,
                "frame error: {}",
                self.message
            );
        }
    }
}

// =============================================================================
// Frame Statistics
// =============================================================================

/// The frame rate, measured over the window the frame loop reports at.
///
/// The live rate a frontend displays and the line the loop logs come from the
/// same three numbers, so the two cannot disagree.
struct FrameStatistics {
    /// Frames counted since the window opened.
    frames: u64,
    /// When the window opened.
    window_started: Instant,
    /// The rate the last completed window measured.
    last_fps: f64,
}

impl FrameStatistics {
    /// Statistics whose first window opens now.
    fn new() -> Self {
        Self {
            frames: 0,
            window_started: Instant::now(),
            last_fps: 0.0,
        }
    }

    /// Count one frame; returns the finished window's rate, and opens the next
    /// window, once [`FRAME_REPORT_INTERVAL_SECONDS`] have passed.
    fn count_frame(&mut self, now: Instant) -> Option<f64> {
        self.frames += 1;
        let elapsed = now.duration_since(self.window_started).as_secs_f64();
        if elapsed < FRAME_REPORT_INTERVAL_SECONDS {
            return None;
        }
        let fps = self.frames as f64 / elapsed;
        self.last_fps = fps;
        self.frames = 0;
        self.window_started = now;
        Some(fps)
    }

    /// The rate the current window measures so far, or the last completed rate
    /// while it has counted no frame yet.
    fn frames_per_second(&self, now: Instant) -> f64 {
        let elapsed = now.duration_since(self.window_started).as_secs_f64();
        if self.frames == 0 || elapsed <= f64::EPSILON {
            return self.last_fps;
        }
        self.frames as f64 / elapsed
    }

    /// The rate the last completed window measured.
    ///
    /// Only the metrics recorder asks for it: the console line reports the rate
    /// of the window that just finished, which the caller already holds.
    #[cfg(feature = "metrics")]
    fn last_measured_fps(&self) -> f64 {
        self.last_fps
    }
}

impl FrameDriver for Runtime {
    type Error = std::convert::Infallible;

    fn run_frame(&mut self) -> Result<(), Self::Error> {
        Runtime::run_frame_from(self, Instant::now());
        Ok(())
    }

    fn engine_mut(&mut self) -> &mut Engine {
        Runtime::engine_mut(self)
    }
}

// =============================================================================
// PackSource
// =============================================================================

/// Where a project's asset pack came from, which the mount logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackSource {
    /// Baked into the binary by the shipping bundle.
    Embedded,
    /// Downloaded beside the page by a browser frontend.
    Fetched,
}

impl PackSource {
    /// The mount line for this source. Each source has its own literal rather
    /// than one format string, so the line is a fixed log token.
    fn mounted_message(self) -> &'static str {
        match self {
            Self::Embedded => "mounted the embedded asset pack",
            Self::Fetched => "mounted the fetched asset pack",
        }
    }

    /// The database-indexed line for this source, likewise a fixed token.
    fn indexed_message(self) -> &'static str {
        match self {
            Self::Embedded => "[assets] database indexed from the embedded pack",
            Self::Fetched => "[assets] database indexed from the fetched pack",
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Write `binding` for surface `id` into the world's
/// [`pill_renderer_api::SurfaceCameras`], creating the resource on first use.
///
/// Shared by every driver with a renderer, so the binding lives where the
/// rendering system reads it.
pub fn set_surface_camera(world: &mut World, id: SurfaceId, binding: Option<CameraBinding>) {
    if world.get_resource::<SurfaceCameras>().is_none() {
        world.insert_resource(SurfaceCameras::default());
    }
    if let Some(cameras) = world.get_resource_mut::<SurfaceCameras>() {
        cameras.set(id, binding);
    }
}

/// Insert the debug-drawing and picking resources into `world`, each only
/// where it is missing.
///
/// Every build has them: debug drawing on in the development host, and in a
/// shipping or web build as its project settings say. A system toggles it at
/// runtime through [`pill_engine::DebugDraw::set_enabled`].
pub fn insert_debug_resources(world: &mut World, debug_draw_enabled: bool) {
    if !world.has_resource::<pill_engine::DebugDraw>() {
        world.insert_resource(pill_engine::DebugDraw::new(debug_draw_enabled));
    }
    if !world.has_resource::<pill_renderer_api::SelectionOutlines>() {
        world.insert_resource(pill_renderer_api::SelectionOutlines::default());
    }
    if !world.has_resource::<pill_renderer_api::PickRequests>() {
        world.insert_resource(pill_renderer_api::PickRequests::default());
    }
    if !world.has_resource::<pill_renderer_api::PickResults>() {
        world.insert_resource(pill_renderer_api::PickResults::default());
    }
}

/// Parse an asset pack and index every asset in it.
///
/// Returns the database to insert as a resource: it holds every packed asset
/// by guid and by path, so a reference can find and load one that is not
/// loaded yet. Parsing and mounting happen in one step because mounting
/// consumes the pack.
///
/// # Errors
///
/// Returns [`ConfigError::AssetPackInvalid`] when the bytes are not a pack.
pub fn mount_asset_pack(
    project_name: &str,
    source: PackSource,
    bytes: impl Into<std::borrow::Cow<'static, [u8]>>,
) -> Result<pill_engine::AssetDatabase, HostError> {
    // Step 1: Parse the pack and report where it came from.
    let pack = pill_engine::asset_store::AssetPack::parse(bytes).map_err(|error| {
        ConfigError::AssetPackInvalid {
            project: project_name.to_owned(),
            detail: error.to_string(),
        }
    })?;
    info!(
        target: telemetry_target::ENGINE,
        files = pack.len(),
        "{}",
        source.mounted_message()
    );

    // Step 2: Index every entry, so a reference can find and load an asset
    // that is not loaded yet.
    let database = pill_engine::AssetDatabase::from_pack(&pack);
    info!(
        target: telemetry_target::ENGINE,
        assets = database.len(),
        "{}",
        source.indexed_message()
    );

    // Step 3: Mount the pack, then close the store to everything else: a
    // shipped build must not fall back to a file on the player's machine when
    // an asset is missing from the bundle.
    pill_engine::asset_store::mount_pack(pack);
    pill_engine::asset_store::restrict_to_packs();
    Ok(database)
}

/// Create the engine and initialize a statically linked project, mounting the
/// pack the project embeds.
///
/// Nothing is built, watched or loaded: the project and its extensions were
/// linked into this binary, and the frontend passes their entry points in. A
/// browser frontend cannot embed a pack, so it fetches one and calls
/// [`setup_with_asset_database`] instead.
///
/// # Errors
///
/// Returns a typed [`HostError`] when the embedded pack is not a pack, when an
/// extension's or the project's entry point reports a non-zero initialization
/// status, or an external backend fails to start. There is no previous
/// generation to fall back to on this path, so the first failure is fatal.
pub fn setup(project: StaticProject) -> Result<Runtime, HostError> {
    // Mount the embedded assets first: a module's or the project's init may
    // already load a `res` file, and a shipped game must read the copy it was
    // built with.
    let database = match project.asset_pack {
        Some(bytes) => Some(mount_asset_pack(project.name, PackSource::Embedded, bytes)?),
        None => None,
    };
    setup_with_asset_database(project, database)
}

/// Create the engine and initialize a statically linked project whose pack has
/// already been mounted.
///
/// `database` is the one [`mount_asset_pack`] returned, or `None` for a
/// project without assets. It is inserted before any module or project init
/// runs, which is why a fetched pack has to arrive here rather than be added
/// afterwards.
///
/// # Errors
///
/// Returns a typed [`HostError`] when an extension's or the project's entry
/// point reports a non-zero initialization status, or an external backend
/// fails to start. There is no previous generation to fall back to on this
/// path, so the first failure is fatal.
pub fn setup_with_asset_database(
    project: StaticProject,
    database: Option<pill_engine::AssetDatabase>,
) -> Result<Runtime, HostError> {
    // Step 1: Create the engine with its debug and picking resources, then
    // apply the log levels the bundle was built with, before anything logs on
    // the project's behalf.
    let mut runtime = Runtime::new();
    insert_debug_resources(runtime.engine_mut().world_mut(), project.debug_draw);

    let logging = project
        .logging
        .settings()
        .map_err(|details| format!("invalid logging settings in the bundle: {details}"))
        .and_then(|settings| {
            crate::apply_logging_settings(&settings).map_err(|error| error.to_string())
        });
    if let Err(error) = logging {
        pill_core::warn!(
            target: telemetry_target::ENGINE,
            "the project's logging settings were not applied: {error}"
        );
    }

    // Step 2: Announce the host and insert the database the caller already
    // mounted, because mounting has to precede every module's and the
    // project's init.
    info!(
        target: telemetry_target::LINK,
        project = project.name,
        modules = project.modules.len(),
        "ECS host starting (statically linked)"
    );

    if let Some(database) = database {
        runtime.engine_mut().world_mut().insert_resource(database);
    }

    // Step 3: Register every module, then the project, in the order and under
    // the owners the reloading host uses.
    runtime._project_guard = project.initialize(runtime.engine_mut())?;
    // The renderer is registered when a window attaches it, like the loaded
    // module; it takes the next owner after the modules, as that module does.
    #[cfg(feature = "rendering")]
    {
        runtime.static_renderer = project
            .renderer
            .map(|renderer| (renderer, crate::registration::renderer_owner()));
    }
    Ok(runtime)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use super::*;

    #[cfg(feature = "metrics")]
    use std::collections::HashMap;
    #[cfg(feature = "metrics")]
    use std::sync::{Arc, OnceLock};

    /// Held by every test that runs a frame.
    ///
    /// The metrics recorder is process-wide, so a test that counts what one
    /// frame records needs the other frame-running tests to be between frames
    /// while it does.
    static FRAME_LOCK: Mutex<()> = Mutex::new(());

    /// How often each metric was updated, keyed by its name.
    #[cfg(feature = "metrics")]
    type MetricSamples = Arc<Mutex<HashMap<String, usize>>>;

    /// A `metrics` recorder that counts updates per name.
    ///
    /// The engine's recorder keeps nothing, so a test that asks how many
    /// samples one frame records - and under which names - has to install a
    /// recorder of its own. `metrics` allows one per process, which is why it
    /// is installed once behind a `OnceLock` and shared by this binary.
    #[cfg(feature = "metrics")]
    struct CountingRecorder {
        samples: MetricSamples,
    }

    /// The handle one metric name gets: every update lands in the shared map.
    #[cfg(feature = "metrics")]
    struct CountingHandle {
        name: String,
        samples: MetricSamples,
    }

    #[cfg(feature = "metrics")]
    impl CountingHandle {
        fn count(&self) {
            *self
                .samples
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(self.name.clone())
                .or_insert(0) += 1;
        }
    }

    #[cfg(feature = "metrics")]
    impl metrics::CounterFn for CountingHandle {
        fn increment(&self, _value: u64) {
            self.count();
        }

        fn absolute(&self, _value: u64) {
            self.count();
        }
    }

    #[cfg(feature = "metrics")]
    impl metrics::GaugeFn for CountingHandle {
        fn increment(&self, _value: f64) {
            self.count();
        }

        fn decrement(&self, _value: f64) {
            self.count();
        }

        fn set(&self, _value: f64) {
            self.count();
        }
    }

    #[cfg(feature = "metrics")]
    impl metrics::HistogramFn for CountingHandle {
        fn record(&self, _value: f64) {
            self.count();
        }

        fn record_many(&self, _value: f64, _count: usize) {
            self.count();
        }
    }

    #[cfg(feature = "metrics")]
    impl metrics::Recorder for CountingRecorder {
        fn describe_counter(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_gauge(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn describe_histogram(
            &self,
            _key: metrics::KeyName,
            _unit: Option<metrics::Unit>,
            _description: metrics::SharedString,
        ) {
        }

        fn register_counter(
            &self,
            key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            metrics::Counter::from_arc(Arc::new(CountingHandle {
                name: key.name().to_owned(),
                samples: self.samples.clone(),
            }))
        }

        fn register_gauge(
            &self,
            key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Gauge {
            metrics::Gauge::from_arc(Arc::new(CountingHandle {
                name: key.name().to_owned(),
                samples: self.samples.clone(),
            }))
        }

        fn register_histogram(
            &self,
            key: &metrics::Key,
            _metadata: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::from_arc(Arc::new(CountingHandle {
                name: key.name().to_owned(),
                samples: self.samples.clone(),
            }))
        }
    }

    /// The installed counting recorder's map, installed on first use.
    #[cfg(feature = "metrics")]
    fn metric_samples() -> MetricSamples {
        static SAMPLES: OnceLock<MetricSamples> = OnceLock::new();
        SAMPLES
            .get_or_init(|| {
                let samples: MetricSamples = Arc::new(Mutex::new(HashMap::new()));
                let installed = metrics::set_global_recorder(CountingRecorder {
                    samples: samples.clone(),
                });
                assert!(
                    installed.is_ok(),
                    "a recorder was installed before this binary's first metrics test"
                );
                samples
            })
            .clone()
    }

    /// A project that registers nothing.
    fn no_op(_engine: &mut Engine) -> u32 {
        0
    }

    /// Whether the project's init saw the asset database as a resource.
    static MOUNTED_BEFORE_INIT: AtomicBool = AtomicBool::new(false);

    /// An init that records whether the asset database was already there.
    fn record_whether_the_pack_is_mounted(engine: &mut Engine) -> u32 {
        MOUNTED_BEFORE_INIT.store(
            engine
                .world()
                .get_resource::<pill_engine::AssetDatabase>()
                .is_some(),
            Ordering::SeqCst,
        );
        0
    }

    /// The bytes of an asset pack holding `files`, in the format
    /// `AssetPack::parse` reads.
    fn pack_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        use pill_engine::asset_store::{ASSET_PACK_MAGIC, ASSET_PACK_VERSION};

        let index_length: usize = files.iter().map(|(path, _)| 4 + path.len() + 16).sum();
        let mut offset = (8 + 4 + 4 + index_length) as u64;
        let mut pack = Vec::new();
        pack.extend_from_slice(ASSET_PACK_MAGIC);
        pack.extend_from_slice(&ASSET_PACK_VERSION.to_le_bytes());
        pack.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, bytes) in files {
            pack.extend_from_slice(&(path.len() as u32).to_le_bytes());
            pack.extend_from_slice(path.as_bytes());
            pack.extend_from_slice(&offset.to_le_bytes());
            pack.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            offset += bytes.len() as u64;
        }
        for (_, bytes) in files {
            pack.extend_from_slice(bytes);
        }
        pack
    }

    /// A statically linked project starts and runs frames.
    #[test]
    fn a_static_project_starts_and_runs_frames() {
        let _frame = FRAME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut runtime = setup(crate::static_project::test_project(
            crate::StaticProjectBackend::Native { init: no_op },
        ))
        .expect("it starts");
        runtime.run_frame_from(Instant::now());
        assert_eq!(runtime.engine().world().entity_count(), 0);
    }

    /// A project whose pack was fetched gets the database the fetch mounted
    /// inserted before it initializes, so an init that loads an asset finds
    /// it - the browser path.
    #[test]
    fn a_fetched_pack_is_mounted_before_the_project_initializes() {
        let bytes = pack_of(&[("fetched/pack-probe.txt", b"from the pack")]);
        let database =
            mount_asset_pack("project", PackSource::Fetched, bytes).expect("the pack is valid");

        let _runtime = setup_with_asset_database(
            crate::static_project::test_project(crate::StaticProjectBackend::Native {
                init: record_whether_the_pack_is_mounted,
            }),
            Some(database),
        )
        .expect("it starts");
        assert!(
            MOUNTED_BEFORE_INIT.load(Ordering::SeqCst),
            "the project's init has to see the database the fetch mounted"
        );
        // The store serves the pack's bytes, not a file of the same name.
        let bytes = pill_engine::asset_store::read(std::path::Path::new("fetched/pack-probe.txt"))
            .expect("the packed entry reads back");
        assert_eq!(bytes, b"from the pack");
    }

    /// One frame records one sample per metric, each under a name that says
    /// which span it measured.
    ///
    /// Two spans exist and they are not the same number: the engine's own pass,
    /// measured inside `Engine::process_frame`, and the frame the frontend owns,
    /// which also covers whatever it draws before the next one. One histogram
    /// carrying both mixes two meanings into one series.
    #[cfg(feature = "metrics")]
    #[test]
    fn a_frame_records_one_sample_per_metric() {
        let _frame = FRAME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let samples = metric_samples();
        samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();

        let mut runtime = Runtime::new();
        runtime.run_frame_from(Instant::now());

        let counts = samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            counts.get("engine.frame_time_ms").copied().unwrap_or(0),
            1,
            "the frontend's frame is recorded once: {counts:?}"
        );
        assert_eq!(
            counts.get("ecs.frame_time_ms").copied().unwrap_or(0),
            1,
            "the engine's pass is recorded once, under its own name: {counts:?}"
        );
        assert_eq!(
            counts.get("ecs.entities").copied().unwrap_or(0),
            1,
            "the entity count is reported once: {counts:?}"
        );
    }

    /// The metric handles are resolved on the first frame, not when the `Runtime`
    /// is built, so a recorder installed after the `Runtime` exists still
    /// receives the samples.
    #[cfg(feature = "metrics")]
    #[test]
    fn the_frame_metric_handles_are_resolved_on_the_first_frame() {
        let _frame = FRAME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut runtime = Runtime::new();
        assert!(
            runtime.frame_metrics.is_none(),
            "a runtime must not resolve metric handles before it runs a frame"
        );

        let samples = metric_samples();
        samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();

        runtime.run_frame_from(Instant::now());
        assert!(
            runtime.frame_metrics.is_some(),
            "the first frame resolves the handles"
        );

        runtime.run_frame_from(Instant::now());
        let counts = samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(
            counts.get("engine.frame_time_ms").copied().unwrap_or(0),
            2,
            "both frames record through the same handles: {counts:?}"
        );
    }

    /// Two failing systems alternate their messages: each prints once per
    /// interval and carries the repeats it swallowed, so neither floods the log
    /// at frame rate and neither silences the other.
    ///
    /// The single-signature limiter this replaced printed both messages on
    /// every frame, because each report reset the other's counter.
    #[test]
    fn alternating_frame_errors_print_once_each_per_interval() {
        const FRAME: Duration = Duration::from_millis(16);
        let started = Instant::now();
        let mut reporter = FrameErrorReporter::new();
        let mut lines = Vec::new();
        // Two and a half intervals of alternating messages, one report a frame.
        for frame in 0..160u32 {
            let now = started + FRAME * frame;
            let message = if frame % 2 == 0 {
                "physics system failed: NaN transform"
            } else {
                "audio system failed: device lost"
            };
            lines.extend(reporter.report(message, now));
        }
        for message in [
            "physics system failed: NaN transform",
            "audio system failed: device lost",
        ] {
            let printed: Vec<u64> = lines
                .iter()
                .filter(|line| line.message == message)
                .map(|line| line.suppressed)
                .collect();
            assert_eq!(
                printed.len(),
                3,
                "{message} printed {} times in 2.5 intervals: {printed:?}",
                printed.len()
            );
            assert_eq!(printed[0], 0, "a first sighting prints at once");
            assert!(
                printed[1] >= 25,
                "the repeats between two prints ride along: {printed:?}"
            );
        }
    }

    /// A message that goes quiet still reports the repeats it swallowed, and a
    /// message that returns after the interval prints once, not twice.
    #[test]
    fn a_silenced_frame_error_reports_its_swallowed_repeats() {
        let started = Instant::now();
        let mut reporter = FrameErrorReporter::new();
        let mut lines = reporter.report("flooding system failed", started);
        assert_eq!(lines.len(), 1, "a first sighting prints at once");
        assert_eq!(lines[0].suppressed, 0);
        // Four repeats inside the interval: counted, not printed.
        for frame in 1..5u32 {
            lines = reporter.report(
                "flooding system failed",
                started + Duration::from_millis(16 * u64::from(frame)),
            );
            assert!(lines.is_empty(), "a repeat inside the interval is silent");
        }
        // Another system's failure arrives once the flooding one has been silent
        // for a whole interval: its count is flushed before the new line prints.
        let later = started
            + Duration::from_millis(64)
            + FRAME_ERROR_REPORT_INTERVAL
            + Duration::from_millis(50);
        let lines = reporter.report("another system failed", later);
        assert_eq!(lines.len(), 2, "the quiet count and the new message print");
        assert_eq!(lines[0].message, "flooding system failed");
        assert_eq!(lines[0].suppressed, 4, "the four repeats it swallowed");
        assert_eq!(lines[1].message, "another system failed");
        assert_eq!(lines[1].suppressed, 0);
        // The flushed message is forgotten; if it comes back it prints once.
        let again = reporter.report(
            "flooding system failed",
            later + FRAME_ERROR_REPORT_INTERVAL + Duration::from_millis(50),
        );
        assert_eq!(again.len(), 1, "a returning message prints once");
        assert_eq!(again[0].suppressed, 0);
    }

    /// Input pushed through the frame driver is what the next frame's systems
    /// read, and the rumble they request comes back out of it.
    #[test]
    fn input_reaches_the_next_frame_and_rumble_comes_back() {
        use pill_engine::{ButtonState, Input, KeyCode, PlayerId, ResMut, SystemError};

        fn rumble_on_space(mut input: ResMut<Input>) -> Result<(), SystemError> {
            let Some(mut input) = input.get_mut() else {
                return Ok(());
            };
            if input.key_pressed(KeyCode::Space) {
                input.request_rumble(PlayerId::Player1, 1.0, 1.0, Duration::from_millis(100));
            }
            Ok(())
        }

        let mut runtime = Runtime::new();
        runtime
            .engine_mut()
            .register_system("rumble_on_space", rumble_on_space);

        let _frame = FRAME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        FrameDriver::push_input(
            &mut runtime,
            InputEvent::Key {
                key: KeyCode::Space,
                state: ButtonState::Pressed,
            },
        );
        // Nothing is applied before the frame runs.
        assert!(FrameDriver::take_rumble_requests(&mut runtime).is_empty());

        FrameDriver::run_frame(&mut runtime).expect("a headless frame cannot fail");
        let requests = FrameDriver::take_rumble_requests(&mut runtime);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].player, PlayerId::Player1);

        // The press was an edge: the next frame sees the key held, not pressed.
        FrameDriver::run_frame(&mut runtime).expect("a headless frame cannot fail");
        assert!(FrameDriver::take_rumble_requests(&mut runtime).is_empty());
        let input = runtime
            .engine()
            .world()
            .get_resource::<Input>()
            .expect("the engine inserts Input");
        assert!(input.key_held(KeyCode::Space));
        assert!(!input.key_pressed(KeyCode::Space));
    }
}
