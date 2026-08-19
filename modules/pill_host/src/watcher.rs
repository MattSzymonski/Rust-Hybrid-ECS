//! Watch source trees and staged artifacts, signalling reloads to the main
//! thread.
//!
//! # Responsibilities
//!
//! - Spawn worker threads that monitor source trees for changes.
//! - Classify file events and paths so only real source edits trigger reloads.
//! - Bump a reload generation counter when relevant file events are detected,
//!   letting the main thread perform a hot reload.
//! - Track the newest engine runtime dylib staged by any process, so an
//!   externally produced build is picked up without a rebuild.
//! - Report which files changed so reloads are debuggable.
//! - Handle cross-platform file notification differences through `notify`.
//!
//! # Design
//!
//! Each watcher runs in its own thread to avoid blocking the main loop. A
//! debounce window coalesces multiple file events into a single reload signal,
//! and the changed paths collected during that window are reported to the
//! console. The main thread compares each generation counter against the last
//! processed value every frame, so events arriving during a reload are never
//! lost.
//!
//! Reload signals are deliberately split across independent counters:
//!
//! | Counter | Watches | Reaction |
//! |---|---|---|
//! | project | the project's source tree | fast, state-preserving project reload |
//! | engine | `pill_engine`, `pill_runtime`, `pill_runtime_api` sources | full engine runtime swap |
//! | staged runtime | the staged runtime directory | adopt a dylib another process built |
//! | shared core | `pill_core` sources | a restart notice, never a reload |
//!
//! `pill_core` is watched but never reloaded: both the host and the runtime
//! link it, so changing it invalidates the running host binary too. Reporting
//! that as a restart notice is more useful than silently running stale code.
//!
//! The staged-runtime watcher reports the highest generation index it has seen
//! rather than a change count. The host stages its own builds with a
//! monotonically increasing index, so it can tell its own artifact apart from
//! one an external `cargo build` produced and never reloads in response to
//! itself.

// Standard library
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// External crates
use notify::event::EventKind;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use pill_core::error::WatcherError;
use pill_core::hot_reload::{
    native_library_extension, parse_runtime_staged_generation, runtime_staging_directory,
};
use pill_core::{debug, error, info};

// Current crate
use crate::ProjectModuleConfig;

// =============================================================================
// Constants
// =============================================================================

/// File events arriving within this window are coalesced into one reload.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(300);

/// Directory names that never contain source code worth rebuilding for.
const IGNORED_DIRECTORY_NAMES: [&str; 3] = ["target", "bin", "obj"];

/// Suffixes editors use for temporary and swap files written around saves.
const EDITOR_TEMPORARY_FILE_SUFFIXES: [&str; 3] = ["~", ".swp", ".swx"];

/// Prefix shared by hidden files, which never contain source code.
const HIDDEN_FILE_PREFIX: &str = ".";

/// Maximum number of changed paths printed per reload report.
const REPORTED_PATH_LIMIT: usize = 5;

/// Workspace-relative source trees whose changes trigger an engine reload.
const ENGINE_SOURCE_DIRECTORIES: [&str; 3] = [
    "pill_engine/src",
    "pill_runtime/src",
    "pill_runtime_api/src",
];

/// Workspace-relative source tree whose changes require a host restart.
const SHARED_CORE_SOURCE_DIRECTORY: &str = "pill_core/src";

// =============================================================================
// Types
// =============================================================================

/// Every reload signal the host's frame loop observes.
///
/// Worker threads only publish into these counters with `Release` ordering and
/// the main thread only reads them with `Acquire`, so no signal can be
/// observed before the file events that produced it.
#[derive(Debug, Default)]
pub(crate) struct ReloadSignals {
    /// Bumped when the project's own sources change.
    pub(crate) project: Arc<AtomicU64>,
    /// Bumped when engine or runtime sources change.
    pub(crate) engine: Arc<AtomicU64>,
    /// Highest staged runtime generation index seen on disk.
    pub(crate) staged_runtime: Arc<AtomicU64>,
    /// Bumped when the shared core changes, which needs a host restart.
    pub(crate) shared_core: Arc<AtomicU64>,
}

