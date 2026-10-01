//! The GPU renderer as a loaded module: starting it, and driving the backend
//! it builds on a window.
//!
//! # Responsibilities
//!
//! - Build, load, initialize and reload the renderer module like any extension,
//!   under an owner of its own ([`RendererModule`]), and keep trying when a
//!   load fails: a renderer that does not build yet leaves the window blank
//!   until the next edit fixes it, instead of ending the host.
//! - Attach it to a window through its `pill_renderer_attach` export, and
//!   wrap the backend that returns in [`ModuleRenderer`], which the frame loop
//!   drives as an ordinary `dyn PillRenderer`.
//!
//! # Design
//!
//! The host links no renderer: wgpu exists only inside the module. The backend
//! the module returns is a `Box<Box<dyn PillRenderer>>` behind a raw pointer;
//! [`ModuleRenderer`] calls through it and, when dropped, hands it back to the
//! module's `pill_renderer_detach`, so the renderer is freed by the image that
//! allocated it while that image is still mapped. A `ModuleRenderer` must
//! therefore be dropped before the module's slot - `RenderingHost` declares
//! its fields in that order.
//!
//! The module is kept out of the host's extension list: that list reloads its
//! members on a source change, and a renderer reload has to detach the backend
//! first. `RenderingHost` reloads it instead, detaching through the reload's
//! `before_commit` hook and attaching whichever generation is current after.
//!
//! A failed load never costs the scene. The module first loads when the window
//! opens - after the project filled the world - so it loads under
//! [`FirstLoadFailure::ClearSystemsOnly`]: a refused generation clears its own
//! systems, stays mapped, and must have registered no data. Its watcher starts
//! before the first load, so the load that failed is simply tried again.

// Standard library
use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// External crates
use pill_core::error::HostError;
use pill_core::telemetry::telemetry_target;
use pill_core::{error, info};
use pill_engine::{AssetManager, Engine, EngineApi, SystemOwner};
use pill_renderer_api::{
    FrameOutcome, PillRenderer, RawWindowData, RenderCapabilities, RenderFrame, RenderMetrics,
    RenderViewport, RendererError,
};

// Current crate
use crate::config::ExtensionConfig;
use crate::extension::{ExtensionSlot, ReloadOutcome};
use crate::reload::FirstLoadFailure;

/// Export that builds a backend on a window.
const ATTACH_SYMBOL: &[u8] = b"pill_renderer_attach";

/// Export that drops a backend inside the module.
const DETACH_SYMBOL: &[u8] = b"pill_renderer_detach";

/// `pill_renderer_attach(window, width, height) -> backend or null`.
type AttachFn = unsafe extern "C" fn(*const RawWindowData, u32, u32) -> *mut c_void;

/// `pill_renderer_detach(backend)`.
type DetachFn = unsafe extern "C" fn(*mut c_void);

/// The renderer module: its slot once a generation has loaded, and the
/// watcher that drives both its reloads and the retry of a failed first load.
pub(crate) struct RendererModule {
    /// The loaded module, or `None` while no generation has loaded yet.
    slot: Option<ExtensionSlot>,
    /// Build, watch and output configuration derived from the module's name
    /// (the project's `renderer:` setting); `None` when the project selects no
    /// renderer, which leaves the module permanently unloaded.
    config: Option<ExtensionConfig>,
    /// Bumped by the watcher on every save under the module's `src/`.
    source_edit_generation: Arc<AtomicU64>,
    /// The source-edit generation the last failed first load was tried at,
    /// so a failure is retried once per edit rather than once per frame.
    last_failed_generation: u64,
    /// Owner every generation's systems are registered under.
    owner: SystemOwner,
}

/// What [`RendererModule::reload_if_changed`] did to the loaded generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RendererModuleChange {
    /// Nothing new; whatever backend exists stays valid.
    Unchanged,
    /// A generation loaded for the first time; it needs attaching.
    Loaded,
    /// An existing module reloaded (or rolled back) with this outcome; if the
    /// `before_commit` hook ran, the backend was detached and needs attaching.
    Reloaded(ReloadOutcome),
}

