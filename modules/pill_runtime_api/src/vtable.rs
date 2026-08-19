//! The function-pointer table, its creation arguments, and layout guards.
//!
//! # Responsibilities
//!
//! - Declare [`PillRuntimeApiV1`], the complete set of calls a host may make
//!   into a loaded runtime dynamic library.
//! - Declare the creation arguments describing the window, the project module,
//!   and the telemetry routing for one runtime generation.
//! - Declare [`CapturedWorldState`], the envelope that carries world state
//!   across a runtime swap.
//! - Provide [`validate_api_table`], the guard both sides run before any other
//!   call.
//!
//! # Design
//!
//! The table is exported by the runtime as a pointer to a `'static` value, so
//! it stays valid for as long as the module is mapped and the host never
//! copies it. Validation compares the reported `struct_size` and
//! `abi_version` against the host's own compilation, which catches both a
//! stale dylib on disk and an intentional contract change.
//!
//! `abi_hash` is reserved for a future whole-contract hash and is `0` in v1.
//! Both sides accept `0` and any equal non-zero value, so introducing it later
//! stays additive.

// Standard library
use std::ffi::{c_char, c_void};
use std::fmt;

// Current crate
use crate::sink::{LogSink, MetricsSink};
use crate::types::{FrameReport, PillWindowHandleV1, RenderViewport, VirtualResolution};
use crate::version::PILL_RUNTIME_ABI_VERSION;

// =============================================================================
// Constants
// =============================================================================

/// Exported symbol every runtime dynamic library must provide.
///
/// Resolving it yields `extern "C" fn() -> *const PillRuntimeApiV1`.
pub const PILL_RUNTIME_API_SYMBOL: &[u8] = b"get_pill_runtime_api_v1\0";

// =============================================================================
// Types
// =============================================================================

/// Opaque pointer identifying one live runtime instance.
///
/// Produced by [`PillRuntimeApiV1::create`] and invalidated by
/// [`PillRuntimeApiV1::destroy`]. Exactly one instance is live per host
/// process at a time.
pub type RuntimeHandle = *mut c_void;

/// Managed-project locations the runtime needs to start the C# backend.
///
/// The host resolves every path from its own configuration so the runtime
/// never parses a project manifest. All fields are NUL-terminated UTF-8 owned
/// by the caller for the duration of the `create` call.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PillCSharpProjectV1 {
    /// Size of this struct, used as the layout guard.
    pub struct_size: u32,
    /// Absolute path of the collectible loader assembly.
    pub runtime_assembly_path_utf8: *const c_char,
    /// Absolute path of the loader's `runtimeconfig.json`.
    pub runtime_config_path_utf8: *const c_char,
    /// Assembly name of the loader, used to build the managed type name.
    pub runtime_assembly_name_utf8: *const c_char,
    /// Absolute directory holding the built project assembly.
    pub project_directory_utf8: *const c_char,
    /// File name of the project assembly inside that directory.
    pub project_assembly_file_name_utf8: *const c_char,
}

/// Everything one runtime generation needs to come up.
///
/// The host fills this immediately before each [`PillRuntimeApiV1::create`]
/// call, including after a reload, so a new generation receives exactly the
/// same window, project, and telemetry wiring as the one it replaces.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PillRuntimeCreateArgsV1 {
    /// Size of this struct, used as the layout guard.
    pub struct_size: u32,
    /// Contract revision the host was compiled against.
    pub abi_version: u32,
    /// Feature bits the host was compiled with; the runtime rejects a mismatch.
    pub features_mask: u32,
    /// Which `PILL_PROJECT_BACKEND_*` constant the project module uses.
    pub project_backend: u32,
    /// Native window to render into, or null for a headless run.
    pub window: *const PillWindowHandleV1,
    /// Physical width of that window, in pixels.
    pub width: u32,
    /// Physical height of that window, in pixels.
    pub height: u32,
    /// Absolute workspace root, used for temporary-copy directories.
    pub workspace_root_utf8: *const c_char,
    /// Built native project library to load, or null to start without one.
    pub project_module_path_utf8: *const c_char,
    /// Managed-project locations, or null for any non-C# backend.
    pub csharp_project: *const PillCSharpProjectV1,
    /// Where runtime `tracing` records are forwarded.
    pub log_sink: LogSink,
    /// Where runtime metric samples are forwarded.
    pub metrics_sink: MetricsSink,
}

