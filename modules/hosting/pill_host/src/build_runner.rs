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
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

// External crates
#[cfg(feature = "hot_patch")]
use pill_core::debug;
use pill_core::error::BuildError;
use pill_core::info;
use pill_core::platform::Instant;
use pill_core::warn;

// Current crate
use crate::analytics::{self, ModuleKind};
use crate::{ExtensionConfig, ProjectModuleBackend, ProjectModuleConfig};

// =============================================================================
// Constants
// =============================================================================

/// Maximum wall-clock time a single build command may run.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

/// How often the build watchdog checks for completion and cancellation.
///
/// Small on purpose: a fresh module build answers in a few hundred ms, so a
/// coarse interval becomes dead time on every module's load - a hundred ms
/// here is a hundred ms per module, and a module set of any size multiplies
/// it. The check is a `try_wait` plus one memory sample, cheap enough to run
/// this often, and the finer granularity also makes a cancellation land
/// sooner during a reload.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Subdirectory, relative to the workspace root, where a host-spawned build
/// writes its freshly compiled artifacts.
///
/// Every cargo build the host spawns runs with `CARGO_TARGET_DIR` pointing at
/// the private module build tree ([`crate::config::MODULE_BUILD_TARGET_DIRECTORY`]),
/// never the shared `target/<profile>` the running binary maps its engine
/// dylibs from - a GUI frontend's module build needs a different engine
/// variant than the host runs, and rewriting the shared slot would make cargo
/// delete a DLL the host has loaded. The produced artifacts are staged into
/// the module's private hot-load directory, exactly as before.
fn cargo_module_output_subdirectory() -> String {
    crate::config::module_build_artifact_directory()
}

/// Private directory the host stages the project's loadable artifacts into,
/// and the default an extension's configuration also names.
///
/// The same protection extensions have always had, extended to the
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
/// A host rebuilt with a different feature set invalidates every cached
/// module artifact, because both features below change the crate metadata on
/// one side of the DLL boundary or the other.
///
/// `hot_patch` is mirrored into every module and project build directly, so
/// it changes the engine each module links.
///
/// `rendering` no longer touches the module's own engine - the renderer left
/// `pill_engine`, and module builds resolve the engine crates from
/// `pill_core`'s feature pin plus the host's explicit engine features (see
/// [`host_engine_features`]). It still belongs here: a windowed host loads
/// the wgpu renderer beside its engine dylibs, and a cached module artifact
/// is only trusted for the posture it was built for.
const HOST_MODULE_FEATURE_SET: &str =
    match (cfg!(feature = "rendering"), cfg!(feature = "hot_patch")) {
        (true, true) => "rendering+hot_patch",
        (true, false) => "rendering",
        (false, true) => "no-rendering+hot_patch",
        (false, false) => "no-rendering",
    };

/// The host's profiling level, part of [`host_build_identity`] for the same
/// reason as [`HOST_MODULE_FEATURE_SET`]: it changes `pill_core`'s resolved
/// features, and every spawned build mirrors it through
/// [`host_engine_features`].
const HOST_PROFILING_FEATURE_SET: &str = if cfg!(feature = "profiling-fine") {
    "profiling-fine"
} else if cfg!(feature = "profiling") {
    "profiling"
} else {
    "no-profiling"
};

/// Host build identity: toolchain, feature set, cargo profile, target, and the
/// environment every spawned build runs with.
///
/// The profile and target belong here for the same reason the feature set
/// does: both change the crate-metadata hash on both sides of the DLL
/// boundary, so an artifact built under one and loaded under the other fails
/// to resolve its exports. Without the profile in this identity, switching
/// between a debug and a release host would silently reuse the other profile's
/// staged copies; without the target, switching between a native host and a
/// `--target` host (a plain `cargo run` vs the dioxus CLI) reuses artifacts
/// whose symbols no longer match. The spawned environment belongs here too:
/// [`crate::config::spawned_build_environment`] decides whether module builds
/// strip `-C prefer-dynamic` (LTO and the fail-safe dx fallback) and whether
/// they mirror the dioxus CLI's `RUSTC_WORKSPACE_WRAPPER`, both of which
/// change every member crate's metadata hash. All of these present as
/// `LoadLibrary` error 127 rather than as anything that names a cause.
fn host_build_identity() -> String {
    let mut spawned_environment = crate::config::spawned_build_environment();
    // The pairs are compared as a set, so ordering never matters.
    spawned_environment.sort();
    let spawned_environment = spawned_environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{HOST_MODULE_FEATURE_SET}\n{HOST_PROFILING_FEATURE_SET}\nprofile={}\ntarget={}\nbuild_tree={}\nengine_features={}\nspawned_env={spawned_environment}",
        crate::config::host_profile_name(),
        crate::config::host_target_triple().unwrap_or("native"),
        crate::config::MODULE_BUILD_TARGET_DIRECTORY,
        host_engine_features().join(",")
    )
}

/// The engine features every host-spawned build must mirror from the running
/// host.
///
/// `pill_core.dll` and `pill_engine_core.dll` are loaded once per process and
/// their exported names hash the features their dependency trees resolved, so
/// a module or project compiled with a different engine feature set imports
/// names the loaded instances do not export and fails to load with
/// "The specified procedure could not be found" (os error 127). The names are
/// all declared as `pill_host` features, so `cfg!` sees exactly the posture
/// the running binary was built with.
fn host_engine_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "hot_patch") {
        features.push("pill_engine/hot_patch");
    }
    if cfg!(feature = "profiling")
        || cfg!(feature = "profiling-fine")
        || cfg!(feature = "profiling-verify")
    {
        features.push("pill_core/tracy");
    }
    if cfg!(feature = "profiling-fine") {
        features.push("pill_engine/profiling-fine");
    } else if cfg!(feature = "profiling-verify") {
        features.push("pill_engine/profiling-verify");
    } else if cfg!(feature = "profiling") {
        features.push("pill_engine/profiling");
    }
    if cfg!(feature = "metrics") {
        features.push("pill_engine/metrics");
    }
    if cfg!(feature = "dev-logs") {
        features.push("pill_core/dev-logs");
    }
    features
}

// =============================================================================
// Build Identity and Artifact Stamps
// =============================================================================

/// The toolchain version line and host feature set of the running host.
///
/// Resolved once per process: spawning `rustc` for every stamp write would
/// cost more than the stamps themselves save.
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
            format!("{rustc_version}\n{}", host_build_identity())
        })
        .clone()
}

/// Path of the stamp recording what the host built for one module.
fn artifact_stamp_path(workspace_root: &Path, module_name: &str) -> PathBuf {
    workspace_root
        .join(ARTIFACT_STAMP_DIRECTORY)
        .join(format!("{module_name}.txt"))
}

