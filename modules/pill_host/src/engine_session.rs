//! Host-side ownership of the engine runtime and its reload transactions.
//!
//! # Responsibilities
//!
//! - Build, load, and create the engine runtime generation the host drives.
//! - Step one frame per [`EngineSession::run_one_frame`] call.
//! - Run the project reload and the engine reload transactions at a quiescent
//!   point between frames, with a defined rollback for every failure.
//! - Re-apply the window, viewport, and projection a swap would otherwise lose.
//!
//! # Design
//!
//! This is what remains of the old `pill_host::runtime` once the engine moved
//! into a dynamic library: orchestration without any engine state. The session
//! holds no world, no renderer, and no project module - only the handle to the
//! generation that owns them, the signals that ask for a swap, and the
//! presentation settings a new generation must be told about.
//!
//! ## Ordering of an engine reload
//!
//! The transaction is ordered so that every failure that *can* be detected
//! before the running generation is torn down actually is:
//!
//! 1. build the runtime, cancelling if newer engine sources land mid-build;
//! 2. capture the world from the running generation;
//! 3. stage and map the new dylib and validate its ABI - all still side by
//!    side with the running generation, because none of this touches the GPU;
//! 4. destroy the running generation, keeping its module mapped;
//! 5. create the new generation and restore the captured world into it;
//! 6. retire the previous module only once the replacement is fully live.
//!
//! A failure in steps 1 to 3 leaves the running generation completely
//! untouched. A failure in step 5 rebuilds the previous generation from the
//! module still held from step 4 and restores the same captured world into it,
//! so a single bad build can never cost the session its state.
//!
//! All of this happens on the host's main thread between frames, never while a
//! boundary call is in flight, which is what makes unmapping a retired module
//! safe at all.

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

// External crates
use pill_core::error::{BuildError, HostError, RuntimeError};
use pill_core::hot_reload::{
    cleanup_stale_temporary_files, native_library_extension, runtime_staged_file_name,
    runtime_staging_directory,
};
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info, warn};
use pill_runtime_api::loader::LoadedRuntimeModule;
use pill_runtime_api::{FrameReport, PillWindowHandleV1, RenderViewport, VirtualResolution};

// Current crate
use crate::build_runner::{
    build_engine_runtime, build_project_module, engine_runtime_artifact_path,
    project_module_artifact_path,
};
use crate::config::ProjectModuleConfig;
use crate::runtime_client::{
    build_create_context, CapturedWorld, RuntimeClient, RuntimeCreateContext, RuntimeGraveyard,
};
use crate::watcher::{spawn_all_watchers, ReloadSignals};

// =============================================================================
// Types
// =============================================================================

/// What asked for an engine reload, and therefore how one is obtained.
enum EngineReloadTrigger {
    /// Engine sources changed, so the runtime must be rebuilt.
    SourcesChanged,
    /// Another process staged a newer runtime, which is adopted as produced.
    ExternallyStaged {
        /// The staged dynamic library to map.
        staged_path: PathBuf,
    },
}

/// A generation that could not be brought up, handed back for retirement.
///
/// Boxed because it travels in an error variant that only the rare failure
/// path constructs: keeping the success path's `Result` small matters more
/// than avoiding one allocation on a swap that already failed.
struct FailedGeneration {
    /// The client whose module still has to be retired.
    client: RuntimeClient,
    /// Why the generation could not be brought up.
    error: RuntimeError,
}

/// Where the replacement dynamic library for one swap comes from.
enum ReplacementSource {
    /// A freshly built artifact that still has to be staged.
    Built(PathBuf),
    /// An already-staged artifact that is mapped as it is.
    Staged(PathBuf),
}

// =============================================================================
// EngineSession
// =============================================================================

