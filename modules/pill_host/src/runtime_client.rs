//! Typed host-side access to one loaded engine runtime generation.
//!
//! # Responsibilities
//!
//! - Stage a built runtime dylib into a uniquely named, loadable copy.
//! - Map it, validate its C ABI, and create one runtime generation.
//! - Expose every boundary call as a safe, typed method.
//! - Keep every retired generation mapped for the process lifetime.
//!
//! # Design
//!
//! [`RuntimeClient`] is the only place in the host that touches the C ABI.
//! Every method converts the contract's status codes into a typed
//! [`RuntimeError`], reading the runtime's thread-local message immediately
//! after a failing call so the diagnostic is never lost to a later call.
//!
//! A retired generation is never unmapped. [`RuntimeGraveyard`] keeps every
//! previous module mapped for the process lifetime, which is the same rule the
//! project-DLL graveyard inside the runtime follows, applied one level up.
//! Destroying a generation tears down its world and its renderer, but not the
//! process-wide state its code installed: the Rayon worker threads its engine
//! started are still parked in its text section, its `tracing` subscriber is
//! still the registry its own statics point at, and its thread-locals still
//! exist on every thread that touched it. Unmapping it would leave all of that
//! executing freed pages, so the module stays and only its staged file is
//! reclaimed - by the next host startup, which clears stale temporary copies.
//!
//! World state is copied out of the runtime's envelope into host memory as
//! soon as it is captured, and the envelope is released through the very table
//! that produced it. That keeps the allocation and its free in one module and
//! frees the rollback path from having to keep a specific generation alive.

// Standard library
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};

// External crates
use pill_core::error::RuntimeError;
use pill_core::hot_reload::{
    native_library_extension, runtime_staged_file_name, runtime_staging_directory,
};
use pill_core::{debug, info};
use pill_runtime_api::loader::LoadedRuntimeModule;
use pill_runtime_api::{
    CapturedWorldState, FrameReport, LogSink, MetricsSink, PillCSharpProjectV1,
    PillRuntimeCreateArgsV1, PillWindowHandleV1, RenderViewport, RuntimeHandle, VirtualResolution,
    PILL_OK, PILL_RUNTIME_ABI_VERSION,
};

// Current crate
use crate::config::{ProjectModuleBackend, ProjectModuleConfig};

// =============================================================================
// Types
// =============================================================================

/// Host-owned copy of one captured world.
///
/// The bytes are copied out of the runtime's envelope immediately, so the host
/// can hand the same state to a replacement generation and then, if that
/// fails, to the previous one, without keeping any particular module alive for
/// the buffer's sake.
#[derive(Debug, Clone)]
pub(crate) struct CapturedWorld {
    /// Revision of the payload document.
    format_version: u32,
    /// Monotonic capture timestamp, retained for diagnostics.
    captured_at_nanos: u64,
    /// The serialized world document.
    payload: Vec<u8>,
    /// Human summary reported by the capturing runtime.
    summary: String,
}

impl CapturedWorld {
    /// Human summary reported by the capturing runtime.
    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }

    /// Size of the captured document, in bytes.
    pub(crate) fn byte_len(&self) -> usize {
        self.payload.len()
    }
}

/// Everything one runtime generation is created with.
///
/// Held by the host across reloads so a replacement generation receives
/// byte-identical wiring: same window, same project artifact, same telemetry
/// sinks.
pub(crate) struct RuntimeCreateContext {
    /// Absolute workspace root, used for temporary-copy directories.
    pub(crate) workspace_root: CString,
    /// Native window to render into, or headless.
    pub(crate) window: PillWindowHandleV1,
    /// Physical width of that window, in pixels.
    pub(crate) width: u32,
    /// Physical height of that window, in pixels.
    pub(crate) height: u32,
    /// Which backend the project module uses.
    pub(crate) project_backend: u32,
    /// Built native project library, when one exists yet.
    pub(crate) project_module_path: Option<CString>,
    /// Managed-project locations, when the C# backend is selected.
    pub(crate) csharp: Option<CSharpProjectStrings>,
    /// Where runtime `tracing` records are forwarded.
    pub(crate) log_sink: LogSink,
    /// Where runtime metric samples are forwarded.
    pub(crate) metrics_sink: MetricsSink,
}

