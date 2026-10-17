//! Native editor executable: the egui frontend's frame loop, window and
//! device (Stage 8).
//!
//! # Responsibilities
//!
//! - Own the `winit` event loop, the main window, the scene views' child
//!   windows, and the editor's own GPU device that draws the UI (D8, D19).
//! - Host the project and its extensions - including the reloadable egui
//!   shell module - through `pill_host` (D15).
//! - Drive one world frame per redraw: push the window events into the
//!   engine input, publish [`EguiShellInput`], run the host's frame (the
//!   shell's egui pass runs inside it), then draw the shell's
//!   [`EguiShellFrame`] through the composition overlay.
//! - Publish the shared selection as the renderer's outline set, and turn
//!   scene clicks into picks whose answers move it (Stage 13).
//! - Publish the transform gizmo's handles for the selection and run the
//!   drags their picks start (Stage 15).
//!
//! # Design
//!
//! The window redraws on request: every presented frame asks for the next
//! one, which keeps the engine stepping while the window is visible and lets
//! the compositor idle a hidden one.
//!
//! The shell boundary is plain data in the world (the shared
//! `pill_ui_egui::boundary` resources), so the frontend and the reloadable
//! shell module never share a type that could dangle across a reload. When
//! no shell module is loaded, the runner draws a minimal egui fallback of
//! its own, so the window is never left without a UI.

// Standard library
use std::sync::Arc;

// External crates
use pill_core::platform::futures::block_on;
use pill_core::platform::Instant;
use pill_core::telemetry::telemetry_target;
use pill_core::{info, warn};
use pill_editor_core_egui::assets::{self, AssetDialog};
use pill_editor_core_egui::commands::{
    ensure_command_resources, read_gizmo_settings, write_gizmo_preferences, EditorPreferences,
    PendingEditorCommands, PREFERENCES_FILE,
};
use pill_editor_core_egui::editor_state::{
    AssetDetail, AssetListing, EditorCommand, EditorSelection, EditorSnapshot, StandaloneTypeInfo,
};
use pill_editor_core_egui::entities;
use pill_editor_core_egui::gizmo::{self, GizmoMode, GizmoSettings, GizmoSpace, GizmoState};
use pill_editor_core_egui::history::{CommandHistory, HistoryEntry, HistoryEntryKind};
use pill_editor_core_egui::inspector;
use pill_editor_core_egui::layout::{
    layout_directory, EditorTab, BOTTOM_ROW_FRACTION, SIDE_COLUMN_FRACTION,
};
use pill_editor_core_egui::preferences::PreferencesView;
use pill_editor_core_egui::session::EguiShellState;
use pill_editor_core_egui::systems;
use pill_engine_core::common_components::TRANSFORM_SHARED_NAME;
use pill_engine_core::debug_draw::DebugDraw;
use pill_engine_core::input::{ButtonState, MouseButton};
use pill_engine_core::Entity;
use pill_engine_core::FieldValue;
use pill_engine_core::InputEvent;
use pill_host::{HostConfig, RenderingHost};
use pill_input::winit_events::translate_window_event;
use pill_runtime::{
    frame_grid, CameraBinding, CameraComponent, FrameDriver, PickHit, PickRequests, PickResults,
    SelectionOutlines, SurfaceCameras, SurfaceId, SurfaceOptions, TransformComponent,
};
use pill_ui_egui::boundary::{
    frame_from_output, EguiShellFrame, EguiShellInput, ShellRect, SCENE_VIEWPORTS,
};
use pill_ui_egui::egui;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::raw_window_handle::HasWindowHandle;
use winit::window::{Window, WindowId};

// Current crate
use crate::frontend::EditorError;
use crate::overlay::OverlaySurface;
use crate::test_channel::{self, TestChannel};

// =============================================================================
// Constants
// =============================================================================

/// The shell module's crate name - the editor-only module's convention.
const SHELL_MODULE_NAME: &str = "pill_editor_shell_egui_editor";

/// The window's initial size in logical pixels.
const INITIAL_WINDOW_SIZE: LogicalSize<f64> = LogicalSize::new(1280.0, 800.0);

/// The scene view's initial size in logical pixels.
const INITIAL_SCENE_SIZE: LogicalSize<f64> = LogicalSize::new(640.0, 480.0);

/// The scene views' surface ids; the game view is the renderer's primary
/// surface, shown by the main window itself. One per scene tab.
const SCENE_SURFACES: [SurfaceId; SCENE_VIEWPORTS] = [SurfaceId(1), SurfaceId(2)];

/// The scene view's rectangle before the shell's first scene rect arrives:
/// the dock's default layout's center, in logical pixels. The shell's
/// measured scene rect replaces it on the first frame.
const SCENE_DEFAULT_TOP_MARGIN: f64 = 32.0;
const SCENE_DEFAULT_MARGIN: f64 = 4.0;

/// How much of each new frame feeds the displayed FPS, the rest being the
/// running average.
const FPS_SMOOTHING: f32 = 0.1;

/// The gizmo's handle length as a fraction of the camera distance: a
/// constant screen size that neither shrinks away nor swallows the scene.
const GIZMO_SCREEN_FRACTION: f32 = 0.18;

/// Where along a straight handle the scripted grab presses it, as a
/// fraction of its length.
const GIZMO_PRESS_FRACTION: f32 = 0.92;

/// Frames between gizmo anchor logs while the handle layout stands still.
const GIZMO_HEARTBEAT_FRAMES: u64 = 210;

/// Frames between periodic asset-listing walks; a file command refreshes
/// the listing at once instead of waiting for the cadence.
const ASSET_REFRESH_FRAMES: u64 = 120;

/// How many asset rows the listing keeps.
const ASSET_ROWS_MAX: usize = 256;

/// The editor's format: the overlay's swapchain and egui's renderer must
/// share it, and composition swapchains offer it.
const EDITOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

// =============================================================================
// Run
// =============================================================================

/// Run the editor until the last window closes.
///
/// `log_buffer` is the console's capture ring; the egui shell's console
/// panel reads it once it is ported (follow-up work).
pub fn run(log_buffer: pill_core::telemetry::capture::LogBuffer) -> Result<(), EditorError> {
    let _ = log_buffer;
    let event_loop =
        EventLoop::new().map_err(|source| EditorError::EventLoopCreation { source })?;
    let mut application = EditorApplication { window: None };
    event_loop
        .run_app(&mut application)
        .map_err(|source| EditorError::EventLoopRun { source })
}

/// The `winit` application: one editor window, created on resume.
struct EditorApplication {
    /// The window and its host, once the loop resumed.
    window: Option<EditorWindow>,
}

