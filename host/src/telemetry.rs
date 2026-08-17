//! Application telemetry bootstrap for every host frontend.
//!
//! # Responsibilities
//!
//! - Install the shared [`pill_core::telemetry`] subscriber stack (terminal,
//!   optional file, optional Tracy) with the default engine filter.
//! - Install the shared metrics recorder when the `metrics` feature is on.
//!
//! # Design
//!
//! The host owns the executable-facing telemetry entry point so `standalone`
//! and `editor` share one consistent setup. Logging verbosity (terminal +
//! file) is independent of Tracy profiling: Tracy spans (`profile::*`) are
//! only routed when the `profiling` feature is active, and their filter is
//! never affected by terminal log levels.

// Standard library
use std::path::PathBuf;

// External crates
use tracing::level_filters::LevelFilter;

// Current crate
use pill_core::telemetry::{
    telemetry_target, LoggingConfig, TelemetryBuilder, TelemetryError, TelemetryHandles,
    DEV_LOG_TARGET,
};

// =============================================================================
// Free Functions
// =============================================================================

/// Install the engine telemetry stack and return its reload handles.
///
/// The terminal lane uses the default engine filter. When `directory` is
/// supplied, a rolling file lane is added with the same engine filter but
/// developer scratch logs (`engine::dev`) disabled; both lanes are
/// live-reloadable through the returned handles. When the `profiling`
/// feature is active, `profile::*` spans are routed to Tracy through an
/// independent filter.
///
/// # Errors
///
/// Returns [`TelemetryError`] when a configured filter directive is invalid
/// or the file appender cannot be created.
pub fn init_telemetry(
    file_log_directory: Option<PathBuf>,
) -> Result<TelemetryHandles, TelemetryError> {
    let mut builder = TelemetryBuilder::new();

    // Step 1: Add a rolling file lane when a log directory is supplied.
    if let Some(directory) = file_log_directory {
        // Permanent engine logs land in the file; scratch `engine::dev`
        // logs stay out of files by default.
        let file_config = LoggingConfig::default_engine()
            .with_directive(DEV_LOG_TARGET, LevelFilter::OFF)
            .with_directive(telemetry_target::RENDERING, LevelFilter::DEBUG);
        builder = builder.with_file_output(file_config, directory);
    }

    // Step 2: Route profiling spans to Tracy when the feature is active.
    #[cfg(feature = "profiling")]
    {
        builder = builder.with_tracy(true);
        #[cfg(feature = "profiling-fine")]
        {
            builder = builder.with_fine_profiling(true);
        }
    }

    // Step 3: Build and initialize the subscriber stack.
    let handles = builder.init()?;

    // Step 4: Install the shared metrics recorder when the feature is on.
    #[cfg(feature = "metrics")]
    {
        // Repeated numerical state flows into the shared recorder; it is
        // process-wide, so the engine and host emit into the same store.
        let _ = pill_core::metrics::install_metrics();
    }

    Ok(handles)
}
