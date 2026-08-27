//! Standalone host binary — thin frontend over the shared [`pill_host`] runner.
//!
//! # Responsibilities
//!
//! - Install the engine report handler and the shared telemetry stack.
//! - Delegate the headless or windowed run loop to [`pill_host::run`].
//! - Convert the final error into one styled miette report at the single
//!   reporting boundary.
//!
//! # Design
//!
//! All hot-reload, module-loading, and rendering logic lives in the shared
//! [`pill_host`] crate. This binary selects the module configuration from the
//! environment, starts the loop, and reports failures exactly once. There is
//! no window, GPU, or event-loop code here: `pill_host::run` owns those behind the
//! `rendering` feature.

// Standard library
use std::path::PathBuf;

// External crates
use pill_core::error;
#[cfg(feature = "hot_reload")]
use pill_host::HostConfig;
use pill_host::{engine_report, install_engine_report_handler};
#[cfg(not(feature = "hot_reload"))]
use pill_host::{StaticModule, StaticProject, StaticProjectBackend};

// A build with reloading compiled out has to link a project instead, or it has
// nothing to run. Caught here rather than at the first frame, because the
// mistake is in the build command and the message should say so.
#[cfg(all(not(feature = "hot_reload"), not(feature = "static_project")))]
compile_error!(
    "with `hot_reload` off this binary links its project in, so it needs the      `static_project` feature: build it as `--no-default-features --features      static_project`. Leaving both off would produce a host with no project."
);

// =============================================================================
// Static Project
// =============================================================================

/// The project and optional modules this binary links in.
///
/// The shipping counterpart of `pill_config.yaml`, and deliberately not read
/// from it: a released binary's contents are decided when it is built, not by a
/// file next to it. The module order matches the config file's, because the
/// project names types `pill_spline` defines and so must initialize after it.
#[cfg(not(feature = "hot_reload"))]
const STATIC_MODULES: &[StaticModule] = &[
    StaticModule {
        name: "pill_spline",
        init: pill_spline::register,
    },
    StaticModule {
        name: "pill_dummy_math",
        init: pill_dummy_math::register,
    },
    StaticModule {
        name: "pill_dummy_text",
        init: pill_dummy_text::register,
    },
    StaticModule {
        name: "pill_dummy_color",
        init: pill_dummy_color::register,
    },
    StaticModule {
        name: "pill_dummy_timer",
        init: pill_dummy_timer::register,
    },
    StaticModule {
        name: "pill_dummy_random",
        init: pill_dummy_random::register,
    },
];

/// Directory the managed backend resolves its assembly paths against.
///
/// The engine workspace root, which is where a development build finds the
/// assemblies `dotnet build` already produced. **A distributed build should use
/// the executable's own directory instead**: the workspace it was compiled in
/// does not travel with the binary. That substitution is the one thing a real
/// packaging step has to change here.
#[cfg(all(not(feature = "hot_reload"), feature = "static_csharp"))]
fn managed_assembly_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crate directory always has a parent")
        .to_path_buf()
}

/// The native Rust project, linked into this binary.
#[cfg(all(not(feature = "hot_reload"), not(feature = "static_csharp")))]
fn project_backend() -> StaticProjectBackend {
    StaticProjectBackend::Native {
        init: project::init,
    }
}

/// The managed project, loaded from assemblies built ahead of time.
#[cfg(all(not(feature = "hot_reload"), feature = "static_csharp"))]
fn project_backend() -> StaticProjectBackend {
    StaticProjectBackend::CSharp {
        config: pill_host::CSharpModuleConfig::new(
            "csharp_runtime",
            "pill_csharp_runtime/bin/Release/net8.0",
            "project_cs",
            "../examples/project_cs/bin/Release/net8.0",
        ),
        root: managed_assembly_root(),
    }
}

/// Assemble the project this binary links in.
///
/// A function rather than a `const` because the managed backend carries owned
/// configuration; the native one is const-constructible either way.
#[cfg(not(feature = "hot_reload"))]
fn static_project() -> StaticProject {
    StaticProject {
        name: "project",
        backend: project_backend(),
        modules: STATIC_MODULES,
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Install the shared telemetry stack before the run loop starts.
///
/// Terminal logging is always active. A file lane is added when `ECS_LOG_DIR`
/// is set. When the `profiling` feature is enabled, `profile::*` spans are
/// routed to Tracy through an independent filter.
///
/// Setup is best-effort: a failure only degrades telemetry and is reported
/// to stderr without aborting the host.
fn init_telemetry() {
    // Step 1: resolve the optional log directory from the environment.
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    // Step 2: install the stack, reporting setup failures to stderr.
    if let Err(error) = pill_host::init_telemetry(file_directory) {
        eprintln!("[standalone] telemetry setup failed: {error}");
    }
}

// =============================================================================
// Reporting Boundary
// =============================================================================

/// Install the report handler once and report the final error once.
///
/// The telemetry stack is brought up before the run loop starts so that any
/// failure is captured on every active lane.
///
/// # Errors
///
/// Returns the styled [`engine_report`] when [`pill_host::run`] terminates with an
/// error, after also recording the failure on the tracing lane for
/// correlation with active spans and log files.
fn main() -> miette::Result<()> {
    // Step 1: install the miette report handler before anything can fail.
    install_engine_report_handler();
    // Step 2: bring up the shared telemetry stack (best-effort).
    init_telemetry();
    // Step 3: delegate to the shared run loop and convert the error once.
    // A reloading build resolves what to run from the environment and
    // `pill_config.yaml`; a shipping build already has it linked in.
    #[cfg(feature = "hot_reload")]
    let outcome = pill_host::run(HostConfig::from_environment()?);
    #[cfg(not(feature = "hot_reload"))]
    let outcome = pill_host::run(static_project());

    outcome.map_err(|error| {
        // Error correlation: the fatal failure also enters the tracing lane
        // so it appears inside any active spans and log files.
        error!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            error = %error,
            "host terminated with an error"
        );
        engine_report(error)
    })
}
