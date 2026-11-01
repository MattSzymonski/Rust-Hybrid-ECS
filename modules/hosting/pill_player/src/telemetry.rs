//! Application telemetry bootstrap for every frontend.
//!
//! # Responsibilities
//!
//! - Install the shared [`pill_core::telemetry`] subscriber stack - terminal,
//!   optional rolling file, optional console capture, optional Tracy - with
//!   the default engine filter.
//! - Apply a project's logging settings - the `logging:` section of its
//!   `project_settings.yaml` - over those defaults once the project is known
//!   ([`apply_logging_settings`]).
//! - Install the shared metrics recorder when the `metrics` feature is on.
//! - Report a frontend failure with its whole cause chain ([`report_failure`]).
//!
//! # Design
//!
//! This crate is the runtime every executable links, so it owns the
//! executable-facing telemetry entry point and every frontend shares one
//! setup. Logging verbosity (terminal and file) is independent of Tracy:
//! `profile::*` spans are routed only when the `profiling` feature is active,
//! and their filter never sees a terminal log level.
//!
//! Telemetry starts before the project's settings are read, so nothing logged
//! while finding the project is lost; the settings arrive afterwards and
//! reload the filters in place. They arrive as [`LoggingSettings`], already
//! validated: this crate reads no YAML, so the development host parses the
//! settings file and a shipping bundle carries them as text.

// Standard library
use std::path::PathBuf;
use std::sync::OnceLock;

// External crates
use tracing::level_filters::LevelFilter;

// Current crate
use pill_core::telemetry::{
    set_show_source_location, set_timestamp_format, telemetry_target, LoggingConfig,
    TelemetryBuilder, TelemetryError, TelemetryHandles, TimestampFormat, DEV_LOG_TARGET,
};
use pill_core::utils::format_error_chain;

// =============================================================================
// Shared State
// =============================================================================

/// The handles of the installed stack, kept so the project's settings can
/// reload its filters after [`init_telemetry`] returned.
static TELEMETRY_HANDLES: OnceLock<TelemetryHandles> = OnceLock::new();

// =============================================================================
// LoggingSettings
// =============================================================================

/// A project's logging settings: the `logging:` section of its
/// `project_settings.yaml`, validated.
///
/// ```yaml
/// logging:
///   level: info                 # every target, replacing the engine's defaults
///   timestamp: date_time        # time (default) or date_time
///   source_location: true       # end lines with file:line (default false)
///   targets:                    # per target, applied last
///     engine::rendering: info
///     wgpu: error
/// ```
///
/// Every key is optional, and an absent section leaves the engine's defaults
/// as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoggingSettings {
    /// One level for every target, replacing the engine's per-target defaults
    /// (the `wgpu` and `naga` dependency filters stay at `warn` unless the
    /// level is quieter, or a target override names them).
    pub level: Option<LevelFilter>,
    /// How each line writes its local time: `[hh:mm:ss:mmm]` by default, or
    /// `[dd.mm.yyyy hh:mm:ss:mmm]`.
    pub timestamp: Option<TimestampFormat>,
    /// Whether each line ends with the `file:line` that emitted it, in dark
    /// gray. Off unless the settings turn it on.
    pub source_location: Option<bool>,
    /// Per-target levels, applied after [`Self::level`], in the order given.
    /// A target is a `tracing` target or a prefix of one: `engine::rendering`,
    /// `engine`, `wgpu`.
    pub targets: Vec<(String, LevelFilter)>,
}