/// Describe the artifacts a build produced, together with the host identity,
/// the sources and the command that produced them.
///
/// `build_info` is [`current_build_info`]: the toolchain and the host's feature
/// set, profile, target and spawned environment. It stays in the stamp because
/// two host processes with different feature sets can share one workspace and
/// one staging directory: a stamp either of them could satisfy would let one
/// process load the other's artifact, which fails inside `LoadLibrary` naming
/// nothing.
///
/// `source_identity` is the watch directory, which is what distinguishes two
/// projects that share a package name - and therefore an output path, a stamp
/// file and a build command. Without it, switching the host between two such
/// projects leaves every earlier check satisfied by the other project's DLL.
///
/// Returns `None` when any artifact is missing or unreadable, which the callers
/// treat as "not host-built" and therefore as a reason to run cargo.
fn artifact_stamp(
    build_info: &str,
    source_identity: &str,
    build_command: &[String],
    artifacts: &[PathBuf],
) -> Option<String> {
    // The separator cannot appear in a command argument, so two different
    // argument lists can never produce the same line.
    let mut lines = vec![
        build_info.to_string(),
        source_identity.to_string(),
        build_command.join("\u{1}"),
    ];
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
    source_identity: &str,
    build_command: &[String],
    artifacts: &[PathBuf],
) {
    let Some(stamp) = artifact_stamp(
        &current_build_info(),
        source_identity,
        build_command,
        artifacts,
    ) else {
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
///
/// The recorded `source_identity` closes the remaining gap: package names are
/// not unique across projects, so `examples/project_rs` and a test fixture
/// project both build `project.dll` into the same slot with the same command.
/// Every timestamp check then passes, because the artifact really is newer than
/// the sources - just the wrong sources. The host would load one project's
/// code while watching another's.
fn artifacts_are_host_built(
    workspace_root: &Path,
    module_name: &str,
    source_identity: &str,
    build_command: &[String],
    artifacts: &[PathBuf],
) -> bool {
    let Some(current) = artifact_stamp(
        &current_build_info(),
        source_identity,
        build_command,
        artifacts,
    ) else {
        return false;
    };
    let recorded = std::fs::read_to_string(artifact_stamp_path(workspace_root, module_name));
    recorded.is_ok_and(|recorded| recorded == current)
}

/// Confirm a staged artifact set still matches the stamp just recorded for it.
///
/// The build that produced these files has exited, but a cancelled build's
/// surviving grandchildren can still write into the staging directory, which
/// is shared with everything else building the same crate. When the stamp no
/// longer matches, the copy about to be loaded is not the one this host built,
/// so the build fails here instead of being loaded as if nothing had changed.
///
/// # Errors
///
/// Returns [`BuildError::StagedArtifactChanged`] when the stamp no longer
/// matches the files on disk.
fn confirm_staged_artifacts(
    workspace_root: &Path,
    module_name: &str,
    watch_directory: &str,
    build_command: &[String],
    produced: &[PathBuf],
) -> Result<(), BuildError> {
    if artifacts_are_host_built(
        workspace_root,
        module_name,
        watch_directory,
        build_command,
        produced,
    ) {
        return Ok(());
    }
    Err(BuildError::StagedArtifactChanged {
        name: module_name.to_string(),
        path: produced
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    })
}

// =============================================================================
// Free Functions
// =============================================================================

/// Apply the environment and argument overrides every host-spawned cargo build
/// needs, regardless of which path spawns it.
///
/// The full module-reload path and the per-function hot-patch path both invoke
/// the module's configured `cargo build ...` command, and both must agree with
/// the host binary that is running right now:
///
/// - `CARGO_TARGET_DIR` redirects into the private module build tree so cargo
///   never has to delete a DLL the host has mapped, and both paths share one
///   artifact set (see [`crate::config::MODULE_BUILD_TARGET_DIRECTORY`]).
/// - The host's own engine features ([`host_engine_features`]) are passed to
///   the build explicitly. `pill_core.dll` and `pill_engine_core.dll` are
///   loaded once per process and their exported names hash the features their
///   dependency trees resolved, so a module or project built with a different
///   engine feature set cannot resolve its imports against the single
///   instances the host already has loaded (Windows deduplicates loaded
///   modules by name). `devops/tests/test_engine_feature_drift.py` checks
///   that every posture agrees.
/// - A launcher-injected profile (the dioxus CLI builds the editor under
///   `--profile desktop-dev`) is not declared in the module workspaces, so
///   cargo would reject `--profile <name>` here. It is defined on the spawned
///   build as an inheritor of `dev` so the module compiles under the same
///   profile name the host binary itself used - profile name is part of
///   cargo's crate-metadata hash, so a differently named profile would produce
///   a module whose DLL imports cannot resolve against the engine dylib the
///   host already has loaded. Built-in profiles need no definition.
///
/// Only cargo builds take these overrides; `dotnet` module builds ignore them.
pub(crate) fn apply_cargo_host_overrides(command: &mut Command, workspace_root: &Path) {
    command.env(
        "CARGO_TARGET_DIR",
        workspace_root.join(crate::config::MODULE_BUILD_TARGET_DIRECTORY),
    );
    if crate::config::running_under_dioxus_cli() {
        // The dioxus editor keeps the cargo anchor. Its own macro graph
        // (dioxus' procedural macros) unions features onto the HOST units of
        // `proc-macro2`/`quote`/`syn` - a resolution cargo keeps separate
        // from the target side - and those unions change the metadata of the
        // derive-macro crates (`serde_derive`, `thiserror_impl`, ...), which
        // cascades into `pill_core`'s and `pill_engine_core`'s exported
        // symbol hashes. Only selecting the editor package reproduces the
        // exact graph; enumerating its unions is not possible.
        if let Some(anchor) = host_anchor_package() {
            command.arg("--package").arg(&anchor);
            // Select the anchor with the SAME features the running host was
            // built with, not merely the same package. The anchor's default
            // features are dropped first, because cargo would otherwise
            // resolve `pill_standalone`'s defaults (`hot_patch`) onto a host
            // built without them.
            if let Some(declared) = declared_features(workspace_root, &anchor) {
                command.arg("--no-default-features");
                for feature in host_posture_features() {
                    if declared.iter().any(|name| name == feature) {
                        command.arg("--features").arg(format!("{anchor}/{feature}"));
                    }
                }
            } else {
                // The anchor's manifest was not found: keep its defaults and
                // add what the host is known to need.
                for feature in host_posture_features() {
                    if feature != "dev" && feature != "hot_patch" {
                        command.arg("--features").arg(format!("{anchor}/{feature}"));
                    }
                }
            }
        }
    } else {
        // Every other host receives its engine features explicitly, so the
        // spawned build resolves `pill_core.dll` and `pill_engine_core.dll`
        // exactly as the loaded host instances did - and nothing it does not
        // need. See [`host_engine_features`].
        for feature in host_engine_features() {
            command.arg("--features").arg(feature);
        }
    }
    // Mirror the host's own `--target` when a launcher (the dioxus CLI) built
    // it with one. Cargo folds the target into every crate's metadata hash,
    // so a module built natively against a `--target` host cannot resolve its
    // dynamic imports against the host's loaded engine dylibs (os error 127
    // at `LoadLibrary`). Cargo writes the mirrored build under
    // `<CARGO_TARGET_DIR>/<triple>/<profile>`, which
    // [`crate::config::module_build_artifact_directory`] already accounts for.
    if let Some(triple) = crate::config::host_target_triple() {
        command.arg("--target").arg(triple);
    }
    let profile = crate::config::host_profile_name();
    if !matches!(profile, "dev" | "release" | "test" | "bench") {
        command
            .arg("--config")
            .arg(format!("profile.{profile}.inherits=\"dev\""));
    }
}

/// The workspace package name to anchor dx-hosted module builds to.
///
/// Cargo unifies features across every selected package, so module builds
/// under the dioxus CLI select the editor's own package (`-p editor`) to
/// force the shared engine crates onto the editor's exact feature universe.
/// The package name is normally the executable's file stem (`editor` ->
/// package `editor`), but the Dioxus CLI (`dx`) stages the built binary under
/// a cargo-metadata-hash suffixed name such as `editor-d6d95e94.exe`; trim
/// that trailing `-<hex>` suffix so the anchor still resolves to the real
/// package.
fn host_anchor_package() -> Option<String> {
    let stem = std::env::current_exe()
        .ok()?
        .file_stem()?
        .to_str()?
        .to_owned();
    if let Some((base, suffix)) = stem.rsplit_once('-') {
        let is_metadata_hash = !suffix.is_empty()
            && suffix.len() <= 16
            && suffix
                .chars()
                .all(|character| character.is_ascii_hexdigit());
        if is_metadata_hash {
            return Some(base.to_owned());
        }
    }
    Some(stem)
}

/// The frontend features the running host was built with, used to pick the
/// anchor's feature selection under the dioxus CLI.
fn host_posture_features() -> Vec<&'static str> {
    let mut features = vec!["dev"];
    if cfg!(feature = "hot_patch") {
        features.push("hot_patch");
    }
    if cfg!(feature = "rendering") {
        features.push("rendering");
    }
    if cfg!(feature = "profiling-fine") {
        features.push("profiling-fine");
    } else if cfg!(feature = "profiling") {
        features.push("profiling");
    }
    features
}

/// The feature names `package` declares, read from its manifest among the
/// workspace members one or two directories below `workspace_root`; `None`
/// when no such manifest is found.
///
/// Cached per package: the members do not change while a host runs. Used by
/// the host's wrapper generation, which copies the extension's declared
/// features onto the wrapper's dependency edge.
pub(crate) fn declared_features(workspace_root: &Path, package: &str) -> Option<Vec<String>> {
    static DECLARED: OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<Vec<String>>>>,
    > = OnceLock::new();
    let mut cache = DECLARED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entry(package.to_string())
        .or_insert_with(|| read_declared_features(workspace_root, package))
        .clone()
}

/// Uncached [`declared_features`]: scans `*/Cargo.toml` and `*/*/Cargo.toml`
/// under `workspace_root` for the package named `package`.
fn read_declared_features(workspace_root: &Path, package: &str) -> Option<Vec<String>> {
    let mut manifests = Vec::new();
    for first in std::fs::read_dir(workspace_root).ok()?.flatten() {
        let directory = first.path();
        manifests.push(directory.join("Cargo.toml"));
        if let Ok(children) = std::fs::read_dir(&directory) {
            manifests.extend(
                children
                    .flatten()
                    .map(|child| child.path().join("Cargo.toml")),
            );
        }
    }
    manifests.into_iter().find_map(|manifest| {
        let text = std::fs::read_to_string(&manifest).ok()?;
        let document = text.parse::<toml_edit::DocumentMut>().ok()?;
        let name = document.get("package")?.get("name")?.as_str()?;
        if name != package {
            return None;
        }
        let features = document
            .get("features")
            .and_then(|item| item.as_table_like())
            .map(|table| table.iter().map(|(key, _)| key.to_string()).collect())
            .unwrap_or_default();
        Some(features)
    })
}

// =============================================================================
// Piggybacked flag capture
// =============================================================================

/// Harvests the module crate's `rustc` invocation out of a build already running.
///
/// # Why
///
/// A fast patch replays the exact compiler line cargo uses for the module, so
/// the patch links the identical dependency closure. Discovering that line
/// costs a `cargo build -v`, and cargo only prints the invocation when it
/// actually compiles - so when the crate is already fresh the discovery path
/// has to TOUCH the crate root and force a rebuild of the module crate.
/// Measured before the wrapper layout, that was 1.8-3.4 s under the standalone
/// host and up to ~15 s under the editor (the old cargo anchor rebuilt the
/// frontend beside the module), paid on the first patch after every module
/// reload, because a reload is exactly what makes the cached line stale.
///
/// The build the host just ran compiled that crate for real. Asking it for
/// `-v` and reading the line out of its output makes the discovery free and
/// makes the cache fresh at the same instant the artifact it describes appears.
///
/// # How
///
/// `-v` is appended at spawn time only, never to the configured build command:
/// that command is part of the cache key and of the artifact stamp, and adding
/// a flag to it would invalidate both. Cargo's stderr is piped rather than
/// inherited so it can be scanned, and every line is written straight back out
/// so the console still streams compiler progress live - minus the two kinds of
/// line `-v` itself adds (`Running` and `Fresh`), which would otherwise bury
/// the diagnostics. Colour is requested explicitly when the host's own stderr
/// is a terminal, because cargo turns it off for a pipe.
#[cfg(feature = "hot_patch")]
struct VerboseCapture {
    /// Crate names to look for: the modules one invocation builds.
    crate_names: Vec<String>,
    /// Workspace the build runs in, so the flags caches land in that
    /// workspace's own build tree rather than a shared temporary directory.
    workspace_root: PathBuf,
    /// Reader thread and the lines it found, once joined.
    reader: Option<std::thread::JoinHandle<Vec<(String, crate::hot_patch::CargoRustcLine)>>>,
}

#[cfg(feature = "hot_patch")]
impl VerboseCapture {
    /// Turn `command` into a verbose, pipe-reading build, for cargo only.
    ///
    /// Returns `None` for a non-cargo build (the managed backend's `dotnet`),
    /// which has no rustc line to harvest and must keep its inherited streams,
    /// and when nothing asked for a capture.
    fn arm(
        command: &mut Command,
        program: &str,
        names: &[&str],
        workspace_root: &Path,
    ) -> Option<Self> {
        if program != "cargo" || names.is_empty() {
            return None;
        }
        use std::io::IsTerminal;
        if std::io::stderr().is_terminal() {
            command.arg("--color").arg("always");
        }
        command.arg("-v").stderr(std::process::Stdio::piped());
        Some(Self {
            crate_names: names.iter().map(|name| (*name).to_string()).collect(),
            workspace_root: workspace_root.to_path_buf(),
            reader: None,
        })
    }

    /// Begin draining the child's stderr.
    fn start(mut self, child: &mut std::process::Child) -> Self {
        let Some(stderr) = child.stderr.take() else {
            return self;
        };
        let crate_names = self.crate_names.clone();
        self.reader = std::thread::Builder::new()
            .name("pill-build-capture".to_string())
            .spawn(move || {
                use std::io::{BufRead, BufReader, Write};
                let mut found: Vec<(String, crate::hot_patch::CargoRustcLine)> = Vec::new();
                let mut reader = BufReader::new(stderr);
                let mut line = Vec::new();
                // Read bytes rather than `lines()`: compiler output is not
                // guaranteed to be valid UTF-8 on every locale, and a decode
                // error must not truncate the build log.
                while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
                    let text = String::from_utf8_lossy(&line);
                    if found.len() < crate_names.len() {
                        for name in &crate_names {
                            if found.iter().any(|(found_name, _)| found_name == name) {
                                continue;
                            }
                            if let Some(found_line) =
                                crate::hot_patch::parse_rustc_line(&text, name)
                            {
                                found.push((name.clone(), found_line));
                            }
                        }
                    }
                    if !is_verbose_only_line(&text) {
                        let _ = std::io::stderr().write_all(&line);
                    }
                    line.clear();
                }
                let _ = std::io::stderr().flush();
                found
            })
            .ok();
        self
    }

    /// Join the reader and cache every line it found.
    ///
    /// A build that recompiled nothing prints no invocation, which is not a
    /// failure: the cached line from when the crate WAS compiled still
    /// describes it, and the patch pipeline's own freshness check decides that.
    /// `commands` pairs every requested name with the build command its flags
    /// cache records.
    fn finish(self, commands: &[(&str, &[String])]) {
        let Some(reader) = self.reader else {
            return;
        };
        let Ok(found) = reader.join() else {
            return;
        };
        for (name, line) in found {
            let Some((_, build_command)) = commands
                .iter()
                .find(|(command_name, _)| *command_name == name)
            else {
                continue;
            };
            let cache = crate::hot_patch::flags_cache_path(&self.workspace_root, &name);
            match line.save(&cache, build_command) {
                Ok(()) => debug!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = name.as_str(),
                    cache = %cache.display(),
                    "captured the patch compiler flags from this build"
                ),
                Err(error) => debug!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    module = name.as_str(),
                    error = %error,
                    "could not cache the patch compiler flags; the next patch will re-capture"
                ),
            }
        }
    }
}

