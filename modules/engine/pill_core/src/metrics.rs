//! Repeated numerical measurements through the `metrics` crate.
//!
//! # Responsibilities
//!
//! - Install one process-wide [`metrics::Recorder`] for the engine.
//! - Forward gauges and histograms to Tracy plots when the `tracy` feature is
//!   active, so repeated numbers appear as native Tracy graphs.
//!
//! # Design
//!
//! Call [`install_metrics`] once at the reporting boundary (usually from the
//! host telemetry bootstrap). After that, the `metrics::gauge!`,
//! `metrics::counter!`, and `metrics::histogram!` macros record into this
//! recorder from any crate.
//!
//! The recorder keeps nothing: a value goes to Tracy when a profiler is
//! attached and nowhere otherwise. `metrics` requires a recorder to exist so
//! those macros have a sink, not so the engine can hold its own copy - the
//! build and the shipping bundle report numbers by reading Tracy or by logging
//! them. Tracy plot names are cached so the leaked `PlotName` is created once
//! per distinct metric name.

// Standard library
#[cfg(feature = "tracy")]
use std::collections::HashMap;
use std::sync::Arc;
#[cfg(feature = "tracy")]
use std::sync::{Mutex, Weak};

// External crates
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Recorder, SharedString, Unit};

// =============================================================================
// Types
// =============================================================================

/// Shared state behind the recorder and its delegated handles: the plot-name
/// cache and the switch that turns forwarding off.
#[cfg(feature = "tracy")]
struct Inner {
    plots: Mutex<HashMap<String, tracy_client::PlotName>>,
    forward_to_tracy: bool,
}

#[cfg(feature = "tracy")]
impl Inner {
    /// Pushes one point into a native Tracy plot when a profiler is connected.
    fn emit_tracy_plot(&self, name: &str, value: f64) {
        // Step 1: Bail out when forwarding is disabled or no profiler is running.
        if !self.forward_to_tracy || !tracy_client::Client::is_running() {
            return;
        }
        let Some(client) = tracy_client::Client::running() else {
            return;
        };
        // Step 2: Fetch the cached plot name, creating it on first use.
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
        // Step 3: Push the sample into the Tracy plot.
        client.plot(plot_name, value);
    }
}

/// Process-wide recorder: accepts every `metrics` update and forwards the
/// plottable ones to Tracy.
///
/// It implements the full [`Recorder`] surface because the `metrics` crate
/// requires one, and it holds nothing else: no store, no lookup per update
/// beyond the plot-name cache a profiler needs anyway.
#[derive(Clone)]
pub struct EngineMetricsRecorder {
    #[cfg(feature = "tracy")]
    inner: Arc<Inner>,
}

impl EngineMetricsRecorder {
    /// Creates a recorder with an empty plot-name cache.
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "tracy")]
            inner: Arc::new(Inner {
                plots: Mutex::new(HashMap::new()),
                forward_to_tracy: true,
            }),
        }
    }

    /// Installs this recorder as the process-wide metrics recorder.
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
        // Counters never reach a Tracy plot, so this handle has nothing to do
        // with or without the feature.
        let _ = key;
        Counter::from_arc(Arc::new(NullHandle))
    }

    fn register_gauge(&self, key: &Key, _metadata: &metrics::Metadata<'_>) -> Gauge {
        #[cfg(feature = "tracy")]
        {
            Gauge::from_arc(Arc::new(DelegatingGauge {
                name: key.name().to_owned(),
                inner: Arc::downgrade(&self.inner),
            }))
        }
        #[cfg(not(feature = "tracy"))]
        {
            let _ = key;
            Gauge::from_arc(Arc::new(NullHandle))
        }
    }

    fn register_histogram(&self, key: &Key, _metadata: &metrics::Metadata<'_>) -> Histogram {
        #[cfg(feature = "tracy")]
        {
            Histogram::from_arc(Arc::new(DelegatingHistogram {
                name: key.name().to_owned(),
                inner: Arc::downgrade(&self.inner),
            }))
        }
        #[cfg(not(feature = "tracy"))]
        {
            let _ = key;
            Histogram::from_arc(Arc::new(NullHandle))
        }
    }
}

/// A handle that drops every update.
///
/// Without a profiler there is no sink and the recorder keeps no copy, so a
/// metric update is a call that does nothing - which is what a recorder with
/// no backend means.
struct NullHandle;

impl metrics::CounterFn for NullHandle {
    fn increment(&self, _value: u64) {}
    fn absolute(&self, _value: u64) {}
}

impl metrics::GaugeFn for NullHandle {
    fn increment(&self, _value: f64) {}
    fn decrement(&self, _value: f64) {}
    fn set(&self, _value: f64) {}
}

impl metrics::HistogramFn for NullHandle {
    fn record(&self, _value: f64) {}
    fn record_many(&self, _value: f64, _count: usize) {}
}

/// `metrics::Gauge` handle forwarding into the Tracy plot cache.
#[cfg(feature = "tracy")]
struct DelegatingGauge {
    name: String,
    inner: Weak<Inner>,
}

#[cfg(feature = "tracy")]
impl metrics::GaugeFn for DelegatingGauge {
    fn increment(&self, _value: f64) {}
    fn decrement(&self, _value: f64) {}
    fn set(&self, value: f64) {
        if let Some(inner) = self.inner.upgrade() {
            inner.emit_tracy_plot(&self.name, value);
        }
    }
}

/// `metrics::Histogram` handle forwarding into the Tracy plot cache.
#[cfg(feature = "tracy")]
struct DelegatingHistogram {
    name: String,
    inner: Weak<Inner>,
}

#[cfg(feature = "tracy")]
impl metrics::HistogramFn for DelegatingHistogram {
    fn record(&self, value: f64) {
        if let Some(inner) = self.inner.upgrade() {
            inner.emit_tracy_plot(&self.name, value);
        }
    }
    fn record_many(&self, value: f64, count: usize) {
        if let Some(inner) = self.inner.upgrade() {
            for _ in 0..count {
                inner.emit_tracy_plot(&self.name, value);
            }
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Converts a metric name into a stable Tracy plot identifier.
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

/// Installs the engine metrics recorder process-wide.
///
/// Returns `true` when the recorder was installed and `false` when another
/// recorder was already active (a foreign metrics system wins).
pub fn install_metrics() -> bool {
    install_metrics_with(EngineMetricsRecorder::new())
}

/// Installs a specific engine metrics recorder process-wide.
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

    /// Every handle accepts every update and keeps nothing.
    ///
    /// `metrics` requires a recorder so the macros have a sink; a value goes to
    /// a Tracy plot when a profiler is attached and nowhere otherwise, so there
    /// is no retained state to assert on. This test is the recorder's contract:
    /// nothing panics, and no update needs a lock or an allocation to be
    /// accepted.
    #[test]
    fn handles_accept_every_update_and_keep_nothing() {
        use metrics::Level;
        let recorder = EngineMetricsRecorder::new();
        let metadata = metrics::Metadata::new("test", Level::INFO, None);

        let gauge = recorder.register_gauge(&Key::from_name("test.gauge"), &metadata);
        gauge.set(7.0);
        gauge.increment(1.0);
        gauge.decrement(0.5);

        let histogram = recorder.register_histogram(&Key::from_name("test.histogram"), &metadata);
        histogram.record(1.5);
        histogram.record_many(1.5, 3);

        let counter = recorder.register_counter(&Key::from_name("test.counter"), &metadata);
        counter.increment(3);
        counter.absolute(9);
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
