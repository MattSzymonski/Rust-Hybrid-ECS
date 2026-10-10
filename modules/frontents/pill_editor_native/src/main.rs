//! Native editor executable: the PILL engine's editor frontend.
//!
//! # Responsibilities
//!
//! - Install the engine report handler and the shared telemetry stack.
//! - Own the `winit` event loop, the main window, and the editor's own GPU
//!   device that draws the editor's UI (D8, D19).
//! - Host the project and its extensions, including the reloadable editor
//!   shell module, through `pill_host` (D15).
//! - Convert the final error into one styled miette report at the single
//!   reporting boundary.
//!
//! # Design
//!
//! The executable is the part of the editor that cannot change while it
//! runs: the platform layer, the loop, and the UI presentation. Everything
//! that should reload - panels, commands, panels' state - lives in the shell
//! module this binary hosts. Changes to this binary require a restart;
//! changes to the shell module hot-patch or reload.

// Standard library
use std::path::PathBuf;

// External crates
use pill_core::error;
use pill_core::error::{engine_report, install_engine_report_handler};

// Current crate
use crate::frontend::EditorError;

/// The editor's errors, with their stable diagnostic codes.
mod frontend;
/// The window, the GPU device and the frame loop.
mod runner;
/// The devops suite's scripted input channel.
mod test_channel;

// =============================================================================
// Telemetry
// =============================================================================

/// Install the shared telemetry stack before the run loop starts.
///
/// Terminal logging is always active. A file lane is added when `ECS_LOG_DIR`
/// is set. Setup is best-effort: a failure only degrades telemetry and is
/// reported to stderr without aborting the run.
fn init_telemetry() {
    // Step 1: resolve the optional log directory from the environment.
    let file_directory = std::env::var_os("ECS_LOG_DIR").map(PathBuf::from);
    // Step 2: install the stack, reporting setup failures to stderr.
    if let Err(error) = pill_runtime::init_telemetry(file_directory) {
        // The one message that cannot go through the logger: it reports that
        // the logger itself did not install.
        eprintln!("[editor] telemetry setup failed: {error}");
    }
}

// =============================================================================
// Reporting Boundary
// =============================================================================

/// Install the report handler once and report the final error once.
///
/// # Errors
///
/// Returns the styled [`engine_report`] when the run terminates with an
/// error, after also recording the failure on the tracing lane for
/// correlation with active spans and log files.
fn main() -> miette::Result<()> {
    // Step 1: install the miette report handler before anything can fail.
    install_engine_report_handler();
    // Step 2: bring up the shared telemetry stack (best-effort).
    init_telemetry();
    // Step 3: run the editor and convert the error once.
    let outcome: Result<(), EditorError> = runner::run();
    outcome.map_err(|error| {
        // Error correlation: the fatal failure also enters the tracing lane
        // so it appears inside any active spans and log files.
        error!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            error = %error,
            "editor terminated with an error"
        );
        engine_report(error)
    })
}
