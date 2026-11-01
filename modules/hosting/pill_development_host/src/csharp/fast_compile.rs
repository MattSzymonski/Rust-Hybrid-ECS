//! In-process Roslyn compilation of the C# project, for hot reload.
//!
//! # Responsibilities
//!
//! - Build and load the managed compiler assembly beside the running runtime.
//! - Replay the compiler command line the startup build captured.
//! - Decide when the capture is too stale to replay and a full build is needed.
//!
//! # Design
//!
//! A `dotnet build` of a one-file gameplay edit costs seconds, while the C#
//! compilation inside it costs tens of milliseconds. The rest is process start,
//! MSBuild evaluation, a restore pass and a walk of the project reference graph,
//! recomputing an answer that has not changed since the host started. This
//! module skips all of it: the project's startup build reports the exact `csc`
//! invocation it used, and a reload replays that invocation through Roslyn
//! inside the host process. Replaying the capture is what makes the fast path
//! faithful - references, defines, language version, analyzers and source list
//! are MSBuild's own.
//!
//! The capture is checked against the project's source set before it is
//! replayed: a file added or removed under the watched source directory changes
//! the list MSBuild computed, so that case goes back through a full build, which
//! refreshes the capture for the next reload.

// Standard library
use std::ffi::CString;
use std::path::{Path, PathBuf};

// External crates
use pill_core::error::CSharpError;
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target::HOT_RELOAD;
use pill_core::{info, warn};
use pill_csharp_bridge::{fetch_managed_buffer, DotnetRuntimeContext};

// Current crate
use crate::config::{
    CSHARP_COMPILER_ARGUMENTS_FILE, CSHARP_COMPILER_ASSEMBLY_NAME,
    CSHARP_COMPILER_OUTPUT_SUBDIRECTORY,
};
use crate::CSharpModuleConfig;

// =============================================================================
// Constants
// =============================================================================

/// Export-contract version this host requires of the compiler assembly.
///
/// Checked before any other export is used, for the same reason the managed
/// runtime's interop version is: a compiler assembly left from an older checkout
/// would otherwise be called through signatures it no longer has.
const COMPILER_ABI_VERSION: u32 = 1;

/// Managed type holding the compiler's unmanaged exports.
const COMPILER_INTEROP_TYPE: &str = "TracyLive.Compiler.CompilerInterop, pill_csharp_compiler";

/// `CompilerInterop.Compile` returned: the assembly was written.
const COMPILE_STATUS_COMPILED: i32 = 0;

/// `CompilerInterop.Compile` returned: the project has errors.
const COMPILE_STATUS_FAILED: i32 = 1;

/// Upper bound for one compile's diagnostics text.
///
/// A failing build reports kilobytes; the cap is generous enough that no real
/// diagnostic set reaches it, and it bounds what a corrupt length can make the
/// host reserve, since the two-call fetch refuses a length above it before
/// allocating.
const MAX_DIAGNOSTICS_BYTES: u32 = 4 * 1024 * 1024;

// =============================================================================
// Managed Export Signatures
// =============================================================================

/// `uint CompilerAbiVersion()`.
type CompilerAbiVersionFn = extern "system" fn() -> u32;

/// `byte Warmup(byte* responseFilePath)`.
type WarmupFn = extern "system" fn(*const u8) -> u8;

/// `int Compile(byte* responseFilePath, byte* outputAssemblyPath)`.
type CompileFn = extern "system" fn(*const u8, *const u8) -> i32;

/// `uint DiagnosticsLength()`.
type DiagnosticsLengthFn = extern "system" fn() -> u32;

/// `byte CopyDiagnostics(byte* output, uint capacity)`.
type CopyDiagnosticsFn = extern "system" fn(*mut u8, u32) -> u8;

// =============================================================================
// FastCompileOutcome
// =============================================================================

