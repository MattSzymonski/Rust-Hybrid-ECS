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
//! A `dotnet build` of a one-file gameplay edit costs about three seconds cold
//! and one second warm, while the C# compilation inside it costs around
//! thirty-five milliseconds. Everything else is process start, MSBuild
//! evaluation, a restore pass and a walk of the project reference graph - work
//! that recomputes an answer which has not changed since the host started. This
//! module skips all of it: the project's startup build reports the exact `csc`
//! invocation it used, and a reload replays that invocation through Roslyn
//! inside the host process.
//!
//! Replaying a captured command line rather than reconstructing one is what
//! makes the fast path faithful. The references, preprocessor defines, language
//! version, analyzers and source list are MSBuild's own; nothing here decides
//! what a gameplay project compiles against.
//!
//! The capture does go stale in exactly one way that matters: adding or removing
//! a source file changes the list MSBuild computed. That case is detected here
//! and sent back through a full build, which refreshes the capture for the next
//! reload.

// Standard library
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Instant;

// External crates
use pill_core::error::CSharpError;
use pill_core::telemetry::telemetry_target::HOT_RELOAD;
use pill_core::{info, warn};

// Current crate
use super::csharp_runtime::DotnetRuntimeContext;
use super::managed_buffer::fetch_managed_buffer;
use crate::config::{
    CSHARP_COMPILER_ASSEMBLY_NAME, CSHARP_COMPILER_OUTPUT_SUBDIRECTORY,
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
/// diagnostic set reaches it, and it is here so a compiler reporting a corrupt
/// length cannot drive the host into an allocation it dies on. This site had no
/// bound at all before the two-call protocol became shared.
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
    /// Returns `None` whenever the fast path cannot be set up - a missing
    /// compiler project, a build failure, an ABI mismatch. Every such case is
    /// logged once and leaves the host on the ordinary `dotnet build` path, so a
    /// broken fast path costs reload speed and nothing else.
    pub(crate) fn try_new(
        runtime: &DotnetRuntimeContext,
        workspace_root: &Path,
        config: &CSharpModuleConfig,
    ) -> Option<Self> {
        // Step 1: Resolve the two paths this compiler works between. A shipping
        // layout has no project `obj` directory and reports no capture path,
        // which is also the posture that never reloads.
        let response_file = workspace_root.join(config.compiler_arguments_file()?);
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

        // Step 4: Warm the compiler on a background thread. The first compile
        // costs about 1.6 seconds - JITting Roslyn, reading the reference set
        // and loading the analyzers - and every one after it under a tenth of
        // that. Paying it now, while the host is still starting, is what keeps
        // that cost off the developer's first edit.
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
    /// Returns the hosting error when an export is missing, or
    /// [`CSharpError::InteropVersionMismatch`] when the assembly implements a
    /// different contract than this host was built against.
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
    /// the first reload pays what every reload used to.
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
        if !self.response_file.is_file() {
            return FastCompileOutcome::Unavailable {
                reason: format!(
                    "no captured compiler arguments at {}",
                    self.response_file.display()
                ),
            };
        }

        // A source file added or removed since the capture is the one edit whose
        // compiler command line genuinely differs from the captured one, so it
        // has to go through MSBuild - which also refreshes the capture, leaving
        // the reload after it fast again.
        if let Some(reason) =
            capture_missed_a_source(&self.response_file, workspace_root, watch_directory)
        {
            return FastCompileOutcome::Unavailable { reason };
        }

        let (Some(response_file), Some(output_assembly)) = (
            encode_path(&self.response_file),
            encode_path(&self.output_assembly),
        ) else {
            return FastCompileOutcome::Unavailable {
                reason: "a compiler path contains an interior NUL".to_string(),
            };
        };

        let started = Instant::now();
        let status = (self.compile)(
            response_file.as_ptr().cast(),
            output_assembly.as_ptr().cast(),
        );
        let milliseconds = started.elapsed().as_secs_f64() * 1000.0;

        match status {
            COMPILE_STATUS_COMPILED => FastCompileOutcome::Compiled { milliseconds },
            COMPILE_STATUS_FAILED => FastCompileOutcome::Failed {
                diagnostics: self.diagnostics(),
            },
            // Anything else is the compiler reporting that IT could not run,
            // which is a fallback rather than a fault in the gameplay source.
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
        return Some(format!("cannot read the source directory {watch_directory}"));
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
