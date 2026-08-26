//! Lifecycle management for optional engine modules.
//!
//! # Responsibilities
//!
//! - Builds, validates and loads one optional module during startup.
//! - Reloads a module in isolation when only its sources changed.
//! - Removes exactly that module's systems across a reload.
//!
//! # Design
//!
//! Each loaded module owns an [`OptionalModuleSlot`], which holds its
//! configuration, its [`SystemOwner`], the currently mapped library and a
//! bounded graveyard of retired ones. Reload is transactional in the same way
//! the project module's is: the replacement is compiled and loaded before any
//! engine state is touched, and a failure at any step leaves the previous
//! generation running.
//!
//! Isolation is what separates this from the project path. The module owns a
//! private reload generation counter fed by its own watcher, and its systems
//! are cleared through [`Engine::clear_systems_owned_by`] rather than the
//! global clear, so reloading one module never disturbs the project or any
//! other module.

// Standard library
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

// External crates
use pill_core::error::{HostError, ModuleError};
use pill_core::{error, info};
use pill_engine::{Engine, EngineApi, SystemOwner};

// Current crate
use crate::analytics;
use crate::build_runner::build_optional_module;
use crate::native_library::{NativeLibrary, OPTIONAL_MODULE_ENTRY_POINTS};
use crate::OptionalModuleConfig;

// =============================================================================
// Constants
// =============================================================================

/// Optional-module ABI revision this host understands.
///
/// A module reporting anything else is rejected before it is handed a pointer
/// into engine memory, because the two sides then disagree about the contract.
///
/// Read from `pill_engine` so the host and every module (whose ABI export is
/// generated from the same constant by `#[pill_module]`) can never drift.
pub use pill_engine::module_abi::MODULE_ABI_VERSION as OPTIONAL_MODULE_ABI_VERSION;

// =============================================================================
// OptionalModuleSlot
// =============================================================================

/// One loaded optional module and everything needed to reload it.
pub(crate) struct OptionalModuleSlot {
    /// How this module is built, watched and loaded.
    config: OptionalModuleConfig,
    /// Owner tag applied to every system this module registers.
    owner: SystemOwner,
    /// Currently active library.
    current: NativeLibrary,
    /// Retired generations, kept mapped because engine-owned pointers and
    /// vtables may still refer to their code.
    old_libraries: Vec<NativeLibrary>,
    /// Bumped by this module's watcher when its sources change.
    reload_generation: Arc<AtomicU64>,
    /// Last generation the frame loop acted on.
    last_processed_generation: u64,
    /// Persistable component type names the last `init` registered, used to
    /// detect types the next generation forgets to re-register.
    registered_type_names: Vec<String>,
    /// Every component type name (plain or persistable) the last `init`
    /// registered, exposed to the C# backend so `project_cs` can use the
    /// module's native components through byte-level bindings.
    exposed_component_names: Vec<String>,
}

impl OptionalModuleSlot {
    /// Build, load and initialize one optional module.
    ///
    /// # Errors
    ///
    /// Returns a [`HostError`] when the module fails to compile, cannot be
    /// loaded, reports an incompatible ABI revision, or fails to register.
    pub(crate) fn start(
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &OptionalModuleConfig,
        owner: SystemOwner,
        reload_generation: Arc<AtomicU64>,
    ) -> Result<Self, HostError> {
        // Step 1: Compile the module through the shared command runner.
        let output_path = build_optional_module(workspace_root, config, None)?;

        // Step 2: Load a uniquely named copy so the next compilation stays free
        // to replace the build output while this generation remains mapped.
        let library = NativeLibrary::load_copy(
            &output_path,
            workspace_root,
            &config.name,
            &OPTIONAL_MODULE_ENTRY_POINTS,
        )?;

        // Step 3: Check the contract before handing the module anything.
        check_abi_version(&library, &config.name)?;

        // Step 4: Register the module's components and systems under its own
        // owner, so a later reload can remove exactly these systems.
        let init_started = Instant::now();
        // Capture the registration sequence before init so the exact set of
        // persistable types this generation registered can be recorded.
        let registration_sequence = engine.world().persist_registration_sequence();
        let component_registration_sequence = engine.world().component_registration_sequence();
        engine.begin_module_registration(owner);
        let status = library.call_init(engine_api);
        engine.end_module_registration();
        analytics::record_init(&config.name, init_started.elapsed().as_secs_f64() * 1000.0);
        if status != 0 {
            return Err(ModuleError::InitializationFailed {
                module: config.name.clone(),
                status,
            }
            .into());
        }
        let registered_type_names = engine
            .world()
            .persist_type_names_registered_since(registration_sequence);
        // The general registration log covers plain and persistable types, so
        // every component this generation registered is exposed to C#.
        let exposed_component_names = engine
            .world()
            .registered_component_names_since(component_registration_sequence);

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = config.name.as_str(),
            owner = owner.0,
            "optional module loaded"
        );

