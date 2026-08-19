//! Host-side receivers for telemetry produced inside the engine runtime.
//!
//! # Responsibilities
//!
//! - Re-emit every runtime log record into the host's `tracing` subscriber.
//! - Reconstruct runtime profiling zones as host spans so Tracy sees them.
//! - Re-emit every runtime metric sample into the host's `metrics` recorder.
//!
//! # Design
//!
//! The runtime dylib links its own copies of `tracing` and `metrics`, whose
//! registries are process-global *per linked copy*. Anything it records lands
//! in registries the host never reads, so both crates would silently drop every
//! runtime record. The contract's sinks invert that: the runtime forwards, and
//! this module re-emits into the single pipeline the host set up - terminal,
//! rolling file, and Tracy.
//!
//! Re-emitting is not a plain forward. `tracing` metadata is static, so a
//! runtime record's target and level are matched against the small, fixed set
//! of engine lanes rather than reproduced verbatim; an unrecognised lane falls
//! back to the general engine target with its original text intact.
//!
//! Profiling zones arrive as span enter and exit edges. Both always occur on
//! the thread that owns the zone, including the scheduler's worker threads, so
//! the reconstructed span guards live in a thread-local stack and nesting is
//! preserved exactly. A zone's real name arrives as a field rather than as
//! metadata, because a host span's name must be a compile-time literal.

// Standard library
use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr};

// External crates
use pill_core::telemetry::{telemetry_target, DEV_LOG_TARGET};
use pill_core::{debug, error, info, trace, warn};
use pill_runtime_api::{
    LogSink, MetricsSink, PILL_LOG_KIND_EVENT, PILL_LOG_KIND_SPAN_ENTER, PILL_LOG_KIND_SPAN_EXIT,
    PILL_LOG_KIND_SPAN_RECORD, PILL_LOG_LEVEL_DEBUG, PILL_LOG_LEVEL_ERROR, PILL_LOG_LEVEL_INFO,
    PILL_LOG_LEVEL_TRACE, PILL_LOG_LEVEL_WARN,
};
#[cfg(feature = "metrics")]
use pill_runtime_api::{
    PILL_METRIC_KIND_COUNTER, PILL_METRIC_KIND_GAUGE, PILL_METRIC_KIND_HISTOGRAM,
};

// =============================================================================
// Constants
// =============================================================================

/// Text substituted for a record whose bytes are not valid UTF-8.
const UNDECODABLE_RECORD: &str = "<runtime record was not valid UTF-8>";

thread_local! {
    /// Reconstructed span guards for the profiling zones currently open on
    /// this thread, innermost last.
    static OPEN_ZONES: RefCell<Vec<tracing::span::EnteredSpan>> = const { RefCell::new(Vec::new()) };
}

// =============================================================================
// Free Functions
// =============================================================================

/// Build the log sink handed to every runtime generation.
///
/// The sink is stateless: it re-emits into the host's process-global
/// subscriber, so it needs no context pointer and stays valid for the whole
/// process lifetime, which is exactly the lifetime the contract requires.
pub(crate) fn host_log_sink() -> LogSink {
    LogSink {
        context: std::ptr::null_mut(),
        emit: Some(receive_log_record),
    }
}

/// Build the metrics sink handed to every runtime generation.
///
/// A host built without the `metrics` feature installs no recorder, so it
/// hands the runtime a disabled sink rather than paying for samples nothing
/// would store.
pub(crate) fn host_metrics_sink() -> MetricsSink {
    #[cfg(feature = "metrics")]
    {
        MetricsSink {
            context: std::ptr::null_mut(),
            record: Some(receive_metric_sample),
        }
    }
    #[cfg(not(feature = "metrics"))]
    {
        MetricsSink::disabled()
    }
}

/// Decode one NUL-terminated UTF-8 argument from the runtime.
///
/// # Safety
///
/// `pointer` must either be null or address a NUL-terminated byte string that
/// stays valid for the duration of the call, which is the borrow the contract
/// grants for every sink argument.
unsafe fn decode(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: The caller guarantees a non-null `pointer` addresses a
    // NUL-terminated string valid for this call.
    match unsafe { CStr::from_ptr(pointer) }.to_str() {
        Ok(text) => text.to_string(),
        Err(_) => String::from(UNDECODABLE_RECORD),
    }
}

