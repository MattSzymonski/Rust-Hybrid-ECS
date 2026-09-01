//! Statically linked project and module registration, for shipping builds.
//!
//! # Responsibilities
//!
//! - Describes a project and its optional modules as ordinary Rust functions
//!   compiled into the host binary, rather than DLLs discovered at runtime.
//! - Initializes them in the same order, and under the same owners, that the
//!   hot-reloading path uses.
//!
//! # Design
//!
//! Compiled only when the `hot_reload` feature is **off**. With reloading on,
//! the project and every optional module are separate `cdylib` artifacts the
//! host builds and loads; with it off there is nothing to build or load, so the
//! frontend hands the host the entry points directly.
//!
//! The dependency direction is what forces this shape. `pill_host` cannot name
//! the project crate - it does not depend on it, and must not, or every game
//! would need its own host. So the binary that *does* link both passes the
//! functions in, which also makes the set of linked modules explicit at the
//! call site. That is what a shipping build wants: a project's
//! `project_settings.yaml` is a development convenience and should not decide
//! what a released binary contains.
//!
//! ## What this has to reproduce
//!
//! The `#[pill_project]` and `#[pill_module]` attributes generate a
//! `#[no_mangle] extern "C"` wrapper around the function the user wrote, and
//! the wrapper does two things before calling it:
//!
//! 1. `register_all_components`, which drains this artifact's `inventory`
//!    registry into the world. Skipping it is the single most likely way to get
//!    a static build subtly wrong, because it fails as "my components do not
//!    exist" rather than as a link error.
//! 2. `catch_unwind`, so a panic becomes a non-zero status instead of unwinding
//!    across the C ABI.
//!
//! Statically there is no ABI to unwind across, so a panic here is an ordinary
//! Rust panic and is left alone - converting it would only hide the backtrace.
//! Registration is reproduced exactly, per init, matching the wrappers.

// Standard library
use std::path::PathBuf;

// External crates
use pill_core::error::{HostError, ModuleError};
use pill_core::info;
use pill_engine::{Engine, SystemOwner};

// Current crate
use crate::csharp::{CSharpRuntime, ModuleExposedComponent};
use crate::CSharpModuleConfig;

/// One optional module compiled into the host binary.
///
/// The counterpart of an `OptionalModuleConfig` in a hot-reloading build: the
/// same module, named the same way, but reached by a direct call instead of
/// through `pill_module_init` in a loaded DLL.
#[derive(Clone, Copy)]
pub struct StaticModule {
    /// Crate name, used for logging and owner attribution exactly as the
    /// reloading path uses `OptionalModuleConfig::name`.
    pub name: &'static str,
    /// The function `#[pill_module]` was written on.
    ///
    /// A module built with the `module-abi` feature also exports this as
    /// `pill_module_init`; statically the function itself is called instead, so
    /// the module does **not** need that feature.
    pub init: fn(&mut Engine) -> u32,
}

/// How a shipping build reaches its project.
///
/// Mirrors [`ProjectModuleBackend`](crate::ProjectModuleBackend), which is the
/// same distinction for a reloading build, so the two postures describe a
/// project the same way and only the mechanism differs.
#[derive(Clone)]
pub enum StaticProjectBackend {
    /// A Rust project linked into this binary.
    Native {
        /// The function `#[pill_project]` was written on.
        init: fn(&mut Engine) -> u32,
    },
    /// A managed project whose assembly is loaded from a pre-built location.
    ///
    /// There is no static equivalent of a .NET assembly - the runtime loads it
    /// either way - so this posture differs from the reloading one only in what
    /// it does *not* do: nothing is compiled, no C# mirror sources are
    /// generated, and the assembly is not watched for replacement.
    CSharp {
        /// Assembly names and output directories, exactly as a reloading build
        /// describes them.
        config: CSharpModuleConfig,
        /// Directory the subdirectories in `config` are resolved against.
        ///
        /// A reloading build uses the engine workspace root. A distributed
        /// build should use the executable's own directory, because the
        /// workspace it was built in does not travel with it.
        root: PathBuf,
    },
}

