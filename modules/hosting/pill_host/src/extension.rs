//! Lifecycle management for extensions.
//!
//! # Responsibilities
//!
//! - Builds, validates and loads one extension during startup.
//! - Reloads a module in isolation when only its sources changed.
//! - Removes exactly that module's systems across a reload.
//!
//! # Design
//!
//! Each loaded module owns an [`ExtensionSlot`], which holds its
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
#[cfg(feature = "hot_reload")]
use std::path::Path;
#[cfg(feature = "hot_reload")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "hot_reload")]
use std::sync::Arc;

// External crates
#[cfg(feature = "hot_reload")]
use pill_core::error::{HostError, ModuleError};
#[cfg(feature = "hot_reload")]
use pill_core::info;
#[cfg(feature = "hot_reload")]
use pill_engine::{Engine, EngineApi, SystemOwner};

// Current crate
#[cfg(feature = "hot_reload")]
use crate::build_runner::build_extension;
#[cfg(feature = "hot_reload")]
use crate::native_library::NativeLibrary;
#[cfg(feature = "hot_reload")]
use crate::ExtensionConfig;

// =============================================================================
// Constants
// =============================================================================

/// Extension ABI revision this host understands.
///
/// A module reporting anything else is rejected before it is handed a pointer
/// into engine memory, because the two sides then disagree about the contract.
///
/// Read from `pill_engine` so the host and every module (whose ABI export is
/// generated from the same constant by `#[pill_module]`) can never drift.
pub use pill_engine::module_abi::MODULE_ABI_VERSION as EXTENSION_ABI_VERSION;

// =============================================================================
// ExtensionSlot
// =============================================================================

// Everything below loads, versions and replaces a module DLL, none of which a
// statically linked build does. The ABI constant above stays unconditional:
// it describes the contract a module crate is compiled against, which is true
// whether or not this host can load one.
#[cfg(feature = "hot_reload")]
pub(crate) use slot::{ExtensionSlot, ReloadOutcome};

#[cfg(feature = "hot_reload")]
mod slot {
    use super::*;

    /// One loaded extension and everything needed to reload it.
    pub(crate) struct ExtensionSlot {
        /// How this module is built, watched and loaded.
        config: ExtensionConfig,
        /// Owner tag applied to every system this module registers.
        owner: SystemOwner,
        /// Currently active library.
        current: NativeLibrary,
        /// Retired generations, kept mapped because engine-owned pointers and
        /// vtables may still refer to their code.
        old_libraries: Vec<NativeLibrary>,
        /// Bumped by this module's watcher when its sources change. The slot
        /// has exactly one producer - its own watcher - so this counter never
        /// conflates a source save with anything the reload pipeline queues.
        source_edit_generation: Arc<AtomicU64>,
        /// Last source-edit generation the frame loop acted on.
        last_processed_source_edit: u64,
        /// Persistable component type names the last `init` registered, used to
        /// detect types the next generation forgets to re-register.
        registered_type_names: Vec<String>,
        /// The schema hash of each of those types as the last `init` declared
        /// it, to find the ones another module's reload has re-laid out since.
        registered_schemas: Vec<(String, u64)>,
        /// Resource ids the last `init` registered, so a type this module stops
        /// owning can be dropped while its image is still mapped.
        registered_resource_ids: Vec<pill_engine::ResourceId>,
        /// Every component type name (plain or persistable) the last `init`
        /// registered, exposed to the C# backend so `project_cs` can use the
        /// module's native components through byte-level bindings.
        exposed_component_names: Vec<String>,
        /// Whether this module's retired generations may be unmapped.
        graveyard_policy: crate::reload::GraveyardPolicy,
    }

    /// What a module reload attempt did.
    ///
    /// Three states, not a `bool`: the caller performs success bookkeeping
    /// only on [`Reloaded`] - patch baselines are re-synced, prologue records
    /// are forgotten, and the editor revision moves - while a failed attempt
    /// must do none of that (the previous image is still current, its patches
    /// are still installed, and its recorded prologues are still the rollback
    /// path), and "nothing was pending" is not an attempt at all. The
    /// generation is carried so the log lines name the edit the outcome
    /// belongs to instead of whatever the counter reads by the time they run.
    ///
    /// [`Reloaded`]: ReloadOutcome::Reloaded
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum ReloadOutcome {
        /// The watcher has signalled nothing new; no attempt was made.
        Unchanged,
        /// The new generation built, initialized and was swapped in.
        Reloaded {
            /// The source-edit generation this reload delivered.
            generation: u64,
        },
        /// The build produced nothing usable or the swap was refused; the
        /// previous generation is still current.
        ///
        /// No error string: every refusal already logs its own reason where it
        /// is detected, and carrying a second copy here would print it twice.
        Failed {
            /// The source-edit generation the attempt was made for.
            generation: u64,
        },
    }

