//! Lifecycle management for the active native or managed project module.
//!
//! # Responsibilities
//!
//! - Builds and loads the selected backend during startup.
//! - Reloads native modules without dropping previously mapped libraries.
//! - Delegates managed reload polling to `csharp_runtime`.
//! - Keeps native component-schema migration beside the DLL swap it protects.
//!
//! # Design
//!
//! The host keeps exactly one [`LoadedProject`] alive at a time. Native backends
//! are reloaded transactionally by [`reload_native`]: the previous DLL stays
//! mapped in a bounded graveyard while changed persist schemas migrate, so
//! engine-owned pointers into retired code remain valid. Managed backends
//! delegate assembly discovery and validation to [`CSharpRuntime`], which
//! reports success or rejection through `poll_reload`.

// The whole module is the loaded-project lifecycle: build, load, initialize,
// reload, retire. A statically linked build does none of that - the project's
// entry point is called once at setup and there is no library object to keep -
// so nothing here is compiled without `hot_reload`.
#[cfg(feature = "hot_reload")]
pub(crate) use loaded::LoadedProject;

#[cfg(feature = "hot_reload")]
mod loaded {
    // Standard library
    use std::path::Path;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    // External crates
    use pill_core::error::{HostError, LibraryError};
    use pill_core::{error, info};
    use pill_engine::{Engine, EngineApi, SystemOwner};

    // Current crate
    use crate::analytics;
    use crate::build_runner::build_project_module;
    use crate::csharp::CSharpRuntime;
    use crate::native_library::{NativeLibrary, PROJECT_ENTRY_POINTS};
    use crate::{ProjectModuleBackend, ProjectModuleConfig};

    // =============================================================================
    // Constants
    // =============================================================================

    // =============================================================================
    // LoadedProject
    // =============================================================================

    /// The backend-specific state kept alive by the host loop.
    ///
    /// The enum lets the host hold either a mapped native library or a managed
    /// runtime behind one interface; the variant in use is fixed at startup and
    /// only changes across a full restart.
    pub(crate) enum LoadedProject {
        /// A mapped native module plus any retired DLLs that must stay mapped.
        Native {
            current: NativeLibrary,
            /// Old DLLs intentionally remain mapped because engine-owned function
            /// pointers and vtables may still refer to their code.
            old_libraries: Vec<NativeLibrary>,
            /// Persistable component type names the last `project_init` registered,
            /// used to detect types the next generation forgets to re-register.
            registered_type_names: Vec<String>,
        },
        /// A collectible managed runtime hosting the C# project assembly.
        CSharp(CSharpRuntime),
    }

    impl LoadedProject {
        /// The project's currently loaded native library, as a patch target.
        ///
        /// `None` for the C# backend, which has no native redirect slots to install
        /// into.
        #[cfg(feature = "hot_patch")]
        pub(crate) fn native_library(&self) -> Option<&NativeLibrary> {
            match self {
                Self::Native { current, .. } => Some(current),
                Self::CSharp(_) => None,
            }
        }

        /// Build and initialize the configured project backend.
        ///
        /// # Errors
        ///
        /// Returns `HostError` when the module fails to compile, when the native
        /// library cannot be loaded, or when the module's `project_init` reports a
        /// non-zero initialization status.
        pub(crate) fn start(
            engine: &mut Engine,
            engine_api: &EngineApi,
            workspace_root: &Path,
            config: &ProjectModuleConfig,
            module_exposed_components: &[crate::csharp::ModuleExposedComponent],
        ) -> Result<Self, HostError> {
            // Step 1: Build the module through the shared command runner.
            // Build before branching so both backends use the same command runner,
            // diagnostics, output validation, and initial failure behavior.
            let output_path = build_project_module(workspace_root, config, None)?;

            // Step 2: Initialize the backend-specific runtime.
            match &config.backend {
                ProjectModuleBackend::NativeLibrary { .. } => {
                    // Native build outputs cannot be loaded in place on Windows:
                    // the OS locks a mapped DLL. Load a uniquely named copy so the
                    // next compilation remains free to replace the original.
                    let library = NativeLibrary::load_copy(
                        &output_path,
                        workspace_root,
                        &config.name,
                        &PROJECT_ENTRY_POINTS,
                    )?;

                    // Native modules register their components and systems through
                    // the stable EngineApi table before the first frame is run.
                    let init_started = Instant::now();
                    // Capture the registration sequence before init so the exact set
                    // of persistable types this generation registered is recorded.
                    let registration_sequence = engine.world().persist_registration_sequence();
                    let status = library.call_init(engine_api);
                    analytics::record_init(
                        &config.name,
                        init_started.elapsed().as_secs_f64() * 1000.0,
                    );
                    if status != 0 {
                        return Err(LibraryError::InitializationFailed { status }.into());
                    }
                    let registered_type_names = engine
                        .world()
                        .persist_type_names_registered_since(registration_sequence);
                    Ok(Self::Native {
                        current: library,
                        old_libraries: Vec::new(),
                        registered_type_names,
                    })
                }
                // The managed runtime performs assembly discovery, component
                // registration, startup commands, and system registration itself.
                ProjectModuleBackend::CSharp(config) => Ok(Self::CSharp(CSharpRuntime::start(
                    engine,
                    workspace_root,
                    config,
                    module_exposed_components,
                )?)),
            }
        }

