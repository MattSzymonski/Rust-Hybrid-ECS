//! Routing of runtime telemetry back into the host's single pipeline.
//!
//! # Responsibilities
//!
//! - Install a `tracing` subscriber inside the dynamic library that forwards
//!   every event and profiling span through the ABI log sink.
//! - Install a `metrics` recorder that forwards every sample through the ABI
//!   metrics sink.
//! - Let a later runtime generation replace the sinks without reinstalling
//!   either process-global registry.
//!
//! # Design
//!
//! `tracing` and `metrics` both resolve their global registry through a static
//! that belongs to one statically linked copy of the crate. The runtime dylib
//! links its own copies, so anything it records lands in registries the host
//! never reads: without this module every runtime log line and metric sample
//! would silently vanish.
//!
//! Installation happens once per loaded module and is deliberately separated
//! from the sinks themselves, which live in atomics the current generation
//! updates on `create`. That matters for rollback: a failed generation can be
//! destroyed and the previous module re-created without either global being
//! set twice, which `tracing` and `metrics` both reject.
//!
//! Profiling zones travel the same channel. `profile_scope!` produces plain
//! `tracing` spans, so the layer forwards their enter, record, and exit edges
//! and the host rebuilds a matching span stack for its own Tracy layer. The
//! runtime therefore never opens a Tracy connection of its own, and the host
//! keeps the single process-global one.

// Standard library
use std::ffi::{c_void, CString};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

// External crates
use pill_runtime_api::{
    LogSink, LogSinkEmitFn, MetricsSink, PILL_LOG_KIND_EVENT, PILL_LOG_KIND_SPAN_ENTER,
    PILL_LOG_KIND_SPAN_EXIT, PILL_LOG_KIND_SPAN_RECORD, PILL_LOG_LEVEL_DEBUG, PILL_LOG_LEVEL_ERROR,
    PILL_LOG_LEVEL_INFO, PILL_LOG_LEVEL_TRACE, PILL_LOG_LEVEL_WARN,
};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

// =============================================================================
// Constants
// =============================================================================

/// Fallback text used when a record's message contains an interior NUL.
const UNPRINTABLE_RECORD: &str = "<record contains an interior NUL byte>";

/// Whether the process-global `tracing` subscriber has been installed by this
/// loaded module.
static SUBSCRIBER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Whether the process-global `metrics` recorder has been installed by this
/// loaded module.
#[cfg(feature = "metrics")]
static RECORDER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Host state passed back with every log record.
static LOG_SINK_CONTEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Host callback receiving every log record, stored as an erased pointer.
static LOG_SINK_EMIT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Host state passed back with every metric sample.
#[cfg(feature = "metrics")]
static METRICS_SINK_CONTEXT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Host callback receiving every metric sample, stored as an erased pointer.
#[cfg(feature = "metrics")]
static METRICS_SINK_RECORD: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

// =============================================================================
// Free Functions
// =============================================================================

/// Point runtime telemetry at the sinks one generation was created with.
///
/// Safe to call for every generation: the registries are installed on the
/// first call and only the sink pointers change afterwards.
///
/// # Safety
///
/// The callbacks and contexts in `log_sink` and `metrics_sink` must stay valid
/// until the next call replaces them or the module is unloaded. The host
/// satisfies this by owning both sinks for its whole process lifetime.
pub(crate) unsafe fn install(log_sink: LogSink, metrics_sink: MetricsSink) {
    // Step 1: Publish the sinks before either registry can be consulted, so a
    // record emitted during installation already has somewhere to go.
    LOG_SINK_CONTEXT.store(log_sink.context, Ordering::Release);
    LOG_SINK_EMIT.store(
        log_sink
            .emit
            .map(|callback| callback as *mut c_void)
            .unwrap_or(std::ptr::null_mut()),
        Ordering::Release,
    );

    #[cfg(feature = "metrics")]
    {
        METRICS_SINK_CONTEXT.store(metrics_sink.context, Ordering::Release);
        METRICS_SINK_RECORD.store(
            metrics_sink
                .record
                .map(|callback| callback as *mut c_void)
                .unwrap_or(std::ptr::null_mut()),
            Ordering::Release,
        );
    }
    #[cfg(not(feature = "metrics"))]
    let _ = metrics_sink;

    // Step 2: Install this module's registries exactly once. A second
    // installation is rejected by both crates, which would turn a rollback
    // into a hard failure rather than a recovered one.
    if !SUBSCRIBER_INSTALLED.swap(true, Ordering::AcqRel) {
        // The registry is the span store the layer looks names and fields up
        // in; forwarding is entirely the layer's job.
        let _ = tracing_subscriber::registry().with(SinkLayer).try_init();
    }

    #[cfg(feature = "metrics")]
    if !RECORDER_INSTALLED.swap(true, Ordering::AcqRel) {
        let _ = metrics::set_global_recorder(SinkRecorder);
    }
}