    impl ExtensionSlot {
        /// Build, load and initialize one extension.
        ///
        /// # Errors
        ///
        /// Returns a [`HostError`] when the module fails to compile, cannot be
        /// loaded, reports an incompatible ABI revision, or fails to register.
        pub(crate) fn start(
            engine: &mut Engine,
            engine_api: &EngineApi,
            workspace_root: &Path,
            config: &ExtensionConfig,
            owner: SystemOwner,
            source_edit_generation: Arc<AtomicU64>,
            first_load_failure: crate::reload::FirstLoadFailure,
        ) -> Result<Self, HostError> {
            // Step 1: Compile the module through the shared command runner.
            let output_path = build_extension(workspace_root, config, None)?;

            // Step 2: Load a uniquely named copy so the next compilation stays free
            // to replace the build output while this generation remains mapped.
            let library = NativeLibrary::load_copy(&output_path, workspace_root, &config.name)?;

            // Step 3: Check the contract before handing the module anything.
            library.check_module_abi(&config.name)?;

            // Step 4: Register the module's components and systems under its
            // own owner, so a later reload can remove exactly these systems.
            // The capture, init and failure handling are shared with the
            // project path - see `crate::reload`.
            // Under `ClearSystemsOnly` the module may register no data of its
            // own, so what already exists is noted first: re-registering a
            // shared component the host registered - which `#[pill_module]`'s
            // generated `init` does for every derived component it links - is
            // not new data, and must not count against it.
            let registered_before = (first_load_failure
                == crate::reload::FirstLoadFailure::ClearSystemsOnly)
                .then(|| {
                    (
                        engine.world().registered_component_names_since(0),
                        engine.world().resource_ids_registered_since(0),
                    )
                });
            let init = crate::reload::initialize_generation(
                engine,
                engine_api,
                &config.name,
                Some(owner),
                owner,
                &library,
                first_load_failure,
            );
            // Under `ClearSystemsOnly` a refused image stays mapped for good:
            // that is half of what makes keeping the world sound (see
            // `FirstLoadFailure`). It leaks one image and its staged copy,
            // which the next host startup cleans up.
            let refuse = |library: NativeLibrary, error: HostError| {
                if first_load_failure == crate::reload::FirstLoadFailure::ClearSystemsOnly {
                    std::mem::forget(library);
                }
                Err(error)
            };
            if init.status != 0 {
                return refuse(
                    library,
                    ModuleError::InitializationFailed {
                        module: config.name.clone(),
                        status: init.status,
                    }
                    .into(),
                );
            }
            // The other half: a module that may keep the world on failure
            // must own no data, so one that registered data nobody had
            // registered before it is refused.
            if let Some((components_before, resources_before)) = &registered_before {
                let new_components = init
                    .component_names
                    .iter()
                    .filter(|name| !components_before.contains(name))
                    .count();
                let new_resources = init
                    .registered_resource_ids
                    .iter()
                    .filter(|id| !resources_before.contains(id))
                    .count();
                if new_components != 0 || new_resources != 0 {
                    engine.clear_systems_owned_by(owner);
                    return refuse(
                        library,
                        ModuleError::RegisteredForbiddenData {
                            module: config.name.clone(),
                            components: new_components,
                            resources: new_resources,
                        }
                        .into(),
                    );
                }
            }

            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = config.name.as_str(),
                owner = owner.0,
                "extension loaded"
            );

            Ok(Self {
                config: config.clone(),
                owner,
                current: library,
                old_libraries: Vec::new(),
                source_edit_generation,
                last_processed_source_edit: 0,
                registered_type_names: init.registered_type_names,
                registered_schemas: init.registered_schemas,
                registered_resource_ids: init.registered_resource_ids,
                exposed_component_names: init.component_names,
                graveyard_policy: crate::reload::GraveyardPolicy::Bounded,
            })
        }

        /// Never unmap this module's retired generations.
        ///
        /// For a module whose images leave state behind that the reload
        /// transaction cannot track: see [`crate::reload::GraveyardPolicy::KeepAll`].
        #[cfg(feature = "rendering")]
        pub(crate) fn keep_every_generation(&mut self) {
            self.graveyard_policy = crate::reload::GraveyardPolicy::KeepAll;
        }

