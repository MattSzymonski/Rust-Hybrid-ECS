//! Standalone host binary — loads the game module, runs the engine loop,
//! and hot-reloads the game module when source files change.
//!
//! # Responsibilities
//!
//! - Creates and owns the [`Engine`] instance.
//! - Watches the game module's source directory for changes via `notify`.
//! - On change, rebuilds the module, loads the new library, and calls
//!   `game_init` to re-register components and systems.
//! - Runs the main game loop: `engine.process_frame()` → `game_update()`.
//!
//! # Design
//!
//! Build and watch settings are externalized into [`GameModuleConfig`] so
//! the same host can support Rust (`cargo build`), C (`make`), Zig
//! (`zig build`), or any other language that produces a shared library
//! exporting `game_init` / `game_update`.
//!
//! On Windows, loaded DLLs are locked by the OS. To work around this, each
//! build is copied to a unique temporary path before loading. Old copies are
//! cleaned up on shutdown.
//!
//! The game loop runs at a fixed 60 FPS using the engine's built-in frame
//! limiter. A separate thread watches for file changes and signals the main
//! thread to reload via an `AtomicBool` flag checked between frames.

// Standard library
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// External crates
use libloading::{Library, Symbol};

// Current crate
use ecs_hybrid::{Engine, EngineApi};

// =============================================================================
// Constants
// =============================================================================

/// Debounce duration — file events within this window are coalesced.
const DEBOUNCE_DURATION: Duration = Duration::from_millis(300);

/// Directory where temporary DLL copies are stored.
const TEMPORARY_DIRECTORY: &str = "standalone_temp";

// =============================================================================
// GameModuleConfig
// =============================================================================

/// Configuration for a hot-reloadable game module.
///
/// Describes how to build the module, where to find its output, and which
/// source directories to watch.  Change the fields here to switch between
/// Rust, C, C++, Zig, or any other language that compiles to a shared library.
///
/// # Examples
///
/// **Rust** (default):
///
/// ```
/// # use standalone::GameModuleConfig;
/// let config = GameModuleConfig::rust_example();
/// ```
///
/// **C (future)**:
///
/// ```ignore
/// GameModuleConfig {
///     name: "cgame",
///     watch_directory: "cgame/src",
///     build_command: &["make", "-C", "cgame"],
///     library_name: "cgame",  // → libcgame.so / cgame.dll
///     output_subdirectory: "cgame/build",
/// }
/// ```
#[derive(Debug, Clone)]
struct GameModuleConfig {
    /// Human-readable name used in log messages.
    pub name: &'static str,

    /// Directory to watch for source changes (relative to workspace root).
    /// All files and subdirectories within are monitored recursively.
    pub watch_directory: &'static str,

    /// Build command: first element is the program, remaining are arguments.
    /// Executed with `current_dir` set to the workspace root.
    pub build_command: &'static [&'static str],

    /// Base name of the shared library (without extension or `lib` prefix).
    /// The extension is determined automatically from the host platform.
    pub library_name: &'static str,

    /// Subdirectory (relative to workspace root) where the build system
    /// places the output shared library.  Defaults to `"target/debug"` for
    /// Cargo projects.
    pub output_subdirectory: &'static str,
}

impl GameModuleConfig {
    /// Default configuration for a Rust `cdylib` game module built with Cargo.
    pub const fn rust_default() -> Self {
        Self {
            name: "game",
            watch_directory: "game/src",
            build_command: &["cargo", "build", "--package", "game"],
            library_name: "game",
            output_subdirectory: "target/debug",
        }
    }

    /// Configuration for the dedicated integration-test game crate.
    pub const fn tests_game() -> Self {
        Self {
            name: "tests-game",
            watch_directory: "tests/game/src",
            build_command: &["cargo", "build", "--manifest-path", "tests/game/Cargo.toml"],
            library_name: "game",
            output_subdirectory: "tests/game/target/debug",
        }
    }

    /// Pick module configuration from environment, defaulting to workspace game.
    pub fn from_environment() -> Self {
        match std::env::var("ECS_HOT_RELOAD_MODULE") {
            Ok(value) if value.eq_ignore_ascii_case("tests-game") => Self::tests_game(),
            _ => Self::rust_default(),
        }
    }
}

// =============================================================================
// GameLibrary — Wraps a Loaded DLL
// =============================================================================