/// The host's complete view of one running engine, across reloads.
pub struct EngineSession {
    /// Absolute workspace root every build and staging path is relative to.
    workspace_root: PathBuf,
    /// How the project module is built, watched, and loaded.
    module_config: ProjectModuleConfig,
    /// Reload signals published by the watcher threads.
    signals: ReloadSignals,
    /// Project reload generation already processed.
    last_project_generation: u64,
    /// Engine reload generation already processed.
    last_engine_generation: u64,
    /// Shared-core generation already reported as needing a restart.
    last_shared_core_generation: u64,
    /// Highest staged runtime index this host has produced or adopted.
    last_staged_generation: u64,
    /// Index the next staged runtime dylib will be written under.
    next_staging_generation: u64,
    /// The live generation, absent only if every rollback path failed.
    client: Option<RuntimeClient>,
    /// Retired modules kept mapped until nothing can reach their code.
    graveyard: RuntimeGraveyard,
    /// Arguments every generation is created with, including after a swap.
    create_context: RuntimeCreateContext,
    /// Physical viewport override to re-apply after a swap.
    viewport: Option<RenderViewport>,
    /// Logical projection to re-apply after a swap.
    virtual_resolution: Option<VirtualResolution>,
}

impl EngineSession {
    /// Build the engine runtime and the project, then bring up the first
    /// generation and start every watcher.
    ///
    /// The runtime is created headless even in a windowed host: the first
    /// build can take tens of seconds, and creating the window only once the
    /// world is live is what keeps a blank surface from ever being shown.
    /// [`Self::attach_window`] binds the surface afterwards.
    ///
    /// # Errors
    ///
    /// Returns a typed [`HostError`] naming the failing subsystem:
    /// configuration, build, runtime loading, watcher startup, or project
    /// initialization inside the runtime.
    pub fn start(module_config: ProjectModuleConfig) -> Result<Self, HostError> {
        // Step 1: Reject inconsistent configurations before any build or load.
        module_config.validate()?;

        // Step 2: Resolve the workspace root and print the configuration.
        let workspace_root = crate::workspace_root()?;
        print_startup_configuration(&workspace_root, &module_config);

        // Clearing stale temporary copies must happen before anything is
        // staged, because it also clears this process's own directory.
        cleanup_stale_temporary_files(&workspace_root);

        // Step 3: Build the reloadable engine runtime and the project module.
        let built_runtime_path = build_engine_runtime(&workspace_root, None)?;
        let project_module_path = build_project_module(&workspace_root, &module_config, None)?;

        // Step 4: Start the watchers before the first frame so an edit made
        // during startup is still observed.
        let signals = spawn_all_watchers(&workspace_root, &module_config)?;
        let adopted_staging_generation = signals.staged_runtime.load(Ordering::Acquire);

        // Step 5: Map the runtime and create the first generation.
        let create_context =
            build_create_context(&workspace_root, &module_config, Some(&project_module_path))?;
        let staging_generation = adopted_staging_generation + 1;
        let mut client =
            RuntimeClient::map(&built_runtime_path, &workspace_root, staging_generation)?;
        client.create(&create_context)?;

        println!();
        println!(
            "[host] Entering project loop. Edit {}/**/* to hot-reload the project, or the engine sources to hot-reload the engine.",
            module_config.watch_directory
        );
        println!();

        Ok(Self {
            workspace_root,
            module_config,
            last_project_generation: signals.project.load(Ordering::Acquire),
            last_engine_generation: signals.engine.load(Ordering::Acquire),
            last_shared_core_generation: signals.shared_core.load(Ordering::Acquire),
            last_staged_generation: staging_generation,
            next_staging_generation: staging_generation + 1,
            signals,
            client: Some(client),
            graveyard: RuntimeGraveyard::default(),
            create_context,
            viewport: None,
            virtual_resolution: None,
        })
    }

    /// Bind the live generation's renderer to a native window.
    ///
    /// The descriptor is retained so every later generation is created against
    /// the same window without the frontend having to re-attach it.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] when no generation is loaded or the surface
    /// cannot be created.
    pub fn attach_window(
        &mut self,
        window: PillWindowHandleV1,
        width: u32,
        height: u32,
    ) -> Result<(), HostError> {
        self.create_context.window = window;
        self.create_context.width = width;
        self.create_context.height = height;
        self.client_mut()?
            .retarget_render_window(&window, width, height)?;
        self.reapply_presentation_settings();
        Ok(())
    }

