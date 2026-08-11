//! Engine ownership and frontend-facing frame orchestration.
//!
//! This module contains the stable API used by `standalone`, `editor`, and
//! other host binaries. Backend-specific loading stays behind [`LoadedGame`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "rendering")]
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Instant;

use ecs_hybrid::{Engine, EngineApi};
#[cfg(feature = "rendering")]
use ecs_hybrid::{Renderer, RendererError, RendererWindow};

use crate::game_module::LoadedGame;
use crate::native_library::cleanup_temporary_files;
use crate::watcher::spawn_file_watcher;
use crate::{GameModuleBackend, GameModuleConfig};

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
}

/// Host state with the engine renderer attached to one native window surface.
///
/// Keeping the renderer beside [`Host`] makes its creation and lifetime part
/// of host setup. Executable crates never construct or retain GPU resources.
#[cfg(feature = "rendering")]
pub struct RenderingHost {
    host: Host,
    renderer: Renderer,
    redraw_pump: Option<RedrawPump>,
}

#[cfg(feature = "rendering")]
impl RenderingHost {
    /// Install an asynchronous request callback for continuous rendering.
    ///
    /// Some UI frameworks force their event loop into a waiting state and
    /// coalesce redraws requested from inside a redraw handler. The pump calls
    /// `request_redraw` from a helper thread after every presented frame,
    /// waking that event loop without moving engine or GPU work off its owner
    /// thread. At most one request can be pending.
    pub fn start_continuous_rendering(&mut self, request_redraw: impl Fn() + Send + 'static) {
        self.redraw_pump = Some(RedrawPump::new(request_redraw));
        self.request_next_frame();
    }

    /// Request one frame through the installed continuous redraw pump.
    pub fn request_next_frame(&self) {
        if let Some(pump) = &self.redraw_pump {
            pump.request();
        }
    }

    /// Forward a physical window resize to the engine renderer.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.renderer.resize(width, height);
    }

    /// Execute one ECS frame and present its resulting world to the surface.
    pub fn run_one_frame(&mut self) -> Result<Option<FrameReport>, RendererError> {
        let report = run_one_frame(&mut self.host);
        self.renderer.render(self.host.engine_mut())?;
        self.request_next_frame();
        Ok(report)
    }
}

/// Single-slot bridge that wakes a waiting platform event loop once per frame.
#[cfg(feature = "rendering")]
struct RedrawPump {
    sender: Option<SyncSender<()>>,
}

#[cfg(feature = "rendering")]
impl RedrawPump {
    /// Start a helper that translates frame completions into redraw requests.
    fn new(request_redraw: impl Fn() + Send + 'static) -> Self {
        let (sender, receiver) = sync_channel(1);
        std::thread::Builder::new()
            .name("ecs-redraw-pump".into())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    request_redraw();
                }
            })
            .expect("failed to start redraw pump");
        Self {
            sender: Some(sender),
        }
    }

    /// Queue a redraw unless one is already waiting to be delivered.
    fn request(&self) {
        let Some(sender) = &self.sender else {
            return;
        };
        match sender.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                eprintln!("[render] Continuous redraw pump stopped unexpectedly");
            }
        }
    }
}

#[cfg(feature = "rendering")]
impl Drop for RedrawPump {
    /// Disconnect the helper without blocking the platform event-loop thread.
    fn drop(&mut self) {
        // The worker captures only its callback, which should hold a Weak
        // window handle. Disconnecting lets it exit after any in-flight
        // request without risking a join/request_redraw shutdown deadlock.
        self.sender.take();
    }
}

/// Build/load the game module, create the engine, and start its source watcher.
pub fn setup(module_config: GameModuleConfig) -> Result<Host, Box<dyn std::error::Error>> {
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

    // EngineApi stores a raw pointer into this allocation, so the engine must
    // reach its final stable address before the API table is constructed.
    let mut engine = Box::new(Engine::new());
    engine.set_parallel_execution(true);
    let engine_api = EngineApi::new(&mut engine);

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
    Ok(RenderingHost {
        host,
        renderer,
        redraw_pump: None,
    })
}

/// Result of one [`run_one_frame`] call when the reporting interval elapses.
#[derive(Debug, Clone, Copy)]
pub struct FrameReport {
    pub fps: f64,
    pub entity_count: usize,
}

/// Process hot reloads, execute one scheduler frame, and update FPS tracking.
///
/// Returns a report roughly every three seconds for a frontend to print or
/// display; all other frames return `None`.
pub fn run_one_frame(host: &mut Host) -> Option<FrameReport> {
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

    // The managed loader watches the built assembly instead of source files.
    host.loaded_game.poll_managed_reload();

    if let Err(errors) = host.engine.process_frame() {
        eprintln!("[host] Frame error: {errors:?}");
    }

    // Managed games run entirely as scheduler systems. Native games retain
    // this compatibility update hook after their scheduled work.
    host.loaded_game.update(&host.engine_api);

    host.frame_count += 1;
    let elapsed = host.last_report.elapsed().as_secs_f64();
    if elapsed < 3.0 {
        return None;
    }

    let report = FrameReport {
        fps: host.frame_count as f64 / elapsed,
        entity_count: host.engine.world().entity_count(),
    };
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
