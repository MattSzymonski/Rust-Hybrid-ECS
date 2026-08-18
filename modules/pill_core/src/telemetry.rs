//! Structured telemetry foundation: one `tracing` system with three lanes.
//!
//! # Responsibilities
//!
//! - Define the static tracing targets for the `engine::dev`, `engine::*`,
//!   and `profile::*` telemetry lanes.
//! - Provide strict [`LoggingConfig`] → `EnvFilter` construction with a
//!   reload handle for live filter changes.
//! - Provide the [`EngineTerminalFormatter`] that owns all terminal styling
//!   decisions (severity, target, semantic fields) using [`PillStyle`].
//! - Build the subscriber stack (terminal + optional file + optional Tracy)
//!   with independent per-layer filters through [`TelemetryBuilder`].
//!
//! # Design
//!
//! The engine emits structured meaning through `tracing`; output layers
//! decide where it goes and how it appears. The three lanes are kept apart:
//!
//! - `engine::dev` — scratch developer logs from the `log!`/`dev_warn!`/`dev_error!`
//!   macros, feature-gated behind `dev-logs`.
//! - `engine::*` — permanent structured engine logs.
//! - `profile::*` — profiling spans routed to Tracy through `TracyLayer`,
//!   controlled independently of terminal verbosity.
//!
//! [`PillStyle`]: crate::PillStyle

// Standard library
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

// External crates
use colored::Colorize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer as _;
use tracing_subscriber::{EnvFilter, Registry};

// =============================================================================
// Static Telemetry Targets
// =============================================================================

/// Target of the simple developer logging macros (`log!`, `dev_warn!`, `dev_error!`).
pub const DEV_LOG_TARGET: &str = "engine::dev";

/// Static `tracing` targets used by instrumentation callsites.
///
/// Targets are chosen once at the callsite and never parsed at runtime.
/// Filtering is expressed over these stable strings.
pub mod telemetry_target {
    /// The main engine lifecycle.
    pub const ENGINE: &str = "engine::engine";
    /// Hot-reload coordination.
    pub const HOT_RELOAD: &str = "engine::hot_reload";
    /// Input handling.
    pub const INPUT: &str = "engine::input";
    /// ECS world and scheduler activity.
    pub const ECS: &str = "engine::ecs";
    /// Renderer activity.
    pub const RENDERING: &str = "engine::rendering";
    /// Resource loading and storage.
    pub const RESOURCES: &str = "engine::resources";

    /// Coarse profiling spans (frame/system/pass architecture).
    pub const PROFILE_COARSE: &str = "profile::coarse";
    /// Fine profiling spans (temporary deep investigation).
    pub const PROFILE_FINE: &str = "profile::fine";
}

/// Targets used by the engine's profiling spans.
pub const PROFILE_COARSE_TARGET: &str = telemetry_target::PROFILE_COARSE;
/// Targets used by fine-grained investigative profiling spans.
pub const PROFILE_FINE_TARGET: &str = telemetry_target::PROFILE_FINE;

// =============================================================================
// Logging Configuration
// =============================================================================

/// Per-target logging levels expressed as strict `EnvFilter` directives.
///
/// The builder composes a baseline level with per-target overrides and
/// parses every directive strictly: an invalid directive is a configuration
/// error, never a silent fallback to `INFO`.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// Baseline level applied before any target-specific directive.
    baseline: tracing::level_filters::LevelFilter,
    /// Ordered `(target, level)` overrides.
    directives: Vec<(String, tracing::level_filters::LevelFilter)>,
}

impl LoggingConfig {
    /// Create an empty logging configuration (baseline `INFO`, no overrides).
    pub fn new() -> Self {
        Self {
            baseline: tracing::level_filters::LevelFilter::INFO,
            directives: Vec::new(),
        }
    }

    /// Set the baseline level applied to every target without an override.
    pub fn with_baseline(mut self, baseline: tracing::level_filters::LevelFilter) -> Self {
        self.baseline = baseline;
        self
    }

