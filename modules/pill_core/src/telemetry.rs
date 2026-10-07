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
//! - Hold the process-wide [`TimestampFormat`] every log line's local time is
//!   written in (`date_time` or `time`), and whether lines end with their
//!   source location ([`set_show_source_location`]).
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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

// External crates
/// Text styling (`.cyan()`, `.bold()`, ...) for [`log_block_colored`] lines,
/// re-exported so a caller needs no `colored` dependency of its own.
pub use colored::Colorize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::reload;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer as _;
use tracing_subscriber::{EnvFilter, Registry};

// Current crate
use crate::platform::log_output::{self, FileGuard, FileWriter, TerminalWriter};

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
    /// Hardware detection and startup configuration reporting.
    pub const SYSTEM: &str = "engine::system";
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
    /// `INFO` (rendering included; its per-shader and per-material detail is
    /// `DEBUG`), developer scratch logs visible when the `dev-logs` feature is
    /// enabled, and dependency noise reduced.
    pub fn default_engine() -> Self {
        use tracing::level_filters::LevelFilter;
        Self::new()
            .with_directive(DEV_LOG_TARGET, LevelFilter::DEBUG)
            .with_directive(telemetry_target::ENGINE, LevelFilter::INFO)
            .with_directive(telemetry_target::HOT_RELOAD, LevelFilter::INFO)
            .with_directive(telemetry_target::INPUT, LevelFilter::INFO)
            .with_directive(telemetry_target::ECS, LevelFilter::INFO)
            .with_directive(telemetry_target::RENDERING, LevelFilter::INFO)
            .with_directive(telemetry_target::RESOURCES, LevelFilter::OFF)
            .with_directive("wgpu", LevelFilter::WARN)
            .with_directive("naga", LevelFilter::WARN)
    }

    /// Validate a complete `RUST_LOG`-style filter string strictly.
    ///
    /// The filter a project configures is applied by building a
    /// [`LoggingConfig`] from its settings and calling
    /// [`LoggingConfig::build_env_filter`]; this checks the same string early,
    /// so a mistyped configuration fails loudly instead of silently degrading.
    ///
    /// # Errors
    ///
    /// Returns a [`TelemetryError::InvalidFilter`] when any directive cannot
    /// be parsed.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::LoggingConfig;
    ///
    /// LoggingConfig::validate_filter("engine=debug,wgpu=warn")
    ///     .expect("valid RUST_LOG-style string");
    /// ```
    pub fn validate_filter(strict_filter: &str) -> Result<(), TelemetryError> {
        EnvFilter::try_new(strict_filter).map_err(|source| TelemetryError::InvalidFilter {
            filter: strict_filter.to_owned(),
            source: Box::new(source),
        })?;
        Ok(())
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
// Timestamp Format
// =============================================================================

/// How the local time at the start of every log line is written.
///
/// Chosen by the project's `logging: timestamp:` setting and applied with
/// [`set_timestamp_format`]. It is one process-wide value, held in
/// `pill_core` so every DLL's log lines agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimestampFormat {
    /// `[dd.mm.yyyy hh:mm:ss:mmm]`, written `date_time` in settings.
    DateTime,
    /// `[hh:mm:ss:mmm]`, written `time` in settings. The default.
    #[default]
    Time,
}

impl TimestampFormat {
    /// Read the settings name: `date_time` or `time`, in any case.
    ///
    /// # Errors
    ///
    /// Returns a message naming the accepted values for anything else.
    ///
    /// # Examples
    ///
    /// ```
    /// use pill_core::telemetry::TimestampFormat;
    ///
    /// assert_eq!(TimestampFormat::parse("time"), Ok(TimestampFormat::Time));
    /// ```
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "date_time" => Ok(Self::DateTime),
            "time" => Ok(Self::Time),
            _ => Err(format!(
                "`{text}` is not a timestamp format; use date_time or time"
            )),
        }
    }

    /// The settings name of this format, as [`Self::parse`] reads it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DateTime => "date_time",
            Self::Time => "time",
        }
    }

    /// The `chrono` pattern that writes this format (`%3f` is milliseconds).
    fn pattern(self) -> &'static str {
        match self {
            Self::DateTime => "%d.%m.%Y %H:%M:%S:%3f",
            Self::Time => "%H:%M:%S:%3f",
        }
    }
}

