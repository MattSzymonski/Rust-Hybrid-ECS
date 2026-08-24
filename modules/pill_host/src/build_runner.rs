//! Project-module build execution and output-path resolution.
//!
//! Build processes inherit the host's standard streams so compiler progress
//! and diagnostics remain visible in the terminal that launched the host.
//!
//! # Responsibilities
//!
//! - Execute backend-specific build commands from the workspace root.
//! - Resolve each backend's expected output artifact path.
//! - Validate that build artifacts exist before loading is attempted.
//!
//! # Design
//!
//! [`build_project_module`] is the host's single entry point for compiling a
//! project module. It treats the build as an opaque process: it never inspects
//! compiler output, and instead decides success from the child's exit status
//! plus a backend-specific output-path resolution step. Cancellation is
//! cooperative: the caller advances a generation counter on newer source
//! saves, and the watchdog loop aborts the build when it observes the change.

// Standard library
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// External crates
use pill_core::error::BuildError;
use pill_core::info;
#[cfg(feature = "hot_patch")]
use pill_core::debug;

// Current crate
use crate::analytics::{self, BuildStatus, ModuleKind};
use crate::{OptionalModuleConfig, ProjectModuleBackend, ProjectModuleConfig};

// =============================================================================
// Constants
// =============================================================================

/// Maximum wall-clock time a single build command may run.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the build watchdog checks for completion and cancellation.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Marker file recording the toolchain and host feature set that produced the
/// current module artifacts.
///
/// Lives directly under the temporary directory rather than inside a
/// per-process subdirectory, so the startup cleanup leaves it in place and
/// every host run against this workspace shares the same record.
const BUILD_INFO_MARKER: &str = "pill_standalone_temp/build_info.txt";

/// Subdirectory, relative to the workspace root, where cargo writes a freshly
/// compiled module `cdylib`.
///
/// The host never loads from this shared per-crate slot: the project's build
/// can overwrite it with the module's export-stripped dependency variant (the
/// "one crate name, two feature sets" collision). The standalone artifact is
/// staged into the module's private hot-load directory instead.
const CARGO_MODULE_OUTPUT_SUBDIRECTORY: &str = "target/debug";

/// Private directory the host stages the project's loadable artifacts into,
/// and the default an optional module's configuration also names.
///
/// The same protection optional modules have always had, extended to the
/// project. Cargo writes `project.dll` to a shared per-crate slot that any
/// other `cargo build` of the same package overwrites - with a different
/// feature set, and therefore a differently configured `pill_engine` compiled
/// into it. Loading that produces an access violation inside `LoadLibrary`,
/// before any of the host's own diagnostics can run.
pub(crate) const PROJECT_HOT_OUTPUT_SUBDIRECTORY: &str = "target/hot";

/// Directory holding one stamp per module, recording what the host built.
const ARTIFACT_STAMP_DIRECTORY: &str = "pill_standalone_temp/artifact_stamps";

/// Host feature set that module builds must mirror.
///
/// Modules are compiled with the same engine features as the host so type
/// layouts stay identical across the DLL boundary; a host rebuilt with a
/// different feature set therefore invalidates every cached module artifact.
///
/// `hot_patch` belongs here for exactly the same reason `rendering` does: the
/// host mirrors it into every module and project build, so it changes the
/// engine's crate metadata on both sides of the boundary. Leaving it out let a
/// host rebuilt with the feature go on trusting artifacts built without it.
const HOST_MODULE_FEATURE_SET: &str = match (cfg!(feature = "rendering"), cfg!(feature = "hot_patch"))
{
    (true, true) => "rendering+hot_patch",
    (true, false) => "rendering",
    (false, true) => "no-rendering+hot_patch",
    (false, false) => "no-rendering",
};

// =============================================================================
// Up-to-Date Build Detection
// =============================================================================

/// The toolchain version line and host feature set of the running host.
///
/// Resolved once per process because spawning `rustc` on every check would
/// dwarf the few milliseconds the check itself saves.
fn current_build_info() -> String {
    static CURRENT_BUILD_INFO: OnceLock<String> = OnceLock::new();
    CURRENT_BUILD_INFO
        .get_or_init(|| {
            // `rustc -vV` prints one version line, for example
            // `rustc 1.95.0 (59807616e 2026-04-14)`, followed by the
            // configuration table. Cargo embeds the same version into every
            // crate's metadata hash, so the first line is enough to detect a
            // toolchain change that would break symbol resolution at load.
            let rustc_version = Command::new("rustc")
                .arg("-vV")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|stdout| stdout.lines().next().map(str::to_string))
                .unwrap_or_default();
            format!("{rustc_version}\n{HOST_MODULE_FEATURE_SET}")
        })
        .clone()
}

/// Read the build-info record written by the last real module build.
fn recorded_build_info(workspace_root: &Path) -> Option<String> {
    std::fs::read_to_string(workspace_root.join(BUILD_INFO_MARKER)).ok()
}

/// Record the toolchain and host feature set that produced the current
/// artifacts.
///
/// Best-effort by design: a failed write simply leaves the marker missing,
/// which makes the next run fall back to a real build.
fn record_build_info(workspace_root: &Path) {
    let marker_path = workspace_root.join(BUILD_INFO_MARKER);
    if let Some(parent) = marker_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker_path, current_build_info());
}