/// Owned C strings backing a [`PillCSharpProjectV1`] argument.
///
/// The contract only borrows the pointers for the duration of the `create`
/// call, so the buffers must outlive the temporary struct that references them.
pub(crate) struct CSharpProjectStrings {
    /// Absolute path of the collectible loader assembly.
    pub(crate) runtime_assembly_path: CString,
    /// Absolute path of the loader's `runtimeconfig.json`.
    pub(crate) runtime_config_path: CString,
    /// Assembly name of the loader.
    pub(crate) runtime_assembly_name: CString,
    /// Absolute directory holding the built project assembly.
    pub(crate) project_directory: CString,
    /// File name of the project assembly inside that directory.
    pub(crate) project_assembly_file_name: CString,
}

impl CSharpProjectStrings {
    /// Build the borrowed `#[repr(C)]` view of these buffers.
    fn as_abi(&self) -> PillCSharpProjectV1 {
        PillCSharpProjectV1 {
            struct_size: std::mem::size_of::<PillCSharpProjectV1>() as u32,
            runtime_assembly_path_utf8: self.runtime_assembly_path.as_ptr(),
            runtime_config_path_utf8: self.runtime_config_path.as_ptr(),
            runtime_assembly_name_utf8: self.runtime_assembly_name.as_ptr(),
            project_directory_utf8: self.project_directory.as_ptr(),
            project_assembly_file_name_utf8: self.project_assembly_file_name.as_ptr(),
        }
    }
}

/// One mapped runtime module, with or without a live generation inside it.
///
/// Mapping and creating are deliberately separate steps. A reload maps and
/// validates the replacement while the running generation is still live -
/// neither touches the GPU - and only tears the running one down once the
/// replacement is known to be loadable.
pub(crate) struct RuntimeClient {
    /// The mapped module and its validated function table.
    module: LoadedRuntimeModule,
    /// Staged file the module was mapped from, deleted when it is retired.
    staged_path: PathBuf,
    /// Opaque handle identifying the live generation, or null when none is.
    handle: RuntimeHandle,
}

/// Retired runtime modules, kept mapped for the process lifetime.
///
/// Nothing is ever evicted; see this module's design note for why unmapping a
/// destroyed generation is unsound. The graveyard exists to own the handles so
/// they cannot be dropped by accident, and to report how many generations one
/// session has been through.
#[derive(Default)]
pub(crate) struct RuntimeGraveyard {
    /// Retired modules, oldest first.
    entries: Vec<RetiredRuntime>,
}

/// One retired runtime module and the staged file it was mapped from.
struct RetiredRuntime {
    /// The mapped module, held so it is never unmapped.
    _module: LoadedRuntimeModule,
    /// Staged file behind the module, retained for diagnostics.
    ///
    /// It is deliberately not deleted here: the operating system keeps a mapped
    /// image locked, and the next host startup clears the whole per-process
    /// temporary directory anyway.
    _staged_path: PathBuf,
}

impl RuntimeGraveyard {
    /// Retire one module, keeping it mapped.
    pub(crate) fn retire(&mut self, module: LoadedRuntimeModule, staged_path: PathBuf) {
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            path = %staged_path.display(),
            "retiring an engine runtime generation; its module stays mapped"
        );
        self.entries.push(RetiredRuntime {
            _module: module,
            _staged_path: staged_path,
        });
    }

    /// Number of retired generations still mapped.
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

// =============================================================================
// RuntimeClient
// =============================================================================

