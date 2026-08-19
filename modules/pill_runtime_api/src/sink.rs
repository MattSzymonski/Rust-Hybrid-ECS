//! Telemetry sinks that route runtime logs and metrics back to the host.
//!
//! # Responsibilities
//!
//! - Define the [`LogSink`] callback the runtime uses for every `tracing`
//!   event and profiling span it produces.
//! - Define the [`MetricsSink`] callback the runtime uses for repeated
//!   numerical measurements.
//!
//! # Design
//!
//! `tracing` and `metrics` keep their registries in process-global statics
//! that belong to one statically linked copy of each crate. The runtime dylib
//! links its own copies, so its records would target registries the host's
//! subscriber and recorder never observe, and every runtime log line would
//! silently disappear.
//!
//! Both sinks solve that by inverting the direction: the runtime installs a
//! subscriber and a recorder that forward through these C callbacks, and the
//! host re-emits each record into its own single authoritative pipeline
//! (terminal, rolling file, and Tracy). The host therefore keeps exactly one
//! Tracy connection, which the runtime never opens for itself.
//!
//! Profiling zones ride the same channel. A zone is a `tracing` span, so the
//! runtime forwards span enter, record, and exit as three record kinds and the
//! host reconstructs a matching span stack per thread. Enter and exit always
//! arrive on the thread that owns the zone, which keeps parallel system zones
//! correctly nested.

// Standard library
use std::ffi::{c_char, c_void};

// =============================================================================
// Constants
// =============================================================================

/// A completed `tracing` event; `message` holds the formatted record.
pub const PILL_LOG_KIND_EVENT: u32 = 0;

/// A span was entered; `message` holds the span name.
pub const PILL_LOG_KIND_SPAN_ENTER: u32 = 1;

/// The most recently entered span on this thread was exited.
pub const PILL_LOG_KIND_SPAN_EXIT: u32 = 2;

/// Fields were recorded on the most recently entered span on this thread;
/// `message` holds the formatted fields.
pub const PILL_LOG_KIND_SPAN_RECORD: u32 = 3;

/// `tracing` ERROR level.
pub const PILL_LOG_LEVEL_ERROR: u32 = 0;
/// `tracing` WARN level.
pub const PILL_LOG_LEVEL_WARN: u32 = 1;
/// `tracing` INFO level.
pub const PILL_LOG_LEVEL_INFO: u32 = 2;
/// `tracing` DEBUG level.
pub const PILL_LOG_LEVEL_DEBUG: u32 = 3;
/// `tracing` TRACE level.
pub const PILL_LOG_LEVEL_TRACE: u32 = 4;

/// A monotonically increasing counter; `value` is the increment.
pub const PILL_METRIC_KIND_COUNTER: u32 = 0;
/// A point-in-time gauge; `value` is the new absolute reading.
pub const PILL_METRIC_KIND_GAUGE: u32 = 1;
/// A distribution sample; `value` is one observation.
pub const PILL_METRIC_KIND_HISTOGRAM: u32 = 2;

// =============================================================================
// LogSink
// =============================================================================

/// Host callback invoked for one runtime log record or profiling span edge.
///
/// `target` and `message` are NUL-terminated UTF-8 buffers owned by the
/// runtime and valid only for the duration of the call, so the host must copy
/// anything it retains. `message` is empty rather than null for record kinds
/// that carry no text.
pub type LogSinkEmitFn = extern "C" fn(
    context: *mut c_void,
    record_kind: u32,
    level: u32,
    target: *const c_char,
    message: *const c_char,
);

/// Routing table the host installs so runtime telemetry reaches its pipeline.
///
/// A `None` callback disables forwarding entirely, which keeps a headless
/// embedding or a unit test from having to provide a sink.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LogSink {
    /// Opaque host state passed back with every call.
    pub context: *mut c_void,
    /// Callback receiving each record, or `None` to discard runtime logs.
    pub emit: Option<LogSinkEmitFn>,
}

impl LogSink {
    /// Build a sink that discards every record.
    pub const fn disabled() -> Self {
        Self {
            context: std::ptr::null_mut(),
            emit: None,
        }
    }

    /// Whether this sink forwards records to the host.
    pub fn is_enabled(&self) -> bool {
        self.emit.is_some()
    }
}

impl Default for LogSink {
    fn default() -> Self {
        Self::disabled()
    }
}

// =============================================================================
// MetricsSink
// =============================================================================

/// Host callback invoked for one runtime metric sample.
///
/// `name` is a NUL-terminated UTF-8 metric key owned by the runtime and valid
/// only for the duration of the call.
pub type MetricsSinkRecordFn =
    extern "C" fn(context: *mut c_void, metric_kind: u32, name: *const c_char, value: f64);

/// Routing table the host installs so runtime metrics reach its recorder.
///
/// A `None` callback disables forwarding, which is the correct configuration
/// for a host built without the `metrics` feature.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MetricsSink {
    /// Opaque host state passed back with every call.
    pub context: *mut c_void,
    /// Callback receiving each sample, or `None` to discard runtime metrics.
    pub record: Option<MetricsSinkRecordFn>,
}

impl MetricsSink {
    /// Build a sink that discards every sample.
    pub const fn disabled() -> Self {
        Self {
            context: std::ptr::null_mut(),
            record: None,
        }
    }

    /// Whether this sink forwards samples to the host.
    pub fn is_enabled(&self) -> bool {
        self.record.is_some()
    }
}

impl Default for MetricsSink {
    fn default() -> Self {
        Self::disabled()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A disabled sink is inert and reports itself as such.
    #[test]
    fn disabled_sinks_forward_nothing() {
        assert!(!LogSink::disabled().is_enabled());
        assert!(!MetricsSink::disabled().is_enabled());
    }

    /// Nullable function pointers keep both sinks two pointers wide.
    #[test]
    fn sinks_have_two_pointer_layout() {
        let pointer_size = std::mem::size_of::<*mut c_void>();
        assert_eq!(std::mem::size_of::<LogSink>(), pointer_size * 2);
        assert_eq!(std::mem::size_of::<MetricsSink>(), pointer_size * 2);
    }
}