// =============================================================================
// Free Functions
// =============================================================================

/// Whether an event kind represents a source change worth rebuilding for.
///
/// Content changes, file creations, and file deletions all matter. Renames
/// surface as a remove/create pair or a name modification depending on the
/// platform, so they are covered by the same three kinds. Access and
/// metadata-only events are ignored.
fn is_relevant_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Whether a changed absolute path should trigger a rebuild.
///
/// Both sides are canonicalized so symlinked directories cannot smuggle
/// events from outside the watch tree, and the remaining policy is applied
/// to the resulting relative path.
fn is_relevant_path(path: &Path, watch_root: &Path) -> bool {
    let Ok(canonical_root) = watch_root.canonicalize() else {
        return false;
    };
    let Ok(canonical_path) = path.canonicalize() else {
        // Events for files deleted between notification and check carry no
        // source content; skipping them is harmless.
        return false;
    };
    if !canonical_path.starts_with(&canonical_root) {
        return false;
    }
    // The `starts_with` check above already verified the prefix, so this
    // strip cannot fail.
    let relative = canonical_path.strip_prefix(&canonical_root).unwrap();
    is_relevant_relative_path(relative)
}

/// Whether a path relative to the canonical watch root should trigger a
/// rebuild.
///
/// Build output directories are ignored only at the top level of the watch
/// tree; deeper directories may legitimately use the same names. Hidden
/// files and directories and editor temporary files are excluded at any
/// depth.
fn is_relevant_relative_path(relative: &Path) -> bool {
    // Build outputs are anchored to the watch root: `target/...` directly
    // beneath it is compiler output, while `src/target_logic/` is source.
    if let Some(first) = relative.components().next() {
        if let Some(name) = first.as_os_str().to_str() {
            if IGNORED_DIRECTORY_NAMES.contains(&name) {
                return false;
            }
        }
    }

    // Hidden entries are excluded at any depth.
    for component in relative.components() {
        if let Some(name) = component.as_os_str().to_str() {
            if name.starts_with(HIDDEN_FILE_PREFIX) {
                return false;
            }
        }
    }

    // Editor temporary files appear and disappear around every save and
    // would otherwise trigger a rebuild of half-written content. Non-UTF-8
    // file names are treated as relevant: they cannot match any ignore
    // suffix, and filtering them out would silently stop hot reload.
    match relative.file_name().and_then(|name| name.to_str()) {
        Some(name) => !EDITOR_TEMPORARY_FILE_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix)),
        None => true,
    }
}

/// Start every watcher the host needs and return their shared signals.
///
/// Missing engine or shared-core directories are reported and skipped rather
/// than treated as fatal: a packaged host may ship without engine sources, and
/// only its engine reload capability is lost. A missing *project* watch
/// directory is still fatal, because it means the configuration is wrong.
///
/// # Errors
///
/// Returns an error if the project watch directory does not exist, or if any
/// watcher cannot be created or registered.
pub(crate) fn spawn_all_watchers(
    workspace_root: &Path,
    config: &ProjectModuleConfig,
) -> Result<ReloadSignals, WatcherError> {
    let signals = ReloadSignals::default();

    // Step 1: The project's own sources drive the fast, state-preserving path.
    let project_watch_path = workspace_root.join(&config.watch_directory);
    if !project_watch_path.exists() {
        return Err(WatcherError::WatchDirectoryMissing {
            path: project_watch_path.display().to_string(),
        });
    }
    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = config.name.as_str(),
        watch_directory = %project_watch_path.display(),
        "watching for project source changes"
    );
    spawn_source_watcher(
        "project",
        vec![project_watch_path],
        Arc::clone(&signals.project),
    )?;

    // Step 2: Engine and runtime sources drive the full runtime swap.
    let engine_watch_paths: Vec<PathBuf> = ENGINE_SOURCE_DIRECTORIES
        .iter()
        .map(|directory| workspace_root.join(directory))
        .filter(|path| path.exists())
        .collect();
    if engine_watch_paths.is_empty() {
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            "no engine sources found in this workspace; engine hot reload is disabled"
        );
    } else {
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            directories = engine_watch_paths.len(),
            "watching for engine source changes"
        );
        spawn_source_watcher("engine", engine_watch_paths, Arc::clone(&signals.engine))?;
    }

    // Step 3: The shared core is watched only to report that a restart is due.
    let shared_core_path = workspace_root.join(SHARED_CORE_SOURCE_DIRECTORY);
    if shared_core_path.exists() {
        spawn_source_watcher(
            "shared core",
            vec![shared_core_path],
            Arc::clone(&signals.shared_core),
        )?;
    }

    // Step 4: The staging directory lets an externally built runtime be
    // adopted without this host rebuilding it first.
    spawn_runtime_staging_watcher(workspace_root, Arc::clone(&signals.staged_runtime))?;

    Ok(signals)
}