impl RuntimeClient {
    /// Stage a built runtime dylib, map it, and validate its ABI.
    ///
    /// The build output is copied to a generation-numbered staged path first:
    /// a mapped module is locked by the operating system, so loading the build
    /// output directly would block the next compilation.
    ///
    /// No generation exists yet; call [`Self::create`] once the previous one
    /// has been torn down.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError`] when staging fails, the module cannot be
    /// mapped, or its ABI does not match this host.
    pub(crate) fn map(
        built_runtime_path: &Path,
        workspace_root: &Path,
        staging_generation: u64,
    ) -> Result<Self, RuntimeError> {
        // Step 1: Copy the build output into a uniquely named staged file.
        let staged_path =
            stage_runtime_dylib(built_runtime_path, workspace_root, staging_generation)?;

        // Step 2: Map the staged copy and validate its exported table.
        // SAFETY: `staged_path` was just copied from this workspace's own
        // `pill_runtime` build output, which is the provenance the loader
        // requires before it maps a module and calls its accessor.
        let module = match unsafe { LoadedRuntimeModule::load(&staged_path) } {
            Ok(module) => module,
            Err(error) => {
                // Nothing was mapped, so the staged file can be removed now.
                let _ = std::fs::remove_file(&staged_path);
                return Err(RuntimeError::ModuleRejected {
                    details: error.to_string(),
                });
            }
        };

        Ok(Self {
            module,
            staged_path,
            handle: std::ptr::null_mut(),
        })
    }

    /// Map an already-staged dynamic library and validate its ABI.
    ///
    /// Used when another process staged a runtime this host did not build: the
    /// artifact is adopted exactly as produced rather than rebuilt, which is
    /// the whole point of the staging directory being observable. The file is
    /// in this process's own staging directory, so the host takes ownership of
    /// it and deletes it when the generation is finally evicted.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::ModuleRejected`] when the file cannot be mapped
    /// or its ABI does not match this host.
    pub(crate) fn map_staged(staged_path: PathBuf) -> Result<Self, RuntimeError> {
        // SAFETY: `staged_path` lies in this process's own staging directory,
        // whose only writers are this host and a developer tool building the
        // very same `pill_runtime` package from this workspace.
        let module = unsafe { LoadedRuntimeModule::load(&staged_path) }.map_err(|error| {
            RuntimeError::ModuleRejected {
                details: error.to_string(),
            }
        })?;

        Ok(Self {
            module,
            staged_path,
            handle: std::ptr::null_mut(),
        })
    }

    /// Rebuild a client around a module that is already mapped.
    ///
    /// Used by the rollback path: the previous generation's module is still
    /// mapped after its handle was destroyed, so the previous engine can be
    /// brought back without re-reading anything from disk.
    pub(crate) fn from_module(module: LoadedRuntimeModule, staged_path: PathBuf) -> Self {
        Self {
            module,
            staged_path,
            handle: std::ptr::null_mut(),
        }
    }

    /// Ask the mapped module to bring up one generation.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CreateFailed`] when the runtime refuses the
    /// arguments - a contract, layout, or feature mismatch - or when the
    /// project module inside it fails to initialize.
    pub(crate) fn create(&mut self, context: &RuntimeCreateContext) -> Result<(), RuntimeError> {
        // The C# sub-struct borrows the owned buffers in `context`, so it must
        // live no longer than this call.
        let csharp = context.csharp.as_ref().map(CSharpProjectStrings::as_abi);
        let args = PillRuntimeCreateArgsV1 {
            struct_size: std::mem::size_of::<PillRuntimeCreateArgsV1>() as u32,
            abi_version: PILL_RUNTIME_ABI_VERSION,
            features_mask: crate::host_feature_mask(),
            project_backend: context.project_backend,
            window: &context.window,
            width: context.width,
            height: context.height,
            workspace_root_utf8: context.workspace_root.as_ptr(),
            project_module_path_utf8: context
                .project_module_path
                .as_ref()
                .map(|path| path.as_ptr())
                .unwrap_or(std::ptr::null()),
            csharp_project: csharp
                .as_ref()
                .map(|value| value as *const PillCSharpProjectV1)
                .unwrap_or(std::ptr::null()),
            log_sink: context.log_sink,
            metrics_sink: context.metrics_sink,
        };

        let status = (self.module.table().create)(&args, &mut self.handle);
        if status != PILL_OK || self.handle.is_null() {
            return Err(RuntimeError::CreateFailed {
                details: self.last_error(),
            });
        }
        Ok(())
    }