/// Path of the stamp recording what the host built for one module.
fn artifact_stamp_path(workspace_root: &Path, module_name: &str) -> PathBuf {
    workspace_root
        .join(ARTIFACT_STAMP_DIRECTORY)
        .join(format!("{module_name}.txt"))
}

/// Describe the artifacts a build produced, together with the command that
/// produced them.
///
/// Returns `None` when any artifact is missing or unreadable, which the callers
/// treat as "not host-built" and therefore as a reason to run cargo.
fn artifact_stamp(build_command: &[String], artifacts: &[PathBuf]) -> Option<String> {
    // The separator cannot appear in a command argument, so two different
    // argument lists can never produce the same line.
    let mut lines = vec![build_command.join("\u{1}")];
    for path in artifacts {
        let metadata = std::fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        lines.push(format!(
            "{}|{}|{}",
            path.display(),
            metadata.len(),
            modified.as_nanos()
        ));
    }
    Some(lines.join("\n"))
}

/// Record that the host itself produced these artifacts with this command.
///
/// Best-effort by design, like the toolchain marker: a failed write leaves the
/// stamp missing, which makes the next run rebuild rather than trust an
/// artifact it cannot identify.
fn record_artifact_stamp(
    workspace_root: &Path,
    module_name: &str,
    build_command: &[String],
    artifacts: &[PathBuf],
) {
    let Some(stamp) = artifact_stamp(build_command, artifacts) else {
        return;
    };
    let path = artifact_stamp_path(workspace_root, module_name);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, stamp);
}

/// Whether the artifacts on disk are the ones this host built with this command.
///
/// The modification-time check alone cannot tell a host-built artifact from one
/// an unrelated `cargo build` wrote to the same path moments later: both are
/// newer than every source file, so both look up to date. Recording the build
/// command alongside each artifact's size and modification time closes that
/// gap. Anything the host did not write itself - a different feature set, a
/// plain `cargo build`, a `cargo build --workspace` - no longer matches, and
/// falls through to a real build instead of being loaded.
fn artifacts_are_host_built(
    workspace_root: &Path,
    module_name: &str,
    build_command: &[String],
    artifacts: &[PathBuf],
) -> bool {
    let Some(current) = artifact_stamp(build_command, artifacts) else {
        return false;
    };
    let recorded = std::fs::read_to_string(artifact_stamp_path(workspace_root, module_name));
    recorded.is_ok_and(|recorded| recorded == current)
}

/// Whether a module's build artifact is already newer than every input a
/// rebuild could depend on, letting the host skip cargo entirely.
///
/// The host still runs the build whenever this check is uncertain: a missing
/// artifact, an unknown toolchain, a changed host feature set, or any input
/// file newer than the artifact all fall through to a real cargo invocation.
/// Path dependencies are followed recursively, so editing an engine crate or
/// an external path crate invalidates every module that links it, not just the
/// module's own sources.
fn is_build_up_to_date(
    workspace_root: &Path,
    output_path: &Path,
    module_manifest: &Path,
    watch_directory: &str,
) -> bool {
    // Step 1: A missing artifact, or a missing source directory to compare
    // against, can never be up to date.
    let Ok(artifact_metadata) = std::fs::metadata(output_path) else {
        return false;
    };
    let Ok(artifact_mtime) = artifact_metadata.modified() else {
        return false;
    };
    if !workspace_root.join(watch_directory).is_dir() {
        return false;
    }

    // Step 2: The artifact must have been produced by the current toolchain
    // and host feature set. A toolchain change alters crate-metadata hashes,
    // so a module built by an older rustc can no longer resolve the engine's
    // symbols at load time, and a feature change shifts type layouts.
    if recorded_build_info(workspace_root).as_deref() != Some(current_build_info().as_str()) {
        return false;
    }

    // Step 3: Every input file must be strictly older than the artifact.
    // Equal timestamps count as stale so a same-instant write is rebuilt.
    for input in collect_build_inputs(workspace_root, module_manifest, watch_directory) {
        let Ok(input_metadata) = std::fs::metadata(&input) else {
            return false;
        };
        let Ok(input_mtime) = input_metadata.modified() else {
            return false;
        };
        if input_mtime >= artifact_mtime {
            return false;
        }
    }
    true
}

/// Collect every file a module rebuild could depend on: the module's own
/// watched sources, the workspace-level manifests and lockfile, and the source
/// trees of every path dependency, resolved recursively.
fn collect_build_inputs(
    workspace_root: &Path,
    module_manifest: &Path,
    watch_directory: &str,
) -> Vec<PathBuf> {
    let mut inputs = Vec::new();
    let mut visited_manifests = HashSet::new();

    // Step 1: The module's own source tree, watched in place.
    collect_tree_files(&workspace_root.join(watch_directory), &mut inputs);

    // Step 2: Workspace-wide inputs shared by every member build.
    inputs.push(workspace_root.join("Cargo.toml"));
    inputs.push(workspace_root.join("Cargo.lock"));
    inputs.push(workspace_root.join(".cargo").join("config.toml"));

    // Step 3: Path dependencies, followed recursively from the module manifest.
    collect_manifest_dependencies(
        workspace_root,
        module_manifest,
        &mut inputs,
        &mut visited_manifests,
    );

    inputs
}

