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
#[cfg(not(feature = "hot_reload"))]
use pill_host::StaticProject;
use pill_host::{engine_report, install_engine_report_handler};

// A build with reloading compiled out has to link a project instead, or it has
// nothing to run. Caught here rather than at the first frame, because the
// mistake is in the build command and the message should say so. The posture
// feature is chosen by the project's scripting language: `static_project` for a
// native Rust project, `static_csharp` for a managed C# project.
#[cfg(all(
    not(feature = "hot_reload"),
    not(feature = "static_project"),
    not(feature = "static_csharp")
))]
compile_error!(
    "with `hot_reload` off this binary links its project in, so it needs the      `static_project` (native) or `static_csharp` (managed) feature: build it as      `--no-default-features --features static_project`. Leaving both off would      produce a host with no project."
);

// A shipping build links its project and modules in, so hot reloading them is
// meaningless, and `main` treats the two postures as mutually exclusive (the
// reloading path would win and the linked project would sit unused). Feature
// unification makes this easy to trigger by accident - e.g. a `--workspace`
// build where another package pulls `hot_reload` back on - so the combination
// is refused at compile time rather than discovered in a shipped binary.
#[cfg(all(
    any(feature = "hot_reload", feature = "hot_patch"),
    any(feature = "static_project", feature = "static_csharp")
))]
compile_error!(
    "a shipping build cannot combine hot-reload features (`hot_reload`/`hot_patch`) with `static_project`/`static_csharp`: build it as `--no-default-features --features static_project`."
);

// =============================================================================
// Static Project
// =============================================================================

/// The project and its optional modules, taken from the generated shipping
/// bundle.
///
/// The bundle is regenerated from the project's `project_settings.yaml` by
/// `devops/tools/generate_shipping_bundle.py` before a shipping build, so this
/// binary never names a project or module itself. The bundle also encodes the
/// backend - native Rust or managed C# - so both shipping features
/// (`static_project` / `static_csharp`) reach the same entry point here and
/// differ only in which posture the build tooling selected.
#[cfg(not(feature = "hot_reload"))]
fn static_project() -> StaticProject {
    pill_shipping_bundle::static_project()
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
    // `PROJECT_PATH`; a shipping build already has it linked in.
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
