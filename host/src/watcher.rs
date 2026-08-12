//! Watch the configured source tree for file changes and signal reloads to the main thread.
//!
//! # Design
//! The file watcher runs in a separate thread to avoid blocking the main frame loop. It uses a debounce
//! window to coalesce multiple file events into a single reload signal. The main thread checks the
//! reload flag each frame and triggers a hot reload when it is set.
//!
//! # Responsibilities
//! - Spawn a file watcher thread that monitors the source tree for changes.
//! - Set a reload flag when relevant file events are detected, allowing the main thread to perform a hot reload.
//! - Use the `notify` crate to watch for file events and handle cross-platform differences in file system notifications.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use notify::event::EventKind;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

use crate::GameModuleConfig;




/// File events arriving within this window are coalesced into one reload.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(300);

/// Watch the configured source tree and signal reloads from a worker thread.
pub(crate) fn spawn_file_watcher(
    workspace_root: PathBuf,
    config: &GameModuleConfig,
    reload_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
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

    // The notify callback should do as little work as possible because its
    // execution model differs between operating-system backends. A channel
    // transfers relevant events to one host-owned debounce thread.
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| {
            if let Ok(event) = result {
                // Editors commonly save through either an in-place write or a
                // newly created replacement file. Both must trigger a build;
                // metadata-only and access events can be safely ignored.
                if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                    // Failure only means the receiving thread has shut down,
                    // so there is no recovery work for the callback to perform.
                    let _ = sender.send(());
                }
            }
        },
        Config::default(),
    )?;

    // Recursive watching covers nested source modules without requiring every
    // language backend to enumerate its own directory structure.
    watcher.watch(&watch_path, RecursiveMode::Recursive)?;

    std::thread::spawn(move || {
        // RecommendedWatcher unregisters its OS handles when dropped. Move it
        // into the worker even though the loop never calls it directly, keeping
        // those handles alive for exactly as long as the receiver remains live.
        let _watcher = watcher;

        loop {
            // Block without consuming CPU until the first event starts a new
            // debounce window. A disconnected channel cleanly ends the worker.
            if receiver.recv().is_err() {
                break;
            }

            // One source save can produce several create/modify notifications.
            // Wait for the burst to settle and discard all duplicate signals,
            // ensuring the main thread performs only one compilation.
            std::thread::sleep(DEBOUNCE_DURATION);
            while receiver.try_recv().is_ok() {}

            // The frame loop swaps this flag with Acquire ordering. Release is
            // sufficient for the cross-thread handoff without the stronger
            // global ordering cost of a sequentially consistent atomic.
            reload_flag.store(true, Ordering::Release);
        }
    });

    Ok(())
}
