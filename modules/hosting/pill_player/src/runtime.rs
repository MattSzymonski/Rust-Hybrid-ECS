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
use pill_engine::{Engine, InputEvent, RumbleRequest, World};
use pill_renderer_api::{CameraBinding, RenderViewport, SurfaceCameras, SurfaceId};
#[cfg(feature = "rendering")]
use pill_renderer_api::{RendererError, SurfaceOptions};

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

    /// Queue an input event for the next frame.
    ///
    /// Required rather than defaulted: a driver that forgot to forward input
    /// would drop every key without a trace.
    fn push_input(&mut self, event: InputEvent);

    /// Take the rumble requests the last frame's systems queued, for the
    /// frontend that owns the gamepads to play.
    fn take_rumble_requests(&mut self) -> Vec<RumbleRequest>;

    #[cfg(feature = "rendering")]
    /// Draw into another window as surface `id`; see
    /// [`crate::AttachedRenderer::attach_surface`]. A driver without a
    /// renderer refuses.
    ///
    /// # Errors
    ///
    /// Returns a [`RendererError`] when the driver draws no window, the window
    /// cannot give its handles, or the renderer refuses the surface.
    fn attach_surface<W: crate::RendererWindow>(
        &mut self,
        id: SurfaceId,
        window: W,
        width: u32,
        height: u32,
        options: SurfaceOptions,
    ) -> Result<(), RendererError> {
        let _ = (window, width, height, options);
        Err(RendererError::UnsupportedSurface {
            detail: format!("surface {} (this driver draws no window)", id.0),
        })
    }

    /// Stop drawing into surface `id`; nothing to do without a renderer.
    fn detach_surface(&mut self, _id: SurfaceId) {}

    /// Forward a physical resize of surface `id`; nothing to do without one.
    ///
    /// Required rather than defaulted: a driver that forgot to forward a
    /// resize would keep drawing at the old size with nothing in the log to
    /// say why. A driver without a renderer implements it as a no-op.
    fn resize_surface(&mut self, id: SurfaceId, width: u32, height: u32);

    /// Restrict drawing to a region of surface `id`; nothing to do without one.
    ///
    /// Required for the same reason [`Self::resize_surface`] is. A driver
    /// without a renderer implements it as a no-op.
    fn set_surface_viewport(&mut self, id: SurfaceId, viewport: Option<RenderViewport>);

    /// Whether surface `id` may present on the next frame (the frontend's
    /// pacing); nothing to do without a renderer.
    fn set_surface_ready(&mut self, _id: SurfaceId, _ready: bool) {}

    /// Bind the camera surface `id` draws through, or remove its binding with
    /// `None`; see [`pill_renderer_api::SurfaceCameras`].
    fn set_surface_camera(&mut self, _id: SurfaceId, _binding: Option<CameraBinding>) {}
}

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
        // Report detected system hardware so users can tune config.
        crate::system_specs::print_system_specs();
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
            // The same numbers flow through the shared `metrics` recorder, which
            // forwards them to Tracy when a profiler is attached; the recorder
            // keeps no copy, so the console report stays the printed record.
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

impl FrameDriver for Runtime {
    type Error = std::convert::Infallible;

    fn run_frame(&mut self) -> Result<Option<FrameReport>, Self::Error> {
        Ok(Runtime::run_frame(self))
    }

    // A headless runtime draws no surface, so a resize and a viewport have
    // nothing to reach.
    fn resize_surface(&mut self, _id: SurfaceId, _width: u32, _height: u32) {}

    fn set_surface_viewport(&mut self, _id: SurfaceId, _viewport: Option<RenderViewport>) {}

    fn push_input(&mut self, event: InputEvent) {
        self.engine.push_input_event(event);
    }

    fn take_rumble_requests(&mut self) -> Vec<RumbleRequest> {
        self.engine.take_rumble_requests()
    }
}

// =============================================================================
// Free Functions
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
    /// The mount line for this source. Two literals rather than one format
    /// string, so the log contract can look for either in the source.
    fn mounted_message(self) -> &'static str {
        match self {
            Self::Embedded => "mounted the embedded asset pack",
            Self::Fetched => "mounted the fetched asset pack",
        }
    }

    /// The database line for this source, for the same reason.
    fn indexed_message(self) -> &'static str {
        match self {
            Self::Embedded => "[assets] database indexed from the embedded pack",
            Self::Fetched => "[assets] database indexed from the fetched pack",
        }
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
    let database = pill_engine::AssetDatabase::from_pack(&pack);
    info!(
        target: telemetry_target::ENGINE,
        assets = database.len(),
        "{}",
        source.indexed_message()
    );
    pill_engine::asset_store::mount_pack(pack);
    // From here on the store reads what the pack holds and nothing else: a
    // shipped build must not fall back to a file on the player's machine when
    // an asset is missing from the bundle.
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
    let mut runtime = Runtime::new();
    insert_debug_resources(runtime.engine_mut().world_mut(), project.debug_draw);

    // The project's log levels, baked into the bundle from its settings file,
    // before anything is logged on its behalf.
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

    info!(
        target: telemetry_target::ENGINE,
        module = project.name,
        modules = project.modules.len(),
        "ECS host starting (statically linked)"
    );

    // The database arrives already mounted, because mounting has to precede
    // every module's and the project's init.
    if let Some(database) = database {
        runtime.engine_mut().world_mut().insert_resource(database);
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
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

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
            asset_pack_url: None,
            logging: crate::StaticLogging::NONE,
            debug_draw: false,
            startup_scene: None,
        })
        .expect("it starts");
        assert!(run_one_frame(&mut runtime).is_none());
        assert_eq!(runtime.current_frame_report().entity_count, 0);
    }

    /// A project whose pack was fetched gets the database the fetch mounted
    /// inserted before it initializes, so an init that loads an asset finds
    /// it - the browser path.
    #[test]
    fn a_fetched_pack_is_mounted_before_the_project_initializes() {
        let bytes = pack_of(&[("fetched/pack-probe.txt", b"from the pack")]);
        let database =
            mount_asset_pack("project", PackSource::Fetched, bytes).expect("the pack is valid");

        let runtime = setup_with_asset_database(
            StaticProject {
                name: "project",
                backend: crate::StaticProjectBackend::Native {
                    init: record_whether_the_pack_is_mounted,
                },
                modules: &[],
                renderer: None,
                asset_pack: None,
                asset_pack_url: None,
                logging: crate::StaticLogging::NONE,
                debug_draw: false,
                startup_scene: None,
            },
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
        assert!(runtime.current_frame_report().entity_count == 0);
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
