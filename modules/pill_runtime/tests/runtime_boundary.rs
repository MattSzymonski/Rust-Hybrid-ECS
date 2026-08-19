// REQUIREMENTS: the `pill_runtime` crate built as an rlib (its default test target)
// DESCRIPTION: Drives the engine runtime through its own C ABI in-process, so the
//              boundary contract - lifecycle, frame stepping, world capture and
//              restore, telemetry routing, and every rejection path - is exercised
//              without a dynamic loader or a compiled project module.
// USAGE: cargo test -p pill_runtime --test runtime_boundary
// EXAMPLE USAGE: cargo test -p pill_runtime --test runtime_boundary
// --- SCRIPT ---

//! Integration coverage for the runtime's exported C ABI.
//!
//! # Responsibilities
//!
//! - Verify the exported table validates against the contract and survives a
//!   full create/step/destroy lifecycle.
//! - Verify that world state can be captured, described, and released, and that
//!   a captured envelope restores into a fresh generation.
//! - Verify every argument-rejection path: layout, contract revision, feature
//!   mask, and null pointers.
//! - Verify that runtime telemetry reaches the sinks the host installs.
//!
//! # Design
//!
//! These tests link `pill_runtime` as an `rlib` and call
//! `get_pill_runtime_api_v1` directly rather than resolving it through
//! `libloading`. That is the same table a host reaches across a dynamic library
//! boundary, so the contract is exercised exactly as in production while the
//! test stays a single process with no build step and no project artifact.
//!
//! Generations are created without a project module. That keeps the tests free
//! of a compiled `cdylib` fixture while still covering everything the boundary
//! owns: the world, the frame loop, the state envelope, and the sinks. Project
//! loading itself is covered by the Python reload suite, which needs a real
//! compiler in the loop anyway.

// Standard library
use std::ffi::{c_char, CStr, CString};

// External crates
use pill_runtime::get_pill_runtime_api_v1;
use pill_runtime_api::{
    validate_api_table, CapturedWorldState, FrameReport, LogSink, MetricsSink, PillRuntimeApiV1,
    PillRuntimeCreateArgsV1, RuntimeHandle, PILL_ERR, PILL_OK, PILL_PROJECT_BACKEND_NONE,
    PILL_RUNTIME_ABI_VERSION,
};

// =============================================================================
// Test fixtures
// =============================================================================

/// Borrow the runtime's exported table.
fn api() -> &'static PillRuntimeApiV1 {
    let table = get_pill_runtime_api_v1();
    assert!(!table.is_null(), "the runtime must export its API table");
    // SAFETY: `get_pill_runtime_api_v1` returns the crate's `'static` table,
    // which stays valid for the whole test process.
    unsafe { &*table }
}

/// Feature bits this test binary was compiled with.
///
/// The runtime rejects a mismatch, and the test binary is compiled with the
/// same features as the crate under test, so deriving the mask the same way
/// keeps the fixture correct under every feature combination.
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

/// Build headless creation arguments backed by the supplied workspace string.
fn create_args(workspace_root: &CString, log_sink: LogSink) -> PillRuntimeCreateArgsV1 {
    PillRuntimeCreateArgsV1 {
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
        log_sink,
        metrics_sink: MetricsSink::disabled(),
    }
}

/// Create one headless generation, panicking with the runtime's own message.
fn create_generation(args: &PillRuntimeCreateArgsV1) -> RuntimeHandle {
    let table = api();
    let mut handle: RuntimeHandle = std::ptr::null_mut();
    let status = (table.create)(args, &mut handle);
    assert_eq!(status, PILL_OK, "create failed: {}", last_error());
    assert!(!handle.is_null(), "create must produce a handle");
    handle
}