/// Forward one record to the host, if a sink is currently installed.
///
/// Both strings are rebuilt as NUL-terminated buffers that live only for the
/// duration of the call, which is exactly the borrow the contract grants the
/// host.
fn emit(record_kind: u32, level: u32, target: &str, message: &str) {
    let callback = LOG_SINK_EMIT.load(Ordering::Acquire);
    if callback.is_null() {
        return;
    }

    // SAFETY: `LOG_SINK_EMIT` only ever holds a `LogSinkEmitFn` written by
    // `install`, cast through `*mut c_void`; the null case returned above.
    let callback: LogSinkEmitFn = unsafe { std::mem::transmute(callback) };
    let context = LOG_SINK_CONTEXT.load(Ordering::Acquire);

    let target = CString::new(target).unwrap_or_else(|_| CString::new("engine").unwrap());
    let message =
        CString::new(message).unwrap_or_else(|_| CString::new(UNPRINTABLE_RECORD).unwrap());
    callback(
        context,
        record_kind,
        level,
        target.as_ptr(),
        message.as_ptr(),
    );
}

/// Map a `tracing` level onto its stable contract code.
fn level_code(level: &Level) -> u32 {
    match *level {
        Level::ERROR => PILL_LOG_LEVEL_ERROR,
        Level::WARN => PILL_LOG_LEVEL_WARN,
        Level::INFO => PILL_LOG_LEVEL_INFO,
        Level::DEBUG => PILL_LOG_LEVEL_DEBUG,
        Level::TRACE => PILL_LOG_LEVEL_TRACE,
    }
}

// =============================================================================
// FieldTextVisitor
// =============================================================================

/// Flattens a record's fields into one human-readable line.
///
/// The `message` field leads, matching how the host's terminal formatter
/// presents an event, and the remaining fields follow as `name=value` pairs.
struct FieldTextVisitor {
    /// Accumulated text, starting with the message when one is present.
    text: String,
    /// Whether a separator is needed before the next field.
    has_content: bool,
}

impl FieldTextVisitor {
    /// Start an empty visitor.
    fn new() -> Self {
        Self {
            text: String::new(),
            has_content: false,
        }
    }

    /// Consume the visitor and return the flattened text.
    fn finish(self) -> String {
        self.text
    }

    /// Insert a separator between two adjacent fields.
    fn separate(&mut self) {
        if self.has_content {
            self.text.push(' ');
        }
        self.has_content = true;
    }
}

impl Visit for FieldTextVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            // The message is the primary text and always leads the record.
            let existing = std::mem::take(&mut self.text);
            self.text = if existing.is_empty() {
                value.to_string()
            } else {
                format!("{value} {existing}")
            };
            self.has_content = true;
            return;
        }
        self.separate();
        let _ = write!(self.text, "{}={}", field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let existing = std::mem::take(&mut self.text);
            self.text = if existing.is_empty() {
                format!("{value:?}")
            } else {
                format!("{value:?} {existing}")
            };
            self.has_content = true;
            return;
        }
        self.separate();
        let _ = write!(self.text, "{}={:?}", field.name(), value);
    }
}

// =============================================================================
// SinkLayer
// =============================================================================

/// Fields captured when a span was created, kept for its enter record.
struct RecordedSpanFields(String);

/// Forwards every `tracing` record produced inside the runtime to the host.
struct SinkLayer;

impl<S> Layer<S> for SinkLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    /// Forward one completed event.
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata: &Metadata<'_> = event.metadata();
        let mut visitor = FieldTextVisitor::new();
        event.record(&mut visitor);
        emit(
            PILL_LOG_KIND_EVENT,
            level_code(metadata.level()),
            metadata.target(),
            &visitor.finish(),
        );
    }

    /// Remember a span's creation-time fields for its enter record.
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let mut visitor = FieldTextVisitor::new();
        attributes.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut()
                .insert(RecordedSpanFields(visitor.finish()));
        }
    }

    /// Forward fields recorded after a span was created.
    ///
    /// Profiling zones with a dynamic name record it immediately after
    /// entering, so this is how the host learns a zone's real identity.
    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        let mut visitor = FieldTextVisitor::new();
        values.record(&mut visitor);
        let text = visitor.finish();
        if text.is_empty() {
            return;
        }

        let Some(span) = context.span(id) else {
            return;
        };
        // Keep the span's stored fields current so a later re-enter carries
        // the same identity the first enter reported.
        {
            let mut extensions = span.extensions_mut();
            match extensions.get_mut::<RecordedSpanFields>() {
                Some(existing) => {
                    if existing.0.is_empty() {
                        existing.0 = text.clone();
                    } else {
                        existing.0.push(' ');
                        existing.0.push_str(&text);
                    }
                }
                None => extensions.insert(RecordedSpanFields(text.clone())),
            }
        }
        emit(
            PILL_LOG_KIND_SPAN_RECORD,
            level_code(span.metadata().level()),
            span.metadata().target(),
            &text,
        );
    }

    /// Forward a span entry, opening a matching zone on the host.
    fn on_enter(&self, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let metadata = span.metadata();
        let fields = span
            .extensions()
            .get::<RecordedSpanFields>()
            .map(|stored| stored.0.clone())
            .unwrap_or_default();
        let message = if fields.is_empty() {
            metadata.name().to_string()
        } else {
            format!("{} {fields}", metadata.name())
        };
        emit(
            PILL_LOG_KIND_SPAN_ENTER,
            level_code(metadata.level()),
            metadata.target(),
            &message,
        );
    }

    /// Forward a span exit, closing the matching zone on the host.
    fn on_exit(&self, id: &Id, context: Context<'_, S>) {
        let Some(span) = context.span(id) else {
            return;
        };
        let metadata = span.metadata();
        emit(
            PILL_LOG_KIND_SPAN_EXIT,
            level_code(metadata.level()),
            metadata.target(),
            "",
        );
    }
}