        /// Rebuild and replace the active module while preserving a working old
        /// generation whenever compilation, loading, or registration fails.
        ///
        /// `cancel_flag` is the watcher's reload signal: a newer save during the
        /// build aborts the in-flight compilation and the next frame retries.
        pub(crate) fn reload(
            &mut self,
            engine: &mut Engine,
            engine_api: &EngineApi,
            workspace_root: &Path,
            config: &ProjectModuleConfig,
            cancel_flag: Option<(&AtomicU64, u64)>,
        ) {
            match self {
                // Native reload owns schema migration and DLL lifetime handling, so
                // keep that transaction isolated in one dedicated function.
                Self::Native {
                    current,
                    old_libraries,
                    registered_type_names,
                } => reload_native(
                    current,
                    old_libraries,
                    registered_type_names,
                    engine,
                    engine_api,
                    workspace_root,
                    config,
                    cancel_flag,
                ),
                // C# source changes are compiled by the host. The collectible
                // managed loader validates the rebuilt assembly's component
                // manifest and system signatures before swapping; poll_reload
                // reports the outcome and logs any rejection.
                Self::CSharp(runtime) => {
                    match build_project_module(workspace_root, config, cancel_flag) {
                        Ok(_) => {
                            info!(
                                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                "C# build complete; polling managed loader"
                            );
                            runtime.poll_reload();
                        }
                        Err(error) => {
                            error!(
                                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                                error = %error,
                                "C# build failed; keeping the currently loaded C# project assembly"
                            );
                        }
                    }
                }
            }
        }

        /// Poll the collectible managed loader after its assembly debounce.
        pub(crate) fn poll_managed_reload(&mut self) {
            // Source and assembly watchers have independent debounce windows. Poll
            // every frame so a successful build is eventually observed even when
            // the assembly was not ready during the source-triggered reload call.
            if let Self::CSharp(runtime) = self {
                runtime.poll_reload();
            }
        }

        /// Invoke the native compatibility update hook after scheduler systems.
        pub(crate) fn update(&self, engine_api: &EngineApi) {
            // C# gameplay is represented entirely by registered ECS systems. Only
            // native modules retain the legacy explicit per-frame callback.
            if let Self::Native { current, .. } = self {
                current.call_update(engine_api);
            }
        }
    }

    // =============================================================================
    // Free Functions
    // =============================================================================

    /// Reload one native generation and migrate components whose persisted schema
    /// changed across the module boundary.
    // Eight parameters, and they are eight distinct collaborators rather than
    // fields of an implicit struct: the three pieces of generation state this
    // mutates, the engine and its API table, where to build, what to build, and the
    // cancellation signal. Grouping them would name a type that exists only to
    // satisfy the lint, and would hide which of them this function mutates.
    #[allow(clippy::too_many_arguments)]
    fn reload_native(
        current: &mut NativeLibrary,
        old_libraries: &mut Vec<NativeLibrary>,
        registered_type_names: &mut Vec<String>,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &ProjectModuleConfig,
        cancel_flag: Option<(&AtomicU64, u64)>,
    ) {
        // Step 1: Compile the new module before touching engine state, so a
        // compiler error can never remove the systems of the working generation.
        let output_path = match build_project_module(workspace_root, config, cancel_flag) {
            Ok(path) => path,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    error = %error,
                    "build failed; keeping the old project module"
                );
                return;
            }
        };

        // Step 2: Load and validate the replacement library transactionally.
        // Keep `current` untouched until a complete replacement library is ready
        // to initialize.
        let new_library = match NativeLibrary::load_copy(
            &output_path,
            workspace_root,
            &config.name,
            &PROJECT_ENTRY_POINTS,
        ) {
            Ok(library) => library,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    error = %error,
                    "failed to load the new library; keeping the old project module"
                );
                return;
            }
        };

        // Steps 3 to 6 are identical for every subject and live in one place:
        // capture metadata, swap systems, drop forgotten types, re-home columns,
        // migrate schemas, retire the old image. Their ORDER is load-bearing -
        // see `crate::reload`.
        let transaction = crate::reload::ReloadTransaction {
            kind: crate::reload::ReloadSubjectKind::Project,
            subject: &config.name,
            owner: SystemOwner::PROJECT,
            current,
            old_libraries,
            registered_type_names,
        };
        // The project reports no component names onward; only a module's reach
        // the C# backend.
        let _ = transaction.commit(engine, engine_api, new_library);
    }
}