    /// Override the level of one static target (for example
    /// `telemetry_target::RENDERING`). Use `LevelFilter::OFF` to silence a
    /// target entirely.
    pub fn with_directive(
        mut self,
        target: impl Into<String>,
        level: tracing::level_filters::LevelFilter,
    ) -> Self {
        self.directives.push((target.into(), level));
        self
    }

    /// A sensible default for the embedded host: permanent engine logs at
    /// `INFO`, rendering at `DEBUG`, developer scratch logs visible when the
    /// `dev-logs` feature is enabled, and dependency noise reduced.
    pub fn default_engine() -> Self {
        use tracing::level_filters::LevelFilter;
        Self::new()
            .with_directive(DEV_LOG_TARGET, LevelFilter::DEBUG)
            .with_directive(telemetry_target::ENGINE, LevelFilter::INFO)
            .with_directive(telemetry_target::HOT_RELOAD, LevelFilter::INFO)
            .with_directive(telemetry_target::INPUT, LevelFilter::INFO)
            .with_directive(telemetry_target::ECS, LevelFilter::INFO)
            .with_directive(telemetry_target::RENDERING, LevelFilter::DEBUG)
            .with_directive(telemetry_target::RESOURCES, LevelFilter::INFO)
            .with_directive("wgpu", LevelFilter::WARN)
            .with_directive("naga", LevelFilter::WARN)
    }

    /// Parse a complete `RUST_LOG`-style filter string strictly.
    ///
    /// # Errors
    ///
    /// Returns a [`TelemetryError::InvalidFilter`] when any directive cannot
    /// be parsed, so a mistyped configuration fails loudly instead of
    /// silently degrading.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::LoggingConfig;
    ///
    /// let config = LoggingConfig::parse("engine=debug,wgpu=warn")
    ///     .expect("valid RUST_LOG-style string");
    /// ```
    pub fn parse(strict_filter: &str) -> Result<Self, TelemetryError> {
        EnvFilter::try_new(strict_filter).map_err(|source| TelemetryError::InvalidFilter {
            filter: strict_filter.to_owned(),
            source: Box::new(source),
        })?;
        Ok(Self::new())
    }

    /// Build a strict [`EnvFilter`] from this configuration.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::InvalidDirective`] when any configured
    /// directive fails to parse.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::LoggingConfig;
    /// use pill_core::tracing::level_filters::LevelFilter;
    ///
    /// let config = LoggingConfig::new()
    ///     .with_directive("engine::ecs", LevelFilter::DEBUG);
    /// let filter = config.build_env_filter().expect("directive must parse");
    /// ```
    pub fn build_env_filter(&self) -> Result<EnvFilter, TelemetryError> {
        let mut filter = EnvFilter::try_new(self.baseline.to_string()).map_err(|source| {
            TelemetryError::InvalidFilter {
                filter: self.baseline.to_string(),
                source: Box::new(source),
            }
        })?;
        for (target, level) in &self.directives {
            let directive = format!("{target}={level}");
            let directive =
                directive
                    .parse()
                    .map_err(|source| TelemetryError::InvalidDirective {
                        directive: directive.clone(),
                        source: Box::new(source),
                    })?;
            filter = filter.add_directive(directive);
        }
        Ok(filter)
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Terminal Formatter
// =============================================================================

/// Terminal formatting layer for `tracing` events.
///
/// Owns every terminal appearance decision: severity colors, target styling,
/// timestamps, file/line, and the message plus its structured fields.
/// Callsites never embed styling; this layer applies it.
#[derive(Debug, Clone, Copy)]
pub struct EngineTerminalFormatter {
    show_timestamps: bool,
}

impl EngineTerminalFormatter {
    /// Create a formatter; timestamps are included by default.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::EngineTerminalFormatter;
    ///
    /// let formatter = EngineTerminalFormatter::new();
    /// ```
    pub fn new() -> Self {
        Self {
            show_timestamps: true,
        }
    }

    /// Disable the timestamp prefix.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::EngineTerminalFormatter;
    ///
    /// let formatter = EngineTerminalFormatter::new().without_timestamps();
    /// ```
    pub fn without_timestamps(mut self) -> Self {
        self.show_timestamps = false;
        self
    }
}

impl Default for EngineTerminalFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, N> FormatEvent<S, N> for EngineTerminalFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        if self.show_timestamps {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            write!(writer, "[{:>9.3}] ", now.as_secs_f64())?;
        }

        write!(writer, "{} ", styled_level(metadata.level()))?;
        write!(writer, "{}", styled_target(metadata.target()))?;

        if let (Some(file), Some(line)) = (metadata.file(), metadata.line()) {
            write!(writer, " {file}:{line}")?;
        }
        write!(writer, " ")?;

        let mut visitor = StyledFieldVisitor {
            writer: &mut writer,
            first_field: true,
        };
        event.record(&mut visitor);
        writeln!(writer)?;
        Ok(())
    }
}

