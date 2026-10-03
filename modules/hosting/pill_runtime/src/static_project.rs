//! Statically linked project and module registration, for shipping builds.
//!
//! # Responsibilities
//!
//! - Describes a project and its extensions as ordinary Rust functions
//!   compiled into the binary, rather than DLLs discovered at runtime.
//! - Initializes them in the same order, and under the same owners, that the
//!   hot-reloading host uses.
//! - Defines [`ProjectBackend`], the extension point through which a project
//!   that is not Rust (a C# assembly, through `pill_csharp_bridge`) starts.
//!
//! # Design
//!
//! The dependency direction is what forces this shape. The runtime cannot name
//! the project crate - it does not depend on it, and must not, or every game
//! would need its own runtime. So the crate that *does* link both (the
//! generated shipping bundle) passes the functions in, which also makes the set
//! of linked modules explicit at the call site. That is what a shipping build
//! wants: a project's `project_settings.yaml` is a development convenience and
//! should not decide what a released binary contains.
//!
//! For the same reason the runtime names no C# type: a managed project arrives
//! as a [`StaticProjectBackend::External`] backend the bridge implements, so a
//! build that runs no C# links none of it.
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
use std::any::Any;
use std::sync::Arc;

// External crates
use pill_core::error::{HostError, LibraryError, ModuleError};
use pill_core::info;
use pill_engine::{Engine, SystemOwner};

// Current crate
use crate::registration::{extension_owner, register_scoped, PROJECT_SCOPE};
use crate::LoggingSettings;

// =============================================================================
// Types
// =============================================================================

/// One extension compiled into the binary.
///
/// The counterpart of an extension the development host loads: the same
/// module, named the same way, but reached by a direct call instead of through
/// `pill_module_init` in a loaded DLL.
#[derive(Clone, Copy)]
pub struct StaticModule {
    /// Crate name, used for logging exactly as the reloading host uses the
    /// extension's name.
    pub name: &'static str,
    /// The function `#[pill_module]` was written on.
    ///
    /// A module built with the `module-abi` feature also exports this as
    /// `pill_module_init`; statically the function itself is called instead, so
    /// the module does **not** need that feature.
    pub init: fn(&mut Engine) -> u32,
}

/// A project that registers itself by other means than a linked Rust
/// function: a C# assembly, through `pill_csharp_bridge`.
///
/// The runtime calls it once, after every extension registered, and keeps
/// what it returns until the engine is gone.
pub trait ProjectBackend {
    /// Register the project's systems and data with `engine`.
    ///
    /// `exposed_component_names` are the components the extensions registered,
    /// in registration order, for a backend that binds them (the C# bridge
    /// gives managed code byte-level access to them). The returned guard holds
    /// whatever must live as long as the systems it registered - for C#, the
    /// .NET runtime itself; dropping it earlier would unload their code.
    ///
    /// # Errors
    ///
    /// Returns a [`HostError`] when the project cannot start; there is no
    /// previous generation to fall back to, so the runtime stops there.
    fn start(
        &self,
        engine: &mut Engine,
        exposed_component_names: &[String],
    ) -> Result<Box<dyn Any>, HostError>;
}

/// How a shipping build reaches its project.
#[derive(Clone)]
pub enum StaticProjectBackend {
    /// A Rust project linked into this binary.
    Native {
        /// The function `#[pill_project]` was written on.
        init: fn(&mut Engine) -> u32,
    },
    /// A project started by a backend outside the runtime - the C# bridge's
    /// CoreCLR or NativeAOT backend today.
    ///
    /// Shared rather than boxed so a [`StaticProject`] stays cloneable, as the
    /// windowed frontend needs.
    External(Arc<dyn ProjectBackend>),
}

/// The renderer a windowed shipping build links, as the two functions the
/// runtime needs from it.
///
/// Function pointers rather than a dependency: the runtime names no renderer
/// type. The shipping bundle - the one crate that depends on the renderer -
/// fills this in.
#[derive(Clone, Copy)]
pub struct StaticRenderer {
    /// The renderer's registration (`pill_master_renderer::register`), which
    /// adds the `rendering` system.
    pub init: fn(&mut Engine) -> u32,
    /// Builds a backend on a window (`pill_master_renderer::attach`), as a
    /// future: a native frontend blocks on it once, a web frontend awaits it.
    ///
    /// Unsafe for the renderer's own reason: the window must outlive the
    /// backend, which the runtime guarantees by dropping the backend first.
    pub attach: StaticRendererAttachFn,
}