impl ApplicationHandler for EditorApplication {
    /// Create the editor window on the first resume.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        match EditorWindow::open(event_loop) {
            Ok(window) => {
                window.window.request_redraw();
                self.window = Some(window);
            }
            Err(error) => {
                pill_core::error!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] window creation failed"
                );
                event_loop.exit();
            }
        }
    }

    /// Route the window events: main-window frames drive everything, the
    /// scene and game child windows contribute their own input to the
    /// engine.
    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(editor) = &mut self.window else {
            return;
        };
        let is_main = window_id == editor.window.id();
        let child_view = editor.views.iter().position(|view| {
            view.as_ref()
                .is_some_and(|view| view.window.window.id() == window_id)
        });
        let is_game = editor
            .game_surface
            .as_ref()
            .is_some_and(|game| game.window.id() == window_id);
        if !is_main && child_view.is_none() && !is_game {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                if is_main {
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                if !is_main {
                    return;
                }
                match editor.frame() {
                    Ok(should_exit) => {
                        if should_exit {
                            event_loop.exit();
                        } else {
                            editor.window.request_redraw();
                        }
                    }
                    Err(error) => {
                        pill_core::error!(
                            target: telemetry_target::ENGINE,
                            error = %error,
                            "[editor] frame failed"
                        );
                        event_loop.exit();
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(view) = child_view {
                    editor.resize_scene(view, size.width, size.height);
                } else if is_game {
                    editor.resize_game(size.width, size.height);
                } else {
                    editor.resize(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                if is_main {
                    editor.scale_factor = editor.window.scale_factor() as f32;
                    let size = editor.window.inner_size();
                    editor.resize(size.width, size.height);
                }
            }
            WindowEvent::Focused(focused) => {
                if is_main {
                    editor.focused = focused;
                }
            }
            other => {
                // The scene and game windows' pointer positions are
                // relative to their own client areas, while the engine -
                // which also feeds the shell's egui pass - works in the
                // main window's client coordinates. The window's placed
                // origin bridges the two, so hovering and clicking the
                // overlay strip works.
                if let WindowEvent::KeyboardInput { event, .. } = &other {
                    editor.gizmo_shortcut(event);
                }
                if let Some(view) = child_view {
                    editor.scene_window_event(view, &other);
                }
                let origin = match child_view {
                    Some(view) => editor.view_origin(view),
                    None if is_game => editor.game_origin(),
                    None => (0.0, 0.0),
                };
                for event in translate_window_event(&other) {
                    editor.pending_events.push(offset_pointer(event, origin));
                }
            }
        }
    }
}

// =============================================================================
// EditorWindow
// =============================================================================

/// The editor's window, its host, and everything the frame loop owns.
struct EditorWindow {
    /// The main window: the shell's UI, and the game view's primary surface
    /// only when the game window could not be created.
    window: Arc<Window>,
    /// The editor's own GPU device, which draws the UI; the project's
    /// renderer keeps its own device inside the host (D8, D19).
    device: wgpu::Device,
    /// The queue of the editor's device.
    queue: wgpu::Queue,
    /// The editor's instance, kept for late overlay creation.
    instance: wgpu::Instance,
    /// The adapter the editor's device runs on, kept for late overlay
    /// creation.
    adapter: wgpu::Adapter,
    /// The host: the project, its extensions and the shell module.
    host: RenderingHost,
    /// The scene views' child windows, one per scene tab: the Scene tab's,
    /// then the second viewport's. A slot is `None` when its window could
    /// not be created.
    views: [Option<SceneView>; SCENE_VIEWPORTS],
    /// The game viewport's window, when one could be created: it carries
    /// the primary surface, so the project's renderer presents the game
    /// view there. `None` leaves the primary on the main window.
    game_surface: Option<SurfaceWindow>,
    /// The editor's camera: what both scene viewports draw through, and the
    /// basis every scene ray and screen projection here is built from.
    camera: EditorCamera,
    /// egui's own renderer on the editor's device.
    egui_renderer: egui_wgpu::Renderer,
    /// The composition overlay the UI is presented through; `None` until it
    /// is created, and left `None` when the platform refuses it.
    overlay: Option<OverlaySurface>,
    /// Frames between overlay creation attempts after a failure.
    overlay_retry_frame: u64,
    /// Whether overlay creation failed and was logged already.
    overlay_failed_logged: bool,
    /// The editor's fallback context, used while no shell module is loaded.
    fallback_context: egui::Context,
    /// Whether the fallback UI was reported already.
    reported_fallback: bool,
    /// The window events not yet pushed into the engine's input.
    pending_events: Vec<InputEvent>,
    /// The last pointer position each scene window reported, in its own
    /// client-area physical pixels; `None` before its first move.
    scene_cursor: [Option<(f32, f32)>; SCENE_VIEWPORTS],
    /// The scene left press waiting for its release: the view and the
    /// press position. The release turns it into a pick.
    scene_press: Option<(usize, f32, f32)>,
    /// The pick request an answer is pending for, matched by id when the
    /// renderer answers frames later.
    pending_pick: Option<u64>,
    /// The entity the renderer's outline set currently names, so the world
    /// resource is written once per selection change.
    outlined_entity: Option<Entity>,
    /// The overlay chips' rectangles the shell reported last frame, in
    /// logical points: clicks inside them are the UI's, not the scene's.
    overlay_regions: Vec<ShellRect>,
    /// The gizmo drag in flight, when one is.
    gizmo_drag: Option<GizmoDrag>,
    /// The last logged gizmo anchor header (mode, space, x, y): the log
    /// line appears on changes and on the heartbeat.
    gizmo_logged: Option<(GizmoMode, GizmoSpace, i32, i32)>,
    /// The pick ids a scripted gizmo grab pressed: the first answer that
    /// names a handle starts the drag, a full miss refuses it.
    synthetic_press_ids: Vec<u64>,
    /// The pixel delta a scripted grab applies once its pick answers.
    synthetic_gizmo_pending: Option<(f32, f32)>,
    /// The view a scripted grab belongs to.
    synthetic_gizmo_view: Option<usize>,
    /// The asset browser's rows, refreshed on a cadence and at once after
    /// any file command (the Assets tab draws them).
    asset_entries: Vec<AssetListing>,
    /// The live standalone asset types the create dialog offers.
    standalone_types: Vec<StandaloneTypeInfo>,
    /// Whether a file command changed the tree: the next frame's listing
    /// refresh walks it at once.
    asset_listing_dirty: bool,
    /// The selected asset's detail for the Inspector's asset section.
    asset_detail: Option<AssetDetail>,
    /// The path `asset_detail` was captured for, so the capture follows the
    /// selection without a disk read every frame.
    asset_detail_path: Option<String>,
    /// The preferences file's text last read; the live reader applies the
    /// history limits and the snapping steps when it changes (Stage 20).
    preferences_text: Option<String>,
    /// The parsed preferences document the snapshot's view and the scripted
    /// dump build on.
    preferences_document: serde_json::Map<String, serde_json::Value>,
    /// The project directory name the per-project preferences are keyed by.
    gizmo_project: Option<String>,
    /// The gizmo settings last written to the preferences file, so an
    /// unchanged tool writes nothing.
    gizmo_persisted: Option<GizmoSettings>,
    /// Whether the main window currently has focus.
    focused: bool,
    /// The main window's client size in physical pixels.
    size: (u32, u32),
    /// The window's scale factor, as the shell's logical pixels run at.
    scale_factor: f32,
    /// World frames run since startup.
    frame_index: u64,
    /// When the last frame started, for the smoothed FPS.
    last_frame_time: Option<Instant>,
    /// The smoothed frame rate, logged with the heartbeat every 210 frames.
    fps: f32,
    /// When the run started, against which the test deadline is measured.
    started: Instant,
    /// Whether egui claimed the pointer in the last frame, logged on change
    /// so a suite can see the routing decision.
    claimed_pointer: bool,
    /// Whether egui claimed the keyboard in the last frame.
    claimed_keyboard: bool,
    /// The scripted command channel, when the environment names one.
    test_channel: Option<TestChannel>,
    /// A scripted left-button drag in flight, when one is.
    scripted_drag: Option<ScriptedDrag>,
    /// Quit after this many seconds, when the environment asks for it.
    test_seconds: Option<f64>,
    /// A quit the scripted channel or the deadline asked for.
    quit: bool,
}

/// A scripted left-button drag in flight: where the press lands, where the
/// pointer moves to, and how far along the drag is.
///
/// The stages run one per frame - press, move, release - because egui only
/// reports a drag when the pointer moves while the button is down across
/// frames; a press and a release inside one frame are a click.
struct ScriptedDrag {
    /// The press position, in the main window's client pixels.
    from: (f32, f32),
    /// The position the single move goes to.
    to: (f32, f32),
    /// 0 = press pending, 1 = move pending, 2 = release pending.
    stage: u8,
}

impl EditorWindow {
    /// Create the window, the editor's device, the host and the views.
    fn open(event_loop: &ActiveEventLoop) -> Result<EditorWindow, EditorError> {
        // Step 1: the main window.
        let attributes = Window::default_attributes()
            .with_title("Pill Editor (egui)")
            .with_inner_size(INITIAL_WINDOW_SIZE);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|source| EditorError::WindowCreation { source })?,
        );
        let size = window.inner_size();
        let size = (size.width.max(1), size.height.max(1));
        let scale_factor = window.scale_factor() as f32;
        // Step 2: the editor's own instance, adapter and device. The editor
        // draws only through the composition overlay, which is DX12-only,
        // so the editor's instance asks for DX12 alone; the project's
        // renderer keeps its own instance inside the host.
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..Default::default()
        });
        let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|source| EditorError::AdapterRequest { source })?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("pill editor (egui)"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|source| EditorError::DeviceRequest { source })?;
        // Step 3: the UI's renderer and the composition overlay, best
        // effort. The overlay must exist BEFORE the project's renderer
        // creates its swapchain on this window: a DirectComposition target
        // cannot be created for a window that is already composed, and
        // attaching the renderer composes it
        // (`DCOMPOSITION_ERROR_WINDOW_ALREADY_COMPOSED`). Without the
        // overlay the window shows the game view alone; the failure is
        // logged once and retried rarely.
        let egui_renderer = egui_wgpu::Renderer::new(&device, EDITOR_FORMAT, None, 1, false);
        let overlay = match OverlaySurface::new(
            &instance,
            &adapter,
            &device,
            Arc::clone(&window),
            EDITOR_FORMAT,
            size,
            scale_factor,
        ) {
            Ok(overlay) => Some(overlay),
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] composition overlay unavailable; the UI will not be drawn"
                );
                None
            }
        };
        // Step 4: the game viewport's window first - it carries the primary
        // surface, so the project renders through its own camera into a
        // child window the Game tab shows through. A refusal leaves the
        // primary on the main window (the pre-game-view posture) and the
        // Game tab merely mirrors what is already drawn there.
        let game_surface = SurfaceWindow::open(event_loop, &window, "Pill Editor Game", "game")
            .map_err(|error| {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] game view window refused; the primary surface stays on the main window"
                )
            })
            .ok();
        let (primary_window, primary_size) = match &game_surface {
            Some(game) => (Arc::clone(&game.window), game.size()),
            None => (Arc::clone(&window), size),
        };
        // Step 4b: the host: the project, its extensions and the egui shell
        // module, with the renderer attached to the primary window.
        let config = HostConfig::from_environment_for_editor_frontend("egui")
            .map_err(pill_core::error::HostError::from)?;
        let mut host =
            pill_host::setup_rendering(config, primary_window, primary_size.0, primary_size.1)?;
        info!(
            target: telemetry_target::ENGINE,
            detached = game_surface.as_ref().is_some_and(|game| game.detached),
            width = primary_size.0,
            height = primary_size.1,
            "[editor] game surface attached"
        );
        // The gizmo tool's starting state (Stage 20): the project's own
        // remembered choice, the file's last-used values, or the defaults.
        // The project's directory name keys the per-project entry, and the
        // executable is the one place that knows it.
        let gizmo_project = std::env::var("PROJECT_PATH").ok().and_then(|path| {
            std::path::Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
        let gizmo_settings = match &gizmo_project {
            Some(project) => read_gizmo_settings(&layout_directory(), project),
            None => GizmoSettings::default(),
        };
        // The frontend owns the drags, a mode change comes from the
        // shortcuts or the scripted channel, and the file's settings stand
        // until one arrives.
        let world = host.host_mut().engine_mut().world_mut();
        world.register_resource::<GizmoState>();
        if world.get_resource::<GizmoState>().is_none() {
            world.insert_resource(GizmoState {
                settings: gizmo_settings,
            });
        }
        // The command resources the assets actions and the scripted channel
        // queue into (the registry, the inbox, the pending batches and the
        // undo history); the shell re-claims the same set on load.
        ensure_command_resources(world);
        // Step 5: the scene views' child windows and their renderer
        // surfaces, with the editor camera they draw through. Both start
        // hidden; each shows when the dock shows its tab.
        let camera = EditorCamera::default();
        let mut views: [Option<SceneView>; SCENE_VIEWPORTS] = [None, None];
        for (view, slot) in views.iter_mut().enumerate() {
            let Some(mut scene) = SceneView::open(event_loop, &window, view) else {
                continue;
            };
            let scene_size = scene.window.size();
            scene.attach(&mut host, &camera, scene_size);
            *slot = Some(scene);
        }
        // The scene grid (Stage 23): the editor's view-bound viewports draw
        // it, so it starts on; the toolbox's grid row and the viewport menu
        // toggle it through `SetGrid`, which flips this same switch.
        {
            let world = host.host_mut().engine_mut().world_mut();
            if world.get_resource::<SurfaceCameras>().is_none() {
                world.insert_resource(SurfaceCameras::default());
            }
            if let Some(cameras) = world.get_resource_mut::<SurfaceCameras>() {
                cameras.grid.enabled = true;
            }
        }
        let mut editor = EditorWindow {
            window,
            device,
            queue,
            instance,
            adapter,
            host,
            views,
            game_surface,
            camera,
            egui_renderer,
            overlay,
            overlay_retry_frame: 0,
            overlay_failed_logged: true,
            fallback_context: {
                // The fallback draws the same theme as the shell, so a
                // missing shell module does not flip the look.
                let context = egui::Context::default();
                pill_ui_egui::style::apply(&context);
                context
            },
            reported_fallback: false,
            pending_events: Vec::new(),
            scene_cursor: [None; SCENE_VIEWPORTS],
            scene_press: None,
            pending_pick: None,
            outlined_entity: None,
            overlay_regions: Vec::new(),
            gizmo_drag: None,
            gizmo_logged: None,
            synthetic_press_ids: Vec::new(),
            synthetic_gizmo_pending: None,
            synthetic_gizmo_view: None,
            asset_entries: Vec::new(),
            standalone_types: Vec::new(),
            asset_listing_dirty: true,
            asset_detail: None,
            asset_detail_path: None,
            preferences_text: None,
            preferences_document: serde_json::Map::new(),
            gizmo_project,
            gizmo_persisted: None,
            focused: true,
            size,
            scale_factor,
            frame_index: 0,
            last_frame_time: None,
            fps: 0.0,
            started: Instant::now(),
            claimed_pointer: false,
            claimed_keyboard: false,
            test_channel: TestChannel::from_environment(),
            scripted_drag: None,
            test_seconds: test_channel::seconds_from_environment(),
            quit: false,
        };
        editor.place_views();
        info!(
            target: telemetry_target::ENGINE,
            width = size.0,
            height = size.1,
            "[editor] window created"
        );
        Ok(editor)
    }

    /// Handle a main-window resize.
    fn resize(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
        // The primary surface lives on the game view's window when it
        // exists; only the fallback posture sizes it from the main window.
        if self.game_surface.is_none() {
            self.host.resize(width.max(1), height.max(1));
        }
        if let Some(overlay) = &mut self.overlay {
            overlay.resize(&self.device, self.size);
        }
        self.place_views();
    }

    /// One scene view's client-area origin in the main window's client
    /// coordinates, in physical pixels: the position its last placement
    /// applied. A resize never moves the window, so the origin stands even
    /// while the placement cache is cleared mid-layout; a drag across the
    /// separator therefore never sees the pointer jump.
    fn view_origin(&self, view: usize) -> (f32, f32) {
        match &self.views[view] {
            Some(scene) => (scene.window.origin.0 as f32, scene.window.origin.1 as f32),
            None => (0.0, 0.0),
        }
    }

    /// Handle a scene window's resize: the view's surface reconfigures with
    /// its window, like the native editor's view surfaces (D29) - a stale
    /// render target draws the scene at the wrong size inside the tab. The
    /// placement cache is cleared, so the next layout pass re-applies the
    /// dock's rectangle.
    fn resize_scene(&mut self, view: usize, width: u32, height: u32) {
        let Some(scene) = &mut self.views[view] else {
            return;
        };
        scene.window.placed = None;
        let (width, height) = (width.max(1), height.max(1));
        self.host.resize_surface(scene.surface_id, width, height);
        info!(
            target: telemetry_target::ENGINE,
            width,
            height,
            view,
            "[editor] scene surface resized"
        );
    }

    /// Handle the game window's resize: the primary surface reconfigures
    /// with its window, like the scene view surfaces - a stale render
    /// target draws the project at the wrong size inside the Game tab.
    /// `resize_surface` does nothing for the primary, so the host's main
    /// resize is the one that reaches it. The placement cache is cleared,
    /// so the next layout pass re-applies the dock's rectangle.
    fn resize_game(&mut self, width: u32, height: u32) {
        let Some(game) = &mut self.game_surface else {
            return;
        };
        game.placed = None;
        let (width, height) = (width.max(1), height.max(1));
        self.host.resize(width, height);
        info!(
            target: telemetry_target::ENGINE,
            width,
            height,
            "[editor] game surface resized"
        );
    }

    /// The game window's client-area origin in the main window's client
    /// coordinates, in physical pixels: the same bridge the scene views
    /// use for pointer positions.
    fn game_origin(&self) -> (f32, f32) {
        match &self.game_surface {
            Some(game) => (game.origin.0 as f32, game.origin.1 as f32),
            None => (0.0, 0.0),
        }
    }

    /// Place the first scene view at the dock's default layout's center
    /// inside the client area, until the shell's first scene rect arrives;
    /// the other views stay hidden until their tabs show.
    fn place_views(&mut self) {
        let Some(scene) = &mut self.views[0] else {
            return;
        };
        let (width, height) = (self.size.0 as i32, self.size.1 as i32);
        let margin = (SCENE_DEFAULT_MARGIN * self.scale_factor as f64) as i32;
        let side = (width as f64 * SIDE_COLUMN_FRACTION as f64) as i32;
        let top = (SCENE_DEFAULT_TOP_MARGIN * self.scale_factor as f64) as i32 + margin;
        let bottom = (height as f64 * BOTTOM_ROW_FRACTION as f64) as i32;
        let left = side + margin;
        let width = (width - 2 * side - 2 * margin).max(1) as u32;
        let height = (height - top - bottom - margin).max(1) as u32;
        scene.window.place(left, top, width, height);
    }

    /// Place one scene view at the dock's scene rectangle, in logical
    /// points, converted to the client area's physical pixels.
    fn place_view_rect(&mut self, view: usize, rect: ShellRect) {
        let (x, y, width, height) = self.view_rect_pixels(rect);
        let Some(scene) = &mut self.views[view] else {
            return;
        };
        scene.window.place(x, y, width, height);
    }

    /// Place the game window at the dock's game rectangle, in logical
    /// points, converted to the client area's physical pixels.
    fn place_game_rect(&mut self, rect: ShellRect) {
        let (x, y, width, height) = self.view_rect_pixels(rect);
        let Some(game) = &mut self.game_surface else {
            return;
        };
        game.place(x, y, width, height);
    }

    /// A dock rectangle in logical points as the client area's physical
    /// pixels, clamped inside it: the dock's rect can overshoot the window
    /// while a split is being dragged.
    fn view_rect_pixels(&self, rect: ShellRect) -> (i32, i32, u32, u32) {
        let scale = self.scale_factor as f64;
        let x = (rect.x as f64 * scale).round() as i32;
        let y = (rect.y as f64 * scale).round() as i32;
        let width = (rect.width as f64 * scale).round().max(1.0) as i32;
        let height = (rect.height as f64 * scale).round().max(1.0) as i32;
        let (client_width, client_height) = (self.size.0 as i32, self.size.1 as i32);
        let x = x.clamp(0, (client_width - 1).max(0));
        let y = y.clamp(0, (client_height - 1).max(0));
        let width = width.min((client_width - x).max(1)) as u32;
        let height = height.min((client_height - y).max(1)) as u32;
        (x, y, width, height)
    }

    /// Hide one scene view's window: its tab is not shown this pass
    /// (another tab of its group is active, or the tab was closed).
    fn hide_view(&mut self, view: usize) {
        if let Some(scene) = &mut self.views[view] {
            scene.window.hide();
        }
    }

    /// Hide the game window: its tab is not shown this pass, so no child
    /// window may paint over the tab that is.
    fn hide_game(&mut self) {
        if let Some(game) = &mut self.game_surface {
            game.hide();
        }
    }

    /// Route one scene window's event (Stage 13, Stage 15): record its
    /// pointer, feed a gizmo drag in flight its moves, and turn a left
    /// press-and-release into a scene pick. The pick is asked on the
    /// release, so the press point is what gets picked; a press that began
    /// over the overlay's chips is the UI's and never becomes one.
    fn scene_window_event(&mut self, view: usize, event: &WindowEvent) {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.scene_cursor[view] = Some((position.x as f32, position.y as f32));
                // While a gizmo drag runs, every move re-reads its ray.
                if self.gizmo_drag.is_some() {
                    self.update_gizmo_drag(position.x as f32, position.y as f32);
                }
            }
            WindowEvent::MouseInput { state, button, .. }
                if *button == winit::event::MouseButton::Left =>
            {
                match state {
                    winit::event::ElementState::Pressed => {
                        self.scene_press = if self.scene_point_is_ui(view) {
                            None
                        } else {
                            self.scene_cursor[view].map(|(x, y)| (view, x, y))
                        };
                    }
                    winit::event::ElementState::Released => {
                        // A release while a gizmo drag runs drops it; the
                        // press that started it already asked its pick.
                        if self.gizmo_drag.is_some() {
                            self.end_gizmo_drag();
                        } else if let Some((pressed_view, x, y)) = self.scene_press.take() {
                            if pressed_view == view {
                                self.request_scene_pick(view, x, y);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether a scene view's pointer is over one of the overlay's chips:
    /// its child-local physical pixels mapped into the main window's
    /// logical points, which is where the shell reported the chips.
    fn scene_point_is_ui(&self, view: usize) -> bool {
        let Some((x, y)) = self.scene_cursor[view] else {
            return false;
        };
        let (origin_x, origin_y) = self.view_origin(view);
        let logical_x = (origin_x + x) / self.scale_factor;
        let logical_y = (origin_y + y) / self.scale_factor;
        self.overlay_regions
            .iter()
            .any(|region| region.contains(logical_x, logical_y))
    }

    /// The scene view whose window holds a main-window physical point, if
    /// one does: a scripted click inside it is a scene click.
    fn scene_view_at(&self, x: f32, y: f32) -> Option<usize> {
        for view in 0..self.views.len() {
            let Some(scene) = &self.views[view] else {
                continue;
            };
            if !scene.window.visible {
                continue;
            }
            let (origin_x, origin_y) = self.view_origin(view);
            let (width, height) = scene.window.size();
            let bounds = ShellRect {
                x: origin_x,
                y: origin_y,
                width: width as f32,
                height: height as f32,
            };
            if bounds.contains(x, y) {
                return Some(view);
            }
        }
        None
    }

    /// Ask the renderer what is under one physical pixel of a scene
    /// surface (Stage 13); the answer arrives frames later in the world's
    /// `PickResults`, and [`Self::drain_scene_picks`] applies it.
    fn request_scene_pick(&mut self, view: usize, x: f32, y: f32) {
        let physical_x = x.max(0.0).round() as u32;
        let physical_y = y.max(0.0).round() as u32;
        let Some(id) = self.queue_scene_pick_at(view, physical_x, physical_y) else {
            return;
        };
        self.pending_pick = Some(id);
        info!(
            target: telemetry_target::ENGINE,
            view,
            x = physical_x,
            y = physical_y,
            id,
            "[editor] pick requested"
        );
    }

    /// Queue one pick at an explicit surface pixel and return its id,
    /// leaving the answer to [`Self::drain_scene_picks`]: the shared half of
    /// the click pick and the scripted gizmo grab.
    fn queue_scene_pick_at(&mut self, view: usize, x: u32, y: u32) -> Option<u64> {
        self.views.get(view)?.as_ref()?;
        let world = self.host.host_mut().engine_mut().world_mut();
        if world.get_resource::<PickRequests>().is_none() {
            world.insert_resource(PickRequests::default());
        }
        let requests = world.get_resource_mut::<PickRequests>()?;
        Some(requests.request(SCENE_SURFACES[view], x, y))
    }

    /// Apply the answers to scene picks (Stage 13, Stage 15): an entity
    /// moves the shared selection onto it, empty space clears the
    /// selection, and a handle starts a gizmo drag. The outline follows
    /// from the selection, and the handles from it, on the next publish.
    fn drain_scene_picks(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(results) = world.get_resource_mut::<PickResults>() else {
            return;
        };
        let results = results.take();
        // A handle press starts a drag through methods that borrow all of
        // `self`, while the world borrow below is still alive; it is
        // collected here and acted on once that borrow ends: the view, the
        // handle, the press pixel and the scripted delta.
        type HandlePress = (usize, u32, f32, f32, Option<(f32, f32)>);
        let mut handle_press: Option<HandlePress> = None;
        for result in results {
            // A scripted press answers on any pixel of its neighbourhood:
            // the first that names a handle starts the drag, and only a
            // full miss refuses it.
            if let Some(index) = self
                .synthetic_press_ids
                .iter()
                .position(|id| *id == result.request.id)
            {
                self.synthetic_press_ids.remove(index);
                if let PickHit::Handle(handle) = result.hit {
                    if let Some(delta) = self.synthetic_gizmo_pending.take() {
                        let view = self.synthetic_gizmo_view.take().unwrap_or(0);
                        info!(
                            target: telemetry_target::ENGINE,
                            handle,
                            "[editor] scene pick handle"
                        );
                        handle_press = Some((
                            view,
                            handle,
                            result.request.x as f32,
                            result.request.y as f32,
                            Some(delta),
                        ));
                    }
                } else if self.synthetic_press_ids.is_empty()
                    && self.synthetic_gizmo_pending.is_some()
                {
                    self.synthetic_gizmo_pending = None;
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the press point hit no handle"
                    );
                }
                continue;
            }
            if Some(result.request.id) != self.pending_pick {
                continue;
            }
            self.pending_pick = None;
            match result.hit {
                PickHit::Entity(entity) => {
                    info!(
                        target: telemetry_target::ENGINE,
                        entity = %entity,
                        "[editor] scene pick entity"
                    );
                    if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
                        if selection.entity != Some(entity) {
                            selection.entity = Some(entity);
                            info!(
                                target: telemetry_target::ENGINE,
                                entity = %entity,
                                "[editor] selection changed"
                            );
                        }
                    }
                }
                PickHit::Nothing => {
                    info!(
                        target: telemetry_target::ENGINE,
                        "[editor] scene pick nothing"
                    );
                    if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
                        selection.entity = None;
                    }
                }
                PickHit::Handle(handle) => {
                    info!(
                        target: telemetry_target::ENGINE,
                        handle,
                        "[editor] scene pick handle"
                    );
                    let view = SCENE_SURFACES
                        .iter()
                        .position(|surface| *surface == result.request.surface)
                        .unwrap_or(0);
                    handle_press = Some((
                        view,
                        handle,
                        result.request.x as f32,
                        result.request.y as f32,
                        None,
                    ));
                }
            }
        }
        if let Some((view, handle, press_x, press_y, scripted)) = handle_press {
            self.begin_gizmo_drag(view, handle, press_x, press_y);
            if let Some((delta_x, delta_y)) = scripted {
                if self.gizmo_drag.is_some() {
                    // One frame's press, move and release, through the same
                    // projections a real drag uses.
                    self.update_gizmo_drag(press_x + delta_x, press_y + delta_y);
                    self.end_gizmo_drag();
                } else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused the scripted press"
                    );
                }
            }
        }
    }

    /// Publish the shared selection as the renderer's outline set (Stage
    /// 13): the orange outline drawn around the selected entity, whether
    /// the Entities tab or a scene pick moved the selection. One resource
    /// write per change, before the frame that draws it.
    fn publish_selection_outline(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let selected = world
            .get_resource::<EditorSelection>()
            .and_then(|selection| selection.entity);
        if selected == self.outlined_entity {
            return;
        }
        self.outlined_entity = selected;
        if world.get_resource::<SelectionOutlines>().is_none() {
            world.insert_resource(SelectionOutlines::default());
        }
        if let Some(outlines) = world.get_resource_mut::<SelectionOutlines>() {
            outlines.entities = selected.into_iter().collect();
        }
        match selected {
            Some(entity) => info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                "[editor] outline set"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                "[editor] outline cleared"
            ),
        }
    }

    /// Draw the gizmo handles for the selection (Stage 15).
    ///
    /// The contract's own primitives with handle ids set: the same frame the
    /// renderer draws is the frame it picks, so a handle a drag grabs is
    /// always the handle that was on screen. Handles keep a constant screen
    /// size - their world length follows the camera distance - and the frame
    /// they are drawn into is one frame's worth: nothing to clear here.
    fn publish_gizmo_geometry(&mut self) {
        // Read the frame's inputs first: the world borrow must end before
        // the camera helpers (which borrow `self`) run.
        let (settings, entity, transform) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let settings = world
                .get_resource::<GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default();
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.entity);
            match selected.and_then(|entity| {
                read_transform(world, entity).map(|transform| (entity, transform))
            }) {
                Some((entity, transform)) => (settings, entity, transform),
                None => {
                    // No handles on screen: the next selection logs its
                    // anchor at once instead of on the heartbeat.
                    self.gizmo_logged = None;
                    return;
                }
            }
        };
        let origin = transform.translation;
        let basis = gizmo_basis(settings.space, &transform);
        let length = self.gizmo_length(origin);
        let geometry = gizmo::build_geometry(settings.mode, origin, basis, length);
        let anchor = self.project_to_screen(origin);
        let world = self.host.host_mut().engine_mut().world_mut();
        if world.get_resource::<DebugDraw>().is_none() {
            world.insert_resource(DebugDraw::new(true));
        }
        if let Some(debug) = world.get_resource_mut::<DebugDraw>() {
            for (start, end, color, handle) in &geometry.lines {
                debug.gizmo_line(*start, *end, *color, *handle);
            }
            for (corners, color, handle) in &geometry.triangles {
                debug.gizmo_triangle(*corners, *color, *handle);
            }
        }
        // The anchor line, on a change or on the periodic cadence: what a
        // developer (and the suite) reads to know where the handles are.
        if let Some((x, y)) = anchor {
            let header = (settings.mode, settings.space, x as i32, y as i32);
            if self.gizmo_logged != Some(header)
                || self.frame_index.is_multiple_of(GIZMO_HEARTBEAT_FRAMES)
            {
                self.gizmo_logged = Some(header);
                info!(
                    target: telemetry_target::ENGINE,
                    entity = %entity,
                    mode = gizmo_mode_name(settings.mode),
                    space = gizmo_space_name(settings.space),
                    x,
                    y,
                    handles = geometry.lines.len() + geometry.triangles.len(),
                    snapped = settings.snap,
                    "[editor] gizmo anchor"
                );
            }
        }
    }

    /// The tool shortcuts: W/E/R select translate/rotate/scale - the Unity
    /// preset's bindings, the same ones the native editor defaults to.
    fn gizmo_shortcut(&mut self, event: &winit::event::KeyEvent) {
        if event.state != winit::event::ElementState::Pressed || event.repeat {
            return;
        }
        let winit::keyboard::PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let mode = match code {
            winit::keyboard::KeyCode::KeyW => GizmoMode::Translate,
            winit::keyboard::KeyCode::KeyE => GizmoMode::Rotate,
            winit::keyboard::KeyCode::KeyR => GizmoMode::Scale,
            _ => return,
        };
        self.set_gizmo_tool(mode);
    }

    /// Switch the gizmo's mode; the shortcuts, the scripted command and
    /// (later) a toolbar all land here.
    fn set_gizmo_tool(&mut self, mode: GizmoMode) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if world.get_resource::<GizmoState>().is_none() {
            world.insert_resource(GizmoState::default());
        }
        let Some(state) = world.get_resource_mut::<GizmoState>() else {
            return;
        };
        if state.settings.mode == mode {
            return;
        }
        state.settings.mode = mode;
        info!(
            target: telemetry_target::ENGINE,
            mode = gizmo_mode_name(mode),
            "[editor] gizmo tool"
        );
    }

    /// The first visible scene view, the scripted drag's default target.
    fn visible_scene_view(&self) -> Option<usize> {
        (0..self.views.len()).find(|view| {
            self.views[*view]
                .as_ref()
                .is_some_and(|scene| scene.window.visible)
        })
    }

    /// How long the handles are, in world units, at `origin`: a constant
    /// fraction of the camera distance, so the gizmo neither shrinks away
    /// nor swallows the scene.
    fn gizmo_length(&self, origin: [f32; 3]) -> f32 {
        (gizmo::distance(self.camera.eye(), origin) * GIZMO_SCREEN_FRACTION).clamp(0.05, 50.0)
    }

    /// One scene view's surface size in physical pixels.
    fn scene_pixel_size(&self, view: usize) -> Option<(f32, f32)> {
        let scene = self.views.get(view)?.as_ref()?;
        let (width, height) = scene.window.size();
        Some((width as f32, height as f32))
    }

    /// The cursor ray through one physical pixel of one scene view's
    /// surface.
    ///
    /// Built from the editor camera's basis and the contract camera's
    /// vertical field of view - the same numbers the renderer projects
    /// with, so a pick and a drag agree about what a pixel looks at.
    fn scene_ray(&self, view: usize, x: f32, y: f32) -> Option<([f32; 3], [f32; 3])> {
        let (width, height) = self.scene_pixel_size(view)?;
        let (tan, aspect) = projection_terms(width, height);
        let forward = self.camera.forward();
        let right = self.camera.right();
        let up = gizmo::cross(right, forward);
        let ndc_x = x / width * 2.0 - 1.0;
        let ndc_y = 1.0 - y / height * 2.0;
        let direction = gizmo::add(
            forward,
            gizmo::add(
                gizmo::scale(right, ndc_x * tan * aspect),
                gizmo::scale(up, ndc_y * tan),
            ),
        );
        gizmo::normalize(direction).map(|direction| (self.camera.eye(), direction))
    }

    /// Where a world point lands on the first scene view's surface, in
    /// physical pixels.
    fn project_to_screen(&self, point: [f32; 3]) -> Option<(f32, f32)> {
        self.project_to_screen_in(0, point)
    }

    /// Where a world point lands on one scene view's surface, in physical
    /// pixels.
    fn project_to_screen_in(&self, view: usize, point: [f32; 3]) -> Option<(f32, f32)> {
        let (width, height) = self.scene_pixel_size(view)?;
        let (tan, aspect) = projection_terms(width, height);
        let eye = self.camera.eye();
        let forward = self.camera.forward();
        let right = self.camera.right();
        let up = gizmo::cross(right, forward);
        let relative = gizmo::sub(point, eye);
        let depth = gizmo::dot(relative, forward);
        if depth <= 1e-4 {
            return None;
        }
        let ndc_x = gizmo::dot(relative, right) / (depth * tan * aspect);
        let ndc_y = gizmo::dot(relative, up) / (depth * tan);
        Some(((ndc_x + 1.0) * 0.5 * width, (1.0 - ndc_y) * 0.5 * height))
    }

    /// Begin a drag from a handle pick: the press pixel's ray records the
    /// parameter or angle every later move is measured against.
    fn begin_gizmo_drag(&mut self, view: usize, handle: u32, press_x: f32, press_y: f32) {
        let Some((mode, axis)) = gizmo::handle_axis(handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle,
                "[editor] gizmo drag refused: unknown handle"
            );
            return;
        };
        let (settings, entity, original) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let settings = world
                .get_resource::<GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default();
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.entity);
            let Some(entity) = selected else {
                return;
            };
            let Some(original) = read_transform(world, entity) else {
                return;
            };
            (settings, entity, original)
        };
        let Some((ray_origin, ray_direction)) = self.scene_ray(view, press_x, press_y) else {
            return;
        };
        let basis = gizmo_basis(settings.space, &original);
        let origin = original.translation;
        let start_value = match mode {
            GizmoMode::Rotate => {
                let Some(point) =
                    gizmo::ray_plane_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the ring"
                    );
                    return;
                };
                gizmo::plane_angle(origin, basis[axis], point)
            }
            _ => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the axis"
                    );
                    return;
                };
                value
            }
        };
        let reference = self.gizmo_length(origin);
        self.gizmo_drag = Some(GizmoDrag {
            view,
            mode,
            axis,
            entity,
            original,
            start_value,
            reference,
            last_value: None,
        });
        info!(
            target: telemetry_target::ENGINE,
            mode = gizmo_mode_name(mode),
            axis = gizmo_axis_name(axis),
            entity = %entity,
            handle,
            "[editor] gizmo drag started"
        );
    }

    /// Apply one move of the drag in flight: re-read the ray, take the delta
    /// against the press, snap it when the tool snaps, and write the values
    /// straight into the world. No undo entry yet - the egui editor has no
    /// command queue wired (W-0027).
    fn update_gizmo_drag(&mut self, cursor_x: f32, cursor_y: f32) {
        let Some(drag) = self.gizmo_drag.as_ref() else {
            return;
        };
        let (view, mode, axis, entity) = (drag.view, drag.mode, drag.axis, drag.entity);
        let reference = drag.reference;
        let start_value = drag.start_value;
        let original = drag.original;
        let Some((ray_origin, ray_direction)) = self.scene_ray(view, cursor_x, cursor_y) else {
            return;
        };
        // The settings and the basis come first: both borrow `self`, and the
        // world borrow below must not overlap them.
        let settings = self
            .host
            .host_mut()
            .engine_mut()
            .world()
            .get_resource::<GizmoState>()
            .map(|state| state.settings)
            .unwrap_or_default();
        let basis = gizmo_basis(settings.space, &original);
        let origin = original.translation;
        let delta = match mode {
            GizmoMode::Rotate => {
                let Some(point) =
                    gizmo::ray_plane_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                gizmo::wrap_angle(gizmo::plane_angle(origin, basis[axis], point) - start_value)
            }
            _ => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                value - start_value
            }
        };
        let Some(value) =
            self.write_gizmo_drag(mode, axis, entity, &original, delta, &settings, reference)
        else {
            return;
        };
        if let Some(drag) = self.gizmo_drag.as_mut() {
            drag.last_value = Some(value);
        }
    }

    /// Write one drag move's values straight into the world; returns the
    /// value for the end log, `None` when the write failed.
    fn write_gizmo_drag(
        &mut self,
        mode: GizmoMode,
        axis: usize,
        entity: Entity,
        original: &TransformComponent,
        delta: f32,
        settings: &GizmoSettings,
        reference: f32,
    ) -> Option<[f32; 4]> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let write = |world: &mut pill_engine_core::World, field: &str, index: usize, value: f32| {
            world
                .write_component_array_element(
                    entity,
                    TRANSFORM_SHARED_NAME,
                    field,
                    index,
                    FieldValue::F32(value),
                )
                .ok()
        };
        match mode {
            GizmoMode::Translate => {
                let delta = if settings.snap {
                    gizmo::snap_value(delta, settings.translate_step)
                } else {
                    delta
                };
                let value = original.translation[axis] + delta;
                write(world, "translation", axis, value).map(|_| [value, 0.0, 0.0, 0.0])
            }
            GizmoMode::Rotate => {
                let degrees = delta.to_degrees();
                let degrees = if settings.snap {
                    gizmo::snap_value(degrees, settings.rotate_step_degrees)
                } else {
                    degrees
                };
                let turn = gizmo::axis_angle_quaternion(gizmo::AXES[axis], degrees.to_radians());
                let combined = quaternion_product(turn, original.rotation);
                let mut written = true;
                for index in 0..4 {
                    written &= write(world, "rotation", index, combined[index]).is_some();
                }
                written.then_some(combined)
            }
            GizmoMode::Scale => {
                let factor = 1.0 + delta / reference.max(1e-4);
                let mut value = original.scale[axis] * factor;
                if settings.snap {
                    value = gizmo::snap_value(value, settings.scale_step);
                }
                // A zero or negative scale is a degenerate mesh; the drag
                // stops at the smallest positive value instead.
                let value = value.max(0.01);
                write(world, "scale", axis, value).map(|_| [value, 0.0, 0.0, 0.0])
            }
        }
    }

    /// End the drag in flight, reporting what it wrote.
    fn end_gizmo_drag(&mut self) {
        let Some(drag) = self.gizmo_drag.take() else {
            return;
        };
        let settings = self
            .host
            .host_mut()
            .engine_mut()
            .world()
            .get_resource::<GizmoState>()
            .map(|state| state.settings)
            .unwrap_or_default();
        match drag.last_value {
            Some(value) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(drag.mode),
                axis = gizmo_axis_name(drag.axis),
                entity = %drag.entity,
                value = ?value,
                snapped = settings.snap,
                "[editor] gizmo drag ended"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(drag.mode),
                axis = gizmo_axis_name(drag.axis),
                entity = %drag.entity,
                "[editor] gizmo drag ended without a move"
            ),
        }
    }

    /// Press on one axis handle of the active tool (the scripted channel):
    /// the press pixel is the handle's grab point projected, so the pick
    /// that answers it proves the same geometry the renderer draws.
    fn start_synthetic_gizmo_drag(&mut self, axis: usize, delta_x: f32, delta_y: f32) {
        if self.gizmo_drag.is_some() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: one is already in flight"
            );
            return;
        }
        let Some(view) = self.visible_scene_view() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: no scene view is visible"
            );
            return;
        };
        let (settings, transform) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let settings = world
                .get_resource::<GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default();
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.entity);
            let Some(entity) = selected else {
                warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] gizmo drag refused: nothing is selected"
                );
                return;
            };
            let Some(transform) = read_transform(world, entity) else {
                warn!(
                    target: telemetry_target::ENGINE,
                    entity = %entity,
                    "[editor] gizmo drag refused: the entity has no transform"
                );
                return;
            };
            (settings, transform)
        };
        let basis = gizmo_basis(settings.space, &transform);
        let length = self.gizmo_length(transform.translation);
        // A rotation ring is a circle of `length` radius, so its grab point
        // sits on that radius - at the diagonal of the two axes the ring
        // turns around, where no other ring passes a pixel away. The
        // straight handles are grabbed just short of their tip instead,
        // along their own axis.
        let grab_direction = match settings.mode {
            GizmoMode::Rotate => {
                gizmo::normalize(gizmo::add(basis[(axis + 1) % 3], basis[(axis + 2) % 3]))
                    .unwrap_or(basis[axis])
            }
            _ => basis[axis],
        };
        let grab_distance = match settings.mode {
            GizmoMode::Rotate => length,
            _ => length * GIZMO_PRESS_FRACTION,
        };
        let tip = gizmo::add(
            transform.translation,
            gizmo::scale(grab_direction, grab_distance),
        );
        let Some((x, y)) = self.project_to_screen_in(view, tip) else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: the handle is behind the camera"
            );
            return;
        };
        self.synthetic_gizmo_pending = Some((delta_x, delta_y));
        self.synthetic_gizmo_view = Some(view);
        info!(
            target: telemetry_target::ENGINE,
            axis = gizmo_axis_name(axis),
            x,
            y,
            "[editor] gizmo drag press"
        );
        // The press pixel projects onto the handle's own geometry, but a
        // one-pixel line straddles pixel rows: the press asks the exact
        // pixel and its four neighbours, and the first that names a handle
        // wins.
        let (centre_x, centre_y) = (x.round() as i64, y.round() as i64);
        self.synthetic_press_ids.clear();
        for (pixel_x, pixel_y) in [
            (centre_x, centre_y),
            (centre_x - 1, centre_y),
            (centre_x + 1, centre_y),
            (centre_x, centre_y - 1),
            (centre_x, centre_y + 1),
        ] {
            if let Some(id) =
                self.queue_scene_pick_at(view, pixel_x.max(0) as u32, pixel_y.max(0) as u32)
            {
                self.synthetic_press_ids.push(id);
            }
        }
    }

    /// Log one component field's value: the suite's window into the world
    /// (the scripted `dump field <entity> <component> <field>`); the
    /// component name may be abbreviated to its `::Name` suffix.
    fn dump_field(&mut self, entity_text: &str, component: &str, field: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let resolved = resolve_display_entity(world, entity_text)
            .and_then(|entity| resolve_component_name(world, component).map(|name| (entity, name)));
        let Some((entity, component)) = resolved else {
            warn!(
                target: telemetry_target::ENGINE,
                entity = entity_text,
                component,
                field,
                "[test] field unreadable"
            );
            return;
        };
        match world.read_component_field(entity, &component, field) {
            Ok(value) => info!(
                target: telemetry_target::ENGINE,
                value = ?value,
                "[test] field"
            ),
            Err(_) => warn!(
                target: telemetry_target::ENGINE,
                entity = entity_text,
                component = component.as_str(),
                field,
                "[test] field unreadable"
            ),
        }
    }

    /// Log every scene view's placed rectangle, in the main window's
    /// physical pixels, for the suite's scripted clicks, and the game
    /// window's own rectangle when it exists.
    fn dump_scenes(&mut self) {
        for view in 0..self.views.len() {
            let Some(scene) = &self.views[view] else {
                continue;
            };
            let (origin_x, origin_y) = self.view_origin(view);
            let (width, height) = scene.window.size();
            info!(
                target: telemetry_target::ENGINE,
                view,
                x = origin_x as i32,
                y = origin_y as i32,
                width,
                height,
                visible = scene.window.visible,
                "[editor] scene view rect"
            );
        }
        if let Some(game) = &self.game_surface {
            let (width, height) = game.size();
            info!(
                target: telemetry_target::ENGINE,
                x = game.origin.0,
                y = game.origin.1,
                width,
                height,
                visible = game.visible,
                "[editor] game view rect"
            );
        }
    }

    /// Log the scene grid's state (Stage 23): the switch, the spacing and
    /// the surfaces the renderer draws it on - the same resolution the
    /// renderer makes from the same bindings.
    fn dump_grid(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let cameras = world.get_resource::<SurfaceCameras>();
        let grid = cameras.map(|cameras| cameras.grid).unwrap_or_default();
        let surfaces = cameras
            .and_then(|cameras| frame_grid(cameras, cameras.grid))
            .map(|grid| {
                grid.surfaces
                    .iter()
                    .map(|surface| surface.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            enabled = grid.enabled,
            cell = grid.cell_size,
            surfaces = %surfaces,
            "[test] scene grid"
        );
    }

    /// Log the asset browser's rows: the count first, then one line per
    /// row with its kind and flags - the suite's window into the listing.
    fn dump_assets(&mut self) {
        info!(
            target: telemetry_target::ENGINE,
            count = self.asset_entries.len(),
            "[test] assets"
        );
        for row in &self.asset_entries {
            info!(
                target: telemetry_target::ENGINE,
                path = row.path.as_str(),
                kind = row.kind.as_str(),
                standalone = row.standalone,
                has_meta = row.has_meta,
                "[test] asset"
            );
        }
    }

    /// Log the live standalone types the create dialog offers; the suite
    /// reads the first one to drive a create.
    fn dump_standalone_types(&mut self) {
        for info_type in &self.standalone_types {
            info!(
                target: telemetry_target::ENGINE,
                type_name = info_type.type_name.as_str(),
                display = info_type.display_name.as_str(),
                extension = info_type.extension.as_str(),
                "[test] standalone type"
            );
        }
    }

    /// Log the shared selection, entity and asset, with `-` for the empty
    /// halves - the native editor's shape.
    fn dump_selection(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let (entity, asset) = world
            .get_resource::<EditorSelection>()
            .map(|selection| (selection.entity, selection.asset.clone()))
            .unwrap_or((None, None));
        let entity_text = entity
            .map(|entity| entity.to_string())
            .unwrap_or_else(|| "-".to_string());
        info!(
            target: telemetry_target::ENGINE,
            entity = entity_text.as_str(),
            asset = asset.as_deref().unwrap_or("-"),
            "[test] selection"
        );
    }

    /// Log every registered system with the state the Systems tab draws,
    /// then the engine's pause flag - the tab's exact data, so a suite can
    /// assert a toggle or a pause without reading the tab's pixels.
    fn dump_systems(&mut self) {
        let (systems, paused) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource::<EditorSnapshot>() {
                Some(snapshot) => (snapshot.systems.clone(), snapshot.paused),
                None => (Vec::new(), false),
            }
        };
        info!(
            target: telemetry_target::ENGINE,
            count = systems.len(),
            paused,
            "[test] systems"
        );
        for system in &systems {
            info!(
                target: telemetry_target::ENGINE,
                index = system.index,
                name = system.name.as_str(),
                owner = system.owner_label.as_str(),
                enabled = system.enabled,
                hot_patchable = system.hot_patchable,
                ambiguous = system.name_is_ambiguous,
                "[test] system"
            );
        }
    }

    /// Log the preferences file's rows as the Preferences tab's view holds
    /// them, then the project key and its remembered entry - the tab's
    /// exact data, so a suite can assert a save or a tool choice without
    /// reading the tab's pixels.
    fn dump_preferences(&mut self) {
        let view = PreferencesView::from_document(
            &self.preferences_document,
            self.gizmo_project.as_deref(),
        );
        info!(
            target: telemetry_target::ENGINE,
            keymap_preset = view.text("keymap_preset"),
            code_editor_command = view.text("code_editor_command"),
            autosave_interval_seconds = view.text("autosave_interval_seconds"),
            snapping_translation = view.text("snapping_steps.translation"),
            snapping_rotation_degrees = view.text("snapping_steps.rotation_degrees"),
            snapping_scale = view.text("snapping_steps.scale"),
            history_step_cap = view.text("history_step_cap"),
            history_memory_budget_mb = view.text("history_memory_budget_mb"),
            "[test] preferences"
        );
        info!(
            target: telemetry_target::ENGINE,
            project = view.project.as_deref().unwrap_or("-"),
            remembered = view.project_remembered,
            mode = ?view.project_gizmo.mode,
            space = ?view.project_gizmo.space,
            snap = ?view.project_gizmo.snap,
            "[test] preferences project"
        );
    }

    /// The scripted asset selection: the same shared resource an Assets row
    /// click writes, logged so a suite can watch either path.
    fn scripted_select_asset(&mut self, path: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
            selection.asset = Some(path.to_string());
        }
        info!(
            target: telemetry_target::ENGINE,
            path,
            "[editor] asset selected"
        );
    }

    /// The scripted entity selection: the same recorded step the Entities
    /// tab's row click queues, resolved against the live entity table.
    ///
    /// A screen-space click cannot re-select an entity a gizmo drag moved;
    /// the handle a pick logged still resolves here.
    fn scripted_select_entity(&mut self, handle: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle = %handle,
                "[editor] select entity target not resolvable"
            );
            return;
        };
        let asset = world
            .get_resource::<EditorSelection>()
            .and_then(|selection| selection.asset.clone());
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(
                vec![EditorCommand::SetSelection {
                    entity: Some(entity),
                    asset,
                }],
                "select entity",
                Some("editor.selection".to_string()),
            );
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %entity,
            "[editor] selection changed"
        );
    }

    /// One Assets toolbar button, driven by the scripted channel: the same
    /// core helpers the buttons call, enablement rules included.
    fn scripted_toolbar(&mut self, action: &str) {
        // The rows, the types and the selection the helpers read, cloned
        // out first: the world's mutable resources are borrowed below.
        let (rows, types, selected) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let (rows, types) = match world.get_resource::<EditorSnapshot>() {
                Some(snapshot) => (snapshot.assets.clone(), snapshot.standalone_types.clone()),
                None => (Vec::new(), Vec::new()),
            };
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.asset.clone());
            (rows, types, selected)
        };
        let mut selection = EditorSelection::default();
        selection.asset = selected;
        match action {
            "create" => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                    assets::open_create(&mut state.asset_dialog, &selection, &rows, &types);
                }
            }
            "rename" => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                    assets::open_rename(&mut state.asset_dialog, &selection, &rows);
                }
            }
            "delete" => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    assets::delete_selection(pending, &selection, &rows);
                }
            }
            "meta" => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    assets::write_meta_selection(pending, &selection, &rows);
                }
            }
            _ => {}
        }
    }

    /// Set the open dialog's name field - what typing into the window's
    /// text field does.
    fn scripted_dialog_set_name(&mut self, name: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            match state.asset_dialog.as_mut() {
                Some(AssetDialog::Create { name: field, .. })
                | Some(AssetDialog::Rename { name: field, .. }) => {
                    *field = name.to_string();
                }
                None => warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] dialog set without an open dialog"
                ),
            }
        }
    }

    /// Set the create dialog's chosen type - what clicking a type row does.
    fn scripted_dialog_set_type(&mut self, type_name: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            match state.asset_dialog.as_mut() {
                Some(AssetDialog::Create {
                    type_name: field, ..
                }) => {
                    *field = type_name.to_string();
                }
                _ => warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] dialog set type needs the create dialog"
                ),
            }
        }
    }

    /// Run the open dialog's submit button: the dialog is taken out of the
    /// session, submitted, and put back, because the submit needs the
    /// pending queue - a second mutable resource the world cannot lend
    /// alongside the session.
    fn scripted_dialog_submit(&mut self) {
        let mut dialog = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<EguiShellState>()
                .and_then(|state| state.asset_dialog.take())
        };
        if dialog.is_none() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] dialog submit without an open dialog"
            );
            return;
        }
        {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                match dialog.as_ref() {
                    Some(AssetDialog::Create { .. }) => {
                        assets::submit_create(&mut dialog, pending);
                    }
                    Some(AssetDialog::Rename { .. }) => {
                        assets::submit_rename(&mut dialog, pending);
                    }
                    None => {}
                }
            }
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.asset_dialog = dialog;
        }
    }

    /// Close the open dialog without acting - the window's Cancel button.
    fn scripted_dialog_cancel(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            assets::cancel(&mut state.asset_dialog);
        }
    }

    /// Drive one direct asset edit through the recorded command path:
    /// `edit asset create <type> <folder|-> <name>`, `edit asset delete
    /// <path>`, `edit asset move <from> <to>` and `edit asset meta <path>` -
    /// the same route the native editor's scripted edits take.
    fn scripted_asset_edit<'a>(
        &mut self,
        parts: &mut std::str::SplitWhitespace<'a>,
        command: &str,
    ) {
        let queued = match parts.next() {
            Some("create") => match (parts.next(), parts.next(), parts.next()) {
                (Some(type_name), Some(folder), Some(name)) => Some((
                    EditorCommand::CreateStandaloneAsset {
                        type_name: type_name.to_string(),
                        folder: if folder == "-" {
                            String::new()
                        } else {
                            folder.to_string()
                        },
                        name: name.to_string(),
                    },
                    format!("create asset {name}"),
                )),
                _ => None,
            },
            Some("delete") => parts.next().map(|path| {
                (
                    EditorCommand::DeleteAssetFile {
                        path: path.to_string(),
                    },
                    format!("delete asset {path}"),
                )
            }),
            Some("move") => match (parts.next(), parts.next()) {
                (Some(from), Some(to)) => Some((
                    EditorCommand::MoveAssetFile {
                        from: from.to_string(),
                        to: to.to_string(),
                    },
                    format!("move asset {from}"),
                )),
                _ => None,
            },
            Some("meta") => parts.next().map(|path| {
                (
                    EditorCommand::WriteAssetMeta {
                        path: path.to_string(),
                    },
                    format!("write meta {path}"),
                )
            }),
            _ => None,
        };
        let Some((command_value, label)) = queued else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit asset needs create, delete, move or meta with its arguments"
            );
            return;
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![command_value], label, None);
        }
    }

    /// Resolve a scripted edit's target: the entity, the registered
    /// component name and the field's current value.
    fn resolve_edit_target(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
    ) -> Option<(Entity, String, FieldValue)> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, entity_text) else {
            warn!(
                target: telemetry_target::ENGINE,
                entity = entity_text,
                "[editor] inspector set target not resolvable"
            );
            return None;
        };
        let Some(component) = resolve_component_name(world, component_text) else {
            warn!(
                target: telemetry_target::ENGINE,
                component = component_text,
                "[editor] inspector set target not resolvable"
            );
            return None;
        };
        match world.read_component_field(entity, &component, field) {
            Ok(current) => Some((entity, component, current)),
            Err(_) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    field,
                    "[editor] inspector set target not resolvable"
                );
                None
            }
        }
    }

    /// One scripted whole-field edit (`edit set <entity> <component>
    /// <field> <value>`): the same recorded `SetField` batch an inspector
    /// widget queues, parsed against the field's current kind.
    fn scripted_edit_set<'a>(&mut self, parts: &mut std::str::SplitWhitespace<'a>, command: &str) {
        let (Some(entity_text), Some(component_text), Some(field)) =
            (parts.next(), parts.next(), parts.next())
        else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit set needs an entity, a component, a field and a value"
            );
            return;
        };
        let value_text = parts.collect::<Vec<_>>().join(" ");
        if value_text.is_empty() {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit set needs a value"
            );
            return;
        }
        let Some((entity, component, current)) =
            self.resolve_edit_target(entity_text, component_text, field)
        else {
            return;
        };
        let value = match inspector::parse_field_value(&value_text, &current) {
            Ok(value) => value,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    field,
                    message = message.as_str(),
                    "[editor] inspector value not understood"
                );
                return;
            }
        };
        let (edit, label) = inspector::field_edit(entity, &component, field, value);
        let coalesce = inspector::field_coalesce_key(entity, &component, field);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, Some(coalesce));
        }
    }

    /// One scripted array-element edit (`edit array <entity> <component>
    /// <field> <index> <value>`): the same recorded `SetArrayElement` batch
    /// an inspector element widget queues.
    fn scripted_edit_array<'a>(
        &mut self,
        parts: &mut std::str::SplitWhitespace<'a>,
        command: &str,
    ) {
        let (Some(entity_text), Some(component_text), Some(field), Some(index_text)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit array needs an entity, a component, a field, an index and a value"
            );
            return;
        };
        let Some(index) = index_text.parse::<usize>().ok() else {
            warn!(
                target: telemetry_target::ENGINE,
                index = index_text,
                "[editor] edit array needs a numeric index"
            );
            return;
        };
        let value_text = parts.collect::<Vec<_>>().join(" ");
        if value_text.is_empty() {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit array needs a value"
            );
            return;
        }
        let Some((entity, component, current)) =
            self.resolve_edit_target(entity_text, component_text, field)
        else {
            return;
        };
        let Some(element) = inspector::element_field(&current, index) else {
            warn!(
                target: telemetry_target::ENGINE,
                field,
                index,
                "[editor] edit array index is outside the array"
            );
            return;
        };
        let value = match inspector::parse_field_value(&value_text, &element) {
            Ok(value) => value,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    field,
                    message = message.as_str(),
                    "[editor] inspector value not understood"
                );
                return;
            }
        };
        let (edit, label) = inspector::element_edit(entity, &component, field, index, value);
        let coalesce =
            inspector::field_coalesce_key(entity, &component, &format!("{field}[{index}]"));
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, Some(coalesce));
        }
    }

    /// One scripted asset-settings edit (`edit asset-settings <path> <key>
    /// <value>`): the same recorded `SetAssetSettings` batch the Inspector's
    /// settings rows queue, parsed against the leaf's current JSON kind.
    fn scripted_asset_settings<'a>(
        &mut self,
        parts: &mut std::str::SplitWhitespace<'a>,
        command: &str,
    ) {
        let (Some(path), Some(key)) = (parts.next(), parts.next()) else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit asset-settings needs a path, a key and a value"
            );
            return;
        };
        let value_text = parts.collect::<Vec<_>>().join(" ");
        if value_text.is_empty() {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit asset-settings needs a value"
            );
            return;
        }
        let Ok(key_path) = inspector::parse_settings_key_path(key) else {
            warn!(
                target: telemetry_target::ENGINE,
                key,
                "[editor] asset settings edit refused"
            );
            return;
        };
        let Some(document) = self.read_asset_settings(path) else {
            warn!(
                target: telemetry_target::ENGINE,
                path,
                "[editor] asset settings edit refused"
            );
            return;
        };
        let Some(current) = inspector::settings_field(&document, &key_path) else {
            warn!(
                target: telemetry_target::ENGINE,
                path,
                key,
                "[editor] asset settings edit refused"
            );
            return;
        };
        let value = match inspector::parse_settings_value(&value_text, current) {
            Ok(value) => value,
            Err(message) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    key,
                    message = message.as_str(),
                    "[editor] inspector value not understood"
                );
                return;
            }
        };
        let mut patched = document;
        if let Err(message) = inspector::patch_settings(&mut patched, &key_path, value) {
            warn!(
                target: telemetry_target::ENGINE,
                key,
                message = message.as_str(),
                "[editor] asset settings edit refused"
            );
            return;
        }
        let (edit, label) = inspector::settings_edit(path, patched, key);
        let coalesce = inspector::settings_coalesce_key(path, key);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, Some(coalesce));
        }
    }

    /// Run one world frame: input, the shell's pass, the overlay draw.
    ///
    /// Returns whether the run should end.
    fn frame(&mut self) -> Result<bool, EditorError> {
        self.frame_index += 1;
        // Step 1: the frame rate heartbeat, for the person reading the log.
        let now = Instant::now();
        if let Some(last) = self.last_frame_time {
            let delta_time = now.duration_since(last).as_secs_f32();
            if delta_time > 0.0 {
                let instant = 1.0 / delta_time;
                self.fps = if self.fps > 0.0 {
                    self.fps * (1.0 - FPS_SMOOTHING) + instant * FPS_SMOOTHING
                } else {
                    instant
                };
            }
        }
        self.last_frame_time = Some(now);
        if self.frame_index.is_multiple_of(210) {
            let overlay_frames = self
                .overlay
                .as_ref()
                .map(OverlaySurface::frames)
                .unwrap_or(0);
            info!(
                target: telemetry_target::ENGINE,
                fps = self.fps,
                frame = self.frame_index,
                overlay_frames,
                "[editor] frame heartbeat"
            );
        }
        // Step 2: the window events of the last request become engine input
        // events; the shell's egui pass reads the same input, so the UI and
        // the game see one event stream.
        for event in self.pending_events.drain(..) {
            self.host.host_mut().engine_mut().push_input_event(event);
        }
        // Step 2b: a scripted drag in flight takes its next stage - the
        // press, the move and the release, one per frame.
        self.advance_scripted_drag();
        // Step 3: the shell boundary's input resource.
        if let Some(input) = self
            .host
            .host_mut()
            .engine_mut()
            .world_mut()
            .get_resource_mut::<EguiShellInput>()
        {
            *input = EguiShellInput {
                screen_size: pill_core::math::Vector2f::new(
                    self.size.0 as f32 / self.scale_factor,
                    self.size.1 as f32 / self.scale_factor,
                ),
                scale_factor: self.scale_factor,
                focused: self.focused,
                time: self.started.elapsed().as_secs_f64(),
                frames_per_second: self.fps,
            };
        }
        // Step 3b: apply the engine mutations the shell's assets actions and
        // the scripted channel queued - the frame boundary the
        // `EditorCommand` contract names. A failed command never aborts the
        // rest of the batch.
        self.apply_pending_commands();
        // Step 3c: the gizmo's persistence and the preferences file's live
        // reader, both before the snapshot so the tabs read the state this
        // frame's edits left.
        self.persist_gizmo_settings();
        self.watch_preferences();
        // Step 4: the frame's snapshot for the shell's data tabs, when the
        // shell loaded and registered its resources.
        self.publish_snapshot();
        // Step 5: the world resources the frame draws and picks with,
        // published before it runs: the shared selection's outline set (one
        // write per change, Stage 13) and the gizmo handles for it (Stage
        // 15).
        self.publish_selection_outline();
        self.publish_gizmo_geometry();
        // Step 6: run the world frame; the shell's egui pass runs inside it
        // (a registered post-update system) and leaves the frame resource
        // for Step 8.
        let report = self.host.run_one_frame().map_err(EditorError::Renderer)?;
        let _ = report;
        // Step 7: answers to scene picks that arrived during the frame
        // move the shared selection; the outline follows on the next
        // publish (Stage 13).
        self.drain_scene_picks();
        // Step 8: draw the shell frame through the overlay.
        self.present_ui();
        // Step 9: the scripted channel, then the auto-quit deadline.
        self.poll_test_channel();
        if let Some(seconds) = self.test_seconds {
            if self.started.elapsed().as_secs_f64() >= seconds {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] test deadline reached; quitting"
                );
                self.quit = true;
            }
        }
        Ok(self.quit)
    }

    /// Capture the editor snapshot the shell's tabs read, when the shell
    /// loaded and registered it.
    ///
    /// The asset listing refreshes on its cadence - or at once after a file
    /// command - because the walk touches the filesystem; the detail tier,
    /// the Inspector's data, is rebuilt for the current selection every
    /// frame: it is one entity's components, cheap, and it keeps the tab's
    /// values live, the same policy the native editor follows.
    fn publish_snapshot(&mut self) {
        let listing_refreshed = self.refresh_asset_listing();
        self.refresh_asset_detail(listing_refreshed);
        let engine = self.host.host_mut().engine_mut();
        if engine.world().get_resource::<EditorSnapshot>().is_none() {
            return;
        }
        let selected = engine
            .world()
            .get_resource::<EditorSelection>()
            .and_then(|selection| selection.entity);
        let mut snapshot = EditorSnapshot::capture_list(engine, self.frame_index, &[], Vec::new());
        snapshot.detail =
            selected.and_then(|entity| EditorSnapshot::capture_detail(engine, entity));
        snapshot.assets = self.asset_entries.clone();
        snapshot.standalone_types = self.standalone_types.clone();
        snapshot.asset_detail = self.asset_detail.clone();
        snapshot.preferences = PreferencesView::from_document(
            &self.preferences_document,
            self.gizmo_project.as_deref(),
        );
        if let Some(resource) = engine.world_mut().get_resource_mut::<EditorSnapshot>() {
            *resource = snapshot;
        }
    }

    /// Refresh the asset browser's rows when the cadence is due or a file
    /// command marked them dirty, and pick up the live standalone types the
    /// create dialog offers - the native editor's listing walk. Returns
    /// whether the walk ran, so the asset detail follows it.
    fn refresh_asset_listing(&mut self) -> bool {
        if !(self.frame_index.is_multiple_of(ASSET_REFRESH_FRAMES) || self.asset_listing_dirty) {
            return false;
        }
        self.asset_listing_dirty = false;
        self.asset_entries = self
            .host
            .host_mut()
            .asset_entries()
            .into_iter()
            .take(ASSET_ROWS_MAX)
            .map(|entry| AssetListing {
                path: entry.path,
                kind: if entry.is_directory {
                    "folder".to_string()
                } else {
                    entry.type_name.unwrap_or_else(|| "file".to_string())
                },
                standalone: entry.standalone,
                has_meta: entry.guid.is_some(),
            })
            .collect();
        let types = pill_host::standalone_types(self.host.host_mut().engine_mut().world());
        self.standalone_types = types
            .into_iter()
            .map(|info| StandaloneTypeInfo {
                type_name: info.type_name,
                display_name: info.display_name,
                extension: info.extension,
            })
            .collect();
        info!(
            target: telemetry_target::ENGINE,
            count = self.asset_entries.len(),
            types = self.standalone_types.len(),
            "[editor] asset listing refreshed"
        );
        true
    }

    /// Capture the selected asset's detail for the Inspector's asset
    /// section, when the selection or the tree changed: reading the settings
    /// document is a disk read, so the capture follows the listing's cadence
    /// instead of running every frame. A settings edit marks the listing
    /// dirty, so the widgets see the document the command wrote.
    fn refresh_asset_detail(&mut self, listing_refreshed: bool) {
        let selected = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.asset.clone())
        };
        if !listing_refreshed && selected == self.asset_detail_path {
            return;
        }
        self.asset_detail_path = selected.clone();
        self.asset_detail = selected.map(|path| {
            let row = self.asset_entries.iter().find(|row| row.path == path);
            let kind = row
                .map(|row| row.kind.clone())
                .unwrap_or_else(|| "file".to_string());
            let standalone = row.map(|row| row.standalone).unwrap_or(false);
            let settings = if row.is_some_and(|row| row.kind == "folder") {
                None
            } else {
                self.read_asset_settings(&path)
            };
            AssetDetail {
                path,
                kind,
                standalone,
                settings,
            }
        });
    }

    /// The settings document an asset's editable rows build on, read through
    /// the import registry - the same reader the `SetAssetSettings` apply
    /// uses.
    fn read_asset_settings(&mut self, path: &str) -> Option<serde_json::Value> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let registry = world
            .get_resource::<pill_engine_core::ImportRegistry>()?
            .clone();
        let relative = pill_engine_core::asset_metadata::checked_relative(path).ok()?;
        registry.settings_json(&relative).ok()
    }

    /// Apply the command batches the shell's assets actions and the
    /// scripted channel queued: the frame boundary the `EditorCommand`
    /// contract names, the same drain the native editor's frame runs. A
    /// failed command never aborts the batch; recorded batches land in the
    /// history; the selection follows a move and clears with a delete; and
    /// a batch that touched files marks the listing for an immediate walk.
    fn apply_pending_commands(&mut self) {
        let batches = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<PendingEditorCommands>()
                .map(|pending| std::mem::take(&mut pending.batches))
                .unwrap_or_default()
        };
        if batches.is_empty() {
            return;
        }
        let now_millis = self.started.elapsed().as_millis() as u64;
        for batch in batches {
            let engine = self.host.host_mut().engine_mut();
            let recorded = EditorCommand::apply_recorded(engine, &batch.commands);
            info!(
                target: telemetry_target::ENGINE,
                count = batch.commands.len(),
                "[editor] engine commands applied"
            );
            // The shared selection follows its file: a move re-points it, a
            // delete clears it - the resource agreeing with the disk.
            for command in &batch.commands {
                let world = self.host.host_mut().engine_mut().world_mut();
                let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                    break;
                };
                match command {
                    EditorCommand::MoveAssetFile { from, to }
                        if selection.asset.as_deref() == Some(from.as_str()) =>
                    {
                        selection.asset = Some(to.clone());
                        info!(
                            target: telemetry_target::ENGINE,
                            path = to.as_str(),
                            "[editor] selection followed the move"
                        );
                    }
                    EditorCommand::DeleteAssetFile { path }
                        if selection.asset.as_deref() == Some(path.as_str()) =>
                    {
                        selection.asset = None;
                        info!(
                            target: telemetry_target::ENGINE,
                            path = path.as_str(),
                            "[editor] selection cleared with the delete"
                        );
                    }
                    _ => {}
                }
            }
            // A batch that moved files on disk brings the rows up to date
            // at once: the toolbar acts on what the rows say.
            if batch.commands.iter().any(|command| {
                matches!(
                    command,
                    EditorCommand::CreateStandaloneAsset { .. }
                        | EditorCommand::RestoreAssetFile { .. }
                        | EditorCommand::DeleteAssetFile { .. }
                        | EditorCommand::MoveAssetFile { .. }
                        | EditorCommand::WriteAssetMeta { .. }
                        | EditorCommand::ImportFiles { .. }
                        | EditorCommand::RemoveFiles { .. }
                )
            }) {
                self.asset_listing_dirty = true;
            }
            for (index, message) in recorded.failures {
                warn!(
                    target: telemetry_target::ENGINE,
                    index,
                    message = %message,
                    "[editor] command failed"
                );
            }
            // A restore assigned fresh handles; the history follows them
            // before anything else is recorded, so a later undo cannot
            // target the dead handle.
            for (previous, assigned) in &recorded.remaps {
                let world = self.host.host_mut().engine_mut().world_mut();
                let Some(history) = world.get_resource_mut::<CommandHistory>() else {
                    break;
                };
                history.remap_entity(*previous, *assigned);
                info!(
                    target: telemetry_target::ENGINE,
                    previous = %previous,
                    assigned = %assigned,
                    "[editor] history remap"
                );
            }
            // An edit batch becomes one undo step, unless it failed or
            // could not be inverted in full - a step that could not be
            // undone would be worse than no step.
            if batch.record && recorded.fully_invertible && !batch.commands.is_empty() {
                let entry = HistoryEntry::engine(
                    batch.label.clone(),
                    batch.commands.clone(),
                    recorded.inverses.clone(),
                );
                let entry = match &batch.coalesce_key {
                    Some(key) => entry.coalescing(key.clone(), now_millis),
                    None => entry,
                };
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(history) = world.get_resource_mut::<CommandHistory>() {
                    let outcome =
                        history.push(entry, now_millis, CommandHistory::DEFAULT_COALESCE_WINDOW);
                    info!(
                        target: telemetry_target::ENGINE,
                        label = batch.label.as_str(),
                        steps = outcome.steps,
                        coalesced = outcome.coalesced,
                        "[editor] history push"
                    );
                }
            }
        }
    }

    /// Write a changed gizmo choice to the project's preferences entry.
    ///
    /// The executable owns this write because it is the side that knows the
    /// project; the file arriving back through the live reader is what keeps
    /// the snapshot's remembered entry honest. An unchanged tool writes
    /// nothing.
    fn persist_gizmo_settings(&mut self) {
        let settings = self
            .host
            .host_mut()
            .engine_mut()
            .world()
            .get_resource::<GizmoState>()
            .map(|state| state.settings);
        let Some(settings) = settings else {
            return;
        };
        if self.gizmo_persisted == Some(settings) {
            return;
        }
        self.gizmo_persisted = Some(settings);
        let Some(project) = self.gizmo_project.clone() else {
            return;
        };
        let directory = layout_directory();
        match write_gizmo_preferences(&directory, &project, &settings) {
            Ok(()) => info!(
                target: telemetry_target::ENGINE,
                project = project.as_str(),
                mode = ?settings.mode,
                snap = settings.snap,
                "[editor] gizmo preferences written"
            ),
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                error = error.as_str(),
                "[editor] gizmo preferences write failed"
            ),
        }
    }

    /// Read the preferences file; when its text changes, apply the live
    /// halves - the history limits and the gizmo's snapping steps - and
    /// refresh the document the snapshot's view and the dump read.
    ///
    /// The tool's mode, space and toggle stay whatever the world state
    /// holds, so the stale copy a persisting write leaves behind never moves
    /// the live tool; the snapping steps are the file's, one set for every
    /// project (D31).
    fn watch_preferences(&mut self) {
        let directory = layout_directory();
        let text = std::fs::read_to_string(directory.join(PREFERENCES_FILE)).ok();
        if text == self.preferences_text {
            return;
        }
        self.preferences_text = text;
        let preferences = self
            .preferences_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<EditorPreferences>(text).ok())
            .unwrap_or_default();
        self.preferences_document = self
            .preferences_text
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        let world = self.host.host_mut().engine_mut().world_mut();
        let step_cap = preferences
            .history_step_cap
            .unwrap_or(CommandHistory::DEFAULT_STEP_CAP);
        let memory_mb = preferences
            .history_memory_budget_mb
            .unwrap_or(CommandHistory::DEFAULT_MEMORY_BUDGET_BYTES / (1024 * 1024));
        if let Some(history) = world.get_resource_mut::<CommandHistory>() {
            let outcome = history.set_limits(step_cap, memory_mb * 1024 * 1024);
            info!(
                target: telemetry_target::ENGINE,
                step_cap,
                memory_mb,
                "[editor] preferences applied"
            );
            if outcome.trimmed > 0 {
                info!(
                    target: telemetry_target::ENGINE,
                    dropped = outcome.trimmed,
                    reason = ?outcome.trim_reason,
                    "[editor] history trimmed"
                );
            }
        }
        if let (Some(state), Some(steps)) = (
            world.get_resource_mut::<GizmoState>(),
            preferences.snapping_steps,
        ) {
            state.settings.translate_step = steps.translation;
            state.settings.rotate_step_degrees = steps.rotation_degrees;
            state.settings.scale_step = steps.scale;
        }
    }

    /// Run `editor.undo` from the scripted channel: pop the newest undo
    /// entry and queue its inverse batch - applied, never recorded - the
    /// same walk the native editor's command takes.
    fn scripted_undo(&mut self) {
        let step = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(history) = world.get_resource_mut::<CommandHistory>() else {
                return;
            };
            match history.pop_undo() {
                None => {
                    info!(
                        target: telemetry_target::ENGINE,
                        "[editor] history undo empty"
                    );
                    return;
                }
                Some(entry) => {
                    let label = entry.label.clone();
                    let remaining = history.undo_depth();
                    let commands = match entry.entry {
                        HistoryEntryKind::Engine { inverse, .. } => Some(inverse),
                        HistoryEntryKind::Selection { .. } => None,
                    };
                    (label, remaining, commands)
                }
            }
        };
        let (label, remaining, commands) = step;
        if let Some(commands) = commands {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push(commands);
            }
        }
        info!(
            target: telemetry_target::ENGINE,
            label = label.as_str(),
            remaining,
            "[editor] history undo"
        );
    }

    /// Run `editor.redo` from the scripted channel: pop the newest redo
    /// entry and queue its forward batch.
    fn scripted_redo(&mut self) {
        let step = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(history) = world.get_resource_mut::<CommandHistory>() else {
                return;
            };
            match history.pop_redo() {
                None => {
                    info!(
                        target: telemetry_target::ENGINE,
                        "[editor] history redo empty"
                    );
                    return;
                }
                Some(entry) => {
                    let label = entry.label.clone();
                    let remaining = history.redo_depth();
                    let commands = match entry.entry {
                        HistoryEntryKind::Engine { forward, .. } => Some(forward),
                        HistoryEntryKind::Selection { .. } => None,
                    };
                    (label, remaining, commands)
                }
            }
        };
        let (label, remaining, commands) = step;
        if let Some(commands) = commands {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                pending.push(commands);
            }
        }
        info!(
            target: telemetry_target::ENGINE,
            label = label.as_str(),
            remaining,
            "[editor] history redo"
        );
    }

    /// Run `editor.pause_toggle` from the scripted channel: queue the flip
    /// of the engine's pause flag - applied, never recorded, the same walk
    /// the Systems tab's Pause/Resume button takes.
    fn scripted_pause_toggle(&mut self) {
        let paused = self.host.host_mut().engine_mut().systems_paused();
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![systems::pause_toggle_command(paused)]);
        }
        info!(
            target: telemetry_target::ENGINE,
            paused = !paused,
            "[editor] pause toggled"
        );
    }

    /// Run `editor.step` from the scripted channel: queue one single step -
    /// applied, never recorded, the same walk the Step button takes.
    fn scripted_step(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::StepOnce]);
        }
        info!(target: telemetry_target::ENGINE, "[editor] step queued");
    }

    /// Run `edit system <index> on|off` from the scripted channel: the same
    /// recorded, undoable toggle the Systems tab's state buttons queue.
    fn scripted_edit_system<'a>(
        &mut self,
        parts: &mut std::str::SplitWhitespace<'a>,
        command: &str,
    ) {
        let (Some(index), Some(state)) = (parts.next(), parts.next()) else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit system needs an index and on or off"
            );
            return;
        };
        let Ok(index) = index.parse::<usize>() else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit system needs an index and on or off"
            );
            return;
        };
        let enabled = match state {
            "on" => true,
            "off" => false,
            _ => {
                warn!(
                    target: telemetry_target::ENGINE,
                    command,
                    "[editor] edit system needs an index and on or off"
                );
                return;
            }
        };
        let (edit, label) = systems::system_toggle(index, enabled);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            index,
            enabled,
            "[editor] system edit queued"
        );
    }

    /// One scripted preferences row (`edit preference <key> <value...>`):
    /// the same `SetPreferences` batch the tab's Save button queues, with
    /// the one row. The value is everything after the key, so a text value
    /// keeps its spaces.
    fn scripted_edit_preference<'a>(
        &mut self,
        parts: &mut std::str::SplitWhitespace<'a>,
        command: &str,
    ) {
        let Some(key) = parts.next() else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit preference needs a key and a value"
            );
            return;
        };
        let value = parts.collect::<Vec<_>>().join(" ");
        if value.is_empty() {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit preference needs a key and a value"
            );
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::SetPreferences {
                values: vec![(key.to_string(), value.clone())],
            }]);
        }
        info!(
            target: telemetry_target::ENGINE,
            key,
            value = value.as_str(),
            "[editor] preference edit queued"
        );
    }

    /// The gizmo settings from the world, defaults when no state exists.
    fn gizmo_settings(&mut self) -> GizmoSettings {
        let world = self.host.host_mut().engine_mut().world_mut();
        world
            .get_resource::<GizmoState>()
            .map(|state| state.settings)
            .unwrap_or_default()
    }

    /// `editor.tool_*` from the scripted channel: queue the same
    /// `SetGizmoTool` edit the Scene menu's items queue (applied, never
    /// recorded - a tool choice is a control, not an edit).
    fn scripted_gizmo_tool(
        &mut self,
        mode: Option<GizmoMode>,
        space: Option<GizmoSpace>,
        snap: Option<bool>,
    ) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::SetGizmoTool { mode, space, snap }]);
        }
    }

    /// `editor.entity_create` from the scripted channel: the recorded
    /// one-empty-entity edit the Entities menu's New Entity queues.
    fn scripted_entity_create(&mut self) {
        let (command, label) = entities::entity_create();
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![command], label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            "[editor] new entity queued"
        );
    }

    /// `editor.entity_delete`: the selected entity's recorded delete (the
    /// destroy and the selection clear as one step), or a note when the
    /// selection holds no entity.
    fn scripted_entity_delete(&mut self) {
        let (entity, asset) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<EditorSelection>()
                .map(|selection| (selection.entity, selection.asset.clone()))
                .unwrap_or((None, None))
        };
        let Some(entity) = entity else {
            info!(
                target: telemetry_target::ENGINE,
                "[editor] delete entity skipped nothing selected"
            );
            return;
        };
        let (commands, label) = entities::entity_delete(entity, asset);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(commands, label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %entity,
            "[editor] delete entity queued"
        );
    }

    /// Run `edit add <entity> <component>` / `edit remove <entity>
    /// <component>` from the scripted channel: the same recorded component
    /// attach or detach the Inspector's menu items queue.
    fn scripted_component_edit<'a>(
        &mut self,
        parts: &mut std::str::SplitWhitespace<'a>,
        command: &str,
        add: bool,
    ) {
        let (Some(entity_text), Some(component_text)) = (parts.next(), parts.next()) else {
            warn!(
                target: telemetry_target::ENGINE,
                command,
                "[editor] edit add or remove needs an entity and a component"
            );
            return;
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, entity_text) else {
            warn!(
                target: telemetry_target::ENGINE,
                entity = entity_text,
                "[editor] inspector set target not resolvable"
            );
            return;
        };
        let Some(component) = resolve_component_name(world, component_text) else {
            warn!(
                target: telemetry_target::ENGINE,
                component = component_text,
                "[editor] inspector set target not resolvable"
            );
            return;
        };
        let (edit, label) = if add {
            inspector::component_add(entity, &component)
        } else {
            inspector::component_remove(entity, &component)
        };
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            component = component.as_str(),
            add,
            "[editor] component edit queued"
        );
    }

    /// Draw the shell's frame through the overlay; without a shell module,
    /// draw the runner's own fallback.
    fn present_ui(&mut self) {
        // Step 1: take the shell frame, falling back to the runner's own
        // minimal pass while no shell module is loaded.
        let frame = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource_mut::<EguiShellFrame>() {
                Some(resource) => std::mem::take(&mut *resource),
                None => self.fallback_frame(),
            }
        };
        let EguiShellFrame {
            meshes,
            textures,
            pixels_per_point,
            wants_pointer,
            wants_keyboard,
            cursor_icon,
            clipboard_text,
            scene_rects,
            game_rect,
            overlay_regions,
            ime_allowed: _,
            ime_rect: _,
        } = frame;
        // Each scene viewport follows its dock rectangle, so the viewports
        // track the layout when the user drags a splitter; a viewport whose
        // tab is not shown hides its window.
        for (view, rect) in scene_rects.iter().enumerate() {
            match rect {
                Some(rect) => self.place_view_rect(view, *rect),
                None => self.hide_view(view),
            }
        }
        // The game viewport follows its own rectangle the same way: the
        // primary surface's window sits where the Game tab's body is, and
        // disappears while another tab of its group is active.
        match game_rect {
            Some(rect) => self.place_game_rect(rect),
            None => self.hide_game(),
        }
        // The chips the shell drew over the viewports, kept for the scene
        // click routing: a click inside them is the UI's, not the scene's.
        self.overlay_regions = overlay_regions;
        // Step 2: the routing decision is visible in the log on change.
        if wants_pointer != self.claimed_pointer || wants_keyboard != self.claimed_keyboard {
            info!(
                target: telemetry_target::ENGINE,
                pointer = wants_pointer,
                keyboard = wants_keyboard,
                "[editor] egui input claims changed"
            );
            self.claimed_pointer = wants_pointer;
            self.claimed_keyboard = wants_keyboard;
        }
        self.apply_cursor_icon(cursor_icon);
        if clipboard_text.is_some() {
            // The OS clipboard write needs the platform half back
            // (follow-up work); note it rather than dropping it silently.
            info!(
                target: telemetry_target::ENGINE,
                "[editor] UI clipboard write requested; the platform half is not ported yet"
            );
        }
        // Step 3: draw through the overlay; retry a missing overlay rarely,
        // so a transient compositor failure heals.
        if self.overlay.is_none() {
            self.retry_overlay();
        }
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        if let Err(error) = overlay.render(
            &self.device,
            &self.queue,
            &mut self.egui_renderer,
            &meshes,
            textures,
            pixels_per_point,
        ) {
            warn!(
                target: telemetry_target::ENGINE,
                error = %error,
                "[editor] overlay frame failed"
            );
        }
    }

    /// The runner's own minimal pass, used while no shell module is loaded.
    fn fallback_frame(&mut self) -> EguiShellFrame {
        if !self.reported_fallback {
            self.reported_fallback = true;
            info!(
                target: telemetry_target::ENGINE,
                module = SHELL_MODULE_NAME,
                "[editor] shell module missing; drawing the fallback UI"
            );
        }
        let logical_width = self.size.0 as f32 / self.scale_factor;
        let logical_height = self.size.1 as f32 / self.scale_factor;
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(logical_width, logical_height),
            )),
            ..Default::default()
        };
        let context = self.fallback_context.clone();
        let output = context.run(raw_input, |context| {
            egui::CentralPanel::default().show(context, |ui| {
                ui.heading("Pill Editor");
                ui.label(format!(
                    "the shell module ({SHELL_MODULE_NAME}) is not loaded; \
                     the dock and panels are unavailable"
                ));
            });
        });
        frame_from_output(&context, output)
    }

    /// Set the cursor shape egui asked for, when it maps onto one.
    fn apply_cursor_icon(&self, icon: egui::CursorIcon) {
        let shape = match icon {
            egui::CursorIcon::Default => winit::window::CursorIcon::Default,
            egui::CursorIcon::PointingHand => winit::window::CursorIcon::Pointer,
            egui::CursorIcon::Text => winit::window::CursorIcon::Text,
            egui::CursorIcon::Crosshair => winit::window::CursorIcon::Crosshair,
            egui::CursorIcon::Grab => winit::window::CursorIcon::Grab,
            egui::CursorIcon::Grabbing => winit::window::CursorIcon::Grabbing,
            _ => return,
        };
        self.window.set_cursor(shape);
    }

    /// Try to create the overlay again, at most every 120 frames.
    fn retry_overlay(&mut self) {
        if self.frame_index < self.overlay_retry_frame + 120 {
            return;
        }
        self.overlay_retry_frame = self.frame_index;
        match OverlaySurface::new(
            &self.instance,
            &self.adapter,
            &self.device,
            Arc::clone(&self.window),
            EDITOR_FORMAT,
            self.size,
            self.scale_factor,
        ) {
            Ok(overlay) => {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] composition overlay created"
                );
                self.overlay = Some(overlay);
            }
            Err(error) => {
                if !self.overlay_failed_logged {
                    self.overlay_failed_logged = true;
                    warn!(
                        target: telemetry_target::ENGINE,
                        error = %error,
                        "[editor] composition overlay still unavailable"
                    );
                }
            }
        }
    }

    /// Run the scripted commands appended since the last frame.
    fn poll_test_channel(&mut self) {
        let Some(channel) = &mut self.test_channel else {
            return;
        };
        let commands = channel.poll();
        for command in commands {
            let mut parts = command.split_whitespace();
            match parts.next() {
                Some("quit") => {
                    info!(target: telemetry_target::ENGINE, "[editor] quit requested");
                    self.quit = true;
                }
                Some("undock") => {
                    let tab = parts.next().and_then(parse_tab);
                    match tab {
                        Some(tab) => self.undock_tab(tab),
                        None => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] unknown undock target"
                        ),
                    }
                }
                Some("tab") => {
                    let tab = parts.next().and_then(parse_tab);
                    match tab {
                        Some(tab) => self.activate_tab(tab),
                        None => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] unknown tab target"
                        ),
                    }
                }
                Some("click") => {
                    let x = parts.next().and_then(|value| value.parse().ok());
                    let y = parts.next().and_then(|value| value.parse().ok());
                    match (x, y) {
                        (Some(x), Some(y)) => self.inject_click(x, y),
                        _ => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] click needs an x and a y"
                        ),
                    }
                }
                Some("rclick") => {
                    let x = parts.next().and_then(|value| value.parse().ok());
                    let y = parts.next().and_then(|value| value.parse().ok());
                    match (x, y) {
                        (Some(x), Some(y)) => self.inject_rclick(x, y),
                        _ => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] rclick needs an x and a y"
                        ),
                    }
                }
                Some("drag") => {
                    let x1 = parts.next().and_then(|value| value.parse().ok());
                    let y1 = parts.next().and_then(|value| value.parse().ok());
                    let x2 = parts.next().and_then(|value| value.parse().ok());
                    let y2 = parts.next().and_then(|value| value.parse().ok());
                    match (x1, y1, x2, y2) {
                        (Some(x1), Some(y1), Some(x2), Some(y2)) => {
                            self.start_scripted_drag((x1, y1), (x2, y2))
                        }
                        _ => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] drag needs x1 y1 x2 y2"
                        ),
                    }
                }
                Some("gizmo") => match parts.next() {
                    Some("drag") => {
                        let axis = parts.next().and_then(parse_gizmo_axis);
                        let delta_x = parts.next().and_then(|value| value.parse().ok());
                        let delta_y = parts.next().and_then(|value| value.parse().ok());
                        match (axis, delta_x, delta_y) {
                            (Some(axis), Some(delta_x), Some(delta_y)) => {
                                self.start_synthetic_gizmo_drag(axis, delta_x, delta_y)
                            }
                            _ => warn!(
                                target: telemetry_target::ENGINE,
                                command,
                                "[editor] gizmo drag needs an axis (x/y/z) and two pixel deltas"
                            ),
                        }
                    }
                    Some(other) => match parse_gizmo_mode(other) {
                        Some(mode) => self.set_gizmo_tool(mode),
                        None => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] unknown gizmo target"
                        ),
                    },
                    None => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] gizmo needs a tool or a drag"
                    ),
                },
                Some("dump") => match parts.next() {
                    Some("tabs") => self.dump_tabs(),
                    Some("scenes") => self.dump_scenes(),
                    Some("grid") => self.dump_grid(),
                    Some("assets") => self.dump_assets(),
                    Some("standalone-types") => self.dump_standalone_types(),
                    Some("selection") => self.dump_selection(),
                    Some("systems") => self.dump_systems(),
                    Some("preferences") => self.dump_preferences(),
                    Some("field") => {
                        let entity = parts.next();
                        let component = parts.next();
                        let field = parts.next();
                        match (entity, component, field) {
                            (Some(entity), Some(component), Some(field)) => {
                                self.dump_field(entity, component, field)
                            }
                            _ => warn!(
                                target: telemetry_target::ENGINE,
                                command,
                                "[editor] dump field needs an entity, a component and a field"
                            ),
                        }
                    }
                    _ => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] unknown dump target"
                    ),
                },
                Some("select") => match parts.next() {
                    Some("asset") => match parts.next() {
                        Some(path) => self.scripted_select_asset(path),
                        None => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] select asset needs a path"
                        ),
                    },
                    Some("entity") => match parts.next() {
                        Some(handle) => self.scripted_select_entity(handle),
                        None => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] select entity needs a handle"
                        ),
                    },
                    _ => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] unknown select target"
                    ),
                },
                Some("toolbar") => match parts.next() {
                    Some(action @ ("create" | "rename" | "delete" | "meta")) => {
                        self.scripted_toolbar(action)
                    }
                    _ => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] toolbar needs create, rename, delete or meta"
                    ),
                },
                Some("dialog") => match parts.next() {
                    Some("open") => match parts.next() {
                        Some(kind @ ("create" | "rename")) => self.scripted_toolbar(kind),
                        _ => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] dialog open needs create or rename"
                        ),
                    },
                    Some("set") => match parts.next() {
                        Some("name") => match parts.next() {
                            Some(name) => self.scripted_dialog_set_name(name),
                            None => warn!(
                                target: telemetry_target::ENGINE,
                                command,
                                "[editor] dialog set name needs the text"
                            ),
                        },
                        Some("type") => match parts.next() {
                            Some(type_name) => self.scripted_dialog_set_type(type_name),
                            None => warn!(
                                target: telemetry_target::ENGINE,
                                command,
                                "[editor] dialog set type needs the type name"
                            ),
                        },
                        _ => warn!(
                            target: telemetry_target::ENGINE,
                            command,
                            "[editor] dialog set needs name or type"
                        ),
                    },
                    Some("submit") => self.scripted_dialog_submit(),
                    Some("cancel") => self.scripted_dialog_cancel(),
                    _ => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] dialog needs open, set, submit or cancel"
                    ),
                },
                Some("edit") => match parts.next() {
                    Some("asset") => self.scripted_asset_edit(&mut parts, &command),
                    Some("set") => self.scripted_edit_set(&mut parts, &command),
                    Some("array") => self.scripted_edit_array(&mut parts, &command),
                    Some("asset-settings") => self.scripted_asset_settings(&mut parts, &command),
                    Some("system") => self.scripted_edit_system(&mut parts, &command),
                    Some("preference") => self.scripted_edit_preference(&mut parts, &command),
                    Some("add") => self.scripted_component_edit(&mut parts, &command, true),
                    Some("remove") => self.scripted_component_edit(&mut parts, &command, false),
                    _ => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] unknown edit target"
                    ),
                },
                Some("command") => match parts.next() {
                    Some("editor.undo") => self.scripted_undo(),
                    Some("editor.redo") => self.scripted_redo(),
                    Some("editor.pause_toggle") => self.scripted_pause_toggle(),
                    Some("editor.step") => self.scripted_step(),
                    Some("editor.entity_create") => self.scripted_entity_create(),
                    Some("editor.entity_delete") => self.scripted_entity_delete(),
                    Some("editor.tool_translate") => {
                        self.scripted_gizmo_tool(Some(GizmoMode::Translate), None, None)
                    }
                    Some("editor.tool_rotate") => {
                        self.scripted_gizmo_tool(Some(GizmoMode::Rotate), None, None)
                    }
                    Some("editor.tool_scale") => {
                        self.scripted_gizmo_tool(Some(GizmoMode::Scale), None, None)
                    }
                    Some("editor.tool_snap_toggle") => {
                        let snap = self.gizmo_settings().snap;
                        self.scripted_gizmo_tool(None, None, Some(!snap))
                    }
                    Some("editor.tool_space_toggle") => {
                        let space = self.gizmo_settings().space;
                        let next = match space {
                            GizmoSpace::World => GizmoSpace::Local,
                            GizmoSpace::Local => GizmoSpace::World,
                        };
                        self.scripted_gizmo_tool(None, Some(next), None)
                    }
                    Some(other) => warn!(
                        target: telemetry_target::ENGINE,
                        command = other,
                        "[editor] command not executable in the egui editor"
                    ),
                    None => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] command needs an id"
                    ),
                },
                Some("refresh") => match parts.next() {
                    Some("assets") => {
                        self.asset_listing_dirty = true;
                        info!(
                            target: telemetry_target::ENGINE,
                            "[editor] test assets refresh"
                        );
                    }
                    _ => warn!(
                        target: telemetry_target::ENGINE,
                        command,
                        "[editor] refresh needs a target"
                    ),
                },
                Some(other) => warn!(
                    target: telemetry_target::ENGINE,
                    command = other,
                    "[editor] unknown scripted command"
                ),
                None => {}
            }
        }
    }

    /// Inject a full right click at a point of the window's client area,
    /// through the same engine input seam a real click enters: the shell's
    /// egui pass hit-tests and routes it, so a context menu opens over
    /// whatever is under the pointer. No scene pick: a right click is the
    /// UI's, never the scene's.
    fn inject_rclick(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        let engine = self.host.host_mut().engine_mut();
        engine.push_input_event(InputEvent::CursorMoved { position });
        engine.push_input_event(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ButtonState::Pressed,
        });
        engine.push_input_event(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ButtonState::Released,
        });
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] right click injected"
        );
    }

    /// Inject a full left click at a point of the window's client area, in
    /// its physical pixels, through the same engine input seam a real click
    /// enters: the shell's egui pass hit-tests and routes it, so an overlay
    /// widget takes it while a point with no widget still reaches the
    /// scene. Press and release arrive in one input frame, which the engine
    /// reports as both edges; the egui bridge forwards them in order.
    fn inject_click(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        let engine = self.host.host_mut().engine_mut();
        engine.push_input_event(InputEvent::CursorMoved { position });
        engine.push_input_event(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Pressed,
        });
        engine.push_input_event(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Released,
        });
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] click injected"
        );
        // A click inside a visible scene viewport also asks what is under
        // it (Stage 13): the pick's answer moves the shared selection, and
        // the outline follows. A point over the overlay's chips is the
        // UI's, not the scene's.
        if let Some(view) = self.scene_view_at(x, y) {
            let scale = self.scale_factor;
            let over_ui = self
                .overlay_regions
                .iter()
                .any(|region| region.contains(x / scale, y / scale));
            if !over_ui {
                let (origin_x, origin_y) = self.view_origin(view);
                info!(
                    target: telemetry_target::ENGINE,
                    view,
                    x,
                    y,
                    "[editor] scene view click"
                );
                self.request_scene_pick(view, x - origin_x, y - origin_y);
            }
        }
    }

    /// Queue a scripted left-button drag (Stage 22): the press, the move
    /// and the release arrive one per frame through
    /// [`Self::advance_scripted_drag`], the shape egui's drag detection
    /// needs. The same engine input seam a real drag enters, so a panel's
    /// drag handle reacts exactly as it does for a user.
    fn start_scripted_drag(&mut self, from: (f32, f32), to: (f32, f32)) {
        if self.scripted_drag.is_some() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] drag refused: one is already in flight"
            );
            return;
        }
        self.scripted_drag = Some(ScriptedDrag { from, to, stage: 0 });
        info!(
            target: telemetry_target::ENGINE,
            from_x = from.0,
            from_y = from.1,
            to_x = to.0,
            to_y = to.1,
            "[editor] pointer drag queued"
        );
    }

    /// Advance a scripted drag one frame: the press first, then the move,
    /// then the release. The completion token marks the pointer free.
    fn advance_scripted_drag(&mut self) {
        let Some(drag) = self.scripted_drag.as_ref() else {
            return;
        };
        let (from, to, stage) = (drag.from, drag.to, drag.stage);
        match stage {
            0 => {
                let engine = self.host.host_mut().engine_mut();
                engine.push_input_event(InputEvent::CursorMoved {
                    position: pill_core::math::Vector2f::new(from.0, from.1),
                });
                engine.push_input_event(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Pressed,
                });
                if let Some(drag) = self.scripted_drag.as_mut() {
                    drag.stage = 1;
                }
            }
            1 => {
                let engine = self.host.host_mut().engine_mut();
                engine.push_input_event(InputEvent::CursorMoved {
                    position: pill_core::math::Vector2f::new(to.0, to.1),
                });
                if let Some(drag) = self.scripted_drag.as_mut() {
                    drag.stage = 2;
                }
            }
            _ => {
                let engine = self.host.host_mut().engine_mut();
                engine.push_input_event(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Released,
                });
                self.scripted_drag = None;
                info!(
                    target: telemetry_target::ENGINE,
                    from_x = from.0,
                    from_y = from.1,
                    to_x = to.0,
                    to_y = to.1,
                    "[editor] pointer drag injected"
                );
            }
        }
    }

    /// Float a dock tab into its own egui window, through the shell's state.
    fn undock_tab(&mut self, tab: EditorTab) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] undock needs the shell module's state"
            );
            return;
        };
        // The tab must leave the dock as it enters the window: a tab drawn
        // in two surfaces in one frame breaks egui's per-widget layer
        // invariant. `detach_tab` both removes it and creates the window.
        let dock = state.dock.state_mut();
        let Some(location) = dock.find_tab(&tab) else {
            warn!(
                target: telemetry_target::ENGINE,
                tab = tab_name(tab),
                "[editor] undock needs the tab in the dock"
            );
            return;
        };
        let rect = egui::Rect::from_min_size(egui::pos2(220.0, 160.0), egui::vec2(360.0, 480.0));
        let index = dock.detach_tab(location, rect);
        info!(
            target: telemetry_target::ENGINE,
            tab = tab_name(tab),
            surface = index.0,
            "[editor] tab undocked"
        );
    }

    /// Bring a dock tab to the front of its group, through the shell's
    /// state: the scripted channel's stand-in for clicking a tab button.
    /// The game viewport depends on it - only the tab that is drawn reports
    /// a rectangle - and so do the suite's checks of the other tabs.
    fn activate_tab(&mut self, tab: EditorTab) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] tab activation needs the shell module's state"
            );
            return;
        };
        let dock = state.dock.state_mut();
        let Some((surface, node, _)) = dock.find_tab(&tab) else {
            warn!(
                target: telemetry_target::ENGINE,
                tab = tab_name(tab),
                "[editor] tab activation needs the tab in the dock"
            );
            return;
        };
        // `set_active_tab` takes the tab's index inside its leaf, and the
        // node index must address the leaf on its own surface: a floating
        // surface's tree is separate.
        let Some(index) = dock[surface][node]
            .get_leaf()
            .and_then(|leaf| leaf.tabs.iter().position(|candidate| *candidate == tab))
        else {
            warn!(
                target: telemetry_target::ENGINE,
                tab = tab_name(tab),
                "[editor] tab activation needs the tab in a leaf"
            );
            return;
        };
        dock.set_active_tab((surface, node, pill_ui_egui::egui_dock::TabIndex(index)));
        dock.set_focused_node_and_surface((surface, node));
        info!(
            target: telemetry_target::ENGINE,
            tab = tab_name(tab),
            "[editor] tab activated"
        );
    }

    /// Log the dock's surfaces and tabs, for the suite's assertions.
    fn dump_tabs(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] dump needs the shell module's state"
            );
            return;
        };
        let surfaces = state.dock.state().surfaces_count();
        info!(
            target: telemetry_target::ENGINE,
            surfaces,
            "[editor] egui dock dump"
        );
    }
}