/// `tracing` visitor that writes the message and structured fields into a
/// terminal writer, styling field names. Write failures are ignored because
/// the terminal is a best-effort sink.
struct StyledFieldVisitor<'a, W> {
    writer: &'a mut W,
    first_field: bool,
}

impl<W: fmt::Write> StyledFieldVisitor<'_, W> {
    fn separator(&mut self) {
        if !self.first_field {
            let _ = self.writer.write_str(" ");
        }
        self.first_field = false;
    }
}

impl<W: fmt::Write> Visit for StyledFieldVisitor<'_, W> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &value)
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            // The message is the primary human-readable text.
            let _ = write!(self.writer, "{value:?}");
            return;
        }
        self.separator();
        let _ = write!(self.writer, "{}={:?}", field.name().dimmed(), value);
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_debug(field, &value)
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_debug(field, &value)
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_debug(field, &value)
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_debug(field, &value)
    }
}

// =============================================================================
// Subscriber Builder
// =============================================================================

/// Concrete terminal layer type: the custom formatter over `Stdout`.
type TerminalLayer = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        Registry,
        tracing_subscriber::fmt::format::DefaultFields,
        EngineTerminalFormatter,
        fn() -> std::io::Stdout,
    >,
    reload::Layer<EnvFilter, Registry>,
    Registry,
>;

/// Registry type after the terminal layer is attached.
type TerminalStack = tracing_subscriber::layer::Layered<TerminalLayer, Registry>;

/// Concrete file layer type: the custom formatter over a non-blocking writer.
type FileLayer = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        TerminalStack,
        tracing_subscriber::fmt::format::DefaultFields,
        EngineTerminalFormatter,
        tracing_appender::non_blocking::NonBlocking,
    >,
    reload::Layer<EnvFilter, TerminalStack>,
    TerminalStack,
>;

/// The three artifacts produced when installing the optional file lane:
/// the layer itself, the non-blocking writer guard, and the reload handle.
type FileLaneArtifacts = (
    Option<FileLayer>,
    Option<Arc<tracing_appender::non_blocking::WorkerGuard>>,
    Option<reload::Handle<EnvFilter, TerminalStack>>,
);

/// Build and install the engine telemetry subscriber stack.
///
/// The terminal and file lanes are filtered independently and both are
/// live-reloadable through [`TelemetryHandles::reload_logging`] and
/// [`TelemetryHandles::reload_file`]. The Tracy lane only ever sees
/// `profile::*` targets.
#[derive(Debug, Clone, Default)]
pub struct TelemetryBuilder {
    logging: LoggingConfig,
    file_logging: Option<(LoggingConfig, PathBuf)>,
    tracy: bool,
    fine_profiling: bool,
}

impl TelemetryBuilder {
    /// Create a builder with the default engine logging configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::TelemetryBuilder;
    ///
    /// let builder = TelemetryBuilder::new();
    /// ```
    pub fn new() -> Self {
        Self {
            logging: LoggingConfig::default_engine(),
            file_logging: None,
            tracy: false,
            fine_profiling: false,
        }
    }

