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

// External crates
use pill_core::error::{HostError, ModuleError};
use pill_core::{debug, error, info};
use pill_engine::{Engine, EngineApi, SystemOwner};

// Current crate
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
pub const OPTIONAL_MODULE_ABI_VERSION: u32 = 1;

/// Maximum number of retired generations kept mapped per module.
///
/// The immediately previous generation must stay mapped because engine-owned
/// pointers may still refer to its code; anything older can be evicted.
const MAX_GRAVEYARD_GENERATIONS: usize = 2;

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
        engine.begin_module_registration(owner);
        let status = library.call_init(engine_api);
        engine.end_module_registration();
        if status != 0 {
            return Err(ModuleError::InitializationFailed {
                module: config.name.clone(),
                status,
            }
            .into());
        }

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

        // Re-read rather than storing the captured value: a save during the
        // build advances the counter again, and the next frame must retry.
        self.last_processed_generation = self.reload_generation.load(Ordering::Acquire);
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

        // Step 3: Swap the systems. Only this module's systems are removed, so
        // the project and every other module keep running across the swap.
        let removed = engine.clear_systems_owned_by(self.owner);
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.config.name.as_str(),
            removed_systems = removed,
            "cleared the retiring generation's systems"
        );

        engine.begin_module_registration(self.owner);
        let status = new_library.call_init(engine_api);
        engine.end_module_registration();
        if status != 0 {
            // The replacement failed to register. Roll back to the previous
            // generation: init is required to be idempotent, so re-running it
            // restores the systems that were just cleared.
            error!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.config.name.as_str(),
                status,
                "new generation failed to initialize; rolling back"
            );
            engine.clear_systems_owned_by(self.owner);
            engine.begin_module_registration(self.owner);
            let rollback_status = self.current.call_init(engine_api);
            engine.end_module_registration();
            if rollback_status != 0 {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = self.config.name.as_str(),
                    status = rollback_status,
                    "rollback also failed; this module now contributes no systems"
                );
            }
            return;
        }

        // Step 4: Retire the previous library without unmapping it. Component
        // operations and persist metadata registered by that generation may
        // still be referenced by engine-owned pointers.
        self.old_libraries
            .push(std::mem::replace(&mut self.current, new_library));
        if self.old_libraries.len() > MAX_GRAVEYARD_GENERATIONS {
            // Dropping the evicted generation unmaps its module and deletes its
            // temporary file on disk.
            drop(self.old_libraries.remove(0));
        }

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.config.name.as_str(),
            entities = engine.world().entity_count(),
            graveyard = self.old_libraries.len(),
            "optional module hot reload complete"
        );
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