/// One scene view: its child window and its renderer surface.
struct SceneView {
    /// The child window the project's renderer presents the scene into.
    window: SurfaceWindow,
    /// The renderer surface id the scene draws through.
    surface_id: SurfaceId,
}

impl SceneView {
    /// Create the `view`-th scene view's child window.
    ///
    /// A refusal is logged and leaves that view without a window.
    fn open(event_loop: &ActiveEventLoop, main: &Window, view: usize) -> Option<SceneView> {
        let title = format!("Pill Editor Scene {}", view + 1);
        let window = SurfaceWindow::open(event_loop, main, &title, "scene")
            .map_err(|error| {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] scene view window refused"
                )
            })
            .ok()?;
        Some(SceneView {
            window,
            surface_id: SCENE_SURFACES[view],
        })
    }
}

// =============================================================================
// SurfaceWindow
// =============================================================================

/// One view surface's window: a child of the main window when the platform
/// allows one, the detached top-level fallback otherwise.
struct SurfaceWindow {
    /// The child window - or the detached fallback - the renderer presents
    /// into.
    window: Arc<Window>,
    /// Whether this is the detached fallback rather than a child window.
    detached: bool,
    /// Whether the window is shown; a window starts hidden and a placement
    /// shows it, because the dock may not show its tab yet.
    visible: bool,
    /// The last placed rectangle in physical pixels, so redundant moves
    /// and resizes are skipped.
    placed: Option<(i32, i32, u32, u32)>,
    /// The client-area origin in the parent's coordinates the last
    /// placement applied, in physical pixels. Pointer events this window
    /// reports are relative to it; a resize clears `placed` but never moves
    /// the window, so the origin survives that.
    origin: (i32, i32),
}