/// Holds a loaded game dynamic library and its exported function symbols.
///
/// The inner [`Library`] is **never dropped** after a hot-reload — it is
/// moved into a graveyard `Vec` so the old DLL stays mapped.  This prevents
/// use-after-free crashes from vtables and function pointers that still
/// reference the old library's code.
struct GameLibrary {
    /// The loaded library handle.
    library: Library,
}

impl GameLibrary {
    /// Load the game DLL from the given path.
    ///
    /// # Safety
    ///
    /// The DLL at `path` must be a valid game module exporting `game_init`
    /// and `game_update` with the correct C ABI signatures.
    unsafe fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // SAFETY: The caller guarantees the path points to a valid DLL.
        let library = unsafe { Library::new(path)? };

        // Verify required symbols exist by loading them once.
        // SAFETY: We only read the symbol address, we do not call it yet.
        let _: Symbol<unsafe extern "C" fn(*const EngineApi)> =
            unsafe { library.get(b"game_init")? };

        Ok(Self { library })
    }

    /// Call the `game_init` entry point in the loaded DLL.
    fn call_game_init(&self, api: &EngineApi) {
        // SAFETY: The function pointer was validated at load time.
        // The EngineApi pointer is valid for the duration of this call.
        unsafe {
            let game_init: Symbol<unsafe extern "C" fn(*const EngineApi)> = self
                .library
                .get(b"game_init")
                .expect("game_init symbol missing");
            game_init(api as *const EngineApi);
        }
    }

    /// Call the `game_update` entry point in the loaded DLL.
    fn call_game_update(&self, api: &EngineApi) {
        // SAFETY: Same preconditions as `call_game_init`.
        unsafe {
            let game_update: Symbol<unsafe extern "C" fn(*const EngineApi)> = self
                .library
                .get(b"game_update")
                .expect("game_update symbol missing");
            game_update(api as *const EngineApi);
        }
    }
}

// =============================================================================
// Build and Load
// =============================================================================

/// Build the game module using its configured build command and return
/// the path to the output shared library.
fn build_game_module(
    workspace_root: &Path,
    config: &GameModuleConfig,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    println!("[standalone] Building {} module...", config.name);

    let program = config.build_command[0];
    let arguments = &config.build_command[1..];

    let output = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Build command failed for '{}':\n{}", config.name, stderr).into());
    }

    // Determine the shared library filename based on the target platform.
    let library_filename = if cfg!(target_os = "windows") {
        format!("{}.dll", config.library_name)
    } else if cfg!(target_os = "macos") {
        format!("lib{}.dylib", config.library_name)
    } else {
        format!("lib{}.so", config.library_name)
    };

    let library_path = workspace_root
        .join(config.output_subdirectory)
        .join(&library_filename);

    if !library_path.exists() {
        return Err(format!(
            "Shared library not found at expected path: {}\n\
             Build succeeded but the output was not where we expected. \
             Check `output_subdirectory` in GameModuleConfig.",
            library_path.display()
        )
        .into());
    }

    Ok(library_path)
}

/// Copy the built shared library to a unique temporary path (avoids file
/// locking on Windows) and load it.
fn load_game_library(
    build_output: &Path,
    workspace_root: &Path,
) -> Result<GameLibrary, Box<dyn std::error::Error>> {
    // Ensure the temporary directory exists.
    let temporary_directory = workspace_root.join(TEMPORARY_DIRECTORY);
    std::fs::create_dir_all(&temporary_directory)?;

    // Create a unique filename using a timestamp so that multiple reloads
    // do not collide and Windows does not lock us out of the file.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dll_extension = build_output
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("dll");
    let unique_name = format!("game_{}.{}", timestamp, dll_extension);
    let temporary_path = temporary_directory.join(&unique_name);

    std::fs::copy(build_output, &temporary_path)?;
    println!("[standalone] Copied DLL to: {}", temporary_path.display());

    // SAFETY: We just built this DLL and copied it. It is a valid game module.
    let game_library = unsafe { GameLibrary::load(&temporary_path)? };
    println!("[standalone] Game DLL loaded successfully.");

    Ok(game_library)
}

// =============================================================================
// File Watcher
// =============================================================================