/// What one in-process compile attempt produced.
pub(crate) enum FastCompileOutcome {
    /// The project assembly was recompiled and written.
    Compiled {
        /// Wall-clock cost of the compile, for the reload log.
        milliseconds: f64,
    },
    /// The project does not compile; the diagnostics say why.
    Failed {
        /// Compiler and analyzer errors, already formatted the way csc prints them.
        diagnostics: String,
    },
    /// The fast path cannot answer this reload; the caller must build normally.
    Unavailable {
        /// Why, so the fallback is never silent.
        reason: String,
    },
}

// =============================================================================
// FastCompiler
// =============================================================================

/// The loaded managed compiler, bound to one project's captured command line.
pub(crate) struct FastCompiler {
    /// `Compile` export.
    compile: CompileFn,
    /// `DiagnosticsLength` export.
    diagnostics_length: DiagnosticsLengthFn,
    /// `CopyDiagnostics` export.
    copy_diagnostics: CopyDiagnosticsFn,
    /// Absolute path of the captured compiler command line.
    response_file: PathBuf,
    /// Absolute path the recompiled project assembly is written to.
    output_assembly: PathBuf,
}

impl FastCompiler {
    /// Build the compiler assembly, load it, and start its warmup.
    ///
    /// Returns `None` whenever the fast path cannot be set up: no captured
    /// command line to replay (a project shape that never hot reloads), a
    /// compiler build failure or an ABI mismatch. The build and load failures
    /// are logged once; either way the host stays on the ordinary `dotnet build`
    /// path, so a broken fast path costs reload speed and nothing else.
    pub(crate) fn try_new(
        runtime: &DotnetRuntimeContext,
        workspace_root: &Path,
        config: &CSharpModuleConfig,
    ) -> Option<Self> {
        // Step 1: Resolve the two paths this compiler works between. A shipping
        // layout has no project `obj` directory and reports no capture path,
        // which is also the posture that never reloads.
        let response_file = workspace_root.join(compiler_arguments_file(config)?);
        let output_assembly = workspace_root
            .join(&config.project_output_subdirectory)
            .join(format!("{}.dll", config.project_assembly_name));

        // Step 2: Build the compiler if its sources changed, then load it.
        // Built here rather than by the project's own build because nothing
        // references it: keeping Roslyn out of every managed project's reference
        // graph is what keeps it out of the shipping bundle.
        if let Err(error) = crate::build_runner::build_csharp_compiler(workspace_root) {
            warn!(
                target: HOT_RELOAD,
                error = %error,
                "could not build the in-process C# compiler; hot reload will run a full build each time"
            );
            return None;
        }
        let assembly = workspace_root
            .join(CSHARP_COMPILER_OUTPUT_SUBDIRECTORY)
            .join(format!("{CSHARP_COMPILER_ASSEMBLY_NAME}.dll"));

        // Step 3: Validate the export contract before resolving anything else.
        let compiler = match Self::resolve(runtime, &assembly, response_file, output_assembly) {
            Ok(compiler) => compiler,
            Err(error) => {
                warn!(
                    target: HOT_RELOAD,
                    error = %error,
                    "could not load the in-process C# compiler; hot reload will run a full build each time"
                );
                return None;
            }
        };

        // Step 4: Warm the compiler. `Warmup` queues the work and returns, so
        // Roslyn's JIT, the reference set and the analyzers are paid for in the
        // background while the host is still starting, which keeps that cost off
        // the developer's first edit.
        compiler.start_warmup(runtime, &assembly);
        info!(
            target: HOT_RELOAD,
            "in-process C# compiler ready; reloads will skip MSBuild"
        );
        Some(compiler)
    }