impl SurfaceWindow {
    /// Create a view window: a child of the main window when the platform
    /// allows one, the detached fallback otherwise.
    fn open(
        event_loop: &ActiveEventLoop,
        main: &Window,
        title: &str,
        kind: &str,
    ) -> Result<SurfaceWindow, EditorError> {
        // Step 1: prefer a child window, so the view is composited inside
        // the editor's content area. winit takes the parent as a raw
        // handle; without one there is no child to try.
        let force_detached = std::env::var_os("PILL_EDITOR_DETACHED_SCENE").is_some();
        let child_attributes = (!force_detached).then(|| {
            main.window_handle().ok().map(|handle| {
                // SAFETY: the handle borrows the main window, which outlives
                // the view window: the surfaces drop before the main window.
                unsafe {
                    Window::default_attributes()
                        .with_title(title)
                        .with_decorations(false)
                        .with_parent_window(Some(handle.as_raw()))
                        .with_inner_size(INITIAL_SCENE_SIZE)
                        .with_visible(false)
                }
            })
        });
        let child_attributes = child_attributes.flatten();
        if force_detached {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] child {kind} window forced off; using the detached fallback"
            );
        }
        if let Some(child_attributes) = child_attributes {
            match event_loop.create_window(child_attributes) {
                Ok(window) => {
                    return Ok(SurfaceWindow {
                        window: Arc::new(window),
                        detached: false,
                        visible: false,
                        placed: None,
                        origin: (0, 0),
                    })
                }
                Err(error) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        error = %error,
                        "[editor] child {kind} window refused, using the detached fallback"
                    );
                }
            }
        }
        // Step 2: the detached fallback is a required, working path on
        // every platform.
        let detached_attributes = Window::default_attributes()
            .with_title(format!("{title} (detached)"))
            .with_inner_size(INITIAL_SCENE_SIZE)
            .with_visible(false);
        let window = event_loop
            .create_window(detached_attributes)
            .map_err(|source| EditorError::WindowCreation { source })?;
        Ok(SurfaceWindow {
            window: Arc::new(window),
            detached: true,
            visible: false,
            placed: None,
            origin: (0, 0),
        })
    }

    /// Place the window at the layout rectangle, in physical pixels,
    /// showing it if it was hidden, and skipping redundant moves. The
    /// detached fallback ignores the position: it is its own top-level
    /// window.
    fn place(&mut self, x: i32, y: i32, width: u32, height: u32) {
        // A placement means the dock shows this window's tab again after a
        // switch or an undock; a hidden window still sits where it was, so
        // only the visibility changes.
        if !self.visible {
            self.window.set_visible(true);
            self.visible = true;
        }
        if self.placed == Some((x, y, width, height)) {
            return;
        }
        if !self.detached {
            self.window.set_outer_position(PhysicalPosition::new(x, y));
            // Recorded separately from the placement cache: pointer events
            // this window reports are relative to it, and a resize (which
            // clears the cache) does not move the window.
            self.origin = (x, y);
        }
        let _ = self
            .window
            .request_inner_size(PhysicalSize::new(width, height));
        self.placed = Some((x, y, width, height));
    }

    /// Hide the window: the dock no longer shows its tab, so nothing may
    /// paint in its rectangle.
    fn hide(&mut self) {
        if self.visible {
            self.window.set_visible(false);
            self.visible = false;
        }
    }

    /// The window's current physical size.
    fn size(&self) -> (u32, u32) {
        let size = self.window.inner_size();
        (size.width.max(1), size.height.max(1))
    }
}