/// Spawn a background thread that watches the configured source directory
/// for changes and sets `reload_flag` to `true` when a change is detected.
///
/// Uses debouncing: rapid successive changes within [`DEBOUNCE_DURATION`]
/// are coalesced into a single reload signal.
fn spawn_file_watcher(
    workspace_root: PathBuf,
    config: &GameModuleConfig,
    reload_flag: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    use notify::event::EventKind;
    use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};

    let watch_path = workspace_root.join(config.watch_directory);
    if !watch_path.exists() {
        return Err(format!("Watch directory does not exist: {}", watch_path.display()).into());
    }

    println!(
        "[standalone] Watching '{}' for changes in: {}",
        config.name,
        watch_path.display()
    );

    // Create a channel for notify events.
    let (sender, receiver) = std::sync::mpsc::channel();

    let mut watcher = RecommendedWatcher::new(
        move |result: Result<Event, notify::Error>| {
            if let Ok(event) = result {
                // Only react to file modifications and creations.
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        let _ = sender.send(());
                    }
                    _ => {}
                }
            }
        },
        Config::default(),
    )?;

    watcher.watch(&watch_path, RecursiveMode::Recursive)?;

    // Spawn the watcher thread. We must keep `watcher` alive, so we move
    // it into the thread.
    std::thread::spawn(move || {
        // `_watcher` is kept alive here, so the watcher keeps running.
        let _watcher = watcher;

        loop {
            // Block until a file event arrives.
            if receiver.recv().is_err() {
                // Channel closed — watcher stopped.
                break;
            }

            // Debounce: after the first event, sleep and drain any
            // additional events that arrive within the debounce window.
            std::thread::sleep(DEBOUNCE_DURATION);
            while receiver.try_recv().is_ok() {
                // Drain the queue.
            }

            // Signal the main thread to reload.
            reload_flag.store(true, Ordering::Release);
        }
    });

    Ok(())
}

// =============================================================================
// Cleanup
// =============================================================================

/// Remove leftover temporary DLL copies from previous runs.
fn cleanup_temporary_files(workspace_root: &Path) {
    let temporary_directory = workspace_root.join(TEMPORARY_DIRECTORY);
    if temporary_directory.exists() {
        let _ = std::fs::remove_dir_all(&temporary_directory);
        println!("[standalone] Cleaned up leftover temporary files.");
    }
}

