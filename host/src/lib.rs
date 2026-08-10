//! Shared game-module host: builds/loads the hot-reloadable game DLL, watches
//! its source for changes, and drives the engine's per-frame loop.
//!
//! # Responsibilities
//!
//! - Creates and owns the [`Engine`] instance.
//! - Watches the game module's source directory for changes via `notify`.
//! - On change, rebuilds the module, loads the new library, and calls
//!   `game_init` to re-register components and systems.
//! - Runs one frame at a time via [`run_one_frame`]: `engine.process_frame()`
//!   → `game_update()`.
//!
//! # Design
//!
//! This crate has no `main()` and owns no window - it is a library shared by
//! every host binary (`standalone`, `editor`, ...). Each binary owns its own
//! event loop / windowing and calls [`setup`] once, then [`run_one_frame`]
//! every tick.
//!
//! Build and watch settings are externalized into [`GameModuleConfig`] so the
//! same host can support Rust (`cargo build`), C (`make`), Zig (`zig build`),
//! or any other language that produces a shared library exporting
//! `game_init` / `game_update`.
//!
//! On Windows, loaded DLLs are locked by the OS. To work around this, each
//! build is copied to a unique temporary path before loading. Old copies are
//! cleaned up on shutdown.

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

mod cs;

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

/// Backend-specific output information for a hot-reloadable game module.
#[derive(Debug, Clone)]
pub enum GameModuleBackend {
    /// A native shared library exporting `game_init` and `game_update`.
    NativeLibrary {
        library_name: &'static str,
        output_subdirectory: &'static str,
    },
    /// A managed game assembly loaded through the stable `cs_runtime` host.
    CSharp(CSharpModuleConfig),
}

#[derive(Debug, Clone)]
pub struct CSharpModuleConfig {
    pub runtime_assembly_name: &'static str,
    pub runtime_output_subdirectory: &'static str,
    pub game_assembly_name: &'static str,
    pub game_output_subdirectory: &'static str,
}

/// Configuration for a hot-reloadable game module.
///
/// Describes how to build the module, where to find its output, and which
/// source directories to watch.  Change the fields here to switch between
/// Rust, C, C++, Zig, or any other language that compiles to a shared library.
#[derive(Debug, Clone)]
pub struct GameModuleConfig {
    /// Human-readable name used in log messages.
    pub name: &'static str,

    /// Directory to watch for source changes (relative to workspace root).
    /// All files and subdirectories within are monitored recursively.
    pub watch_directory: &'static str,

    /// Build command: first element is the program, remaining are arguments.
    /// Executed with `current_dir` set to the workspace root.
    pub build_command: &'static [&'static str],

    /// How the built module is loaded and executed.
    pub backend: GameModuleBackend,
}

impl GameModuleConfig {
    /// Default configuration for a Rust `cdylib` game module built with Cargo.
    ///
    /// When the host is built with the `rendering` feature, the game module
    /// is built with its own `rendering` feature too, so both sides agree on
    /// which components (`Position`, `Sprite`, ...) exist.
    #[cfg(not(feature = "rendering"))]
    pub const fn rust_default() -> Self {
        Self {
            name: "game-rs",
            watch_directory: "game_rs/src",
            build_command: &["cargo", "build", "--package", "game"],
            backend: GameModuleBackend::NativeLibrary {
                library_name: "game",
                output_subdirectory: "target/debug",
            },
        }
    }

    /// See the non-`rendering` variant above.
    #[cfg(feature = "rendering")]
    pub const fn rust_default() -> Self {
        Self {
            name: "game-rs",
            watch_directory: "game_rs/src",
            build_command: &[
                "cargo",
                "build",
                "--package",
                "game",
                "--features",
                "rendering",
            ],
            backend: GameModuleBackend::NativeLibrary {
                library_name: "game",
                output_subdirectory: "target/debug",
            },
        }
    }