    /// Replace the terminal logging configuration.
    pub fn with_logging_config(mut self, config: LoggingConfig) -> Self {
        self.logging = config;
        self
    }

    /// Add a rolling file lane with its own independent filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::{LoggingConfig, TelemetryBuilder};
    ///
    /// let builder = TelemetryBuilder::new().with_file_output(
    ///     LoggingConfig::new(),
    ///     std::env::temp_dir(),
    /// );
    /// ```
    pub fn with_file_output(
        mut self,
        config: LoggingConfig,
        directory: impl Into<PathBuf>,
    ) -> Self {
        self.file_logging = Some((config, directory.into()));
        self
    }

    /// Route `profile::*` spans to Tracy through `TracyLayer`.
    pub fn with_tracy(mut self, enabled: bool) -> Self {
        self.tracy = enabled;
        self
    }

    /// Also enable `profile::fine` spans in the Tracy lane (investigative).
    pub fn with_fine_profiling(mut self, enabled: bool) -> Self {
        self.fine_profiling = enabled;
        self
    }

    /// Build, install, and return the reload handles.
    ///
    /// Installs the subscriber stack once per process; a second call returns
    /// the previously installed handles without reinstalling. Concurrent
    /// callers are serialized so only one ever touches the global subscriber.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError`] when any configured filter directive is
    /// invalid or the file appender cannot be created.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::TelemetryBuilder;
    ///
    /// let handles = TelemetryBuilder::new()
    ///     .init()
    ///     .expect("default configuration installs cleanly");
    /// ```
    pub fn init(self) -> Result<TelemetryHandles, TelemetryError> {
        static INSTALLED: OnceLock<TelemetryHandles> = OnceLock::new();
        static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // Step 1: Fast-path return when the subscriber is already installed.
        if let Some(handles) = INSTALLED.get() {
            return Ok(handles.clone());
        }
        // Step 2: Serialize the check-and-install so two threads cannot both
        // set the process-wide default subscriber.
        let _guard = INSTALL_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Step 3: Re-check under the lock; install only when this thread is
        // the first to reach the installer.
        if let Some(handles) = INSTALLED.get() {
            return Ok(handles.clone());
        }
        let handles = Self::install(self)?;
        // A concurrent caller may win the race; either handle set is valid.
        let _ = INSTALLED.set(handles.clone());
        Ok(handles)
    }

    /// Build the full subscriber stack (terminal, optional file, optional
    /// Tracy) and install it as the process-wide default subscriber.
    ///
    /// Invoked exactly once by [`Self::init`] under its install lock.
    fn install(self) -> Result<TelemetryHandles, TelemetryError> {
        // Step 1: Build the terminal lane with its own reloadable filter.
        let terminal_filter = self.logging.build_env_filter()?;
        let (terminal_reload, terminal_handle) = reload::Layer::new(terminal_filter);
        let terminal_layer: TerminalLayer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stdout as fn() -> std::io::Stdout)
            .with_ansi(true)
            .event_format(EngineTerminalFormatter::new())
            .with_filter(terminal_reload);

        // Step 2: Build the optional file lane. `Option<L>` implements
        // `Layer`, so a missing file lane is simply a no-op layer in the
        // same position.
        let (file_layer, file_guard, file_handle): FileLaneArtifacts = match self.file_logging {
            Some((file_config, directory)) => {
                let file_filter = file_config.build_env_filter()?;
                let (file_reload, file_handle) =
                    reload::Layer::<EnvFilter, TerminalStack>::new(file_filter);
                let appender = tracing_appender::rolling::daily(&directory, "engine.log");
                let (writer, guard) = tracing_appender::non_blocking(appender);
                let layer: FileLayer = tracing_subscriber::fmt::layer::<TerminalStack>()
                    .with_writer(writer)
                    .with_ansi(false)
                    .event_format(EngineTerminalFormatter::new())
                    .with_filter(file_reload);
                (Some(layer), Some(Arc::new(guard)), Some(file_handle))
            }
            None => (None, None, None),
        };