/// Walk a directory tree, collecting every file.
///
/// Build-output directories are skipped so the walk never descends into a
/// crate's `target/` directory.
fn collect_tree_files(directory: &Path, inputs: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if entry.file_name() != "target" {
                collect_tree_files(&path, inputs);
            }
        } else if file_type.is_file() {
            inputs.push(path);
        }
    }
}

/// Recursively collect the source trees of every path dependency reachable
/// from `manifest`, guarding against cycles with a visited set keyed on
/// canonical manifest paths.
fn collect_manifest_dependencies(
    workspace_root: &Path,
    manifest: &Path,
    inputs: &mut Vec<PathBuf>,
    visited_manifests: &mut HashSet<PathBuf>,
) {
    // The manifest itself is an input to the build, and is canonicalized so
    // the same crate reached through different spellings is only processed once.
    let canonical_manifest = manifest
        .canonicalize()
        .unwrap_or_else(|_| manifest.to_path_buf());
    if !visited_manifests.insert(canonical_manifest) {
        return;
    }
    inputs.push(manifest.to_path_buf());

    // Parse every `path = "..."` value. This lightweight scan matches the
    // project-member materialization and needs no TOML dependency; every
    // occurrence is resolved against the manifest directory, which covers
    // `[dependencies]`, `[patch]`, and a `[lib]` `path` alike.
    let Ok(content) = std::fs::read_to_string(manifest) else {
        return;
    };
    let manifest_directory = manifest.parent().unwrap_or(workspace_root);
    let mut cursor = 0usize;
    while let Some(relative_start) = content[cursor..].find("path = \"") {
        let value_start = cursor + relative_start + "path = \"".len();
        let Some(relative_end) = content[value_start..].find('"') else {
            break;
        };
        let relative_end = value_start + relative_end;
        let dependency_path = Path::new(&content[value_start..relative_end]);
        let dependency_path = if dependency_path.is_absolute() {
            dependency_path.to_path_buf()
        } else {
            manifest_directory.join(dependency_path)
        };

        // A `path` pointing at a file (a `[lib]` source) is an input as-is; a
        // directory is a dependency crate, so its tree is walked and its
        // manifest is parsed for further path dependencies.
        if dependency_path.is_dir() {
            let dependency_manifest = dependency_path.join("Cargo.toml");
            let canonical_dependency = dependency_manifest
                .canonicalize()
                .unwrap_or_else(|_| dependency_manifest.clone());
            // Avoid a second walk of a dependency reached through several
            // parents; the recursion below performs the insert for its own
            // cycle guard.
            if !visited_manifests.contains(&canonical_dependency) {
                collect_tree_files(&dependency_path, inputs);
            }
            collect_manifest_dependencies(
                workspace_root,
                &dependency_manifest,
                inputs,
                visited_manifests,
            );
        } else {
            inputs.push(dependency_path);
        }
        cursor = relative_end;
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Run one module's build command to completion.
///
/// Shared by the project module and by optional modules so both use the same
/// process handling, watchdog, cancellation, and failure reporting. Resolving
/// and validating the produced artifact is left to the caller, because each
/// module kind names and locates its output differently.
///
/// # Errors
///
/// Returns an error if the command is empty, fails to spawn, exits with a
/// non-zero status, times out, or is cancelled by a newer source change.
pub(crate) fn run_build_command(
    workspace_root: &Path,
    name: &str,
    build_command: &[String],
    build_environment: &[(String, String)],
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<(), BuildError> {
    // Step 1: Split the configured command into its executable and arguments.
    //
    // Module configuration stores commands as owned strings so callers can
    // define both Cargo and dotnet builds without shell-specific quoting. The
    // first item is always the executable; every remaining item is passed verbatim.
    let (program, arguments) = build_command
        .split_first()
        .ok_or(BuildError::EmptyCommand)?;

    // Step 2: Spawn the child process from the workspace root.
    //
    // Run from the workspace root because configured paths and Cargo package
    // selection are workspace-relative. The child inherits the host's stdout
    // and stderr instead of capturing them, which keeps compiler progress,
    // warnings, and errors visible during startup and hot reload. Configured
    // environment overrides are applied last so they win over anything the
    // host itself inherited.
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .envs(build_environment.iter().map(|(key, value)| (key, value)))
        .spawn()
        .map_err(|source| BuildError::SpawnFailed {
            name: name.to_string(),
            source,
        })?;

    // Step 3: Poll for completion, cancellation, or timeout under a watchdog.
    //
    // The host frame loop must never block indefinitely on a hung compiler or
    // an interactive prompt, so the build is polled with a deadline and a
    // cancellation signal driven by newer source saves. The build's wall time
    // and the cargo child's peak working set are sampled for the analytics
    // report.
    let build_started = Instant::now();
    let mut cargo_peak_bytes: u64 = 0;
    let deadline = Instant::now() + BUILD_TIMEOUT;
    let status = loop {
        // A newer save during the build advances the generation beyond the
        // baseline captured when the reload started, which cancels this
        // attempt. The caller keeps the old module and the next frame
        // rebuilds with the newer sources.
        if cancel_flag
            .is_some_and(|(generation, baseline)| generation.load(Ordering::Acquire) != baseline)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BuildError::Cancelled);
        }
        if let Some(status) = child.try_wait().map_err(|source| BuildError::WaitFailed {
            name: name.to_string(),
            source,
        })? {
            break status;
        }
        // `PeakWorkingSetSize` is monotonic, so the latest sample is the peak.
        if let Some((_, peak)) = analytics::process_memory(Some(child.id())) {
            cargo_peak_bytes = cargo_peak_bytes.max(peak);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(BuildError::TimedOut {
                name: name.to_string(),
                seconds: BUILD_TIMEOUT.as_secs(),
            });
        }
        std::thread::sleep(WATCHDOG_POLL_INTERVAL);
    };

    // Step 4: Reject a non-zero exit status.
    //
    // A failed compiler must stop the load transaction. During hot reload the
    // caller handles this error by leaving the current project module untouched.
    if !status.success() {
        return Err(BuildError::CommandFailed {
            name: name.to_string(),
            status,
        });
    }
    analytics::record_build_command(
        name,
        build_started.elapsed().as_millis() as u64,
        cargo_peak_bytes,
    );
    // Separate this build's compiler output from whatever the host prints
    // next (load/init logs, the next module's build, or the analytics block),
    // so each compilation in a cascade is visually distinct.
    println!();
    Ok(())
}

/// Build the selected project module and return its expected output artifact.
///
/// # Errors
///
/// Returns an error if the build fails for any of the reasons reported by
/// [`run_build_command`], or if the resolved output artifact does not exist at
/// the configured path.
pub(crate) fn build_project_module(
    workspace_root: &Path,
    config: &ProjectModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = config.name.as_str(),
        "building project module"
    );

    // Step 1: Resolve the backend-specific artifact paths.
    //
    // The build command itself is backend-agnostic, but each backend names and
    // locates its loadable artifact differently. Native outputs use platform
    // naming conventions; managed outputs always use an assembly `.dll`.
    //
    // A native project has two: the slot cargo writes into, and the private
    // copy the host loads from. They are kept apart for the reason optional
    // modules already keep them apart - any other `cargo build` of the same
    // package overwrites cargo's slot, and for the project that means a DLL
    // carrying a differently configured `pill_engine`, which access-violates
    // inside `LoadLibrary`. The managed backend has no such collision and
    // loads from where its build wrote.
    let (build_output, output_path) = match &config.backend {
        ProjectModuleBackend::NativeLibrary {
            library_name,
            output_subdirectory,
        } => (
            workspace_root
                .join(output_subdirectory)
                .join(native_library_filename(library_name)),
            workspace_root
                .join(PROJECT_HOT_OUTPUT_SUBDIRECTORY)
                .join(native_library_filename(library_name)),
        ),
        ProjectModuleBackend::CSharp(config) => {
            let path = workspace_root
                .join(&config.project_output_subdirectory)
                .join(format!("{}.dll", config.project_assembly_name));
            (path.clone(), path)
        }
    };

    // The crate's `rlib`, which a generated patch links to reach the project's
    // types. Staged alongside the library so a patch cannot link a copy some
    // other build replaced, which would compile it against a differently
    // configured engine and give every type a different `TypeId`.
    #[cfg(feature = "hot_patch")]
    let (rlib_build_output, rlib_output) = (
        workspace_root
            .join("target")
            .join("debug")
            .join(format!("lib{}.rlib", config.name)),
        workspace_root
            .join(PROJECT_HOT_OUTPUT_SUBDIRECTORY)
            .join(format!("lib{}.rlib", config.name)),
    );

    // Step 2: Skip cargo entirely for a native module whose artifact is
    // already newer than every build input. The project's own manifest is the
    // effective source of truth: when it changes, the materialized workspace
    // member is regenerated and cargo rebuilds, so its modification time is a
    // reliable staleness signal. Any uncertainty in the check falls through to
    // a real build below.
    if matches!(&config.backend, ProjectModuleBackend::NativeLibrary { .. }) {
        if let Some(manifest_path) = &config.manifest_path {
            let manifest_path = workspace_root.join(manifest_path);

            // Every artifact this build must have produced. Hot patching adds
            // the crate's `rlib`: the staleness check only knows about the
            // library, so without naming the rlib here an existing, up-to-date
            // DLL makes cargo be skipped forever and the rlib is never produced
            // - leaving hot patching permanently idle with nothing obviously
            // wrong.
            #[cfg(feature = "hot_patch")]
            let required_artifacts = vec![output_path.clone(), rlib_output.clone()];
            #[cfg(not(feature = "hot_patch"))]
            let required_artifacts = vec![output_path.clone()];

            if required_artifacts.iter().all(|path| path.is_file())
                && artifacts_are_host_built(
                    workspace_root,
                    &config.name,
                    &config.build_command,
                    &required_artifacts,
                )
                && is_build_up_to_date(
                    workspace_root,
                    &output_path,
                    &manifest_path,
                    &config.watch_directory,
                )
            {
                info!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = config.name.as_str(),
                    "project module already up to date, skipping build"
                );
                analytics::record_module_artifact(
                    &config.name,
                    ModuleKind::Project,
                    BuildStatus::Fresh,
                    0.0,
                    workspace_root,
                    &output_path,
                );
                return Ok(output_path);
            }
        }
    }

    run_build_command(
        workspace_root,
        &config.name,
        &config.build_command,
        &config.build_environment,
        cancel_flag,
    )?;

    // Step 3: Stage the freshly built artifacts into the private hot-load
    // directory, so what the host loads is never the slot other builds write
    // to. The managed backend loads from where its build wrote, so its two
    // paths are the same and the copy is skipped.
    let mut stage_ms = 0.0;
    if matches!(&config.backend, ProjectModuleBackend::NativeLibrary { .. }) {
        let stage_started = Instant::now();
        stage_artifact(&build_output, &output_path)?;
        // The build just produced a consistent set in `deps`; snapshot it
        // before anything else can write to those shared per-crate slots.
        #[cfg(feature = "hot_patch")]
        stage_shared_dependency_rlibs(workspace_root);
        #[cfg(feature = "hot_patch")]
        stage_artifact(&rlib_build_output, &rlib_output)?;
        stage_ms = stage_started.elapsed().as_secs_f64() * 1000.0;

        // Record the toolchain and host feature set that produced these
        // artifacts, then the artifacts themselves, so later runs can both
        // recognize them as up to date and tell them apart from anything
        // another build writes to the same paths.
        record_build_info(workspace_root);
        #[cfg(feature = "hot_patch")]
        let produced = vec![output_path.clone(), rlib_output.clone()];
        #[cfg(not(feature = "hot_patch"))]
        let produced = vec![output_path.clone()];
        record_artifact_stamp(
            workspace_root,
            &config.name,
            &config.build_command,
            &produced,
        );
    }

    // Step 4: Confirm the resolved artifact exists before reporting success.
    //
    // A successful process exit does not guarantee that configuration points
    // at the artifact it produced. Validate the resolved path here so loading
    // errors identify an output-directory mismatch rather than an opaque DLL
    // or managed-runtime failure later in the startup sequence.
    if !output_path.exists() {
        return Err(BuildError::OutputMissing {
            path: output_path.display().to_string(),
        });
    }

    analytics::record_module_artifact(
        &config.name,
        ModuleKind::Project,
        BuildStatus::Built,
        stage_ms,
        workspace_root,
        &output_path,
    );

    Ok(output_path)
}

