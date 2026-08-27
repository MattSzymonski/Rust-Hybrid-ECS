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
#[cfg(feature = "hot_reload")]
use std::path::{Path, PathBuf};
#[cfg(feature = "hot_reload")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "hot_reload")]
use std::sync::Arc;
use std::time::{Duration, Instant};

// External crates
#[cfg(feature = "hot_reload")]
use pill_core::error::CSharpError;
use pill_core::error::{EngineMessage, HostError};
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
use pill_engine::Engine;
#[cfg(feature = "hot_reload")]
use pill_engine::EngineApi;
#[cfg(feature = "rendering")]
use pill_engine::EngineError;
#[cfg(feature = "rendering")]
use pill_engine::{RenderViewport, Renderer, RendererError, RendererWindow, VirtualResolution};

// Current crate
#[cfg(feature = "hot_reload")]
use crate::analytics;
#[cfg(feature = "hot_reload")]
use crate::config::project_depends_on_crate;
#[cfg(feature = "hot_reload")]
use crate::csharp::ModuleExposedComponent;
#[cfg(feature = "hot_reload")]
use crate::native_library::cleanup_temporary_files;
#[cfg(feature = "hot_reload")]
use crate::optional_module::OptionalModuleSlot;
#[cfg(feature = "hot_reload")]
use crate::project_module::LoadedProject;
#[cfg(feature = "hot_reload")]
use crate::watcher::spawn_source_watcher;
#[cfg(not(feature = "hot_reload"))]
use crate::StaticProject;
#[cfg(feature = "hot_reload")]
use crate::{HostConfig, ProjectModuleBackend, ProjectModuleConfig};

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
    /// Only a reloading build needs this: it is the directory every spawned
    /// cargo build runs in and every artifact path is resolved against.
    #[cfg(feature = "hot_reload")]
    workspace_root: PathBuf,
    #[cfg(feature = "hot_reload")]
    module_config: ProjectModuleConfig,
    // Boxed before EngineApi is created so its raw engine pointer remains
    // stable even if Host is moved by a caller.
    engine: Box<Engine>,
    /// The C-callable table a loaded artifact is handed at every entry point.
    ///
    /// A pure function-pointer table with no side effects, so a statically
    /// linked build - which calls Rust functions directly and never crosses an
    /// FFI boundary - has no use for it and does not build one.
    #[cfg(feature = "hot_reload")]
    engine_api: EngineApi,
    /// The managed runtime, when a shipping build runs a C# project.
    ///
    /// Held only to keep .NET alive: dropping it unloads the runtime out from
    /// under the systems its assembly registered. Nothing calls into it per
    /// frame, because managed gameplay is entirely scheduler systems.
    #[cfg(not(feature = "hot_reload"))]
    _managed_runtime: Option<crate::csharp::CSharpRuntime>,
    /// The loaded project image, and the generations retired behind it.
    ///
    /// Absent without `hot_reload`: a statically linked project has no image to
    /// hold, its entry point having been called once during setup.
    #[cfg(feature = "hot_reload")]
    loaded_project: LoadedProject,
    /// Optional modules, each with its own watcher and reload transaction.
    #[cfg(feature = "hot_reload")]
    optional_modules: Vec<OptionalModuleSlot>,
    #[cfg(feature = "hot_reload")]
    reload_generation: Arc<AtomicU64>,
    #[cfg(feature = "hot_reload")]
    last_processed_generation: u64,
    /// Per-function fast path, when the project opted in with `#[pill_hot]`.
    ///
    /// `None` when the feature is off, when no function is annotated, or when
    /// the project does not also build an `rlib` - all of which simply leave the
    /// existing whole-module reload as the only path.
    #[cfg(feature = "hot_patch")]
    hot_patch: Option<crate::hot_patch::HotPatchSession>,
    /// Per-function fast path for each optional module, positionally paired
    /// with `optional_modules`.
    ///
    /// An entry is `None` when that module annotated nothing, so a module that
    /// has not opted in costs nothing beyond one source scan at startup.
    #[cfg(feature = "hot_patch")]
    module_hot_patch: Vec<Option<crate::hot_patch::HotPatchSession>>,
    /// Every patch library loaded in this process, newest last.
    ///
    /// Process-wide rather than per-session on purpose: a patch links its own
    /// copy of everything its body calls, so patching one crate has to redirect
    /// the copies sitting inside another crate's patches too. Never unloaded - a
    /// jump or a slot may point into any of them for the rest of the run.
    #[cfg(feature = "hot_patch")]
    loaded_patches: Vec<crate::hot_patch::LoadedPatch>,
    last_frame_error: Option<String>,
    last_error_report: Instant,
    suppressed_error_count: u64,
    frame_count: u64,
    last_report: Instant,
    last_measured_fps: f64,
}