/// Receive one log record or profiling span edge from a runtime generation.
///
/// # Safety
///
/// Invoked by the runtime through the contract, which guarantees both string
/// pointers are null or NUL-terminated UTF-8 valid for the duration of the
/// call.
extern "C" fn receive_log_record(
    _context: *mut c_void,
    record_kind: u32,
    level: u32,
    target: *const c_char,
    message: *const c_char,
) {
    // SAFETY: The contract guarantees both pointers are null or
    // NUL-terminated buffers valid for the duration of this call.
    let (target, message) = unsafe { (decode(target), decode(message)) };

    match record_kind {
        PILL_LOG_KIND_EVENT => emit_event(&target, level, &message),
        PILL_LOG_KIND_SPAN_ENTER => open_zone(&target, &message),
        PILL_LOG_KIND_SPAN_EXIT => close_zone(),
        PILL_LOG_KIND_SPAN_RECORD => record_on_open_zone(&message),
        // An unknown record kind comes from a newer runtime than this host
        // understands; keep the text rather than dropping the diagnostic.
        _ => emit_event(&target, level, &message),
    }
}

/// Re-emit one runtime event on the matching host lane.
///
/// `tracing` metadata is static, so the runtime's target is matched against the
/// engine's fixed lanes. Anything unrecognised keeps its text and lands on the
/// general engine lane with its original target preserved in a field.
fn emit_event(target: &str, level: u32, message: &str) {
    macro_rules! emit_on_lane {
        ($lane:expr) => {
            match level {
                PILL_LOG_LEVEL_ERROR => error!(target: $lane, "{message}"),
                PILL_LOG_LEVEL_WARN => warn!(target: $lane, "{message}"),
                PILL_LOG_LEVEL_INFO => info!(target: $lane, "{message}"),
                PILL_LOG_LEVEL_DEBUG => debug!(target: $lane, "{message}"),
                PILL_LOG_LEVEL_TRACE => trace!(target: $lane, "{message}"),
                _ => info!(target: $lane, "{message}"),
            }
        };
    }

    match target {
        telemetry_target::ENGINE => emit_on_lane!(telemetry_target::ENGINE),
        telemetry_target::HOT_RELOAD => emit_on_lane!(telemetry_target::HOT_RELOAD),
        telemetry_target::INPUT => emit_on_lane!(telemetry_target::INPUT),
        telemetry_target::ECS => emit_on_lane!(telemetry_target::ECS),
        telemetry_target::RENDERING => emit_on_lane!(telemetry_target::RENDERING),
        telemetry_target::RESOURCES => emit_on_lane!(telemetry_target::RESOURCES),
        DEV_LOG_TARGET => emit_on_lane!(DEV_LOG_TARGET),
        unknown => match level {
            PILL_LOG_LEVEL_ERROR => {
                error!(target: telemetry_target::ENGINE, runtime_target = unknown, "{message}")
            }
            PILL_LOG_LEVEL_WARN => {
                warn!(target: telemetry_target::ENGINE, runtime_target = unknown, "{message}")
            }
            PILL_LOG_LEVEL_DEBUG => {
                debug!(target: telemetry_target::ENGINE, runtime_target = unknown, "{message}")
            }
            PILL_LOG_LEVEL_TRACE => {
                trace!(target: telemetry_target::ENGINE, runtime_target = unknown, "{message}")
            }
            _ => info!(target: telemetry_target::ENGINE, runtime_target = unknown, "{message}"),
        },
    }
}

/// Open a host span mirroring one runtime profiling zone.
///
/// The reconstructed span carries a fixed metadata name because a host span's
/// name must be a literal; the zone's real identity travels in the `name`
/// field, which is where the profiling macros put dynamic zone names anyway.
fn open_zone(target: &str, zone: &str) {
    let span = if target == telemetry_target::PROFILE_FINE {
        tracing::trace_span!(
            target: telemetry_target::PROFILE_FINE,
            "runtime",
            name = tracing::field::Empty,
            details = tracing::field::Empty,
        )
    } else {
        tracing::trace_span!(
            target: telemetry_target::PROFILE_COARSE,
            "runtime",
            name = tracing::field::Empty,
            details = tracing::field::Empty,
        )
    };

    // A disabled span still needs a stack entry: the matching exit is coming
    // regardless, and dropping it here would unbalance every outer zone.
    let entered = span.entered();
    if !zone.is_empty() {
        entered.record("name", zone);
    }
    OPEN_ZONES.with(|zones| zones.borrow_mut().push(entered));
}

