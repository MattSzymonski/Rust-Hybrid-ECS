//! Watch the configured source tree for file changes and signal reloads to
//! the main thread.
//!
//! # Responsibilities
//!
//! - Spawn a worker thread that monitors the source tree for changes.
//! - Classify file events and paths so only real source edits trigger reloads.
//! - Bump a reload generation counter when relevant file events are
//!   detected, letting the main thread perform a hot reload.
//! - Report which files changed so reloads are debuggable.
//! - Handle cross-platform file notification differences through `notify`.
//!
//! # Design
//!
//! The file watcher runs in a separate thread to avoid blocking the main loop.
//! A debounce window coalesces multiple file events into a single reload signal,
//! and the changed paths collected during that window are reported to the
//! console. The main thread compares a generation counter against the last
//! processed value each loop, so events arriving during a reload are never
//! lost.

// Standard library
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

// External crates
use notify::event::EventKind;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use pill_core::error::WatcherError;
use pill_core::{error, info};

// =============================================================================
// Constants
// =============================================================================

/// File events arriving within this window are coalesced into one reload.
///
/// This is pure latency: it is spent sleeping, on every edit, before any work
/// starts. At 300 ms it was 40% of a ~700 ms save-to-live, chosen before that
/// number was ever measured.
///
/// It exists because one save produces several notifications - editors write,
/// rename and touch metadata - and each would otherwise start its own rebuild.
/// The window only has to outlast that burst, which is a few milliseconds, not
/// hundreds. Override with `PILL_WATCH_DEBOUNCE_MS` when an editor's save
/// pattern needs more room.
const DEFAULT_DEBOUNCE_MILLISECONDS: u64 = 60;

/// Environment variable overriding the debounce window.
const DEBOUNCE_OVERRIDE_VARIABLE: &str = "PILL_WATCH_DEBOUNCE_MS";

/// How long to coalesce a burst of file events, resolved once per process.
fn debounce_duration() -> Duration {
    static RESOLVED: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        let milliseconds = std::env::var(DEBOUNCE_OVERRIDE_VARIABLE)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DEBOUNCE_MILLISECONDS);
        Duration::from_millis(milliseconds)
    })
}

/// Directory names that never contain source code worth rebuilding for.
const IGNORED_DIRECTORY_NAMES: [&str; 3] = ["target", "bin", "obj"];

/// Suffixes editors use for temporary and swap files written around saves.
const EDITOR_TEMPORARY_FILE_SUFFIXES: [&str; 3] = ["~", ".swp", ".swx"];

/// Prefix shared by hidden files, which never contain source code.
const HIDDEN_FILE_PREFIX: &str = ".";

/// Maximum number of changed paths printed per reload report.
const REPORTED_PATH_LIMIT: usize = 5;

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

/// Watch the configured source tree and signal reloads from a worker thread.
///
/// # Errors
///
/// Returns an error if the watch directory does not exist, the watcher cannot
/// be created, or the watch path cannot be registered.
pub(crate) fn spawn_source_watcher(
    workspace_root: PathBuf,
    module_name: &str,
    watch_directory: &str,
    reload_generation: Arc<AtomicU64>,
) -> Result<(), WatcherError> {
    // Step 1: Resolve and validate the configured watch directory.
    // Watch paths are configured relative to the repository so the same
    // configuration works regardless of the process's current directory.
    let watch_path = workspace_root.join(watch_directory);

    // Fail during host setup instead of silently running without hot reload
    // when a module configuration contains an outdated source path.
    if !watch_path.exists() {
        return Err(WatcherError::WatchDirectoryMissing {
            path: watch_path.display().to_string(),
        });
    }

    info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        module = module_name,
        watch_directory = %watch_path.display(),
        "watching for source changes"
    );

    // Step 2: Create the watcher with a minimal callback that forwards
    // relevant paths to a debounce channel.
    let callback_root = watch_path.clone();
    let (sender, receiver) = std::sync::mpsc::channel::<PathBuf>();
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| match result {
            Ok(event) => {
                if is_relevant_event(&event.kind) {
                    for path in event.paths {
                        if is_relevant_path(&path, &callback_root) {
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
    watcher
        .watch(&watch_path, RecursiveMode::Recursive)
        .map_err(|source| WatcherError::RegistrationFailed {
            path: watch_path.display().to_string(),
            source,
        })?;

    // Step 3: Run the debounce worker that reports changes and signals the
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
            std::thread::sleep(debounce_duration());
            while let Ok(path) = receiver.try_recv() {
                changed_paths.insert(path);
            }

            // Prepare a short report of the changed paths for the console.
            // Paths are printed relative to the watch directory.
            let mut report: String = changed_paths
                .iter()
                .take(REPORTED_PATH_LIMIT)
                .map(|path| {
                    path.strip_prefix(&watch_path)
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
            // How long the edit sat before this thread saw it, measured from
            // the file's own modification time. Reported at INFO, and reported
            // at all, because a slow save-to-live is dominated by this number
            // rather than by the patch that follows it - and until now the only
            // way to know that was to time it from outside the process.
            //
            // The debounce below is part of it by construction, so a healthy
            // reading is a little over DEBOUNCE_DURATION, not zero.
            let detection_delay_ms = changed_paths
                .iter()
                .filter_map(|path| std::fs::metadata(path).ok())
                .filter_map(|metadata| metadata.modified().ok())
                .max()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
                .unwrap_or(f64::NAN);
            info!(
                target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
                changed_paths = %report,
                detection_delay_ms = format!("{detection_delay_ms:.0}").as_str(),
                debounce_ms = debounce_duration().as_millis() as u64,
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
}