impl SceneView {
    /// Attach the scene window to the renderer and publish its camera.
    fn attach(&mut self, host: &mut RenderingHost, camera: &EditorCamera, size: (u32, u32)) {
        match host.attach_surface(
            self.surface_id,
            Arc::clone(&self.window.window),
            size.0,
            size.1,
            SurfaceOptions::default(),
        ) {
            Ok(()) => {
                host.set_surface_camera(self.surface_id, Some(camera.binding()));
                info!(
                    target: telemetry_target::ENGINE,
                    detached = self.window.detached,
                    "[editor] scene surface attached"
                );
            }
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                error = %error,
                "[editor] scene surface refused; the scene view stays empty"
            ),
        }
    }
}

/// Shift a pointer event the scene window reported into the main window's
/// client coordinates. Only cursor positions move: buttons and wheels carry
/// no position of their own, and egui reads the cursor's for them.
fn offset_pointer(event: InputEvent, (dx, dy): (f32, f32)) -> InputEvent {
    if dx == 0.0 && dy == 0.0 {
        return event;
    }
    match event {
        InputEvent::CursorMoved { position } => InputEvent::CursorMoved {
            position: pill_core::math::Vector2f::new(position.x + dx, position.y + dy),
        },
        other => other,
    }
}

/// Parse a tab name from a scripted command.
fn parse_tab(name: &str) -> Option<EditorTab> {
    match name {
        "scene" => Some(EditorTab::Scene),
        "scene2" => Some(EditorTab::SecondScene),
        "game" => Some(EditorTab::Game),
        "entities" => Some(EditorTab::Entities),
        "systems" => Some(EditorTab::Systems),
        "inspector" => Some(EditorTab::Inspector),
        "assets" => Some(EditorTab::Assets),
        "preferences" => Some(EditorTab::Preferences),
        _ => None,
    }
}