impl RendererModule {
    /// Watch the renderer module's sources and try its first load.
    ///
    /// A first load that fails - it does not build, load, pass the ABI check,
    /// initialize, or it registers data - is logged, not returned: the host
    /// keeps running with the window blank, and the next save tries again.
    /// The module's `init` registers the `rendering` system under `owner`; the
    /// data it draws was registered by the host before anything loaded.
    ///
    /// # Errors
    ///
    /// Returns a [`HostError`] only when the configuration is invalid or the
    /// sources cannot be watched - the two things a later edit cannot fix.
    pub(crate) fn start(
        name: Option<&str>,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        owner: SystemOwner,
    ) -> Result<Self, HostError> {
        // No renderer selected: nothing to watch or load. With no watcher the
        // edit counter never moves, so `reload_if_changed` never retries.
        let Some(name) = name else {
            info!(
                target: telemetry_target::HOT_RELOAD,
                "the project selects no renderer; the window stays blank"
            );
            return Ok(Self {
                slot: None,
                config: None,
                source_edit_generation: Arc::new(AtomicU64::new(0)),
                last_failed_generation: 0,
                owner,
            });
        };
        let config = ExtensionConfig::workspace_member(name);
        config.validate()?;
        // The watcher first: it is what retries a first load that fails below.
        let source_edit_generation = Arc::new(AtomicU64::new(0));
        crate::watcher::spawn_source_watcher(
            workspace_root.to_path_buf(),
            &config.name,
            &config.watch_directory,
            Arc::clone(&source_edit_generation),
        )?;
        let mut module = Self {
            slot: None,
            config: Some(config),
            source_edit_generation,
            last_failed_generation: 0,
            owner,
        };
        module.try_first_load(engine, engine_api, workspace_root, 0);
        Ok(module)
    }

    /// The loaded module, if a generation has loaded.
    pub(crate) fn slot(&self) -> Option<&ExtensionSlot> {
        self.slot.as_ref()
    }

    /// Owner every generation's systems are registered under.
    pub(crate) fn owner(&self) -> SystemOwner {
        self.owner
    }