impl LoggingSettings {
    /// Read one level by name: `off`, `error`, `warn`, `info`, `debug` or
    /// `trace`, in any case.
    ///
    /// # Errors
    ///
    /// Returns a message naming the accepted values for anything else.
    pub fn parse_level(text: &str) -> Result<LevelFilter, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(LevelFilter::OFF),
            "error" => Ok(LevelFilter::ERROR),
            "warn" => Ok(LevelFilter::WARN),
            "info" => Ok(LevelFilter::INFO),
            "debug" => Ok(LevelFilter::DEBUG),
            "trace" => Ok(LevelFilter::TRACE),
            _ => Err(format!(
                "`{text}` is not a log level; use off, error, warn, info, debug or trace"
            )),
        }
    }

    /// Check one target name: letters, digits and the `_`, `-`, `.`, `:`
    /// characters a `tracing` target is made of.
    ///
    /// # Errors
    ///
    /// Returns a message naming the target when it holds anything else - a
    /// `=`, a `,`, a space - which would otherwise change what the filter
    /// directive means.
    pub fn check_target(target: &str) -> Result<(), String> {
        let valid = !target.is_empty()
            && target
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_-.:".contains(character));
        if valid {
            Ok(())
        } else {
            Err(format!(
                "`{target}` is not a log target; use names like `engine::rendering` or `wgpu`"
            ))
        }
    }

    /// Build the settings from the text a shipping bundle carries.
    ///
    /// # Errors
    ///
    /// Returns the message of the first level, timestamp format or target that
    /// does not read; the bundle generator validates all three, so this only
    /// fails on a hand-edited bundle.
    pub fn from_text(
        level: Option<&str>,
        timestamp: Option<&str>,
        targets: &[(&str, &str)],
    ) -> Result<Self, String> {
        let level = level.map(Self::parse_level).transpose()?;
        let timestamp = timestamp.map(TimestampFormat::parse).transpose()?;
        let targets = targets
            .iter()
            .map(|(target, level)| {
                Self::check_target(target)?;
                Ok((target.to_string(), Self::parse_level(level)?))
            })
            .collect::<Result<_, String>>()?;
        Ok(Self {
            level,
            timestamp,
            source_location: None,
            targets,
        })
    }

    /// The terminal filter these settings ask for, over the engine's defaults.
    pub fn terminal_config(&self) -> LoggingConfig {
        self.apply_to(terminal_defaults())
    }

    /// The file filter these settings ask for, over the file lane's defaults.
    pub fn file_config(&self) -> LoggingConfig {
        self.apply_to(file_defaults())
    }

    /// Layer these settings over `defaults`.
    fn apply_to(&self, defaults: LoggingConfig) -> LoggingConfig {
        let mut config = match self.level {
            // One level for everything: the engine's per-target defaults are
            // what the project is overriding, so they are dropped; the
            // dependency filters stay, because their debug output is noise
            // whatever the engine's level.
            Some(level) => {
                let dependency_level = level.min(LevelFilter::WARN);
                LoggingConfig::new()
                    .with_baseline(level)
                    .with_directive("wgpu", dependency_level)
                    .with_directive("naga", dependency_level)
            }
            None => defaults,
        };
        for (target, level) in &self.targets {
            config = config.with_directive(target.clone(), *level);
        }
        config
    }
}

/// The terminal lane's filter before any project settings.
fn terminal_defaults() -> LoggingConfig {
    LoggingConfig::default_engine()
}

/// The file lane's filter before any project settings: the engine's, with the
/// developer scratch logs (`engine::dev`) left out.
fn file_defaults() -> LoggingConfig {
    LoggingConfig::default_engine()
        .with_directive(DEV_LOG_TARGET, LevelFilter::OFF)
        .with_directive(telemetry_target::RENDERING, LevelFilter::DEBUG)
}

// =============================================================================
// Free Functions
// =============================================================================

/// Install the engine telemetry stack and return its reload handles.
///
/// The terminal lane uses the default engine filter. When
/// `file_log_directory` is supplied, a rolling file lane is added with the
/// engine's filter minus the developer scratch logs (`engine::dev`) and with
/// the rendering target at `debug`; both lanes are live-reloadable through the
/// returned handles. When the `profiling` feature is active, `profile::*`
/// spans are routed to Tracy through an independent filter.
///
/// The stack installs once per process: a later call returns the first
/// install's handles rather than installing a second subscriber.
///
/// # Errors
///
/// Returns [`TelemetryError`] when a configured filter directive is invalid
/// or the file appender cannot be created.
pub fn init_telemetry(
    file_log_directory: Option<PathBuf>,
) -> Result<TelemetryHandles, TelemetryError> {
    init_telemetry_with_capture(file_log_directory, None)
}

/// [`init_telemetry`], plus an optional in-process capture ring.
///
/// The editor's console panel owns the ring's reader side; passing `None`
/// installs the stack without a capture lane.
///
/// # Errors
///
/// Same as [`init_telemetry`].
pub fn init_telemetry_with_capture(
    file_log_directory: Option<PathBuf>,
    capture: Option<pill_core::telemetry::capture::LogBuffer>,
) -> Result<TelemetryHandles, TelemetryError> {
    let mut builder = TelemetryBuilder::new().with_logging_config(terminal_defaults());

    // Step 1: choose the lane set - the console's capture ring when one was
    // supplied, a rolling file lane when a log directory was, and Tracy when
    // the `profiling` feature is active.
    if let Some(buffer) = capture {
        builder = builder.with_capture(buffer);
    }
    if let Some(directory) = file_log_directory {
        builder = builder.with_file_output(file_defaults(), directory);
    }
    #[cfg(feature = "profiling")]
    {
        builder = builder.with_tracy(true);
        #[cfg(feature = "profiling-fine")]
        {
            builder = builder.with_fine_profiling(true);
        }
    }

    // Step 2: install the stack and keep the handles the project's settings
    // reload through.
    let handles = builder.init()?;
    let _ = TELEMETRY_HANDLES.set(handles.clone());

    // Step 3: install the shared metrics recorder when the feature is on.
    #[cfg(feature = "metrics")]
    {
        // The recorder is process-wide, so the engine and host share one
        // store; the result is ignored because a foreign recorder that is
        // already installed wins by design.
        let _ = pill_core::metrics::install_metrics();
    }

    Ok(handles)
}

