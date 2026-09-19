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
            /// Resource ids the last `project_init` registered, so a type the
            /// project stops owning can be dropped while its image is mapped.
            registered_resource_ids: Vec<pill_engine::ResourceId>,
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
            mirror_methods: &[crate::csharp::ResolvedMirrorMethod],
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
                    // Taken before init for the same reason, and not read as zero:
                    // the log accumulates across generations, so `since(0)` would
                    // record the modules' and the engine's resources as this
                    // project generation's own claims.
                    let resource_registration_sequence =
                        engine.world().resource_registration_sequence();
                    let status = library.call_init(engine_api);
                    analytics::record_init(
                        &config.name,
                        init_started.elapsed().as_secs_f64() * 1000.0,
                    );
                    if status != 0 {
                        // Everything the failed generation owns has to be
                        // released before its image is unmapped: its systems are
                        // `Box<dyn System>` trait objects, and its data -
                        // resources it inserted, columns its entities live in -
                        // carries drop glue from the same image. Systems first,
                        // then the world by replacement, both while the image is
                        // still mapped.
                        engine.clear_systems_owned_by(pill_engine::SystemOwner::PROJECT);
                        let abandoned =
                            std::mem::replace(engine.world_mut(), pill_engine::World::new());
                        let resources = abandoned.resource_count();
                        drop(abandoned);
                        info!(
                            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                            module = config.name.as_str(),
                            resources,
                            "cleared the world before unmapping the generation that failed to initialize"
                        );
                        return Err(LibraryError::InitializationFailed { status }.into());
                    }
                    let registered_type_names = engine
                        .world()
                        .persist_type_names_registered_since(registration_sequence);
                    // What this generation registered, for the same reason the
                    // type names are kept - and claimed in the world, so one
                    // subject's retirement cannot take a shared resource out
                    // from under another.
                    let registered_resource_ids = engine
                        .world()
                        .resource_ids_registered_since(resource_registration_sequence);
                    engine
                        .world_mut()
                        .retain_resource_claims(&registered_resource_ids);
                    Ok(Self::Native {
                        current: library,
                        old_libraries: Vec::new(),
                        registered_type_names,
                        registered_resource_ids,
                    })
                }
                // The managed runtime performs assembly discovery, component
                // registration, startup commands, and system registration itself.
                ProjectModuleBackend::CSharp(config) => Ok(Self::CSharp(CSharpRuntime::start(
                    engine,
                    workspace_root,
                    config,
                    module_exposed_components,
                    mirror_methods,
                )?)),
            }
        }

        /// Rebuild and replace the active module while preserving a working old
        /// generation whenever compilation, loading, or registration fails.
        ///
        /// Returns whether the running image was replaced. Every failed
        /// branch (a build error, a load refusal, a rolled-back init) keeps the
        /// current image and reports `false`, so the caller can gate the
        /// bookkeeping that only a replacement invalidates - patch records
        /// pointing into an image the graveyard will unmap - on there being a
        /// replacement at all.
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
        ) -> bool {
            match self {
                // Native reload owns schema migration and DLL lifetime handling, so
                // keep that transaction isolated in one dedicated function.
                Self::Native {
                    current,
                    old_libraries,
                    registered_type_names,
                    registered_resource_ids,
                } => reload_native(
                    current,
                    old_libraries,
                    registered_type_names,
                    registered_resource_ids,
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
                    if !recompile_csharp(runtime, workspace_root, config, cancel_flag) {
                        return false;
                    }
                    info!(
                        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                        "C# build complete; polling managed loader"
                    );
                    // A refusal keeps the currently loaded assembly;
                    // `poll_reload` logs it once per distinct status.
                    // The loader's debounce can outlive this call, in
                    // which case the swap lands in a later frame's
                    // `poll_managed_reload`; only a swap this poll
                    // reports counts as a replacement here.
                    managed_poll_replaced_assembly(runtime, engine)
                }
            }
        }

        /// Poll the collectible managed loader after its assembly debounce.
        ///
        /// Returns whether this poll landed an assembly swap. The debounce can
        /// outlive the frame that triggered the build, so a swap - and the
        /// bookkeeping it invalidates - is observed here as often as in
        /// `reload`'s own poll.
        pub(crate) fn poll_managed_reload(&mut self, engine: &mut Engine) -> bool {
            // Source and assembly watchers have independent debounce windows. Poll
            // every frame so a successful build is eventually observed even when
            // the assembly was not ready during the source-triggered reload call.
            if let Self::CSharp(runtime) = self {
                return managed_poll_replaced_assembly(runtime, engine);
            }
            false
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

    /// Poll the managed loader and report whether it swapped the assembly.
    ///
    /// Exists so the poll's error is reported rather than dropped. `poll_reload`
    /// logs the statuses it understands, but a `CSharpError` returned through
    /// `?` - re-registration refusing an arriving assembly's systems is the one
    /// that matters - reaches the caller as a plain `Err` that both call sites
    /// used to discard. A project left with no systems and nothing on the
    /// console is the worst outcome available here, so it is logged loudly.
    fn managed_poll_replaced_assembly(runtime: &mut CSharpRuntime, engine: &mut Engine) -> bool {
        match runtime.poll_reload(engine) {
            Ok(status) => status == crate::csharp::POLL_RELOADED,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    error = %error,
                    "the managed reload poll failed; the project may now be running without its systems"
                );
                false
            }
        }
    }

    /// Produce a new C# project assembly, in-process when that is possible.
    ///
    /// Returns whether an assembly the managed loader can pick up now exists.
    ///
    /// The in-process compiler replays the compiler command line the startup
    /// build captured, which takes tens of milliseconds where a `dotnet build`
    /// of the same edit takes one to three seconds. It cannot answer every
    /// reload - a source file added or removed changes the command line itself -
    /// and it may not exist at all, so both cases fall through to the full build
    /// that every reload used to run. That build also refreshes the capture,
    /// which is what makes the reload after it fast again.
    fn recompile_csharp(
        runtime: &CSharpRuntime,
        workspace_root: &Path,
        config: &ProjectModuleConfig,
        cancel_flag: Option<(&AtomicU64, u64)>,
    ) -> bool {
        let fallback_reason = match runtime.fast_compile(workspace_root, &config.watch_directory) {
            Some(crate::csharp::FastCompileOutcome::Compiled { milliseconds }) => {
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = config.name.as_str(),
                    compile_ms = format!("{milliseconds:.1}").as_str(),
                    "C# compiled in-process"
                );
                return true;
            }
            // Errors in the developer's own source. Reported as-is and not
            // retried through MSBuild: a full build would spend seconds
            // reaching the same diagnostics.
            Some(crate::csharp::FastCompileOutcome::Failed { diagnostics }) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    "C# compilation failed; keeping the currently loaded C# project assembly"
                );
                print!("{diagnostics}");
                return false;
            }
            Some(crate::csharp::FastCompileOutcome::Unavailable { reason }) => reason,
            None => "no in-process compiler is loaded".to_string(),
        };

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            reason = fallback_reason.as_str(),
            "falling back to a full C# build"
        );
        match build_project_module(workspace_root, config, cancel_flag) {
            Ok(_) => true,
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    error = %error,
                    "C# build failed; keeping the currently loaded C# project assembly"
                );
                false
            }
        }
    }

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
        registered_resource_ids: &mut Vec<pill_engine::ResourceId>,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &ProjectModuleConfig,
        cancel_flag: Option<(&AtomicU64, u64)>,
    ) -> bool {
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
                return false;
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
                return false;
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
            registered_resource_ids,
        };
        // The project reports no component names onward; only a module's reach
        // the C# backend. The commit is the swap: `None` means the new
        // generation failed to initialize and the previous one was restored,
        // so the caller is told nothing was replaced.
        transaction
            .commit(engine, engine_api, new_library)
            .is_some()
    }
}