/// Envelope carrying captured world state across a runtime swap.
///
/// # Ownership
///
/// The runtime allocates the envelope and its payload in
/// [`PillRuntimeApiV1::capture_world_state`] and releases both in
/// [`PillRuntimeApiV1::release_world_state`]. Because the allocation belongs
/// to the allocating module, the host must call `release_world_state` through
/// the *same* table it captured with, and must keep that module mapped until
/// it does.
///
/// [`PillRuntimeApiV1::restore_world_state`] only reads the envelope, so a
/// host may hand the same state to a replacement runtime and then, if that
/// fails, to the previous one, before releasing it exactly once.
///
/// # Layout stability
///
/// The header is declared here, in the contract crate, rather than being
/// private to the runtime: a reload swaps two independently compiled engine
/// binaries, and a runtime-private layout could differ between the generation
/// that wrote it and the generation that reads it. The engine-specific data
/// lives entirely in `payload`, a self-describing JSON document tagged with
/// its own `format_version`.
#[repr(C)]
#[derive(Debug)]
pub struct CapturedWorldState {
    /// Size of this struct, used as the layout guard.
    pub struct_size: u32,
    /// Revision of the payload document; see
    /// [`PILL_RUNTIME_STATE_FORMAT_VERSION`](crate::PILL_RUNTIME_STATE_FORMAT_VERSION).
    pub format_version: u32,
    /// Monotonic capture timestamp in nanoseconds, for diagnostics only.
    pub captured_at_nanos: u64,
    /// Start of the serialized world document.
    pub payload: *const u8,
    /// Length of that document in bytes.
    pub payload_len: u64,
    /// NUL-terminated UTF-8 human summary shown by host diagnostics.
    pub summary_utf8: *const c_char,
}

impl CapturedWorldState {
    /// Whether this envelope was produced by a matching contract build.
    pub fn has_expected_layout(&self) -> bool {
        self.struct_size as usize == std::mem::size_of::<Self>()
    }

    /// Borrow the serialized world document.
    ///
    /// Returns an empty slice when the envelope carries no payload.
    ///
    /// # Safety
    ///
    /// `payload` must either be null or point to `payload_len` initialized
    /// bytes that stay valid and immutable for the returned lifetime. The
    /// runtime that allocated the envelope guarantees this until
    /// `release_world_state` is called on it.
    pub unsafe fn payload_bytes(&self) -> &[u8] {
        if self.payload.is_null() || self.payload_len == 0 {
            return &[];
        }
        // SAFETY: The caller guarantees `payload` addresses `payload_len`
        // initialized, immutable bytes for the returned lifetime.
        unsafe { std::slice::from_raw_parts(self.payload, self.payload_len as usize) }
    }
}

// =============================================================================
// PillRuntimeApiV1
// =============================================================================

/// The complete host↔runtime function table.
///
/// Every fallible call returns [`PILL_OK`](crate::PILL_OK) or
/// [`PILL_ERR`](crate::PILL_ERR); on failure the caller reads the reason
/// through [`Self::last_error_utf8`] before making any other call on the same
/// thread.
///
/// # Safety
///
/// All calls must happen on the host's main thread, never concurrently, and
/// never while another call on the same handle is in flight. The runtime is
/// swapped only between frames, at a point where no call is active.
#[repr(C)]
pub struct PillRuntimeApiV1 {
    /// Size of this struct, used as the layout guard.
    pub struct_size: u32,
    /// Contract revision this runtime was compiled against.
    pub abi_version: u32,
    /// Reserved for a future whole-contract hash; `0` in v1.
    pub abi_hash: u64,

    // --- Diagnostics ---
    /// Message describing the most recent failure on the calling thread.
    ///
    /// The returned buffer is owned by the runtime and stays valid until the
    /// next failing call on the same thread.
    pub last_error_utf8: extern "C" fn() -> *const c_char,