/// Whether a line exists only because the build was asked for `-v`.
///
/// Filtered out on the way to the console so the reload output reads exactly as
/// it did before the capture was added; nothing else is touched.
///
/// Cargo colours its status words, so the label is preceded by ANSI escape
/// sequences whenever the host's stderr is a terminal - matching on the raw
/// prefix would silently stop filtering in exactly the case a human is
/// watching, and dump every multi-kilobyte `rustc` command line into the
/// console.
#[cfg(feature = "hot_patch")]
fn is_verbose_only_line(line: &str) -> bool {
    let plain = without_ansi(line);
    let label = plain.trim_start();
    // The three status words `-v` adds: the invocation itself, the crates it
    // skipped, and the reason it did not skip the others.
    label.starts_with("Running `") || label.starts_with("Fresh ") || label.starts_with("Dirty ")
}

/// The line with every ANSI escape sequence removed.
///
/// Cargo colours the status word alone, so the escapes sit both before and
/// immediately after the word being matched; dropping only the leading ones
/// still leaves `Running<ESC>[0m ` and matches nothing.
#[cfg(feature = "hot_patch")]
fn without_ansi(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(escape) = rest.find('\u{1b}') {
        plain.push_str(&rest[..escape]);
        let after = &rest[escape..];
        // A CSI sequence: ESC '[' <parameters> <final byte in @..~>. Anything
        // that does not look like one is kept verbatim, so unusual compiler
        // output is passed through rather than swallowed.
        let Some(parameters) = after.strip_prefix("\u{1b}[") else {
            plain.push('\u{1b}');
            rest = &after[1..];
            continue;
        };
        match parameters.find(|character: char| ('@'..='~').contains(&character)) {
            Some(end) => rest = &parameters[end + 1..],
            None => return plain,
        }
    }
    plain.push_str(rest);
    plain
}

// =============================================================================
// Build Process Tree
// =============================================================================