    /// Resolve every export and check the contract version.
    ///
    /// # Errors
    ///
    /// Returns the export-resolution errors of
    /// [`DotnetRuntimeContext::get_unmanaged_fn`] when the assembly lacks one of
    /// the exports above, and [`CSharpError::InteropVersionMismatch`] when the
    /// assembly implements a different contract than this host was built
    /// against.
    fn resolve(
        runtime: &DotnetRuntimeContext,
        assembly: &Path,
        response_file: PathBuf,
        output_assembly: PathBuf,
    ) -> Result<Self, CSharpError> {
        let abi_version = runtime.get_unmanaged_fn::<CompilerAbiVersionFn>(
            assembly,
            COMPILER_INTEROP_TYPE,
            "CompilerAbiVersion",
        )?;
        if abi_version() != COMPILER_ABI_VERSION {
            return Err(CSharpError::InteropVersionMismatch {
                expected: COMPILER_ABI_VERSION,
                actual: abi_version(),
            });
        }
        Ok(Self {
            compile: runtime.get_unmanaged_fn::<CompileFn>(
                assembly,
                COMPILER_INTEROP_TYPE,
                "Compile",
            )?,
            diagnostics_length: runtime.get_unmanaged_fn::<DiagnosticsLengthFn>(
                assembly,
                COMPILER_INTEROP_TYPE,
                "DiagnosticsLength",
            )?,
            copy_diagnostics: runtime.get_unmanaged_fn::<CopyDiagnosticsFn>(
                assembly,
                COMPILER_INTEROP_TYPE,
                "CopyDiagnostics",
            )?,
            response_file,
            output_assembly,
        })
    }

    /// Ask the compiler to populate its caches on a background thread.
    ///
    /// Best effort: a warmup that cannot be resolved or cannot start only means
    /// the first reload pays the full Roslyn startup cost.
    fn start_warmup(&self, runtime: &DotnetRuntimeContext, assembly: &Path) {
        let Ok(warmup) =
            runtime.get_unmanaged_fn::<WarmupFn>(assembly, COMPILER_INTEROP_TYPE, "Warmup")
        else {
            return;
        };
        let Some(response_file) = encode_path(&self.response_file) else {
            return;
        };
        warmup(response_file.as_ptr().cast());
    }

    /// Recompile the project in-process, or say why that was not possible.
    ///
    /// `watch_directory` is the project's source directory, workspace-relative;
    /// it is what the captured source list is checked against.
    pub(crate) fn compile(
        &self,
        workspace_root: &Path,
        watch_directory: &str,
    ) -> FastCompileOutcome {
        // Step 1: Refuse a capture that cannot be replayed as it stands. A file
        // added or removed under the watched directory changes the source list
        // MSBuild computed, so it has to go through MSBuild - which also
        // refreshes the capture, leaving the reload after it fast again.
        if !self.response_file.is_file() {
            return FastCompileOutcome::Unavailable {
                reason: format!(
                    "no captured compiler arguments at {}",
                    self.response_file.display()
                ),
            };
        }
        if let Some(reason) =
            capture_missed_a_source(&self.response_file, workspace_root, watch_directory)
        {
            return FastCompileOutcome::Unavailable { reason };
        }

        // Step 2: Encode both paths for the managed entry point, which reads
        // NUL-terminated UTF-8.
        let (Some(response_file), Some(output_assembly)) = (
            encode_path(&self.response_file),
            encode_path(&self.output_assembly),
        ) else {
            return FastCompileOutcome::Unavailable {
                reason: "a compiler path contains an interior NUL".to_string(),
            };
        };

        // Step 3: Run the compile and time it for the reload log.
        let started = Instant::now();
        let status = (self.compile)(
            response_file.as_ptr().cast(),
            output_assembly.as_ptr().cast(),
        );
        let milliseconds = started.elapsed().as_secs_f64() * 1000.0;

        // Step 4: Map the status onto an outcome. Two is the managed side's code
        // for "the fast path could not run", so any status other than
        // `COMPILE_STATUS_COMPILED` or `COMPILE_STATUS_FAILED` is a fallback
        // rather than a fault in the gameplay source.
        match status {
            COMPILE_STATUS_COMPILED => FastCompileOutcome::Compiled { milliseconds },
            COMPILE_STATUS_FAILED => FastCompileOutcome::Failed {
                diagnostics: self.diagnostics(),
            },
            _ => FastCompileOutcome::Unavailable {
                reason: self.diagnostics(),
            },
        }
    }

