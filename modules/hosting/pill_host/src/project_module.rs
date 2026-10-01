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
//! delegate assembly discovery and validation to the C# bridge's runtime, which
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

    // External crates
    use pill_core::error::{HostError, LibraryError};
    use pill_core::{error, info};
    use pill_engine::{Engine, EngineApi, SystemOwner};

    // Current crate
    use crate::build_runner::build_project_module;
    use crate::csharp::CSharpProject;
    use crate::native_library::NativeLibrary;
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
            /// Persistable component type names the last `pill_module_init` registered,
            /// used to detect types the next generation forgets to re-register.
            registered_type_names: Vec<String>,
            /// The schema hash of each of those types as the last
            /// `pill_module_init` declared it, to find the ones a module
            /// reload has re-laid out since.
            registered_schemas: Vec<(String, u64)>,
            /// Resource ids the last `pill_module_init` registered, so a type the
            /// project stops owning can be dropped while its image is mapped.
            registered_resource_ids: Vec<pill_engine::ResourceId>,
        },
        /// A collectible managed runtime hosting the C# project assembly,
        /// with the in-process compiler that rebuilds it.
        CSharp(CSharpProject),
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

        /// The persistable components the native project registered whose
        /// layout a module reload has replaced since; empty while it is
        /// current, and always empty for the C# backend, which registers its
        /// own descriptor components. See [`crate::reload::stale_components`].
        pub(crate) fn stale_components(&self, world: &pill_engine::World) -> Vec<String> {
            match self {
                Self::Native {
                    registered_schemas, ..
                } => crate::reload::stale_components(world, registered_schemas),
                Self::CSharp(_) => Vec::new(),
            }
        }

        /// Build and initialize the configured project backend.
        ///
        /// # Errors
        ///
        /// Returns `HostError` when the module fails to compile, when the native
        /// library cannot be loaded, or when the module's `pill_module_init` reports a
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
                    let library =
                        NativeLibrary::load_copy(&output_path, workspace_root, &config.name)?;

                    // Native modules register their components and systems
                    // through the stable EngineApi table before the first frame
                    // is run. The capture, init and failure handling are shared
                    // with the extension path - see `crate::reload`.
                    let init = crate::reload::initialize_generation(
                        engine,
                        engine_api,
                        &config.name,
                        None,
                        SystemOwner::PROJECT,
                        &library,
                        crate::reload::FirstLoadFailure::ClearWorld,
                    );
                    if init.status != 0 {
                        return Err(LibraryError::InitializationFailed {
                            status: init.status,
                        }
                        .into());
                    }
                    Ok(Self::Native {
                        current: library,
                        old_libraries: Vec::new(),
                        registered_type_names: init.registered_type_names,
                        registered_schemas: init.registered_schemas,
                        registered_resource_ids: init.registered_resource_ids,
                    })
                }
                // The managed runtime performs assembly discovery, component
                // registration, startup commands, and system registration itself.
                ProjectModuleBackend::CSharp(config) => Ok(Self::CSharp(CSharpProject::start(
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
                    registered_schemas,
                    registered_resource_ids,
                } => reload_native(
                    current,
                    old_libraries,
                    registered_type_names,
                    registered_schemas,
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
                //
                // TWO DELIBERATE MECHANISMS, ONE SWAP. The swap itself always
                // happens inside the managed loader's `PollReload`; what
                // differs is when the host asks for it:
                //
                // - The `dotnet build` fallback has no completion signal the
                //   loader can trust - it watches the assembly file, which a
                //   build may rewrite at any point - so the frame loop polls
                //   once per frame and the loader's 500 ms interval decides
                //   when the bytes are settled. That is why
                //   `poll_managed_reload` exists on the frame path.
                // - The in-process compiler wrote the file itself through an
                //   atomic rename and calls `NotifyAssemblyReplaced`, which
                //   collapses that interval so the swap lands in the same
                //   frame as the build.
                //
                // Neither path may assume the build implies the swap: a
                // rejection keeps the previous assembly, and a slow swap is
                // observed by a later frame. `managed_poll_replaced_assembly`
                // therefore reports what this poll actually landed, and the
                // caller records bookkeeping only then.
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
        /// Returns whether this poll landed an assembly swap. This is the
        /// frame path's half of the deliberate two-mechanism design described
        /// on the C# arm of `reload`: the `dotnet build` fallback has no
        /// completion signal, so every frame gives the loader a chance to
        /// observe settled bytes, while the in-process compiler short-circuits
        /// the interval through `NotifyAssemblyReplaced`. The debounce can
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
    fn managed_poll_replaced_assembly(runtime: &mut CSharpProject, engine: &mut Engine) -> bool {
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
        runtime: &CSharpProject,
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
    // Ten parameters, and they are ten distinct collaborators rather than
    // fields of an implicit struct: the five pieces of generation state this
    // mutates, the engine and its API table, where to build, what to build, and the
    // cancellation signal. Grouping them would name a type that exists only to
    // satisfy the lint, and would hide which of them this function mutates.
    #[allow(clippy::too_many_arguments)]
    fn reload_native(
        current: &mut NativeLibrary,
        old_libraries: &mut Vec<NativeLibrary>,
        registered_type_names: &mut Vec<String>,
        registered_schemas: &mut Vec<(String, u64)>,
        registered_resource_ids: &mut Vec<pill_engine::ResourceId>,
        engine: &mut Engine,
        engine_api: &EngineApi,
        workspace_root: &Path,
        config: &ProjectModuleConfig,
        cancel_flag: Option<(&AtomicU64, u64)>,
    ) -> bool {
        // Steps 1 to 3 are shared with the extension path and live in
        // `crate::reload`: compile before touching engine state, load a private
        // copy, then swap through the transaction whose step order is
        // load-bearing. A refused step returns `None`, which for the project
        // means nothing was replaced.
        let transaction = crate::reload::ReloadTransaction {
            kind: crate::reload::ReloadSubjectKind::Project,
            subject: &config.name,
            owner: SystemOwner::PROJECT,
            current,
            old_libraries,
            registered_type_names,
            registered_schemas,
            registered_resource_ids,
            graveyard_policy: crate::reload::GraveyardPolicy::Bounded,
        };
        crate::reload::build_load_and_commit(
            engine,
            engine_api,
            workspace_root,
            |cancel_flag| build_project_module(workspace_root, config, cancel_flag),
            cancel_flag,
            crate::reload::LoadValidation::None,
            &mut || {},
            transaction,
        )
        .is_some()
    }
}