    /// Read the runtime's message for the most recent failing call.
    fn last_error(&self) -> String {
        let message = (self.module.table().last_error_utf8)();
        if message.is_null() {
            return String::from("the runtime reported no diagnostic");
        }
        // SAFETY: The contract guarantees a non-null result is a
        // NUL-terminated buffer owned by the runtime and valid until the next
        // failing call on this thread, which has not happened yet.
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    }

    /// Turn a boundary status code into a typed error.
    fn check(&self, operation: &str, status: i32) -> Result<(), RuntimeError> {
        if status == PILL_OK {
            return Ok(());
        }
        Err(RuntimeError::CallFailed {
            operation: operation.to_string(),
            details: self.last_error(),
        })
    }

    /// Path the mapped module was loaded from.
    pub(crate) fn module_path(&self) -> &Path {
        self.module.path()
    }

    /// Run one frame of the loaded generation.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when presentation fails fatally.
    /// Recoverable per-frame errors are reported by the runtime itself and
    /// never surface here.
    pub(crate) fn run_one_frame(&mut self) -> Result<(), RuntimeError> {
        let status = (self.module.table().run_one_frame)(self.handle);
        self.check("run_one_frame", status)
    }

    /// Take the periodic console report produced by the last frame.
    pub(crate) fn take_frame_report(&mut self) -> Option<FrameReport> {
        let mut report = FrameReport::default();
        let produced = (self.module.table().take_frame_report)(self.handle, &mut report);
        (produced != 0).then_some(report)
    }

    /// Read live frame statistics without consuming the periodic report.
    pub(crate) fn current_frame_report(&self) -> FrameReport {
        let mut report = FrameReport::default();
        let status = (self.module.table().current_frame_report)(self.handle, &mut report);
        if status != PILL_OK {
            // Statistics drive overlays only, so a failure degrades to zeroes
            // rather than interrupting the caller.
            return FrameReport::default();
        }
        report
    }

    /// Forward a physical window resize.
    pub(crate) fn resize(&mut self, width: u32, height: u32) {
        (self.module.table().resize)(self.handle, width, height);
    }

    /// Restrict drawing to a physical region, or restore the full surface.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when the runtime rejects the call.
    pub(crate) fn set_viewport(
        &mut self,
        viewport: Option<RenderViewport>,
    ) -> Result<(), RuntimeError> {
        let status = match viewport {
            Some(viewport) => (self.module.table().set_viewport)(self.handle, &viewport),
            None => (self.module.table().set_viewport)(self.handle, std::ptr::null()),
        };
        self.check("set_viewport", status)
    }

    /// Map a stable project coordinate space into the current viewport.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when the runtime rejects the call.
    pub(crate) fn set_virtual_resolution(
        &mut self,
        resolution: Option<VirtualResolution>,
    ) -> Result<(), RuntimeError> {
        let status = match resolution {
            Some(resolution) => {
                (self.module.table().set_virtual_resolution)(self.handle, &resolution)
            }
            None => (self.module.table().set_virtual_resolution)(self.handle, std::ptr::null()),
        };
        self.check("set_virtual_resolution", status)
    }

    /// Move rendering to another native window.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when the new surface cannot be
    /// created; the runtime keeps the current one in that case.
    pub(crate) fn retarget_render_window(
        &mut self,
        window: &PillWindowHandleV1,
        width: u32,
        height: u32,
    ) -> Result<(), RuntimeError> {
        let status =
            (self.module.table().retarget_render_window)(self.handle, window, width, height);
        self.check("retarget_render_window", status)
    }