/// The active [`TimestampFormat`], as its position in the enum.
static TIMESTAMP_FORMAT: AtomicU8 = AtomicU8::new(TimestampFormat::Time as u8);

/// Set how every later log line writes its time.
pub fn set_timestamp_format(format: TimestampFormat) {
    TIMESTAMP_FORMAT.store(format as u8, Ordering::Relaxed);
}

/// How log lines currently write their time.
pub fn timestamp_format() -> TimestampFormat {
    if TIMESTAMP_FORMAT.load(Ordering::Relaxed) == TimestampFormat::DateTime as u8 {
        TimestampFormat::DateTime
    } else {
        TimestampFormat::Time
    }
}

/// Whether log lines end with the `file:line` that emitted them. Off by
/// default; the project's `logging: source_location:` setting turns it on.
static SHOW_SOURCE_LOCATION: AtomicBool = AtomicBool::new(false);

/// Set whether every later log line ends with its source location.
pub fn set_show_source_location(show: bool) {
    SHOW_SOURCE_LOCATION.store(show, Ordering::Relaxed);
}

/// Whether log lines currently end with their source location.
pub fn show_source_location() -> bool {
    SHOW_SOURCE_LOCATION.load(Ordering::Relaxed)
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
        // A record bridged from the `log` crate arrives with the target `log`
        // and its real target, file and line as `log.*` fields; normalizing
        // shows it like any other event.
        let normalized = event.normalized_metadata();
        let metadata = normalized.as_ref().unwrap_or_else(|| event.metadata());
        // The file lane is built without ANSI, so styling follows the writer
        // rather than whether a terminal happens to be attached.
        let ansi = writer.has_ansi_escapes();
        if self.show_timestamps {
            let pattern = timestamp_format().pattern();
            let now = format!("[{}]", chrono::Local::now().format(pattern));
            write!(writer, "{} ", paint(ansi, &now, |text| text.dimmed()))?;
        }

        write!(writer, "{} ", styled_level(metadata.level(), ansi))?;
        write!(writer, "{}", styled_target(metadata.target(), ansi))?;

        // Message first, then its fields after a two-space gap, so the text
        // never runs into the first field name.
        let mut visitor = StyledFieldVisitor {
            ansi,
            skip_log_fields: event.is_log(),
            message: String::new(),
            fields: String::new(),
        };
        event.record(&mut visitor);
        // A message may carry color codes (see `log_block_colored`). They reach
        // only a lane that shows colors - an interactive terminal - and are
        // stripped for the file lane and for piped output, which the suites read.
        if !(ansi && colored::control::SHOULD_COLORIZE.should_colorize()) {
            visitor.message = strip_color_codes(&visitor.message);
        }
        // A multi-line message is a block (see `log_block`): the prefix line
        // carries only the time, level, target and fields, and the message
        // follows below it from the left edge, so wide tables and trees keep
        // their full width.
        let block = visitor.message.contains('\n');
        if !block {
            write!(writer, "  {}", visitor.message)?;
        }
        if !visitor.fields.is_empty() {
            write!(writer, "  {}", visitor.fields)?;
        }

        // The source location is opt-in and trails the line in dark gray:
        // useful for navigation, but not what a reader scans for.
        if show_source_location() {
            if let (Some(file), Some(line)) = (metadata.file(), metadata.line()) {
                let location = format!("{file}:{line}");
                let location = paint(ansi, &location, |text| text.bright_black());
                write!(writer, "  {location}")?;
            }
        }
        if block {
            write!(writer, "\n{}", visitor.message)?;
        }
        writeln!(writer)?;
        Ok(())
    }
}

/// `tracing` visitor that collects the message and the structured fields
/// separately, so the formatter can lay them out in a fixed order whatever
/// order the callsite declared them in.
struct StyledFieldVisitor {
    ansi: bool,
    /// Drop the `log.*` fields of a bridged `log` record; the normalized
    /// metadata already carries what they hold.
    skip_log_fields: bool,
    message: String,
    fields: String,
}

