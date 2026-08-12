//! Engine ownership and frontend-facing frame orchestration.
//!
//! # Responsibilities
//!
//! - Own the engine instance and expose safe frontend access.
//! - Assemble the host state shared by all frontends.
//! - Execute one hot-reload-aware frame per [`run_one_frame`] call.
//!
//! # Design
//!
//! This module contains the stable API used by `standalone`, `editor`, and
//! other host binaries. Backend-specific loading stays behind [`LoadedGame`].

// Standard library
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

// External crates
use pill_engine::{Engine, EngineApi};
#[cfg(feature = "rendering")]
use pill_engine::{RenderViewport, Renderer, RendererError, RendererWindow, VirtualResolution};

// Current crate
use crate::game_module::LoadedGame;
use crate::native_library::cleanup_temporary_files;
use crate::watcher::spawn_file_watcher;
use crate::{GameModuleBackend, GameModuleConfig};

// =============================================================================
// Types + Impls
// =============================================================================

/// Everything the frame-step loop needs, assembled once during startup.
///
/// Bundling this state lets headless, windowed, and editor frontends share the
/// same engine lifetime and hot-reload behavior through [`run_one_frame`].
pub struct Host {
    workspace_root: PathBuf,
    module_config: GameModuleConfig,
    // Boxed before EngineApi is created so its raw engine pointer remains
    // stable even if Host is moved by a caller.
    engine: Box<Engine>,
    engine_api: EngineApi,
    loaded_game: LoadedGame,
    reload_flag: Arc<AtomicBool>,
    frame_count: u64,
    last_report: Instant,
    last_measured_fps: f64,
}

impl Host {
    /// Read-only engine access for rendering and diagnostics.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Mutable engine access for frontend-owned ad-hoc work.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }

    /// Snapshot the current frame rate and entity count without resetting the
    /// three-second reporting window used by console frontends.
    pub fn current_frame_report(&self) -> FrameReport {
        let elapsed = self.last_report.elapsed().as_secs_f64();
        let fps = if self.frame_count == 0 || elapsed <= f64::EPSILON {
            self.last_measured_fps
        } else {
            self.frame_count as f64 / elapsed
        };

        FrameReport {
            fps,
            entity_count: self.engine.world().entity_count(),
        }
    }
}

/// Host state with the engine renderer attached to one native window surface.
///
/// Keeping the renderer beside [`Host`] makes its creation and lifetime part
/// of host setup. Executable crates never construct or retain GPU resources.
#[cfg(feature = "rendering")]
pub struct RenderingHost {
    host: Host,
    renderer: Renderer,
}

#[cfg(feature = "rendering")]
impl RenderingHost {
    /// Move rendering to a newly created native window surface.
    ///
    /// The existing ECS host and game module remain alive. A replacement is
    /// constructed before the old renderer is dropped, so initialization
    /// failure leaves the current surface untouched.
    pub fn retarget_render_window<W>(
        &mut self,
        window: W,
        width: u32,
        height: u32,
    ) -> Result<(), RendererError>
    where
        W: RendererWindow + 'static,
    {
        let renderer = Renderer::new(window, width, height)?;
        self.renderer = renderer;
        Ok(())
    }

    /// Forward a physical window resize to the engine renderer.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.renderer.resize(width, height);
    }

    /// Restrict engine drawing to a physical region of the native surface.
    ///
    /// Use `None` for full-window rendering. Embedded frontends can leave the
    /// corresponding WebView region transparent and keep surrounding UI
    /// panels opaque.
    pub fn set_render_viewport(&mut self, viewport: Option<RenderViewport>) {
        self.renderer.set_viewport(viewport);
    }

    /// Map a stable game coordinate space into the current physical viewport.
    ///
    /// Pass `None` to make logical renderer units match physical pixels again.
    pub fn set_render_virtual_resolution(&mut self, resolution: Option<VirtualResolution>) {
        self.renderer.set_virtual_resolution(resolution);
    }

    /// Execute one ECS frame and present its resulting world to the surface.
    pub fn run_one_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        let report = run_one_frame(&mut self.host);
        self.renderer.render(self.host.engine_mut())?;
        Ok(report)
    }

    /// Read live frame statistics for UI overlays without affecting the
    /// lower-frequency report returned by [`Self::run_one_frame`].
    pub fn current_frame_report(&self) -> FrameReport {
        self.host.current_frame_report()
    }
}

