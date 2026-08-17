//! Repeated numerical measurements through the `metrics` crate.
//!
//! # Responsibilities
//!
//! - Install one process-wide [`metrics::Recorder`] for the engine.
//! - Keep the most recent counter, gauge, and histogram values in memory for
//!   diagnostics and UI overlays.
//! - Forward gauges and histograms to Tracy plots when the `tracy` feature
//!   is active, so repeated numbers appear as native Tracy graphs.
//!
//! # Design
//!
//! Call [`install_metrics`] once at the reporting boundary (usually from the
//! host telemetry bootstrap). After that, the `metrics::gauge!`,
//! `metrics::counter!`, and `metrics::histogram!` macros record into this
//! recorder from any crate. The recorder is a fan-out hub: it keeps a
//! recent-value store and optionally forwards to Tracy, matching the "one
//! metrics API, many sinks" goal. Tracy plot names are cached so the leaked
//! `PlotName` is created once per distinct metric name.

// Standard library
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

// External crates
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Recorder, SharedString, Unit};

// =============================================================================
// Types
// =============================================================================

/// One recorded metric with its latest value, for diagnostics and UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricSnapshot {
    /// Latest counter value.
    Counter(u64),
    /// Latest gauge value.
    Gauge(f64),
    /// Latest histogram sample.
    Histogram(f64),
}

/// Shared state behind the recorder and its delegated handles.
struct Inner {
    store: Mutex<HashMap<String, MetricSnapshot>>,
    #[cfg(feature = "tracy")]
    plots: Mutex<HashMap<String, tracy_client::PlotName>>,
    #[cfg(feature = "tracy")]
    forward_to_tracy: bool,
}

impl Inner {
    /// Push one point into a native Tracy plot when a profiler is connected.
    #[cfg(feature = "tracy")]
    fn emit_tracy_plot(&self, name: &str, value: f64) {
        if !self.forward_to_tracy || !tracy_client::Client::is_running() {
            return;
        }
        let Some(client) = tracy_client::Client::running() else {
            return;
        };
        // `PlotName::new_leak` leaks one static string per distinct metric;
        // cache it so the leak happens exactly once per name.
        let mut plots = self
            .plots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let plot_name = match plots.get(name) {
            Some(existing) => *existing,
            None => {
                let created = tracy_client::PlotName::new_leak(sanitize_plot_name(name));
                plots.insert(name.to_owned(), created);
                created
            }
        };
        client.plot(plot_name, value);
    }
}

/// Process-wide recorder that keeps recent values and forwards to Tracy.
///
/// The recorder is cheap: updates lock a `HashMap` and, when Tracy is
/// running, push a single plot point. It implements the full [`Recorder`]
/// surface so `metrics` handles can be returned to callers.
#[derive(Clone)]
pub struct EngineMetricsRecorder {
    inner: Arc<Inner>,
}

impl EngineMetricsRecorder {
    /// Create a recorder with a fresh recent-value store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                store: Mutex::new(HashMap::new()),
                #[cfg(feature = "tracy")]
                plots: Mutex::new(HashMap::new()),
                #[cfg(feature = "tracy")]
                forward_to_tracy: true,
            }),
        }
    }

    /// Snapshot every metric recorded so far, sorted by name.
    pub fn snapshot(&self) -> Vec<(String, MetricSnapshot)> {
        let mut values: Vec<_> = self
            .inner
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(name, value)| (name.clone(), *value))
            .collect();
        values.sort_by(|left, right| left.0.cmp(&right.0));
        values
    }

    /// Directly record one gauge value into the store (test helper).
    #[cfg(test)]
    fn record_gauge(&self, name: &str, value: f64) {
        self.inner
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_owned(), MetricSnapshot::Gauge(value));
        #[cfg(feature = "tracy")]
        self.inner.emit_tracy_plot(name, value);
    }

    /// Directly record one counter value into the store (test helper).
    #[cfg(test)]
    fn record_counter(&self, name: &str, value: u64) {
        self.inner
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_owned(), MetricSnapshot::Counter(value));
    }

    /// Directly record one histogram sample into the store (test helper).
    #[cfg(test)]
    fn record_histogram(&self, name: &str, value: f64) {
        self.inner
            .store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(name.to_owned(), MetricSnapshot::Histogram(value));
        #[cfg(feature = "tracy")]
        self.inner.emit_tracy_plot(name, value);
    }

    /// Install this recorder as the process-wide metrics recorder.
    ///
    /// # Errors
    ///
    /// Returns the recorder back when another recorder was already installed.
    pub fn try_install(self) -> Result<(), EngineMetricsRecorder> {
        metrics::set_global_recorder(self).map_err(|error| error.into_inner())
    }
}