/// The tab's display name for log lines.
fn tab_name(tab: EditorTab) -> &'static str {
    match tab {
        EditorTab::Scene => "scene",
        EditorTab::SecondScene => "scene2",
        EditorTab::Game => "game",
        EditorTab::Entities => "entities",
        EditorTab::Systems => "systems",
        EditorTab::Inspector => "inspector",
        EditorTab::Assets => "assets",
        EditorTab::Preferences => "preferences",
    }
}

// =============================================================================
// EditorCamera
// =============================================================================

/// The editor's own camera for the Scene view, as plain data (D29).
///
/// The camera never enters the world - the scene surface is bound to it as
/// a [`CameraBinding::View`], which keeps project systems from seeing or
/// moving it. Navigation gestures are follow-up work (Stage 10).
struct EditorCamera {
    /// The point the camera looks at and orbits around.
    pivot: [f32; 3],
    /// Rotation around the world's Y axis, in radians.
    yaw: f32,
    /// Downward tilt, in radians; positive looks down at the pivot.
    pitch: f32,
    /// Distance from the pivot.
    distance: f32,
}

impl Default for EditorCamera {
    fn default() -> Self {
        Self {
            pivot: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: 0.30,
            distance: 16.0,
        }
    }
}

impl EditorCamera {
    /// The direction the camera looks along.
    fn forward(&self) -> [f32; 3] {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        [-cos_pitch * sin_yaw, -sin_pitch, -cos_pitch * cos_yaw]
    }