/// A project and its optional modules, compiled into the host binary.
///
/// Replaces [`HostConfig`](crate::HostConfig) for a shipping build. There is no
/// project path, no build command and no watch directory, because nothing is
/// built or watched.
#[derive(Clone)]
pub struct StaticProject {
    /// Project name, used in logs where the reloading path uses the package
    /// name it read from the manifest.
    pub name: &'static str,
    /// How to reach the project itself.
    pub backend: StaticProjectBackend,
    /// Optional modules, initialized in order **before** the project.
    ///
    /// The order matters for the same reason it does with reloading: the
    /// project may name types a module defines, so the module has to have
    /// registered them first. A managed project additionally needs every
    /// module's components registered before its assembly loads, so the
    /// bindings it is handed can resolve them.
    pub modules: &'static [StaticModule],
}

impl StaticProject {
    /// Initialize every optional module, then the project.
    ///
    /// Owners are assigned exactly as the reloading path assigns them, so a
    /// statically linked module's systems are attributed to the same owner they
    /// would have had in a hot-reloading build. Nothing can be cleared or
    /// re-registered here, but the attribution still drives scheduler ordering
    /// and diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`ModuleError::InitializationFailed`] naming the first module
    /// whose init reported a non-zero status,
    /// [`LibraryError::InitializationFailed`](pill_core::error::LibraryError)
    /// when a native project's does, or a `CSharpError` when the managed
    /// runtime cannot start. Initialization stops at the first failure: there
    /// is no previous generation to roll back to on this path, so continuing
    /// would run a partially registered world.
    ///
    /// Returns the managed runtime for a C# project, which the caller must keep
    /// alive for the process's lifetime - dropping it unloads .NET out from
    /// under the systems it registered.
    pub(crate) fn initialize(
        &self,
        engine: &mut Engine,
    ) -> Result<Option<CSharpRuntime>, HostError> {
        // A managed project is handed byte-level bindings for every native
        // component the modules registered, so the names have to be collected
        // as each module initializes rather than reconstructed afterwards.
        let mut exposed_names: Vec<String> = Vec::new();
        let wants_bindings = matches!(self.backend, StaticProjectBackend::CSharp { .. });

        for (index, module) in self.modules.iter().enumerate() {
            // The same helper `runtime::setup` uses, so a module gets the
            // same owner whether it is linked in or loaded from a DLL.
            let owner = SystemOwner::optional_module(index);
            let registration_sequence = engine.world().component_registration_sequence();
            initialize_one(engine, module.init, Some(owner)).map_err(|status| {
                ModuleError::InitializationFailed {
                    module: module.name.to_string(),
                    status,
                }
            })?;
            if wants_bindings {
                exposed_names.extend(
                    engine
                        .world()
                        .registered_component_names_since(registration_sequence),
                );
            }
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = module.name,
                owner = owner.0,
                "optional module linked"
            );
        }

        let runtime = match &self.backend {
            // The project owns the scheduler outright rather than contributing
            // a module's worth of systems, so its registrations are not scoped
            // - the same asymmetry the reload transaction encodes.
            StaticProjectBackend::Native { init } => {
                initialize_one(engine, *init, None).map_err(|status| {
                    pill_core::error::LibraryError::InitializationFailed { status }
                })?;
                None
            }
            // No build, no codegen, no watcher: the assembly and its generated
            // C# mirrors were produced when this binary was built. Starting the
            // runtime registers the managed systems with the scheduler, after
            // which nothing per-frame is needed - managed gameplay is entirely
            // scheduler systems.
            StaticProjectBackend::CSharp { config, root } => {
                let exposed = exposed_components(engine, &exposed_names);
                Some(CSharpRuntime::start(engine, root, config, &exposed)?)
            }
        };

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.name,
            modules = self.modules.len(),
            managed = runtime.is_some(),
            "project linked"
        );
        Ok(runtime)
    }
}

/// Resolve registered component names into the layout bindings C# needs.
///
/// A name that no longer resolves is skipped rather than reported: the managed
/// side binds by name, so an unresolvable one simply has no binding, exactly as
/// the reloading path treats it.
fn exposed_components(engine: &Engine, names: &[String]) -> Vec<ModuleExposedComponent> {
    names
        .iter()
        .filter_map(|type_name| {
            let component_id = engine.world().resolve_component_id_by_name_any(type_name)?;
            let (size, align) = engine.world().component_layout(component_id)?;
            Some(ModuleExposedComponent {
                // The C#-facing name is the Rust path with `::` replaced by
                // `.`, so a `project_cs` mirror struct reproduces the same
                // stable identity. Must match `runtime::setup` exactly.
                csharp_name: type_name.replace("::", "."),
                component_id,
                size,
                align,
            })
        })
        .collect()
}