impl Visit for StyledFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_debug(field, &value)
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        use fmt::Write as _;
        if field.name() == "message" {
            // The message is the primary human-readable text.
            let _ = write!(self.message, "{value:?}");
            return;
        }
        if self.skip_log_fields && field.name().starts_with("log.") {
            return;
        }
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        // Values keep their `Debug` form (strings quoted, options as
        // `Some(..)`): the end-to-end suites match on that exact text.
        let name = paint(self.ansi, field.name(), |text| text.dimmed());
        let value = format!("{value:?}");
        let value = if MODULE_FIELD_NAMES.contains(&field.name()) {
            paint(self.ansi, &value, |text| text.cyan().bold())
        } else {
            value
        };
        let _ = write!(self.fields, "{name}={value}");
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
        TerminalWriter,
    >,
    reload::Layer<EnvFilter, Registry>,
    Registry,
>;

/// Registry type after the terminal layer is attached.
type TerminalStack = tracing_subscriber::layer::Layered<TerminalLayer, Registry>;

/// Concrete file layer type: the custom formatter over the platform's file writer.
type FileLayer = tracing_subscriber::filter::Filtered<
    tracing_subscriber::fmt::Layer<
        TerminalStack,
        tracing_subscriber::fmt::format::DefaultFields,
        EngineTerminalFormatter,
        FileWriter,
    >,
    reload::Layer<EnvFilter, TerminalStack>,
    TerminalStack,
>;