// =============================================================================
// Main
// =============================================================================

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Determine workspace root (standalone's parent directory).
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Cannot determine workspace root")?
        .to_path_buf();

    // Configure the game module.  Change this to switch languages:
    //   GameModuleConfig::rust_default()  — Rust cdylib (Cargo)
    //   GameModuleConfig { name: "cgame", watch_directory: "cgame/src",
    //       build_command: &["make", "-C", "cgame"], library_name: "cgame",
    //       output_subdirectory: "cgame/build" }  — C (Make)
    let module_config = GameModuleConfig::from_environment();

    println!("=== ECS Standalone Host ===");
    println!("Workspace:   {}", workspace_root.display());
    println!(
        "Game module: {} ({})",
        module_config.name, module_config.library_name
    );
    println!("Build cmd:   {}", module_config.build_command.join(" "));
    println!("Watch dir:   {}", module_config.watch_directory);
    println!();

    // Clean up leftover temporary files from previous runs.
    cleanup_temporary_files(&workspace_root);

    // Step 1: Create the engine and its C-compatible API table.
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);
    engine.set_fps_limit(60.0);

    // Build the EngineApi ONCE — all function pointers target `engine`.
    // The game module receives this via `game_init`.
    let engine_api = EngineApi::new(&mut engine);

    // Step 2: Build and load the game module for the first time.
    let library_path = build_game_module(&workspace_root, &module_config)?;
    let mut game_library = load_game_library(&library_path, &workspace_root)?;

    // Step 3: Call game_init so the module registers its components and systems.
    game_library.call_game_init(&engine_api);

    // Step 4: Set up the file watcher for hot-reloading.
    let reload_flag = Arc::new(AtomicBool::new(false));
    spawn_file_watcher(workspace_root.clone(), &module_config, reload_flag.clone())?;

    // Step 5: Main game loop.
    println!();
    println!(
        "[standalone] Entering game loop. Edit {}/**/* to hot-reload.",
        module_config.watch_directory
    );
    println!("[standalone] Press Ctrl+C to stop.");
    println!();

    let mut frame_count: u64 = 0;
    let mut last_report = std::time::Instant::now();

    // Graveyard: old DLLs are never unloaded — their code/vtables may still
    // be referenced by storage factories and component copiers in the World.
    // Keeping them mapped avoids use-after-free crashes on hot-reload.
    let mut old_libraries: Vec<Library> = Vec::new();

    loop {
        // Check if a reload was requested by the file watcher.
        if reload_flag.swap(false, Ordering::Acquire) {
            println!();
            println!("[standalone] === Hot-reload triggered ===");

            // Step A: Try to build and load the new library BEFORE dropping
            // the old one. If the build or load fails, we keep the old
            // game module running — zero downtime.
            match build_game_module(&workspace_root, &module_config) {
                Ok(path) => match load_game_library(&path, &workspace_root) {
                    Ok(new_library) => {
                        let previous_metadata_by_name =
                            engine.world().capture_persist_type_metadata();
                        let previous_manifest = engine.world().persist_type_manifest();

                        println!("[standalone] === Reload step 1/4: clearing old systems ===");
                        engine.clear_systems();
                        println!(
                            "[standalone] === Reload step 2/4: calling game_init on new DLL ==="
                        );
                        new_library.call_game_init(&engine_api);

                        let current_manifest = engine.world().persist_type_manifest();
                        let current_schema_by_name: std::collections::HashMap<String, u64> =
                            current_manifest
                                .iter()
                                .map(|entry| (entry.type_name.clone(), entry.schema_hash))
                                .collect();

                        let changed_type_names: std::collections::HashSet<String> =
                            previous_manifest
                                .iter()
                                .filter_map(|entry| {
                                    current_schema_by_name
                                        .get(&entry.type_name)
                                        .filter(|&&current_hash| current_hash != entry.schema_hash)
                                        .map(|_| entry.type_name.clone())
                                })
                                .collect();

                        if changed_type_names.is_empty() {
                            println!(
                                "[standalone] Schema unchanged for all persistable component types — fast path.",
                            );
                        } else {
                            println!(
                                "[standalone] === Reload step 3/4: selectively migrating {} component type(s) ===",
                                changed_type_names.len(),
                            );
                            let migration_report =
                                engine.world_mut().migrate_changed_persistable_components(
                                    &previous_metadata_by_name,
                                    &changed_type_names,
                                );

                            println!(
                                "[standalone] Selective migration complete: {} type(s), {} entities.",
                                migration_report.migrated_type_count,
                                migration_report.migrated_entity_count,
                            );

                            if !migration_report.skipped_type_names.is_empty() {
                                eprintln!(
                                    "[standalone] Selective migration skipped {} type(s): {:?}",
                                    migration_report.skipped_type_names.len(),
                                    migration_report.skipped_type_names,
                                );
                            }
                        }

                        println!(
                            "[standalone] === Reload step 4/4: archiving old DLL, swapping ==="
                        );
                        old_libraries.push(game_library.library);
                        game_library = new_library;
                        println!(
                            "[standalone] Hot-reload complete ({} entities, {} old libs in graveyard).",
                            engine.world().entity_count(),
                            old_libraries.len(),
                        );
                    }
                    Err(error) => {
                        eprintln!("[standalone] Failed to load new library: {}", error);
                        eprintln!(
                            "[standalone] Keeping old game module. Fix errors and save again."
                        );
                    }
                },
                Err(error) => {
                    eprintln!("[standalone] Build failed: {}", error);
                    eprintln!(
                        "[standalone] Keeping old game module. Fix compilation errors and save again."
                    );
                }
            }
        }

        // Run one frame of engine systems.
        if let Err(errors) = engine.process_frame() {
            eprintln!("[standalone] Frame error: {:?}", errors);
        }

        // Call the game's per-frame update hook.
        game_library.call_game_update(&engine_api);

        frame_count += 1;

        // Report FPS every 2 seconds.
        let elapsed = last_report.elapsed().as_secs_f64();
        if elapsed >= 2.0 {
            let fps = frame_count as f64 / elapsed;
            let entity_count = engine.world().entity_count();
            println!("  {:>6.0} FPS | {:>5} entities", fps, entity_count);
            frame_count = 0;
            last_report = std::time::Instant::now();
        }
    }
}