/// Apply a project's logging settings to the installed stack, both lanes.
///
/// Called once the project is known, after [`init_telemetry`]. Settings that
/// name no level and no target leave the filters as they are; without an
/// installed stack there is nothing to reload, which is not an error.
///
/// # Errors
///
/// Returns [`TelemetryError`] when a filter does not build or does not reload;
/// the previous filter stays active.
pub fn apply_logging_settings(settings: &LoggingSettings) -> Result<(), TelemetryError> {
    // Step 1: apply the process-wide switches. They live in `pill_core` rather
    // than in a filter, so they take effect even without an installed stack.
    if let Some(timestamp) = settings.timestamp {
        set_timestamp_format(timestamp);
    }
    if let Some(show) = settings.source_location {
        set_show_source_location(show);
    }

    // Step 2: reload both lanes, when there is a filter to change and a stack
    // to change it on.
    if settings.level.is_none() && settings.targets.is_empty() {
        return Ok(());
    }
    let Some(handles) = TELEMETRY_HANDLES.get() else {
        return Ok(());
    };
    handles.reload_logging_config(&settings.terminal_config())?;
    handles.reload_file_config(&settings.file_config())
}

/// Report a frontend failure to the engine's log, with its whole cause chain.
///
/// A frontend reports a failure where it happens rather than only returning
/// it: dropping a development run unmaps module images, so a report made
/// afterwards may never reach the console. `context` names the operation that
/// failed; the cause chain names the reason, since the outer message names
/// only the operation.
pub fn report_failure(context: &str, error: &(dyn std::error::Error + 'static)) {
    let cause_chain = format_error_chain(error);
    pill_core::error!(
        target: telemetry_target::ENGINE,
        error = %cause_chain,
        "{} failed",
        context
    );
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_read_by_name_in_any_case() {
        assert_eq!(LoggingSettings::parse_level("Info"), Ok(LevelFilter::INFO));
        assert_eq!(LoggingSettings::parse_level(" off "), Ok(LevelFilter::OFF));
        assert!(LoggingSettings::parse_level("verbose").is_err());
    }

    #[test]
    fn a_target_with_filter_syntax_in_it_is_refused() {
        assert!(LoggingSettings::check_target("engine::rendering").is_ok());
        assert!(LoggingSettings::check_target("engine=debug").is_err());
        assert!(LoggingSettings::check_target("a,b").is_err());
        assert!(LoggingSettings::check_target("").is_err());
    }

    /// A target override lands over the engine's default for that target.
    #[test]
    fn a_target_override_replaces_the_engine_default() {
        let settings =
            LoggingSettings::from_text(None, None, &[("engine::rendering", "debug")]).unwrap();

        let rendered = format!("{}", settings.terminal_config().build_env_filter().unwrap());

        assert!(rendered.contains("engine::rendering=debug"), "{rendered}");
        assert!(!rendered.contains("engine::rendering=info"), "{rendered}");
        assert!(rendered.contains("engine::hot_reload=info"), "{rendered}");
    }

    /// One level replaces every per-target default, but keeps the dependencies
    /// at `warn`.
    #[test]
    fn a_level_replaces_every_engine_default() {
        let settings = LoggingSettings::from_text(Some("debug"), None, &[]).unwrap();

        let rendered = format!("{}", settings.terminal_config().build_env_filter().unwrap());

        assert!(!rendered.contains("engine::"), "{rendered}");
        assert!(rendered.contains("wgpu=warn"), "{rendered}");
        assert!(rendered.contains("debug"), "{rendered}");
    }

    #[test]
    fn bundle_text_with_a_bad_level_is_refused() {
        assert!(LoggingSettings::from_text(Some("loud"), None, &[]).is_err());
        assert!(LoggingSettings::from_text(None, None, &[("wgpu", "loud")]).is_err());
        assert!(LoggingSettings::from_text(None, Some("unix"), &[]).is_err());
    }

    #[test]
    fn bundle_text_reads_the_timestamp_format() {
        let settings = LoggingSettings::from_text(None, Some("time"), &[]).unwrap();

        assert_eq!(settings.timestamp, Some(TimestampFormat::Time));
    }
}