        /// Reload this module when its watcher signalled a source change.
        ///
        /// Returns [`ReloadOutcome::Reloaded`] when a new generation replaced
        /// the current one, [`ReloadOutcome::Failed`] when an attempt was made
        /// and the previous generation was kept, and
        /// [`ReloadOutcome::Unchanged`] when nothing was pending. The caller
        /// gates its success bookkeeping on the outcome: a failed attempt
        /// leaves every patch, baseline and record of the current generation
        /// in place. Every module keeps its own counter, so this never
        /// rebuilds another module or the project.
        pub(crate) fn reload_if_changed(
            &mut self,
            engine: &mut Engine,
            engine_api: &EngineApi,
            workspace_root: &Path,
        ) -> ReloadOutcome {
            self.reload_if_changed_with(engine, engine_api, workspace_root, &mut || {})
        }

        /// [`Self::reload_if_changed`], running `before_commit` once the
        /// replacement has built, loaded and passed the ABI check, immediately
        /// before it is swapped in - and not at all when an earlier step
        /// refused it.
        ///
        /// For a module whose caller holds state built on the current image
        /// that has to be released while that image is still mapped: the
        /// renderer module's GPU backend.
        pub(crate) fn reload_if_changed_with(
            &mut self,
            engine: &mut Engine,
            engine_api: &EngineApi,
            workspace_root: &Path,
            before_commit: &mut dyn FnMut(),
        ) -> ReloadOutcome {
            let generation = self.source_edit_generation.load(Ordering::Acquire);
            if generation == self.last_processed_source_edit {
                return ReloadOutcome::Unchanged;
            }

            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = self.config.name.as_str(),
                generation,
                "extension reload triggered"
            );
            let outcome = self.reload(
                engine,
                engine_api,
                workspace_root,
                generation,
                before_commit,
            );