    /// Swap in a rebuilt project module, preserving world state.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when the path cannot be passed
    /// across the boundary or the runtime rejects the call. A failed project
    /// generation is handled inside the runtime, which keeps the previous one.
    pub(crate) fn reload_project(
        &mut self,
        module_path: Option<&Path>,
    ) -> Result<(), RuntimeError> {
        let encoded = match module_path {
            Some(path) => Some(path_to_c_string(path, "reload_project")?),
            None => None,
        };
        let status = (self.module.table().reload_project)(
            self.handle,
            encoded
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null()),
        );
        self.check("reload_project", status)
    }

    /// Whether the runtime asked the host to stop the frame loop.
    pub(crate) fn is_exit_requested(&self) -> bool {
        (self.module.table().is_exit_requested)(self.handle) != 0
    }

    /// Capture the world and copy it into host-owned memory.
    ///
    /// The runtime's envelope is released before returning, through the same
    /// table that allocated it, so no host-side buffer ever depends on a
    /// particular module staying mapped.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when the runtime cannot serialize
    /// its world.
    pub(crate) fn capture_world_state(&mut self) -> Result<CapturedWorld, RuntimeError> {
        let mut envelope: *mut CapturedWorldState = std::ptr::null_mut();
        let status = (self.module.table().capture_world_state)(self.handle, &mut envelope);
        self.check("capture_world_state", status)?;
        if envelope.is_null() {
            return Err(RuntimeError::CallFailed {
                operation: String::from("capture_world_state"),
                details: String::from("the runtime reported success but produced no envelope"),
            });
        }

        // SAFETY: The contract guarantees a successful capture yields a live
        // envelope, owned by this module, valid until it is released below.
        let captured = unsafe { read_envelope(self.module.table(), envelope) };
        (self.module.table().release_world_state)(envelope);
        Ok(captured)
    }

    /// Rebuild the world from a host-owned capture.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::CallFailed`] when the runtime cannot decode or
    /// apply the captured document. The runtime keeps its freshly initialized
    /// world in that case, so the caller can continue without state.
    pub(crate) fn restore_world_state(
        &mut self,
        captured: &CapturedWorld,
    ) -> Result<(), RuntimeError> {
        // The envelope is rebuilt on the stack around the host's own buffer,
        // which the contract's `#[repr(C)]` header makes portable across
        // generations. Only the runtime reads it, and only for this call.
        let envelope = CapturedWorldState {
            struct_size: std::mem::size_of::<CapturedWorldState>() as u32,
            format_version: captured.format_version,
            captured_at_nanos: captured.captured_at_nanos,
            payload: captured.payload.as_ptr(),
            payload_len: captured.payload.len() as u64,
            summary_utf8: std::ptr::null(),
        };
        let status = (self.module.table().restore_world_state)(self.handle, &envelope);
        self.check("restore_world_state", status)
    }

    /// Destroy the live generation and return the still-mapped module.
    ///
    /// The module is deliberately *not* unmapped: drop glue reached during
    /// teardown and any envelope this generation allocated still point into
    /// its code. The caller either retires it into the graveyard or rebuilds a
    /// client around it through [`Self::from_module`].
    pub(crate) fn destroy(mut self) -> (LoadedRuntimeModule, PathBuf) {
        if !self.handle.is_null() {
            (self.module.table().destroy)(self.handle);
            self.handle = std::ptr::null_mut();
        }
        (self.module, self.staged_path)
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Copy a captured envelope's contents into host-owned memory.
///
/// # Safety
///
/// `envelope` must point to a live [`CapturedWorldState`] produced by `table`
/// and not yet released.
unsafe fn read_envelope(
    table: &pill_runtime_api::PillRuntimeApiV1,
    envelope: *mut CapturedWorldState,
) -> CapturedWorld {
    // SAFETY: The caller guarantees `envelope` points to a live envelope.
    let header = unsafe { &*envelope };
    // SAFETY: Same as above; the payload stays valid until the envelope is
    // released, which the caller does only after this function returns.
    let payload = unsafe { header.payload_bytes() }.to_vec();

    let mut summary_pointer: *const c_char = std::ptr::null();
    let summary = if (table.describe_state)(envelope, &mut summary_pointer) == PILL_OK
        && !summary_pointer.is_null()
    {
        // SAFETY: A successful `describe_state` yields a NUL-terminated buffer
        // owned by the envelope, valid until it is released.
        unsafe { CStr::from_ptr(summary_pointer) }
            .to_string_lossy()
            .into_owned()
    } else {
        String::from("captured world")
    };

    // `state_byte_len` is the runtime's own view of the payload size; a
    // disagreement means the envelope is malformed, so the borrowed slice
    // wins and the mismatch is reported rather than trusted.
    let reported_len = (table.state_byte_len)(envelope);
    if reported_len != payload.len() as u64 {
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            reported = reported_len,
            copied = payload.len(),
            "captured world envelope reported a payload size that differs from its buffer"
        );
    }

    CapturedWorld {
        format_version: header.format_version,
        captured_at_nanos: header.captured_at_nanos,
        payload,
        summary,
    }
}

