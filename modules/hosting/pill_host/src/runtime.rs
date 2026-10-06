//! The development host: a runtime whose project and modules are DLLs that
//! reload while it runs.
//!
//! # Responsibilities
//!
//! - Build and load the extensions and the project, and watch their sources.
//! - Reload, patch and roll back at every frame boundary, around the frame the
//!   wrapped [`pill_runtime::Runtime`] runs ([`run_one_frame`]).
//! - With `rendering`: load the renderer as a reloadable module, rebuild it
//!   with its data crate, pause it on a layout it was not built against, and
//!   deliver re-cooked shaders ([`RenderingHost`]).
//!
//! # Design
//!
//! Compiled only with `hot_reload`. The engine, its frame and the renderer's
//! window live in `pill_runtime`, which a shipping build runs directly; this
//! module adds what developing needs around them. [`DevHost`] wraps the
//! runtime for the headless path, [`RenderingHost`] adds the renderer module
//! and its window, and backend-specific loading stays behind [`LoadedProject`].

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "hot_patch")]
use std::sync::mpsc;
use std::sync::Arc;
#[cfg(feature = "hot_patch")]
use std::thread;

// External crates
use pill_core::error::{CSharpError, HostError};
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::utils::format_error_chain;
use pill_core::warn;
use pill_core::{error, info};
// The fast path's own reports: patch outcomes, rollback requests, generations.
#[cfg(feature = "hot_patch")]
use pill_core::telemetry::{log_block_colored, Colorize};
use pill_engine::Engine;
use pill_engine::EngineApi;
use pill_engine::{InputEvent, RumbleRequest};
#[cfg(feature = "rendering")]
use pill_renderer_api::{PillRenderer, RenderViewport, RendererError};
use pill_runtime::registration::extension_owner;
#[cfg(feature = "rendering")]
use pill_runtime::{AttachedRenderer, NativeAssets, RendererWindow};
use pill_runtime::{FrameDriver, FrameReport, Runtime};

// Current crate
use crate::analytics;
use crate::config::project_depends_on_crate;
use crate::csharp::ModuleExposedComponent;
use crate::extension::{ExtensionSlot, ReloadOutcome};
#[cfg(feature = "hot_patch")]
use crate::hot_patch::{BeginOutcome, PatchOutcome};
use crate::native_library::cleanup_temporary_files;
use crate::project_module::LoadedProject;
use crate::watcher::spawn_source_watcher;
use crate::{HostConfig, ProjectModuleBackend, ProjectModuleConfig};

// =============================================================================
// Types + Impls
// =============================================================================

/// Everything the frame-step loop needs, assembled once during startup.
///
/// Bundling this state lets headless, windowed, and editor frontends share the
/// same engine lifetime and hot-reload behavior through [`run_one_frame`].
pub struct DevHost {
    /// The directory every spawned cargo build runs in and every artifact path
    /// is resolved against.
    workspace_root: PathBuf,
    module_config: ProjectModuleConfig,
    /// The renderer GPU module a windowed build loads, from the project's
    /// `renderer:` setting; `None` when it selects no renderer. Its data crate
    /// is already among `extensions`.
    #[cfg(feature = "rendering")]
    renderer: Option<String>,
    /// The engine and its frame. Declared before the loaded images, so the
    /// world - whose values may call drop functions inside them - is gone
    /// before any of them unmaps. The engine is boxed inside, so the raw
    /// pointer in `engine_api` stays valid when the host moves.
    runtime: Runtime,
    /// The C-callable table a loaded artifact is handed at every entry point.
    engine_api: EngineApi,
    /// The loaded project image, and the generations retired behind it.
    loaded_project: LoadedProject,
    /// Extensions, each with its own watcher and reload transaction.
    extensions: Vec<ExtensionSlot>,
    /// Counter bumped by the project watcher on every relevant source save.
    ///
    /// One producer (the watcher) and two consumers: `try_project_fast_path`,
    /// which consumes an edit its patch delivered, and the Step 5 reload
    /// transaction, which rebuilds from it.
    source_edit_generation: Arc<AtomicU64>,
    /// The `source_edit_generation` value the frame loop last acted on.
    last_processed_source_edit: u64,
    /// Counter bumped by the reload pipeline itself when it owes the project a
    /// rebuild: a reloaded module the project links directly, or a module swap
    /// that changed the C# mirror surface.
    ///
    /// Separate from [`Self::source_edit_generation`] on purpose. The two mean
    /// different things - "a source save arrived" versus "a rebuilt module
    /// crate must be re-embedded" - and while one counter carried both, a
    /// fast-path patch of the project's own edit consumed the cascade's bump
    /// along with the edit, silently skipping the rebuild. No fast path
    /// consumes this counter, so the rebuild stays owed until
    /// `loaded_project.reload` runs. Written and read by the frame thread
    /// alone, which is why it is a plain integer rather than an atomic.
    queued_reload_generation: u64,
    /// The `queued_reload_generation` value the frame loop last rebuilt for.
    last_processed_queued_reload: u64,
    /// The project watcher's recent signals, so the reload this loop starts
    /// can be timed from the save that caused it.
    ///
    /// The project's watcher is the only producer: the module and renderer
    /// watchers pass `None`, because nothing times a reload from their
    /// signals. Entries are consumed by generation, so each signal times
    /// exactly one reload.
    source_triggers: Arc<crate::watcher::TriggerLog>,
    /// Owners whose systems this host suspended because their binary was built
    /// against a shared component layout another reload has replaced.
    ///
    /// Kept so only these are resumed when they are rebuilt: an owner some
    /// other mechanism suspended (the renderer's pause) is not this code's to
    /// turn back on.
    stale_suspended_owners: Vec<pill_engine::SystemOwner>,
    /// Per-function fast path, when the project opted in with `#[pill_hot]`.
    ///
    /// `None` when the feature is off, when no function is annotated, or when
    /// the project does not also build an `rlib` - all of which simply leave the
    /// existing whole-module reload as the only path.
    #[cfg(feature = "hot_patch")]
    hot_patch: Option<crate::hot_patch::HotPatchSession>,
    /// Per-function fast path for each extension, positionally paired
    /// with `extensions`.
    ///
    /// An entry is `None` when that module annotated nothing, so a module that
    /// has not opted in costs nothing beyond one source scan at startup.
    #[cfg(feature = "hot_patch")]
    module_hot_patch: Vec<Option<crate::hot_patch::HotPatchSession>>,
    /// The patch build running on its own thread, when one is.
    ///
    /// Every reload step defers while this is `Some`: a reload rebuilds the
    /// rlibs the build links against, and a second attempt would race the
    /// first for the same pending edit. The compile no longer freezes the
    /// frame loop, which is the reason it runs there at all.
    #[cfg(feature = "hot_patch")]
    patch_attempt: Option<InFlightPatch>,
    /// Every patch library loaded in this process, newest last.
    ///
    /// Process-wide rather than per-session on purpose: a patch links its own
    /// copy of everything its body calls, so patching one crate has to redirect
    /// the copies sitting inside another crate's patches too. Never unloaded - a
    /// jump or a slot may point into any of them for the rest of the run.
    #[cfg(feature = "hot_patch")]
    loaded_patches: Vec<crate::hot_patch::LoadedPatch>,
    /// Watches the project's `res` and queues the assets whose source or
    /// metadata file changed; `None` when the project has no `res` or the
    /// watch could not start.
    asset_watcher: Option<crate::asset_watcher::AssetWatcher>,
    /// The project's `res` directory, for the asset browser; `None` when the
    /// configuration did not come from a project directory.
    asset_directory: Option<PathBuf>,
    /// Whether every source asset is kept paired with a `.meta` file; see
    /// [`DevHost::set_ensure_asset_metadata`]. Off unless a frontend asks.
    ensure_asset_metadata: bool,
    /// Monotonic counter of reload/rollback/patch events.
    ///
    /// The editor keys its cached engine metadata on this. It is NOT how the
    /// editor learns that entities changed - gameplay changes those every
    /// frame without a reload - it only says "the set of loaded code changed".
    editor_revision: u64,
}

impl DevHost {
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
        let DevHost {
            hot_patch,
            module_hot_patch,
            loaded_patches,
            extensions,
            loaded_project,
            runtime,
            ..
        } = self;
        let engine = runtime.engine_mut();

        // The owning session is whichever one patched this function; searching
        // avoids making callers know whether it came from the project or a
        // module.
        let session = hot_patch
            .iter_mut()
            .chain(module_hot_patch.iter_mut().flatten())
            .find(|session| session.knows_function(function))
            .ok_or_else(|| format!("`{function}` has not been patched in this session"))?;

        let targets = patch_targets(loaded_project, extensions);
        // A successful rollback is logged by the session itself
        // ("patch generation rolled back").
        let result = session.rollback(engine, &targets, loaded_patches, function, generation);
        drop(targets);
        result
    }

    /// Read-only engine access for rendering and diagnostics.
    pub fn engine(&self) -> &Engine {
        self.runtime.engine()
    }

    /// Mutable engine access for frontend-owned ad-hoc work.
    pub fn engine_mut(&mut self) -> &mut Engine {
        self.runtime.engine_mut()
    }

    /// Monotonic counter of reload/rollback/patch events completed by this
    /// host. Editor caches of engine metadata key on it.
    pub fn revision(&self) -> u64 {
        self.editor_revision
    }

    /// Advance the editor revision counter after a reload/rollback/patch.
    ///
    /// The increment itself is the editor's cache-invalidation signal; the
    /// log line exists so integration suites can assert a bump happened
    /// without driving a GUI.
    fn bump_editor_revision(&mut self) {
        self.editor_revision = self.editor_revision.wrapping_add(1);
        info!(
            target: telemetry_target::HOT_RELOAD,
            revision = self.editor_revision,
            "editor revision bumped"
        );
    }

    /// Names of the loaded extensions, in `SystemOwner` order.
    ///
    /// `SystemOwner::extension(i)` is `i + 1`, so index `i` of this
    /// vector labels owner `i + 1`; owner `0` is the project.
    pub fn extension_names(&self) -> Vec<String> {
        self.extensions
            .iter()
            .map(|slot| slot.name().to_string())
            .collect()
    }

    /// Snapshot the current frame rate and entity count without resetting the
    /// three-second reporting window used by console frontends.
    pub fn current_frame_report(&self) -> FrameReport {
        self.runtime.current_frame_report()
    }

    /// The project's `res` directory, when the host knows it.
    pub fn asset_directory(&self) -> Option<&Path> {
        self.asset_directory.as_deref()
    }

    /// The project's `res` tree with each file's asset type, load state and
    /// guid; empty when the host has no `res` directory.
    pub fn asset_entries(&self) -> Vec<crate::asset_browser::AssetEntry> {
        match &self.asset_directory {
            Some(directory) => {
                crate::asset_browser::list_assets(self.runtime.engine().world(), directory)
            }
            None => Vec::new(),
        }
    }

    /// The import settings (or standalone document) of the asset at `path`,
    /// relative to `res`, as JSON.
    ///
    /// # Errors
    ///
    /// A message when no registered type imports the path or its file cannot
    /// be read.
    pub fn asset_settings(&self, path: &str) -> Result<serde_json::Value, String> {
        crate::asset_browser::asset_settings(self.runtime.engine().world(), path)
    }

    /// Save edited import settings for the asset at `path`; the asset watcher
    /// reimports it at the next frame. Returns the normalized settings written.
    ///
    /// # Errors
    ///
    /// A message when the settings do not fit the type or the file cannot be
    /// written.
    pub fn save_asset_settings(
        &mut self,
        path: &str,
        settings: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let directory = self
            .asset_directory
            .clone()
            .ok_or("the host has no project `res` directory")?;
        crate::asset_browser::save_asset_settings(
            self.runtime.engine_mut().world_mut(),
            &directory,
            path,
            settings,
        )
    }

    /// Keep every source asset in `res` paired with a `.meta` file.
    ///
    /// Turning it on writes the missing files at once: every source a
    /// registered sourced type imports gets its type's default settings and a
    /// guid, without being decoded or loaded. While it stays on, a `.meta`
    /// deleted under a running host is written again - with the loaded asset's
    /// guid when it is loaded - and a new source gets one when the watcher
    /// imports it. The editor turns this on; a game run leaves it off.
    pub fn set_ensure_asset_metadata(&mut self, enabled: bool) {
        self.ensure_asset_metadata = enabled;
        if !enabled {
            return;
        }
        let Some(directory) = self.asset_directory.clone() else {
            return;
        };
        let world = self.runtime.engine().world();
        let (Some(registry), Some(assets)) = (
            world.get_resource::<pill_engine::ImportRegistry>(),
            world.get_resource::<pill_engine::AssetManager>(),
        ) else {
            return;
        };
        let (written, failed) = registry.ensure_all_metadata(assets, &directory);
        for name in &written {
            info!(
                target: telemetry_target::HOT_RELOAD,
                asset = name.as_str(),
                "[assets] wrote missing metadata"
            );
        }
        for (name, error) in &failed {
            warn!(
                target: telemetry_target::HOT_RELOAD,
                asset = name.as_str(),
                "[assets] could not write missing metadata: {error}"
            );
        }
        info!(
            target: telemetry_target::HOT_RELOAD,
            written = written.len(),
            failed = failed.len(),
            "[assets] every source asset has a .meta file"
        );
    }

    /// The standalone asset types a new file can be created for.
    pub fn standalone_asset_types(&self) -> Vec<crate::asset_browser::StandaloneType> {
        crate::asset_browser::standalone_types(self.runtime.engine().world())
    }

    /// Create and load a new standalone asset; see
    /// [`crate::asset_browser::create_standalone_asset`]. Returns its path
    /// relative to `res`.
    ///
    /// # Errors
    ///
    /// A message when the input is refused or the file cannot be written.
    pub fn create_standalone_asset(
        &mut self,
        type_name: &str,
        folder: &str,
        name: &str,
    ) -> Result<String, String> {
        let directory = self
            .asset_directory
            .clone()
            .ok_or("the host has no project `res` directory")?;
        crate::asset_browser::create_standalone_asset(
            self.runtime.engine_mut().world_mut(),
            &directory,
            type_name,
            folder,
            name,
        )
    }

    /// Move the asset at `from` to `to` (both relative to `res`) with its
    /// `.meta`; the asset watcher follows the move.
    ///
    /// # Errors
    ///
    /// A message when a path leaves `res` or the move is refused.
    pub fn move_asset(&self, from: &str, to: &str) -> Result<(), String> {
        let directory = self
            .asset_directory
            .as_deref()
            .ok_or("the host has no project `res` directory")?;
        crate::asset_browser::move_asset(directory, from, to)
    }
}