    /// Ask for a rebuild and reload on the next [`Self::reload_if_changed`], as
    /// a save under the module's sources would.
    ///
    /// Used when the renderer's data crate reloaded: the renderer compiles that
    /// crate into its own image, so it must be rebuilt against the new source
    /// before it may read the data again. A no-op when no renderer is selected.
    pub(crate) fn request_rebuild(&self) {
        if self.config.is_some() {
            self.source_edit_generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Reload the module when its sources changed, or retry a first load that
    /// failed.
    ///
    /// `before_commit` is the reload's hook (see
    /// [`ExtensionSlot::reload_if_changed_with`]); a first load has no backend
    /// to detach and does not run it.
    pub(crate) fn reload_if_changed(
        &mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        before_commit: &mut dyn FnMut(),
    ) -> RendererModuleChange {
        if let Some(slot) = &mut self.slot {
            return RendererModuleChange::Reloaded(slot.reload_if_changed_with(
                engine,
                engine_api,
                workspace_root,
                before_commit,
            ));
        }
        let generation = self.source_edit_generation.load(Ordering::Acquire);
        if generation == self.last_failed_generation {
            return RendererModuleChange::Unchanged;
        }
        if self.try_first_load(engine, engine_api, workspace_root, generation) {
            RendererModuleChange::Loaded
        } else {
            RendererModuleChange::Unchanged
        }
    }

    /// Build, load and initialize the first generation from the sources as of
    /// `generation`; `true` when it loaded.
    fn try_first_load(
        &mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        generation: u64,
    ) -> bool {
        let Some(config) = &self.config else {
            return false;
        };
        let module_name = config.name.clone();
        match ExtensionSlot::start(
            engine,
            engine_api,
            workspace_root,
            config,
            self.owner,
            Arc::clone(&self.source_edit_generation),
            FirstLoadFailure::ClearSystemsOnly,
        ) {
            Ok(mut slot) => {
                // A renderer image leaves state behind - wgpu's, the driver's,
                // `tracing` callsites - that nothing can release, and evicting
                // one hung the frame loop. Every generation stays mapped.
                slot.keep_every_generation();
                // The sources this generation was built from are handled; a
                // save after this point is the next reload.
                slot.consume_pending_reload(generation);
                self.slot = Some(slot);
                info!(
                    target: telemetry_target::HOT_RELOAD,
                    module = module_name.as_str(),
                    "renderer module loaded"
                );
                true
            }
            Err(failure) => {
                self.last_failed_generation = generation;
                error!(
                    target: telemetry_target::HOT_RELOAD,
                    module = module_name.as_str(),
                    "the renderer module did not load; the window stays blank until the next renderer edit: {failure}"
                );
                false
            }
        }
    }
}

/// Build a backend on `window` through the module's attach export.
///
/// # Errors
///
/// Returns [`RendererError::Other`] when the module lacks the attach or detach
/// export, or reports that it could not attach (it logs why).
///
/// # Safety
///
/// `window` names a live window that outlives the returned renderer, and the
/// returned renderer is dropped before `module` is.
pub(crate) unsafe fn attach_module_renderer(
    module: &RendererModule,
    window: RawWindowData,
    width: u32,
    height: u32,
) -> Result<Box<dyn PillRenderer>, RendererError> {
    let library = module
        .slot()
        .ok_or_else(|| RendererError::Other {
            detail: "the renderer module has not loaded".to_owned(),
        })?
        .current_library();
    let missing = |export: &str| RendererError::Other {
        detail: format!("the renderer module does not export `{export}`"),
    };
    // SAFETY: both types are the exports' exact signatures, and the pointers
    // are used only while the module is mapped - the caller drops the renderer
    // before the module.
    let attach: AttachFn = unsafe { library.resolve_export(ATTACH_SYMBOL) }
        .ok_or_else(|| missing("pill_renderer_attach"))?;
    // SAFETY: as above.
    let detach: DetachFn = unsafe { library.resolve_export(DETACH_SYMBOL) }
        .ok_or_else(|| missing("pill_renderer_detach"))?;

    // SAFETY: `window` is a valid value on this stack for the call, and names a
    // live window that outlives the backend by this function's contract.
    let backend = unsafe { attach(&window, width, height) };
    let backend = NonNull::new(backend.cast::<Box<dyn PillRenderer>>()).ok_or_else(|| {
        RendererError::Other {
            detail: "the renderer module could not attach to the window; see the log above"
                .to_owned(),
        }
    })?;
    Ok(Box::new(ModuleRenderer { backend, detach }))
}

/// A renderer backend that lives in the renderer module.
///
/// Drives the module's `dyn PillRenderer` through the pointer the attach
/// export returned, and hands it back to the module's detach export on drop.
struct ModuleRenderer {
    /// The module's `Box<dyn PillRenderer>`, owned by this value.
    backend: NonNull<Box<dyn PillRenderer>>,
    /// The module's detach export, which frees `backend` inside the module.
    detach: DetachFn,
}

impl ModuleRenderer {
    /// The backend, for a call that only reads it.
    fn backend(&self) -> &dyn PillRenderer {
        // SAFETY: `backend` came from the module's attach export, is owned by
        // this value until drop, and the module stays mapped for that long.
        unsafe { self.backend.as_ref() }.as_ref()
    }

    /// The backend, for a call that changes it.
    fn backend_mut(&mut self) -> &mut dyn PillRenderer {
        // SAFETY: as in `backend`; `&mut self` makes this the only borrow.
        unsafe { self.backend.as_mut() }.as_mut()
    }
}

impl PillRenderer for ModuleRenderer {
    fn capabilities(&self) -> RenderCapabilities {
        self.backend().capabilities()
    }

    fn metrics(&self) -> RenderMetrics {
        self.backend().metrics()
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.backend_mut().resize(width, height);
    }

    fn set_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.backend_mut().set_viewport(viewport);
    }

    fn render(
        &mut self,
        frame: &RenderFrame,
        assets: &AssetManager,
    ) -> Result<FrameOutcome, RendererError> {
        self.backend_mut().render(frame, assets)
    }

    fn invalidate_assets(&mut self) {
        self.backend_mut().invalidate_assets();
    }
}

impl Drop for ModuleRenderer {
    fn drop(&mut self) {
        // SAFETY: the pointer came from the module's attach export and is
        // released exactly once, here; the module is still mapped because its
        // slot is dropped after this renderer.
        unsafe { (self.detach)(self.backend.as_ptr().cast()) };
    }
}