/// Copy a built runtime dylib into a uniquely named staged file.
///
/// # Errors
///
/// Returns [`RuntimeError`] when the staging directory cannot be created or
/// the copy fails.
fn stage_runtime_dylib(
    built_runtime_path: &Path,
    workspace_root: &Path,
    staging_generation: u64,
) -> Result<PathBuf, RuntimeError> {
    let directory = runtime_staging_directory(workspace_root);
    std::fs::create_dir_all(&directory).map_err(|source| RuntimeError::StagingDirectory {
        directory: directory.display().to_string(),
        source,
    })?;

    let extension = built_runtime_path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_else(|| native_library_extension());
    let staged_path = directory.join(runtime_staged_file_name(staging_generation, extension));

    std::fs::copy(built_runtime_path, &staged_path).map_err(|source| {
        RuntimeError::StageCopyFailed {
            source_path: built_runtime_path.display().to_string(),
            target_path: staged_path.display().to_string(),
            source,
        }
    })?;
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        path = %staged_path.display(),
        "staged engine runtime dylib"
    );
    Ok(staged_path)
}

/// Encode a path as a NUL-terminated UTF-8 string for the boundary.
///
/// # Errors
///
/// Returns [`RuntimeError::CallFailed`] when the path is not valid UTF-8 or
/// contains an interior NUL, neither of which the contract can represent.
pub(crate) fn path_to_c_string(path: &Path, operation: &str) -> Result<CString, RuntimeError> {
    let text = path.to_str().ok_or_else(|| RuntimeError::CallFailed {
        operation: operation.to_string(),
        details: format!("the path {} is not valid UTF-8", path.display()),
    })?;
    CString::new(text).map_err(|_| RuntimeError::CallFailed {
        operation: operation.to_string(),
        details: format!("the path {} contains an interior NUL byte", path.display()),
    })
}