/// Watch one or more source trees and bump a generation counter on changes.
///
/// # Errors
///
/// Returns an error if the watcher cannot be created or a path cannot be
/// registered.
fn spawn_source_watcher(
    label: &'static str,
    watch_paths: Vec<PathBuf>,
    reload_generation: Arc<AtomicU64>,
) -> Result<(), WatcherError> {
    // Step 1: Create the watcher with a minimal callback that forwards
    // relevant paths to a debounce channel.
    let callback_roots = watch_paths.clone();
    let (sender, receiver) = std::sync::mpsc::channel::<PathBuf>();
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| match result {
            Ok(event) => {
                if is_relevant_event(&event.kind) {
                    for path in event.paths {
                        if callback_roots
                            .iter()
                            .any(|root| is_relevant_path(&path, root))
                        {
                            // Failure only means the receiving thread has
                            // shut down, so there is no recovery work for the
                            // callback to perform.
                            let _ = sender.send(path);
                        }
                    }
                }
            }
            // Watching must never panic inside the callback, which some
            // backends run on their own threads; report and continue.
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    watcher = label,
                    error = %error,
                    "file watcher error"
                );
            }
        },
        Config::default(),
    )
    .map_err(|source| WatcherError::CreationFailed { source })?;

    // Recursive watching covers nested source modules without requiring every
    // language backend to enumerate its own directory structure.
    for watch_path in &watch_paths {
        watcher
            .watch(watch_path, RecursiveMode::Recursive)
            .map_err(|source| WatcherError::RegistrationFailed {
                path: watch_path.display().to_string(),
                source,
            })?;
    }

    // Step 2: Run the debounce worker that reports changes and signals the
    // main loop in the host.
    std::thread::spawn(move || {
        // RecommendedWatcher unregisters its OS handles when dropped.
        // Move it into the worker even though the loop never calls it directly,
        // keeping those handles alive for exactly as long as the receiver remains live.
        let _watcher = watcher;

        // Block without consuming CPU until an event starts a debounce window.
        while let Ok(first_path) = receiver.recv() {
            // One source save can produce several notifications for the same file.
            // Wait for the burst to settle, deduplicate all signals in a set,
            // and report the trigger before signalling the main loop in the host.
            let mut changed_paths = HashSet::from([first_path]);
            std::thread::sleep(DEBOUNCE_DURATION);
            while let Ok(path) = receiver.try_recv() {
                changed_paths.insert(path);
            }

            debug!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                watcher = label,
                changed_paths = %summarize_changed_paths(&changed_paths, &watch_paths),
                "source change detected"
            );

            // Bump the reload generation. The main loop compares it against
            // the last processed value, so signals are never overwritten.
            // Release publishing makes every earlier write on this thread
            // visible to the Acquire read on the frame loop.
            reload_generation.fetch_add(1, Ordering::Release);
        }
    });

    Ok(())
}