    /// Forward a physical window resize to the live generation.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.create_context.width = width;
        self.create_context.height = height;
        if let Some(client) = self.client.as_mut() {
            client.resize(width, height);
        }
    }

    /// Restrict engine drawing to a physical region of the native surface.
    ///
    /// `None` restores full-surface rendering. The setting is retained and
    /// re-applied after an engine swap, which rebuilds the renderer.
    pub fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.viewport = viewport;
        if let Some(client) = self.client.as_mut() {
            if let Err(error) = client.set_viewport(viewport) {
                warn!(
                    target: telemetry_target::RENDERING,
                    error = %error,
                    "the engine runtime rejected a viewport change"
                );
            }
        }
    }

    /// Map a stable project coordinate space into the current viewport.
    ///
    /// `None` makes logical units match physical pixels again. The setting is
    /// retained and re-applied after an engine swap.
    pub fn set_render_virtual_resolution(&mut self, resolution: Option<VirtualResolution>) {
        self.virtual_resolution = resolution;
        if let Some(client) = self.client.as_mut() {
            if let Err(error) = client.set_virtual_resolution(resolution) {
                warn!(
                    target: telemetry_target::RENDERING,
                    error = %error,
                    "the engine runtime rejected a virtual resolution change"
                );
            }
        }
    }

    /// Move rendering to a different native window, such as a detached editor
    /// scene window.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] when no generation is loaded or the new surface
    /// cannot be created; the current surface is kept in that case.
    pub fn retarget_render_window(
        &mut self,
        window: PillWindowHandleV1,
        width: u32,
        height: u32,
    ) -> Result<(), HostError> {
        self.client_mut()?
            .retarget_render_window(&window, width, height)?;
        // Only a successful retarget updates the retained descriptor, so a
        // failed move cannot make the next generation bind the wrong window.
        self.create_context.window = window;
        self.create_context.width = width;
        self.create_context.height = height;
        self.reapply_presentation_settings();
        Ok(())
    }

    /// Read live frame statistics for UI overlays.
    ///
    /// Returns zeroed statistics when no generation is loaded, so an overlay
    /// keeps rendering while the host reports the failure through its frame
    /// loop instead.
    pub fn current_frame_report(&self) -> FrameReport {
        self.client
            .as_ref()
            .map(RuntimeClient::current_frame_report)
            .unwrap_or_default()
    }

    /// Whether the live generation asked the host to stop the frame loop.
    pub fn is_exit_requested(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(RuntimeClient::is_exit_requested)
    }

    /// Process pending reloads and execute one frame.
    ///
    /// Returns the periodic console report roughly every three seconds; all
    /// other frames return `None`.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] when no generation is loaded or the frame fails
    /// fatally, which for a windowed host means presentation is no longer
    /// possible.
    pub fn run_one_frame(&mut self) -> Result<Option<FrameReport>, HostError> {
        // Step 1: Report a shared-core change once. Both the host binary and
        // the runtime link `pill_core`, so a change there invalidates the
        // running process itself and cannot be hot reloaded.
        self.report_shared_core_changes();

        // Step 2: An engine change supersedes a project change: the engine
        // swap reloads the project inside the new generation anyway.
        if let Some(trigger) = self.take_engine_reload_request() {
            self.reload_engine(trigger);
        } else if self.take_project_reload_request() {
            self.reload_project();
        }

        // Step 3: Run the frame and take any report it produced.
        let client = self.client_mut()?;
        client.run_one_frame()?;
        Ok(client.take_frame_report())
    }

    /// Borrow the live generation, or report that none is loaded.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::NotLoaded`] once every rollback path has been
    /// exhausted, which the frame loop treats as fatal.
    fn client_mut(&mut self) -> Result<&mut RuntimeClient, HostError> {
        self.client
            .as_mut()
            .ok_or_else(|| HostError::from(RuntimeError::NotLoaded))
    }

    /// Log a restart notice for each new shared-core change.
    fn report_shared_core_changes(&mut self) {
        let generation = self.signals.shared_core.load(Ordering::Acquire);
        if generation == self.last_shared_core_generation {
            return;
        }
        self.last_shared_core_generation = generation;
        warn!(
            target: telemetry_target::HOT_RELOAD,
            "pill_core changed; both the host and the engine runtime link it, so this host must be restarted to pick the change up"
        );
    }

    /// Whether a project reload is pending, consuming the signal if so.
    fn take_project_reload_request(&mut self) -> bool {
        let generation = self.signals.project.load(Ordering::Acquire);
        if generation == self.last_project_generation {
            return false;
        }
        self.last_project_generation = generation;
        true
    }

    /// Whether an engine reload is pending, and what triggered it.
    ///
    /// Two independent inputs can ask for one: an edit to the engine sources,
    /// which requires a rebuild, and a staged dylib with a higher index than
    /// this host produced, which means another process already built one.
    /// Consuming both signals here is what keeps the host from reacting to its
    /// own staged artifact on the next frame.
    fn take_engine_reload_request(&mut self) -> Option<EngineReloadTrigger> {
        let engine_generation = self.signals.engine.load(Ordering::Acquire);
        let staged_generation = self.signals.staged_runtime.load(Ordering::Acquire);

        let sources_changed = engine_generation != self.last_engine_generation;
        let external_build_staged = staged_generation > self.last_staged_generation;
        if !sources_changed && !external_build_staged {
            return None;
        }

        self.last_engine_generation = engine_generation;
        if !external_build_staged {
            return Some(EngineReloadTrigger::SourcesChanged);
        }

        self.last_staged_generation = staged_generation;
        self.next_staging_generation = self.next_staging_generation.max(staged_generation + 1);

        // A source edit wins over an adoption: the external artifact predates
        // the edit, so rebuilding is the only way to honour it.
        if sources_changed {
            return Some(EngineReloadTrigger::SourcesChanged);
        }

        let staged_path = runtime_staging_directory(&self.workspace_root).join(
            runtime_staged_file_name(staged_generation, native_library_extension()),
        );
        info!(
            target: telemetry_target::HOT_RELOAD,
            staged_generation,
            path = %staged_path.display(),
            "another process staged a newer engine runtime; adopting it without rebuilding"
        );
        Some(EngineReloadTrigger::ExternallyStaged { staged_path })
    }

    /// Rebuild the project module and swap it inside the live generation.
    ///
    /// A failed build or a failed generation is handled without ever losing
    /// the working one: the build error keeps the current module, and the
    /// runtime rolls its own project generation back internally.
    fn reload_project(&mut self) {
        let baseline = self.last_project_generation;
        let built = build_project_module(
            &self.workspace_root,
            &self.module_config,
            // A save during the build advances the generation beyond this
            // baseline, which cancels the in-flight compilation; the next
            // frame observes the newer generation and rebuilds.
            Some((&self.signals.project, baseline)),
        );

        let module_path = match built {
            Ok(path) => path,
            Err(BuildError::Cancelled) => {
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    "project build cancelled by a newer save; retrying on the next frame"
                );
                // Re-arm so the newer sources are picked up rather than
                // treated as already processed.
                self.last_project_generation = baseline;
                return;
            }
            Err(error) => {
                error!(
                    target: telemetry_target::HOT_RELOAD,
                    error = %error,
                    "project build failed; keeping the old project module"
                );
                return;
            }
        };

        let Some(client) = self.client.as_mut() else {
            return;
        };
        if let Err(error) = client.reload_project(Some(&module_path)) {
            error!(
                target: telemetry_target::HOT_RELOAD,
                error = %error,
                "the engine runtime rejected the rebuilt project module"
            );
        }
    }

    /// Rebuild or adopt an engine runtime and swap the whole generation.
    ///
    /// The world is captured before the swap and restored into the replacement
    /// generation, so entities, persistable components, and persistable
    /// resources survive an engine edit.
    fn reload_engine(&mut self, trigger: EngineReloadTrigger) {
        let started_at = Instant::now();
        info!(
            target: telemetry_target::HOT_RELOAD,
            "engine hot reload triggered"
        );

        // Step 1: Obtain the replacement dynamic library, either by building it
        // or by adopting one another process already staged.
        let replacement_source = match trigger {
            EngineReloadTrigger::ExternallyStaged { staged_path } => {
                ReplacementSource::Staged(staged_path)
            }
            EngineReloadTrigger::SourcesChanged => {
                let baseline = self.last_engine_generation;
                match build_engine_runtime(
                    &self.workspace_root,
                    // A save during the build advances the generation beyond
                    // this baseline, cancelling the in-flight compilation.
                    Some((&self.signals.engine, baseline)),
                ) {
                    Ok(path) => ReplacementSource::Built(path),
                    Err(BuildError::Cancelled) => {
                        info!(
                            target: telemetry_target::HOT_RELOAD,
                            "engine build cancelled by a newer save; retrying on the next frame"
                        );
                        self.last_engine_generation = baseline;
                        return;
                    }
                    Err(error) => {
                        error!(
                            target: telemetry_target::HOT_RELOAD,
                            error = %error,
                            "engine build failed; keeping the running engine runtime"
                        );
                        return;
                    }
                }
            }
        };

        // Step 2: Rebuild the project too, so the replacement generation loads
        // a module compiled against the engine it will run inside. A failed
        // project build is not fatal: the previously built artifact is still
        // on disk and the runtime falls back to it.
        //
        // The project signal is consumed only on success. A project edit made
        // before this swap is then already covered - the replacement generation
        // loads the module this build produced - so it must not trigger a
        // second reload on the very next frame. A failed build leaves the
        // signal armed instead, so the next frame retries and surfaces the
        // compiler error again.
        let project_generation = self.signals.project.load(Ordering::Acquire);
        match build_project_module(&self.workspace_root, &self.module_config, None) {
            Ok(_) => self.last_project_generation = project_generation,
            Err(error) => warn!(
                target: telemetry_target::HOT_RELOAD,
                error = %error,
                "project build failed during an engine reload; the new engine will load the previously built project module"
            ),
        }

        // Step 3: Capture the world while the running generation is still
        // fully alive. A capture failure aborts the swap rather than trading
        // the session's state for a newer engine.
        //
        // Swap latency is measured from here rather than from the trigger, so
        // the reported number is the time the session is actually unavailable
        // and not the compiler's wall clock.
        let swap_started_at = Instant::now();
        let Some(client) = self.client.as_mut() else {
            error!(
                target: telemetry_target::HOT_RELOAD,
                "no engine runtime generation is loaded; cannot swap"
            );
            return;
        };
        let captured = match client.capture_world_state() {
            Ok(captured) => {
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    summary = captured.summary(),
                    bytes = captured.byte_len(),
                    "captured world state for the engine swap"
                );
                captured
            }
            Err(error) => {
                error!(
                    target: telemetry_target::HOT_RELOAD,
                    error = %error,
                    "could not capture world state; keeping the running engine runtime"
                );
                return;
            }
        };

        // Step 4: Stage, map, and ABI-validate the replacement before the
        // running generation is torn down. None of this touches the GPU, so
        // both modules can be mapped side by side, and a rejected dylib costs
        // nothing but a log line.
        let mapped = match &replacement_source {
            ReplacementSource::Built(built_runtime_path) => {
                let staging_generation = self.next_staging_generation;
                let mapped = RuntimeClient::map(
                    built_runtime_path,
                    &self.workspace_root,
                    staging_generation,
                );
                if mapped.is_ok() {
                    self.next_staging_generation += 1;
                    self.last_staged_generation =
                        self.last_staged_generation.max(staging_generation);
                }
                mapped
            }
            ReplacementSource::Staged(staged_path) => {
                RuntimeClient::map_staged(staged_path.clone())
            }
        };
        let replacement = match mapped {
            Ok(client) => client,
            Err(error) => {
                error!(
                    target: telemetry_target::HOT_RELOAD,
                    error = %error,
                    "the rebuilt engine runtime was rejected; keeping the running one"
                );
                return;
            }
        };

        // Step 5: Tear the running generation down, keeping its module mapped.
        // Its code is still reachable through drop glue, so it is only
        // retired once a replacement is fully live.
        let Some(previous) = self.client.take() else {
            return;
        };
        let (previous_module, previous_staged_path) = previous.destroy();

        // Step 6: Bring the replacement up and restore the captured world.
        match Self::create_and_restore(replacement, &self.create_context, &captured) {
            Ok(client) => {
                let module_path = client.module_path().display().to_string();
                self.client = Some(client);
                self.graveyard.retire(previous_module, previous_staged_path);
                self.reapply_presentation_settings();
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    total_ms = started_at.elapsed().as_secs_f64() * 1000.0,
                    swap_ms = swap_started_at.elapsed().as_secs_f64() * 1000.0,
                    graveyard = self.graveyard.len(),
                    module = %module_path,
                    "engine hot reload complete"
                );
            }
            Err(failure) => {
                error!(
                    target: telemetry_target::HOT_RELOAD,
                    error = %failure.error,
                    "the rebuilt engine runtime failed to start; rolling back to the previous generation"
                );
                self.roll_back(
                    failure.client,
                    previous_module,
                    previous_staged_path,
                    &captured,
                );
            }
        }
    }

    /// Create one generation and restore a captured world into it.
    ///
    /// On failure the client is handed back so the caller can retire its
    /// module rather than leaking a mapped dylib.
    fn create_and_restore(
        mut client: RuntimeClient,
        context: &RuntimeCreateContext,
        captured: &CapturedWorld,
    ) -> Result<RuntimeClient, Box<FailedGeneration>> {
        if let Err(error) = client.create(context) {
            return Err(Box::new(FailedGeneration { client, error }));
        }
        if let Err(error) = client.restore_world_state(captured) {
            // A restore failure is never fatal: `project_init` already built a
            // fresh world, so the session continues without the captured
            // state rather than dropping the generation.
            error!(
                target: telemetry_target::HOT_RELOAD,
                error = %error,
                "world state could not be restored; continuing with the freshly initialized world"
            );
        }
        Ok(client)
    }

    /// Restore the previous generation after a failed replacement.
    ///
    /// The previous module is still mapped, so its generation is rebuilt
    /// directly from it and the same captured world is restored into it. Only
    /// if that also fails does the session end up without a runtime, which the
    /// frame loop then reports as fatal.
    fn roll_back(
        &mut self,
        failed_client: RuntimeClient,
        previous_module: LoadedRuntimeModule,
        previous_staged_path: PathBuf,
        captured: &CapturedWorld,
    ) {
        // The failed replacement never produced a live generation, but its
        // module must still be retired rather than unmapped immediately: its
        // `create` may have run initializers whose teardown needs its code.
        let (failed_module, failed_staged_path) = failed_client.destroy();
        self.graveyard.retire(failed_module, failed_staged_path);

        let restored = RuntimeClient::from_module(previous_module, previous_staged_path);
        match Self::create_and_restore(restored, &self.create_context, captured) {
            Ok(client) => {
                self.client = Some(client);
                self.reapply_presentation_settings();
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    "rolled back to the previous engine runtime with the captured world state"
                );
            }
            Err(failure) => {
                let (module, staged_path) = failure.client.destroy();
                self.graveyard.retire(module, staged_path);
                error!(
                    target: telemetry_target::HOT_RELOAD,
                    error = %failure.error,
                    "rollback to the previous engine runtime also failed; the host has no live engine generation"
                );
            }
        }
    }

    /// Re-apply the presentation settings a rebuilt renderer does not inherit.
    ///
    /// A new generation creates its own renderer with default settings, so the
    /// viewport and projection the frontend configured must be pushed again or
    /// an editor panel would silently revert to full-window rendering.
    fn reapply_presentation_settings(&mut self) {
        let viewport = self.viewport;
        let virtual_resolution = self.virtual_resolution;
        let Some(client) = self.client.as_mut() else {
            return;
        };
        if let Err(error) = client.set_viewport(viewport) {
            warn!(
                target: telemetry_target::RENDERING,
                error = %error,
                "could not re-apply the viewport after an engine swap"
            );
        }
        if let Err(error) = client.set_virtual_resolution(virtual_resolution) {
            warn!(
                target: telemetry_target::RENDERING,
                error = %error,
                "could not re-apply the virtual resolution after an engine swap"
            );
        }
    }
}

impl Drop for EngineSession {
    /// Destroy the live generation before the host's window disappears.
    ///
    /// The renderer holds a surface created from a window the host owns, so
    /// the generation must be torn down while that window is still alive.
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let (module, staged_path) = client.destroy();
            self.graveyard.retire(module, staged_path);
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Print the selected backend before any build output starts streaming.
fn print_startup_configuration(workspace_root: &Path, module_config: &ProjectModuleConfig) {
    info!(
        target: telemetry_target::ENGINE,
        workspace = %workspace_root.display(),
        module = module_config.name.as_str(),
        backend = ?module_config.backend,
        build_command = %module_config.build_command.join(" "),
        watch_directory = module_config.watch_directory.as_str(),
        engine_runtime = %engine_runtime_artifact_path(workspace_root).display(),
        project_module = %project_module_artifact_path(workspace_root, module_config).display(),
        "ECS host starting"
    );
}