/// Assemble the reusable creation context for one project configuration.
///
/// Every string is owned here so the borrowed `#[repr(C)]` arguments a
/// `create` call builds always point at buffers that outlive it, including
/// across a reload where the same context is reused.
///
/// The context starts headless. A frontend that owns a window installs it
/// afterwards, which is also what keeps a slow first build from opening an
/// empty surface.
///
/// # Errors
///
/// Returns [`RuntimeError::CallFailed`] when a configured path cannot be
/// represented as a NUL-terminated UTF-8 string.
pub(crate) fn build_create_context(
    workspace_root: &Path,
    config: &ProjectModuleConfig,
    project_module_path: Option<&Path>,
) -> Result<RuntimeCreateContext, RuntimeError> {
    let (project_backend, csharp) = match &config.backend {
        ProjectModuleBackend::NativeLibrary { .. } => {
            (pill_runtime_api::PILL_PROJECT_BACKEND_NATIVE, None)
        }
        ProjectModuleBackend::CSharp(csharp) => {
            let runtime_directory = workspace_root.join(&csharp.runtime_output_subdirectory);
            let project_directory = workspace_root.join(&csharp.project_output_subdirectory);
            (
                pill_runtime_api::PILL_PROJECT_BACKEND_CSHARP,
                Some(CSharpProjectStrings {
                    runtime_assembly_path: path_to_c_string(
                        &runtime_directory.join(format!("{}.dll", csharp.runtime_assembly_name)),
                        "create",
                    )?,
                    runtime_config_path: path_to_c_string(
                        &runtime_directory.join(format!(
                            "{}.runtimeconfig.json",
                            csharp.runtime_assembly_name
                        )),
                        "create",
                    )?,
                    runtime_assembly_name: CString::new(csharp.runtime_assembly_name.as_str())
                        .map_err(|_| RuntimeError::CallFailed {
                            operation: String::from("create"),
                            details: String::from(
                                "the managed runtime assembly name contains an interior NUL byte",
                            ),
                        })?,
                    project_directory: path_to_c_string(&project_directory, "create")?,
                    project_assembly_file_name: CString::new(format!(
                        "{}.dll",
                        csharp.project_assembly_name
                    ))
                    .map_err(|_| RuntimeError::CallFailed {
                        operation: String::from("create"),
                        details: String::from(
                            "the managed project assembly name contains an interior NUL byte",
                        ),
                    })?,
                }),
            )
        }
    };

    Ok(RuntimeCreateContext {
        workspace_root: path_to_c_string(workspace_root, "create")?,
        window: PillWindowHandleV1::none(),
        width: 0,
        height: 0,
        project_backend,
        project_module_path: project_module_path
            .map(|path| path_to_c_string(path, "create"))
            .transpose()?,
        csharp,
        // Both sinks are process-wide host state with no per-generation
        // configuration, so they are resolved here rather than threaded
        // through every caller.
        log_sink: crate::sink::host_log_sink(),
        metrics_sink: crate::sink::host_metrics_sink(),
    })
}

/// Build a captured world from a payload the host already owns.
///
/// Only used by tests, which need a capture that no runtime produced.
#[cfg(test)]
pub(crate) fn captured_world_for_test(payload: Vec<u8>, summary: &str) -> CapturedWorld {
    CapturedWorld {
        format_version: pill_runtime_api::PILL_RUNTIME_STATE_FORMAT_VERSION,
        captured_at_nanos: 0,
        payload,
        summary: summary.to_string(),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Paths become boundary strings only when they are representable.
    #[test]
    fn boundary_paths_reject_interior_nul_bytes() {
        let encoded = path_to_c_string(Path::new("modules/target/debug"), "create")
            .expect("a plain path encodes");
        assert_eq!(encoded.to_str().unwrap(), "modules/target/debug");

        let invalid = path_to_c_string(Path::new("modules/\0/debug"), "create");
        assert!(matches!(invalid, Err(RuntimeError::CallFailed { .. })));
    }

    /// A retired generation is unmapped only once the cap is exceeded.
    #[test]
    fn graveyard_reports_its_retained_generations() {
        let graveyard = RuntimeGraveyard::default();
        assert_eq!(graveyard.len(), 0);
    }

    /// A host-owned capture keeps its payload and summary verbatim.
    #[test]
    fn captured_worlds_retain_their_payload_and_summary() {
        let captured = captured_world_for_test(b"{\"entities\":[]}".to_vec(), "0 entities");
        assert_eq!(captured.byte_len(), 15);
        assert_eq!(captured.summary(), "0 entities");
        assert_eq!(
            captured.format_version,
            pill_runtime_api::PILL_RUNTIME_STATE_FORMAT_VERSION
        );
    }
}