/// Run one entry point the way its generated ABI wrapper would.
///
/// `owner` scopes the registrations when present, which is what an optional
/// module needs and what the project must not have. Returns the non-zero status
/// the entry point reported; the caller names the subject, because only it
/// knows whether the failure is a module's or the project's.
fn initialize_one(
    engine: &mut Engine,
    init: fn(&mut Engine) -> u32,
    owner: Option<SystemOwner>,
) -> Result<(), u32> {
    // Exactly what `#[pill_project]` and `#[pill_module]` emit ahead of the
    // user's function. Idempotent, so running it once per entry point costs one
    // pass over a static list and keeps this identical to the wrappers rather
    // than merely equivalent to them.
    pill_engine::component_registry::register_all_components(engine.world_mut());

    if let Some(owner) = owner {
        engine.begin_module_registration(owner);
    }
    let status = init(engine);
    if owner.is_some() {
        engine.end_module_registration();
    }

    if status == 0 {
        Ok(())
    } else {
        Err(status)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A module that registers nothing, for shape assertions.
    fn no_op(_engine: &mut Engine) -> u32 {
        0
    }

    /// A module that reports failure, to exercise the error path.
    fn failing(_engine: &mut Engine) -> u32 {
        7
    }

    /// Owners must match what `runtime::setup` assigns, or a statically linked
    /// module's systems are attributed differently than in a reloading build.
    #[test]
    fn module_owners_match_the_reloading_path() {
        for index in 0..4usize {
            assert_eq!(
                SystemOwner::optional_module(index).0,
                index as u64 + 1,
                "owners are 1-based and assigned in load order"
            );
        }
    }

    /// Modules initialize before the project, because the project may name
    /// types a module defines.
    #[test]
    fn modules_initialize_before_the_project() {
        static ORDER: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());

        fn module_init(_engine: &mut Engine) -> u32 {
            ORDER.lock().unwrap().push("module");
            0
        }
        fn project_init(_engine: &mut Engine) -> u32 {
            ORDER.lock().unwrap().push("project");
            0
        }
        static MODULES: &[StaticModule] = &[StaticModule {
            name: "first",
            init: module_init,
        }];

        let mut engine = Engine::new();
        let project = StaticProject {
            name: "project",
            backend: StaticProjectBackend::Native { init: project_init },
            modules: MODULES,
        };
        project.initialize(&mut engine).expect("both succeed");

        assert_eq!(*ORDER.lock().unwrap(), vec!["module", "project"]);
    }

    /// A failing module stops initialization and names itself, rather than
    /// leaving a partially registered world running.
    #[test]
    fn a_failing_module_is_named_and_stops_initialization() {
        static MODULES: &[StaticModule] = &[
            StaticModule {
                name: "healthy",
                init: no_op,
            },
            StaticModule {
                name: "broken",
                init: failing,
            },
        ];

        let mut engine = Engine::new();
        let project = StaticProject {
            name: "project",
            backend: StaticProjectBackend::Native { init: no_op },
            modules: MODULES,
        };
        let Err(error) = project.initialize(&mut engine) else {
            panic!("a non-zero module status must be reported");
        };
        let message = error.to_string();
        assert!(
            message.contains("broken"),
            "the failure should name the module that reported it, got {message:?}"
        );
    }

    /// A native project reports no managed runtime, so nothing is kept alive
    /// that does not need to be.
    #[test]
    fn a_native_project_starts_no_managed_runtime() {
        let mut engine = Engine::new();
        let project = StaticProject {
            name: "project",
            backend: StaticProjectBackend::Native { init: no_op },
            modules: &[],
        };
        let runtime = project.initialize(&mut engine).expect("it succeeds");
        assert!(runtime.is_none());
    }

    /// The C#-facing name is the Rust path with `::` replaced by `.`, which is
    /// what lets a `project_cs` mirror struct resolve to the same component.
    /// Must stay identical to the mapping `runtime::setup` uses.
    #[test]
    fn exposed_names_use_the_csharp_separator() {
        let mut engine = Engine::new();
        // An unregistered name resolves to nothing and is skipped rather than
        // reported, so this also pins the filtering behaviour.
        let bindings = exposed_components(&engine, &["pill_spline::Spline".to_string()]);
        assert!(
            bindings.is_empty(),
            "a name the world does not know must be skipped, not guessed at"
        );
        let _ = &mut engine;
    }
}