    /// Read back the last compile's diagnostics as UTF-8.
    ///
    /// A compile with nothing to say reports no diagnostics at all, which is an
    /// empty string rather than a failure; anything else that goes wrong is
    /// reported as text, because the caller is already on a failure path and
    /// has nowhere better to put an error.
    fn diagnostics(&self) -> String {
        if (self.diagnostics_length)() == 0 {
            return String::new();
        }
        match fetch_managed_buffer(
            || (self.diagnostics_length)(),
            |pointer, length| (self.copy_diagnostics)(pointer, length),
            MAX_DIAGNOSTICS_BYTES,
        ) {
            Ok(buffer) => String::from_utf8_lossy(&buffer).into_owned(),
            Err(_) => "the compiler reported diagnostics that could not be read".to_string(),
        }
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Workspace-relative path of `config`'s captured compiler command line.
///
/// The project's build writes it into the project's own `obj` directory, so
/// it is derived from the output directory rather than stored: that
/// directory is `<project>/bin/<configuration>/<framework>` by
/// construction, and the directory three levels above it is the project root.
///
/// Returns `None` when the output directory has no such shape, which is the
/// shipping posture - a bundle keeps the assembly flat beside the
/// executable. That posture never hot reloads, so it never needs a capture.
fn compiler_arguments_file(config: &CSharpModuleConfig) -> Option<PathBuf> {
    let output_directory = Path::new(&config.project_output_subdirectory);
    let project_root = output_directory.ancestors().nth(3)?;
    if project_root.as_os_str().is_empty() {
        return None;
    }
    Some(
        project_root
            .join("obj")
            .join(CSHARP_COMPILER_ARGUMENTS_FILE),
    )
}

/// Encode a path as NUL-terminated UTF-8 for the managed entry points.
///
/// Returns `None` for a path containing an interior NUL, which cannot cross the
/// boundary at all.
fn encode_path(path: &Path) -> Option<CString> {
    CString::new(path.to_string_lossy().into_owned()).ok()
}

/// Report whether the captured source list still describes the project.
///
/// Returns `None` when the two agree, or the reason they do not. Only files
/// under the watched source directory are compared: the capture also lists
/// generated mirrors and SDK-generated files from elsewhere, and those change
/// only as part of work that rebuilds the project anyway.
fn capture_missed_a_source(
    response_file: &Path,
    workspace_root: &Path,
    watch_directory: &str,
) -> Option<String> {
    let Ok(watch_root) = std::fs::canonicalize(workspace_root.join(watch_directory)) else {
        // No source directory to compare against: leave the decision to the
        // build, which will report the real problem.
        return Some(format!(
            "cannot read the source directory {watch_directory}"
        ));
    };
    // MSBuild wrote the capture into the project's `obj` directory and resolved
    // relative source paths against the project root, which is its parent.
    let project_root = response_file.parent()?.parent()?;

    let Ok(contents) = std::fs::read_to_string(response_file) else {
        return Some("the captured compiler arguments could not be read".to_string());
    };
    let mut captured: Vec<PathBuf> = Vec::new();
    for line in contents.lines() {
        let line = line.trim().trim_matches('"');
        // Everything that is not a switch is a source file path.
        if line.is_empty() || line.starts_with('/') || line.starts_with('-') {
            continue;
        }
        let Ok(resolved) = std::fs::canonicalize(project_root.join(line)) else {
            continue;
        };
        if resolved.starts_with(&watch_root) {
            captured.push(resolved);
        }
    }

    let mut present = Vec::new();
    collect_csharp_sources(&watch_root, &mut present);
    captured.sort();
    present.sort();
    if captured == present {
        return None;
    }
    Some(format!(
        "the project's source file set changed ({} captured, {} on disk)",
        captured.len(),
        present.len()
    ))
}

/// Collect every `.cs` file under one directory, recursively.
fn collect_csharp_sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_csharp_sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "cs") {
            if let Ok(resolved) = std::fs::canonicalize(&path) {
                found.push(resolved);
            }
        }
    }
}