// =============================================================================
// SinkRecorder
// =============================================================================

/// Forwards every runtime metric sample to the host's recorder.
#[cfg(feature = "metrics")]
mod metrics_recorder {
    use super::*;
    use std::sync::Arc;

    use metrics::{
        Counter, CounterFn, Gauge, GaugeFn, Histogram, HistogramFn, Key, KeyName, Metadata,
        Recorder, SharedString, Unit,
    };
    use pill_runtime_api::{
        MetricsSinkRecordFn, PILL_METRIC_KIND_COUNTER, PILL_METRIC_KIND_GAUGE,
        PILL_METRIC_KIND_HISTOGRAM,
    };

    /// Forward one metric sample to the host, if a sink is installed.
    fn record(metric_kind: u32, name: &str, value: f64) {
        let callback = METRICS_SINK_RECORD.load(Ordering::Acquire);
        if callback.is_null() {
            return;
        }

        // SAFETY: `METRICS_SINK_RECORD` only ever holds a
        // `MetricsSinkRecordFn` written by `install`, cast through
        // `*mut c_void`; the null case returned above.
        let callback: MetricsSinkRecordFn = unsafe { std::mem::transmute(callback) };
        let context = METRICS_SINK_CONTEXT.load(Ordering::Acquire);
        let Ok(name) = CString::new(name) else {
            return;
        };
        callback(context, metric_kind, name.as_ptr(), value);
    }

    /// One counter whose increments are forwarded by name.
    struct SinkCounter(String);

    impl CounterFn for SinkCounter {
        fn increment(&self, value: u64) {
            record(PILL_METRIC_KIND_COUNTER, &self.0, value as f64);
        }

        fn absolute(&self, value: u64) {
            // The host store keeps counters monotonic, so an absolute set is
            // forwarded as the value itself and reconciled on the host side.
            record(PILL_METRIC_KIND_COUNTER, &self.0, value as f64);
        }
    }

    /// One gauge whose readings are forwarded by name.
    struct SinkGauge(String);

    impl GaugeFn for SinkGauge {
        fn increment(&self, value: f64) {
            record(PILL_METRIC_KIND_GAUGE, &self.0, value);
        }

        fn decrement(&self, value: f64) {
            record(PILL_METRIC_KIND_GAUGE, &self.0, -value);
        }

        fn set(&self, value: f64) {
            record(PILL_METRIC_KIND_GAUGE, &self.0, value);
        }
    }

    /// One histogram whose observations are forwarded by name.
    struct SinkHistogram(String);

    impl HistogramFn for SinkHistogram {
        fn record(&self, value: f64) {
            record(PILL_METRIC_KIND_HISTOGRAM, &self.0, value);
        }
    }

    /// The runtime-side recorder installed in this dynamic library.
    pub(super) struct SinkRecorder;

    impl Recorder for SinkRecorder {
        fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        }
        fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
        fn describe_histogram(
            &self,
            _key: KeyName,
            _unit: Option<Unit>,
            _description: SharedString,
        ) {
        }

        fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
            Counter::from_arc(Arc::new(SinkCounter(key.name().to_owned())))
        }

        fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
            Gauge::from_arc(Arc::new(SinkGauge(key.name().to_owned())))
        }

        fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
            Histogram::from_arc(Arc::new(SinkHistogram(key.name().to_owned())))
        }
    }
}

#[cfg(feature = "metrics")]
use metrics_recorder::SinkRecorder;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `tracing` level maps onto a distinct contract code.
    #[test]
    fn level_codes_are_distinct_and_ordered() {
        assert_eq!(level_code(&Level::ERROR), PILL_LOG_LEVEL_ERROR);
        assert_eq!(level_code(&Level::WARN), PILL_LOG_LEVEL_WARN);
        assert_eq!(level_code(&Level::INFO), PILL_LOG_LEVEL_INFO);
        assert_eq!(level_code(&Level::DEBUG), PILL_LOG_LEVEL_DEBUG);
        assert_eq!(level_code(&Level::TRACE), PILL_LOG_LEVEL_TRACE);
    }

    /// Forwarding without an installed sink is a no-op rather than a crash.
    #[test]
    fn emitting_without_a_sink_is_inert() {
        LOG_SINK_EMIT.store(std::ptr::null_mut(), Ordering::Release);
        emit(PILL_LOG_KIND_EVENT, PILL_LOG_LEVEL_INFO, "engine", "hello");
    }
}