/// Attach late-recorded fields to the innermost open zone on this thread.
fn record_on_open_zone(fields: &str) {
    if fields.is_empty() {
        return;
    }
    OPEN_ZONES.with(|zones| {
        if let Some(entered) = zones.borrow().last() {
            entered.record("details", fields);
        }
    });
}

/// Close the innermost open zone on this thread.
///
/// An exit without a matching enter is possible only if a record was lost, so
/// it is ignored rather than allowed to close an unrelated outer zone.
fn close_zone() {
    OPEN_ZONES.with(|zones| {
        let _ = zones.borrow_mut().pop();
    });
}

/// Receive one metric sample from a runtime generation.
///
/// # Safety
///
/// Invoked by the runtime through the contract, which guarantees `name` is a
/// NUL-terminated UTF-8 buffer valid for the duration of the call.
#[cfg(feature = "metrics")]
extern "C" fn receive_metric_sample(
    _context: *mut c_void,
    metric_kind: u32,
    name: *const c_char,
    value: f64,
) {
    // SAFETY: The contract guarantees `name` is null or a NUL-terminated
    // buffer valid for the duration of this call.
    let name = unsafe { decode(name) };
    if name.is_empty() {
        return;
    }

    // The metric key must outlive the macro call, and `metrics` accepts an
    // owned `String` as a key name.
    match metric_kind {
        PILL_METRIC_KIND_COUNTER => {
            metrics::counter!(name).increment(value.max(0.0) as u64);
        }
        PILL_METRIC_KIND_GAUGE => {
            metrics::gauge!(name).set(value);
        }
        PILL_METRIC_KIND_HISTOGRAM => {
            metrics::histogram!(name).record(value);
        }
        // An unknown metric kind comes from a newer runtime; recording it as a
        // gauge keeps the value visible instead of discarding it.
        _ => {
            metrics::gauge!(name).set(value);
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The host always forwards logs; metrics only when it can store them.
    #[test]
    fn sinks_reflect_the_host_build() {
        assert!(host_log_sink().is_enabled());
        assert_eq!(host_metrics_sink().is_enabled(), cfg!(feature = "metrics"));
    }

    /// A null or invalid string argument degrades instead of panicking.
    #[test]
    fn undecodable_arguments_degrade_to_placeholder_text() {
        // SAFETY: A null pointer is explicitly permitted by the contract.
        assert_eq!(unsafe { decode(std::ptr::null()) }, "");

        let invalid = [0xF0_u8, 0x28, 0x8C, 0x28, 0x00];
        // SAFETY: The buffer is NUL-terminated and lives for the whole call.
        let decoded = unsafe { decode(invalid.as_ptr() as *const c_char) };
        assert_eq!(decoded, UNDECODABLE_RECORD);
    }

    /// Zone edges keep the thread-local stack balanced, including a stray exit.
    #[test]
    fn zone_stack_stays_balanced() {
        open_zone(telemetry_target::PROFILE_COARSE, "movement");
        open_zone(telemetry_target::PROFILE_COARSE, "collision");
        record_on_open_zone("entities=100");
        OPEN_ZONES.with(|zones| assert_eq!(zones.borrow().len(), 2));

        close_zone();
        close_zone();
        // A stray exit must not underflow or close an unrelated outer zone.
        close_zone();
        OPEN_ZONES.with(|zones| assert!(zones.borrow().is_empty()));
    }

    /// Re-emitting an event never panics, whatever target or level it carries.
    #[test]
    fn events_are_re_emitted_on_every_lane() {
        emit_event(
            telemetry_target::HOT_RELOAD,
            PILL_LOG_LEVEL_INFO,
            "reloaded",
        );
        emit_event(telemetry_target::ECS, PILL_LOG_LEVEL_ERROR, "frame error");
        emit_event(DEV_LOG_TARGET, PILL_LOG_LEVEL_DEBUG, "scratch");
        emit_event("some::future::lane", PILL_LOG_LEVEL_WARN, "unknown lane");
        emit_event(telemetry_target::ENGINE, 99, "unknown level");
    }
}