/// Watch the runtime staging directory and publish the newest generation seen.
///
/// The published value is the highest staged index on disk rather than a
/// change count, which is what lets the host distinguish its own staged
/// artifact from one another process produced: it knows the index it wrote
/// last, so a strictly higher value is the only signal worth reacting to.
///
/// # Errors
///
/// Returns an error if the watcher cannot be created or the staging directory
/// cannot be registered.
fn spawn_runtime_staging_watcher(
    workspace_root: &Path,
    staged_generation: Arc<AtomicU64>,
) -> Result<(), WatcherError> {
    // Step 1: The directory must exist before it can be watched, and this
    // process owns it, so creating it here is the natural place.
    let staging_directory = runtime_staging_directory(workspace_root);
    if let Err(error) = std::fs::create_dir_all(&staging_directory) {
        // Without the directory the host simply loses external-build adoption;
        // its own staging path reports the same failure with a typed error.
        info!(
            target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
            path = %staging_directory.display(),
            error = %error,
            "could not prepare the runtime staging directory; external runtime builds will not be adopted"
        );
        return Ok(());
    }
    publish_highest_staged_generation(&staging_directory, &staged_generation);

    // Step 2: Only the platform's dynamic-library files matter; debug symbols
    // and partial writes land here too and must not be mistaken for a build.
    let extension = native_library_extension();
    let scan_directory = staging_directory.clone();
    let scan_generation = Arc::clone(&staged_generation);
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| match result {
            Ok(event) => {
                if !is_relevant_event(&event.kind) {
                    return;
                }
                let names_a_dylib = event.paths.iter().any(|path| {
                    path.extension().and_then(|value| value.to_str()) == Some(extension)
                });
                if names_a_dylib {
                    publish_highest_staged_generation(&scan_directory, &scan_generation);
                }
            }
            Err(error) => {
                error!(
                    target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                    error = %error,
                    "runtime staging watcher error"
                );
            }
        },
        Config::default(),
    )
    .map_err(|source| WatcherError::CreationFailed { source })?;

    watcher
        .watch(&staging_directory, RecursiveMode::NonRecursive)
        .map_err(|source| WatcherError::RegistrationFailed {
            path: staging_directory.display().to_string(),
            source,
        })?;

    // Step 3: Keep the watcher alive for the process lifetime. The staging
    // watcher owns no channel, so its thread only parks on the handle.
    std::thread::spawn(move || {
        let _watcher = watcher;
        loop {
            std::thread::park();
        }
    });

    Ok(())
}

/// Scan the staging directory and publish the highest generation index found.
///
/// The value only ever moves forward: a deleted newer file must not make the
/// host believe an older generation is current.
fn publish_highest_staged_generation(staging_directory: &Path, staged_generation: &AtomicU64) {
    let Ok(entries) = std::fs::read_dir(staging_directory) else {
        return;
    };
    let highest = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(parse_runtime_staged_generation)
        })
        .max();
    if let Some(highest) = highest {
        staged_generation.fetch_max(highest, Ordering::Release);
    }
}