impl Default for EngineMetricsRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder for EngineMetricsRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}
    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn register_counter(&self, key: &Key, _metadata: &metrics::Metadata<'_>) -> Counter {
        Counter::from_arc(Arc::new(DelegatingCounter {
            name: key.name().to_owned(),
            inner: Arc::downgrade(&self.inner),
        }))
    }

    fn register_gauge(&self, key: &Key, _metadata: &metrics::Metadata<'_>) -> Gauge {
        Gauge::from_arc(Arc::new(DelegatingGauge {
            name: key.name().to_owned(),
            inner: Arc::downgrade(&self.inner),
        }))
    }

    fn register_histogram(&self, key: &Key, _metadata: &metrics::Metadata<'_>) -> Histogram {
        Histogram::from_arc(Arc::new(DelegatingHistogram {
            name: key.name().to_owned(),
            inner: Arc::downgrade(&self.inner),
        }))
    }
}

/// Convert a metric name into a stable Tracy plot identifier.
///
/// Tracy plot names are identifiers; punctuation is replaced with underscores
/// so `engine.frame_time_ms` becomes `engine_frame_time_ms`.
#[cfg(feature = "tracy")]
fn sanitize_plot_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

// =============================================================================
// Delegated Handles
// =============================================================================

/// `metrics::Counter` handle delegating into the recorder store.
struct DelegatingCounter {
    name: String,
    inner: Weak<Inner>,
}

impl metrics::CounterFn for DelegatingCounter {
    fn increment(&self, value: u64) {
        if let Some(inner) = self.inner.upgrade() {
            let mut store = inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current = match store.get(&self.name) {
                Some(MetricSnapshot::Counter(previous)) => *previous,
                _ => 0,
            };
            store.insert(self.name.clone(), MetricSnapshot::Counter(current + value));
        }
    }
    fn absolute(&self, value: u64) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.name.clone(), MetricSnapshot::Counter(value));
        }
    }
}

/// `metrics::Gauge` handle delegating into the recorder store.
struct DelegatingGauge {
    name: String,
    inner: Weak<Inner>,
}

impl metrics::GaugeFn for DelegatingGauge {
    fn increment(&self, value: f64) {
        if let Some(inner) = self.inner.upgrade() {
            let mut store = inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current = match store.get(&self.name) {
                Some(MetricSnapshot::Gauge(previous)) => *previous,
                _ => 0.0,
            };
            store.insert(self.name.clone(), MetricSnapshot::Gauge(current + value));
        }
    }
    fn decrement(&self, value: f64) {
        if let Some(inner) = self.inner.upgrade() {
            let mut store = inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current = match store.get(&self.name) {
                Some(MetricSnapshot::Gauge(previous)) => *previous,
                _ => 0.0,
            };
            store.insert(self.name.clone(), MetricSnapshot::Gauge(current - value));
        }
    }
    fn set(&self, value: f64) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.name.clone(), MetricSnapshot::Gauge(value));
            #[cfg(feature = "tracy")]
            inner.emit_tracy_plot(&self.name, value);
        }
    }
}

/// `metrics::Histogram` handle delegating into the recorder store.
struct DelegatingHistogram {
    name: String,
    inner: Weak<Inner>,
}