    /// Default scheduler-integrated C# game loaded through `cs_runtime`.
    pub const fn csharp_default() -> Self {
        Self {
            name: "game-cs",
            watch_directory: "game_cs/src",
            build_command: &[
                "dotnet",
                "build",
                "game_cs/game_cs.csproj",
                "-c",
                "Release",
                "--nologo",
            ],
            backend: GameModuleBackend::CSharp(CSharpModuleConfig {
                runtime_assembly_name: "cs_runtime",
                runtime_output_subdirectory: "cs_runtime/bin/Release/net8.0",
                game_assembly_name: "game_cs",
                game_output_subdirectory: "game_cs/bin/Release/net8.0",
            }),
        }
    }

    /// Configuration for the dedicated integration-test game crate.
    pub const fn tests_game() -> Self {
        Self {
            name: "tests-game",
            watch_directory: "tests/game/src",
            build_command: &["cargo", "build", "--manifest-path", "tests/game/Cargo.toml"],
            backend: GameModuleBackend::NativeLibrary {
                library_name: "game",
                output_subdirectory: "tests/game/target/debug",
            },
        }
    }

    /// Pick module configuration from environment, defaulting to workspace game.
    pub fn from_environment() -> Self {
        match std::env::var("ECS_HOT_RELOAD_MODULE") {
            Ok(value) if value.eq_ignore_ascii_case("tests-game") => Self::tests_game(),
            Ok(value)
                if value.eq_ignore_ascii_case("cs")
                    || value.eq_ignore_ascii_case("csharp")
                    || value.eq_ignore_ascii_case("game-cs") =>
            {
                Self::csharp_default()
            }
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
    println!("[host] Building {} module...", config.name);

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

    let library_path = match &config.backend {
        GameModuleBackend::NativeLibrary {
            library_name,
            output_subdirectory,
        } => {
            let filename = if cfg!(target_os = "windows") {
                format!("{library_name}.dll")
            } else if cfg!(target_os = "macos") {
                format!("lib{library_name}.dylib")
            } else {
                format!("lib{library_name}.so")
            };
            workspace_root.join(output_subdirectory).join(filename)
        }
        GameModuleBackend::CSharp(config) => workspace_root
            .join(config.game_output_subdirectory)
            .join(format!("{}.dll", config.game_assembly_name)),
    };

    if !library_path.exists() {
        return Err(format!(
            "Shared library not found at expected path: {}\n\
             Build succeeded but the output was not where we expected. \
             Check the selected backend output directory in GameModuleConfig.",
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
    println!("[host] Copied DLL to: {}", temporary_path.display());

    // SAFETY: We just built this DLL and copied it. It is a valid game module.
    let game_library = unsafe { GameLibrary::load(&temporary_path)? };
    println!("[host] Game DLL loaded successfully.");

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
        "[host] Watching '{}' for changes in: {}",
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
        println!("[host] Cleaned up leftover temporary files.");
    }
}

// =============================================================================
// Host
// =============================================================================

/// Everything the frame-step loop needs, assembled once during startup.
///
/// Bundled into a struct so any host binary (headless loop, windowed loop,
/// Dioxus-driven loop, ...) can share the exact same per-frame/hot-reload
/// logic via [`run_one_frame`].
pub struct Host {
    workspace_root: PathBuf,
    module_config: GameModuleConfig,
    // Boxed so its heap address stays stable even when `Host` itself is
    // moved (e.g. returned by value from `setup`, or moved again into a
    // caller's own struct). `engine_api.engine_handle` is a raw pointer into
    // this allocation - if `Engine` lived inline in `Host`, moving `Host`
    // after constructing `EngineApi` would leave that pointer dangling.
    engine: Box<Engine>,
    engine_api: EngineApi,
    loaded_game: LoadedGame,
    reload_flag: Arc<AtomicBool>,
    frame_count: u64,
    last_report: std::time::Instant,
}

enum LoadedGame {
    Native {
        current: GameLibrary,
        old_libraries: Vec<Library>,
    },
    CSharp(cs::CSharpRuntime),
}

impl Host {
    /// Read-only access to the engine (e.g. for rendering, `world().entity_count()`).
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable access to the engine (e.g. for a renderer's ad-hoc queries).
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

/// Perform one-time setup: build/load the game module, create the engine,
/// and start the file watcher. Returns the assembled [`Host`] state.
pub fn setup(module_config: GameModuleConfig) -> Result<Host, Box<dyn std::error::Error>> {
    // Determine workspace root (this crate's parent directory).
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Cannot determine workspace root")?
        .to_path_buf();

    println!("=== ECS Host ===");
    println!("Workspace:   {}", workspace_root.display());
    println!(
        "Game module: {} ({:?})",
        module_config.name, module_config.backend
    );
    println!("Build cmd:   {}", module_config.build_command.join(" "));
    println!("Watch dir:   {}", module_config.watch_directory);
    println!();

    // Clean up leftover temporary files from previous runs.
    if matches!(
        module_config.backend,
        GameModuleBackend::NativeLibrary { .. }
    ) {
        cleanup_temporary_files(&workspace_root);
    }

    // Step 1: Create the engine and its C-compatible API table.
    //
    // `engine` is boxed BEFORE building `EngineApi` so `engine_api.engine_handle`
    // points at its final, stable heap address. If `EngineApi::new` captured
    // the address of a stack-local `Engine` that later moved into `Host`
    // (returned by value below), the pointer would dangle - see the comment
    // on `Host::engine`.
    let mut engine = Box::new(Engine::new());
    engine.set_parallel_execution(true);

    // Build the EngineApi ONCE — all function pointers target `engine`.
    // The game module receives this via `game_init`.
    let engine_api = EngineApi::new(&mut engine);

    // Step 2: Build and load the game module for the first time.
    let output_path = build_game_module(&workspace_root, &module_config)?;
    let loaded_game = match &module_config.backend {
        GameModuleBackend::NativeLibrary { .. } => {
            let library = load_game_library(&output_path, &workspace_root)?;
            library.call_game_init(&engine_api);
            LoadedGame::Native {
                current: library,
                old_libraries: Vec::new(),
            }
        }
        GameModuleBackend::CSharp(config) => LoadedGame::CSharp(cs::CSharpRuntime::start(
            &mut engine,
            &workspace_root,
            config,
        )?),
    };

    // Step 4: Set up the file watcher for hot-reloading.
    let reload_flag = Arc::new(AtomicBool::new(false));
    spawn_file_watcher(workspace_root.clone(), &module_config, reload_flag.clone())?;

    println!();
    println!(
        "[host] Entering game loop. Edit {}/**/* to hot-reload.",
        module_config.watch_directory
    );
    println!();

    Ok(Host {
        workspace_root,
        module_config,
        engine,
        engine_api,
        loaded_game,
        reload_flag,
        frame_count: 0,
        last_report: std::time::Instant::now(),
    })
}

/// Result of one [`run_one_frame`] call: the FPS report, if one was due.
#[derive(Debug, Clone, Copy)]
pub struct FrameReport {
    pub fps: f64,
    pub entity_count: usize,
}

/// Run one iteration of the host loop: check for hot-reload, process one
/// engine frame, call the game's update hook, and track FPS.
///
/// Returns `Some(FrameReport)` roughly every 3 seconds; `None` otherwise.
/// Callers that want to print or display live stats should check this.
pub fn run_one_frame(host: &mut Host) -> Option<FrameReport> {
    let Host {
        workspace_root,
        module_config,
        engine,
        engine_api,
        loaded_game,
        reload_flag,
        frame_count,
        last_report,
    } = host;

    // Check if a reload was requested by the file watcher.
    if reload_flag.swap(false, Ordering::Acquire) {
        println!();
        println!("[host] === Hot-reload triggered ===");

        match loaded_game {
            LoadedGame::Native {
                current: game_library,
                old_libraries,
            } => {
                // Step A: Try to build and load the new library BEFORE dropping
                // the old one. If the build or load fails, we keep the old
                // game module running — zero downtime.
                match build_game_module(workspace_root, module_config) {
                    Ok(path) => match load_game_library(&path, workspace_root) {
                        Ok(new_library) => {
                            let previous_metadata_by_name =
                                engine.world().capture_persist_type_metadata();
                            let previous_manifest = engine.world().persist_type_manifest();

                            println!("[host] === Reload step 1/4: clearing old systems ===");
                            engine.clear_systems();
                            println!(
                                "[host] === Reload step 2/4: calling game_init on new DLL ==="
                            );
                            new_library.call_game_init(engine_api);

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
                                            .filter(|&&current_hash| {
                                                current_hash != entry.schema_hash
                                            })
                                            .map(|_| entry.type_name.clone())
                                    })
                                    .collect();

                            if changed_type_names.is_empty() {
                                println!(
                            "[host] Schema unchanged for all persistable component types — fast path.",
                        );
                            } else {
                                println!(
                            "[host] === Reload step 3/4: selectively migrating {} component type(s) ===",
                            changed_type_names.len(),
                        );
                                let migration_report =
                                    engine.world_mut().migrate_changed_persistable_components(
                                        &previous_metadata_by_name,
                                        &changed_type_names,
                                    );

                                println!(
                                    "[host] Selective migration complete: {} type(s), {} entities.",
                                    migration_report.migrated_type_count,
                                    migration_report.migrated_entity_count,
                                );

                                if !migration_report.skipped_type_names.is_empty() {
                                    eprintln!(
                                        "[host] Selective migration skipped {} type(s): {:?}",
                                        migration_report.skipped_type_names.len(),
                                        migration_report.skipped_type_names,
                                    );
                                }
                            }

                            println!("[host] === Reload step 4/4: archiving old DLL, swapping ===");
                            old_libraries
                                .push(std::mem::replace(game_library, new_library).library);
                            println!(
                        "[host] Hot-reload complete ({} entities, {} old libs in graveyard).",
                        engine.world().entity_count(),
                        old_libraries.len(),
                    );
                        }
                        Err(error) => {
                            eprintln!("[host] Failed to load new library: {}", error);
                            eprintln!("[host] Keeping old game module. Fix errors and save again.");
                        }
                    },
                    Err(error) => {
                        eprintln!("[host] Build failed: {}", error);
                        eprintln!(
                    "[host] Keeping old game module. Fix compilation errors and save again."
                );
                    }
                }
            }
            LoadedGame::CSharp(runtime) => match build_game_module(workspace_root, module_config) {
                Ok(_) => {
                    println!("[host] C# build complete; waiting for managed reload.");
                    runtime.poll_reload();
                }
                Err(error) => {
                    eprintln!("[host] C# build failed: {error}");
                    eprintln!("[host] Keeping the currently loaded C# game assembly.");
                }
            },
        }
    }

    // The managed loader watches the built assembly rather than source files,
    // so poll each frame to pick up a successful build after its debounce.
    if let LoadedGame::CSharp(runtime) = loaded_game {
        runtime.poll_reload();
    }

    // Run one frame of engine systems.
    if let Err(errors) = engine.process_frame() {
        eprintln!("[host] Frame error: {:?}", errors);
    }

    // Managed games express their work as scheduler systems. Native games also
    // retain the optional per-frame update hook.
    if let LoadedGame::Native { current, .. } = loaded_game {
        current.call_game_update(engine_api);
    }

    *frame_count += 1;

    // Report FPS every 3 seconds.
    let elapsed = last_report.elapsed().as_secs_f64();
    if elapsed >= 3.0 {
        let fps = *frame_count as f64 / elapsed;
        let entity_count = engine.world().entity_count();
        *frame_count = 0;
        *last_report = std::time::Instant::now();
        Some(FrameReport { fps, entity_count })
    } else {
        None
    }
}
