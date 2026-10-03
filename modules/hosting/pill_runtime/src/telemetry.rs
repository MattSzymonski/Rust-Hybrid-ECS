//! Application telemetry bootstrap for every frontend.
//!
//! # Responsibilities
//!
//! - Install the shared [`pill_core::telemetry`] subscriber stack (terminal,
//!   optional file, optional Tracy) with the default engine filter.
//! - Apply a project's logging settings - the `logging:` section of its
//!   `project_settings.yaml` - over those defaults once the project is known
//!   ([`apply_logging_settings`]).
//! - Install the shared metrics recorder when the `metrics` feature is on.
//!
//! # Design
//!
//! The runtime owns the executable-facing telemetry entry point so every
//! frontend shares one consistent setup. Logging verbosity (terminal +
//! file) is independent of Tracy profiling: Tracy spans (`profile::*`) are
//! only routed when the `profiling` feature is active, and their filter is
//! never affected by terminal log levels.
//!
//! Telemetry starts before the project's settings are read, so nothing logged
//! while finding the project is lost; the settings arrive afterwards and reload
//! the filters in place. They come as [`LoggingSettings`], already validated:
//! this crate reads no YAML, so the development host parses the settings file
//! and a shipping build has them baked into its bundle.

// Standard library
use std::path::PathBuf;
use std::sync::OnceLock;

// External crates
use tracing::level_filters::LevelFilter;

// Current crate
use pill_core::telemetry::{
    telemetry_target, LoggingConfig, TelemetryBuilder, TelemetryError, TelemetryHandles,
    DEV_LOG_TARGET,
};

// =============================================================================
// LoggingSettings
// =============================================================================

/// A project's logging settings: the `logging:` section of its
/// `project_settings.yaml`, validated.
///
/// ```yaml
/// logging:
///   level: info                 # every target, replacing the engine's defaults
///   targets:                    # per target, applied last
///     engine::rendering: info
///     wgpu: error
/// ```
///
/// Both keys are optional, and an absent section leaves the engine's defaults
/// as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoggingSettings {
    /// One level for every target, replacing the engine's per-target defaults
    /// (the `wgpu` and `naga` dependency filters stay at `warn` unless the
    /// level is quieter, or a target override names them).
    pub level: Option<LevelFilter>,
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

    /// Check one target name: letters, digits, `_`, `-`, `.` and `::`
    /// separators, which is what a `tracing` target is made of.
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
    /// Returns the first level or target that does not read; the bundle
    /// generator validates both, so this only fails on a hand-edited bundle.
    pub fn from_text(level: Option<&str>, targets: &[(&str, &str)]) -> Result<Self, String> {
        let level = level.map(Self::parse_level).transpose()?;
        let targets = targets
            .iter()
            .map(|(target, level)| {
                Self::check_target(target)?;
                Ok((target.to_string(), Self::parse_level(level)?))
            })
            .collect::<Result<_, String>>()?;
        Ok(Self { level, targets })
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

/// The handles of the installed stack, kept so the project's settings can
/// reload its filters after [`init_telemetry`] returned.
static TELEMETRY_HANDLES: OnceLock<TelemetryHandles> = OnceLock::new();

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
    let mut builder = TelemetryBuilder::new().with_logging_config(terminal_defaults());

    // Step 1: Add a rolling file lane when a log directory is supplied.
    if let Some(directory) = file_log_directory {
        builder = builder.with_file_output(file_defaults(), directory);
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
    let _ = TELEMETRY_HANDLES.set(handles.clone());

    // Step 4: Install the shared metrics recorder when the feature is on.
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
/// Called once the project is known, after [`init_telemetry`]. Settings with
/// nothing in them leave the defaults as they are; without an installed stack
/// there is nothing to reload, which is not an error.
///
/// # Errors
///
/// Returns [`TelemetryError`] when a filter does not build or does not reload;
/// the previous filter stays active.
pub fn apply_logging_settings(settings: &LoggingSettings) -> Result<(), TelemetryError> {
    if *settings == LoggingSettings::default() {
        return Ok(());
    }
    let Some(handles) = TELEMETRY_HANDLES.get() else {
        return Ok(());
    };
    handles.reload_logging_config(&settings.terminal_config())?;
    handles.reload_file_config(&settings.file_config())
}

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
        let settings = LoggingSettings::from_text(None, &[("engine::rendering", "info")]).unwrap();

        let rendered = format!("{}", settings.terminal_config().build_env_filter().unwrap());

        assert!(rendered.contains("engine::rendering=info"), "{rendered}");
        assert!(!rendered.contains("engine::rendering=debug"), "{rendered}");
        assert!(rendered.contains("engine::hot_reload=info"), "{rendered}");
    }

    /// One level replaces every per-target default, but keeps the dependencies
    /// at `warn`.
    #[test]
    fn a_level_replaces_every_engine_default() {
        let settings = LoggingSettings::from_text(Some("debug"), &[]).unwrap();

        let rendered = format!("{}", settings.terminal_config().build_env_filter().unwrap());

        assert!(!rendered.contains("engine::"), "{rendered}");
        assert!(rendered.contains("wgpu=warn"), "{rendered}");
        assert!(rendered.contains("debug"), "{rendered}");
    }

    #[test]
    fn bundle_text_with_a_bad_level_is_refused() {
        assert!(LoggingSettings::from_text(Some("loud"), &[]).is_err());
        assert!(LoggingSettings::from_text(None, &[("wgpu", "loud")]).is_err());
    }
}