/// Copy one freshly built artifact into its private hot-load location.
///
/// # Errors
///
/// Returns [`BuildError::OutputMissing`] when the build did not produce the
/// source artifact, and [`BuildError::HotArtifactCopyFailed`] when the copy
/// itself fails.
fn stage_artifact(build_output: &Path, hot_output: &Path) -> Result<(), BuildError> {
    if !build_output.exists() {
        return Err(BuildError::OutputMissing {
            path: build_output.display().to_string(),
        });
    }
    let Some(hot_directory) = hot_output.parent() else {
        return Err(BuildError::OutputMissing {
            path: hot_output.display().to_string(),
        });
    };
    std::fs::create_dir_all(hot_directory).map_err(|source| BuildError::HotArtifactCopyFailed {
        source_path: build_output.display().to_string(),
        target_path: hot_output.display().to_string(),
        source,
    })?;
    std::fs::copy(build_output, hot_output).map_err(|source| {
        BuildError::HotArtifactCopyFailed {
            source_path: build_output.display().to_string(),
            target_path: hot_output.display().to_string(),
            source,
        }
    })?;
    Ok(())
}

/// Where the shared per-crate dependency rlibs are staged for patch linking.
///
/// A sibling of the artifacts already staged in `target/hot`, and private to the
/// host for the same reason they are.
#[cfg(feature = "hot_patch")]
pub(crate) const STAGED_DEPENDENCY_SUBDIRECTORY: &str = "target/hot/deps";