impl FrameDriver for DevHost {
    type Error = std::convert::Infallible;

    fn run_frame(&mut self) -> Result<Option<FrameReport>, Self::Error> {
        Ok(run_one_frame(self))
    }

    fn push_input(&mut self, event: InputEvent) {
        self.engine_mut().push_input_event(event);
    }

    fn take_rumble_requests(&mut self) -> Vec<RumbleRequest> {
        self.engine_mut().take_rumble_requests()
    }
}

/// The development host with the engine renderer attached to one native window.
///
/// Keeping the renderer beside [`DevHost`] makes its creation and lifetime part
/// of host setup. Executable crates never construct or retain GPU resources.
#[cfg(feature = "rendering")]
pub struct RenderingHost {
    host: DevHost,
    /// The backend and the window it draws on; the backend drops first.
    /// Declared before `renderer_module` so the backend is detached while the
    /// module it lives in is still mapped.
    display: AttachedRenderer,
    /// The renderer module the backend lives in.
    renderer_module: RendererModule,
    /// Renderer assets prepared beside the engine.
    assets: NativeAssets,
    /// Why rendering is paused, or `None` while it runs.
    ///
    /// Set when the renderer's data crate reloaded with a different component
    /// layout and no renderer built against the new layout could take over:
    /// the live renderer's `rendering` system and `render` would read the new
    /// rows through the old field offsets. While paused, the renderer's owner
    /// is disabled and `render` is skipped; the window keeps its last frame.
    paused: Option<PauseReason>,
    /// Watches the renderer data crate's `shaders/` and queues what it
    /// re-cooks; `None` without a renderer, or when its data crate has no
    /// shaders or the watch could not start.
    shader_watcher: Option<crate::shader_watcher::ShaderWatcher>,
    /// Set whenever a renderer backend attaches; the next frame then reports
    /// the render data the renderer ignores, once per renderer generation.
    unsupported_data_check_pending: bool,
}

/// Why a windowed host stopped rendering; see [`RenderingHost`]'s `paused`.
#[cfg(feature = "rendering")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseReason {
    /// The renderer's data crate reloaded with a different component layout,
    /// and the renderer rebuilt against it failed to build, initialize or
    /// attach.
    StaleRendererAfterDataLayoutChange,
}

/// What a renderer step did, for the pause decision.
#[cfg(feature = "rendering")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RendererStep {
    /// Nothing reloaded; whatever was attached stays.
    Unchanged,
    /// A generation loaded or reloaded and is attached to the window.
    Attached,
    /// A reload or first load was attempted and did not end with a new
    /// generation attached (build, `init` or attach failed; a rollback may
    /// have re-attached the previous generation).
    Failed,
}

/// Whether rendering should be paused after one frame boundary.
///
/// `paused` is the state before it; `data_layout_changed` says whether the
/// renderer's data crate reloaded this boundary with a different component
/// layout; `step` is what the renderer step did. A renderer that attached
/// successfully always resumes; a layout change without one pauses; anything
/// else keeps the current state.
#[cfg(feature = "rendering")]
fn pause_after_boundary(
    paused: Option<PauseReason>,
    data_layout_changed: bool,
    step: RendererStep,
) -> Option<PauseReason> {
    match step {
        RendererStep::Attached => None,
        RendererStep::Failed | RendererStep::Unchanged if data_layout_changed => {
            Some(PauseReason::StaleRendererAfterDataLayoutChange)
        }
        RendererStep::Failed | RendererStep::Unchanged => paused,
    }
}