impl Host {
    /// Every patch generation this process has installed, newest last.
    ///
    /// Generation zero - the code each artifact was built with - has no entry
    /// because it needs no loaded library; [`Self::rollback_patch`] still
    /// accepts it.
    #[cfg(feature = "hot_patch")]
    pub fn patch_generations(&self) -> Vec<crate::hot_patch::PatchGeneration> {
        let mut all = Vec::new();
        if let Some(session) = &self.hot_patch {
            all.extend(session.generations());
        }
        for session in self.module_hot_patch.iter().flatten() {
            all.extend(session.generations());
        }
        all
    }

    /// Reinstall an earlier generation of one patched function.
    ///
    /// `generation` is one-based in the order patches were applied; zero
    /// restores the code the running artifact was built with. This is a pointer
    /// store per artifact - nothing is rebuilt, reloaded or unloaded, which is
    /// what dispatching through a slot buys over rewriting a function prologue.
    ///
    /// Call it between frames. It is safe from a frontend because it borrows
    /// the host mutably, and the frame loop cannot be running concurrently.
    ///
    /// # Errors
    ///
    /// Returns a message when no session has patched `function`, when the
    /// generation does not exist, or when an artifact refuses the address. On
    /// any error the currently running implementation is left in place.
    #[cfg(feature = "hot_patch")]
    pub fn rollback_patch(&mut self, function: &str, generation: u32) -> Result<(), String> {
        let Host {
            hot_patch,
            module_hot_patch,
            loaded_patches,
            optional_modules,
            loaded_project,
            engine,
            ..
        } = self;

        // The owning session is whichever one patched this function; searching
        // avoids making callers know whether it came from the project or a
        // module.
        let session = hot_patch
            .iter_mut()
            .chain(module_hot_patch.iter_mut().flatten())
            .find(|session| session.knows_function(function))
            .ok_or_else(|| format!("`{function}` has not been patched in this session"))?;

        let targets = patch_targets(loaded_project, optional_modules);
        let result = session.rollback(engine, &targets, loaded_patches, function, generation);
        drop(targets);
        if result.is_ok() {
            println!(
                "{} {function} {}",
                crate::console::bold_cyan("[hot]"),
                crate::console::green(&format!("ROLLED BACK to generation {generation}"))
            );
        }
        result
    }

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

/// What a frontend passes to say which project to run.
///
/// The two postures answer that question with different things, and neither
/// makes sense in the other: a reloading build resolves a project path, build
/// command and watch directory from a [`HostConfig`], while a shipping build
/// has the entry points linked in and describes them with a
/// [`StaticProject`](crate::StaticProject).
///
/// Aliasing them lets every frontend-facing entry point - headless `run`,
/// windowed `run`, [`setup_rendering`] - keep one signature instead of a
/// `#[cfg]` pair, because `impl Into<ProjectSource>` is satisfied by
/// `HostConfig`, by `ProjectModuleConfig` through its `From`, and by
/// `StaticProject` through the blanket `From<T> for T`.
#[cfg(feature = "hot_reload")]
pub type ProjectSource = HostConfig;

/// See the `hot_reload` version above.
#[cfg(not(feature = "hot_reload"))]
pub type ProjectSource = StaticProject;

// =============================================================================
// Free Functions
// =============================================================================

#[cfg(feature = "hot_reload")]
/// Build/load the project module, create the engine, and start its source watcher.
///
/// # Errors
///
/// Returns a typed [`HostError`] naming the failing subsystem: configuration,
/// build, library loading, watcher startup, or managed backend startup.
pub fn setup(host_config: impl Into<HostConfig>) -> Result<Host, HostError> {
    // Step 1: Reject inconsistent configurations before any build or load.
    let host_config = host_config.into();
    let module_config = host_config.project;
    module_config.validate()?;
    for module in &host_config.optional_modules {
        module.validate()?;
    }

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

    // Step 4: Build, load and watch the optional modules before the project.
    // Modules are infrastructure: loading them first means the project can rely
    // on whatever they register. Each gets its own owner tag and its own
    // generation counter, so later reloads stay isolated from each other.
    let mut optional_modules = Vec::with_capacity(host_config.optional_modules.len());
    for (index, module_config) in host_config.optional_modules.iter().enumerate() {
        let module_generation = Arc::new(AtomicU64::new(0));
        let slot = OptionalModuleSlot::start(
            &mut engine,
            &engine_api,
            &workspace_root,
            module_config,
            pill_engine::SystemOwner::optional_module(index),
            Arc::clone(&module_generation),
        )?;
        spawn_source_watcher(
            workspace_root.clone(),
            &module_config.name,
            &module_config.watch_directory,
            module_generation,
        )?;
        optional_modules.push(slot);
    }

    // Step 5: Build and load the project module, then start its source watcher.
    // Optional modules load first, so the C# backend can be handed every
    // native component the modules exposed to managed code: each module's
    // registered type names resolve to its native components, and the
    // C#-facing name is the Rust path with `::` replaced by `.` so a
    // `project_cs` mirror struct reproduces the same stable identity.
    // The generated C# mirror files must exist before the project build
    // compiles `project_cs`, so write them here (managed backend only), one
    // per optional module that exposes components, derived from each module's
    // real registered layout. Nothing is hand-written in the project.
    let mut module_exposed_components: Vec<ModuleExposedComponent> = Vec::new();
    if let ProjectModuleBackend::CSharp(_) = &module_config.backend {
        for slot in &optional_modules {
            let exposed: Vec<ModuleExposedComponent> = slot
                .exposed_component_names()
                .iter()
                .filter_map(|type_name| {
                    let component_id =
                        engine.world().resolve_component_id_by_name_any(type_name)?;
                    let (size, align) = engine.world().component_layout(component_id)?;
                    Some(ModuleExposedComponent {
                        csharp_name: type_name.replace("::", "."),
                        component_id,
                        size,
                        align,
                    })
                })
                .collect();
            crate::csharp::generate_module_components_csharp(
                &workspace_root,
                slot.name(),
                &exposed,
            )
            .map_err(|message| CSharpError::CodegenFailed { message })?;
            module_exposed_components.extend(exposed);
        }
    }
    let loaded_project = LoadedProject::start(
        &mut engine,
        &engine_api,
        &workspace_root,
        &module_config,
        &module_exposed_components,
    )?;

    let reload_generation = Arc::new(AtomicU64::new(0));
    spawn_source_watcher(
        workspace_root.clone(),
        &module_config.name,
        &module_config.watch_directory,
        Arc::clone(&reload_generation),
    )?;

    // Step 6: Snapshot host memory and print the startup analytics report.
    // Every module has been built, staged, loaded and initialized by now, so
    // the table carries the complete startup picture.
    analytics::record_host_memory();
    analytics::print_startup_report();

    println!();
    println!(
        "[host] Entering project loop. Edit {}/**/* to hot-reload.",
        module_config.watch_directory
    );
    println!();

    // Say how to drive rollback, once, next to where the fast path announces
    // itself - an interface nothing mentions is one nobody uses.
    #[cfg(feature = "hot_patch")]
    println!(
        "{} rollback: write `function@generation`, `function@previous` or `list` \
         to {}",
        crate::console::bold_cyan("[hot]"),
        crate::console::dim(ROLLBACK_REQUEST_FILE)
    );

    // Arm the per-function fast path. It reads the project's sources for
    // `#[pill_hot]` annotations and returns `None` when there is nothing to do,
    // so a project that has not opted in pays nothing.
    #[cfg(feature = "hot_patch")]
    let hot_patch = crate::hot_patch::HotPatchSession::new(
        &workspace_root,
        &module_config.name,
        &module_config.watch_directory,
        crate::build_runner::PROJECT_HOT_OUTPUT_SUBDIRECTORY,
        &module_config.build_command,
    );

    // The same fast path for each optional module. A module's `#[pill_hot_fn]`
    // functions are compiled into every artifact that links the crate, so this
    // is what lets an edit reach the project's embedded copy without the
    // cascading project reload a module swap would otherwise queue.
    #[cfg(feature = "hot_patch")]
    let module_hot_patch: Vec<Option<crate::hot_patch::HotPatchSession>> = host_config
        .optional_modules
        .iter()
        .map(|configuration| {
            crate::hot_patch::HotPatchSession::new(
                &workspace_root,
                &configuration.name,
                &configuration.watch_directory,
                &configuration.output_subdirectory,
                &configuration.build_command,
            )
        })
        .collect();

    let host = Host {
        workspace_root,
        module_config,
        engine,
        engine_api,
        loaded_project,
        optional_modules,
        reload_generation,
        last_processed_generation: 0,
        #[cfg(feature = "hot_patch")]
        hot_patch,
        #[cfg(feature = "hot_patch")]
        module_hot_patch,
        #[cfg(feature = "hot_patch")]
        loaded_patches: Vec::new(),
        last_frame_error: None,
        last_error_report: Instant::now(),
        suppressed_error_count: 0,
        frame_count: 0,
        last_report: Instant::now(),
        last_measured_fps: 0.0,
    };

    Ok(host)
}

/// Create the engine and initialize a statically linked project.
///
/// The shipping counterpart of the `hot_reload` [`setup`] above. Nothing is
/// built, watched or loaded: the project and its optional modules were linked
/// into this binary, and the frontend passes their entry points in.
///
/// # Errors
///
/// Returns a typed [`HostError`] when an optional module's or the project's
/// entry point reports a non-zero initialization status. There is no previous
/// generation to fall back to on this path, so the first failure is fatal.
#[cfg(not(feature = "hot_reload"))]
pub fn setup(project: StaticProject) -> Result<Host, HostError> {
    // Step 1: Construct the engine and its stable API table. Boxed first so the
    // raw pointer inside `EngineApi` stays valid if the caller moves the host.
    let mut engine = Box::new(Engine::new());

    info!(
        target: telemetry_target::ENGINE,
        module = project.name,
        modules = project.modules.len(),
        "ECS host starting (statically linked)"
    );

    // Step 2: Register every module, then the project, in the order and under
    // the owners the reloading path would have used.
    let managed_runtime = project.initialize(&mut engine)?;

    Ok(Host {
        engine,
        _managed_runtime: managed_runtime,
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
    project: impl Into<ProjectSource>,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, EngineError>
where
    W: RendererWindow + 'static,
{
    let host = setup(project.into())?;
    let renderer = Renderer::new(window, width, height)?;
    Ok(RenderingHost { host, renderer })
}

/// Complete rendering setup from an already-built [`Host`], attaching the
/// engine renderer to a supplied native window.
///
/// Frontends that must finish project setup (building and loading the project
/// module) before any window exists — for example the standalone runner — call
/// [`setup`] first and this function once a window is available, so a slow
/// first build never shows a blank surface.
///
/// # Errors
///
/// Returns a [`RendererError`] when surface or renderer creation fails.
#[cfg(feature = "rendering")]
pub fn attach_renderer<W>(
    host: Host,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, RendererError>
where
    W: RendererWindow + 'static,
{
    let renderer = Renderer::new(window, width, height)?;
    Ok(RenderingHost { host, renderer })
}

/// Process hot reloads, execute one scheduler frame, and update FPS tracking.
///
/// Returns a report roughly every three seconds for a frontend to print or
/// display; all other frames return `None`.
/// Drop every recorded prologue address, because an image was just replaced.
///
/// A prologue patch overwrote bytes inside a loaded artifact. When that artifact
/// is replaced, the recorded addresses point into an image the graveyard will
/// unmap, and writing saved bytes back to one is not a rollback - it is a write
/// into retired, possibly re-used memory.
///
/// Every session is cleared, not only the one whose artifact reloaded: patches
/// fan out across every loaded artifact, so a module's records include addresses
/// inside the project image and cannot be cleared selectively. A partial restore
/// would leave two copies of one function disagreeing, which is worse than none.
///
/// Idempotent, so calling it once after several reloads is the same as calling
/// it after each.
#[cfg(feature = "hot_patch")]
fn forget_prologue_records(host: &mut Host) {
    if let Some(session) = host.hot_patch.as_mut() {
        session.forget_prologue_patches();
    }
    for session in host.module_hot_patch.iter_mut().flatten() {
        session.forget_prologue_patches();
    }
}

/// See the `hot_patch` version above; without the feature nothing is recorded.
#[cfg(all(not(feature = "hot_patch"), feature = "hot_reload"))]
fn forget_prologue_records(_host: &mut Host) {}

/// Arm the thread that owns the frame boundary as the only one allowed to
/// rewrite live code.
///
/// Declared per frame rather than at setup because setup may run on a different
/// thread; idempotent, and a single relaxed load once declared.
#[cfg(feature = "hot_patch")]
fn arm_patching_thread() {
    pill_engine::hot_patch::declare_patching_thread();
}

/// See the `hot_patch` version above; without the feature nothing patches.
#[cfg(all(not(feature = "hot_patch"), feature = "hot_reload"))]
fn arm_patching_thread() {}

/// Try to deliver every pending optional-module edit by patching, not rebuilding.
///
/// A module's plain functions are compiled into every artifact that links the
/// crate, so one patch is offered to all of them at once. That is what makes the
/// cascading project reload a module swap normally queues unnecessary: the
/// project's embedded copy is redirected too.
///
/// Anything patching refuses falls straight through to the full rebuild the
/// frame loop performs next, so the worst case is the behaviour that existed
/// before the fast path.
///
/// A no-op without the `hot_patch` feature, so the frame loop reads the same in
/// both configurations rather than carrying a `cfg` of its own.
#[cfg(feature = "hot_patch")]
fn try_module_fast_path(host: &mut Host) {
    let Host {
        optional_modules,
        module_hot_patch,
        loaded_patches,
        loaded_project,
        engine,
        ..
    } = &mut *host;

    for index in 0..module_hot_patch.len() {
        // Captured before the patch runs: a save that lands while it compiles
        // advances the counter past this value and must stay pending, because
        // nothing has delivered it.
        let Some(pending) = optional_modules[index].pending_reload_generation() else {
            continue;
        };
        let Some(session) = module_hot_patch[index].as_mut() else {
            continue;
        };
        let targets = patch_targets(loaded_project, optional_modules);
        let outcome = session.try_patch(engine, &targets, loaded_patches);
        // The borrow of the module list ends here, so the slot below can be
        // updated.
        drop(targets);
        if report_patch_outcome(outcome) {
            optional_modules[index].consume_pending_reload(pending);
        }
    }
}

/// See the `hot_patch` version above; without the feature there is no fast path.
#[cfg(all(not(feature = "hot_patch"), feature = "hot_reload"))]
fn try_module_fast_path(_host: &mut Host) {}

/// Try to deliver a pending project edit by patching, not rebuilding.
///
/// This runs at a frame boundary: reloads are already processed here, before
/// `process_frame`, so no system is executing while a dispatch slot is written.
/// A successful patch consumes the pending generation, which is what skips the
/// full rebuild; anything refused falls through to it.
#[cfg(feature = "hot_patch")]
fn try_project_fast_path(host: &mut Host) {
    let pending = host.reload_generation.load(Ordering::Acquire);
    if pending == host.last_processed_generation {
        return;
    }

    // Disjoint field borrows, as the module path does.
    let Host {
        hot_patch,
        optional_modules,
        loaded_patches,
        loaded_project,
        engine,
        ..
    } = &mut *host;
    let mut patched = false;
    if let Some(session) = hot_patch {
        let targets = patch_targets(loaded_project, optional_modules);
        let outcome = session.try_patch(engine, &targets, loaded_patches);
        drop(targets);
        patched = report_patch_outcome(outcome);
    }
    if patched {
        // The edit is fully accounted for; skip the rebuild. Recorded as the
        // generation observed above rather than a fresh read: a save that
        // arrived while the patch compiled is a different edit that nothing has
        // delivered, and must stay pending.
        host.last_processed_generation = pending;
    }
}

/// See the `hot_patch` version above; without the feature there is no fast path.
#[cfg(all(not(feature = "hot_patch"), feature = "hot_reload"))]
fn try_project_fast_path(_host: &mut Host) {}

/// Re-sync the patch baselines of every subject a reload has just rebuilt.
///
/// `classify` decides "body-only" by diffing the file against a snapshot of
/// what is currently running, and that snapshot only advances when a patch
/// succeeds. A reload advances what is running without touching it, so an edit
/// the fast path refused stays in the diff forever: the next edit is compared
/// against a baseline that is now two edits stale, reports a change outside a
/// hot function body, and is refused for a change the reload already absorbed.
/// One unpatchable edit would otherwise turn patching off for the rest of the
/// run, which reads as "live patching stopped working".
///
/// Called after the reload rather than before it, so a save that lands during
/// the build is not folded into the baseline: that save has advanced the
/// generation counter, the next frame observes it, and it gets its own reload.
#[cfg(feature = "hot_patch")]
fn resync_patch_baselines(host: &mut Host, project: bool, modules: &[usize]) {
    if project {
        if let Some(session) = host.hot_patch.as_mut() {
            session.refresh_snapshots();
        }
    }
    for index in modules {
        if let Some(Some(session)) = host.module_hot_patch.get_mut(*index) {
            session.refresh_snapshots();
        }
    }
}

/// See the `hot_patch` version above; without the feature there is no baseline.
#[cfg(all(not(feature = "hot_patch"), feature = "hot_reload"))]
fn resync_patch_baselines(_host: &mut Host, _project: bool, _modules: &[usize]) {}

/// Run every reload step of one frame: module reloads, the per-function fast
/// paths, a pending project reload, and the analytics drain that reports them.
///
/// Separated from [`run_one_frame`] so the frame loop reads the same in both
/// build configurations. Without `hot_reload` there is nothing to reload, and
/// the no-op twin below compiles the whole sequence out.
#[cfg(feature = "hot_reload")]
fn run_reload_steps(host: &mut Host) {
    // Step 1: Reload any optional module whose sources changed. Each module
    // owns an independent generation counter and clears only its own systems,
    // so editing one module never rebuilds another and never disturbs the
    // project's systems, entities, or resources.
    // The reload transaction begins here so the analytics total line spans
    // the whole cascade (edited module + queued project reload), not just the
    // last transaction.
    let reload_started = Instant::now();

    // This is the thread that owns the frame boundary, and therefore the only
    // one allowed to rewrite live code.
    arm_patching_thread();

    // Step 2: Honour a rollback request, at the same frame boundary the
    // patch installs use - the prologue route rewrites live code, so this is a
    // requirement rather than a convenience.
    process_rollback_request(host);

    // Step 3: Try the per-function fast path for the optional modules, before
    // the reload below turns a pending change into a full module rebuild.
    try_module_fast_path(host);

    // Destructure so the module list, the engine and the API table are borrowed
    // as disjoint fields rather than through the whole host.
    let Host {
        optional_modules,
        engine,
        engine_api,
        workspace_root,
        reload_generation,
        module_config,
        ..
    } = &mut *host;
    let mut any_module_reloaded = false;
    // Which ones, not just whether any: each carries its own patch session, and
    // only the sessions whose sources were rebuilt need a new baseline.
    let mut reloaded_modules: Vec<usize> = Vec::new();
    for (index, slot) in optional_modules.iter_mut().enumerate() {
        if slot.reload_if_changed(engine, engine_api, workspace_root) {
            reloaded_modules.push(index);
            // The reloaded image is unpatched and every recorded prologue
            // address points into the previous one. Noted here and acted on
            // once the borrow below ends; forgetting is idempotent, so doing it
            // once after the loop is the same as doing it per reload.
            any_module_reloaded = true;
            info!(
                target: telemetry_target::HOT_RELOAD,
                module = slot.name(),
                "optional module reload processed"
            );
            // A module the project links directly is compiled into the project
            // DLL as well as its own DLL, so after the module swaps, the
            // project still runs the old embedded copy of that crate. Queue a
            // project reload so the new code reaches the project too; the
            // existing transaction below handles build, rollback, and schema
            // migration. The check is cheap: one small manifest read.
            if project_depends_on_crate(workspace_root, module_config, slot.name()) {
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    module = slot.name(),
                    "module is a direct dependency of the project; queuing a project reload"
                );
                reload_generation.fetch_add(1, Ordering::Release);
            }
        }
    }

    // The borrow above has ended, so the records a module reload invalidated
    // can be dropped now, and the rebuilt sources become the new patch baseline.
    if any_module_reloaded {
        forget_prologue_records(host);
        resync_patch_baselines(host, false, &reloaded_modules);
    }

    // Step 4: Try the per-function fast path for the project, before the
    // reload below turns a pending change into a full rebuild.
    try_project_fast_path(host);

    // Step 5: Process a pending project reload before running systems.
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

        // The project image is about to be replaced, and the graveyard unmaps
        // it two generations later, so every recorded prologue address is now
        // stale for the same reason a module reload makes them stale.
        forget_prologue_records(host);

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
        // The baseline the reload ran against, not a fresh read. A save during
        // the build advances the counter past it and cancels the compilation
        // above; recording the newer value would mark that save as handled when
        // the build it cancelled produced nothing, stranding the edit on disk.
        // Recording the baseline is what lets the next frame observe it and
        // rebuild - which is what the cancellation is for.
        host.last_processed_generation = generation;

        // The project now runs the sources on disk, so the patch classifier's
        // baseline has to say so too. Skipping this is what makes one refused
        // patch disable the fast path for the rest of the session.
        resync_patch_baselines(host, true, &[]);
    }

    // Print the analytics line for every reload completed this frame (optional
    // modules from Step 0, the project from Step 1), plus one aggregate total.
    // The events were recorded with their build/stage/load/init/migrate
    // breakdowns already populated, so this is a pure drain-and-print.
    analytics::print_reload_events(reload_started);
}

/// See the `hot_reload` version above; a statically linked build reloads
/// nothing, so there is no work and no analytics to drain.
#[cfg(not(feature = "hot_reload"))]
fn run_reload_steps(_host: &mut Host) {}

pub fn run_one_frame(host: &mut Host) -> Option<FrameReport> {
    #[cfg(feature = "metrics")]
    let frame_start = Instant::now();

    // Steps 1 to 5: everything reloading does, in one call so this loop is
    // identical whether or not the machinery is compiled in.
    run_reload_steps(host);

    // Step 6: Poll the managed loader for an assembly swap.
    // The managed loader watches the built assembly instead of source files.
    // Only a reloading build has a managed loader to poll.
    #[cfg(feature = "hot_reload")]
    host.loaded_project.poll_managed_reload();

    // Step 7: Execute one scheduler frame and report its failures.
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

    // Step 8: Invoke the native compatibility update after scheduler systems.
    // Managed games run entirely as scheduler systems. Native games retain
    // this compatibility update hook after their scheduled work.
    //
    // Both hooks are optional DLL exports (`project_update`,
    // `pill_module_update`) that the attribute macros do not generate, so a
    // statically linked build has nothing to call: its gameplay runs entirely
    // through scheduler systems, which Step 7 already ran.
    #[cfg(feature = "hot_reload")]
    {
        host.loaded_project.update(&host.engine_api);

        // Optional modules may also export a per-frame hook. Run them after the
        // project so a module observes the world the project's systems produced.
        for slot in &host.optional_modules {
            slot.update(&host.engine_api);
        }
    }

    // Step 9: Track and report FPS over the three-second window.
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

/// File a developer or a script drops to drive live patching, relative to the
/// workspace root.
///
/// A file rather than an environment variable because the request has to reach a
/// process that is already running, and rather than a console command because
/// the standalone host has no input loop and its stdout is routinely redirected.
///
/// One line, either:
///
/// - `list` - print every generation this session has installed
/// - `<function>@<generation>` - reinstall that generation
/// - `<function>@previous` - step back one from whatever is running
/// - `<function>@0` - the code the artifact was built with
///
/// It is deleted as soon as it is read, so a request is honoured exactly once.
#[cfg(feature = "hot_patch")]
const ROLLBACK_REQUEST_FILE: &str = "target/hot/rollback.request";

/// Honour a pending request, if one was dropped.
///
/// Called at the frame boundary, where no system is executing - the same
/// guarantee a patch install relies on, and a requirement rather than a
/// convenience for the prologue route, which rewrites live code.
#[cfg(feature = "hot_patch")]
fn process_rollback_request(host: &mut Host) {
    let request_path = host.workspace_root.join(ROLLBACK_REQUEST_FILE);
    let Ok(request) = std::fs::read_to_string(&request_path) else {
        return;
    };
    let request = request.trim();

    // An empty read is almost always a partial write: this runs every frame, so
    // it routinely observes the file between creation and the writer flushing.
    // Leave it and look again next frame rather than reporting nonsense.
    if request.is_empty() {
        return;
    }

    // Removed before acting, so a request that fails is not retried on every
    // subsequent frame - and so a malformed one is reported exactly once.
    let request = request.to_string();
    let _ = std::fs::remove_file(&request_path);

    if request.eq_ignore_ascii_case("list") {
        print_patch_generations(host);
        return;
    }

    let Some((function, wanted)) = request.rsplit_once('@') else {
        eprintln!(
            "{} request `{request}` is malformed; expected `function@generation`, \
             `function@previous`, or `list`",
            crate::console::bold_cyan("[hot]")
        );
        return;
    };
    let function = function.trim();
    let wanted = wanted.trim();

    // `previous` saves a developer looking up numbers to undo the last edit,
    // which is what a rollback is wanted for almost every time.
    let generation = if wanted.eq_ignore_ascii_case("previous") {
        match host
            .patch_generations()
            .iter()
            .filter(|generation| generation.function == function)
            .map(|generation| generation.number)
            .max()
        {
            Some(newest) => newest.saturating_sub(1),
            None => {
                eprintln!(
                    "{} `{function}` has no recorded generations",
                    crate::console::bold_cyan("[hot]")
                );
                print_patch_generations(host);
                return;
            }
        }
    } else {
        match wanted.parse::<u32>() {
            Ok(generation) => generation,
            Err(_) => {
                eprintln!(
                    "{} `{wanted}` is not a generation number or `previous`",
                    crate::console::bold_cyan("[hot]")
                );
                return;
            }
        }
    };

    if let Err(detail) = host.rollback_patch(function, generation) {
        eprintln!(
            "{} rollback of `{function}` to generation {generation} failed\n      {detail}",
            crate::console::bold_cyan("[hot]")
        );
        // A rollback usually fails because the generation does not exist, so
        // show what does rather than making the developer guess.
        print_patch_generations(host);
    }
}

/// See the `hot_patch` version above; without the feature there is nothing to
/// roll back to.
#[cfg(all(not(feature = "hot_patch"), feature = "hot_reload"))]
fn process_rollback_request(_host: &mut Host) {}

/// Print every generation this session has installed.
///
/// Rollback is unusable without it: the request needs a number, and nothing
/// else in the host ever reports which numbers exist.
#[cfg(feature = "hot_patch")]
fn print_patch_generations(host: &Host) {
    let generations = host.patch_generations();
    if generations.is_empty() {
        println!(
            "{} no patch generations yet; edit a function body to create one",
            crate::console::bold_cyan("[hot]")
        );
        return;
    }
    println!(
        "{} patch generations ({} total)",
        crate::console::bold_cyan("[hot]"),
        generations.len()
    );
    for generation in &generations {
        println!(
            "      {:<48} generation {:<3} {}",
            generation.function,
            generation.number,
            crate::console::dim(&format!("{:.0}s ago", generation.age_seconds))
        );
    }
    println!(
        "      {}",
        crate::console::dim("generation 0 is the code each artifact was built with")
    );
}

/// Every loaded artifact a plain-function patch must be offered to.
///
/// One entry per currently loaded library: the project and each optional
/// module. A crate linked into several of them is compiled into each, so each
/// holds an independent redirect slot for the same function and all of them
/// have to be told about the replacement.
///
/// Retired generations are deliberately left out. They stay mapped only so
/// outstanding pointers into their code remain valid, and their systems have
/// already been cleared, so nothing calls their copy of the function.
#[cfg(feature = "hot_patch")]
fn patch_targets<'a>(
    loaded_project: &'a LoadedProject,
    optional_modules: &'a [OptionalModuleSlot],
) -> Vec<(&'a str, &'a crate::native_library::NativeLibrary)> {
    let mut targets = Vec::with_capacity(optional_modules.len() + 1);
    if let Some(library) = loaded_project.native_library() {
        targets.push(("project", library));
    }
    for slot in optional_modules {
        targets.push((slot.name(), slot.current_library()));
    }
    targets
}

/// Report one patch attempt, and say whether it fully handled the change.
///
/// `true` means the edit is live and the pending reload has nothing left to do.
/// Every other outcome falls through to the normal rebuild, so the worst case
/// is exactly the behaviour that existed before the fast path.
#[cfg(feature = "hot_patch")]
fn report_patch_outcome(outcome: crate::hot_patch::PatchOutcome) -> bool {
    match outcome {
        crate::hot_patch::PatchOutcome::Patched {
            function,
            generation,
            elapsed_milliseconds,
            stages,
            artifact_bytes,
            exports,
            routes,
            copies,
        } => {
            // How the replacement was delivered, said out loud rather than left
            // to the analytics line. A slot route is provable - the install is
            // one atomic pointer store and every call that reaches the
            // dispatcher runs the new code. The prologue route cannot make that
            // promise: it overwrites a live function's first bytes, so a caller
            // that inlined the body still runs the old one. A developer
            // watching a change not take effect needs to know which they got.
            let delivery = routes
                .iter()
                .map(|route| route.label())
                .collect::<Vec<_>>()
                .join("+");
            let best_effort = !routes.iter().all(|route| route.is_provable());
            let note = format!(
                "(generation {generation} via {delivery}, {copies} {}{})",
                if copies == 1 { "copy" } else { "copies" },
                if best_effort { " - best effort" } else { "" }
            );
            println!(
                "{} {function} {} {}\n      {}",
                crate::console::bold_cyan("[hot]"),
                crate::console::green(&format!("LIVE {elapsed_milliseconds:.0} ms")),
                if best_effort {
                    crate::console::yellow(&note)
                } else {
                    crate::console::dim(&note)
                },
                crate::console::dim(&stages.to_string())
            );
            info!(
                target: telemetry_target::HOT_RELOAD,
                function = function.as_str(),
                generation,
                elapsed_milliseconds,
                "per-function patch applied"
            );
            // Recorded in the same shape a module reload is, so one parser in
            // `devops/benchmarks/hot_reload_harness.py` reads both.
            analytics::record_patch(
                &function,
                generation,
                stages.classify + stages.generate + stages.flags,
                stages.compile as u64,
                stages.load,
                stages.activate,
                artifact_bytes,
                exports,
                &routes,
                copies,
            );
            true
        }
        crate::hot_patch::PatchOutcome::NotPatchable { refusal } => {
            println!(
                "{} {} {}\n      reason: {}\n      falling back to module reload",
                crate::console::bold_cyan("[hot]"),
                crate::console::yellow("FAST PATCH NOT POSSIBLE"),
                crate::console::dim(&format!("({})", refusal.code)),
                refusal.detail
            );
            info!(
                target: telemetry_target::HOT_RELOAD,
                code = refusal.code,
                detail = refusal.detail.as_str(),
                "fast patch refused; falling back to a reload"
            );
            analytics::record_patch_refusal();
            false
        }
        crate::hot_patch::PatchOutcome::Failed {
            function,
            active_generation,
            failure,
        } => {
            // The running implementation is intact, so the message says what is
            // still executing rather than only what did not happen.
            eprintln!(
                "{} {function} patch failed {}\n      {}\n      \
                 still running generation {active_generation}; falling back to module reload",
                crate::console::bold_cyan("[hot]"),
                crate::console::dim(&format!("({})", failure.code)),
                failure.detail
            );
            pill_core::warn!(
                target: telemetry_target::HOT_RELOAD,
                function = function.as_str(),
                code = failure.code,
                detail = failure.detail.as_str(),
                active_generation,
                "fast patch failed; the previous implementation is still running"
            );
            analytics::record_patch_failure();
            false
        }
        crate::hot_patch::PatchOutcome::Unchanged => false,
    }
}

/// Record one frame's numerical state into the shared metrics recorder.
#[cfg(feature = "metrics")]
fn record_frame_metrics(entity_count: usize, frame_time_ms: f64, fps: f64) {
    metrics::gauge!("ecs.entities").set(entity_count as f64);
    metrics::histogram!("engine.frame_time_ms").record(frame_time_ms);
    metrics::gauge!("engine.fps").set(fps);
}

#[cfg(feature = "hot_reload")]
/// Print the selected backend before any build output starts streaming.
fn print_startup_configuration(workspace_root: &Path, module_config: &ProjectModuleConfig) {
    info!(
        target: telemetry_target::ENGINE,
        workspace = %workspace_root.display(),
        module = module_config.name.as_str(),
        backend = ?module_config.backend,
        build_command = %module_config.build_command.join(" "),
        watch_directory = module_config.watch_directory.as_str(),
        "ECS host starting"
    );
}