/// `attach(window, width, height)`: a renderer backend being built on `window`.
pub type StaticRendererAttachFn =
    unsafe fn(pill_renderer_api::RawWindowData, u32, u32) -> pill_renderer_api::AttachFuture;

/// A project and its extensions, compiled into the binary.
///
/// Takes the place of the development host's project settings. There is no
/// project path, no build command and no watch directory, because nothing is
/// built or watched.
#[derive(Clone)]
pub struct StaticProject {
    /// Project name, used in logs and as the window title.
    pub name: &'static str,
    /// How to reach the project itself.
    pub backend: StaticProjectBackend,
    /// Extensions, initialized in order **before** the project.
    ///
    /// The order matters for the same reason it does with reloading: the
    /// project may name types a module defines, so the module has to have
    /// registered them first. A managed project additionally needs every
    /// module's components registered before its assembly loads, so the
    /// bindings it is handed can resolve them.
    pub modules: &'static [StaticModule],
    /// The renderer this binary links, when it was built to draw.
    ///
    /// `None` for a headless shipping build, which links no renderer at all.
    pub renderer: Option<StaticRenderer>,
    /// The project's `res` directory as an asset pack, embedded in the binary
    /// by the shipping bundle; mounted before anything initializes.
    ///
    /// `None` for a project without a `res` directory.
    pub asset_pack: Option<&'static [u8]>,
    /// The `logging:` section of the project's settings, as the bundle
    /// generator validated it; [`StaticLogging::NONE`] for none.
    pub logging: StaticLogging,
}

/// A project's logging settings, compiled into the binary as text.
///
/// Text rather than levels so the generated bundle needs nothing but string
/// literals; [`Self::settings`] reads it into [`LoggingSettings`].
#[derive(Clone, Copy, Debug, Default)]
pub struct StaticLogging {
    /// The `level:` key, when the settings set one.
    pub level: Option<&'static str>,
    /// The `targets:` map, as `(target, level)` pairs in file order.
    pub targets: &'static [(&'static str, &'static str)],
}

impl StaticLogging {
    /// No logging settings: the engine's defaults apply.
    pub const NONE: Self = Self {
        level: None,
        targets: &[],
    };

    /// The settings this text describes.
    ///
    /// # Errors
    ///
    /// Returns the first level or target that does not read, which only a
    /// hand-edited bundle can hold: the generator refuses both.
    pub fn settings(&self) -> Result<LoggingSettings, String> {
        LoggingSettings::from_text(self.level, self.targets)
    }
}

// =============================================================================
// Impls
// =============================================================================

impl StaticProject {
    /// Initialize every extension, then the project.
    ///
    /// Owners come from [`crate::registration`], as in the reloading host, so a
    /// statically linked module's systems are attributed to the same owner
    /// they would have had there. Nothing can be cleared or re-registered
    /// here, but the attribution still drives scheduler ordering and
    /// diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`ModuleError::InitializationFailed`] naming the first module
    /// whose init reported a non-zero status,
    /// [`LibraryError::InitializationFailed`] when a native project's does, or
    /// what an external backend returns. Initialization stops at the first
    /// failure: there is no previous generation to roll back to on this path,
    /// so continuing would run a partially registered world.
    ///
    /// Returns an external backend's guard, which the caller must keep alive
    /// for as long as the engine - see [`ProjectBackend::start`].
    pub(crate) fn initialize(
        &self,
        engine: &mut Engine,
    ) -> Result<Option<Box<dyn Any>>, HostError> {
        // An external backend is handed the components every module
        // registered, so the names are collected as each module initializes
        // rather than reconstructed afterwards. The renderer's data crate is
        // the first module, as in the reloading host (the bundle generator puts
        // it there), so modules and the project find its components registered.
        let mut exposed_names: Vec<String> = Vec::new();

        for (index, module) in self.modules.iter().enumerate() {
            let owner = extension_owner(index);
            let registration_sequence = engine.world().component_registration_sequence();
            initialize_linked(engine, module.init, Some(owner)).map_err(|status| {
                ModuleError::InitializationFailed {
                    module: module.name.to_string(),
                    status,
                }
            })?;
            exposed_names.extend(
                engine
                    .world()
                    .registered_component_names_since(registration_sequence),
            );
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = module.name,
                owner = owner.0,
                "extension linked"
            );
        }