/// The structural hash of every component the named extension registered,
/// by component name, or empty when no such extension is loaded.
///
/// Taken before and after a reload of the renderer's data crate, to tell a
/// layout change (the renderer must not read the rows until it is rebuilt)
/// from a body-only one (it may).
#[cfg(feature = "rendering")]
fn component_schemas_of(host: &DevHost, extension: &str) -> Vec<(String, Option<u64>)> {
    let Some(slot) = host.extensions.iter().find(|slot| slot.name() == extension) else {
        return Vec::new();
    };
    let world = host.runtime.engine().world();
    let mut schemas: Vec<(String, Option<u64>)> = slot
        .exposed_component_names()
        .iter()
        .map(|name| {
            let schema = world
                .resolve_component_id_by_name_any(name)
                .ok()
                .flatten()
                .and_then(|component| world.component_field_layout(component))
                .filter(|fields| !fields.is_empty())
                .map(pill_engine::component::component_schema_hash);
            (name.clone(), schema)
        })
        .collect();
    schemas.sort();
    schemas.dedup();
    schemas
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
        let renderer_module = &self.renderer_module;
        self.display.retarget(window, width, height, |window_data| {
            attach_backend(renderer_module, window_data, width, height)
        })
    }

    /// Forward a physical window resize to the engine renderer.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.display.resize(width, height);
    }

    /// Reload the renderer module when its sources changed, and keep the
    /// window drawn by whichever generation is current afterwards.
    ///
    /// The backend is built on the current module image, so it is detached in
    /// the reload's `before_commit` hook - after the replacement has built and
    /// loaded, and before the swap retires the old image - while that image is
    /// still mapped. Until the attach below, the host draws nothing. After the
    /// commit, the slot's current library is the new generation, or the old
    /// one when the new `init` failed and the transaction rolled back; both
    /// are attached the same way. A build or load that failed never reaches
    /// the hook, and the backend is left as it was.
    ///
    /// A backend that fails to attach leaves the window undrawn and is
    /// retried on the next source change; the reason is logged.
    fn reload_renderer_if_changed(&mut self) -> RendererStep {
        let RenderingHost {
            host,
            display,
            renderer_module,
            unsupported_data_check_pending,
            ..
        } = self;
        let mut detached = false;
        let outcome = renderer_module.reload_if_changed(
            host.runtime.engine_mut(),
            &host.engine_api,
            &host.workspace_root,
            &mut || {
                // Dropping the module's backend runs its detach export inside
                // the image that is about to retire, while it is still mapped.
                display.detach_backend();
                detached = true;
            },
        );
        // A generation that loaded for the first time - after a startup load
        // that failed - has nothing attached yet, exactly like a detached one.
        let loaded = outcome == crate::renderer_module::RendererModuleChange::Loaded;
        if !detached && !loaded {
            // A reload that failed before its commit (build or load) left the
            // backend attached; one that did not run changed nothing.
            return match outcome {
                crate::renderer_module::RendererModuleChange::Reloaded(ReloadOutcome::Failed {
                    ..
                }) => RendererStep::Failed,
                _ => RendererStep::Unchanged,
            };
        }
        // After the commit: a rolled-back reload re-attaches the previous
        // generation, which is not a renderer built for new data.
        let rolled_back = matches!(
            outcome,
            crate::renderer_module::RendererModuleChange::Reloaded(ReloadOutcome::Failed { .. })
        );
        let (width, height) = display.surface_size();
        match attach_backend(renderer_module, display.window_data(), width, height) {
            Ok(attached) => {
                display.replace_backend(attached);
                // A new renderer generation: what it ignores is reported anew.
                *unsupported_data_check_pending = true;
                // Counted, not assumed: a generation whose system escaped its
                // owner would leave a second `rendering` system behind.
                let rendering_systems = host
                    .runtime
                    .engine()
                    .system_snapshots()
                    .iter()
                    .filter(|system| system.name == "rendering")
                    .count();
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    outcome = ?outcome,
                    rendering_systems,
                    "renderer reattached to the window"
                );
                if rolled_back {
                    RendererStep::Failed
                } else {
                    RendererStep::Attached
                }
            }
            Err(error) => {
                warn!(
                    target: telemetry_target::HOT_RELOAD,
                    outcome = ?outcome,
                    "renderer reloaded but could not reattach; the window stays undrawn until the next renderer edit: {error}"
                );
                RendererStep::Failed
            }
        }
    }

    /// The renderer step of one frame boundary, run between the host's reload
    /// phase and its frame phase: reload the renderer when its own sources
    /// changed, or when its data crate just reloaded, and pause or resume
    /// rendering accordingly.
    ///
    /// Module builds are synchronous, so a data reload and the renderer rebuild
    /// it requests complete in the same boundary: in the normal case no frame
    /// runs between the two swaps and nothing pauses. The pause covers the
    /// rest: a layout change whose renderer could not be rebuilt, initialized
    /// or attached.
    fn renderer_step(
        &mut self,
        data_extension: Option<&str>,
        reloaded_extensions: &[String],
        schemas_before: Option<Vec<(String, Option<u64>)>>,
    ) {
        // Step 1: A reloaded data crate means the renderer's compiled-in copy
        // of it is stale: rebuild the renderer against the new source.
        let data_reloaded =
            data_extension.is_some_and(|data| reloaded_extensions.iter().any(|name| name == data));
        let data_layout_changed = match (data_reloaded, data_extension, schemas_before) {
            (true, Some(data), Some(before)) => component_schemas_of(&self.host, data) != before,
            _ => false,
        };
        if data_reloaded {
            info!(
                target: telemetry_target::HOT_RELOAD,
                layout_changed = data_layout_changed,
                "the renderer's data crate reloaded; rebuilding the renderer against it"
            );
            self.renderer_module.request_rebuild();
        }

        // Step 2: The renderer reload itself, from its own edit or the request.
        let step = self.reload_renderer_if_changed();

        // Step 3: Pause or resume.
        let owner = self.renderer_module.owner();
        let next = pause_after_boundary(self.paused, data_layout_changed, step);
        if next != self.paused {
            let enabled = next.is_none();
            self.host
                .runtime
                .engine_mut()
                .set_systems_enabled_for_owner(owner, enabled);
            match next {
                Some(reason) => warn!(
                    target: telemetry_target::HOT_RELOAD,
                    reason = ?reason,
                    "rendering paused: the renderer is not built against the current data layout; the window keeps its last frame until a renderer edit builds and loads"
                ),
                None => info!(
                    target: telemetry_target::HOT_RELOAD,
                    "rendering resumed: the renderer is built against the current data layout"
                ),
            }
            self.paused = next;
        } else if next.is_some() {
            // Still paused: a generation that registered during the pause
            // starts disabled already (the owner stays disabled).
            self.host
                .runtime
                .engine_mut()
                .set_systems_enabled_for_owner(owner, false);
        }
    }

    /// Hand every shader the watcher re-cooked to the renderer data crate's
    /// `pill_render_data_shader_changed`, which puts the WGSL into the shader
    /// assets built from it.
    ///
    /// The export is resolved again on every delivery, from the data module
    /// generation current at this frame boundary, so a shader edit made while
    /// the data crate was rebuilding lands in the generation that replaced it.
    /// A data crate without the export (a shipping build, where the shader
    /// reload path is compiled out) leaves the edits undelivered, with a
    /// warning.
    fn deliver_shader_changes(&mut self, data_extension: Option<&str>) {
        let Some(watcher) = &self.shader_watcher else {
            return;
        };
        let cooked = watcher.drain();
        if cooked.is_empty() {
            return;
        }
        let DevHost {
            extensions,
            runtime,
            ..
        } = &mut self.host;
        let engine = runtime.engine_mut();
        let Some(slot) =
            data_extension.and_then(|data| extensions.iter().find(|slot| slot.name() == data))
        else {
            return;
        };
        let Some(address) = slot.export_address(SHADER_CHANGED_EXPORT) else {
            warn!(
                target: telemetry_target::HOT_RELOAD,
                module = slot.name(),
                export = SHADER_CHANGED_EXPORT,
                "the renderer data module does not export the shader reload function; shader edits are not applied"
            );
            return;
        };
        // SAFETY: the data crate exports this name with exactly
        // `ShaderChangedFunction`'s signature, and a function pointer and a data
        // pointer have the same size on every target this engine builds for.
        let shader_changed =
            unsafe { std::mem::transmute_copy::<*const (), ShaderChangedFunction>(&address.0) };
        let world: *mut pill_engine::World = engine.world_mut();
        for shader in cooked {
            // SAFETY: `world` is the live engine world, used by nothing else for
            // the call, and both buffers are the owned strings' bytes.
            let changed = unsafe {
                shader_changed(
                    world,
                    shader.relative_path.as_ptr(),
                    shader.relative_path.len(),
                    shader.wgsl.as_ptr(),
                    shader.wgsl.len(),
                )
            };
            if changed == SHADER_CHANGE_REFUSED {
                warn!(
                    target: telemetry_target::HOT_RELOAD,
                    path = shader.relative_path.as_str(),
                    "the renderer data module refused a re-cooked shader"
                );
            }
        }
    }

    /// Warn once about each render component the attached renderer ignores.
    ///
    /// The candidates are the shared components the renderer's data crate
    /// registered and at least one entity carries. Shared, because render data
    /// has to be to reach the renderer module at all - its own copy of a type
    /// only finds the world's column through the shared identity - while the
    /// plain components the data crate registers along the way (the engine's
    /// `Position` and `Color`) are not render data. A candidate missing from the
    /// renderer's `RenderCapabilities::consumed_components` draws nothing, and
    /// a project should hear so rather than wonder why its fog volume or light
    /// has no effect.
    fn report_unsupported_render_data(&self, data_extension: Option<&str>) {
        let Some(slot) = data_extension
            .and_then(|data| self.host.extensions.iter().find(|slot| slot.name() == data))
        else {
            return;
        };
        let consumed = self.display.renderer().capabilities().consumed_components;
        let ignored = unsupported_render_components(
            self.host.engine().world(),
            slot.exposed_component_names(),
            &consumed,
        );
        for (component, entities) in ignored {
            warn!(
                target: telemetry_target::RENDERING,
                component = component.as_str(),
                entities,
                renderer = self.host.renderer.as_deref().unwrap_or_default(),
                "entities carry a render component the active renderer does not draw; it has no effect on the picture"
            );
        }
    }

    /// Restrict engine drawing to a physical region of the native surface.
    ///
    /// Use `None` for full-window rendering. Embedded frontends can leave the
    /// corresponding WebView region transparent and keep surrounding UI
    /// panels opaque.
    pub fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.display.set_viewport(viewport);
    }

    /// Execute one ECS frame and present its resulting world to the surface.
    pub fn run_one_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        let frame_start = Instant::now();

        // Step 1: The data crate's schemas before any reload, when it has an
        // edit pending: compared after the reload phase to tell a layout change
        // from a body-only one.
        let data_extension = self
            .host
            .renderer
            .as_deref()
            .map(|renderer| format!("{renderer}_data"));
        let schemas_before = data_extension.as_deref().and_then(|data| {
            let pending = self
                .host
                .extensions
                .iter()
                .any(|slot| slot.name() == data && slot.pending_reload_generation().is_some());
            pending.then(|| component_schemas_of(&self.host, data))
        });

        // Step 2: Extension and project reloads, then the renderer step, all
        // before any system runs: the renderer this frame draws with is built
        // against the data this frame holds, or rendering is paused.
        let reloaded_extensions = run_reload_phase(&mut self.host);
        self.renderer_step(
            data_extension.as_deref(),
            &reloaded_extensions,
            schemas_before,
        );
        // Re-cooked shaders go to the data generation that is current now,
        // after every swap of this boundary.
        self.deliver_shader_changes(data_extension.as_deref());

        // Step 3: Asset sync, then the frame's systems.
        self.assets.update(self.host.engine_mut())?;
        let report = run_frame_phase(&mut self.host, frame_start);

        // Step 4: While paused the live renderer must not read the world.
        if self.paused.is_some() {
            return Ok(report);
        }
        // After the frame's systems, so entities a startup system spawns are
        // counted too; while paused the check waits for the renderer that
        // resumes drawing.
        if self.unsupported_data_check_pending {
            self.unsupported_data_check_pending = false;
            self.report_unsupported_render_data(data_extension.as_deref());
        }
        self.display.render(self.host.runtime.engine().world())?;
        Ok(report)
    }

    /// Read live frame statistics for UI overlays without affecting the
    /// lower-frequency report returned by [`Self::run_one_frame`].
    pub fn current_frame_report(&self) -> FrameReport {
        self.host.current_frame_report()
    }

    /// Read-only engine access for frontend diagnostics and editor snapshots.
    pub fn engine(&self) -> &Engine {
        self.host.engine()
    }

    /// Mutable engine access for frontend-owned, frame-boundary work.
    pub fn engine_mut(&mut self) -> &mut Engine {
        self.host.engine_mut()
    }

    /// Monotonic reload/rollback/patch counter; see [`DevHost::revision`].
    pub fn revision(&self) -> u64 {
        self.host.revision()
    }

    /// Loaded extension names in `SystemOwner` order; see
    /// [`DevHost::extension_names`].
    pub fn extension_names(&self) -> Vec<String> {
        self.host.extension_names()
    }

    /// The project's `res` tree; see [`DevHost::asset_entries`].
    pub fn asset_entries(&self) -> Vec<crate::asset_browser::AssetEntry> {
        self.host.asset_entries()
    }

    /// An asset's settings as JSON; see [`DevHost::asset_settings`].
    ///
    /// # Errors
    ///
    /// As [`DevHost::asset_settings`].
    pub fn asset_settings(&self, path: &str) -> Result<serde_json::Value, String> {
        self.host.asset_settings(path)
    }

    /// Save an asset's settings; see [`DevHost::save_asset_settings`].
    ///
    /// # Errors
    ///
    /// As [`DevHost::save_asset_settings`].
    pub fn save_asset_settings(
        &mut self,
        path: &str,
        settings: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.host.save_asset_settings(path, settings)
    }

    /// Keep every source asset paired with a `.meta`; see
    /// [`DevHost::set_ensure_asset_metadata`].
    pub fn set_ensure_asset_metadata(&mut self, enabled: bool) {
        self.host.set_ensure_asset_metadata(enabled);
    }

    /// The standalone asset types; see [`DevHost::standalone_asset_types`].
    pub fn standalone_asset_types(&self) -> Vec<crate::asset_browser::StandaloneType> {
        self.host.standalone_asset_types()
    }

    /// Create a standalone asset; see [`DevHost::create_standalone_asset`].
    ///
    /// # Errors
    ///
    /// As [`DevHost::create_standalone_asset`].
    pub fn create_standalone_asset(
        &mut self,
        type_name: &str,
        folder: &str,
        name: &str,
    ) -> Result<String, String> {
        self.host.create_standalone_asset(type_name, folder, name)
    }

    /// Move an asset with its `.meta`; see [`DevHost::move_asset`].
    ///
    /// # Errors
    ///
    /// As [`DevHost::move_asset`].
    pub fn move_asset(&self, from: &str, to: &str) -> Result<(), String> {
        self.host.move_asset(from, to)
    }
}

#[cfg(feature = "rendering")]
impl FrameDriver for RenderingHost {
    type Error = RendererError;

    fn run_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        self.run_one_frame()
    }

    fn resize(&mut self, width: u32, height: u32) {
        RenderingHost::resize(self, width, height);
    }

    fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        RenderingHost::set_render_viewport(self, viewport);
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

/// Import every known source under `asset_directory` through the world's
/// import registry, writing missing metadata files, and log what it did.
///
/// A failed asset is logged and skipped rather than failing the startup: the
/// scan is a convenience on top of the project's own loading code, which
/// reports its own errors for the assets it actually needs.
fn scan_project_assets(engine: &mut pill_engine::Engine, asset_directory: &Path) {
    let world = engine.world_mut();
    // A clone, so the registry can be called against the asset manager, which
    // is a second resource of the same world.
    let Some(registry) = world.get_resource::<pill_engine::ImportRegistry>().cloned() else {
        info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            "[assets] scan_on_start: no imported asset types are registered; nothing to scan"
        );
        return;
    };
    let Some(assets) = world.get_resource_mut::<pill_engine::AssetManager>() else {
        return;
    };
    let report = registry.scan(
        assets,
        asset_directory,
        pill_engine::MetadataPolicy::CreateIfMissing,
    );
    for (name, error) in &report.failed {
        warn!(
            target: pill_core::telemetry::telemetry_target::ECS,
            asset = name.as_str(),
            "[assets] scan_on_start could not import an asset: {error}"
        );
    }
    info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        directory = %asset_directory.display(),
        imported = report.imported.len(),
        already_loaded = report.already_loaded.len(),
        metadata_created = report.metadata_created.len(),
        failed = report.failed.len(),
        unknown_extensions = ?report.unknown_extensions,
        "[assets] scan_on_start finished"
    );
}

/// Report a setup failure and hand the error back to the caller.
///
/// The host state built so far unwinds as soon as the error is returned, and
/// what it owns is loaded module images: dropping one unmaps code the engine
/// may still be running, and a fault in that unmap would destroy the only copy
/// of this error. Reporting first is what keeps the failure legible when the
/// unload then crashes, which is how the first windowed-startup failure went
/// missing.
fn report_setup_failure(error: HostError) -> HostError {
    let cause_chain = format_error_chain(&error);
    // "Host setup failed" is what the suites wait for when a start must fail.
    error!(
        target: telemetry_target::ENGINE,
        error = %cause_chain,
        "Host setup failed"
    );
    error
}

