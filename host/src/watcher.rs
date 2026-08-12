//! Watch the configured source tree for file changes and signal reloads to
//! the main thread.
//!
//! # Responsibilities
//!
//! - Spawn a worker thread that monitors the source tree for changes.
//! - Classify file events and paths so only real source edits trigger reloads.
//! - Set a reload flag when relevant file events are detected, letting the
//!   main thread perform a hot reload.
//! - Report which files changed so reloads are debuggable.
//! - Handle cross-platform file notification differences through `notify`.
//!
//! # Design
//!
//! The file watcher runs in a separate thread to avoid blocking the main
//! frame loop. A debounce window coalesces multiple file events into a
//! single reload signal, and the changed paths collected during that window
//! are reported to the console. The main thread checks the reload flag each
//! frame and triggers a hot reload when it is set.

// Standard library
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// External crates
use notify::event::EventKind;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

// Current crate
use crate::GameModuleConfig;

// =============================================================================
// Constants
// =============================================================================

/// File events arriving within this window are coalesced into one reload.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(300);

/// Directory names that never contain source code worth rebuilding for.
const IGNORED_DIRECTORY_NAMES: [&str; 3] = ["target", "bin", "obj"];

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

/// Whether a changed path should trigger a rebuild.
///
/// Hidden files, editor temporary and swap files, and build output
/// directories are excluded because none of them contain source code.
fn is_relevant_path(path: &Path) -> bool {
    // Every directory component must be neither hidden nor a build output
    // directory; a single write inside `target/` or `.git/` must not
    // trigger a rebuild.
    for component in path.parent().into_iter().flat_map(Path::components) {
        if let Some(name) = component.as_os_str().to_str() {
            if name.starts_with('.') || IGNORED_DIRECTORY_NAMES.contains(&name) {
                return false;
            }
        }
    }

    // Editor temporary files appear and disappear around every save and
    // would otherwise trigger a rebuild of half-written content.
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => {
            !name.starts_with('.')
                && !name.ends_with('~')
                && !name.ends_with(".swp")
                && !name.ends_with(".swx")
        }
        None => false,
    }
}

/// Watch the configured source tree and signal reloads from a worker thread.
///
/// # Errors
///
/// Returns an error if the watch directory does not exist, the watcher cannot
/// be created, or the watch path cannot be registered.
pub(crate) fn spawn_file_watcher(
    workspace_root: PathBuf,
    config: &GameModuleConfig,
    reload_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Step 1: Resolve and validate the configured watch directory.
    // Watch paths are configured relative to the repository so the same
    // configuration works regardless of the process's current directory.
    let watch_path = workspace_root.join(config.watch_directory);

    // Fail during host setup instead of silently running without hot reload
    // when a module configuration contains an outdated source path.
    if !watch_path.exists() {
        return Err(format!("Watch directory does not exist: {}", watch_path.display()).into());
    }

    println!(
        "[host] Watching '{}' for changes in: {}",
        config.name,
        watch_path.display()
    );

    // Step 2: Create the watcher with a minimal callback that forwards
    // relevant paths to a debounce channel.
    // The notify callback should do as little work as possible because its
    // execution model differs between operating-system backends. A channel
    // transfers relevant paths to one host-owned debounce thread.
    let (sender, receiver) = std::sync::mpsc::channel::<PathBuf>();
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| match result {
            Ok(event) => {
                if is_relevant_event(&event.kind) {
                    for path in event.paths {
                        if is_relevant_path(&path) {
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
            Err(error) => eprintln!("[host] File watcher error: {error}"),
        },
        Config::default(),
    )?;

    // Recursive watching covers nested source modules without requiring every
    // language backend to enumerate its own directory structure.
    watcher.watch(&watch_path, RecursiveMode::Recursive)?;

    // Step 3: Run the debounce worker that reports changes and signals the
    // main frame loop.
    std::thread::spawn(move || {
        // RecommendedWatcher unregisters its OS handles when dropped. Move it
        // into the worker even though the loop never calls it directly, keeping
        // those handles alive for exactly as long as the receiver remains live.
        let _watcher = watcher;

        // Block without consuming CPU until an event starts a debounce
        // window. A disconnected channel cleanly ends the worker.
        while let Ok(first_path) = receiver.recv() {
            // One source save can produce several notifications for the same
            // file. Wait for the burst to settle, deduplicate all signals in
            // a set, and report the trigger before signalling the frame loop.
            let mut changed_paths = HashSet::from([first_path]);
            std::thread::sleep(DEBOUNCE_DURATION);
            while let Ok(path) = receiver.try_recv() {
                changed_paths.insert(path);
            }

            // Paths are printed relative to the watch directory so the report
            // stays short and readable even for long workspace paths.
            let mut report = changed_paths
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
            println!("[host] Change detected: {report}");

            // The frame loop swaps this flag with Acquire ordering. Release is
            // sufficient for the cross-thread handoff without the stronger
            // global ordering cost of a sequentially consistent atomic.
            reload_flag.store(true, Ordering::Release);
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

    /// Verifies that build output and hidden directories are filtered out
    /// while ordinary source paths are accepted.
    #[test]
    fn paths_inside_build_and_hidden_directories_are_filtered_out() {
        assert!(!is_relevant_path(Path::new(
            "game_rs/target/debug/libgame.so"
        )));
        assert!(!is_relevant_path(Path::new(
            "game_cs/bin/Release/game_cs.dll"
        )));
        assert!(!is_relevant_path(Path::new(
            "game_cs/obj/x64/game_cs.csproj.CoreCompileInputs.cache"
        )));
        assert!(!is_relevant_path(Path::new("game_rs/src/.hidden_file")));
        assert!(is_relevant_path(Path::new("game_rs/src/main.rs")));
        assert!(is_relevant_path(Path::new("game_cs/src/Bird.cs")));
    }

    /// Verifies that editor temporary and swap files never trigger a rebuild.
    #[test]
    fn editor_temporary_and_swap_files_are_filtered_out() {
        assert!(!is_relevant_path(Path::new("game_rs/src/main.rs~")));
        assert!(!is_relevant_path(Path::new("game_rs/src/.main.rs.swp")));
        assert!(!is_relevant_path(Path::new("game_rs/src/main.rs.swx")));
    }
}