        let guard = match &self.backend {
            StaticProjectBackend::Native { init } => {
                initialize_linked(engine, *init, PROJECT_SCOPE)
                    .map_err(|status| LibraryError::InitializationFailed { status })?;
                None
            }
            // No build, no codegen, no watcher: whatever the backend loads was
            // produced when this binary was built.
            StaticProjectBackend::External(backend) => Some(backend.start(engine, &exposed_names)?),
        };

        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = self.name,
            modules = self.modules.len(),
            external = guard.is_some(),
            "project linked"
        );
        Ok(guard)
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Register the statically linked renderer's `rendering` system under `owner`,
/// exactly as its module entry point would.
///
/// # Errors
///
/// Returns the renderer's non-zero status, or `u32::MAX` for a registration
/// error it left in the world.
#[cfg(feature = "rendering")]
pub(crate) fn initialize_static_renderer(
    engine: &mut Engine,
    renderer: StaticRenderer,
    owner: SystemOwner,
) -> Result<(), u32> {
    initialize_linked(engine, renderer.init, Some(owner))
}

/// Run one linked entry point the way its generated ABI wrapper would.
///
/// `scope` scopes the registrations when present, which is what an extension
/// needs and what the project must not have. Returns the non-zero status the
/// entry point reported; the caller names the subject, because only it knows
/// whether the failure is a module's or the project's.
fn initialize_linked(
    engine: &mut Engine,
    init: fn(&mut Engine) -> u32,
    scope: Option<SystemOwner>,
) -> Result<(), u32> {
    // Exactly what `#[pill_project]` and `#[pill_module]` emit ahead of the
    // user's function. Idempotent, so running it once per entry point costs one
    // pass over a static list and keeps this identical to the wrappers rather
    // than merely equivalent to them.
    // The failure, if any, is also recorded in the world's registration-error
    // slot, which is checked below; the direct error would duplicate it.
    let _ = pill_engine::component_registry::register_all_components(engine.world_mut());

    let status = register_scoped(engine, scope, init);

    // The generated wrappers read the world's registration-error slot once more
    // after the user's function, because resource guards are raised from that
    // code rather than from `register_all_components` above. Same here, so a
    // shared-name conflict fails this entry point instead of being recorded and
    // forgotten.
    if status != 0 {
        Err(status)
    } else if engine.world_mut().take_registration_error().is_some() {
        Err(u32::MAX)
    } else {
        Ok(())
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

    /// A backend that records the names it was handed and returns a guard.
    struct RecordingBackend {
        /// The names the runtime handed to `start`.
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl ProjectBackend for RecordingBackend {
        fn start(
            &self,
            _engine: &mut Engine,
            exposed_component_names: &[String],
        ) -> Result<Box<dyn Any>, HostError> {
            *self.seen.lock().unwrap() = exposed_component_names.to_vec();
            Ok(Box::new(42u32))
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
        fn project_module_init(_engine: &mut Engine) -> u32 {
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
            backend: StaticProjectBackend::Native {
                init: project_module_init,
            },
            modules: MODULES,
            renderer: None,
            asset_pack: None,
            logging: StaticLogging::NONE,
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
            renderer: None,
            asset_pack: None,
            logging: StaticLogging::NONE,
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

    /// A native project returns no guard, so nothing is kept alive that does
    /// not need to be.
    #[test]
    fn a_native_project_returns_no_guard() {
        let mut engine = Engine::new();
        let project = StaticProject {
            name: "project",
            backend: StaticProjectBackend::Native { init: no_op },
            modules: &[],
            renderer: None,
            asset_pack: None,
            logging: StaticLogging::NONE,
        };
        let guard = project.initialize(&mut engine).expect("it succeeds");
        assert!(guard.is_none());
    }

    /// An external backend starts after the modules, is handed the components
    /// they registered, and its guard is returned for the caller to keep.
    #[test]
    fn an_external_backend_gets_the_module_components_and_returns_its_guard() {
        fn registers_common_components(engine: &mut Engine) -> u32 {
            pill_engine::register_common_components(engine.world_mut());
            0
        }
        static MODULES: &[StaticModule] = &[StaticModule {
            name: "common",
            init: registers_common_components,
        }];

        let backend = Arc::new(RecordingBackend {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let mut engine = Engine::new();
        let project = StaticProject {
            name: "project",
            backend: StaticProjectBackend::External(backend.clone()),
            modules: MODULES,
            renderer: None,
            asset_pack: None,
            logging: StaticLogging::NONE,
        };
        let guard = project.initialize(&mut engine).expect("it succeeds");

        let guard = guard.expect("an external backend's guard is returned");
        assert_eq!(guard.downcast_ref::<u32>(), Some(&42));
        assert!(
            !backend.seen.lock().unwrap().is_empty(),
            "the backend must see what the module registered"
        );
    }
}