/// How long a stopped build's process tree is given to disappear.
///
/// Termination is asynchronous and the next build starts against the same
/// `CARGO_TARGET_DIR` immediately afterwards, so this wait is what keeps a
/// dying compiler from holding cargo's package lock across the handover.
const TREE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// A build's whole process tree, on platforms that can name one.
///
/// Killing the direct child reaches cargo alone: the rustc and linker
/// grandchildren survive, keep writing into the shared module build tree, and
/// can hold cargo's package lock - making the next build block for its whole
/// timeout and then be reported as `TimedOut`, which reads as an edit that was
/// ignored. A Windows job object makes the tree the unit of termination: every
/// process cargo spawns inherits membership, and
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` guarantees the tree dies even when the
/// last handle is closed rather than a termination being requested.
#[cfg(windows)]
struct BuildProcessTree {
    /// The job object handle, closed on drop.
    job: isize,
}

/// A build's whole process tree, where the platform has no job equivalent.
///
/// The direct child is killed instead, which is all those platforms offer
/// here; the type exists so the call sites need no platform `cfg` of their own.
#[cfg(not(windows))]
struct BuildProcessTree;

/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing the last handle to the job
/// kills every process still in it.
#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

/// `JobObjectExtendedLimitInformation`, the information class through which
/// the kill-on-close limit is set.
#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;

/// `JobObjectBasicAccountingInformation`, the information class that reports
/// how many processes a job currently holds.
#[cfg(windows)]
const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: u32 = 1;

// The `kernel32` entry points that make a job object the unit of build
// cancellation. Declared here rather than pulled from a crate with
// `windows-sys`, whose features would unify with the modules' dependency graph
// and split the shared engine crates into differently featured variants - the
// same reason the process-memory counters are declared by hand in `analytics`.
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *mut std::ffi::c_void, name: *const u16) -> isize;
    fn SetInformationJobObject(
        job: isize,
        information_class: u32,
        information: *const std::ffi::c_void,
        length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
    fn TerminateJobObject(job: isize, exit_code: u32) -> i32;
    fn QueryInformationJobObject(
        job: isize,
        information_class: u32,
        information: *mut std::ffi::c_void,
        length: u32,
        returned_length: *mut u32,
    ) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
}

/// `JOBOBJECT_BASIC_LIMIT_INFORMATION`, the limits half of the job's extended
/// limit information.
///
/// Only `limit_flags` is ever set; the rest is present because the kernel
/// reads the whole structure through the pointer.
#[cfg(windows)]
#[repr(C)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

/// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, the layout the extended-limit
/// information class names.
#[cfg(windows)]
#[repr(C)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    /// `IO_COUNTERS`, six `ULONGLONG`s, only ever zeroed.
    io_info: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

/// `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION`, whose `active_processes` field is
/// what the drain wait polls.
#[cfg(windows)]
#[repr(C)]
struct JobObjectBasicAccountingInformation {
    total_user_time: i64,
    total_kernel_time: i64,
    this_period_total_user_time: i64,
    this_period_total_kernel_time: i64,
    total_page_fault_count: u32,
    total_processes: u32,
    active_processes: u32,
    total_terminated_processes: u32,
}

#[cfg(windows)]
impl BuildProcessTree {
    /// Create a job that kills whatever is still in it when its handle closes.
    ///
    /// `None` when Windows refuses; the caller then falls back to killing the
    /// direct child, which is exactly what happens on platforms without jobs.
    fn create() -> Option<Self> {
        // SAFETY: a null security descriptor and an unnamed job are both
        // valid; the call returns a handle this code owns on success.
        let job = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if job == 0 {
            return None;
        }
        let limits = JobObjectExtendedLimitInformation {
            basic_limit_information: JobObjectBasicLimitInformation {
                per_process_user_time_limit: 0,
                per_job_user_time_limit: 0,
                limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                minimum_working_set_size: 0,
                maximum_working_set_size: 0,
                active_process_limit: 0,
                affinity: 0,
                priority_class: 0,
                scheduling_class: 0,
            },
            io_info: [0; 6],
            process_memory_limit: 0,
            job_memory_limit: 0,
            peak_process_memory_used: 0,
            peak_job_memory_used: 0,
        };
        // SAFETY: a live job handle, a structure whose layout matches the
        // named information class, and that structure's own size.
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                std::ptr::addr_of!(limits).cast(),
                std::mem::size_of::<JobObjectExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            // A job that cannot promise to kill its tree is worse than none:
            // the caller would stop using `Child::kill` for no benefit, so the
            // handle is closed and the job treated as unavailable.
            // SAFETY: the handle is live and owned by this function.
            unsafe { CloseHandle(job) };
            return None;
        }
        Some(Self { job })
    }

    /// Join a freshly spawned process to the job, so the compiler and linker
    /// invocations it starts inherit membership.
    ///
    /// A process started before the join would sit outside the tree, which is
    /// why the caller creates the job before spawning and joins immediately
    /// after.
    fn join(&self, process_id: u32) -> bool {
        // PROCESS_TERMINATE and PROCESS_SET_QUOTA: the two rights assigning a
        // process to a job requires.
        const ASSIGN_ACCESS: u32 = 0x0001 | 0x0100;
        // SAFETY: an access mask, no handle inheritance, and the process id of
        // a child the caller has not yet waited on.
        let process = unsafe { OpenProcess(ASSIGN_ACCESS, 0, process_id) };
        if process == 0 {
            return false;
        }
        // SAFETY: both handles are live; the job handle is owned by `self` and
        // the job keeps its own reference to the process.
        let assigned = unsafe { AssignProcessToJobObject(self.job, process) } != 0;
        // SAFETY: the process handle is owned by this function.
        unsafe { CloseHandle(process) };
        assigned
    }

    /// Terminate every process still in the job.
    fn terminate(&self) {
        // SAFETY: a live job handle, and a non-zero exit code because zero
        // would read as a successful exit to anything that reaps one of these
        // processes.
        unsafe { TerminateJobObject(self.job, 1) };
    }

    /// How many processes the job still holds.
    fn active_processes(&self) -> u32 {
        let mut accounting = JobObjectBasicAccountingInformation {
            total_user_time: 0,
            total_kernel_time: 0,
            this_period_total_user_time: 0,
            this_period_total_kernel_time: 0,
            total_page_fault_count: 0,
            total_processes: 0,
            active_processes: 0,
            total_terminated_processes: 0,
        };
        // SAFETY: a live job handle, a structure whose layout matches the
        // named information class, and that structure's own size; the optional
        // returned-length pointer is null.
        let queried = unsafe {
            QueryInformationJobObject(
                self.job,
                JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                std::ptr::addr_of_mut!(accounting).cast(),
                std::mem::size_of::<JobObjectBasicAccountingInformation>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            // An unanswerable query is treated as "something may still be
            // running": the drain wait then times out and says so, which is
            // the safe reading for a function about to hand the staging
            // directory to the next build.
            return 1;
        }
        accounting.active_processes
    }

    /// Wait until the job reports no live process, or the deadline passes.
    ///
    /// Returns whether the tree emptied.
    fn wait_until_empty(&self, deadline: Instant) -> bool {
        loop {
            if self.active_processes() == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
impl Drop for BuildProcessTree {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateJobObjectW` and is closed
        // exactly once, here. Closing the last handle is what
        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` makes lethal to anything still
        // in the job.
        unsafe { CloseHandle(self.job) };
    }
}

/// Create the tree for a build about to be spawned.
///
/// Must run before the spawn: a process started before it joins would sit
/// outside the tree and survive a cancellation.
#[cfg(windows)]
fn create_process_tree() -> Option<BuildProcessTree> {
    BuildProcessTree::create()
}

/// Platforms without job objects have nothing to create; `Child::kill` is the
/// whole of the vocabulary there.
#[cfg(not(windows))]
fn create_process_tree() -> Option<BuildProcessTree> {
    None
}

/// Join a freshly spawned child to its build's tree.
///
/// A refusal is reported rather than fatal: the build still runs, it just
/// keeps the weaker guarantee that a cancellation reaches the direct child.
#[cfg(windows)]
fn join_process_tree(tree: Option<&BuildProcessTree>, child: &Child) {
    let Some(tree) = tree else {
        return;
    };
    if !tree.join(child.id()) {
        warn!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            process_id = child.id(),
            "could not join the build process to its job object; a cancelled build will only stop the direct child"
        );
    }
}

/// Nothing to join on platforms without job objects.
#[cfg(not(windows))]
fn join_process_tree(_tree: Option<&BuildProcessTree>, _child: &Child) {}

/// Stop a build and its whole tree.
///
/// The tree is terminated first and then waited for, because the next build
/// starts against the same `CARGO_TARGET_DIR` as soon as this returns: an
/// orphan still holding cargo's package lock would make that build block for
/// its whole timeout, and an orphan writing into the staging directory would
/// race the artifact the host is about to load.
#[cfg(windows)]
fn stop_process_tree(tree: Option<&BuildProcessTree>, child: &mut Child) {
    let Some(tree) = tree else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    tree.terminate();
    let _ = child.wait();
    if !tree.wait_until_empty(Instant::now() + TREE_DRAIN_TIMEOUT) {
        warn!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            active_processes = tree.active_processes(),
            "a stopped build's process tree is still running; the next build may block on cargo's package lock or see its staged artifacts rewritten"
        );
    }
}

/// Kill the direct child: without a job object, a grandchild cannot be reached
/// at all.
#[cfg(not(windows))]
fn stop_process_tree(_tree: Option<&BuildProcessTree>, child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Run one module's build command to completion.
///
/// Shared by the project module and by extensions so both use the same
/// process handling, watchdog, cancellation, and failure reporting. Resolving
/// and validating the produced artifact is left to the caller, because each
/// module kind names and locates its output differently.
///
/// With the `hot_patch` feature this also harvests the compiler flags the fast
/// patch pipeline needs, out of the build it was going to run anyway - see
/// [`VerboseCapture`]; `capture_entries` names the crates to harvest and pairs
/// each with the build command its flags cache records. An empty or absent
/// list skips the harvest and keeps the child's streams inherited.
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
    capture_entries: Option<&[(&str, &[String])]>,
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
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(workspace_root)
        .envs(build_environment.iter().map(|(key, value)| (key, value)));
    // Redirect the artifact tree, unify the feature universe with the running
    // host, and define the launcher-injected profile. See the helper's docs
    // for why each override exists; `dotnet` builds ignore them all.
    if program == "cargo" {
        apply_cargo_host_overrides(&mut command, workspace_root);
    }
    // Ask this build to say which rustc invocation it used, so the fast patch
    // pipeline never has to run a build of its own to find out.
    #[cfg(feature = "hot_patch")]
    let capture_names: Vec<&str> = capture_entries
        .into_iter()
        .flatten()
        .map(|(captured_name, _)| *captured_name)
        .collect();
    #[cfg(feature = "hot_patch")]
    let capture = VerboseCapture::arm(&mut command, program, &capture_names, workspace_root);
    #[cfg(not(feature = "hot_patch"))]
    let _ = capture_entries;
    // The tree is created before the spawn so the child can never run outside
    // it, and joined immediately after: a process started between the two
    // would survive a cancellation of the rest.
    let process_tree = create_process_tree();
    let mut child = command.spawn().map_err(|source| BuildError::SpawnFailed {
        name: name.to_string(),
        source,
    })?;
    join_process_tree(process_tree.as_ref(), &child);
    // Started before the watchdog loop: cargo's stderr must be drained while
    // the build runs, or the pipe fills and the compiler blocks forever.
    #[cfg(feature = "hot_patch")]
    let capture = capture.map(|capture| capture.start(&mut child));

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
            stop_process_tree(process_tree.as_ref(), &mut child);
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
            stop_process_tree(process_tree.as_ref(), &mut child);
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
    // Only after a successful build: a failed one may have recompiled the crate
    // with flags that never produced the artifact now on disk.
    #[cfg(feature = "hot_patch")]
    if let Some(capture) = capture {
        capture.finish(capture_entries.unwrap_or(&[]));
    }
    analytics::record_build_command(
        name,
        build_started.elapsed().as_millis() as u64,
        cargo_peak_bytes,
    );
    Ok(())
}

/// Add the compiler-command-line capture to a managed project's build.
///
/// A native build is returned unchanged. For a C# build this appends the two
/// MSBuild properties that make the build report its own `csc` invocation:
/// `ProvideCommandLineArgs` populates the item group, and the injected targets
/// file writes it out. Neither changes what is compiled - only whether the
/// build leaves behind a record of how it compiled it.
///
/// A missing targets file silently leaves the command alone, which costs the
/// fast reload path and nothing else.
#[cfg(feature = "hot_reload")]
fn with_compiler_argument_capture(
    workspace_root: &Path,
    config: &ProjectModuleConfig,
) -> Vec<String> {
    let mut build_command = config.build_command.clone();
    if !matches!(&config.backend, ProjectModuleBackend::CSharp(_)) {
        return build_command;
    }
    let targets = workspace_root.join(crate::config::CSHARP_COMPILER_ARGUMENTS_TARGETS);
    if !targets.is_file() {
        return build_command;
    }
    build_command.push("-p:ProvideCommandLineArgs=true".to_string());
    build_command.push(format!(
        "-p:CustomAfterMicrosoftCSharpTargets={}",
        targets.display()
    ));
    build_command
}

/// Build the managed assembly that compiles C# projects in-process.
///
/// Built separately from every managed project because nothing references it:
/// Roslyn is large and hostile to NativeAOT, so keeping it out of each project's
/// reference graph is what keeps it out of a shipping bundle. MSBuild's own
/// incremental check is what makes an unchanged compiler project cheap to
/// "build"; the host does not second-guess it.
///
/// # Errors
///
/// Returns [`BuildError::OutputMissing`] when the compiler project or its
/// output assembly is absent, or whatever [`run_build_command`] reports.
#[cfg(feature = "hot_reload")]
pub(crate) fn build_csharp_compiler(workspace_root: &Path) -> Result<PathBuf, BuildError> {
    let manifest = workspace_root.join(crate::config::CSHARP_COMPILER_MANIFEST);
    if !manifest.is_file() {
        return Err(BuildError::OutputMissing {
            path: manifest.display().to_string(),
        });
    }
    let output_path = workspace_root
        .join(crate::config::CSHARP_COMPILER_OUTPUT_SUBDIRECTORY)
        .join(format!(
            "{}.dll",
            crate::config::CSHARP_COMPILER_ASSEMBLY_NAME
        ));
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        "building the in-process C# compiler"
    );
    let build_command = vec![
        "dotnet".to_string(),
        "build".to_string(),
        crate::config::CSHARP_COMPILER_MANIFEST.to_string(),
        "-c".to_string(),
        "Release".to_string(),
        "--nologo".to_string(),
    ];
    run_build_command(
        workspace_root,
        crate::config::CSHARP_COMPILER_ASSEMBLY_NAME,
        &build_command,
        &[],
        None,
        None,
    )?;
    if !output_path.exists() {
        return Err(BuildError::OutputMissing {
            path: output_path.display().to_string(),
        });
    }
    Ok(output_path)
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
    // A startup build is counted off against the announced plan; a reload
    // build is logged as such (the suites wait for that line).
    if !crate::build_progress::announce_build(&config.name) {
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = config.name.as_str(),
            "building project module"
        );
    }

    // Step 1: Resolve the backend-specific artifact paths.
    //
    // The build command itself is backend-agnostic, but each backend names and
    // locates its loadable artifact differently. Native outputs use platform
    // naming conventions; managed outputs always use an assembly `.dll`.
    //
    // A native project has two: the slot cargo writes into, and the private
    // copy the host loads from. They are kept apart for the reason extensions
    //  already keep them apart - any other `cargo build` of the same
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
    let (rlib_build_output, rlib_output) = (
        workspace_root
            .join(crate::config::module_build_artifact_directory())
            .join(format!("lib{}.rlib", config.name)),
        workspace_root
            .join(PROJECT_HOT_OUTPUT_SUBDIRECTORY)
            .join(format!("lib{}.rlib", config.name)),
    );

    // Step 2: Run cargo and let it decide what needs rebuilding. The host used
    // to keep its own freshness engine - modification-time stamps, a toolchain
    // marker, a recursive walk of every path dependency - which duplicated
    // cargo's fingerprint check and had to model features, targets and
    // environment overrides by hand. Cargo still owns freshness: every build
    // runs unless the startup batch already ran it (the token below), and a
    // fresh workspace answers in a few hundred milliseconds with `Finished`.

    // A managed build also captures the compiler command line MSBuild computed,
    // so a later hot reload can replay it in-process instead of paying for
    // MSBuild again. See `crate::csharp::fast_compile`.
    #[cfg(feature = "hot_reload")]
    let build_command = with_compiler_argument_capture(workspace_root, config);
    #[cfg(not(feature = "hot_reload"))]
    let build_command = config.build_command.clone();

    // The startup batch may already have built this project with these exact
    // flags (see [`build_extension_batch`]); the token covers that first load
    // only, so a later reload always builds. A managed project never joins the
    // batch - its `dotnet` build shares none of cargo's fixed cost - and this
    // token must not skip that build either.
    let batch_validated = matches!(&config.backend, ProjectModuleBackend::NativeLibrary { .. })
        && take_batch_validation(&config.name, &workspace_root.join(&config.watch_directory));
    if !batch_validated {
        run_build_command(
            workspace_root,
            &config.name,
            &build_command,
            &config.build_environment,
            Some(&[(config.name.as_str(), build_command.as_slice())]),
            cancel_flag,
        )?;
    }

    // Step 3: Stage the freshly built artifacts into the private hot-load
    // directory, so what the host loads is never the slot other builds write
    // to. The managed backend loads from where its build wrote, so its two
    // paths are the same and the copy is skipped.
    let mut stage_ms = 0.0;
    if matches!(&config.backend, ProjectModuleBackend::NativeLibrary { .. }) {
        let stage_started = Instant::now();
        // The project always produces an rlib, so a missing one is an error
        // rather than a crate that simply has none.
        let produced = stage_build_outputs(
            workspace_root,
            (&build_output, &output_path),
            Some((&rlib_build_output, &rlib_output)),
            true,
        )?;
        stage_ms = stage_started.elapsed().as_secs_f64() * 1000.0;

        // Stamp the staged copies so a later run can tell them apart from
        // anything another build writes to the same paths.
        record_artifact_stamp(
            workspace_root,
            &config.name,
            &config.watch_directory,
            &config.build_command,
            &produced,
        );

        // Then confirm the stamp still describes what is on disk: a compiler
        // orphaned by an earlier cancellation writes into these shared slots,
        // and what it writes must not be loaded as this build's output.
        confirm_staged_artifacts(
            workspace_root,
            &config.name,
            &config.watch_directory,
            &config.build_command,
            &produced,
        )?;
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
    // Skip a copy whose destination already is this build's output. A re-copy
    // renews the destination's timestamp, and on Windows opening a freshly
    // written DLL pays the platform's first-access scan - a cost every load of
    // an unchanged module would then pay again. Unchanged builds leave the
    // source's timestamps alone, so the steady state is a pair of `stat`s.
    if staged_copy_is_current(build_output, hot_output) {
        return Ok(());
    }
    // Replace through a private staging file and a rename. A previous
    // generation may still have the destination mapped - a load copy shares
    // its file - and Windows refuses to open a mapped image for writing, while
    // a rename replaces the directory entry and leaves the mapped file intact.
    let staged_output = hot_output.with_extension("staged");
    std::fs::copy(build_output, &staged_output).map_err(|source| {
        BuildError::HotArtifactCopyFailed {
            source_path: build_output.display().to_string(),
            target_path: hot_output.display().to_string(),
            source,
        }
    })?;
    std::fs::rename(&staged_output, hot_output).map_err(|source| {
        let _ = std::fs::remove_file(&staged_output);
        BuildError::HotArtifactCopyFailed {
            source_path: build_output.display().to_string(),
            target_path: hot_output.display().to_string(),
            source,
        }
    })?;
    Ok(())
}

/// Stage the module-world engine dylibs beside the hot-load copies.
///
/// Every native module and project imports `pill_core.dll` and
/// `pill_engine_core.dll`. The engine dylibs a host-spawned build produces (in
/// the private module build tree) can differ from the ones the host binary
/// itself maps from the regular target directory, since a GUI frontend unions
/// extra features onto shared crates; the loader then gives modules their
/// matching copies. When they are byte-identical (a plain CLI host) these
/// staged copies simply stay unused and the loader keeps the host's single
/// instances.
fn stage_engine_dylib(workspace_root: &Path) {
    for stem in crate::native_library::ENGINE_DYLIB_STEMS {
        let file_name = format!("{stem}.dll");
        let source = workspace_root
            .join(crate::config::module_build_artifact_directory())
            .join(&file_name);
        if !source.is_file() {
            continue;
        }
        let destination = workspace_root
            .join(PROJECT_HOT_OUTPUT_SUBDIRECTORY)
            .join(&file_name);
        // Copy only when the staged copy is stale. A re-copy renews the
        // destination's timestamp, and on Windows opening a freshly written
        // DLL pays the platform's first-access scan - a cost the isolation
        // check would then pay on every module's load. The source only
        // changes when a build produced a new engine, so the steady state is
        // a pair of `stat` calls.
        if staged_copy_is_current(&source, &destination) {
            continue;
        }
        if let Err(error) = std::fs::copy(source, destination) {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                error = %error,
                dylib = %file_name,
                "could not stage the module-world engine dylib into the hot-load directory"
            );
        }
    }
}

/// Copy everything one build produced into the host's private hot-load paths.
///
/// Both build paths end the same way, and that ending is where two real bugs
/// have already been found: the shared-slot collision that made the host load a
/// differently-featured artifact, and the missing dependency staging that made
/// the first patch after any external `cargo build` fail. Both fixes had to be
/// written twice because this sequence existed twice. It exists once now.
///
/// `rlib` is `None` for a crate that produces none. `require_rlib` says whether
/// a missing one is an error: the project always produces an rlib and a missing
/// one means something is wrong, while an extension declaring only a
/// `cdylib` legitimately has none and simply leaves the fast path idle.
///
/// Returns the staged paths, in the order a stamp should record them.
fn stage_build_outputs(
    workspace_root: &Path,
    library: (&Path, &Path),
    #[cfg_attr(not(feature = "hot_patch"), allow(unused_variables))] rlib: Option<(&Path, &Path)>,
    #[cfg_attr(not(feature = "hot_patch"), allow(unused_variables))] require_rlib: bool,
) -> Result<Vec<PathBuf>, BuildError> {
    let (build_output, hot_output) = library;
    stage_artifact(build_output, hot_output)?;

    // The build just produced a consistent set in `deps`; snapshot it before
    // anything else can write to those shared per-crate slots.
    #[cfg(feature = "hot_patch")]
    stage_shared_dependency_rlibs(workspace_root);

    #[cfg_attr(not(feature = "hot_patch"), allow(unused_mut))]
    let mut produced = vec![hot_output.to_path_buf()];

    #[cfg(feature = "hot_patch")]
    if let Some((rlib_build_output, rlib_output)) = rlib {
        if require_rlib || rlib_build_output.is_file() {
            stage_artifact(rlib_build_output, rlib_output)?;
            produced.push(rlib_output.to_path_buf());
        }
    }

    // Stage the module-world engine dylib beside the hot copies: modules
    // import `pill_core.dll`, and when the host runs a different engine
    // variant than this build produced the loader hands the module this
    // matching copy instead of the host's.
    stage_engine_dylib(workspace_root);

    Ok(produced)
}

/// The newest rlib cargo wrote for `package` into `build_directory`.
///
/// A wrapper build compiles the extension as a dependency, so its rlib sits
/// under the build's `deps` directory - metadata-hashed, or in the unhashed
/// workspace-member slot, whichever this cargo version uplifts - and some
/// configurations also leave a copy in the profile root. The host stages
/// whichever file was written last, which is the one the build that just ran
/// produced; a patch links it to reach the module's types. `None` when the
/// build wrote none, which leaves the per-function fast path idle for that
/// module.
pub(crate) fn newest_extension_rlib(build_directory: &Path, package: &str) -> Option<PathBuf> {
    let library_name = package.replace('-', "_");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut consider = |path: PathBuf| {
        let Ok(modified) = std::fs::metadata(&path).and_then(|metadata| metadata.modified()) else {
            return;
        };
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, path));
        }
    };
    if let Ok(entries) = std::fs::read_dir(build_directory.join("deps")) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let stem = file_name
                .strip_prefix("lib")
                .unwrap_or(file_name)
                .strip_suffix(".rlib")
                .unwrap_or(file_name);
            if stem == library_name || stem.starts_with(&format!("{library_name}-")) {
                consider(path);
            }
        }
    }
    consider(build_directory.join(format!("lib{library_name}.rlib")));
    newest.map(|(_, path)| path)
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
        .join(cargo_module_output_subdirectory())
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
        // Generated member crates write unhashed rlibs too, but nothing ever
        // links them: a wrapper's rlib is not a patch target, and a generated
        // project member's rlib is rebuilt by every project build anyway.
        if is_generated_member_rlib(file_name) {
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
/// another build can silently replace. The patch linker (`hot_patch::compile`)
/// and the dependency staging below must agree about which files are shared,
/// or a file is staged and never linked, or linked and never staged - so this
/// is the single implementation both call.
#[cfg(feature = "hot_patch")]
pub(crate) fn is_shared_slot_rlib(file_name: &str) -> bool {
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

/// Whether a `deps` filename is the rlib of a host-generated member crate.
///
/// Their crate names all carry a generated prefix. Nothing links the rlib of
/// a wrapper or of a generated project member, so the dependency staging skips
/// them instead of snapshotting files no patch can use.
#[cfg(feature = "hot_patch")]
fn is_generated_member_rlib(file_name: &str) -> bool {
    let stem = file_name.strip_suffix(".rlib").unwrap_or(file_name);
    let stem = stem.strip_prefix("lib").unwrap_or(stem);
    stem.starts_with(crate::config::HOST_PROJECT_MEMBER_PREFIX)
        || stem.starts_with(crate::config::HOST_MODULE_MEMBER_PREFIX)
}

/// Whether a staged copy already matches its source.
///
/// Same size and no older. Used by the shared-rlib staging, the engine dylib
/// staging and the patch session's rlib refresh, so a copy either path leaves
/// behind satisfies the other; anything unknown counts as out of date, so the
/// copy happens.
pub(crate) fn staged_copy_is_current(source: &Path, staged: &Path) -> bool {
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

/// What one startup batch invocation validated, and when it finished.
#[derive(Debug)]
struct BatchValidation {
    /// When the batch invocation returned successfully.
    ///
    /// A token is only honoured while no file under the module's watch
    /// directory is newer than this instant: the batch validated exactly the
    /// sources cargo saw, and a save after it must rebuild.
    completed_at: SystemTime,
    /// Module names whose artifacts the batch built or confirmed.
    names: std::collections::HashSet<String>,
}

/// One startup batch invocation's validation, consumed per module.
///
/// One batch runs per process, before anything loads; each name is consumed
/// by its startup load, so a later reload always builds on its own.
static BATCH_VALIDATION: OnceLock<std::sync::Mutex<Option<BatchValidation>>> = OnceLock::new();

/// Run `callback` with exclusive access to the batch validation state.
fn with_batch_validation<T>(callback: impl FnOnce(&mut Option<BatchValidation>) -> T) -> T {
    callback(
        &mut BATCH_VALIDATION
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

/// Build every module of the startup plan in one cargo invocation.
///
/// Every module build otherwise pays cargo's fixed cost - process start,
/// workspace resolve, fingerprint scan - once per module, and that cost grows
/// with the workspace, so a project with hundreds of modules would restart
/// cargo hundreds of times per start. The extensions' wrappers, the native
/// project's member and the renderer's wrapper (windowed postures only)
/// differ only in `--package`, so one invocation selects them all; its flags
/// mirror a single module's build exactly, which keeps every unit's
/// fingerprints identical to a per-module build's. The modules it validated
/// skip their own build when they load (see [`take_batch_validation`]); an
/// edit later still rebuilds a module on its own, and the first patch of a
/// module the batch recompiled re-captures its compiler flags on demand.
///
/// A managed (C#) project is never selected: its `dotnet` build shares none
/// of cargo's fixed cost. The renderer's wrapper joins the batch because it
/// is an ordinary wrapper build; only its load waits for the window (see
/// `crate::renderer_module`), and a rebuild request drops its token so a
/// reloaded data crate still forces a compile.
///
/// Feature unification is the one difference from per-module builds: a
/// selected package that depends on another selected package would receive
/// the union of both feature requests. Independent modules - the common case,
/// and the case in every project this workspace ships - see no union at all.
///
/// Returns whether the batch ran and validated the modules. A failure is not
/// an error: the modules then build individually, which keeps per-module
/// failure reporting exactly as it was.
pub(crate) fn build_extension_batch(
    workspace_root: &Path,
    configs: &[ExtensionConfig],
    project: Option<&ProjectModuleConfig>,
    renderer: Option<&str>,
) -> bool {
    // Only a native project shares cargo's fixed cost; a managed build's
    // `dotnet` invocation is its own machinery, and selecting its member here
    // would fail on a missing cargo package.
    let project_package = project.and_then(|project| match &project.backend {
        ProjectModuleBackend::NativeLibrary { .. } => Some(format!(
            "{}{}",
            crate::config::HOST_PROJECT_MEMBER_PREFIX,
            project.name
        )),
        ProjectModuleBackend::CSharp(_) => None,
    });
    // The renderer's wrapper is built exactly like an extension's.
    // Constructed from the name alone, which is all `RendererModule` needs to
    // load it.
    let renderer_config = renderer.map(ExtensionConfig::workspace_member);
    let total = configs.len()
        + usize::from(project_package.is_some())
        + usize::from(renderer_config.is_some());
    // One package has no fixed cost to share; its own step builds it.
    if total < 2 {
        return false;
    }

    let mut packages: Vec<String> = configs
        .iter()
        .map(|config| config.wrapper_library_name.clone())
        .collect();
    if let Some(package) = &project_package {
        packages.push(package.clone());
    }
    if let Some(config) = &renderer_config {
        packages.push(config.wrapper_library_name.clone());
    }
    let command = crate::config::host_cargo_command(&packages);
    // One capture entry per selected package, so the patch pipeline harvests
    // every module's `rustc` line from this single invocation.
    let mut entries: Vec<(&str, &[String])> = configs
        .iter()
        .map(|config| (config.name.as_str(), config.build_command.as_slice()))
        .collect();
    if let (Some(project), Some(_)) = (project, project_package.as_ref()) {
        entries.push((project.name.as_str(), project.build_command.as_slice()));
    }
    if let Some(config) = &renderer_config {
        entries.push((config.name.as_str(), config.build_command.as_slice()));
    }
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        modules = total,
        "building every module in one cargo invocation"
    );
    match run_build_command(
        workspace_root,
        "startup batch",
        &command,
        &crate::config::spawned_build_environment(),
        Some(&entries),
        None,
    ) {
        Ok(()) => {
            let mut validation = BatchValidation {
                completed_at: SystemTime::now(),
                names: Default::default(),
            };
            for config in configs {
                validation.names.insert(config.name.clone());
            }
            if let (Some(project), Some(_)) = (project, project_package.as_ref()) {
                validation.names.insert(project.name.clone());
            }
            if let Some(config) = &renderer_config {
                validation.names.insert(config.name.clone());
            }
            with_batch_validation(|state| *state = Some(validation));
            true
        }
        Err(error) => {
            warn!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                error = %error,
                "the startup batch build failed; each module builds on its own"
            );
            false
        }
    }
}

/// Whether the startup batch already validated `name`'s artifact.
///
/// Consumed per module: the token belongs to the startup load, so a reload of
/// the same module builds on its own. Any file under `watch_directory` newer
/// than the batch also invalidates it - the batch validated exactly the
/// sources cargo saw, and a save after it must rebuild rather than load stale
/// code. The window between the batch and a load is largest for the renderer,
/// which loads when the window opens.
fn take_batch_validation(name: &str, watch_directory: &Path) -> bool {
    with_batch_validation(|state| {
        let Some(validation) = state.as_mut() else {
            return false;
        };
        if !validation.names.remove(name) {
            return false;
        }
        if source_newer_than(watch_directory, validation.completed_at) {
            pill_core::debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                module = name,
                "source changed after the startup batch; the module builds on its own"
            );
            return false;
        }
        true
    })
}

/// Drop `name`'s batch token, so its next load builds instead of skipping.
///
/// The renderer's rebuild request uses this: a reloaded data crate makes the
/// artifact the batch built stale even though no renderer source changed.
#[cfg(all(feature = "rendering", feature = "hot_reload"))]
pub(crate) fn forget_batch_validation(name: &str) {
    with_batch_validation(|state| {
        if let Some(validation) = state.as_mut() {
            validation.names.remove(name);
        }
    });
}

/// Whether any entry under `directory` was modified after `time`.
///
/// Watch directories hold module sources - small trees - so the walk is cheap
/// per load. Anything unreadable counts as newer, which costs one build and
/// can never load stale code.
fn source_newer_than(directory: &Path, time: SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return true;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return true;
        };
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            return true;
        };
        let was_modified = metadata
            .modified()
            .ok()
            .is_none_or(|modified| modified > time);
        if metadata.is_dir() {
            if was_modified || source_newer_than(&path, time) {
                return true;
            }
            continue;
        }
        if was_modified {
            return true;
        }
    }
    false
}

/// Build one extension and return its expected output artifact.
///
/// Extensions are workspace members, so their output always follows the
/// platform's native-library naming inside the configured output directory.
///
/// # Errors
///
/// Returns an error if the build fails for any of the reasons reported by
/// [`run_build_command`], or if the built library is missing afterwards.
pub(crate) fn build_extension(
    workspace_root: &Path,
    config: &ExtensionConfig,
    cancel_flag: Option<(&AtomicU64, u64)>,
) -> Result<PathBuf, BuildError> {
    // A startup build is counted off against the announced plan; a reload
    // build is logged as such (the suites wait for that line).
    if !crate::build_progress::announce_build(&config.name) {
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            module = config.name.as_str(),
            "building extension"
        );
    }

    // Cargo writes the freshly compiled cdylib into the shared per-crate
    // output slot, while the host loads from the private hot-load copy.
    // Keeping these paths distinct is what resolves the "one crate name, two
    // feature sets" collision: a project build may overwrite the shared slot
    // with the extension's export-stripped dependency variant, but the loaded
    // generation always comes from the untouched hot copy.
    //
    // The built crate is the generated wrapper; the staged artifact carries
    // the extension's own name, which is how the host addresses the module.
    let build_output = workspace_root
        .join(cargo_module_output_subdirectory())
        .join(native_library_filename(&config.wrapper_library_name));
    let hot_output = workspace_root
        .join(&config.output_subdirectory)
        .join(native_library_filename(&config.library_name));

    // The extension's `rlib`, staged for the same reason the project's is: a
    // generated patch links it to reach the module's types. The wrapper build
    // compiles the extension as a dependency, so its rlib sits under cargo's
    // hashed (or workspace-uplifted) name in the build's `deps` directory;
    // the newest one is from the build that just ran.
    let rlib_build_output = newest_extension_rlib(
        &workspace_root.join(cargo_module_output_subdirectory()),
        &config.name,
    );
    let rlib_output = workspace_root
        .join(&config.output_subdirectory)
        .join(format!("lib{}.rlib", config.name));

    // Extensions carry no per-module environment of their own, but they
    // need the same profile-driven `RUSTFLAGS` handling the project gets: an
    // optimized build must not inherit `-C prefer-dynamic`.
    //
    // One batch invocation may already have built this module with these exact
    // flags (see [`build_extension_batch`]). The token covers that one startup
    // load only: a reload finds no token and always builds on its own, and a
    // save newer than the batch invalidates it.
    if !take_batch_validation(&config.name, &workspace_root.join(&config.watch_directory)) {
        run_build_command(
            workspace_root,
            &config.name,
            &config.build_command,
            &crate::config::spawned_build_environment(),
            Some(&[(config.name.as_str(), config.build_command.as_slice())]),
            cancel_flag,
        )?;
    }

    // Stage the freshly built wrapper library into the hot-load directory,
    // under the extension's name. The shared per-crate slot may later hold a
    // differently featured copy of the extension's dependency rlib, which is
    // exactly why the loadable artifact lives apart from it.
    //
    // The extension's rlib is staged beside it for the per-function fast
    // path; a build that produced none is not an error - it just leaves the
    // fast path idle for that module.
    let stage_started = Instant::now();
    let produced = stage_build_outputs(
        workspace_root,
        (&build_output, &hot_output),
        rlib_build_output
            .as_deref()
            .map(|built| (built, rlib_output.as_path())),
        false,
    )?;
    let stage_ms = stage_started.elapsed().as_secs_f64() * 1000.0;

    // Stamp the staged copies so a later run can tell them apart from anything
    // another build writes to the same paths.
    record_artifact_stamp(
        workspace_root,
        &config.name,
        &config.watch_directory,
        &config.build_command,
        &produced,
    );

    // Same recheck as the project path: the staged copy is only loadable if it
    // is still the one this build wrote.
    confirm_staged_artifacts(
        workspace_root,
        &config.name,
        &config.watch_directory,
        &config.build_command,
        &produced,
    )?;

    if !hot_output.exists() {
        return Err(BuildError::OutputMissing {
            path: hot_output.display().to_string(),
        });
    }
    analytics::record_module_artifact(
        &config.name,
        ModuleKind::Extension,
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

    /// The token guard: a file written after the batch invalidates the token,
    /// one written before it does not, and a missing directory counts as
    /// changed.
    #[test]
    fn source_newer_than_watches_file_times() {
        let directory =
            std::env::temp_dir().join(format!("pill_batch_guard_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("nested")).expect("create the test directory");
        let before_write = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(directory.join("nested").join("edited.rs"), "// edited")
            .expect("write the test file");
        assert!(
            source_newer_than(&directory, before_write),
            "a file written after the batch must invalidate the token"
        );
        let after_write = SystemTime::now();
        assert!(
            !source_newer_than(&directory, after_write),
            "files older than the batch keep the token valid"
        );
        assert!(
            source_newer_than(&directory.join("missing"), after_write),
            "a missing watch directory counts as changed"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The token itself: refused when the sources moved on, consumed once.
    #[test]
    fn batch_validation_is_consumed_once() {
        let directory =
            std::env::temp_dir().join(format!("pill_batch_token_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create the test directory");
        let stale_moment = SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(directory.join("lib.rs"), "// module").expect("write the test file");
        with_batch_validation(|state| {
            *state = Some(BatchValidation {
                completed_at: stale_moment,
                names: ["batch_guard_expiring".to_string()].into_iter().collect(),
            });
        });
        assert!(
            !take_batch_validation("batch_guard_expiring", &directory),
            "a source newer than the batch must force the module's own build"
        );
        with_batch_validation(|state| {
            *state = Some(BatchValidation {
                completed_at: SystemTime::now(),
                names: ["batch_guard_once".to_string()].into_iter().collect(),
            });
        });
        assert!(take_batch_validation("batch_guard_once", &directory));
        assert!(
            !take_batch_validation("batch_guard_once", &directory),
            "the token belongs to one load only"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

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

        record_artifact_stamp(
            &root,
            "module",
            "module/src",
            &command,
            std::slice::from_ref(&output),
        );
        assert!(
            artifacts_are_host_built(
                &root,
                "module",
                "module/src",
                &command,
                std::slice::from_ref(&output)
            ),
            "the host's own artifact must be recognized"
        );

        // Another build writes a different library to the same path. It is
        // newer than every source, so the modification-time check alone would
        // accept it.
        std::thread::sleep(Duration::from_millis(30));
        fs_write(&output, "a differently configured native library");
        assert!(
            !artifacts_are_host_built(
                &root,
                "module",
                "module/src",
                &command,
                std::slice::from_ref(&output)
            ),
            "an artifact the host did not write must be rebuilt, not loaded"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The same artifact built under a different host identity is a different
    /// artifact, which is what makes the check feature-aware PER MODULE.
    ///
    /// The workspace-wide build-info marker cannot answer this. It is rewritten
    /// by the first module that rebuilds after a host feature change, so every
    /// module checked after that one compares equal and skips its build while
    /// still holding an artifact built against the engine variant that rebuild
    /// just replaced - which loads as os error 127 and names nothing.
    #[test]
    fn a_different_host_identity_is_a_different_stamp() {
        let (root, _) = test_workspace();
        let output = root.join("target/debug/module.dll");
        let command = vec!["cargo".to_string(), "build".to_string()];
        let artifacts = std::slice::from_ref(&output);

        let windowed = artifact_stamp(
            "rustc 1.95.0
rendering+hot_patch",
            "module/src",
            &command,
            artifacts,
        )
        .expect("the artifact exists");
        let headless = artifact_stamp(
            "rustc 1.95.0
no-rendering+hot_patch",
            "module/src",
            &command,
            artifacts,
        )
        .expect("the artifact exists");

        assert_ne!(
            windowed, headless,
            "a host feature change must not leave a module's stamp unchanged"
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

        record_artifact_stamp(
            &root,
            "module",
            "module/src",
            &with_feature,
            std::slice::from_ref(&output),
        );
        assert!(!artifacts_are_host_built(
            &root,
            "module",
            "module/src",
            &without_feature,
            std::slice::from_ref(&output)
        ));

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Two projects can share a package name, and therefore an output path, a
    /// stamp file and a build command. Only the watched sources differ.
    ///
    /// This is not hypothetical: `examples/project_rs` and the test fixture at
    /// `devops/tests/project` are both package `project`, both build
    /// `target/debug/project.dll`, and both are launched by
    /// `cargo build --package project`. Running one after the other left the
    /// second host loading the first project's DLL and reporting it as up to
    /// date, because the artifact genuinely was newer than every source it
    /// checked - they were just the wrong sources.
    #[test]
    fn a_different_project_with_the_same_package_name_is_not_host_built() {
        let root = std::env::temp_dir().join("pill_stamp_other_project");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let output = root.join("project.dll");
        std::fs::write(&output, b"artifact").unwrap();
        let command = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--package".to_string(),
            "project".to_string(),
        ];

        record_artifact_stamp(
            &root,
            "project",
            "../examples/project_rs/src",
            &command,
            std::slice::from_ref(&output),
        );

        assert!(
            artifacts_are_host_built(
                &root,
                "project",
                "../examples/project_rs/src",
                &command,
                std::slice::from_ref(&output)
            ),
            "the project that produced the artifact must still recognise it"
        );
        assert!(
            !artifacts_are_host_built(
                &root,
                "project",
                "../devops/tests/project/src",
                &command,
                std::slice::from_ref(&output)
            ),
            "a different project must not adopt an artifact it did not build"
        );

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
            !artifacts_are_host_built(
                &root,
                "module",
                "module/src",
                &command,
                std::slice::from_ref(&output)
            ),
            "no stamp has been recorded yet"
        );

        record_artifact_stamp(
            &root,
            "module",
            "module/src",
            &command,
            std::slice::from_ref(&output),
        );
        std::fs::remove_file(&output).unwrap();
        assert!(
            !artifacts_are_host_built(
                &root,
                "module",
                "module/src",
                &command,
                std::slice::from_ref(&output)
            ),
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

        record_artifact_stamp(&root, "module", "module/src", &command, &artifacts);
        assert!(artifacts_are_host_built(
            &root,
            "module",
            "module/src",
            &command,
            &artifacts
        ));

        // Only the second artifact moves.
        std::thread::sleep(Duration::from_millis(30));
        fs_write(&rlib, "a differently configured rlib");
        assert!(
            !artifacts_are_host_built(&root, "module", "module/src", &command, &artifacts),
            "a replaced rlib must invalidate the build as surely as a replaced library"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A cancelled build must stop its whole tree, not just cargo.
    ///
    /// Cargo is the direct child, but the compiler and linker it starts are
    /// grandchildren: killing the child leaves them running against the same
    /// shared build tree, where they hold cargo's package lock - the next
    /// build then blocks until its timeout and reads as an ignored edit - and
    /// can write over an artifact between staging and loading. The job object
    /// is what makes the tree the unit of termination, so this test drives one
    /// through the same helpers `run_build_command` uses.
    #[cfg(windows)]
    #[test]
    fn cancelled_build_kills_the_whole_tree() {
        // `cmd /C` starts a long-running grandchild and waits for it: one
        // direct child with one process below, the shape of a cargo build.
        let mut command = Command::new("cmd");
        command
            .args(["/C", "ping", "-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        let tree = create_process_tree();
        assert!(tree.is_some(), "a job object must be creatable here");
        let mut child = command.spawn().expect("cmd must spawn");
        join_process_tree(tree.as_ref(), &child);

        // Wait for the grandchild to join the job. That inheritance is the
        // property under test, so a tree of one process would prove nothing.
        let deadline = Instant::now() + Duration::from_secs(10);
        while tree.as_ref().expect("created above").active_processes() < 2 {
            assert!(
                Instant::now() < deadline,
                "the grandchild must join the job"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        stop_process_tree(tree.as_ref(), &mut child);

        let deadline = Instant::now() + Duration::from_secs(10);
        while tree.as_ref().expect("created above").active_processes() != 0 {
            assert!(Instant::now() < deadline, "the tree must be gone");
            std::thread::sleep(Duration::from_millis(10));
        }
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

    // -------------------------------------------------------------------------
    // Piggybacked flag capture
    // -------------------------------------------------------------------------

    /// The two line kinds `-v` adds are recognised with and without colour.
    ///
    /// Cargo colours its status words whenever stderr is a terminal, which is
    /// exactly when a human is reading the console - so a filter that only
    /// matched the uncoloured spelling would dump every multi-kilobyte `rustc`
    /// command line in front of the person it was meant to spare.
    #[cfg(feature = "hot_patch")]
    #[test]
    fn verbose_only_lines_are_recognised_with_and_without_colour() {
        for line in [
            "     Running `rustc --crate-name project`\n",
            "\u{1b}[0m\u{1b}[1m\u{1b}[32m     Running\u{1b}[0m `rustc --crate-name project`\n",
            "       Fresh pill_core v0.1.0\n",
            "\u{1b}[1m\u{1b}[32m       Fresh\u{1b}[0m pill_core v0.1.0\n",
            "       Dirty pill_spline v0.1.0: the list of features changed\n",
            "\u{1b}[1m\u{1b}[32m       Dirty\u{1b}[0m pill_spline v0.1.0: the file changed\n",
        ] {
            assert!(is_verbose_only_line(line), "must be filtered: {line:?}");
        }
        for line in [
            "   Compiling pill_core v0.1.0\n",
            "\u{1b}[1m\u{1b}[32m   Compiling\u{1b}[0m pill_core v0.1.0\n",
            "error[E0433]: failed to resolve\n",
            "warning: unused variable `x`\n",
            "\n",
        ] {
            assert!(
                !is_verbose_only_line(line),
                "must reach the console: {line:?}"
            );
        }
    }

    /// The module crate's invocation is picked out of cargo's verbose stream,
    /// and every other crate's is ignored - the patch must replay the flags of
    /// the crate it is patching, not of whatever else the build recompiled.
    #[cfg(feature = "hot_patch")]
    #[test]
    fn the_harvest_picks_only_the_module_crate() {
        let other = r"     Running `C:\rustc.exe --crate-name pill_core --edition=2021 x.rs`";
        let wanted = r"     Running `C:\rustc.exe --crate-name project --edition=2021 y.rs`";
        assert!(crate::hot_patch::parse_rustc_line(other, "project").is_none());
        assert!(
            crate::hot_patch::parse_rustc_line("   Compiling project v0.1.0", "project").is_none()
        );
        let line = crate::hot_patch::parse_rustc_line(wanted, "project")
            .expect("the module crate's invocation");
        assert_eq!(line.program, r"C:\rustc.exe");
        assert!(line.args.contains(&"--edition=2021".to_string()));
    }
}