/// Render a short, watch-root-relative report of the changed paths.
fn summarize_changed_paths(changed_paths: &HashSet<PathBuf>, watch_paths: &[PathBuf]) -> String {
    let mut report: String = changed_paths
        .iter()
        .take(REPORTED_PATH_LIMIT)
        .map(|path| {
            watch_paths
                .iter()
                .find_map(|root| path.strip_prefix(root).ok())
                .unwrap_or(path)
                .display()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(", ");
    if changed_paths.len() > REPORTED_PATH_LIMIT {
        report.push_str(&format!(
            " (+{} more)",
            changed_paths.len() - REPORTED_PATH_LIMIT
        ));
    }
    report
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // External crates
    use notify::event::{
        AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
    };

    /// Verifies that content, creation, and removal events trigger a rebuild
    /// while access and other events are ignored.
    #[test]
    fn relevant_events_cover_changes_and_exclude_noise() {
        assert!(is_relevant_event(&EventKind::Create(CreateKind::File)));
        assert!(is_relevant_event(&EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
        // Renames surface as name modifications on some platforms.
        assert!(is_relevant_event(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Both
        ))));
        assert!(is_relevant_event(&EventKind::Remove(RemoveKind::File)));
        assert!(!is_relevant_event(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_relevant_event(&EventKind::Other));
    }

    /// Verifies that build output directories are filtered out only at the
    /// top level of the watch tree while deeper source directories using the
    /// same names stay relevant.
    #[test]
    fn build_output_directories_are_filtered_only_at_the_watch_root() {
        assert!(!is_relevant_relative_path(Path::new(
            "target/debug/libproject.so"
        )));
        assert!(!is_relevant_relative_path(Path::new(
            "bin/Release/project_cs.dll"
        )));
        assert!(!is_relevant_relative_path(Path::new(
            "obj/x64/project.cache"
        )));
        // Deeper directories may legitimately use these names.
        assert!(is_relevant_relative_path(Path::new(
            "src/target_logic/module.rs"
        )));
        assert!(is_relevant_relative_path(Path::new("scripts/bin/run.rs")));
        assert!(is_relevant_relative_path(Path::new("src/main.rs")));
        assert!(is_relevant_relative_path(Path::new("Bird.cs")));
    }

    /// Verifies that hidden files and directories are excluded at any depth.
    #[test]
    fn hidden_files_and_directories_are_filtered_out() {
        assert!(!is_relevant_relative_path(Path::new(".hidden_file")));
        assert!(!is_relevant_relative_path(Path::new(
            "src/.hidden_dir/file.rs"
        )));
    }

    /// Verifies that editor temporary and swap files never trigger a rebuild.
    #[test]
    fn editor_temporary_and_swap_files_are_filtered_out() {
        assert!(!is_relevant_relative_path(Path::new("src/main.rs~")));
        assert!(!is_relevant_relative_path(Path::new("src/.main.rs.swp")));
        assert!(!is_relevant_relative_path(Path::new("src/main.rs.swx")));
    }

    /// Verifies that paths outside the watch root are rejected, which also
    /// closes the symlink-escape hatch.
    #[test]
    fn paths_outside_the_watch_root_are_filtered_out() {
        let base = std::env::temp_dir().join(format!("pill_watcher_test_{}", std::process::id()));
        let watch_root = base.join("watch");
        let outside = base.join("outside");
        std::fs::create_dir_all(&watch_root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let outside_file = outside.join("project.rs");
        std::fs::write(&outside_file, "// test").unwrap();
        assert!(!is_relevant_path(&outside_file, &watch_root));

        let inside_file = watch_root.join("main.rs");
        std::fs::write(&inside_file, "// test").unwrap();
        assert!(is_relevant_path(&inside_file, &watch_root));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Verifies that non-UTF-8 file names stay relevant instead of being
    /// silently filtered out. Unix-only because non-UTF-8 `OsStr` literals are
    /// not constructible through stable APIs on Windows.
    #[cfg(unix)]
    #[test]
    fn non_utf8_file_names_stay_relevant() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        assert!(is_relevant_relative_path(Path::new(OsStr::from_bytes(
            b"src/caf\xe9.rs"
        ))));
    }

    /// The staged-runtime signal reports the newest index and never regresses.
    #[test]
    fn staged_runtime_generation_only_moves_forward() {
        let directory =
            std::env::temp_dir().join(format!("pill_staging_test_{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("pill_runtime_hot_reloaded_2.dll"), b"x").unwrap();
        std::fs::write(directory.join("pill_runtime_hot_reloaded_5.dll"), b"x").unwrap();
        std::fs::write(directory.join("unrelated.txt"), b"x").unwrap();

        let generation = AtomicU64::new(0);
        publish_highest_staged_generation(&directory, &generation);
        assert_eq!(generation.load(Ordering::Acquire), 5);

        // Removing the newest file must not make an older one look current.
        std::fs::remove_file(directory.join("pill_runtime_hot_reloaded_5.dll")).unwrap();
        publish_highest_staged_generation(&directory, &generation);
        assert_eq!(generation.load(Ordering::Acquire), 5);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Changed-path reports are rendered relative to their watch root.
    #[test]
    fn changed_path_reports_are_relative_to_the_watch_root() {
        let root = PathBuf::from("/workspace/pill_engine/src");
        let changed = HashSet::from([root.join("world.rs")]);
        let report = summarize_changed_paths(&changed, std::slice::from_ref(&root));
        assert_eq!(report, "world.rs");
    }
}