        Ok(Self {
            config: config.clone(),
            owner,
            current: library,
            old_libraries: Vec::new(),
            reload_generation,
            last_processed_generation: 0,
            registered_type_names,
            exposed_component_names,
        })
    }

    /// Reload this module when its watcher signalled a source change.
    ///
    /// Returns true when a reload was attempted, so the caller can report it.
    /// Every module keeps its own counter, so this never rebuilds another
    /// module or the project.
    pub(crate) fn reload_if_changed(
        &mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
    ) -> bool {
        let generation = self.reload_generation.load(Ordering::Acquire);
        if generation == self.last_processed_generation {
            return false;
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.config.name.as_str(),
            generation,
            "optional module reload triggered"
        );
        self.reload(engine, engine_api, workspace_root, generation);

        // The generation observed BEFORE the reload, deliberately, not a fresh
        // read. A save during the build advances the counter past this value,
        // and recording the newer one would mark that save as handled when
        // nothing built it - the edit would then sit on disk, never compiled,
        // until something else happened to touch the crate. Recording the
        // baseline instead leaves the newer save pending, so the next frame
        // rebuilds with it. This is also what makes the build cancellation in
        // `run_build_command` mean anything: it aborts the moment the counter
        // moves, precisely so the newer sources win.
        self.last_processed_generation = generation;
        true
    }

    /// Invoke the optional per-frame hook, when the module exports one.
    pub(crate) fn update(&self, engine_api: &EngineApi) {
        self.current.call_update(engine_api);
    }

    /// Name of this module, used for reporting.
    pub(crate) fn name(&self) -> &str {
        &self.config.name
    }

    /// The generation this module's watcher has signalled but nothing has acted
    /// on yet, if any.
    ///
    /// Lets the per-function fast path look at a pending change before
    /// [`Self::reload_if_changed`] turns it into a full rebuild. The value is
    /// returned rather than just a flag so the caller can hand the exact
    /// generation it acted on back to [`Self::consume_pending_reload`].
    #[cfg(feature = "hot_patch")]
    pub(crate) fn pending_reload_generation(&self) -> Option<u64> {
        let generation = self.reload_generation.load(Ordering::Acquire);
        (generation != self.last_processed_generation).then_some(generation)
    }

    /// Mark one observed generation as handled without rebuilding.
    ///
    /// Called only when a patch has already delivered that edit, so the reload
    /// it would otherwise trigger has nothing left to do.
    ///
    /// Takes the generation the caller acted on rather than reading a fresh one,
    /// for the same reason [`Self::reload_if_changed`] records its baseline: a
    /// save that lands while the patch is compiling advances the counter past
    /// it, and that save has not been delivered by anything. Recording it as
    /// handled would strand the edit on disk.
    #[cfg(feature = "hot_patch")]
    pub(crate) fn consume_pending_reload(&mut self, generation: u64) {
        self.last_processed_generation = generation;
    }

    /// The module's currently loaded library, as a patch target.
    #[cfg(feature = "hot_patch")]
    pub(crate) fn current_library(&self) -> &NativeLibrary {
        &self.current
    }

    /// Every component type name the current generation registered, exposed
    /// to the C# backend for byte-level bindings.
    pub(crate) fn exposed_component_names(&self) -> &[String] {
        &self.exposed_component_names
    }

    /// Rebuild and swap one generation, keeping the previous one on any failure.
    fn reload(
        &mut self,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        generation: u64,
    ) {
        // Step 1: Compile before touching engine state, so a compiler error can
        // never remove the systems of the working generation. A newer save
        // during the build cancels it and the next frame retries.
        let output_path = match build_optional_module(
            workspace_root,
            &self.config,
            Some((&self.reload_generation, generation)),
        ) {
            Ok(path) => path,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.config.name.as_str(),
                    error = %error,
                    "build failed; keeping the old module generation"
                );
                return;
            }
        };

        // Step 2: Load and validate the replacement transactionally, leaving
        // the active generation untouched until it is ready to initialize.
        let new_library = match NativeLibrary::load_copy(
            &output_path,
            workspace_root,
            &self.config.name,
            &OPTIONAL_MODULE_ENTRY_POINTS,
        ) {
            Ok(library) => library,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.config.name.as_str(),
                    error = %error,
                    "failed to load the new library; keeping the old module generation"
                );
                return;
            }
        };
        if let Err(error) = check_abi_version(&new_library, &self.config.name) {
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.config.name.as_str(),
                error = %error,
                "rejected the new library; keeping the old module generation"
            );
            return;
        }

        // Steps 3 to 6 are identical for every subject and live in one place:
        // capture metadata, swap systems, drop forgotten types, re-home
        // columns, migrate schemas, retire the old image. Their ORDER is
        // load-bearing - see `crate::reload`.
        let transaction = crate::reload::ReloadTransaction {
            kind: crate::reload::ReloadSubjectKind::OptionalModule,
            subject: &self.config.name,
            owner: self.owner,
            current: &mut self.current,
            old_libraries: &mut self.old_libraries,
            registered_type_names: &mut self.registered_type_names,
        };
        let Some(commit) = transaction.commit(engine, engine_api, new_library) else {
            // The new generation failed to initialize and the previous one
            // was restored; nothing swapped.
            return;
        };
        // Refresh the C#-exposed component set to the new generation's
        // registrations (plain and persistable alike).
        self.exposed_component_names = commit.exposed_component_names;
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Reject a module built against a different revision of the module ABI.
///
/// Checked before the module is called, because a contract mismatch means the
/// two sides disagree about what the entry points expect.
fn check_abi_version(library: &NativeLibrary, module_name: &str) -> Result<(), ModuleError> {
    match library.abi_version() {
        Some(OPTIONAL_MODULE_ABI_VERSION) => Ok(()),
        Some(module_version) => Err(ModuleError::AbiVersionMismatch {
            module: module_name.to_string(),
            module_version,
            host_version: OPTIONAL_MODULE_ABI_VERSION,
        }),
        // A module without the export predates the versioned contract and
        // cannot be assumed compatible.
        None => Err(ModuleError::AbiVersionMissing {
            module: module_name.to_string(),
        }),
    }
}