/// Copy every shared per-crate rlib out of cargo's `deps` directory.
///
/// Cargo gives a workspace crate's rlib an unhashed, per-crate path -
/// `deps/libpill_dummy_color.rlib` - and *overwrites it in place* on every
/// build. Third-party crates get a metadata hash in their filename and are
/// therefore safe; workspace crates share one slot per crate name across every
/// feature configuration of that crate.
///
/// That slot is the whole problem. The host builds modules with
/// `--features pill_engine/hot_patch`; a developer running a plain
/// `cargo build` in a terminal writes a differently-featured artifact to the
/// same path. A generated patch then links a module rlib built against one
/// variant while the `--extern` for its dependency names the other, and rustc
/// refuses with `error[E0463]: can't find crate for <the module>` - which names
/// the module rather than the dependency that actually moved.
///
/// Copying them here, immediately after a host build produced a consistent set,
/// makes the patch link closure immune to anything written to those slots
/// afterwards. It is the same protection [`stage_artifact`] already gives the
/// module's own rlib, extended to what that rlib links against.
///
/// Only files whose staged copy is out of date are copied, so the steady-state
/// cost is a handful of `stat` calls. Failures are reported and not fatal: a
/// missing staged dependency only means the patch links the shared slot as
/// before.
#[cfg(feature = "hot_patch")]
pub(crate) fn stage_shared_dependency_rlibs(workspace_root: &Path) -> usize {
    let source_directory = workspace_root
        .join(CARGO_MODULE_OUTPUT_SUBDIRECTORY)
        .join("deps");
    let staged_directory = workspace_root.join(STAGED_DEPENDENCY_SUBDIRECTORY);
    let Ok(entries) = std::fs::read_dir(&source_directory) else {
        return 0;
    };
    if std::fs::create_dir_all(&staged_directory).is_err() {
        return 0;
    }

    let mut copied = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_shared_slot_rlib(file_name) {
            continue;
        }
        let staged = staged_directory.join(file_name);
        if staged_copy_is_current(&path, &staged) {
            continue;
        }
        if std::fs::copy(&path, &staged).is_ok() {
            copied += 1;
        }
    }
    if copied > 0 {
        debug!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            copied,
            directory = %staged_directory.display(),
            "staged shared dependency rlibs for patch linking"
        );
    }
    copied
}