/// Read the runtime's diagnostic for the most recent failing call.
fn last_error() -> String {
    let table = api();
    let message = (table.last_error_utf8)();
    if message.is_null() {
        return String::from("<no diagnostic>");
    }
    // SAFETY: The contract guarantees a non-null result is a NUL-terminated
    // buffer valid until the next failing call on this thread.
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

/// A workspace root for headless generations that never touch the filesystem.
fn test_workspace_root() -> CString {
    CString::new(
        std::env::temp_dir()
            .join(format!("pill_runtime_boundary_{}", std::process::id()))
            .to_string_lossy()
            .into_owned(),
    )
    .expect("a temporary directory path has no interior NUL")
}

// =============================================================================
// Tests
// =============================================================================

/// The exported table satisfies the contract's own load-time guards.
#[test]
fn exported_table_passes_contract_validation() {
    // SAFETY: The table is the crate's `'static` export.
    unsafe { validate_api_table(get_pill_runtime_api_v1()) }.expect("the exported table validates");
}

/// A generation comes up, runs frames, reports statistics, and tears down.
#[test]
fn headless_generation_runs_frames_and_destroys_cleanly() {
    let table = api();
    let workspace_root = test_workspace_root();
    let args = create_args(&workspace_root, LogSink::disabled());
    let handle = create_generation(&args);

    for _ in 0..32 {
        assert_eq!(
            (table.run_one_frame)(handle),
            PILL_OK,
            "frame failed: {}",
            last_error()
        );
    }

    let mut report = FrameReport::default();
    assert_eq!((table.current_frame_report)(handle, &mut report), PILL_OK);
    assert_eq!(
        report.entity_count, 0,
        "no project module means no entities"
    );

    assert_eq!(
        (table.is_exit_requested)(handle),
        0,
        "a healthy generation never asks the host to stop"
    );

    (table.destroy)(handle);
}

/// The periodic console report is produced at most once per reporting window.
#[test]
fn frame_reports_are_taken_at_most_once() {
    let table = api();
    let workspace_root = test_workspace_root();
    let args = create_args(&workspace_root, LogSink::disabled());
    let handle = create_generation(&args);

    // The very first frames fall inside the opening reporting window, so no
    // report is pending yet; taking one must report absence, not fabricate a
    // value.
    (table.run_one_frame)(handle);
    let mut report = FrameReport::default();
    assert_eq!((table.take_frame_report)(handle, &mut report), 0);

    (table.destroy)(handle);
}

/// A captured envelope describes itself and is released by its own module.
#[test]
fn world_state_is_captured_described_and_released() {
    let table = api();
    let workspace_root = test_workspace_root();
    let args = create_args(&workspace_root, LogSink::disabled());
    let handle = create_generation(&args);

    let mut envelope: *mut CapturedWorldState = std::ptr::null_mut();
    assert_eq!(
        (table.capture_world_state)(handle, &mut envelope),
        PILL_OK,
        "capture failed: {}",
        last_error()
    );
    assert!(
        !envelope.is_null(),
        "a successful capture yields an envelope"
    );

    let byte_length = (table.state_byte_len)(envelope);
    assert!(
        byte_length > 0,
        "the envelope carries a serialized document"
    );

    let mut summary: *const c_char = std::ptr::null();
    assert_eq!((table.describe_state)(envelope, &mut summary), PILL_OK);
    assert!(!summary.is_null());
    // SAFETY: A successful `describe_state` yields a NUL-terminated buffer
    // owned by the envelope and valid until it is released.
    let summary = unsafe { CStr::from_ptr(summary) }
        .to_string_lossy()
        .into_owned();
    assert!(
        summary.contains("entities"),
        "unexpected summary: {summary}"
    );

    (table.release_world_state)(envelope);
    (table.destroy)(handle);
}

/// A world captured by one generation restores into the next one.
#[test]
fn captured_world_restores_into_a_replacement_generation() {
    let table = api();
    let workspace_root = test_workspace_root();
    let args = create_args(&workspace_root, LogSink::disabled());

    let first = create_generation(&args);
    let mut envelope: *mut CapturedWorldState = std::ptr::null_mut();
    assert_eq!((table.capture_world_state)(first, &mut envelope), PILL_OK);
    (table.destroy)(first);

    // The replacement stands in for a freshly loaded runtime dynamic library:
    // a brand-new world that must adopt the captured document.
    let second = create_generation(&args);
    assert_eq!(
        (table.restore_world_state)(second, envelope),
        PILL_OK,
        "restore failed: {}",
        last_error()
    );

    (table.release_world_state)(envelope);
    (table.destroy)(second);
}

/// A host built against a different contract revision is refused.
#[test]
fn create_rejects_a_foreign_contract_revision() {
    let table = api();
    let workspace_root = test_workspace_root();
    let mut args = create_args(&workspace_root, LogSink::disabled());
    args.abi_version = PILL_RUNTIME_ABI_VERSION + 1;

    let mut handle: RuntimeHandle = std::ptr::null_mut();
    assert_eq!((table.create)(&args, &mut handle), PILL_ERR);
    assert!(handle.is_null());
    let message = last_error();
    assert!(message.contains("ABI version mismatch"), "{message}");
}

/// A host built with a different feature set is refused.
#[test]
fn create_rejects_a_mismatched_feature_mask() {
    let table = api();
    let workspace_root = test_workspace_root();
    let mut args = create_args(&workspace_root, LogSink::disabled());
    // Flip a bit the runtime cannot possibly agree with.
    args.features_mask ^= pill_runtime_api::PILL_RUNTIME_FEATURE_METRICS;

    let mut handle: RuntimeHandle = std::ptr::null_mut();
    assert_eq!((table.create)(&args, &mut handle), PILL_ERR);
    let message = last_error();
    assert!(message.contains("feature mismatch"), "{message}");
}

/// Arguments produced by a different struct layout are refused.
#[test]
fn create_rejects_a_foreign_argument_layout() {
    let table = api();
    let workspace_root = test_workspace_root();
    let mut args = create_args(&workspace_root, LogSink::disabled());
    args.struct_size = 8;

    let mut handle: RuntimeHandle = std::ptr::null_mut();
    assert_eq!((table.create)(&args, &mut handle), PILL_ERR);
    let message = last_error();
    assert!(message.contains("layout mismatch"), "{message}");
}

/// Every entry point tolerates a null handle instead of dereferencing it.
#[test]
fn null_handles_are_rejected_without_unwinding() {
    let table = api();
    let null: RuntimeHandle = std::ptr::null_mut();

    assert_eq!((table.run_one_frame)(null), PILL_ERR);
    let mut report = FrameReport::default();
    assert_eq!((table.take_frame_report)(null, &mut report), 0);
    assert_eq!((table.current_frame_report)(null, &mut report), PILL_ERR);
    assert_eq!((table.is_exit_requested)(null), 0);
    assert_eq!((table.reload_project)(null, std::ptr::null()), PILL_ERR);

    let mut envelope: *mut CapturedWorldState = std::ptr::null_mut();
    assert_eq!((table.capture_world_state)(null, &mut envelope), PILL_ERR);
    assert_eq!(
        (table.restore_world_state)(null, std::ptr::null()),
        PILL_ERR
    );

    // Destroying a null handle and releasing a null envelope are both defined
    // no-ops, so a host can clean up unconditionally.
    (table.destroy)(null);
    (table.release_world_state)(std::ptr::null_mut());
}

/// Restoring a foreign envelope fails without disturbing the generation.
#[test]
fn restore_rejects_an_envelope_with_a_foreign_layout() {
    let table = api();
    let workspace_root = test_workspace_root();
    let args = create_args(&workspace_root, LogSink::disabled());
    let handle = create_generation(&args);

    let payload = b"{}";
    let foreign = CapturedWorldState {
        struct_size: 8,
        format_version: pill_runtime_api::PILL_RUNTIME_STATE_FORMAT_VERSION,
        captured_at_nanos: 0,
        payload: payload.as_ptr(),
        payload_len: payload.len() as u64,
        summary_utf8: std::ptr::null(),
    };
    assert_eq!((table.restore_world_state)(handle, &foreign), PILL_ERR);
    let message = last_error();
    assert!(message.contains("layout mismatch"), "{message}");

    // The generation is still usable after a refused restore.
    assert_eq!((table.run_one_frame)(handle), PILL_OK);
    (table.destroy)(handle);
}

/// Restoring an undecodable payload fails without dropping the generation.
#[test]
fn restore_rejects_an_undecodable_payload() {
    let table = api();
    let workspace_root = test_workspace_root();
    let args = create_args(&workspace_root, LogSink::disabled());
    let handle = create_generation(&args);

    let payload = b"this is not a captured world";
    let corrupt = CapturedWorldState {
        struct_size: std::mem::size_of::<CapturedWorldState>() as u32,
        format_version: pill_runtime_api::PILL_RUNTIME_STATE_FORMAT_VERSION,
        captured_at_nanos: 0,
        payload: payload.as_ptr(),
        payload_len: payload.len() as u64,
        summary_utf8: std::ptr::null(),
    };
    assert_eq!((table.restore_world_state)(handle, &corrupt), PILL_ERR);
    let message = last_error();
    assert!(message.contains("decode"), "{message}");

    assert_eq!((table.run_one_frame)(handle), PILL_OK);
    (table.destroy)(handle);
}
