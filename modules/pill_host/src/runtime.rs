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
use pill_core::error::{CSharpError, EngineMessage, HostError};
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
#[cfg(feature = "rendering")]
use pill_engine::EngineError;
use pill_engine::{Engine, EngineApi};
#[cfg(feature = "rendering")]
use pill_engine::{RenderViewport, Renderer, RendererError, RendererWindow, VirtualResolution};

// Current crate
use crate::analytics;
use crate::config::project_depends_on_crate;
use crate::csharp::ModuleExposedComponent;
use crate::native_library::cleanup_temporary_files;
use crate::optional_module::OptionalModuleSlot;
use crate::project_module::LoadedProject;
use crate::watcher::spawn_source_watcher;
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
    workspace_root: PathBuf,
    module_config: ProjectModuleConfig,
    // Boxed before EngineApi is created so its raw engine pointer remains
    // stable even if Host is moved by a caller.
    engine: Box<Engine>,
    engine_api: EngineApi,
    loaded_project: LoadedProject,
    /// Optional modules, each with its own watcher and reload transaction.
    optional_modules: Vec<OptionalModuleSlot>,
    reload_generation: Arc<AtomicU64>,
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
            .ok_or_else(|| {
                format!("`{function}` has not been patched in this session")
            })?;

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

// =============================================================================
// Free Functions
// =============================================================================

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
    host_config: impl Into<HostConfig>,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, EngineError>
where
    W: RendererWindow + 'static,
{
    let host = setup(host_config)?;
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
pub fn run_one_frame(host: &mut Host) -> Option<FrameReport> {
    #[cfg(feature = "metrics")]
    let frame_start = Instant::now();

    // Step 0: Reload any optional module whose sources changed. Each module
    // owns an independent generation counter and clears only its own systems,
    // so editing one module never rebuilds another and never disturbs the
    // project's systems, entities, or resources.
    // The reload transaction begins here so the analytics total line spans
    // the whole cascade (edited module + queued project reload), not just the
    // last transaction.
    let reload_started = Instant::now();

    // This is the thread that owns the frame boundary, and therefore the only
    // one allowed to rewrite live code. Declared here rather than at setup
    // because setup may run elsewhere; idempotent and a single relaxed load
    // once declared.
    #[cfg(feature = "hot_patch")]
    pill_engine::hot_patch::declare_patching_thread();

    // Step 0a2: Honour a rollback request, at the same frame boundary the
    // patch installs use - the prologue route rewrites live code, so this is a
    // requirement rather than a convenience.
    #[cfg(feature = "hot_patch")]
    process_rollback_request(host);

    // Step 0b: Try the per-function fast path for the optional modules, before
    // the loop below turns a pending change into a full module rebuild.
    //
    // A module's plain functions are compiled into every artifact that links
    // the crate, so one patch is offered to all of them at once. That is what
    // makes the cascading project reload a module swap normally queues
    // unnecessary: the project's embedded copy is redirected too.
    #[cfg(feature = "hot_patch")]
    {
        let Host {
            optional_modules,
            module_hot_patch,
            loaded_patches,
            loaded_project,
            engine,
            ..
        } = &mut *host;

        for index in 0..module_hot_patch.len() {
            if !optional_modules[index].has_pending_reload() {
                continue;
            }
            let Some(session) = module_hot_patch[index].as_mut() else {
                continue;
            };
            let targets = patch_targets(loaded_project, optional_modules);
            let outcome = session.try_patch(engine, &targets, loaded_patches);
            // The borrow of the module list ends here, so the slot below can be
            // updated.
            drop(targets);
            if report_patch_outcome(outcome) {
                optional_modules[index].consume_pending_reload();
            }
        }
    }

    // Destructure so the module list, the engine and the API table are borrowed
    // as disjoint fields rather than through the whole host.
    let Host {
        optional_modules,
        engine,
        engine_api,
        workspace_root,
        reload_generation,
        module_config,
        #[cfg(feature = "hot_patch")]
        hot_patch,
        #[cfg(feature = "hot_patch")]
        module_hot_patch,
        ..
    } = &mut *host;
    for slot in optional_modules.iter_mut() {
        if slot.reload_if_changed(engine, engine_api, workspace_root) {
            // The reloaded image is unpatched and the recorded addresses point
            // into the previous one, so every prologue record is now stale.
            #[cfg(feature = "hot_patch")]
            {
                if let Some(session) = hot_patch.as_mut() {
                    session.forget_prologue_patches();
                }
                for session in module_hot_patch.iter_mut().flatten() {
                    session.forget_prologue_patches();
                }
            }
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

    // Step 0c: Try the per-function fast path before the reload transaction.
    //
    // This is a frame boundary: reloads are already processed here, before
    // `process_frame`, so no system is executing while a dispatch slot is
    // written. A successful patch consumes the pending generation, which is what
    // skips the full rebuild below; anything it refuses falls straight through
    // to that rebuild, so the worst case is the behavior that existed before.
    #[cfg(feature = "hot_patch")]
    {
        let pending = host.reload_generation.load(Ordering::Acquire);
        if pending != host.last_processed_generation {
            // Disjoint field borrows, as Step 0 above does.
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
                // The edit is fully accounted for; skip the rebuild.
                host.last_processed_generation = host.reload_generation.load(Ordering::Acquire);
            }
        }
    }

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

        // The project image is about to be replaced, and the graveyard unmaps it
        // two generations later. Every recorded prologue address points into the
        // image being retired, so the records are dropped here for the same
        // reason the module path drops them: writing saved bytes back to a
        // retired - or worse, re-used - address is not a rollback.
        #[cfg(feature = "hot_patch")]
        {
            if let Some(session) = host.hot_patch.as_mut() {
                session.forget_prologue_patches();
            }
            for session in host.module_hot_patch.iter_mut().flatten() {
                session.forget_prologue_patches();
            }
        }

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

    // Print the analytics line for every reload completed this frame (optional
    // modules from Step 0, the project from Step 1), plus one aggregate total.
    // The events were recorded with their build/stage/load/init/migrate
    // breakdowns already populated, so this is a pure drain-and-print.
    analytics::print_reload_events(reload_started);

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

    // Optional modules may also export a per-frame hook. Run them after the
    // project so a module observes the world the project's systems produced.
    for slot in &host.optional_modules {
        slot.update(&host.engine_api);
    }

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