impl metrics::HistogramFn for DelegatingHistogram {
    fn record(&self, value: f64) {
        if let Some(inner) = self.inner.upgrade() {
            inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.name.clone(), MetricSnapshot::Histogram(value));
            #[cfg(feature = "tracy")]
            inner.emit_tracy_plot(&self.name, value);
        }
    }
    fn record_many(&self, value: f64, count: usize) {
        // Without the `tracy` feature, the recent-value store keeps only the
        // latest sample, so `count` is intentionally unused.
        #[cfg(not(feature = "tracy"))]
        let _ = count;
        if let Some(inner) = self.inner.upgrade() {
            inner
                .store
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(self.name.clone(), MetricSnapshot::Histogram(value));
            #[cfg(feature = "tracy")]
            for _ in 0..count {
                inner.emit_tracy_plot(&self.name, value);
            }
        }
    }
}

// =============================================================================
// Installation
// =============================================================================

/// Install the engine metrics recorder process-wide.
///
/// Returns `true` when the recorder was installed and `false` when another
/// recorder was already active (a foreign metrics system wins).
pub fn install_metrics() -> bool {
    install_metrics_with(EngineMetricsRecorder::new())
}

/// Install a specific engine metrics recorder process-wide.
///
/// Returns `true` when the recorder was installed and `false` when another
/// recorder was already active.
pub fn install_metrics_with(recorder: EngineMetricsRecorder) -> bool {
    recorder.try_install().is_ok()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Gauges, counters, and histograms are all captured in the store.
    #[test]
    fn recorder_captures_gauge_counter_and_histogram() {
        let recorder = EngineMetricsRecorder::new();
        recorder.record_gauge("engine.fps", 144.0);
        recorder.record_counter("ecs.entities", 54_120);
        recorder.record_histogram("engine.frame_time_ms", 6.9);

        let snapshot = recorder.snapshot();
        assert!(snapshot.contains(&("engine.fps".to_owned(), MetricSnapshot::Gauge(144.0))));
        assert!(snapshot.contains(&("ecs.entities".to_owned(), MetricSnapshot::Counter(54_120))));
        assert!(snapshot.contains(&(
            "engine.frame_time_ms".to_owned(),
            MetricSnapshot::Histogram(6.9)
        )));
    }

    /// Repeated gauge updates overwrite the previous value.
    #[test]
    fn gauge_updates_overwrite_previous_value() {
        let recorder = EngineMetricsRecorder::new();
        recorder.record_gauge("engine.fps", 60.0);
        recorder.record_gauge("engine.fps", 144.0);
        assert_eq!(recorder.snapshot()[0].1, MetricSnapshot::Gauge(144.0));
    }

    /// The delegated handles record into the shared store.
    #[test]
    fn delegated_handles_reach_the_shared_store() {
        use metrics::Level;
        let recorder = EngineMetricsRecorder::new();
        let metadata = metrics::Metadata::new("test", Level::INFO, None);
        let gauge = recorder.register_gauge(&Key::from_name("test.gauge"), &metadata);
        gauge.set(7.0);
        let histogram = recorder.register_histogram(&Key::from_name("test.histogram"), &metadata);
        histogram.record(1.5);
        let counter = recorder.register_counter(&Key::from_name("test.counter"), &metadata);
        counter.increment(3);
        counter.increment(2);
        let snapshot = recorder.snapshot();
        assert!(snapshot.contains(&("test.gauge".to_owned(), MetricSnapshot::Gauge(7.0))));
        assert!(snapshot.contains(&("test.histogram".to_owned(), MetricSnapshot::Histogram(1.5))));
        assert!(snapshot.contains(&("test.counter".to_owned(), MetricSnapshot::Counter(5))));
    }

    /// Tracy plot names sanitize punctuation into identifiers.
    #[cfg(feature = "tracy")]
    #[test]
    fn plot_names_are_sanitized_into_identifiers() {
        assert_eq!(
            sanitize_plot_name("engine.frame_time_ms"),
            "engine_frame_time_ms"
        );
        assert_eq!(sanitize_plot_name("ecs.entities"), "ecs_entities");
    }
}