/// The three artifacts produced when installing the optional file lane:
/// the layer itself, the file writer's guard, and the reload handle.
type FileLaneArtifacts = (
    Option<FileLayer>,
    Option<Arc<FileGuard>>,
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
        // Exactly-once install. `OnceLock::get_or_init` runs the installer at
        // most once process-wide and blocks concurrent callers until it
        // completes, so the double guard of a second `Mutex` adds nothing: the
        // lock existed to serialize check-and-install, which the OnceLock does
        // itself. The stored value is the install's `Result`, flattened to a
        // message so it can be shared with every caller - the error itself is
        // not `Clone` (it boxes the underlying parse/appender source).
        static INSTALLED: OnceLock<Result<TelemetryHandles, String>> = OnceLock::new();
        match INSTALLED.get_or_init(|| Self::install(self).map_err(|error| error.to_string())) {
            Ok(handles) => Ok(handles.clone()),
            Err(message) => Err(TelemetryError::InstallFailed {
                message: message.clone(),
            }),
        }
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
            .with_writer(log_output::terminal_writer())
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
                let (writer, guard) = log_output::open_file_output(&directory, "engine.log")
                    .map_err(|message| TelemetryError::FileOutput { message })?;
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
        // that still emit through `log` (winit, wgpu, notify, ...) reach the
        // same lanes. Bridged records carry the target `log`, so the
        // EnvFilter directives above cannot select them by crate. The bridge
        // filters them itself instead: only warnings and errors pass (a
        // dependency's info and debug chatter, such as symphonia's format
        // probing, is not engine news), and the targets in IGNORED_LOG_TARGETS
        // are dropped entirely. The bridge is process-wide and installs once;
        // a second attempt only reports that it is already active.
        let _ = tracing_log::LogTracer::builder()
            .with_max_level(tracing_log::log::LevelFilter::Warn)
            .ignore_all(IGNORED_LOG_TARGETS.iter().copied())
            .init();

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
    /// Keeps the file writer alive for the app lifetime.
    _file_guard: Option<Arc<FileGuard>>,
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

    /// Reload the terminal logging filter from a [`LoggingConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`TelemetryError::InvalidDirective`] when a directive of the
    /// configuration cannot be parsed, or [`TelemetryError::Reload`] when the
    /// reload itself fails; the previous filter stays active either way.
    pub fn reload_logging_config(&self, config: &LoggingConfig) -> Result<(), TelemetryError> {
        let filter = config.build_env_filter()?;
        self.logging_filter
            .reload(filter)
            .map_err(|error| TelemetryError::Reload {
                error: error.to_string(),
            })
    }

    /// Reload the file logging filter from a [`LoggingConfig`]. Does nothing
    /// when no file lane is installed.
    ///
    /// # Errors
    ///
    /// As [`Self::reload_logging_config`].
    pub fn reload_file_config(&self, config: &LoggingConfig) -> Result<(), TelemetryError> {
        let Some(handle) = &self.file_filter else {
            return Ok(());
        };
        let filter = config.build_env_filter()?;
        handle
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

    /// The file lane could not be opened.
    #[error("failed to open the log file: {message}")]
    FileOutput {
        /// Rendered description of the failure.
        message: String,
    },

    /// The one-time subscriber install failed.
    ///
    /// The install is attempted at most once per process; the failure is
    /// rendered into a message and cached so every concurrent or later caller
    /// sees the same error instead of silently re-attempting the install.
    #[error("failed to install the telemetry subscriber: {message}")]
    InstallFailed {
        /// Rendered description of the underlying install failure.
        message: String,
    },
}

// =============================================================================
// Free Functions
// =============================================================================

/// Join a heading and its lines into one multi-line log message.
///
/// The terminal formatter prints any message with a line break as a block:
/// the time, level and target alone on the first line, then the message from
/// the left edge. The block ends with a line break, so an empty line separates
/// it from the next log entry.
///
/// ```text
/// [21:51:45:003] INFO  engine::hot_reload
/// Modules to build:
/// 1. pill_spline  extension
/// 2. project      project
///
/// ```
///
/// Pass the result to any logging macro: `info!(target: ..., "{}", block)`.
///
/// # Examples
///
/// ```
/// use pill_core::telemetry::log_block;
///
/// let block = log_block("Modules to build:", ["1. pill_spline", "2. project"]);
/// assert_eq!(block, "Modules to build:\n1. pill_spline\n2. project\n");
/// ```
pub fn log_block<I, S>(heading: &str, lines: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut block = heading.to_string();
    for line in lines {
        block.push('\n');
        block.push_str(line.as_ref());
    }
    block.push('\n');
    block
}

/// Join a heading and its lines into one multi-line log message, where any
/// part may carry color.
///
/// The same as [`log_block`], but the heading and the lines are anything that
/// displays - plain strings, or text styled with [`Colorize`] (re-exported
/// here) or [`PillStyle`](crate::PillStyle). Colors show in an interactive
/// terminal only: the formatter strips them for the file log and for piped
/// output.
///
/// # Examples
///
/// ```
/// use pill_core::telemetry::{log_block_colored, Colorize};
///
/// let block = log_block_colored(
///     "Modules to build:".bold(),
///     [
///         format!("1. {}  extension", "pill_spline".cyan()),
///         format!("2. {}  project", "project".cyan()),
///     ],
/// );
/// assert!(block.starts_with(&"Modules to build:".bold().to_string()));
/// ```
pub fn log_block_colored<H, I, L>(heading: H, lines: I) -> String
where
    H: fmt::Display,
    I: IntoIterator<Item = L>,
    L: fmt::Display,
{
    log_block(
        &heading.to_string(),
        lines.into_iter().map(|line| line.to_string()),
    )
}

/// `text` without its ANSI escape sequences (`ESC [ ... letter`): what a lane
/// that does not show colors receives from colored text.
///
/// # Examples
///
/// ```
/// use pill_core::telemetry::strip_color_codes;
///
/// assert_eq!(strip_color_codes("\u{1b}[36mpill_spline\u{1b}[0m"), "pill_spline");
/// ```
pub fn strip_color_codes(text: &str) -> String {
    if !text.contains('\u{1b}') {
        return text.to_string();
    }
    let mut plain = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' && characters.peek() == Some(&'[') {
            // Skip the parameters up to and including the final letter.
            characters.next();
            for code in characters.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
}

/// The column width of the level, padded to the widest name (`ERROR`, `DEBUG`).
const LEVEL_WIDTH: usize = 5;

/// Severity color mapping owned by the terminal formatter.
fn styled_level(level: &Level, ansi: bool) -> String {
    // Padded to the widest level so the targets line up in a column.
    let text = format!("{:<LEVEL_WIDTH$}", level.as_str());
    match *level {
        Level::TRACE => paint(ansi, &text, |text| text.magenta()),
        Level::DEBUG => paint(ansi, &text, |text| text.blue().bold()),
        Level::INFO => paint(ansi, &text, |text| text.green()),
        Level::WARN => paint(ansi, &text, |text| text.yellow().bold()),
        Level::ERROR => paint(ansi, &text, |text| text.red().bold()),
    }
}

/// Target styling owned by the terminal formatter.
fn styled_target(target: &str, ansi: bool) -> String {
    paint(ansi, target, |text| text.cyan())
}

/// Field names whose value names a module; their value is highlighted so the
/// module a lifecycle line is about stands out.
const MODULE_FIELD_NAMES: &[&str] = &["module", "extension"];

/// `log` targets the bridge drops, each with the reason it is noise.
///
/// - `wgpu_hal::vulkan::conv`: wgpu 25 warns `Unrecognized present mode
///   1000361000` on every surface query when the driver offers
///   `VK_PRESENT_MODE_FIFO_LATEST_READY_EXT`, which it does not know yet and
///   skips. The module only logs such unknown-value notices.
const IGNORED_LOG_TARGETS: &[&str] = &["wgpu_hal::vulkan::conv"];

/// Applies `style` to `text` when the writer accepts ANSI escapes, and returns
/// the text unchanged otherwise.
fn paint(ansi: bool, text: &str, style: impl FnOnce(&str) -> colored::ColoredString) -> String {
    if ansi {
        style(text).to_string()
    } else {
        text.to_string()
    }
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
        assert!(LoggingConfig::validate_filter("engine=info, ====").is_err());
        assert!(LoggingConfig::validate_filter("engine=debug,wgpu=warn").is_ok());
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

    /// Both timestamp formats read back from their settings names and write
    /// the documented shape: `dd.mm.yyyy hh:mm:ss:mmm` and `hh:mm:ss:mmm`.
    #[test]
    fn timestamp_formats_parse_and_render() {
        for format in [TimestampFormat::DateTime, TimestampFormat::Time] {
            assert_eq!(TimestampFormat::parse(format.as_str()), Ok(format));
        }
        assert_eq!(TimestampFormat::parse(" TIME "), Ok(TimestampFormat::Time));
        assert!(TimestampFormat::parse("unix").is_err());

        let moment = chrono::NaiveDate::from_ymd_opt(2026, 10, 3)
            .and_then(|date| date.and_hms_milli_opt(9, 5, 7, 42))
            .expect("valid date");
        let render = |format: TimestampFormat| moment.format(format.pattern()).to_string();
        assert_eq!(render(TimestampFormat::DateTime), "03.10.2026 09:05:07:042");
        assert_eq!(render(TimestampFormat::Time), "09:05:07:042");
    }

    /// Color codes in a message reach no lane that cannot show them: a plain
    /// lane gets the text alone.
    #[test]
    fn color_codes_are_stripped_for_a_plain_lane() {
        use std::sync::{Arc, Mutex};

        assert_eq!(
            strip_color_codes("\u{1b}[1;36mpill_spline\u{1b}[0m  extension"),
            "pill_spline  extension"
        );
        assert_eq!(strip_color_codes("no codes"), "no codes");

        let captured = Arc::new(Mutex::new(String::new()));
        let writer = CapturingMakeWriter(Arc::clone(&captured));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(EngineTerminalFormatter::new().without_timestamps()),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "engine::hot_reload",
                "{}",
                log_block_colored(
                    "\u{1b}[1mModules to build:\u{1b}[0m",
                    ["1. \u{1b}[36mpill_spline\u{1b}[0m  extension"]
                )
            );
        });
        let output = captured.lock().unwrap().clone();
        assert!(!output.contains('\u{1b}'), "{output}");
        assert!(
            output.contains("Modules to build:\n1. pill_spline  extension"),
            "{output}"
        );
    }

    /// A multi-line message logs as a block: the prefix and fields alone on the
    /// first line, then the message from the left edge.
    #[test]
    fn terminal_formatter_lays_out_a_multi_line_block() {
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
            tracing::info!(
                target: "engine::hot_reload",
                total = 2,
                "{}",
                log_block("Modules to build:", ["1. pill_spline", "2. project"])
            );
        });
        let output = captured.lock().unwrap().clone();
        let lines: Vec<&str> = output.lines().collect();
        // Only the start of the prefix line: another test may switch the
        // process-wide source location on while this one runs.
        assert!(
            lines[0].starts_with("INFO  engine::hot_reload  total=2"),
            "{output}"
        );
        // The trailing empty line is the block's own closing line break.
        assert_eq!(
            lines[1..],
            ["Modules to build:", "1. pill_spline", "2. project", ""]
        );
    }

    /// The message is separated from its fields, the source location appears
    /// only when enabled and then trails the line, and a lane without ANSI
    /// gets no escape codes even for highlighted fields.
    ///
    /// The only test that changes the source location setting, so the toggle
    /// cannot race another test's expectation.
    #[test]
    fn terminal_formatter_separates_message_fields_and_location() {
        use std::sync::{Arc, Mutex};

        let captured = Arc::new(Mutex::new(String::new()));
        let emit = || {
            let writer = CapturingMakeWriter(Arc::clone(&captured));
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .event_format(EngineTerminalFormatter::new().without_timestamps()),
            );
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(
                    target: "engine::hot_reload",
                    module = "pill_spline",
                    owner = 2,
                    "extension loaded"
                );
            });
            let output = std::mem::take(&mut *captured.lock().unwrap());
            output.lines().next().expect("one line").to_owned()
        };

        let hidden = emit();
        assert_eq!(
            hidden, "INFO  engine::hot_reload  extension loaded  module=\"pill_spline\" owner=2",
            "the location is off by default"
        );

        set_show_source_location(true);
        let shown = emit();
        set_show_source_location(false);
        assert!(
            shown.starts_with(&format!("{hidden}  ")) && shown.contains("telemetry.rs:"),
            "missing trailing location: {shown}"
        );
        assert!(
            !shown.contains('\u{1b}'),
            "escape codes in a plain lane: {shown}"
        );
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

    /// A later directive for a target replaces an earlier one, which is what
    /// lets project settings layer over the engine's defaults.
    #[test]
    fn a_later_directive_for_a_target_wins() {
        use tracing::level_filters::LevelFilter;
        let filter = LoggingConfig::default_engine()
            .with_directive("engine::rendering", LevelFilter::DEBUG)
            .build_env_filter()
            .unwrap();
        let rendered = format!("{filter}");
        assert!(rendered.contains("engine::rendering=debug"), "{rendered}");
        assert!(!rendered.contains("engine::rendering=info"), "{rendered}");
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

    /// Concurrent `init` calls install the subscriber exactly once, and every
    /// caller receives the same installed handle set.
    ///
    /// This is the property the old `OnceLock` + `Mutex` double guard existed
    /// to provide; the single `OnceLock` must preserve it. Uses a file lane
    /// like `reload_handles_work_for_terminal_and_file_lanes` so the two tests
    /// agree on what "installed" means whichever one wins the process-wide race.
    #[test]
    fn concurrent_init_installs_exactly_once() {
        let directory = std::env::temp_dir().join(format!(
            "ecs-telemetry-concurrent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let handles: Vec<TelemetryHandles> = std::thread::scope(|scope| {
            let mut joiners = Vec::new();
            for _ in 0..8 {
                joiners.push(scope.spawn(|| {
                    TelemetryBuilder::new()
                        .with_logging_config(LoggingConfig::new())
                        .with_file_output(LoggingConfig::new(), &directory)
                        .init()
                        .expect("concurrent install must succeed")
                }));
            }
            joiners
                .into_iter()
                .map(|joiner| joiner.join().expect("install thread must not panic"))
                .collect()
        });

        // Every caller holds the same install: the worker guards are literally
        // the same `Arc`, which is only possible if the OnceLock ran the
        // installer exactly once and every other caller read the result.
        let first_guard = handles[0]
            ._file_guard
            .as_ref()
            .expect("the shared install must carry a file lane");
        for handle in &handles[1..] {
            let guard = handle
                ._file_guard
                .as_ref()
                .expect("every caller must see the same install");
            assert!(
                std::sync::Arc::ptr_eq(first_guard, guard),
                "concurrent callers must share one install, not each get their own"
            );
        }
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
            // Warnings and errors only: the engine's bridge drops lower
            // levels, and whichever test installs it first sets that cap.
            log::warn!(target: "wgpu", "adapter selected");
            log::error!(target: "winit", "swapchain lost");
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
        // Shown under their own targets, without the bridge's `log.*` fields.
        assert!(
            output.contains("WARN  wgpu  adapter selected"),
            "bridged record not normalized: {output}"
        );
        assert!(
            !output.contains("log."),
            "bridge fields leaked into the line: {output}"
        );
    }
}