/// Whether a `deps` filename is a shared per-crate slot rather than a
/// hash-qualified artifact.
///
/// Cargo appends `-<16 hex digits>` to a crate's filename when the artifact is
/// specific to one resolved configuration. A name without that suffix is the
/// shared slot every build of that crate name writes to, and is the only kind
/// another build can silently replace.
#[cfg(feature = "hot_patch")]
fn is_shared_slot_rlib(file_name: &str) -> bool {
    let Some(stem) = file_name.strip_suffix(".rlib") else {
        return false;
    };
    match stem.rsplit_once('-') {
        Some((_, suffix)) => {
            !(suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
        }
        None => true,
    }
}

/// Whether a staged copy already matches its source.
///
/// Same size and no older, which is the comparison the module rlib staging
/// already uses. Anything unknown counts as out of date, so the copy happens.
#[cfg(feature = "hot_patch")]
fn staged_copy_is_current(source: &Path, staged: &Path) -> bool {
    let (Ok(source_metadata), Ok(staged_metadata)) =
        (std::fs::metadata(source), std::fs::metadata(staged))
    else {
        return false;
    };
    if source_metadata.len() != staged_metadata.len() {
        return false;
    }
    match (source_metadata.modified(), staged_metadata.modified()) {
        (Ok(source_time), Ok(staged_time)) => staged_time >= source_time,
        _ => false,
    }
}

/// Build one optional module and return its expected output artifact.
///
/// Optional modules are workspace members, so their output always follows the
/// platform's native-library naming inside the configured output directory.
///
/// # Errors
///
/// Returns an error if the build fails for any of the reasons reported by
/// [`run_build_command`], or if the built library is missing afterwards.
pub(crate) fn build_optional_module(
    workspace_root: &Path,
    config: &OptionalModuleConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = config.name.as_str(),
        "building optional module"
    );

    // Cargo writes the freshly compiled cdylib into the shared per-crate
    // output slot, while the host loads from the private hot-load copy.
    // Keeping these paths distinct is what resolves the "one crate name, two
    // feature sets" collision: the project build may overwrite the shared
    // slot with the module's export-stripped dependency variant, but the
    // loaded generation always comes from the untouched hot copy.
    let build_output = workspace_root
        .join(CARGO_MODULE_OUTPUT_SUBDIRECTORY)
        .join(native_library_filename(&config.library_name));
    let hot_output = workspace_root
        .join(&config.output_subdirectory)
        .join(native_library_filename(&config.library_name));

    // The module's `rlib`, staged for the same reason the project's is: a
    // generated patch links it to reach the module's types, and cargo writes it
    // to an unhashed per-crate path that any other build of the same package
    // overwrites - including with a different feature set. Optional, because a
    // module that declares only a `cdylib` has none, which simply leaves the
    // fast path idle for that module.
    #[cfg(feature = "hot_patch")]
    let (rlib_build_output, rlib_output) = (
        workspace_root
            .join(CARGO_MODULE_OUTPUT_SUBDIRECTORY)
            .join(format!("lib{}.rlib", config.name)),
        workspace_root
            .join(&config.output_subdirectory)
            .join(format!("lib{}.rlib", config.name)),
    );

    // The module manifest sits next to the watched source directory, so its
    // path is derived from the watch directory rather than stored separately.
    let module_manifest = workspace_root
        .join(&config.watch_directory)
        .parent()
        .map(|parent| parent.join("Cargo.toml"));

    // Skip cargo when the hot-load artifact is already newer than every build
    // input; any uncertainty in the check falls through to a real build below.
    if let Some(manifest_path) = module_manifest {
        #[cfg_attr(not(feature = "hot_patch"), allow(unused_mut))]
        let mut required_artifacts = vec![hot_output.clone()];
        // Listed only when it is actually there, so the set matches what the
        // stamp recorded for a module that produces no `rlib`.
        #[cfg(feature = "hot_patch")]
        if rlib_output.is_file() {
            required_artifacts.push(rlib_output.clone());
        }
        if artifacts_are_host_built(
            workspace_root,
            &config.name,
            &config.build_command,
            &required_artifacts,
        ) && is_build_up_to_date(
            workspace_root,
            &hot_output,
            &manifest_path,
            &config.watch_directory,
        ) {
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = config.name.as_str(),
                "optional module already up to date, skipping build"
            );
            analytics::record_module_artifact(
                &config.name,
                ModuleKind::Optional,
                BuildStatus::Fresh,
                0.0,
                workspace_root,
                &hot_output,
            );
            return Ok(hot_output);
        }
    }

    run_build_command(
        workspace_root,
        &config.name,
        &config.build_command,
        &[],
        cancel_flag,
    )?;

    // Record the toolchain and host feature set that produced this artifact so
    // later runs can recognize it as up to date.
    record_build_info(workspace_root);

    // Stage the freshly built standalone library into the hot-load directory.
    // The build command produced the module-abi variant (with its
    // `pill_module_*` exports); the shared slot may later be overwritten by
    // the project's build of the export-stripped dependency variant, which is
    // exactly why the loadable copy lives apart from it.
    let stage_started = Instant::now();
    stage_artifact(&build_output, &hot_output)?;
    // As above: the dependency closure is consistent exactly now.
    #[cfg(feature = "hot_patch")]
    stage_shared_dependency_rlibs(workspace_root);
    #[cfg_attr(not(feature = "hot_patch"), allow(unused_mut))]
    let mut produced = vec![hot_output.clone()];
    #[cfg(feature = "hot_patch")]
    if rlib_build_output.is_file() {
        stage_artifact(&rlib_build_output, &rlib_output)?;
        produced.push(rlib_output.clone());
    }
    let stage_ms = stage_started.elapsed().as_secs_f64() * 1000.0;

    // Stamp the staged copies so a later run can tell them apart from anything
    // another build writes to the same paths.
    record_artifact_stamp(
        workspace_root,
        &config.name,
        &config.build_command,
        &produced,
    );

    if !hot_output.exists() {
        return Err(BuildError::OutputMissing {
            path: hot_output.display().to_string(),
        });
    }
    analytics::record_module_artifact(
        &config.name,
        ModuleKind::Optional,
        BuildStatus::Built,
        stage_ms,
        workspace_root,
        &hot_output,
    );
    Ok(hot_output)
}