    /// The camera's right direction, horizontal by construction: the gizmo's
    /// screen-space rays are built from it.
    fn right(&self) -> [f32; 3] {
        let forward = self.forward();
        let horizontal = (forward[0] * forward[0] + forward[2] * forward[2]).sqrt();
        if horizontal <= f32::EPSILON {
            return [1.0, 0.0, 0.0];
        }
        [-forward[2] / horizontal, 0.0, forward[0] / horizontal]
    }

    /// Where the camera sits: the pivot, backed off along the look
    /// direction.
    fn eye(&self) -> [f32; 3] {
        let forward = self.forward();
        [
            self.pivot[0] - forward[0] * self.distance,
            self.pivot[1] - forward[1] * self.distance,
            self.pivot[2] - forward[2] * self.distance,
        ]
    }

    /// The camera's orientation as a quaternion built from yaw and pitch,
    /// in the transform's `[x, y, z, w]` layout.
    fn rotation(&self) -> [f32; 4] {
        let half_yaw = self.yaw * 0.5;
        let half_pitch = self.pitch * 0.5;
        let yaw_quaternion = [0.0, half_yaw.sin(), 0.0, half_yaw.cos()];
        let pitch_quaternion = [(-half_pitch).sin(), 0.0, 0.0, half_pitch.cos()];
        quaternion_product(yaw_quaternion, pitch_quaternion)
    }