        // Step 3: Build the optional Tracy lane, restricted to `profile::*`
        // targets and controlled independently of terminal verbosity.
        #[cfg(feature = "tracy")]
        let tracy_layer = if self.tracy {
            let mut tracy_filter = EnvFilter::default();
            tracy_filter = tracy_filter.add_directive(
                format!("{PROFILE_COARSE_TARGET}=trace")
                    .parse()
                    .map_err(|source| TelemetryError::InvalidDirective {
                        directive: format!("{PROFILE_COARSE_TARGET}=trace"),
                        source: Box::new(source),
                    })?,
            );
            if self.fine_profiling {
                tracy_filter = tracy_filter.add_directive(
                    format!("{PROFILE_FINE_TARGET}=trace")
                        .parse()
                        .map_err(|source| TelemetryError::InvalidDirective {
                            directive: format!("{PROFILE_FINE_TARGET}=trace"),
                            source: Box::new(source),
                        })?,
                );
            }
            Some(tracing_tracy::TracyLayer::default().with_filter(tracy_filter))
        } else {
            None
        };

        #[cfg(not(feature = "tracy"))]
        let tracy_layer: Option<tracing_subscriber::layer::Identity> = None;

        // Step 4: Assemble the three-layer stack and install it as the
        // process-wide default subscriber.
        let registry = tracing_subscriber::registry()
            .with(terminal_layer)
            .with(file_layer)
            .with(tracy_layer);
        registry.init();

        // Step 5: Bridge the legacy `log` crate into tracing so dependencies
        // that still emit through `log` (winit, wgpu, notify, ...) become
        // tracing events on their own targets and fall under the EnvFilter
        // directives above. The bridge is process-wide and installs once; a
        // second attempt only reports that it is already active.
        let _ = tracing_log::LogTracer::init();

        Ok(TelemetryHandles {
            logging_filter: terminal_handle,
            file_filter: file_handle,
            _file_guard: file_guard,
        })
    }
}

/// Handles returned by [`TelemetryBuilder::init`] for live configuration.
///
/// Holds the reload handles for the terminal and file lanes plus the guard
/// that keeps the non-blocking file writer alive for the process lifetime.
#[derive(Debug, Clone)]
pub struct TelemetryHandles {
    /// Reload handle for the terminal logging filter.
    pub logging_filter: reload::Handle<EnvFilter, Registry>,
    /// Reload handle for the file logging filter, when a file lane exists.
    pub file_filter: Option<reload::Handle<EnvFilter, TerminalStack>>,
    /// Keeps the non-blocking file writer thread alive for the app lifetime.
    _file_guard: Option<Arc<tracing_appender::non_blocking::WorkerGuard>>,
}

impl TelemetryHandles {
    /// Reload the terminal logging filter from a strict filter string.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::InvalidFilter`] when the replacement string
    /// cannot be parsed; the previous filter stays active.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::TelemetryBuilder;
    ///
    /// let handles = TelemetryBuilder::new()
    ///     .init()
    ///     .expect("default configuration installs cleanly");
    /// handles
    ///     .reload_logging("engine=debug,wgpu=warn")
    ///     .expect("replacement filter must parse");
    /// ```
    pub fn reload_logging(&self, filter: &str) -> Result<(), TelemetryError> {
        let filter =
            EnvFilter::try_new(filter).map_err(|source| TelemetryError::InvalidFilter {
                filter: filter.to_owned(),
                source: Box::new(source),
            })?;
        self.logging_filter
            .reload(filter)
            .map_err(|error| TelemetryError::Reload {
                error: error.to_string(),
            })
    }