    // --- Lifecycle ---
    /// Bring up one runtime generation and write its handle to `out_runtime`.
    pub create:
        extern "C" fn(args: *const PillRuntimeCreateArgsV1, out_runtime: *mut RuntimeHandle) -> i32,
    /// Tear down a generation, releasing the renderer, engine, and project.
    pub destroy: extern "C" fn(runtime: RuntimeHandle),

    // --- Frame and rendering ---
    /// Process pending project reloads, run one scheduler frame, and present.
    pub run_one_frame: extern "C" fn(runtime: RuntimeHandle) -> i32,
    /// Take the periodic console report produced by the last frame.
    ///
    /// Returns `1` and fills `out_report` when a report was pending, `0`
    /// otherwise. The report is consumed, so each one is returned once.
    pub take_frame_report:
        extern "C" fn(runtime: RuntimeHandle, out_report: *mut FrameReport) -> i32,
    /// Read live frame statistics without consuming the periodic report.
    pub current_frame_report:
        extern "C" fn(runtime: RuntimeHandle, out_report: *mut FrameReport) -> i32,
    /// Forward a physical window resize to the renderer.
    pub resize: extern "C" fn(runtime: RuntimeHandle, width: u32, height: u32),
    /// Restrict drawing to a physical region, or pass null for the full surface.
    pub set_viewport: extern "C" fn(runtime: RuntimeHandle, viewport: *const RenderViewport) -> i32,
    /// Map a stable project coordinate space into the viewport, or pass null
    /// to make logical units match physical pixels again.
    pub set_virtual_resolution:
        extern "C" fn(runtime: RuntimeHandle, resolution: *const VirtualResolution) -> i32,
    /// Move rendering to another native window, such as a detached editor scene.
    pub retarget_render_window: extern "C" fn(
        runtime: RuntimeHandle,
        window: *const PillWindowHandleV1,
        width: u32,
        height: u32,
    ) -> i32,

    // --- Project module ---
    /// Swap in a rebuilt project module, preserving world state.
    ///
    /// `project_module_path` names the freshly built native library; it is
    /// ignored by the managed backend, which polls its collectible loader.
    pub reload_project:
        extern "C" fn(runtime: RuntimeHandle, project_module_path: *const c_char) -> i32,

    // --- Engine hot reload ---
    /// Serialize the world into a new envelope owned by this runtime.
    pub capture_world_state:
        extern "C" fn(runtime: RuntimeHandle, out_state: *mut *mut CapturedWorldState) -> i32,
    /// Rebuild the world from an envelope, migrating changed schemas first.
    pub restore_world_state:
        extern "C" fn(runtime: RuntimeHandle, state: *const CapturedWorldState) -> i32,
    /// Release an envelope produced by this same runtime.
    pub release_world_state: extern "C" fn(state: *mut CapturedWorldState),

    // --- State diagnostics ---
    /// Payload size of an envelope, in bytes.
    pub state_byte_len: extern "C" fn(state: *const CapturedWorldState) -> u64,
    /// Write a human summary of an envelope to `out_summary`.
    pub describe_state:
        extern "C" fn(state: *const CapturedWorldState, out_summary: *mut *const c_char) -> i32,

    // --- Exit signal ---
    /// Whether the runtime asked the host to stop the frame loop.
    pub is_exit_requested: extern "C" fn(runtime: RuntimeHandle) -> i32,
}

// =============================================================================
// ApiValidationError
// =============================================================================

/// Why a loaded runtime table was refused.
///
/// The host keeps its current generation on any of these and logs the expected
/// and actual values so a stale or mismatched dylib is obvious.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiValidationError {
    /// The exported accessor returned a null table pointer.
    NullTable,
    /// The table reports a different contract revision.
    AbiVersionMismatch {
        /// Revision this host was compiled against.
        expected: u32,
        /// Revision the loaded runtime reports.
        actual: u32,
    },
    /// The table reports a different size, so its field layout differs.
    StructSizeMismatch {
        /// Size this host was compiled against.
        expected: u32,
        /// Size the loaded runtime reports.
        actual: u32,
    },
    /// The table reports a contract hash this host cannot satisfy.
    AbiHashMismatch {
        /// Hash this host was compiled against.
        expected: u64,
        /// Hash the loaded runtime reports.
        actual: u64,
    },
}