/// Result of one [`run_one_frame`] call when the reporting interval elapses.
#[derive(Debug, Clone, Copy)]
pub struct FrameReport {
    /// Frames per second measured over the reporting window.
    pub fps: f64,
    /// Number of live entities at report time.
    pub entity_count: usize,
}

// =============================================================================
// Free Functions
// =============================================================================

/// Build/load the game module, create the engine, and start its source watcher.
pub fn setup(module_config: GameModuleConfig) -> Result<Host, Box<dyn std::error::Error>> {
    // Step 1: Resolve the workspace root and print the selected configuration.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Cannot determine workspace root")?
        .to_path_buf();

    print_startup_configuration(&workspace_root, &module_config);

    if matches!(
        module_config.backend,
        GameModuleBackend::NativeLibrary { .. }
    ) {
        cleanup_temporary_files(&workspace_root);
    }

    // Step 2: Construct the engine and its stable API table.
    // EngineApi stores a raw pointer into this allocation, so the engine must
    // reach its final stable address before the API table is constructed.
    let mut engine = Box::new(Engine::new());
    engine.set_parallel_execution(true);
    let engine_api = EngineApi::new(&mut engine);

    // Step 3: Build and load the game module, then start its source watcher.
    let loaded_game = LoadedGame::start(&mut engine, &engine_api, &workspace_root, &module_config)?;

    let reload_flag = Arc::new(AtomicBool::new(false));
    spawn_file_watcher(
        workspace_root.clone(),
        &module_config,
        Arc::clone(&reload_flag),
    )?;

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
        last_report: Instant::now(),
        last_measured_fps: 0.0,
    })
}

/// Set up the engine, game module, hot reload, and renderer together.
///
/// A frontend owns its platform event loop and supplies its cloneable window
/// handle. The engine creates exactly one surface for that window, while the
/// returned [`RenderingHost`] owns the renderer for the rest of its lifetime.
#[cfg(feature = "rendering")]
pub fn setup_rendering<W>(
    module_config: GameModuleConfig,
    window: W,
    width: u32,
    height: u32,
) -> Result<RenderingHost, Box<dyn std::error::Error>>
where
    W: RendererWindow + 'static,
{
    let host = setup(module_config)?;
    let renderer = Renderer::new(window, width, height)?;
    Ok(RenderingHost { host, renderer })
}

/// Process hot reloads, execute one scheduler frame, and update FPS tracking.
///
/// Returns a report roughly every three seconds for a frontend to print or
/// display; all other frames return `None`.
pub fn run_one_frame(host: &mut Host) -> Option<FrameReport> {
    // Step 1: Process a pending hot reload before running systems.
    if host.reload_flag.swap(false, Ordering::Acquire) {
        println!();
        println!("[host] === Hot-reload triggered ===");
        host.loaded_game.reload(
            &mut host.engine,
            &host.engine_api,
            &host.workspace_root,
            &host.module_config,
        );
    }

    // Step 2: Poll the managed loader for an assembly swap.
    // The managed loader watches the built assembly instead of source files.
    host.loaded_game.poll_managed_reload();

    // Step 3: Execute one scheduler frame.
    if let Err(errors) = host.engine.process_frame() {
        eprintln!("[host] Frame error: {errors:?}");
    }

    // Step 4: Invoke the native compatibility update after scheduler systems.
    // Managed games run entirely as scheduler systems. Native games retain
    // this compatibility update hook after their scheduled work.
    host.loaded_game.update(&host.engine_api);

    // Step 5: Track and report FPS over the three-second window.
    host.frame_count += 1;
    let elapsed = host.last_report.elapsed().as_secs_f64();
    if elapsed < 3.0 {
        return None;
    }

    let fps = host.frame_count as f64 / elapsed;
    let report = FrameReport {
        fps,
        entity_count: host.engine.world().entity_count(),
    };
    host.last_measured_fps = fps;
    host.frame_count = 0;
    host.last_report = Instant::now();
    Some(report)
}

/// Print the selected backend before any build output starts streaming.
fn print_startup_configuration(workspace_root: &Path, module_config: &GameModuleConfig) {
    println!("=== ECS Host ===");
    println!("Workspace:   {}", workspace_root.display());
    println!(
        "Game module: {} ({:?})",
        module_config.name, module_config.backend
    );
    println!("Build cmd:   {}", module_config.build_command.join(" "));
    println!("Watch dir:   {}", module_config.watch_directory);
    println!();
}
