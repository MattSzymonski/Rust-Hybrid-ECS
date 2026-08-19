// REQUIREMENTS: the `pill_runtime` crate built as an rlib (its default test target)
// DESCRIPTION: Verifies that telemetry produced inside the engine runtime reaches
//              the log sink the host installs through the create arguments.
// USAGE: cargo test -p pill_runtime --test runtime_telemetry
// EXAMPLE USAGE: cargo test -p pill_runtime --test runtime_telemetry
// --- SCRIPT ---

//! Integration coverage for the runtime's telemetry routing.
//!
//! # Responsibilities
//!
//! - Verify that a generation forwards its `tracing` records to the host sink.
//! - Verify that the buffers a forwarded record carries are readable.
//!
//! # Design
//!
//! The sink pointers the runtime forwards through are process-global, because
//! `tracing` resolves its subscriber through a process-global static. This file
//! is therefore a separate integration binary - cargo runs each file under
//! `tests/` as its own process - so no concurrently created generation can
//! replace the sink under test halfway through.

// Standard library
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicU32, Ordering};

// External crates
use pill_runtime::get_pill_runtime_api_v1;
use pill_runtime_api::{
    CapturedWorldState, LogSink, MetricsSink, PillRuntimeCreateArgsV1, RuntimeHandle, PILL_OK,
    PILL_PROJECT_BACKEND_NONE, PILL_RUNTIME_ABI_VERSION,
};

// =============================================================================
// Test fixtures
// =============================================================================

/// Number of records the counting log sink has received.
static LOG_RECORD_COUNT: AtomicU32 = AtomicU32::new(0);

/// Number of forwarded records whose target buffer was readable.
static READABLE_TARGET_COUNT: AtomicU32 = AtomicU32::new(0);

/// Count one forwarded log record without retaining any of its buffers.
extern "C" fn counting_log_sink(
    _context: *mut c_void,
    _record_kind: u32,
    _level: u32,
    target: *const c_char,
    _message: *const c_char,
) {
    LOG_RECORD_COUNT.fetch_add(1, Ordering::Relaxed);
    if target.is_null() {
        return;
    }
    // Decoding the target proves the runtime handed over a live buffer rather
    // than a dangling pointer, which is the failure mode worth catching here.
    // SAFETY: The contract guarantees `target` is a NUL-terminated buffer valid
    // for the duration of this call.
    if unsafe { CStr::from_ptr(target) }.to_str().is_ok() {
        READABLE_TARGET_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Number of samples the counting metrics sink has received.
#[cfg(feature = "metrics")]
static METRIC_SAMPLE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Number of samples that named the runtime's per-frame entity gauge.
#[cfg(feature = "metrics")]
static ENTITY_GAUGE_SAMPLE_COUNT: AtomicU32 = AtomicU32::new(0);

/// Count one forwarded metric sample without retaining its key buffer.
#[cfg(feature = "metrics")]
extern "C" fn counting_metrics_sink(
    _context: *mut c_void,
    _metric_kind: u32,
    name: *const c_char,
    _value: f64,
) {
    METRIC_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    if name.is_null() {
        return;
    }
    // SAFETY: The contract guarantees `name` is a NUL-terminated buffer valid
    // for the duration of this call.
    if unsafe { CStr::from_ptr(name) }.to_str() == Ok("ecs.entities") {
        ENTITY_GAUGE_SAMPLE_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Build the metrics sink this test installs, if the feature is compiled in.
fn test_metrics_sink() -> MetricsSink {
    #[cfg(feature = "metrics")]
    {
        MetricsSink {
            context: std::ptr::null_mut(),
            record: Some(counting_metrics_sink),
        }
    }
    #[cfg(not(feature = "metrics"))]
    {
        MetricsSink::disabled()
    }
}

/// Feature bits this test binary was compiled with.
fn feature_mask() -> u32 {
    let mut mask = 0;
    if cfg!(feature = "rendering") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_RENDERING;
    }
    if cfg!(feature = "metrics") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_METRICS;
    }
    if cfg!(feature = "profiling") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_PROFILING;
    }
    if cfg!(feature = "dev-logs") {
        mask |= pill_runtime_api::PILL_RUNTIME_FEATURE_DEV_LOGS;
    }
    mask
}

// =============================================================================
// Tests
// =============================================================================

/// Runtime telemetry reaches the log sink the host installed.
#[test]
fn runtime_logs_reach_the_host_sink() {
    let table = get_pill_runtime_api_v1();
    assert!(!table.is_null(), "the runtime must export its API table");
    // SAFETY: `get_pill_runtime_api_v1` returns the crate's `'static` table.
    let table = unsafe { &*table };

    let workspace_root = CString::new(
        std::env::temp_dir()
            .join(format!("pill_runtime_telemetry_{}", std::process::id()))
            .to_string_lossy()
            .into_owned(),
    )
    .expect("a temporary directory path has no interior NUL");

    let args = PillRuntimeCreateArgsV1 {
        struct_size: std::mem::size_of::<PillRuntimeCreateArgsV1>() as u32,
        abi_version: PILL_RUNTIME_ABI_VERSION,
        features_mask: feature_mask(),
        project_backend: PILL_PROJECT_BACKEND_NONE,
        window: std::ptr::null(),
        width: 0,
        height: 0,
        workspace_root_utf8: workspace_root.as_ptr(),
        project_module_path_utf8: std::ptr::null(),
        csharp_project: std::ptr::null(),
        log_sink: LogSink {
            context: std::ptr::null_mut(),
            emit: Some(counting_log_sink),
        },
        metrics_sink: test_metrics_sink(),
    };

    let mut handle: RuntimeHandle = std::ptr::null_mut();
    assert_eq!((table.create)(&args, &mut handle), PILL_OK);
    assert!(!handle.is_null());

    // Frames drive the per-frame metric samples; a capture followed by a
    // restore always logs its progress, so together they are a reliable source
    // of both record kinds without depending on a particular startup message
    // surviving future changes.
    for _ in 0..8 {
        assert_eq!((table.run_one_frame)(handle), PILL_OK);
    }
    let mut envelope: *mut CapturedWorldState = std::ptr::null_mut();
    assert_eq!((table.capture_world_state)(handle, &mut envelope), PILL_OK);
    assert_eq!((table.restore_world_state)(handle, envelope), PILL_OK);
    (table.release_world_state)(envelope);
    (table.destroy)(handle);

    let forwarded = LOG_RECORD_COUNT.load(Ordering::Relaxed);
    assert!(
        forwarded > 0,
        "the runtime must forward its telemetry to the host sink"
    );
    assert_eq!(
        READABLE_TARGET_COUNT.load(Ordering::Relaxed),
        forwarded,
        "every forwarded record must carry a readable target buffer"
    );

    #[cfg(feature = "metrics")]
    {
        assert!(
            METRIC_SAMPLE_COUNT.load(Ordering::Relaxed) > 0,
            "the runtime must forward its metric samples to the host sink"
        );
        assert!(
            ENTITY_GAUGE_SAMPLE_COUNT.load(Ordering::Relaxed) > 0,
            "the per-frame entity gauge must reach the host sink by name"
        );
    }
}