            // The generation observed BEFORE the reload, deliberately, not a fresh
            // read. A save during the build advances the counter past this value,
            // and recording the newer one would mark that save as handled when
            // nothing built it - the edit would then sit on disk, never compiled,
            // until something else happened to touch the crate. Recording the
            // baseline instead leaves the newer save pending, so the next frame
            // rebuilds with it. This is also what makes the build cancellation in
            // `run_build_command` mean anything: it aborts the moment the counter
            // moves, precisely so the newer sources win.
            self.last_processed_source_edit = generation;
            outcome
        }

        /// Invoke the optional per-frame hook, when the module exports one.
        pub(crate) fn update(&self, engine_api: &EngineApi) {
            self.current.call_update(engine_api);
        }

        /// Name of this module, used for reporting.
        pub(crate) fn name(&self) -> &str {
            &self.config.name
        }

        /// The module's crate directory, relative to the workspace root: the
        /// parent of the source directory its watcher watches.
        #[cfg(feature = "rendering")]
        pub(crate) fn crate_directory(&self) -> &Path {
            Path::new(&self.config.watch_directory)
                .parent()
                .unwrap_or_else(|| Path::new(""))
        }

        /// The generation this module's watcher has signalled but nothing has acted
        /// on yet, if any.
        ///
        /// Lets the per-function fast path look at a pending change before
        /// [`Self::reload_if_changed`] turns it into a full rebuild. The value is
        /// returned rather than just a flag so the caller can hand the exact
        /// generation it acted on back to [`Self::consume_pending_reload`].
        #[cfg(any(feature = "hot_patch", feature = "rendering"))]
        pub(crate) fn pending_reload_generation(&self) -> Option<u64> {
            let generation = self.source_edit_generation.load(Ordering::Acquire);
            (generation != self.last_processed_source_edit).then_some(generation)
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
        #[cfg(any(feature = "hot_patch", feature = "rendering"))]
        pub(crate) fn consume_pending_reload(&mut self, generation: u64) {
            self.last_processed_source_edit = generation;
        }

        /// The module's currently loaded library: a patch target, and where the
        /// renderer module's attach and detach exports are resolved.
        #[cfg(any(feature = "hot_patch", feature = "rendering"))]
        pub(crate) fn current_library(&self) -> &NativeLibrary {
            &self.current
        }

        /// The address of the current generation's export named `name`, if it
        /// has one; how the C# bridge finds the functions a module offers by
        /// name (the renderer data crate's asset functions).
        ///
        /// Valid while this generation stays current: callers republish after
        /// every reload rather than keep an address across one.
        pub(crate) fn export_address(
            &self,
            name: &str,
        ) -> Option<pill_engine::component_registry::ExportAddress> {
            // SAFETY: the symbol is read as a plain address and never called
            // here; whoever calls it states its signature.
            let address = unsafe { self.current.resolve_export::<*const ()>(name.as_bytes()) }?;
            Some(pill_engine::component_registry::ExportAddress(address))
        }

        /// The persistable components this module registered whose layout
        /// another module's reload has replaced since; empty while it is
        /// current. See [`crate::reload::stale_components`].
        pub(crate) fn stale_components(&self, world: &pill_engine::World) -> Vec<String> {
            crate::reload::stale_components(world, &self.registered_schemas)
        }

        /// The owner tag this module's systems are registered under.
        pub(crate) fn owner(&self) -> SystemOwner {
            self.owner
        }

        /// Every component type name the current generation registered, exposed
        /// to the C# backend for byte-level bindings.
        pub(crate) fn exposed_component_names(&self) -> &[String] {
            &self.exposed_component_names
        }

        /// The current generation's `#[derive(PillMirror)]` value-type
        /// descriptors, used by the C# codegen to resolve nested struct tags.
        ///
        /// Only descriptors this module's own package declared: an artifact
        /// also carries the submissions of every crate it links (an extension
        /// depending on an extension), and those belong to the module that
        /// owns them - emitting them here would duplicate the owner's
        /// generated file with a second C# type of the same name.
        pub(crate) fn value_type_descriptors(
            &self,
        ) -> Vec<pill_engine::component_registry::PillValueTypeDescriptor> {
            let module_name = self.name();
            self.current
                .value_type_descriptors()
                .into_iter()
                .filter(|descriptor| descriptor.crate_name == module_name)
                .collect()
        }

        /// The current generation's `#[pill_mirror_method]` entries, each with
        /// the resolved address of its C-ABI trampoline, used by the C#
        /// codegen and the managed runtime's method table.
        ///
        /// Filtered to this module's own declarations for the same reason as
        /// [`Self::value_type_descriptors`]: the managed table is keyed by the
        /// declaring names, and a linked dependency's copy of the same
        /// trampoline would shadow the owner's address - a patch reaches the
        /// owner's copy, so the managed call would read stale code.
        pub(crate) fn mirror_methods(&self) -> Vec<crate::csharp::ResolvedMirrorMethod> {
            let module_name = self.name();
            self.current
                .mirror_methods()
                .into_iter()
                .filter(|method| method.crate_name == module_name)
                .collect()
        }

        /// The current generation's heap-field accessors, each with the
        /// resolved addresses of its operation trampolines, used by the C#
        /// mirror codegen and the managed runtime's method table.
        pub(crate) fn field_accessors(&self) -> Vec<crate::csharp::ResolvedFieldAccessor> {
            self.current.field_accessors()
        }

        /// Rebuild and swap one generation, keeping the previous one on any failure.
        ///
        /// Returns [`ReloadOutcome::Reloaded`] when the swap happened; every
        /// refusal on the way - build, load, ABI, or a rolled-back init -
        /// reports [`ReloadOutcome::Failed`] carrying the generation, so the
        /// caller can skip the bookkeeping that only a replaced image
        /// justifies and still log which edit was lost.
        fn reload(
            &mut self,
            engine: &mut Engine,
            engine_api: &EngineApi,
            workspace_root: &Path,
            generation: u64,
            before_commit: &mut dyn FnMut(),
        ) -> ReloadOutcome {
            // Steps 1 to 3 are shared with the project path and live in
            // `crate::reload`: compile before touching engine state (a newer
            // save during the build cancels it and the next frame retries),
            // load a private copy, verify the ABI, then swap through the
            // transaction whose step order is load-bearing.
            let transaction = crate::reload::ReloadTransaction {
                kind: crate::reload::ReloadSubjectKind::Extension,
                subject: &self.config.name,
                owner: self.owner,
                current: &mut self.current,
                old_libraries: &mut self.old_libraries,
                registered_type_names: &mut self.registered_type_names,
                registered_schemas: &mut self.registered_schemas,
                registered_resource_ids: &mut self.registered_resource_ids,
                graveyard_policy: self.graveyard_policy,
            };
            let Some(commit) = crate::reload::build_load_and_commit(
                engine,
                engine_api,
                workspace_root,
                |cancel_flag| build_extension(workspace_root, &self.config, cancel_flag),
                Some((&self.source_edit_generation, generation)),
                crate::reload::LoadValidation::ModuleAbi,
                before_commit,
                transaction,
            ) else {
                // A refused step left the previous generation running.
                return ReloadOutcome::Failed { generation };
            };
            // Refresh the C#-exposed component set to the new generation's
            // registrations (plain and persistable alike).
            self.exposed_component_names = commit.exposed_component_names;
            ReloadOutcome::Reloaded { generation }
        }
    }

    impl Drop for ExtensionSlot {
        /// Announce the unload of this module's images, current and retired.
        ///
        /// The fields drop straight after this body, and each one unmaps a
        /// native library whose exported code engine-owned pointers may still
        /// refer to. That makes this the riskiest moment in a host's life, so
        /// every module names itself in the log before its images go.
        fn drop(&mut self) {
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = %self.config.name,
                generations = self.old_libraries.len() + 1,
                "unloading extension"
            );
        }
    }
}