/// Return the platform-specific filename produced for a native library.
fn native_library_filename(library_name: &str) -> String {
    // Cargo follows each platform's conventional dynamic-library prefix and
    // extension. Keeping this mapping here prevents backend orchestration from
    // accumulating operating-system-specific branches.
    if cfg!(target_os = "windows") {
        format!("{library_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{library_name}.dylib")
    } else {
        format!("lib{library_name}.so")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway workspace containing one module, a path dependency,
    /// and a freshly produced artifact, and return the workspace root plus the
    /// module's manifest path.
    ///
    /// The artifact is written after a short pause so its modification time is
    /// strictly newer than every input, matching the invariant a real build
    /// leaves behind.
    /// The scenario that produced an access violation in the field: the host
    /// builds an artifact, an unrelated `cargo build` overwrites it moments
    /// later with a different feature set, and every timestamp still looks
    /// fresh. The stamp is what tells the two apart.
    #[test]
    fn an_artifact_another_build_overwrote_is_not_host_built() {
        let (root, _) = test_workspace();
        let output = root.join("target/debug/module.dll");
        let command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--features".to_string(),
            "pill_engine/hot_patch".to_string(),
        ];

        record_artifact_stamp(&root, "module", &command, &[output.clone()]);
        assert!(
            artifacts_are_host_built(&root, "module", &command, &[output.clone()]),
            "the host's own artifact must be recognized"
        );

        // Another build writes a different library to the same path. It is
        // newer than every source, so the modification-time check alone would
        // accept it.
        std::thread::sleep(Duration::from_millis(30));
        fs_write(&output, "a differently configured native library");
        assert!(
            !artifacts_are_host_built(&root, "module", &command, &[output.clone()]),
            "an artifact the host did not write must be rebuilt, not loaded"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The same artifact built by a different command is a different artifact,
    /// which is what makes the check feature-aware.
    #[test]
    fn a_different_build_command_is_not_host_built() {
        let (root, _) = test_workspace();
        let output = root.join("target/debug/module.dll");
        let with_feature = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--features".to_string(),
            "pill_engine/hot_patch".to_string(),
        ];
        let without_feature = vec!["cargo".to_string(), "build".to_string()];

        record_artifact_stamp(&root, "module", &with_feature, &[output.clone()]);
        assert!(!artifacts_are_host_built(
            &root,
            "module",
            &without_feature,
            &[output.clone()]
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A missing stamp, or a missing artifact, can never be host-built - both
    /// fall through to a real build rather than being trusted.
    #[test]
    fn a_missing_stamp_or_artifact_is_not_host_built() {
        let (root, _) = test_workspace();
        let output = root.join("target/debug/module.dll");
        let command = vec!["cargo".to_string(), "build".to_string()];

        assert!(
            !artifacts_are_host_built(&root, "module", &command, &[output.clone()]),
            "no stamp has been recorded yet"
        );

        record_artifact_stamp(&root, "module", &command, &[output.clone()]);
        std::fs::remove_file(&output).unwrap();
        assert!(
            !artifacts_are_host_built(&root, "module", &command, &[output.clone()]),
            "a stamped artifact that no longer exists must be rebuilt"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Every artifact a build produced is covered, not just the first: the
    /// project's `rlib` is overwritten by the same builds that overwrite its
    /// library, and a patch linking the wrong one compiles against a
    /// differently configured engine.
    #[test]
    fn every_recorded_artifact_is_checked() {
        let (root, _) = test_workspace();
        let library = root.join("target/debug/module.dll");
        let rlib = root.join("target/debug/libmodule.rlib");
        fs_write(&rlib, "fake rlib");
        let command = vec!["cargo".to_string(), "build".to_string()];
        let artifacts = vec![library.clone(), rlib.clone()];

        record_artifact_stamp(&root, "module", &command, &artifacts);
        assert!(artifacts_are_host_built(&root, "module", &command, &artifacts));

        // Only the second artifact moves.
        std::thread::sleep(Duration::from_millis(30));
        fs_write(&rlib, "a differently configured rlib");
        assert!(
            !artifacts_are_host_built(&root, "module", &command, &artifacts),
            "a replaced rlib must invalidate the build as surely as a replaced library"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    fn test_workspace() -> (PathBuf, PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "pill_build_runner_test_{}_{unique}",
            std::process::id()
        ));

        // Workspace-level inputs shared by every member build.
        fs_write(&root.join("Cargo.toml"), "[workspace]\nmembers = []\n");
        fs_write(&root.join("Cargo.lock"), "# lockfile\n");
        fs_write(&root.join(".cargo/config.toml"), "");

        // The module itself, plus a path dependency it links.
        fs_write(&root.join("module/Cargo.toml"), DEPENDENT_MANIFEST);
        fs_write(
            &root.join("module/src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        );
        fs_write(
            &root.join("dependency/Cargo.toml"),
            "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        fs_write(
            &root.join("dependency/src/lib.rs"),
            "pub fn helper() -> u32 { 7 }\n",
        );

        // The artifact is produced last so it is newer than every input.
        std::thread::sleep(Duration::from_millis(30));
        let output = root.join("target/debug/module.dll");
        fs_write(&output, "fake native library");
        record_build_info(&root);

        let module_manifest = root.join("module/Cargo.toml");
        (root, module_manifest)
    }

    /// Write a file, creating parent directories as needed.
    fn fs_write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    /// A module manifest that links the local path dependency.
    const DEPENDENT_MANIFEST: &str = r#"
[package]
name = "module"
version = "0.1.0"
edition = "2021"

[dependencies]
dependency = { path = "../dependency" }
"#;

    /// A freshly built artifact with unchanged inputs is up to date.
    #[test]
    fn fresh_artifact_is_up_to_date() {
        let (root, module_manifest) = test_workspace();
        let output = root.join("target/debug/module.dll");
        let watch = "module/src";

        assert!(is_build_up_to_date(&root, &output, &module_manifest, watch));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A source edit newer than the artifact makes the module stale.
    #[test]
    fn edited_source_is_stale() {
        let (root, module_manifest) = test_workspace();
        let output = root.join("target/debug/module.dll");

        std::thread::sleep(Duration::from_millis(30));
        fs_write(
            &root.join("module/src/lib.rs"),
            "pub fn answer() -> u32 { 43 }\n",
        );

        assert!(!is_build_up_to_date(
            &root,
            &output,
            &module_manifest,
            "module/src"
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// An edit to a path dependency is visible to the dependent module.
    #[test]
    fn edited_path_dependency_is_stale() {
        let (root, module_manifest) = test_workspace();
        let output = root.join("target/debug/module.dll");

        std::thread::sleep(Duration::from_millis(30));
        fs_write(
            &root.join("dependency/src/lib.rs"),
            "pub fn helper() -> u32 { 8 }\n",
        );

        assert!(!is_build_up_to_date(
            &root,
            &output,
            &module_manifest,
            "module/src"
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A workspace lockfile update invalidates every cached artifact.
    #[test]
    fn edited_lockfile_is_stale() {
        let (root, module_manifest) = test_workspace();
        let output = root.join("target/debug/module.dll");

        std::thread::sleep(Duration::from_millis(30));
        fs_write(&root.join("Cargo.lock"), "# updated lockfile\n");

        assert!(!is_build_up_to_date(
            &root,
            &output,
            &module_manifest,
            "module/src"
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A missing toolchain marker disables the fast path.
    #[test]
    fn missing_build_info_is_stale() {
        let (root, module_manifest) = test_workspace();
        let output = root.join("target/debug/module.dll");

        std::fs::remove_file(root.join(BUILD_INFO_MARKER)).unwrap();

        assert!(!is_build_up_to_date(
            &root,
            &output,
            &module_manifest,
            "module/src"
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A missing artifact can never be up to date.
    #[test]
    fn missing_artifact_is_stale() {
        let (root, module_manifest) = test_workspace();
        let output = root.join("target/debug/module.dll");

        std::fs::remove_file(&output).unwrap();

        assert!(!is_build_up_to_date(
            &root,
            &output,
            &module_manifest,
            "module/src"
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