impl fmt::Display for ApiValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullTable => write!(formatter, "the runtime returned a null API table"),
            Self::AbiVersionMismatch { expected, actual } => write!(
                formatter,
                "runtime ABI version mismatch: host expects {expected}, runtime reports {actual}"
            ),
            Self::StructSizeMismatch { expected, actual } => write!(
                formatter,
                "runtime ABI layout mismatch: host expects {expected} bytes, runtime reports {actual} bytes"
            ),
            Self::AbiHashMismatch { expected, actual } => write!(
                formatter,
                "runtime ABI hash mismatch: host expects 0x{expected:016X}, runtime reports 0x{actual:016X}"
            ),
        }
    }
}

impl std::error::Error for ApiValidationError {}

// =============================================================================
// Free Functions
// =============================================================================

/// Verify a loaded table before any of its function pointers are called.
///
/// Checks are ordered from cheapest and most fundamental to most specific so
/// the reported failure always names the real cause: a null table, then the
/// declared revision, then the physical layout, then the reserved contract
/// hash.
///
/// # Errors
///
/// Returns the [`ApiValidationError`] describing the first failed guard.
///
/// # Safety
///
/// `table` must either be null or point to a `PillRuntimeApiV1` that stays
/// valid for the duration of this call, which holds for the `'static` table a
/// mapped runtime exports.
pub unsafe fn validate_api_table(table: *const PillRuntimeApiV1) -> Result<(), ApiValidationError> {
    // Step 1: Reject a null table before any field is read.
    if table.is_null() {
        return Err(ApiValidationError::NullTable);
    }

    // SAFETY: The caller guarantees a non-null `table` points to a live
    // `PillRuntimeApiV1`; the null case returned above.
    let table = unsafe { &*table };

    // Step 2: Reject an intentional contract change.
    if table.abi_version != PILL_RUNTIME_ABI_VERSION {
        return Err(ApiValidationError::AbiVersionMismatch {
            expected: PILL_RUNTIME_ABI_VERSION,
            actual: table.abi_version,
        });
    }

    // Step 3: Reject accidental layout drift between two builds claiming the
    // same revision.
    let expected_size = std::mem::size_of::<PillRuntimeApiV1>() as u32;
    if table.struct_size != expected_size {
        return Err(ApiValidationError::StructSizeMismatch {
            expected: expected_size,
            actual: table.struct_size,
        });
    }

    // Step 4: Compare the reserved whole-contract hash. Both sides report `0`
    // in v1, so introducing a real hash later stays additive.
    if table.abi_hash != 0 {
        return Err(ApiValidationError::AbiHashMismatch {
            expected: 0,
            actual: table.abi_hash,
        });
    }

    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a table whose function pointers are never called by these tests.
    fn sample_table() -> PillRuntimeApiV1 {
        extern "C" fn last_error() -> *const c_char {
            std::ptr::null()
        }
        extern "C" fn create(
            _args: *const PillRuntimeCreateArgsV1,
            _out: *mut RuntimeHandle,
        ) -> i32 {
            crate::PILL_ERR
        }
        extern "C" fn destroy(_runtime: RuntimeHandle) {}
        extern "C" fn run_one_frame(_runtime: RuntimeHandle) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn frame_report(_runtime: RuntimeHandle, _out: *mut FrameReport) -> i32 {
            0
        }
        extern "C" fn resize(_runtime: RuntimeHandle, _width: u32, _height: u32) {}
        extern "C" fn set_viewport(
            _runtime: RuntimeHandle,
            _viewport: *const RenderViewport,
        ) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn set_virtual_resolution(
            _runtime: RuntimeHandle,
            _resolution: *const VirtualResolution,
        ) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn retarget(
            _runtime: RuntimeHandle,
            _window: *const PillWindowHandleV1,
            _width: u32,
            _height: u32,
        ) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn reload_project(_runtime: RuntimeHandle, _path: *const c_char) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn capture(_runtime: RuntimeHandle, _out: *mut *mut CapturedWorldState) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn restore(_runtime: RuntimeHandle, _state: *const CapturedWorldState) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn release(_state: *mut CapturedWorldState) {}
        extern "C" fn byte_len(_state: *const CapturedWorldState) -> u64 {
            0
        }
        extern "C" fn describe(_state: *const CapturedWorldState, _out: *mut *const c_char) -> i32 {
            crate::PILL_OK
        }
        extern "C" fn is_exit_requested(_runtime: RuntimeHandle) -> i32 {
            0
        }

        PillRuntimeApiV1 {
            struct_size: std::mem::size_of::<PillRuntimeApiV1>() as u32,
            abi_version: PILL_RUNTIME_ABI_VERSION,
            abi_hash: 0,
            last_error_utf8: last_error,
            create,
            destroy,
            run_one_frame,
            take_frame_report: frame_report,
            current_frame_report: frame_report,
            resize,
            set_viewport,
            set_virtual_resolution,
            retarget_render_window: retarget,
            reload_project,
            capture_world_state: capture,
            restore_world_state: restore,
            release_world_state: release,
            state_byte_len: byte_len,
            describe_state: describe,
            is_exit_requested,
        }
    }

    /// A table built by this same contract revision validates.
    #[test]
    fn matching_table_validates() {
        let table = sample_table();
        // SAFETY: `table` is a live local value for the duration of the call.
        assert_eq!(unsafe { validate_api_table(&table) }, Ok(()));
    }

    /// A null table is rejected before any field is read.
    #[test]
    fn null_table_is_rejected() {
        // SAFETY: A null pointer is explicitly permitted by the contract.
        assert_eq!(
            unsafe { validate_api_table(std::ptr::null()) },
            Err(ApiValidationError::NullTable)
        );
    }

    /// A runtime built against another contract revision is rejected.
    #[test]
    fn abi_version_mismatch_is_rejected() {
        let mut table = sample_table();
        table.abi_version = PILL_RUNTIME_ABI_VERSION + 1;
        // SAFETY: `table` is a live local value for the duration of the call.
        assert_eq!(
            unsafe { validate_api_table(&table) },
            Err(ApiValidationError::AbiVersionMismatch {
                expected: PILL_RUNTIME_ABI_VERSION,
                actual: PILL_RUNTIME_ABI_VERSION + 1,
            })
        );
    }

    /// Layout drift under the same revision is rejected.
    #[test]
    fn struct_size_mismatch_is_rejected() {
        let mut table = sample_table();
        table.struct_size = 8;
        // SAFETY: `table` is a live local value for the duration of the call.
        assert_eq!(
            unsafe { validate_api_table(&table) },
            Err(ApiValidationError::StructSizeMismatch {
                expected: std::mem::size_of::<PillRuntimeApiV1>() as u32,
                actual: 8,
            })
        );
    }

    /// The reserved contract hash must stay zero for v1 runtimes.
    #[test]
    fn non_zero_abi_hash_is_rejected() {
        let mut table = sample_table();
        table.abi_hash = 0xDEAD_BEEF;
        // SAFETY: `table` is a live local value for the duration of the call.
        assert_eq!(
            unsafe { validate_api_table(&table) },
            Err(ApiValidationError::AbiHashMismatch {
                expected: 0,
                actual: 0xDEAD_BEEF,
            })
        );
    }

    /// An envelope carrying no payload borrows an empty slice rather than
    /// dereferencing a null pointer.
    #[test]
    fn empty_state_payload_is_borrowable() {
        let state = CapturedWorldState {
            struct_size: std::mem::size_of::<CapturedWorldState>() as u32,
            format_version: crate::PILL_RUNTIME_STATE_FORMAT_VERSION,
            captured_at_nanos: 0,
            payload: std::ptr::null(),
            payload_len: 0,
            summary_utf8: std::ptr::null(),
        };
        assert!(state.has_expected_layout());
        // SAFETY: The envelope carries a null payload, which the accessor
        // handles by returning an empty slice.
        assert!(unsafe { state.payload_bytes() }.is_empty());
    }
}