    /// Reload the file logging filter from a strict filter string.
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::InvalidFilter`] when the replacement string
    /// cannot be parsed, or [`TelemetryError::Reload`] when no file lane is
    /// installed.
    pub fn reload_file(&self, filter: &str) -> Result<(), TelemetryError> {
        let Some(handle) = &self.file_filter else {
            return Err(TelemetryError::Reload {
                error: "no file logging lane installed".to_owned(),
            });
        };
        let filter =
            EnvFilter::try_new(filter).map_err(|source| TelemetryError::InvalidFilter {
                filter: filter.to_owned(),
                source: Box::new(source),
            })?;
        handle
            .reload(filter)
            .map_err(|error| TelemetryError::Reload {
                error: error.to_string(),
            })
    }
}

// =============================================================================
// TelemetryError
// =============================================================================

/// Configuration or installation failures of the telemetry stack.
///
/// Every variant carries the offending input and the underlying parse or
/// reload error so callers can render precise diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// A complete `RUST_LOG`-style filter string could not be parsed.
    #[error("invalid logging filter `{filter}`: {source}")]
    InvalidFilter {
        /// The rejected filter string.
        filter: String,
        /// The underlying parse error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// One `target=level` directive could not be parsed.
    #[error("invalid logging directive `{directive}`: {source}")]
    InvalidDirective {
        /// The rejected directive.
        directive: String,
        /// The underlying parse error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// A live filter reload failed.
    #[error("failed to reload the logging filter: {error}")]
    Reload {
        /// Human-readable reload failure.
        error: String,
    },
}

// =============================================================================
// Free Functions
// =============================================================================

/// Severity color mapping owned by the terminal formatter.
fn styled_level(level: &Level) -> String {
    match *level {
        Level::TRACE => "TRACE".magenta().to_string(),
        Level::DEBUG => "DEBUG".blue().bold().to_string(),
        Level::INFO => "INFO".white().to_string(),
        Level::WARN => "WARN".yellow().bold().to_string(),
        Level::ERROR => "ERROR".red().bold().to_string(),
    }
}

/// Target styling owned by the terminal formatter.
fn styled_target(target: &str) -> String {
    target.cyan().to_string()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The default engine configuration builds a strict, usable filter.
    #[test]
    fn default_engine_config_builds_a_strict_filter() {
        let filter = LoggingConfig::default_engine()
            .build_env_filter()
            .expect("default directives must be valid");
        let rendered = format!("{filter}");
        assert!(rendered.contains("engine::dev"));
        assert!(rendered.contains("engine::rendering"));
        assert!(rendered.contains("wgpu"));
    }

    /// Invalid directives are rejected instead of silently ignored.
    #[test]
    fn invalid_directives_are_rejected() {
        use tracing::level_filters::LevelFilter;
        // A target containing a second `=` cannot form a directive.
        let config = LoggingConfig::new().with_directive("a=b=c", LevelFilter::INFO);
        assert!(config.build_env_filter().is_err());
    }

    /// An invalid RUST_LOG-style string is a strict configuration error.
    #[test]
    fn invalid_filter_string_is_a_configuration_error() {
        assert!(LoggingConfig::parse("engine=info, ====").is_err());
        assert!(LoggingConfig::parse("engine=debug,wgpu=warn").is_ok());
    }

    /// The terminal formatter renders level, target, and message text.
    #[test]
    fn terminal_formatter_renders_level_target_and_message() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(String::new()));
        let writer = CapturingMakeWriter(Arc::clone(&captured));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(EngineTerminalFormatter::new().without_timestamps()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(
                target: "engine::resources",
                texture = "default_normal",
                "texture created"
            );
        });
        let output = captured.lock().unwrap().clone();
        assert!(output.contains("DEBUG"), "missing level: {output}");
        assert!(
            output.contains("engine::resources"),
            "missing target: {output}"
        );
        assert!(
            output.contains("texture created"),
            "missing message: {output}"
        );
        assert!(output.contains("texture="), "missing field: {output}");
    }

    /// A shared capture buffer used by [`MakeWriter`].
    struct CapturingMakeWriter(Arc<std::sync::Mutex<String>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingMakeWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            CapturingWriter(Arc::clone(&self.0))
        }
    }

    /// Writes into a shared capture buffer.
    struct CapturingWriter(Arc<std::sync::Mutex<String>>);

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut output = self.0.lock().unwrap();
            output.push_str(&String::from_utf8_lossy(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A fresh `LoggingConfig` parses and emits a filter string.
    #[test]
    fn logging_config_round_trips_through_env_filter() {
        use tracing::level_filters::LevelFilter;
        let config = LoggingConfig::new()
            .with_directive("engine::rendering", LevelFilter::DEBUG)
            .with_directive("wgpu", LevelFilter::WARN);
        let filter = config.build_env_filter().unwrap();
        let rendered = format!("{filter}");
        assert!(rendered.contains("engine::rendering"));
        assert!(rendered.contains("wgpu"));
    }

    /// The simple developer macros emit on the `engine::dev` lane.
    #[cfg(feature = "dev-logs")]
    #[test]
    fn developer_macros_emit_on_the_dev_lane() {
        use crate::{dev_error, dev_warn, log};
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(String::new()));
        let writer = CapturingMakeWriter(Arc::clone(&captured));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(EngineTerminalFormatter::new().without_timestamps()),
        );
        tracing::subscriber::with_default(subscriber, || {
            log!("frame = 42");
            dev_warn!("invalid render queue key");
            dev_error!("failed to create texture: disk full");
        });
        let output = captured.lock().unwrap().clone();
        assert!(
            output.contains("engine::dev"),
            "missing dev target: {output}"
        );
        assert!(
            output.contains("DEBUG"),
            "log! should map to DEBUG: {output}"
        );
        assert!(
            output.contains("WARN"),
            "dev_warn! should map to WARN: {output}"
        );
        assert!(
            output.contains("ERROR"),
            "dev_error! should map to ERROR: {output}"
        );
        assert!(
            output.contains("frame = 42"),
            "missing log! message: {output}"
        );
        assert!(
            output.contains("invalid render queue key"),
            "missing dev_warn! message: {output}"
        );
    }

    /// The reload handles reject invalid filters strictly and, with a file
    /// lane installed, reload the file filter too.
    ///
    /// A single test installs the process-wide subscriber once so it does not
    /// collide with other telemetry tests running in parallel.
    #[test]
    fn reload_handles_work_for_terminal_and_file_lanes() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-telemetry-reload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let handles = TelemetryBuilder::new()
            .with_logging_config(LoggingConfig::new())
            .with_file_output(LoggingConfig::new(), &directory)
            .init()
            .expect("telemetry install should succeed");
        assert!(handles.file_filter.is_some());

        assert!(handles.reload_logging("engine=debug,wgpu=warn").is_ok());
        assert!(handles.reload_logging("engine=debug, ====").is_err());
        assert!(handles.reload_logging("a=b=c").is_err());

        assert!(handles.reload_file("engine=debug").is_ok());
        assert!(handles.reload_file("engine=debug, ====").is_err());
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Legacy `log`-crate records are bridged into the tracing subscriber on
    /// their own target, so dependency diagnostics reach the same lanes.
    #[test]
    fn log_records_are_bridged_into_tracing() {
        use std::sync::{Arc, Mutex};

        // The global `log` -> `tracing` bridge installs once per process; a
        // repeated init only reports that it is already active, which still
        // leaves the bridge in place for this test.
        let _ = tracing_log::LogTracer::init();

        let captured = Arc::new(Mutex::new(String::new()));
        let writer = CapturingMakeWriter(Arc::clone(&captured));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(EngineTerminalFormatter::new().without_timestamps()),
        );
        tracing::subscriber::with_default(subscriber, || {
            log::info!(target: "wgpu", "adapter selected");
            log::warn!(target: "winit", "swapchain lost");
        });
        let output = captured.lock().unwrap().clone();
        assert!(output.contains("wgpu"), "missing log target: {output}");
        assert!(
            output.contains("adapter selected"),
            "missing log message: {output}"
        );
        assert!(
            output.contains("winit"),
            "missing second log target: {output}"
        );
        assert!(
            output.contains("swapchain lost"),
            "missing warn message: {output}"
        );
    }
}