    /// The binding the scene surface draws through.
    fn binding(&self) -> CameraBinding {
        CameraBinding::View {
            camera: CameraComponent::default(),
            transform: TransformComponent {
                translation: self.eye(),
                rotation: self.rotation(),
                ..Default::default()
            },
        }
    }
}

/// The Hamilton product of two `[x, y, z, w]` quaternions.
fn quaternion_product(left: [f32; 4], right: [f32; 4]) -> [f32; 4] {
    let [left_x, left_y, left_z, left_w] = left;
    let [right_x, right_y, right_z, right_w] = right;
    [
        left_w * right_x + left_x * right_w + left_y * right_z - left_z * right_y,
        left_w * right_y - left_x * right_z + left_y * right_w + left_z * right_x,
        left_w * right_z + left_x * right_y - left_y * right_x + left_z * right_w,
        left_w * right_w - left_x * right_x - left_y * right_y - left_z * right_z,
    ]
}

// =============================================================================
// Scripted dump helpers
// =============================================================================

/// Resolve an `NvG` display handle against the live entity table, so a
/// scripted dump can never address an entity the world does not hold.
fn resolve_display_entity(world: &pill_engine_core::World, display: &str) -> Option<Entity> {
    let (id, generation) = display.split_once('v')?;
    let id: u64 = id.parse().ok()?;
    let generation: u32 = generation.parse().ok()?;
    world
        .entity_rows()
        .into_iter()
        .find(|row| row.entity.id() == id && row.entity.generation() == generation)
        .map(|row| row.entity)
}

/// Resolve a component type name that may be abbreviated to its `::Name`
/// suffix; an ambiguous abbreviation resolves to nothing.
fn resolve_component_name(world: &pill_engine_core::World, name: &str) -> Option<String> {
    let suffix = format!("::{name}");
    let mut matches = world
        .registered_components()
        .into_iter()
        .filter(|(registered, _)| registered == name || registered.ends_with(&suffix))
        .map(|(registered, _)| registered);
    let first = matches.next()?;
    if matches.any(|candidate| candidate != first) {
        return None;
    }
    Some(first)
}

// =============================================================================
// Gizmo helpers (Stage 15)
// =============================================================================

/// The wire name of a tool mode, for logs and suite tokens.
fn gizmo_mode_name(mode: GizmoMode) -> &'static str {
    match mode {
        GizmoMode::Translate => "translate",
        GizmoMode::Rotate => "rotate",
        GizmoMode::Scale => "scale",
    }
}

/// The wire name of a gizmo space.
fn gizmo_space_name(space: GizmoSpace) -> &'static str {
    match space {
        GizmoSpace::World => "world",
        GizmoSpace::Local => "local",
    }
}

/// The wire name of one axis, for logs and suite tokens.
fn gizmo_axis_name(axis: usize) -> &'static str {
    match axis {
        0 => "x",
        1 => "y",
        _ => "z",
    }
}

/// Parse a scripted tool name (`gizmo translate` and friends).
fn parse_gizmo_mode(name: &str) -> Option<GizmoMode> {
    match name {
        "translate" => Some(GizmoMode::Translate),
        "rotate" => Some(GizmoMode::Rotate),
        "scale" => Some(GizmoMode::Scale),
        _ => None,
    }
}

/// Parse a scripted axis name.
fn parse_gizmo_axis(name: &str) -> Option<usize> {
    match name {
        "x" => Some(0),
        "y" => Some(1),
        "z" => Some(2),
        _ => None,
    }
}

/// Read one entity's transform as plain arrays; `None` when the entity is
/// not alive, carries no transform, or a field does not read back as floats.
fn read_transform(world: &pill_engine_core::World, entity: Entity) -> Option<TransformComponent> {
    let read = |field: &str| -> Option<Vec<f32>> {
        let value = world
            .read_component_field(entity, TRANSFORM_SHARED_NAME, field)
            .ok()?;
        let FieldValue::Array(elements) = value else {
            return None;
        };
        elements
            .iter()
            .map(|element| match element {
                FieldValue::F32(value) => Some(*value),
                _ => None,
            })
            .collect()
    };
    let translation: [f32; 3] = read("translation")?.try_into().ok()?;
    let rotation: [f32; 4] = read("rotation")?.try_into().ok()?;
    let scale: [f32; 3] = read("scale")?.try_into().ok()?;
    Some(TransformComponent {
        translation,
        rotation,
        scale,
    })
}

/// The frame the gizmo's axes are measured in.
fn gizmo_basis(space: GizmoSpace, transform: &TransformComponent) -> [[f32; 3]; 3] {
    if space == GizmoSpace::World {
        return gizmo::AXES;
    }
    [
        gizmo::rotate_vector(transform.rotation, gizmo::AXES[0]),
        gizmo::rotate_vector(transform.rotation, gizmo::AXES[1]),
        gizmo::rotate_vector(transform.rotation, gizmo::AXES[2]),
    ]
}

/// The tangent of half the vertical field of view, and the aspect ratio.
fn projection_terms(width: f32, height: f32) -> (f32, f32) {
    let fov = CameraComponent::default().vertical_fov.clamp(1.0, 179.0);
    let tan = (fov.to_radians() * 0.5).tan();
    (tan, (width / height).max(1e-3))
}

/// One gizmo drag in flight.
///
/// The press records where the drag started - the axis parameter or ring
/// angle the cursor ray named, plus the transform it started from - so every
/// move applies a delta against it and nothing jumps on the first pixel.
struct GizmoDrag {
    /// The scene view the drag belongs to: every move re-reads that view's
    /// ray.
    view: usize,
    /// The tool mode the grabbed handle belongs to.
    mode: GizmoMode,
    /// The axis index the handle belongs to.
    axis: usize,
    /// The entity being dragged.
    entity: Entity,
    /// The transform at the press; every move is computed from it, so a
    /// snapped drag cannot accumulate rounding.
    original: TransformComponent,
    /// The axis parameter (translate, scale) or ring angle (rotate) at the
    /// press.
    start_value: f32,
    /// How long the handles were at the press, in world units: the scale
    /// drag's reference.
    reference: f32,
    /// The value written on the previous move, for the end log.
    last_value: Option<[f32; 4]>,
}