/// Report a setup failure and tear the half-built host down world-first.
///
/// Locals unwind in reverse declaration order, which here means the loaded
/// module images unmap while the world is still alive - and a resource a module
/// inserted stores its drop function in that module's image, so the world's own
/// teardown is what calls it. An unmapped call is an access violation, which is
/// why a failed startup could end in a crash after printing the reason.
///
/// Dropping the world first makes every one of those drops happen while the
/// images are still mapped; the slots then unmap with nothing pointing into
/// them. The error is reported before either drop, so it survives whatever the
/// teardown does.
fn fail_setup(runtime: Runtime, error: HostError) -> HostError {
    let reported = report_setup_failure(error);
    drop(runtime);
    reported
}

/// Build/load the project module, create the engine, and start its source watcher.
///
/// # Errors
///
/// Returns a typed [`HostError`] naming the failing subsystem: configuration,
/// build, library loading, watcher startup, or managed backend startup.
pub fn setup(host_config: impl Into<HostConfig>) -> Result<DevHost, HostError> {
    // Step 1: Reject inconsistent configurations before any build or load,
    // and apply the project's log levels so the build and load below are
    // already logged at them.
    let host_config = host_config.into();
    if let Err(error) = pill_runtime::apply_logging_settings(&host_config.logging) {
        pill_core::warn!(
            target: telemetry_target::ENGINE,
            "the project's logging settings were not applied: {error}"
        );
    }
    let module_config = host_config.project;
    module_config.validate()?;
    for module in &host_config.extensions {
        module.validate()?;
    }

    // Step 2: Resolve the workspace root and print the selected configuration.
    // The engine owns this root (see [`crate::config::engine_workspace_root`]),
    // so it does not depend on the directory the process was launched from.
    let workspace_root = crate::config::engine_workspace_root()?;

    print_startup_configuration(&workspace_root, &module_config);

    if matches!(
        module_config.backend,
        ProjectModuleBackend::NativeLibrary { .. }
    ) {
        cleanup_temporary_files(&workspace_root);
    }

    // Step 3: Construct the engine and its stable API table.
    // EngineApi stores a raw pointer into the runtime's boxed engine, whose
    // address does not change when the runtime moves.
    let mut runtime = Runtime::new();
    runtime.engine_mut().set_parallel_execution(true);
    let engine_api = EngineApi::new(runtime.engine_mut());

    // Step 3b: Announce every module this start builds, in build order: the
    // extensions, the project, and - in a windowed host - the renderer's GPU
    // module, which is built once the window opens.
    let mut planned_builds: Vec<crate::build_progress::PlannedBuild> = host_config
        .extensions
        .iter()
        .map(|extension| crate::build_progress::PlannedBuild {
            module: extension.name.clone(),
            kind: "extension",
        })
        .collect();
    planned_builds.push(crate::build_progress::PlannedBuild {
        module: module_config.name.clone(),
        kind: "project",
    });
    if cfg!(feature = "rendering") {
        if let Some(renderer) = &host_config.renderer {
            planned_builds.push(crate::build_progress::PlannedBuild {
                module: renderer.clone(),
                kind: "renderer",
            });
        }
    }
    crate::build_progress::announce_plan(&host_config.name, planned_builds);

    // Step 3b: Build every native module in ONE cargo invocation: the
    // extensions, the project's member and - in a windowed host - the
    // renderer's wrapper, which is an ordinary wrapper build whose load waits
    // for the window. Each module build otherwise pays cargo's fixed
    // process-and-resolve cost again, which is what made a start scale
    // linearly in cargo invocations; the batch shares it. Modules whose
    // artifacts the batch validated skip their own build when they load below
    // (Step 4) - an edit later still rebuilds a module on its own.
    crate::build_runner::build_extension_batch(
        &workspace_root,
        &host_config.extensions,
        Some(&module_config),
        if cfg!(feature = "rendering") {
            host_config.renderer.as_deref()
        } else {
            None
        },
    );

    // Step 4: Build, load and watch the extensions before the project.
    // Modules are infrastructure: loading them first means the project can rely
    // on whatever they register. The renderer's data crate, when the project
    // selects a renderer, is the first of them (see `HostConfig::renderer`), so
    // extensions and the project find its components registered and the C#
    // bridge binds them like any extension's, headless included. Each gets its own owner tag and its own
    // generation counter, so later reloads stay isolated from each other.
    let mut extensions = Vec::with_capacity(host_config.extensions.len());
    for (index, module_config) in host_config.extensions.iter().enumerate() {
        let module_generation = Arc::new(AtomicU64::new(0));
        let slot = match ExtensionSlot::start(
            runtime.engine_mut(),
            &engine_api,
            &workspace_root,
            module_config,
            extension_owner(index),
            Arc::clone(&module_generation),
            crate::reload::FirstLoadFailure::ClearWorld,
        ) {
            Ok(slot) => slot,
            Err(error) => return Err(fail_setup(runtime, error)),
        };
        if let Err(error) = spawn_source_watcher(
            workspace_root.clone(),
            &module_config.name,
            &module_config.watch_directory,
            module_generation,
            None,
        ) {
            return Err(fail_setup(runtime, error.into()));
        }
        extensions.push(slot);
    }
    // The renderer data crate's asset functions, for the C# bridge: resolved
    // from whichever loaded module offers them, before any managed code runs.
    publish_asset_exports(&extensions);

    // Step 5: Build and load the project module, then start its source watcher.
    // Extensions load first, so the C# backend can be handed every
    // native component the modules exposed to managed code: each module's
    // registered type names resolve to its native components, and the
    // C#-facing name is the Rust path with `::` replaced by `.` so a
    // `project_cs` mirror struct reproduces the same stable identity.
    // The generated C# mirror files must exist before the project build
    // compiles `project_cs`, so write them here (managed backend only), one
    // per extension that exposes components, derived from each module's
    // real registered layout. Nothing is hand-written in the project.
    let mut module_exposed_components: Vec<ModuleExposedComponent> = Vec::new();
    let mut all_mirror_methods: Vec<crate::csharp::ResolvedMirrorMethod> = Vec::new();
    if let ProjectModuleBackend::CSharp(_) = &module_config.backend {
        for slot in &extensions {
            // Regenerate the module's C# mirror from its current generation.
            // The returned change flag is ignored at startup (the mirror is
            // always written before the project compiles); the reload path
            // uses it to decide whether to queue a C# project rebuild.
            let (exposed, methods, accessors, _changed) =
                regenerate_module_csharp_mirror(&workspace_root, runtime.engine_mut(), slot)?;
            module_exposed_components.extend(exposed);
            all_mirror_methods.extend(methods);
            // Container accessors ride the mirror-method table, so the managed
            // runtime resolves them through the same lookup and the same
            // per-reload refresh as value-type methods.
            all_mirror_methods.extend(crate::csharp::accessor_rows(&accessors));
        }
    }
    let loaded_project = match LoadedProject::start(
        runtime.engine_mut(),
        &engine_api,
        &workspace_root,
        &module_config,
        &module_exposed_components,
        &all_mirror_methods,
    ) {
        Ok(project) => project,
        Err(error) => return Err(fail_setup(runtime, error)),
    };

    let source_edit_generation = Arc::new(AtomicU64::new(0));
    let source_triggers = Arc::new(crate::watcher::TriggerLog::new());
    if let Err(error) = spawn_source_watcher(
        workspace_root.clone(),
        &module_config.name,
        &module_config.watch_directory,
        Arc::clone(&source_edit_generation),
        Some(Arc::clone(&source_triggers)),
    ) {
        return Err(fail_setup(runtime, error.into()));
    }

    // Step 5b: Import the project's source assets when its settings ask for
    // it, now that every module has registered its imported asset types.
    if host_config.assets.scan_on_start {
        if let Some(asset_directory) = &host_config.asset_directory {
            scan_project_assets(runtime.engine_mut(), asset_directory);
        }
    }

    // Step 6: Snapshot host memory and print the startup analytics report.
    // Every module has been built, staged, loaded and initialized by now, so
    // the table carries the complete startup picture.
    analytics::record_host_memory();
    analytics::print_startup_report();

    info!(
        target: telemetry_target::ENGINE,
        "Entering project loop. Edit {}/**/* to hot-reload",
        module_config.watch_directory
    );

    // Say how to drive rollback, once, next to where the fast path announces
    // itself - an interface nothing mentions is one nobody uses.
    #[cfg(feature = "hot_patch")]
    info!(
        target: telemetry_target::HOT_RELOAD,
        "Patch rollback: write `function@generation`, `function@previous` or `list` to {}",
        ROLLBACK_REQUEST_FILE
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

    // The same fast path for each extension. A module's `#[pill_hot_fn]`
    // functions are compiled into every artifact that links the crate, so this
    // is what lets an edit reach the project's embedded copy without the
    // cascading project reload a module swap would otherwise queue.
    #[cfg(feature = "hot_patch")]
    let module_hot_patch: Vec<Option<crate::hot_patch::HotPatchSession>> = host_config
        .extensions
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

    let host = DevHost {
        workspace_root,
        module_config,
        #[cfg(feature = "rendering")]
        renderer: host_config.renderer.clone(),
        runtime,
        engine_api,
        loaded_project,
        extensions,
        source_edit_generation,
        last_processed_source_edit: 0,
        queued_reload_generation: 0,
        last_processed_queued_reload: 0,
        source_triggers,
        stale_suspended_owners: Vec::new(),
        #[cfg(feature = "hot_patch")]
        hot_patch,
        #[cfg(feature = "hot_patch")]
        module_hot_patch,
        #[cfg(feature = "hot_patch")]
        patch_attempt: None,
        #[cfg(feature = "hot_patch")]
        loaded_patches: Vec::new(),
        asset_watcher: start_asset_watcher(host_config.asset_directory.as_deref()),
        asset_directory: host_config.asset_directory.clone(),
        ensure_asset_metadata: false,
        editor_revision: 0,
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
/// Returns the composed [`RenderingError`](pill_runtime::RenderingError),
/// which transparently carries either a [`HostError`] from setup or a
/// [`RendererError`] from surface creation.
#[cfg(feature = "rendering")]
pub fn setup_rendering<W>(
    project: impl Into<HostConfig>,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, pill_runtime::RenderingError>
where
    W: RendererWindow + 'static,
{
    let host = setup(project.into())?;
    attach_renderer(host, window, width, height)
}

/// The shared components among `registered` that at least one entity in
/// `world` carries and `consumed` does not name, each with its entity count,
/// sorted by name and listed once.
///
/// See [`RenderingHost::report_unsupported_render_data`] for why only shared
/// components are candidates.
#[cfg(feature = "rendering")]
fn unsupported_render_components(
    world: &pill_engine::World,
    registered: &[String],
    consumed: &[String],
) -> Vec<(String, usize)> {
    let mut names: Vec<&String> = registered.iter().collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| !consumed.contains(name))
        .filter_map(|name| {
            let component = world
                .resolve_component_id_by_name_any(name)
                .ok()
                .flatten()?;
            if !matches!(component, pill_engine::ComponentId::Shared(_)) {
                return None;
            }
            let entities = world.live_row_count(component);
            (entities > 0).then(|| (name.clone(), entities))
        })
        .collect()
}

/// The renderer data crate's development shader reload export; see
/// `pill_master_renderer_data::shader_hot_reload`.
#[cfg(feature = "rendering")]
const SHADER_CHANGED_EXPORT: &str = "pill_render_data_shader_changed";

/// What that export returns when it could not look at the assets at all.
#[cfg(feature = "rendering")]
const SHADER_CHANGE_REFUSED: u32 = u32::MAX;

/// Signature of [`SHADER_CHANGED_EXPORT`]: the world, the re-cooked file's
/// path relative to `shaders/`, and its WGSL; returns how many shader assets
/// changed.
#[cfg(feature = "rendering")]
type ShaderChangedFunction = unsafe extern "C" fn(
    world: *mut pill_engine::World,
    path: *const u8,
    path_len: usize,
    wgsl: *const u8,
    wgsl_len: usize,
) -> u32;

/// Start watching the renderer data crate's shaders, when the host has a
/// renderer and its data crate has a `shaders/` directory.
///
/// A watch that cannot start is reported and leaves the host without shader
/// reload; nothing else depends on it.
#[cfg(feature = "rendering")]
fn start_shader_watcher(host: &DevHost) -> Option<crate::shader_watcher::ShaderWatcher> {
    let data = format!(
        "{}{}",
        host.renderer.as_deref()?,
        crate::config::RENDERER_DATA_SUFFIX
    );
    let slot = host.extensions.iter().find(|slot| slot.name() == data)?;
    let shaders_directory = host
        .workspace_root
        .join(slot.crate_directory())
        .join("shaders");
    match crate::shader_watcher::ShaderWatcher::spawn(&data, shaders_directory) {
        Ok(watcher) => watcher,
        Err(error) => {
            warn!(
                target: telemetry_target::HOT_RELOAD,
                module = data.as_str(),
                "shader reload is off: the shader watcher could not start: {error}"
            );
            None
        }
    }
}

/// The renderer module a windowed host loads: the extension slot it lives in.
#[cfg(feature = "rendering")]
type RendererModule = crate::renderer_module::RendererModule;

/// Load the renderer module and attach it to `window`.
///
/// The module takes the owner after the last extension, so its `rendering`
/// system is cleared and re-registered with it rather than living forever
/// under the engine's owner. A renderer that did not load, or loaded but could
/// not attach, leaves the window blank rather than ending the host: the scene
/// keeps running, and the next renderer edit loads and attaches again.
///
/// # Errors
///
/// Returns a [`HostError`] when the module cannot be started, and a
/// [`RendererError`] when the window cannot give its handles.
#[cfg(feature = "rendering")]
fn attach_renderer_module<W: RendererWindow>(
    host: &mut DevHost,
    window: W,
    width: u32,
    height: u32,
) -> Result<(RendererModule, AttachedRenderer), pill_runtime::RenderingError> {
    let renderer_module = crate::renderer_module::RendererModule::start(
        host.renderer.as_deref(),
        host.runtime.engine_mut(),
        &host.engine_api,
        &host.workspace_root,
        pill_runtime::registration::renderer_owner(host.extensions.len()),
    )?;
    let display = AttachedRenderer::attach(window, width, height, |window_data| {
        if renderer_module.slot().is_none() {
            // Already logged by the failed load.
            return Ok(Box::new(pill_renderer_api::HeadlessRenderer));
        }
        Ok(
            attach_backend(&renderer_module, window_data, width, height).unwrap_or_else(
                |failure| {
                    error!(
                        target: telemetry_target::HOT_RELOAD,
                        "the renderer could not attach to the window; it stays blank until the next renderer edit: {failure}"
                    );
                    Box::new(pill_renderer_api::HeadlessRenderer)
                },
            ),
        )
    })?;
    Ok((renderer_module, display))
}

/// Build a backend on `window_data` from the loaded renderer module.
///
/// # Errors
///
/// Returns a [`RendererError`] when the module cannot attach.
#[cfg(feature = "rendering")]
fn attach_backend(
    renderer_module: &RendererModule,
    window_data: pill_renderer_api::RawWindowData,
    width: u32,
    height: u32,
) -> Result<Box<dyn PillRenderer>, RendererError> {
    // SAFETY: every caller stores the backend in the `AttachedRenderer` of a
    // `RenderingHost`, which drops the backend before its window, and whose
    // `display` field is declared before `renderer_module`: the backend drops
    // while the window is alive and the module is mapped.
    unsafe {
        crate::renderer_module::attach_module_renderer(renderer_module, window_data, width, height)
    }
}

/// Complete rendering setup from an already-built [`DevHost`], attaching the
/// engine renderer to a supplied native window.
///
/// Frontends that must finish project setup (building and loading the project
/// module) before any window exists — for example the standalone runner — call
/// [`setup`] first and this function once a window is available, so a slow
/// first build never shows a blank surface.
///
/// # Errors
///
/// Returns the composed [`RenderingError`](pill_runtime::RenderingError):
/// a [`HostError`] when the renderer module cannot be built, loaded or
/// initialized, or a [`RendererError`] when attaching it to the window fails.
#[cfg(feature = "rendering")]
pub fn attach_renderer<W>(
    mut host: DevHost,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, pill_runtime::RenderingError>
where
    W: RendererWindow + 'static,
{
    info!(
        target: telemetry_target::RENDERING,
        width,
        height,
        "attaching the engine renderer to the window surface"
    );
    let (renderer_module, display) = attach_renderer_module(&mut host, window, width, height)?;
    let project_root = host
        .workspace_root
        .join(&host.module_config.watch_directory)
        .parent()
        .unwrap()
        .to_path_buf();
    let assets = NativeAssets::prepare(Some(&project_root))?;
    let shader_watcher = start_shader_watcher(&host);
    Ok(RenderingHost {
        host,
        display,
        renderer_module,
        assets,
        paused: None,
        shader_watcher,
        unsupported_data_check_pending: true,
    })
}

/// Publish the C# asset functions the loaded extensions offer (the renderer
/// data crate's), taking the last module that offers each name.
///
/// A project with no renderer data crate publishes none, and its C# asset
/// calls report that no renderer data provides them.
fn publish_asset_exports(extensions: &[ExtensionSlot]) {
    let found = crate::csharp::publish_asset_exports(|name| {
        extensions
            .iter()
            .rev()
            .find_map(|slot| slot.export_address(name))
    });
    info!(
        target: telemetry_target::HOT_RELOAD,
        found,
        "C# asset functions published from the loaded extensions"
    );
}

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
fn forget_prologue_records(host: &mut DevHost) {
    if let Some(session) = host.hot_patch.as_mut() {
        session.forget_prologue_patches();
    }
    for session in host.module_hot_patch.iter_mut().flatten() {
        session.forget_prologue_patches();
    }
}

/// See the `hot_patch` version above; without the feature nothing is recorded.
#[cfg(not(feature = "hot_patch"))]
fn forget_prologue_records(_host: &mut DevHost) {}

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
#[cfg(not(feature = "hot_patch"))]
fn arm_patching_thread() {}

/// Which session an in-flight patch build belongs to.
#[cfg(feature = "hot_patch")]
#[derive(Clone, Copy)]
enum PatchSubject {
    /// The project's own session.
    Project,
    /// The extension at this index.
    Module(usize),
}

/// One patch build running on its own thread.
///
/// The build compiles and maps the patch; installing it stays on the frame
/// thread, which is what [`advance_patch_attempt`] does when the report
/// arrives. The thread is joined on collection, on teardown, and nowhere
/// else - a build that outlived its host would be a thread parked in code
/// that is about to be unloaded.
#[cfg(feature = "hot_patch")]
struct InFlightPatch {
    /// Which session the build is for, and who consumes its pending
    /// generation once the patch is live.
    subject: PatchSubject,
    /// The generation counter captured when the build started. A save that
    /// lands while it compiles advances the counter past this value and must
    /// stay pending.
    pending: u64,
    /// The build's report, once it has one.
    receiver: mpsc::Receiver<(
        crate::hot_patch::PatchAttempt,
        Vec<crate::hot_patch::BodyOutcome>,
    )>,
    /// The worker, joined when the report is collected or this value drops.
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(feature = "hot_patch")]
impl Drop for InFlightPatch {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start a patch build on its own thread.
///
/// The attempt travels through its own channel rather than the closure's
/// capture, so a thread that cannot start leaves it here to build inline
/// instead of losing it with the closure.
#[cfg(feature = "hot_patch")]
fn start_patch_build(
    attempt: crate::hot_patch::PatchAttempt,
    subject: PatchSubject,
    pending: u64,
) -> InFlightPatch {
    let (report_sender, receiver) = mpsc::channel();
    let fallback_sender = report_sender.clone();
    let (job_sender, job_receiver) = mpsc::channel::<crate::hot_patch::PatchAttempt>();
    let thread = thread::Builder::new()
        .name("pill-patch-build".to_string())
        .spawn(move || {
            let Ok(attempt) = job_receiver.recv() else {
                return;
            };
            let report = crate::hot_patch::build_attempt(attempt);
            // A receiver that has gone away means the host is shutting down;
            // the report's job is then nobody's.
            let _ = report_sender.send(report);
        });
    match thread {
        Ok(thread) => {
            // Sent after the thread exists; it blocks on this receive.
            let _ = job_sender.send(attempt);
            InFlightPatch {
                subject,
                pending,
                receiver,
                thread: Some(thread),
            }
        }
        Err(_) => {
            // Building on this thread blocks the frame the way the old
            // pipeline did, which is the safe direction for a spawn failure.
            let report = crate::hot_patch::build_attempt(attempt);
            let _ = fallback_sender.send(report);
            InFlightPatch {
                subject,
                pending,
                receiver,
                thread: None,
            }
        }
    }
}

/// Collect a finished patch build and install it at this frame boundary.
///
/// Activation happens here rather than on the worker because a dispatch slot
/// write belongs to the thread that owns the frame boundary; the worker only
/// compiled and mapped the image. While the build is still running nothing is
/// consumed - the pending generation stays pending, so the reload the frame
/// loop would otherwise perform still owes it.
///
/// Returns `true` when this frame must not start another attempt: a build
/// that came back without installing its patch leaves the edit for the reload
/// path below, exactly the order the in-line pipeline had. Without that, a
/// patch that cannot compile is retried forever and the reload that would
/// deliver the edit never runs. A collected success returns `false`, because a
/// save that arrived while the build ran is a fresh edit the fast path may
/// handle immediately.
///
/// The second element is when the collected transaction began - the attempt's
/// own start. The build runs between frames, so the frame that reports it
/// cannot measure the analytics total from its own beginning; `run_reload_steps`
/// spans its total back to this instant. `None` when nothing was collected.
#[cfg(feature = "hot_patch")]
fn advance_patch_attempt(host: &mut DevHost) -> (bool, Option<Instant>) {
    let Some(mut in_flight) = host.patch_attempt.take() else {
        return (false, None);
    };
    let (attempt, outcomes) = match in_flight.receiver.try_recv() {
        Ok(report) => report,
        Err(mpsc::TryRecvError::Empty) => {
            // Still compiling; put the state back and let the frame run.
            host.patch_attempt = Some(in_flight);
            return (false, None);
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            // The worker died without a report. Nothing is consumed, so the
            // frame loop falls back to the reload path - the same place a
            // refusal would have led - and it must not be raced by a fresh
            // attempt this frame.
            warn!(
                target: telemetry_target::HOT_RELOAD,
                "the patch build stopped unexpectedly; falling back to a reload"
            );
            return (true, None);
        }
    };
    if let Some(thread) = in_flight.thread.take() {
        let _ = thread.join();
    }
    // The transaction this frame's analytics line describes began when this
    // attempt did: the build ran between frames, so the frame collecting it
    // cannot time the total from its own start.
    let attempt_started = attempt.started;

    let patched;
    {
        let DevHost {
            hot_patch,
            module_hot_patch,
            extensions,
            loaded_patches,
            loaded_project,
            runtime,
            ..
        } = &mut *host;
        let engine = runtime.engine_mut();
        let session = match in_flight.subject {
            PatchSubject::Project => hot_patch.as_mut(),
            PatchSubject::Module(index) => module_hot_patch
                .get_mut(index)
                .and_then(|slot| slot.as_mut()),
        };
        let Some(session) = session else {
            warn!(
                target: telemetry_target::HOT_RELOAD,
                "the patch build's session is gone; falling back to a reload"
            );
            return (true, Some(attempt_started));
        };
        let targets = patch_targets(loaded_project, extensions);
        let outcome = session.activate_attempt(engine, &targets, loaded_patches, attempt, outcomes);
        // The borrow of the module list ends here, so the slot below can be
        // updated.
        drop(targets);
        patched = report_patch_outcome(outcome);
        if patched {
            // The generation this build was started for is handled now; a
            // save that arrived while it compiled bumped the counter past it
            // and stays pending.
            if let PatchSubject::Module(index) = in_flight.subject {
                if let Some(slot) = extensions.get_mut(index) {
                    slot.consume_pending_reload(in_flight.pending);
                }
            }
        }
    }
    if patched {
        if matches!(in_flight.subject, PatchSubject::Project) {
            host.last_processed_source_edit = in_flight.pending;
        }
        // A patch replaced live code; the editor must refresh its metadata.
        host.bump_editor_revision();
    }
    (!patched, Some(attempt_started))
}

/// See the `hot_patch` version above; without the feature nothing is built.
#[cfg(not(feature = "hot_patch"))]
fn advance_patch_attempt(_host: &mut DevHost) -> (bool, Option<Instant>) {
    (false, None)
}

/// Whether a patch build is running on its own thread right now.
#[cfg(feature = "hot_patch")]
fn patch_attempt_in_flight(host: &DevHost) -> bool {
    host.patch_attempt.is_some()
}

/// See the `hot_patch` version above; without the feature there is none.
#[cfg(not(feature = "hot_patch"))]
fn patch_attempt_in_flight(_host: &DevHost) -> bool {
    false
}

/// Try to deliver every pending extension edit by patching, not rebuilding.
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
fn try_module_fast_path(host: &mut DevHost, may_start: bool) {
    // `may_start` is false for the frame that just collected a failed build:
    // the reload below owes the edit, and a fresh attempt would race it
    // forever.
    if !may_start || host.patch_attempt.is_some() {
        return;
    }
    // Disjoint field borrows, as the reload steps below do.
    let DevHost {
        extensions,
        module_hot_patch,
        patch_attempt,
        ..
    } = &mut *host;

    for index in 0..module_hot_patch.len() {
        // Captured before the attempt begins: a save that lands while it
        // compiles advances the counter past this value and must stay
        // pending, because nothing has delivered it.
        let Some(pending) = extensions[index].pending_reload_generation() else {
            continue;
        };
        let Some(session) = module_hot_patch[index].as_mut() else {
            continue;
        };
        match session.begin_attempt() {
            BeginOutcome::Unchanged => {}
            BeginOutcome::NotPatchable { refusal } => {
                report_patch_outcome(PatchOutcome::NotPatchable { refusal });
            }
            BeginOutcome::Failed {
                function,
                active_generation,
                failure,
            } => {
                report_patch_outcome(PatchOutcome::Failed {
                    function,
                    active_generation,
                    failure,
                });
            }
            BeginOutcome::Ready(attempt) => {
                // The build runs on its own thread; the pending generation is
                // consumed only once its replacement is installed, and every
                // reload step waits for that outcome.
                *patch_attempt = Some(start_patch_build(
                    attempt,
                    PatchSubject::Module(index),
                    pending,
                ));
                break;
            }
        }
    }
}

/// See the `hot_patch` version above; without the feature there is no fast path.
#[cfg(not(feature = "hot_patch"))]
fn try_module_fast_path(_host: &mut DevHost, _may_start: bool) {}

/// Try to deliver a pending project edit by patching, not rebuilding.
///
/// This runs at a frame boundary: reloads are already processed here, before
/// `process_frame`, so no system is executing while a dispatch slot is written.
/// A successful patch consumes the pending generation, which is what skips the
/// full rebuild; anything refused falls through to it.
#[cfg(feature = "hot_patch")]
fn try_project_fast_path(host: &mut DevHost, may_start: bool) {
    // As in the module path: a frame that just collected a failed build lets
    // the reload below deliver the edit instead of racing it.
    if !may_start || host.patch_attempt.is_some() {
        return;
    }
    let pending = host.source_edit_generation.load(Ordering::Acquire);
    if pending == host.last_processed_source_edit {
        return;
    }

    // Disjoint field borrows, as the module path does.
    let DevHost {
        hot_patch,
        patch_attempt,
        ..
    } = &mut *host;
    let Some(session) = hot_patch else {
        return;
    };
    match session.begin_attempt() {
        BeginOutcome::Unchanged => {}
        BeginOutcome::NotPatchable { refusal } => {
            report_patch_outcome(PatchOutcome::NotPatchable { refusal });
        }
        BeginOutcome::Failed {
            function,
            active_generation,
            failure,
        } => {
            report_patch_outcome(PatchOutcome::Failed {
                function,
                active_generation,
                failure,
            });
        }
        BeginOutcome::Ready(attempt) => {
            // The edit is consumed only once the replacement is installed;
            // that is what lets a save landing mid-build stay pending.
            *patch_attempt = Some(start_patch_build(attempt, PatchSubject::Project, pending));
        }
    }
}

/// See the `hot_patch` version above; without the feature there is no fast path.
#[cfg(not(feature = "hot_patch"))]
fn try_project_fast_path(_host: &mut DevHost, _may_start: bool) {}

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
fn resync_patch_baselines(host: &mut DevHost, project: bool, modules: &[usize]) {
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
#[cfg(not(feature = "hot_patch"))]
fn resync_patch_baselines(_host: &mut DevHost, _project: bool, _modules: &[usize]) {}

/// What `regenerate_module_csharp_mirror` hands back: the exposed component
/// bindings, the resolved mirror methods and heap-field accessors for the
/// managed method table, and whether the mirror file on disk changed.
type RegeneratedMirror = (
    Vec<ModuleExposedComponent>,
    Vec<crate::csharp::ResolvedMirrorMethod>,
    Vec<crate::csharp::ResolvedFieldAccessor>,
    bool,
);

/// Compute the C#-exposed bindings for one extension's current
/// generation and regenerate its `generated/<module>_Components.g.cs` mirror
/// file from that generation's real registry, value types, and mirrored
/// methods.
///
/// Returns the exposed component bindings (for the C# backend's native
/// bindings), the resolved mirror methods and container accessors (for the
/// managed method table), and whether the mirror file on disk actually
/// changed. The change flag lets the reload path queue a C# project rebuild
/// only when the C# surface moved.
fn regenerate_module_csharp_mirror(
    workspace_root: &Path,
    engine: &mut Engine,
    slot: &ExtensionSlot,
) -> Result<RegeneratedMirror, CSharpError> {
    // Each registered type name resolves to its native component, under the
    // C#-facing name a generated mirror struct is declared with.
    let exposed = crate::csharp::exposed_components_from_names(
        engine.world(),
        slot.exposed_component_names(),
    );
    let methods = slot.mirror_methods();
    let accessors = slot.field_accessors();
    let changed = crate::csharp::generate_module_components_csharp(
        workspace_root,
        slot.name(),
        &exposed,
        &slot.value_type_descriptors(),
        &methods,
        &accessors,
    )
    .map_err(|message| CSharpError::CodegenFailed { message })?;
    Ok((exposed, methods, accessors, changed))
}

/// Run every reload step of one frame: module reloads, the per-function fast
/// paths, a pending project reload, and the analytics drain that reports them.
///
/// Runs in two parts around a background patch build: with nothing in flight
/// it prepares one and starts it, and while one is running every step here is
/// deferred - the frame keeps rendering and the build is collected and
/// activated at the first boundary after it finishes.
///
/// Separated from [`run_one_frame`] so the reload work reads as one sequence,
/// apart from the frame the runtime runs after it.
fn run_reload_steps(host: &mut DevHost) -> Vec<String> {
    // Step 1: Reload any extension whose sources changed. Each module
    // owns an independent generation counter and clears only its own systems,
    // so editing one module never rebuilds another and never disturbs the
    // project's systems, entities, or resources.
    // The reload transaction begins here so the analytics total line spans
    // the whole cascade (edited module + queued project reload), not just the
    // last transaction. A collected patch transaction is the exception: its
    // build ran between frames, so the mark moves back to the attempt's own
    // start below.
    let mut reload_started = Instant::now();

    // This is the thread that owns the frame boundary, and therefore the only
    // one allowed to rewrite live code.
    arm_patching_thread();

    // Collect a background patch build that has finished. While one is still
    // running, every step below defers: a reload rebuilds the rlibs the build
    // links against, and a second attempt would race this one for the same
    // pending edit. Rendering and systems continue; the build is collected on
    // a later frame. A build that came back without installing its patch also
    // blocks new attempts for this frame, so the reload below gets to deliver
    // the edit - the order the in-line pipeline had.
    let (failed_attempt, collected_patch_started) = advance_patch_attempt(host);
    if let Some(started) = collected_patch_started {
        // The collected build ran while earlier frames kept rendering; the
        // transaction began when its attempt did, so the total spans the
        // build instead of only the milliseconds this frame spent installing
        // it.
        if started < reload_started {
            reload_started = started;
        }
    }
    if patch_attempt_in_flight(host) {
        return Vec::new();
    }

    // Step 2: Honour a rollback request, at the same frame boundary the
    // patch installs use - the prologue route rewrites live code, so this is a
    // requirement rather than a convenience.
    process_rollback_request(host);

    // Step 3: Try the per-function fast path for the extensions, before
    // the reload below turns a pending change into a full module rebuild.
    try_module_fast_path(host, !failed_attempt);
    if patch_attempt_in_flight(host) {
        // A build was started; the pending edit waits for its outcome.
        return Vec::new();
    }

    // Destructure so the module list, the engine and the API table are borrowed
    // as disjoint fields rather than through the whole host.
    let DevHost {
        extensions,
        runtime,
        engine_api,
        workspace_root,
        queued_reload_generation,
        module_config,
        ..
    } = &mut *host;
    let engine = runtime.engine_mut();
    let mut any_module_reloaded = false;
    // Which ones, not just whether any: each carries its own patch session, and
    // only the sessions whose sources were rebuilt need a new baseline.
    let mut reloaded_modules: Vec<usize> = Vec::new();
    for (index, slot) in extensions.iter_mut().enumerate() {
        match slot.reload_if_changed(engine, engine_api, workspace_root) {
            ReloadOutcome::Reloaded { generation } => {
                reloaded_modules.push(index);
                // The reloaded image is unpatched and every recorded prologue
                // address points into the previous one. Noted here and acted on
                // once the borrow below ends; forgetting is idempotent, so doing it
                // once after the loop is the same as doing it per reload.
                any_module_reloaded = true;
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    module = slot.name(),
                    generation,
                    "extension reload processed"
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
                    // Owed on the pipeline's own counter, never the watcher's:
                    // no fast path consumes this one, so a patch of the
                    // project's own edit cannot swallow this rebuild.
                    *queued_reload_generation += 1;
                }
            }
            ReloadOutcome::Failed { generation } => {
                // The old generation is still current: its patches are still
                // installed, its baselines are still accurate and its prologue
                // records are still the rollback path, so none of the success
                // bookkeeping below may run for it.
                warn!(
                    target: telemetry_target::HOT_RELOAD,
                    module = slot.name(),
                    generation,
                    "extension reload failed; keeping the previous generation and its patch state"
                );
            }
            ReloadOutcome::Unchanged => {}
        }
    }

    // Which extensions reloaded, by name, for the caller: a windowed host
    // rebuilds its renderer when the renderer's data crate is among them.
    let reloaded_extensions: Vec<String> = reloaded_modules
        .iter()
        .map(|&index| extensions[index].name().to_owned())
        .collect();

    // Step 3a: A reloaded module may be the one offering the C# asset
    // functions; its previous generation's addresses must not be used again,
    // and managed code (a queued C# reload re-runs startups) may call them next.
    if any_module_reloaded {
        publish_asset_exports(extensions);
    }

    // Step 3b: A reloaded module may have changed the C# mirror surface
    // (component fields, value types, or mirrored methods), and even a
    // body-only edit recompiles a module with mirrored methods onto a fresh
    // base, moving every trampoline address C# calls. When the active project
    // is managed, regenerate each affected module's mirror file and queue a C#
    // project reload so `project_cs.dll` rebuilds and the collectible loader
    // swaps it in — a mirrored method added (or edited) at runtime becomes
    // callable without a host restart. Modules with no C#-visible content skip
    // the rebuild entirely.
    if matches!(&module_config.backend, ProjectModuleBackend::CSharp(_)) && any_module_reloaded {
        let mut needs_csharp_reload = false;
        for index in &reloaded_modules {
            match regenerate_module_csharp_mirror(workspace_root, engine, &extensions[*index]) {
                Ok((_exposed, methods, accessors, mirror_changed)) => {
                    // A mirror-content change (fields/value types/method set)
                    // always rebuilds; a module exposing mirrored methods or
                    // heap-field accessors also rebuilds on any reload, because
                    // its trampolines live at new addresses and body edits
                    // should reach C#.
                    needs_csharp_reload |=
                        mirror_changed || !methods.is_empty() || !accessors.is_empty();
                }
                Err(error) => {
                    error!(
                        target: telemetry_target::HOT_RELOAD,
                        module = extensions[*index].name(),
                        error = %error,
                        "failed to regenerate the module's C# mirror after reload"
                    );
                }
            }
        }
        // Republish the mirror-method table from every module's current
        // generation: reloaded modules expose fresh addresses, untouched ones
        // keep the addresses they were loaded with. Container accessors are
        // appended as rows so the managed side resolves them through the same
        // lookup.
        let mut rows: Vec<crate::csharp::ResolvedMirrorMethod> = extensions
            .iter()
            .flat_map(ExtensionSlot::mirror_methods)
            .collect();
        for slot in extensions.iter() {
            rows.extend(crate::csharp::accessor_rows(&slot.field_accessors()));
        }
        crate::csharp::publish_mirror_methods(&rows);
        if needs_csharp_reload {
            info!(
                target: telemetry_target::HOT_RELOAD,
                "module reload changed the C# mirror surface; queuing a C# project reload"
            );
            *queued_reload_generation += 1;
        }
    }

    // The borrow above has ended, so the records a module reload invalidated
    // can be dropped now, and the rebuilt sources become the new patch baseline.
    if any_module_reloaded {
        forget_prologue_records(host);
        resync_patch_baselines(host, false, &reloaded_modules);
        // The set of loaded code changed; editor metadata caches must drop.
        host.bump_editor_revision();
    }

    // Step 4: Try the per-function fast path for the project, before the
    // reload below turns a pending change into a full rebuild.
    try_project_fast_path(host, !failed_attempt);
    if patch_attempt_in_flight(host) {
        // A build was started; the pending edit waits for its outcome.
        return Vec::new();
    }

    // Step 5: Process a pending project reload before running systems.
    // Two counters feed this, one meaning each. The watcher's source-edit
    // counter says an edit arrived and nothing has delivered it yet; the
    // pipeline's queued-reload counter says a rebuilt module crate still has
    // to be re-embedded into the project image. Setting a counter is how a
    // producer states its reason, and recording the observed values after the
    // reload is how the frame loop marks exactly those reasons handled - so a
    // save that arrives mid-build stays pending instead of being swallowed.
    let source_edits = host.source_edit_generation.load(Ordering::Acquire);
    let queued_reloads = host.queued_reload_generation;
    if source_edits != host.last_processed_source_edit
        || queued_reloads != host.last_processed_queued_reload
    {
        info!(
            target: telemetry_target::HOT_RELOAD,
            generation = source_edits,
            queued = queued_reloads,
            "hot reload triggered"
        );

        // Attribute the reload to the watcher signal that asked for it, so
        // the managed backend can time the whole save-to-swap span. The entry
        // is consumed here because it belongs to exactly this reload; a
        // cascade the pipeline queued itself has no source trigger and times
        // from the frame instead.
        host.loaded_project
            .arm_managed_reload_timing(host.source_triggers.take(source_edits));

        // The project image about to be replaced unmaps two generations later,
        // so every recorded prologue address inside it goes stale the moment
        // the swap commits - the same clear a module reload performs, gated
        // the same way.
        let replaced = host.loaded_project.reload(
            host.runtime.engine_mut(),
            &host.engine_api,
            &host.workspace_root,
            &host.module_config,
            // A save during the build advances the generation beyond this
            // baseline, which cancels the in-flight compilation; the next
            // frame observes the newer generation and rebuilds.
            Some((&host.source_edit_generation, source_edits)),
        );
        // A failed or refused reload keeps the current image, whose patches
        // are still installed and whose recorded prologues are still their
        // rollback route, so the records are dropped only on a real swap.
        if replaced {
            forget_prologue_records(host);
        }
        // The baseline the reload ran against, not a fresh read. A save during
        // the build advances the counter past it and cancels the compilation
        // above; recording the newer value would mark that save as handled when
        // the build it cancelled produced nothing, stranding the edit on disk.
        // Recording the baseline is what lets the next frame observe it and
        // rebuild - which is what the cancellation is for.
        host.last_processed_source_edit = source_edits;
        // The rebuild the cascade asked for has happened; a later module swap
        // bumps the counter again.
        host.last_processed_queued_reload = queued_reloads;

        // The project now runs the sources on disk, so the patch classifier's
        // baseline has to say so too. Skipping this is what makes one refused
        // patch disable the fast path for the rest of the session.
        resync_patch_baselines(host, true, &[]);
        // A project reload replaced the running image; the editor must refresh.
        host.bump_editor_revision();
    }

    // Step 6: A reload above may have re-laid out a shared component other
    // subjects registered too. Those still run code built against the old
    // layout, so their systems stay suspended until a rebuild catches them up;
    // the project normally does in Step 5 of this same boundary.
    suspend_stale_subjects(host);

    // Print the analytics line for every reload completed this frame (extensions
    //  from Step 0, the project from Step 1), plus one aggregate total.
    // The events were recorded with their build/stage/load/init/migrate
    // breakdowns already populated, so this is a pure drain-and-print.
    analytics::print_reload_events(reload_started);
    reloaded_extensions
}

/// Suspend the systems of every subject built against a shared component
/// layout a reload has since replaced, and resume the ones rebuilt since.
///
/// A stale subject's queries are already refused by the declared schema check,
/// but its systems can reach the type in other ways, such as spawning a value,
/// which would write rows in the old layout. Suspending the owner stops all of
/// it until the subject is rebuilt against the layout now registered.
fn suspend_stale_subjects(host: &mut DevHost) {
    // Step 1: Every native subject that registers data, with what is stale in
    // it. The renderer module is not among them: it holds no slot here, and a
    // renderer built against a replaced layout is covered by its own pause.
    let world = host.runtime.engine().world();
    let mut subjects: Vec<(String, pill_engine::SystemOwner, Vec<String>)> = host
        .extensions
        .iter()
        .map(|slot| {
            (
                slot.name().to_owned(),
                slot.owner(),
                slot.stale_components(world),
            )
        })
        .collect();
    subjects.push((
        host.module_config.name.clone(),
        pill_engine::SystemOwner::PROJECT,
        host.loaded_project.stale_components(world),
    ));

    // Step 2: Act only on transitions, so each is logged once.
    for (subject, owner, stale) in subjects {
        let suspended = host.stale_suspended_owners.contains(&owner);
        if !stale.is_empty() && !suspended {
            host.runtime
                .engine_mut()
                .set_systems_enabled_for_owner(owner, false);
            host.stale_suspended_owners.push(owner);
            error!(
                target: telemetry_target::HOT_RELOAD,
                subject = subject.as_str(),
                components = ?stale,
                "systems suspended: this binary was built against a previous layout of these shared components; they resume once it is rebuilt"
            );
        } else if stale.is_empty() && suspended {
            host.runtime
                .engine_mut()
                .set_systems_enabled_for_owner(owner, true);
            host.stale_suspended_owners.retain(|other| *other != owner);
            info!(
                target: telemetry_target::HOT_RELOAD,
                subject = subject.as_str(),
                "systems resumed: rebuilt against the current shared component layouts"
            );
        }
    }
}

/// Process hot reloads, execute one scheduler frame, and update FPS tracking.
///
/// Returns a report roughly every three seconds for a frontend to print or
/// display; all other frames return `None`. The two phases back to back: a
/// windowed host runs them itself, with its renderer step between them (see
/// [`RenderingHost::run_one_frame`]).
pub fn run_one_frame(host: &mut DevHost) -> Option<FrameReport> {
    let frame_start = Instant::now();
    run_reload_phase(host);
    run_frame_phase(host, frame_start)
}

/// Everything reloading does at a frame boundary, before any system runs:
/// extension and project reloads, patches, and the managed loader's swap.
///
/// Returns the names of the extensions that reloaded. Separate from
/// [`run_frame_phase`] so a windowed host can reload its renderer between the
/// two - after the data it draws reloaded, before any system reads it.
pub(crate) fn run_reload_phase(host: &mut DevHost) -> Vec<String> {
    // Steps 1 to 5: everything reloading does, in one call so this loop is
    // identical whether or not the machinery is compiled in.
    let reloaded_extensions = run_reload_steps(host);

    // Step 6: Poll the managed loader for an assembly swap.
    // The managed loader watches the built assembly instead of source files.
    // Only a reloading build has a managed loader to poll; this is the
    // fallback half of the deliberate two-mechanism design described on the
    // C# arm of `project_module::reload` (an in-process compile collapses the
    // loader's poll interval through `NotifyAssemblyReplaced`).
    if host
        .loaded_project
        .poll_managed_reload(host.runtime.engine_mut())
    {
        // The loader's debounce outlived Step 5, so the swap landed here. The
        // records go stale for the same reason and are cleared the same way.
        forget_prologue_records(host);
    }

    // Step 7: Apply asset edits, after every reload of this boundary, so they
    // go through the registrations current now.
    apply_asset_changes(host);
    reloaded_extensions
}

/// Start watching the project's asset directory, or `None` when there is none
/// or the watch cannot start (logged; the host runs on without it).
fn start_asset_watcher(
    asset_directory: Option<&Path>,
) -> Option<crate::asset_watcher::AssetWatcher> {
    let asset_directory = asset_directory?;
    match crate::asset_watcher::AssetWatcher::spawn(asset_directory.to_owned()) {
        Ok(watcher) => watcher,
        Err(error) => {
            warn!(
                target: telemetry_target::HOT_RELOAD,
                directory = %asset_directory.display(),
                error = %error,
                "[assets] could not watch the project's assets; asset edits will not be reimported"
            );
            None
        }
    }
}

/// Import or reimport every asset the watcher reported, through the world's
/// import registry.
///
/// A source moved together with its metadata file is followed: the loaded
/// asset takes the new name and keeps its handle and guid, and its old path's
/// deletion is not reported. A new source is imported, writing its metadata
/// file. A loaded one is decoded again into its slot, keeping its handle and
/// guid. Nothing here ever removes an asset: a failed reimport leaves the
/// loaded value in place, and a deleted source stays loaded until the next
/// run, because components may still hold its handle.
fn apply_asset_changes(host: &mut DevHost) {
    use crate::asset_watcher::AssetChange;

    let Some(watcher) = &host.asset_watcher else {
        return;
    };
    let changes = watcher.drain();
    if changes.is_empty() {
        return;
    }
    let ensure_metadata = host.ensure_asset_metadata;
    let world = host.runtime.engine_mut().world_mut();
    // A clone, so the registry can be called against the asset manager, which
    // is a second resource of the same world.
    let Some(registry) = world.get_resource::<pill_engine::ImportRegistry>().cloned() else {
        return;
    };
    let Some(assets) = world.get_resource_mut::<pill_engine::AssetManager>() else {
        return;
    };
    // Only files a registered type imports are assets; the rest of `res`
    // (shaders, configuration, licences) is not this code's business.
    let is_asset = |source: &str| {
        Path::new(source)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| registry.type_for_extension(extension).is_some())
    };

    // Edits first: a move arrives as the new path's edit and the old path's
    // removal, and only the edit can tell it is a move.
    let (edits, removals): (Vec<AssetChange>, Vec<AssetChange>) = changes
        .into_iter()
        .partition(|change| matches!(change, AssetChange::Edited { .. }));
    let mut moved_from: Vec<String> = Vec::new();

    for change in edits.into_iter().chain(removals) {
        match change {
            AssetChange::Edited {
                source,
                through_metadata,
            } if is_asset(&source) => {
                let path = Path::new(&source);
                if let Ok(Some(old_name)) = registry.follow_move(assets, path) {
                    info!(
                        target: telemetry_target::HOT_RELOAD,
                        from = old_name.as_str(),
                        to = source.as_str(),
                        "[assets] followed a move; the asset keeps its handle and guid"
                    );
                    moved_from.push(old_name);
                    continue;
                }
                // A `.meta` written or edited for a source nothing has loaded
                // concerns no running asset; importing it here would load it
                // just because its sidecar changed.
                if through_metadata && !registry.is_loaded(assets, path) {
                    continue;
                }
                let imported =
                    registry.import(assets, path, pill_engine::MetadataPolicy::CreateIfMissing);
                match imported {
                    Ok(outcome) if !outcome.previously_loaded => info!(
                        target: telemetry_target::HOT_RELOAD,
                        asset = source.as_str(),
                        guid = %outcome.guid,
                        metadata = ?outcome.metadata,
                        "[assets] imported a new asset"
                    ),
                    Ok(_) => match registry.reimport(assets, path) {
                        Ok(outcome) => info!(
                            target: telemetry_target::HOT_RELOAD,
                            asset = source.as_str(),
                            guid = %outcome.guid,
                            through_metadata,
                            content_version_before = ?outcome.previous_content_version,
                            content_version = ?outcome.content_version,
                            "[assets] reimported"
                        ),
                        Err(error) => warn!(
                            target: telemetry_target::HOT_RELOAD,
                            asset = source.as_str(),
                            "[assets] reimport failed; keeping the loaded value: {error}"
                        ),
                    },
                    Err(error) => warn!(
                        target: telemetry_target::HOT_RELOAD,
                        asset = source.as_str(),
                        "[assets] reimport failed; keeping the loaded value: {error}"
                    ),
                }
            }
            // Reported only while something is loaded under that name: after
            // a move the old name resolves to nothing, even when its removal
            // arrives in a later batch than the move.
            AssetChange::SourceRemoved { source }
                if !moved_from.contains(&source)
                    && registry.is_loaded(assets, Path::new(&source)) =>
            {
                warn!(
                target: telemetry_target::HOT_RELOAD,
                asset = source.as_str(),
                    "[assets] source deleted; the asset stays loaded until the next run"
                )
            }
            // With metadata kept, a deleted `.meta` is written again: with the
            // loaded asset's guid when it is loaded, a new one otherwise.
            AssetChange::MetadataRemoved { source }
                if ensure_metadata && !moved_from.contains(&source) =>
            {
                match registry.ensure_metadata(assets, Path::new(&source)) {
                    Ok(Some(guid)) => info!(
                        target: telemetry_target::HOT_RELOAD,
                        asset = source.as_str(),
                        guid = %guid,
                        "[assets] wrote missing metadata"
                    ),
                    Ok(None) => {}
                    Err(error) => warn!(
                        target: telemetry_target::HOT_RELOAD,
                        asset = source.as_str(),
                        "[assets] could not write missing metadata: {error}"
                    ),
                }
            }
            AssetChange::MetadataRemoved { source }
                if !moved_from.contains(&source)
                    && registry.is_loaded(assets, Path::new(&source)) =>
            {
                info!(
                target: telemetry_target::HOT_RELOAD,
                asset = source.as_str(),
                    "[assets] metadata file deleted; the loaded asset keeps its guid and settings until the next run"
                )
            }
            _ => {}
        }
    }
}

/// One scheduler frame and its reporting, run by the wrapped runtime, with
/// the native update hooks after the systems.
pub(crate) fn run_frame_phase(host: &mut DevHost, frame_start: Instant) -> Option<FrameReport> {
    let DevHost {
        runtime,
        engine_api,
        loaded_project,
        extensions,
        ..
    } = host;
    runtime.run_frame_with(frame_start, |_| {
        // The native compatibility update, after the scheduler systems.
        // Managed games run entirely as scheduler systems; native games keep
        // this optional hook (`pill_module_update`), which a statically linked
        // build does not have. Extensions may export one too, and run after
        // the project so a module observes the world the project's systems
        // produced.
        loaded_project.update(engine_api);
        for slot in extensions.iter() {
            slot.update(engine_api);
        }
    })
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
fn process_rollback_request(host: &mut DevHost) {
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
        warn!(
            target: telemetry_target::HOT_RELOAD,
            "Patch rollback request `{request}` is malformed; expected \
             `function@generation`, `function@previous`, or `list`"
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
                warn!(
                    target: telemetry_target::HOT_RELOAD,
                    "Patch rollback: `{function}` has no recorded generations"
                );
                print_patch_generations(host);
                return;
            }
        }
    } else {
        match wanted.parse::<u32>() {
            Ok(generation) => generation,
            Err(_) => {
                warn!(
                    target: telemetry_target::HOT_RELOAD,
                    "Patch rollback: `{wanted}` is not a generation number or `previous`"
                );
                return;
            }
        }
    };

    if let Err(detail) = host.rollback_patch(function, generation) {
        warn!(
            target: telemetry_target::HOT_RELOAD,
            "Patch rollback of `{function}` to generation {generation} failed: {detail}"
        );
        // A rollback usually fails because the generation does not exist, so
        // show what does rather than making the developer guess.
        print_patch_generations(host);
    } else {
        // A rollback changed which implementation is live; refresh the editor.
        host.bump_editor_revision();
    }
}

/// See the `hot_patch` version above; without the feature there is nothing to
/// roll back to.
#[cfg(not(feature = "hot_patch"))]
fn process_rollback_request(_host: &mut DevHost) {}

/// Print every generation this session has installed.
///
/// Rollback is unusable without it: the request needs a number, and nothing
/// else in the host ever reports which numbers exist.
#[cfg(feature = "hot_patch")]
fn print_patch_generations(host: &DevHost) {
    let generations = host.patch_generations();
    if generations.is_empty() {
        info!(
            target: telemetry_target::HOT_RELOAD,
            "No patch generations yet; edit a function body to create one"
        );
        return;
    }
    let mut lines: Vec<String> = generations
        .iter()
        .map(|generation| {
            format!(
                "{:<48} generation {:<3} {}",
                generation.function,
                generation.number,
                format!("{:.0}s ago", generation.age_seconds).dimmed()
            )
        })
        .collect();
    lines.push(
        "generation 0 is the code each artifact was built with"
            .dimmed()
            .to_string(),
    );
    info!(
        target: telemetry_target::HOT_RELOAD,
        "{}",
        log_block_colored(
            format!("Patch generations ({} total)", generations.len()),
            lines
        )
    );
}

/// Every loaded artifact a plain-function patch must be offered to.
///
/// One entry per currently loaded library: the project and each extension.
///  A crate linked into several of them is compiled into each, so each
/// holds an independent redirect slot for the same function and all of them
/// have to be told about the replacement.
///
/// Retired generations are deliberately left out. They stay mapped only so
/// outstanding pointers into their code remain valid, and their systems have
/// already been cleared, so nothing calls their copy of the function.
#[cfg(feature = "hot_patch")]
fn patch_targets<'a>(
    loaded_project: &'a LoadedProject,
    extensions: &'a [ExtensionSlot],
) -> Vec<(&'a str, &'a crate::native_library::NativeLibrary)> {
    let mut targets = Vec::with_capacity(extensions.len() + 1);
    if let Some(library) = loaded_project.native_library() {
        targets.push(("project", library));
    }
    for slot in extensions {
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
            // `[hot] <function> LIVE <ms> ms (generation <n> via <route>` is the
            // text `devops/tests/test_hot_patch_coverage.py` parses; keep it.
            let note = if best_effort {
                note.yellow()
            } else {
                note.dimmed()
            };
            info!(
                target: telemetry_target::HOT_RELOAD,
                "{}",
                log_block_colored(
                    format!(
                        "[hot] {function} {} {note}",
                        format!("LIVE {elapsed_milliseconds:.0} ms").green()
                    ),
                    [stages.to_string().dimmed()]
                )
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
            info!(
                target: telemetry_target::HOT_RELOAD,
                code = refusal.code,
                detail = refusal.detail.as_str(),
                "fast patch refused; falling back to a reload"
            );
            analytics::record_patch_refusal(refusal.code, refusal.detail.as_str());
            false
        }
        crate::hot_patch::PatchOutcome::Failed {
            function,
            active_generation,
            failure,
        } => {
            // The running implementation is intact, so the message says what is
            // still executing rather than only what did not happen.
            pill_core::warn!(
                target: telemetry_target::HOT_RELOAD,
                function = function.as_str(),
                code = failure.code,
                detail = failure.detail.as_str(),
                active_generation,
                "fast patch failed; the previous implementation is still running"
            );
            analytics::record_patch_failure(
                function.as_str(),
                failure.code,
                failure.detail.as_str(),
            );
            false
        }
        crate::hot_patch::PatchOutcome::Unchanged => false,
    }
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

#[cfg(all(test, feature = "rendering"))]
mod tests {
    use super::*;

    const PAUSED: Option<PauseReason> = Some(PauseReason::StaleRendererAfterDataLayoutChange);

    /// A world holding one entity with each of the engine's common components
    /// (the shared transform, the plain position), and the names they are
    /// registered under.
    fn world_with_common_components() -> (pill_engine::World, String, String) {
        let mut world = pill_engine::World::new();
        pill_engine::register_common_components(&mut world);
        world
            .create_entity()
            .with(pill_engine::TransformComponent::default())
            .with(pill_engine::Position::default())
            .build()
            .expect("registered components");
        let transform = pill_engine::common_components::TRANSFORM_SHARED_NAME.to_owned();
        let position =
            pill_engine::component::ComponentRegistry::registered_name::<pill_engine::Position>();
        (world, transform, position)
    }

    /// A shared component with entities that the renderer does not consume is
    /// reported once, with its entity count; a plain one never is.
    #[test]
    fn an_ignored_shared_component_is_reported_once() {
        let (world, transform, position) = world_with_common_components();
        let registered = vec![transform.clone(), position, transform.clone()];

        assert_eq!(
            unsupported_render_components(&world, &registered, &[]),
            vec![(transform, 1)]
        );
    }

    /// A consumed component, or one no entity carries, is not reported.
    #[test]
    fn consumed_or_unused_components_are_not_reported() {
        let (world, transform, _) = world_with_common_components();

        assert!(unsupported_render_components(
            &world,
            std::slice::from_ref(&transform),
            std::slice::from_ref(&transform)
        )
        .is_empty());
        let mut empty = pill_engine::World::new();
        pill_engine::register_common_components(&mut empty);
        let transform = pill_engine::common_components::TRANSFORM_SHARED_NAME.to_owned();
        assert!(unsupported_render_components(&empty, &[transform], &[]).is_empty());
    }

    /// A data layout change pauses rendering unless a renderer rebuilt against
    /// it attached in the same boundary.
    #[test]
    fn a_layout_change_pauses_unless_the_rebuilt_renderer_attached() {
        assert_eq!(
            pause_after_boundary(None, true, RendererStep::Attached),
            None
        );
        assert_eq!(
            pause_after_boundary(None, true, RendererStep::Failed),
            PAUSED
        );
        assert_eq!(
            pause_after_boundary(None, true, RendererStep::Unchanged),
            PAUSED
        );
    }

    /// A body-only data change, or none, never pauses: the renderer's layout
    /// still matches, even when its rebuild failed.
    #[test]
    fn without_a_layout_change_nothing_pauses() {
        assert_eq!(
            pause_after_boundary(None, false, RendererStep::Failed),
            None
        );
        assert_eq!(
            pause_after_boundary(None, false, RendererStep::Unchanged),
            None
        );
        assert_eq!(
            pause_after_boundary(None, false, RendererStep::Attached),
            None
        );
    }

    /// A pause lasts until a renderer attaches, and only that ends it.
    #[test]
    fn a_pause_ends_only_when_a_renderer_attaches() {
        assert_eq!(
            pause_after_boundary(PAUSED, false, RendererStep::Unchanged),
            PAUSED
        );
        assert_eq!(
            pause_after_boundary(PAUSED, false, RendererStep::Failed),
            PAUSED
        );
        assert_eq!(
            pause_after_boundary(PAUSED, false, RendererStep::Attached),
            None
        );
    }
}
