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
use pill_editor_core_egui::camera::{self, CameraButton, CameraNavigation, Modifiers, OrbitCamera};
use pill_editor_core_egui::commands::{
    ensure_command_resources, read_gizmo_settings, write_gizmo_preferences, EditorPreferences,
    KeymapPreset, PendingEditorCommands, PREFERENCES_FILE,
};
use pill_editor_core_egui::drain::{self, HistoryDirection};
use pill_editor_core_egui::editor_state::{
    AssetDetail, AssetListing, EditorCommand, EditorSelection, EditorSnapshot, StandaloneTypeInfo,
};
use pill_editor_core_egui::entities;
use pill_editor_core_egui::gizmo::{
    self, GizmoGeometry, GizmoMode, GizmoSettings, GizmoSpace, GizmoState, HandleKind, HandleStates,
};
use pill_editor_core_egui::history::CommandHistory;
use pill_editor_core_egui::inspector;
use pill_editor_core_egui::layout::{
    layout_directory, EditorTab, BOTTOM_ROW_FRACTION, SIDE_COLUMN_FRACTION,
};
use pill_editor_core_egui::notices::{EditorNotices, Notice, NoticeLevel, ProblemRow};
use pill_editor_core_egui::preferences::{self, PreferencesView};
use pill_editor_core_egui::problems::{self as problem_parser, Severity};
use pill_editor_core_egui::scripted::{AssetEdit, Dump, KeyChord, ScriptedCommand, ToolbarAction};
use pill_editor_core_egui::session::{EguiShellState, HistoryRequest};
use pill_editor_core_egui::systems;
use pill_engine_core::common_components::TRANSFORM_SHARED_NAME;
use pill_engine_core::debug_draw::{DebugDraw, DebugStyle};
use pill_engine_core::input::{ButtonState, KeyCode, Modifiers as EngineModifiers, MouseButton};
use pill_engine_core::Entity;
use pill_engine_core::FieldValue;
use pill_engine_core::InputEvent;
use pill_host::{take_notices, HostConfig, HostNotice, RenderingHost};
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

/// How far the axis constraint guide reaches on both sides of the gizmo, as
/// a fraction of the handle length: long enough to leave any view.
const GIZMO_GUIDE_SPAN: f32 = 200.0;

/// The hover test's radius around a handle, as a fraction of the handle
/// length: roughly the width the overlay draws a line with.
const GIZMO_HOVER_RADIUS: f32 = 0.07;

/// One dash (and one gap) of the translation displacement line, as a
/// fraction of the handle length.
const GIZMO_DASH: f32 = 0.05;

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
                // A window losing focus takes every press with it: no
                // release is coming, so no interaction may stay in flight.
                if !focused {
                    editor.cancel_interactions("focus lost");
                }
                if is_main {
                    editor.focused = focused;
                }
            }
            other => {
                // The modifiers are read here, before the event is routed:
                // navigation chords match on them, and winit reports them on
                // the window that has focus.
                if let WindowEvent::ModifiersChanged(modifiers) = &other {
                    let state = modifiers.state();
                    editor.modifiers = Modifiers {
                        shift: state.shift_key(),
                        control: state.control_key(),
                        alt: state.alt_key(),
                    };
                }
                // The scene and game windows' pointer positions are
                // relative to their own client areas, while the engine -
                // which also feeds the shell's egui pass - works in the
                // main window's client coordinates. The window's placed
                // origin bridges the two, so hovering and clicking the
                // overlay strip works.
                if let WindowEvent::KeyboardInput { event, .. } = &other {
                    editor.gizmo_shortcut(event);
                }
                // A release over the editor's own UI still finishes a gizmo
                // drag or a navigation that left its viewport's window: no
                // pointer capture is platform-independent, so the release
                // is honoured wherever it lands.
                if let WindowEvent::MouseInput { state, button, .. } = &other {
                    if is_main
                        && *button == winit::event::MouseButton::Left
                        && *state == winit::event::ElementState::Released
                    {
                        editor.release_scene(editor.active_view);
                        editor.end_camera_navigation();
                    }
                }
                if let Some(view) = child_view {
                    editor.scene_window_event(view, &other);
                }
                let origin = match child_view {
                    Some(view) => editor.view_origin(view),
                    None if is_game => editor.game_origin(),
                    None => (0.0, 0.0),
                };
                // The focus rule (W-0038 Stage 1): the game's window is the
                // game's, so its events become the engine's input and the
                // shell never sees them. The editor's own windows - the main
                // one and the scene views - are the UI's: their events feed
                // the shell, and a project system never receives a keystroke
                // meant for the Inspector.
                for event in translate_window_event(&other) {
                    let event = offset_pointer(event, origin);
                    if is_game {
                        editor.pending_events.push(event);
                    } else {
                        match &event {
                            InputEvent::CursorMoved { position } => {
                                editor.shell_cursor = Some(*position);
                            }
                            InputEvent::CursorLeft => editor.shell_cursor = None,
                            _ => {}
                        }
                        editor.shell_events.push(event);
                    }
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
    /// The editor's cameras, one per scene viewport: navigation moves the
    /// active viewport's camera only, and every scene ray projects through
    /// its own view's camera.
    cameras: [OrbitCamera; SCENE_VIEWPORTS],
    /// The viewport the last scene click activated; camera navigation and
    /// the gizmo read it.
    active_view: usize,
    /// Whether each scene view's left button is down: a pick answers a frame
    /// or two after the press, and a handle grab only starts a drag while
    /// the button is still held.
    left_down: [bool; SCENE_VIEWPORTS],
    /// The camera navigation drag in flight, when one is.
    camera_drag: Option<CameraDrag>,
    /// The keyboard modifiers held, as of the last modifiers event.
    modifiers: Modifiers,
    /// The keymap preset the preferences name: the navigation table in force.
    keymap: KeymapPreset,
    /// Whether each view's published camera binding is stale: navigation
    /// marks it and the next frame re-publishes the surface camera.
    camera_dirty: [bool; SCENE_VIEWPORTS],
    /// egui's own renderer on the editor's device.
    egui_renderer: egui_wgpu::Renderer,
    /// The composition overlay the UI is presented through; `None` until it
    /// is created, and left `None` when the platform refuses it.
    overlay: Option<OverlaySurface>,
    /// Frames between overlay creation attempts after a failure.
    overlay_retry_frame: u64,
    /// The editor's fallback context, used while no shell module is loaded.
    fallback_context: egui::Context,
    /// Whether the fallback UI was reported already.
    reported_fallback: bool,
    /// The window events not yet pushed into the engine's input.
    pending_events: Vec<InputEvent>,
    /// The editor's own windows' events, not yet handed to the shell.
    shell_events: Vec<InputEvent>,
    /// The pointer's last position over the editor's windows, in the main
    /// window's client pixels; the shell input carries it so a release that
    /// arrives without a move still has a place to land.
    shell_cursor: Option<pill_core::math::Vector2f>,
    /// The notices the host's queue produced: the toast events and the
    /// Problems rows the shell reads.
    notices: EditorNotices,
    /// The last pointer position each scene window reported, in its own
    /// client-area physical pixels; `None` before its first move.
    scene_cursor: [Option<(f32, f32)>; SCENE_VIEWPORTS],
    /// The scene left press waiting for its pick answer: the view, the press
    /// position and the pick id. The answer starts a gizmo drag on a handle
    /// or moves the selection - both while the button is held.
    press_pick: Option<PressPick>,
    /// The entity the renderer's outline set currently names, so the world
    /// resource is written once per selection change.
    outlined_entity: Option<Entity>,
    /// The overlay chips' rectangles the shell reported last frame, in
    /// logical points: clicks inside them are the UI's, not the scene's.
    overlay_regions: Vec<ShellRect>,
    /// The gizmo drag in flight, when one is.
    gizmo_drag: Option<GizmoDrag>,
    /// The gizmo geometry drawn this frame: the hover test runs against the
    /// exact handles on screen, so the highlight cannot disagree with them.
    gizmo_geometry: GizmoGeometry,
    /// The handle under the pointer, when one is.
    hovered_handle: Option<u32>,
    /// How long the handles were when they were last built, in world units:
    /// the hover test's radius and the drag guides' dash length follow it.
    gizmo_reference: f32,
    /// Whether the drag guides drew last frame, so the log line appears on
    /// the state change instead of every frame.
    gizmo_guides: Option<bool>,
    /// The last logged gizmo anchor header (mode, space, x, y): the log
    /// line appears on changes and on the heartbeat.
    gizmo_logged: Option<(GizmoMode, GizmoSpace, i32, i32)>,
    /// The pick ids a scripted gizmo grab pressed: the first answer that
    /// names a handle starts the drag, a full miss refuses it.
    synthetic_press_ids: Vec<u64>,
    /// The pixel delta a scripted grab applies once its pick answers.
    synthetic_gizmo_pending: Option<(f32, f32)>,
    /// Whether a scripted press holds the drag open instead of finishing it:
    /// `gizmo press`, `gizmo move` and `gizmo release` drive the same drag a
    /// real mouse does, one command per frame.
    synthetic_gizmo_hold: bool,
    /// The press pixel of the scripted drag in flight, in its view's
    /// physical pixels: every scripted move measures its delta from here.
    synthetic_gizmo_press: Option<(f32, f32)>,
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
    /// When the heartbeat last printed; the heartbeat fires every 210 frames
    /// or every two seconds, whichever comes first, so a slow run (the
    /// benchmark's debug posture) still reports inside its window.
    last_heartbeat: Option<Instant>,
    /// The smoothed frame rate, logged with the heartbeat.
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
        // surfaces, each with its own camera (navigation moves the active
        // viewport's). Both start hidden; each shows when the dock shows its
        // tab.
        let cameras = [OrbitCamera::default(); SCENE_VIEWPORTS];
        let mut views: [Option<SceneView>; SCENE_VIEWPORTS] = [None, None];
        for (view, slot) in views.iter_mut().enumerate() {
            let Some(mut scene) = SceneView::open(event_loop, &window, view) else {
                continue;
            };
            let scene_size = scene.window.size();
            scene.attach(&mut host, &cameras[view], scene_size);
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
            cameras,
            active_view: 0,
            left_down: [false; SCENE_VIEWPORTS],
            camera_drag: None,
            modifiers: Modifiers::default(),
            keymap: KeymapPreset::default(),
            camera_dirty: [true; SCENE_VIEWPORTS],
            egui_renderer,
            overlay,
            overlay_retry_frame: 0,
            fallback_context: {
                // The fallback draws the same theme as the shell, so a
                // missing shell module does not flip the look.
                let context = egui::Context::default();
                pill_ui_egui::style::apply(&context);
                context
            },
            reported_fallback: false,
            pending_events: Vec::new(),
            shell_events: Vec::new(),
            shell_cursor: None,
            notices: EditorNotices::default(),
            scene_cursor: [None; SCENE_VIEWPORTS],
            press_pick: None,
            outlined_entity: None,
            overlay_regions: Vec::new(),
            gizmo_drag: None,
            gizmo_geometry: GizmoGeometry::default(),
            hovered_handle: None,
            gizmo_reference: 0.0,
            gizmo_guides: None,
            gizmo_logged: None,
            synthetic_press_ids: Vec::new(),
            synthetic_gizmo_pending: None,
            synthetic_gizmo_hold: false,
            synthetic_gizmo_press: None,
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
            last_heartbeat: None,
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
        scaled_view_rect(rect, self.scale_factor, self.size)
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

    /// Route one scene window's event: activate the viewport on any press,
    /// start camera navigation on its chord, start a gizmo drag or ask a
    /// pick on a left press, feed a drag in flight its moves, and end what
    /// the release ends.
    ///
    /// The viewport that owns an interaction keeps it: winit reports a
    /// drag's moves to the window whose press started it even when the
    /// pointer leaves it, so a gizmo drag or a navigation that crosses the
    /// viewport's edge carries on and no other viewport takes the pointer
    /// mid-drag.
    fn scene_window_event(&mut self, view: usize, event: &WindowEvent) {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let point = (position.x as f32, position.y as f32);
                self.scene_cursor[view] = Some(point);
                if let Some(drag) = &self.camera_drag {
                    if drag.view == view {
                        self.update_camera_navigation(point);
                        return;
                    }
                }
                if let Some(drag) = &self.gizmo_drag {
                    if drag.view == view {
                        self.update_gizmo_drag(point.0, point.1);
                    }
                    return;
                }
                self.update_hover(view, point);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = *state == winit::event::ElementState::Pressed;
                let button = match button {
                    winit::event::MouseButton::Left => CameraButton::Left,
                    winit::event::MouseButton::Middle => CameraButton::Middle,
                    winit::event::MouseButton::Right => CameraButton::Right,
                    _ => return,
                };
                if button == CameraButton::Left {
                    self.left_down[view] = pressed;
                }
                if !pressed {
                    if button == CameraButton::Left {
                        self.release_scene(view);
                    }
                    if self
                        .camera_drag
                        .as_ref()
                        .is_some_and(|drag| drag.view == view)
                    {
                        self.end_camera_navigation();
                    }
                    return;
                }
                // A press over the overlay's chips is the UI's: no activation
                // and no scene interaction.
                if self.scene_point_is_ui(view) {
                    return;
                }
                self.activate_view(view);
                let Some(point) = self.scene_cursor[view] else {
                    return;
                };
                if let Some(navigation) = self.navigation_for(button) {
                    self.begin_camera_navigation(view, navigation, point);
                    return;
                }
                if button == CameraButton::Left {
                    self.press_scene(view, point.0, point.1);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, lines) => *lines,
                    winit::event::MouseScrollDelta::PixelDelta(position) => {
                        position.y as f32 / 40.0
                    }
                };
                if lines == 0.0 {
                    return;
                }
                // The wheel zooms the viewport it arrived over without
                // taking the active slot: a click is what activates.
                self.zoom_camera(view, lines);
            }
            _ => {}
        }
    }

    /// Make `view` the active viewport: the one camera navigation, the
    /// gizmo's handle length and the viewport commands address. Only a
    /// press inside a scene view reaches here - a click on the editor's own
    /// UI never activates a viewport.
    fn activate_view(&mut self, view: usize) {
        if self.active_view == view {
            return;
        }
        self.active_view = view;
        info!(
            target: telemetry_target::ENGINE,
            view,
            "[editor] active viewport"
        );
    }

    /// The navigation gesture a press means right now, if any: no gesture
    /// while a gizmo drag or another navigation owns the pointer.
    fn navigation_for(&self, button: CameraButton) -> Option<CameraNavigation> {
        if self.gizmo_drag.is_some() || self.camera_drag.is_some() {
            return None;
        }
        camera::navigation_binding(self.keymap, button, self.modifiers)
    }

    /// Ask what is under a left press, while the button is held: the answer
    /// either starts a gizmo drag on the handle it names or moves the shared
    /// selection.
    fn press_scene(&mut self, view: usize, x: f32, y: f32) {
        let physical_x = x.max(0.0).round() as u32;
        let physical_y = y.max(0.0).round() as u32;
        let Some(id) = self.queue_scene_pick_at(view, physical_x, physical_y) else {
            return;
        };
        self.press_pick = Some(PressPick {
            view,
            x: physical_x as f32,
            y: physical_y as f32,
            id,
        });
        info!(
            target: telemetry_target::ENGINE,
            view,
            x = physical_x,
            y = physical_y,
            id,
            "[editor] pick requested"
        );
    }

    /// A left release in `view`: it ends the gizmo drag a press started -
    /// the drag applied its moves all along - and does nothing when the
    /// press became a selection.
    fn release_scene(&mut self, view: usize) {
        if self
            .gizmo_drag
            .as_ref()
            .is_some_and(|drag| drag.view == view)
        {
            self.end_gizmo_drag();
        }
    }

    /// Begin a camera navigation: the press records the anchor pixel every
    /// later move measures its delta against.
    fn begin_camera_navigation(
        &mut self,
        view: usize,
        navigation: CameraNavigation,
        start: (f32, f32),
    ) {
        if self.gizmo_drag.is_some() || self.camera_drag.is_some() {
            return;
        }
        self.camera_drag = Some(CameraDrag {
            view,
            navigation,
            last: start,
        });
        info!(
            target: telemetry_target::ENGINE,
            view,
            navigation = camera_navigation_name(navigation),
            "[editor] camera navigation started"
        );
    }

    /// Apply one navigation move: the pixel delta since the last move goes
    /// through the gesture's own clamped math.
    fn update_camera_navigation(&mut self, point: (f32, f32)) {
        let Some(mut drag) = self.camera_drag.take() else {
            return;
        };
        let (delta_x, delta_y) = (point.0 - drag.last.0, point.1 - drag.last.1);
        drag.last = point;
        self.apply_camera_gesture(drag.view, drag.navigation, delta_x, delta_y);
        if self.camera_drag.is_none() {
            self.camera_drag = Some(drag);
        }
    }

    /// Move one view's camera by a gesture's pixel delta; the drags and the
    /// scripted channel both land here.
    fn apply_camera_gesture(
        &mut self,
        view: usize,
        navigation: CameraNavigation,
        delta_x: f32,
        delta_y: f32,
    ) {
        if delta_x == 0.0 && delta_y == 0.0 {
            return;
        }
        let (_, height) = self.scene_pixel_size(view).unwrap_or((1.0, 1.0));
        let fov = CameraComponent::default()
            .vertical_fov
            .clamp(1.0, 179.0)
            .to_radians();
        let camera = &mut self.cameras[view];
        match navigation {
            CameraNavigation::Orbit => camera.orbit(delta_x, delta_y),
            CameraNavigation::Pan => {
                let world_per_pixel = 2.0 * camera.distance * (fov * 0.5).tan() / height.max(1.0);
                camera.pan(delta_x, delta_y, world_per_pixel);
            }
            CameraNavigation::Dolly => camera.dolly(delta_y),
        }
        self.camera_dirty[view] = true;
    }

    /// End the navigation in flight, logging the gesture that finished.
    fn end_camera_navigation(&mut self) {
        let Some(drag) = self.camera_drag.take() else {
            return;
        };
        info!(
            target: telemetry_target::ENGINE,
            view = drag.view,
            navigation = camera_navigation_name(drag.navigation),
            "[editor] camera navigation ended"
        );
    }

    /// Zoom one view's camera by a wheel's line count.
    fn zoom_camera(&mut self, view: usize, lines: f32) {
        self.cameras[view].zoom(lines);
        self.camera_dirty[view] = true;
        info!(
            target: telemetry_target::ENGINE,
            view,
            lines,
            distance = self.cameras[view].distance,
            "[editor] camera zoom"
        );
    }

    /// Update the hovered handle from the pointer's ray, against the handles
    /// the last frame drew. The radius follows the handle length, so it
    /// reads like the drawn line rather than a fixed world distance.
    fn update_hover(&mut self, view: usize, point: (f32, f32)) {
        if self.gizmo_drag.is_some() || self.camera_drag.is_some() {
            return;
        }
        let Some((ray_origin, ray_direction)) = self.scene_ray(view, point.0, point.1) else {
            self.set_hovered_handle(view, None);
            return;
        };
        let radius = (self.gizmo_reference * GIZMO_HOVER_RADIUS).max(1e-4);
        let hit = gizmo::hit_test(&self.gizmo_geometry, ray_origin, ray_direction, radius);
        self.set_hovered_handle(view, hit);
    }

    /// Record the hovered handle, logging the change once - the suite's
    /// window into the feedback states.
    fn set_hovered_handle(&mut self, view: usize, handle: Option<u32>) {
        if self.hovered_handle == handle {
            return;
        }
        self.hovered_handle = handle;
        match handle {
            Some(handle) => info!(
                target: telemetry_target::ENGINE,
                view,
                handle,
                "[editor] gizmo hover handle"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                view,
                "[editor] gizmo hover none"
            ),
        }
    }

    /// Publish every view's camera binding whose camera moved: the renderer
    /// reads the binding, so a navigation shows on the next frame.
    fn publish_cameras(&mut self) {
        for view in 0..self.views.len() {
            if !self.camera_dirty[view] {
                continue;
            }
            let Some(surface_id) = self.views[view].as_ref().map(|scene| scene.surface_id) else {
                continue;
            };
            let binding = camera_binding(&self.cameras[view]);
            self.host.set_surface_camera(surface_id, Some(binding));
            self.camera_dirty[view] = false;
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
                    let delta = self.synthetic_gizmo_pending.take();
                    // A `gizmo press` holds the drag open for the following
                    // `gizmo move`/`gizmo release`; a `gizmo drag` carries
                    // its own delta and finishes in this answer.
                    if delta.is_some() || self.synthetic_gizmo_hold {
                        let view = self.synthetic_gizmo_view.take().unwrap_or(0);
                        info!(
                            target: telemetry_target::ENGINE,
                            handle,
                            "[editor] scene pick handle"
                        );
                        self.synthetic_gizmo_press =
                            Some((result.request.x as f32, result.request.y as f32));
                        handle_press = Some((
                            view,
                            handle,
                            result.request.x as f32,
                            result.request.y as f32,
                            delta,
                        ));
                    }
                } else if self.synthetic_press_ids.is_empty()
                    && (self.synthetic_gizmo_pending.is_some() || self.synthetic_gizmo_hold)
                {
                    self.synthetic_gizmo_pending = None;
                    self.synthetic_gizmo_hold = false;
                    self.synthetic_gizmo_view = None;
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the press point hit no handle"
                    );
                }
                continue;
            }
            let Some(press) = self
                .press_pick
                .as_ref()
                .filter(|press| press.id == result.request.id)
                .map(|press| (press.view, press.x, press.y))
            else {
                continue;
            };
            let (view, press_x, press_y) = press;
            self.press_pick = None;
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
                    // The drag begins while the button is still held: the
                    // press asked the pick, and the answer starts the
                    // manipulation, not the release. A click that began and
                    // ended before the answer arrived (the scripted channel's
                    // one-command click) manipulates nothing.
                    if self.left_down[view] {
                        handle_press = Some((view, handle, press_x, press_y, None));
                    } else {
                        info!(
                            target: telemetry_target::ENGINE,
                            handle,
                            "[editor] gizmo drag skipped: the press was released"
                        );
                    }
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

    /// Draw the gizmo handles for the selection (Stage 15, extended in
    /// Stage 28 with the hover/drag states, the translation planes, the
    /// dynamic scale handles and the drag guides).
    ///
    /// The contract's own primitives with handle ids set: the same frame the
    /// renderer draws is the frame it picks, so a handle a drag grabs is
    /// always the handle that was on screen. Handles keep a constant screen
    /// size - their world length follows the active view's camera distance -
    /// and the frame they are drawn into is one frame's worth: nothing to
    /// clear here. The geometry is kept for the hover test, so what the
    /// pointer sees and what the highlight names cannot disagree.
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
                    self.gizmo_geometry = GizmoGeometry::default();
                    self.gizmo_reference = 0.0;
                    self.hovered_handle = None;
                    return;
                }
            }
        };
        let origin = transform.translation;
        let basis = gizmo_basis(settings.space, &transform);
        let view = self.active_view;
        let length = self.gizmo_length(view, origin);
        let states = HandleStates {
            hovered: self.hovered_handle,
            active: self.gizmo_drag.as_ref().map(|drag| drag.handle),
        };
        // A scale drag grows or shrinks the dragged axis' handle with the
        // factor it is writing: the handle's length is derived from the
        // press's scale every frame, never from the previous frame's, so it
        // cannot drift.
        let lengths = match self.gizmo_drag.as_ref() {
            Some(drag) => match drag.kind {
                HandleKind::Axis {
                    mode: GizmoMode::Scale,
                    axis,
                } => gizmo::scale_handle_lengths(
                    drag.reference,
                    drag.original.scale,
                    transform.scale,
                    axis,
                ),
                _ => [length; 3],
            },
            None => [length; 3],
        };
        let geometry = gizmo::build_geometry_styled(settings.mode, origin, basis, lengths, states);
        let anchor = self.project_to_screen(origin);
        let guides = self.gizmo_drag.as_ref().map(|drag| {
            (
                drag.kind,
                drag.basis,
                drag.start_pivot,
                drag.current_pivot,
                drag.reference,
            )
        });
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
            if let Some((kind, basis, start_pivot, current_pivot, reference)) = guides {
                draw_gizmo_guides(debug, kind, basis, start_pivot, current_pivot, reference);
            }
        }
        let guide_state = self.gizmo_drag.as_ref().map(|drag| drag.kind);
        let dotted = guide_state.is_some_and(|kind| gizmo_drag_mode(kind) == GizmoMode::Translate);
        let axis_guide = guide_state.and_then(HandleKind::axis).map(|(_, axis)| axis);
        if self.gizmo_guides != Some(guide_state.is_some()) {
            self.gizmo_guides = Some(guide_state.is_some());
            match axis_guide {
                Some(axis) => info!(
                    target: telemetry_target::ENGINE,
                    axis = gizmo_axis_name(axis),
                    dotted,
                    "[editor] gizmo guides"
                ),
                None => info!(
                    target: telemetry_target::ENGINE,
                    guides = guide_state.is_some(),
                    dotted,
                    "[editor] gizmo guides"
                ),
            }
        }
        self.gizmo_geometry = geometry;
        self.gizmo_reference = length;
        // The anchor line, on a change or on the periodic cadence: what a
        // developer (and the suite) reads to know where the handles are.
        if let Some((x, y)) = anchor {
            let header = (settings.mode, settings.space, x as i32, y as i32);
            if self.gizmo_logged != Some(header)
                || self.frame_index.is_multiple_of(GIZMO_HEARTBEAT_FRAMES)
            {
                self.gizmo_logged = Some(header);
                let geometry = &self.gizmo_geometry;
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
    /// preset's bindings, the same ones the native editor defaults to, and
    /// the same mapping a scripted `key` chord runs.
    fn gizmo_shortcut(&mut self, event: &winit::event::KeyEvent) {
        if event.state != winit::event::ElementState::Pressed || event.repeat {
            return;
        }
        let winit::keyboard::PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let Some(key) = pill_input::winit_events::key_code(code) else {
            return;
        };
        if let Some(mode) = gizmo_mode_for_key(key) {
            self.set_gizmo_tool(mode);
        }
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

    /// How long the handles are, in world units, at `origin` in `view`: a
    /// constant fraction of that view's camera distance, so the gizmo
    /// neither shrinks away nor swallows the scene.
    fn gizmo_length(&self, view: usize, origin: [f32; 3]) -> f32 {
        (gizmo::distance(self.cameras[view].eye(), origin) * GIZMO_SCREEN_FRACTION)
            .clamp(0.05, 50.0)
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
        let camera = &self.cameras[view];
        let forward = camera.forward();
        let right = camera.right();
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
        gizmo::normalize(direction).map(|direction| (camera.eye(), direction))
    }

    /// Where a world point lands on the active scene view's surface, in
    /// physical pixels.
    fn project_to_screen(&self, point: [f32; 3]) -> Option<(f32, f32)> {
        self.project_to_screen_in(self.active_view, point)
    }

    /// Where a world point lands on one scene view's surface, in physical
    /// pixels.
    fn project_to_screen_in(&self, view: usize, point: [f32; 3]) -> Option<(f32, f32)> {
        let (width, height) = self.scene_pixel_size(view)?;
        let (tan, aspect) = projection_terms(width, height);
        let camera = &self.cameras[view];
        let eye = camera.eye();
        let forward = camera.forward();
        let right = camera.right();
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
        let Some(kind) = gizmo::handle_kind(handle) else {
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
        let (start_value, start_plane) = match kind {
            HandleKind::Axis {
                mode: GizmoMode::Rotate,
                axis,
            } => {
                let Some(point) =
                    gizmo::ray_plane_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the ring"
                    );
                    return;
                };
                (gizmo::plane_angle(origin, basis[axis], point), None)
            }
            HandleKind::Axis { axis, .. } => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the axis"
                    );
                    return;
                };
                (value, None)
            }
            HandleKind::Plane { first, second } => {
                // The plane is the one the pair spans: its normal is the
                // remaining axis, whichever space the basis is in.
                let normal = basis[3 - first - second];
                let Some(point) = gizmo::ray_plane_point(origin, normal, ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the plane"
                    );
                    return;
                };
                let (along_first, along_second) =
                    gizmo::plane_coordinates(origin, basis[first], basis[second], point);
                (0.0, Some((along_first, along_second)))
            }
        };
        let reference = self.gizmo_length(view, origin);
        self.gizmo_drag = Some(GizmoDrag {
            view,
            handle,
            kind,
            entity,
            original,
            basis,
            start_value,
            start_plane,
            start_pivot: origin,
            current_pivot: origin,
            reference,
            last_value: None,
        });
        // The grabbed handle is the active one from here on, so it draws
        // dimmed for as long as the drag runs.
        self.set_hovered_handle(view, Some(handle));
        match kind.plane() {
            Some((first, second)) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(GizmoMode::Translate),
                plane = gizmo_plane_name(first, second),
                entity = %entity,
                handle,
                "[editor] gizmo drag started"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(gizmo::handle_axis(handle).map(|(mode, _)| mode).unwrap_or(GizmoMode::Translate)),
                axis = gizmo_axis_name(kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                entity = %entity,
                handle,
                "[editor] gizmo drag started"
            ),
        }
    }

    /// Apply one move of the drag in flight: re-read the ray, take the delta
    /// against the press, snap it when the tool snaps, and write the values
    /// straight into the world. No undo entry yet - the egui editor has no
    /// command queue wired (W-0027).
    ///
    /// Every write is computed from the press's transform, never from the
    /// previous frame's, so a snapped or clamped drag cannot accumulate
    /// rounding - and the drag's basis is the one the handles were built in,
    /// so a local-space drag keeps following the object's own axes.
    fn update_gizmo_drag(&mut self, cursor_x: f32, cursor_y: f32) {
        let Some(drag) = self.gizmo_drag.as_ref() else {
            return;
        };
        let (view, kind, entity) = (drag.view, drag.kind, drag.entity);
        let (original, basis) = (drag.original, drag.basis);
        let (start_value, start_plane) = (drag.start_value, drag.start_plane);
        let reference = drag.reference;
        let Some((ray_origin, ray_direction)) = self.scene_ray(view, cursor_x, cursor_y) else {
            return;
        };
        let settings = self.gizmo_settings();
        let origin = original.translation;
        let (written, pivot) = match kind {
            HandleKind::Axis {
                mode: GizmoMode::Rotate,
                axis,
            } => {
                let Some(point) =
                    gizmo::ray_plane_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                let delta =
                    gizmo::wrap_angle(gizmo::plane_angle(origin, basis[axis], point) - start_value);
                let degrees = if settings.snap {
                    gizmo::snap_value(delta.to_degrees(), settings.rotate_step_degrees)
                } else {
                    delta.to_degrees()
                };
                // The axis is the basis' own, so a local-mode drag turns
                // around the object's axis and a world-mode drag around the
                // world's.
                let turn = gizmo::axis_angle_quaternion(basis[axis], degrees.to_radians());
                let combined = quaternion_product(turn, original.rotation);
                let written = self.write_rotation(entity, combined);
                (written.then_some(combined), origin)
            }
            HandleKind::Axis {
                mode: GizmoMode::Translate,
                axis,
            } => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                let mut delta = value - start_value;
                if settings.snap {
                    delta = gizmo::snap_value(delta, settings.translate_step);
                }
                // The displacement is along the handle's axis, not a world
                // component: in local space the two differ as soon as the
                // object is rotated.
                let translation =
                    gizmo::add(original.translation, gizmo::scale(basis[axis], delta));
                let written = self.write_translation(entity, translation);
                (
                    written.then_some([translation[0], translation[1], translation[2], 1.0]),
                    translation,
                )
            }
            HandleKind::Axis {
                mode: GizmoMode::Scale,
                axis,
            } => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                let factor = 1.0 + (value - start_value) / reference.max(1e-4);
                let mut scaled = original.scale[axis] * factor;
                if settings.snap {
                    scaled = gizmo::snap_value(scaled, settings.scale_step);
                }
                // A zero or negative scale is a degenerate mesh; the drag
                // stops at the smallest positive value instead.
                let scaled = scaled.max(0.01);
                let mut values = original.scale;
                values[axis] = scaled;
                let written = self.write_scale(entity, values);
                (written.then_some([scaled, 0.0, 0.0, 0.0]), origin)
            }
            HandleKind::Plane { first, second } => {
                let Some((start_first, start_second)) = start_plane else {
                    return;
                };
                let normal = basis[3 - first - second];
                let Some(point) = gizmo::ray_plane_point(origin, normal, ray_origin, ray_direction)
                else {
                    return;
                };
                let (along_first, along_second) =
                    gizmo::plane_coordinates(origin, basis[first], basis[second], point);
                let (mut delta_first, mut delta_second) =
                    (along_first - start_first, along_second - start_second);
                if settings.snap {
                    delta_first = gizmo::snap_value(delta_first, settings.translate_step);
                    delta_second = gizmo::snap_value(delta_second, settings.translate_step);
                }
                let translation = gizmo::add(
                    gizmo::add(
                        original.translation,
                        gizmo::scale(basis[first], delta_first),
                    ),
                    gizmo::scale(basis[second], delta_second),
                );
                let written = self.write_translation(entity, translation);
                (
                    written.then_some([delta_first, delta_second, 0.0, 0.0]),
                    translation,
                )
            }
        };
        if let Some(drag) = self.gizmo_drag.as_mut() {
            if let Some(written) = written {
                drag.last_value = Some(written);
            }
            drag.current_pivot = pivot;
        }
    }

    /// Write a translation straight into the world; false when a component
    /// refused the write.
    fn write_translation(&mut self, entity: Entity, translation: [f32; 3]) -> bool {
        self.write_transform_field(entity, "translation", &translation)
    }

    /// Write a rotation straight into the world.
    fn write_rotation(&mut self, entity: Entity, rotation: [f32; 4]) -> bool {
        self.write_transform_field(entity, "rotation", &rotation)
    }

    /// Write a scale straight into the world.
    fn write_scale(&mut self, entity: Entity, scale: [f32; 3]) -> bool {
        self.write_transform_field(entity, "scale", &scale)
    }

    /// Write one transform field's components, in order; false when any of
    /// them was refused.
    fn write_transform_field(&mut self, entity: Entity, field: &str, values: &[f32]) -> bool {
        let world = self.host.host_mut().engine_mut().world_mut();
        let mut written = true;
        for (index, value) in values.iter().enumerate() {
            written &= world
                .write_component_array_element(
                    entity,
                    TRANSFORM_SHARED_NAME,
                    field,
                    index,
                    FieldValue::F32(*value),
                )
                .is_ok();
        }
        written
    }

    /// Write the press's whole transform back: a cancelled drag leaves the
    /// object exactly where it started.
    fn restore_transform(&mut self, entity: Entity, transform: &TransformComponent) {
        let _ = self.write_translation(entity, transform.translation);
        let _ = self.write_rotation(entity, transform.rotation);
        let _ = self.write_scale(entity, transform.scale);
    }

    /// End the drag in flight, reporting what it wrote. The guides and the
    /// active highlight go with it: the handles return to their idle (or
    /// hovered) look on the next frame.
    fn end_gizmo_drag(&mut self) {
        let Some(drag) = self.gizmo_drag.take() else {
            return;
        };
        let settings = self.gizmo_settings();
        let mode = gizmo_drag_mode(drag.kind);
        match (drag.kind.plane(), drag.last_value) {
            (Some((first, second)), Some(value)) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                plane = gizmo_plane_name(first, second),
                entity = %drag.entity,
                value = ?value,
                snapped = settings.snap,
                "[editor] gizmo drag ended"
            ),
            (Some((first, second)), None) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                plane = gizmo_plane_name(first, second),
                entity = %drag.entity,
                "[editor] gizmo drag ended without a move"
            ),
            (None, Some(value)) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                axis = gizmo_axis_name(drag.kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                entity = %drag.entity,
                value = ?value,
                snapped = settings.snap,
                "[editor] gizmo drag ended"
            ),
            (None, None) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                axis = gizmo_axis_name(drag.kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                entity = %drag.entity,
                "[editor] gizmo drag ended without a move"
            ),
        }
        self.set_hovered_handle(drag.view, None);
    }

    /// Cancel the drag in flight: the press's transform is written back and
    /// the drag dropped, so a focus loss or a viewport change leaves no
    /// half-applied manipulation (or its guides) behind.
    fn cancel_gizmo_drag(&mut self) {
        let Some(drag) = self.gizmo_drag.take() else {
            return;
        };
        self.restore_transform(drag.entity, &drag.original);
        info!(
            target: telemetry_target::ENGINE,
            mode = gizmo_mode_name(gizmo_drag_mode(drag.kind)),
            entity = %drag.entity,
            "[editor] gizmo drag canceled"
        );
        self.set_hovered_handle(drag.view, None);
    }

    /// Drop every interaction in flight: the focus was lost, so no press or
    /// release is coming to finish them.
    fn cancel_interactions(&mut self, reason: &str) {
        if self.camera_drag.take().is_some() {
            info!(
                target: telemetry_target::ENGINE,
                reason,
                "[editor] camera navigation canceled"
            );
        }
        self.cancel_gizmo_drag();
        self.press_pick = None;
        self.left_down = [false; SCENE_VIEWPORTS];
        self.set_hovered_handle(self.active_view, None);
    }

    /// Resolve a scripted handle name to its id: `x`/`y`/`z` name the
    /// active tool's axes, `xy`/`xz`/`yz` the translation planes.
    fn scripted_gizmo_handle(&mut self, name: &str) -> Option<u32> {
        let settings = self.gizmo_settings();
        if let Some(axis) = parse_gizmo_axis(name) {
            return Some(gizmo::axis_handle(settings.mode, axis));
        }
        let (first, second) = match name {
            "xy" => (0, 1),
            "xz" => (0, 2),
            "yz" => (1, 2),
            _ => return None,
        };
        if settings.mode != GizmoMode::Translate {
            warn!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(settings.mode),
                "[editor] plane handles belong to the translate tool"
            );
            return None;
        }
        Some(gizmo::plane_handle(first, second))
    }

    /// Apply a scripted camera gesture to the active viewport, through the
    /// same math a drag uses.
    fn scripted_camera(&mut self, navigation: CameraNavigation, delta_x: f32, delta_y: f32) {
        let view = self.active_view;
        self.apply_camera_gesture(view, navigation, delta_x, delta_y);
        info!(
            target: telemetry_target::ENGINE,
            view,
            navigation = camera_navigation_name(navigation),
            "[editor] camera gesture applied"
        );
    }

    /// Move one view's pointer and update the hovered handle - the scripted
    /// channel's stand-in for a real pointer move.
    fn scripted_hover(&mut self, view: usize, x: f32, y: f32) {
        if view >= self.views.len() {
            warn!(
                target: telemetry_target::ENGINE,
                view,
                "[editor] hover needs an existing scene view"
            );
            return;
        }
        self.scene_cursor[view] = Some((x, y));
        self.update_hover(view, (x, y));
        info!(
            target: telemetry_target::ENGINE,
            view,
            x,
            y,
            "[editor] hover injected"
        );
    }

    /// Press on one handle of the active tool (the scripted channel): the
    /// press pixel is the handle's grab point projected, so the pick that
    /// answers it proves the same geometry the renderer draws.
    fn start_synthetic_gizmo_drag(&mut self, handle: u32, delta: Option<(f32, f32)>) {
        let Some(kind) = gizmo::handle_kind(handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle,
                "[editor] gizmo drag refused: unknown handle"
            );
            return;
        };
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
        let length = self.gizmo_length(view, transform.translation);
        // Each handle is grabbed where its own geometry carries it: a
        // rotation ring on its radius at the diagonal between two axes (no
        // other ring passes a pixel away), a plane square at its centre,
        // and a straight handle just short of its tip.
        let grab = match kind {
            HandleKind::Axis { axis, .. } => match gizmo_drag_mode(kind) {
                GizmoMode::Rotate => gizmo::add(
                    transform.translation,
                    gizmo::scale(
                        gizmo::normalize(gizmo::add(basis[(axis + 1) % 3], basis[(axis + 2) % 3]))
                            .unwrap_or(basis[axis]),
                        length,
                    ),
                ),
                _ => gizmo::add(
                    transform.translation,
                    gizmo::scale(basis[axis], length * GIZMO_PRESS_FRACTION),
                ),
            },
            HandleKind::Plane { first, second } => gizmo::add(
                transform.translation,
                gizmo::add(
                    gizmo::scale(basis[first], length * gizmo::PLANE_OFFSET),
                    gizmo::scale(basis[second], length * gizmo::PLANE_OFFSET),
                ),
            ),
        };
        let Some((x, y)) = self.project_to_screen_in(view, grab) else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: the handle is behind the camera"
            );
            return;
        };
        self.synthetic_gizmo_pending = delta;
        self.synthetic_gizmo_hold = delta.is_none();
        self.synthetic_gizmo_press = None;
        self.synthetic_gizmo_view = Some(view);
        match kind.plane() {
            Some((first, second)) => info!(
                target: telemetry_target::ENGINE,
                plane = gizmo_plane_name(first, second),
                x,
                y,
                "[editor] gizmo drag press"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                axis = gizmo_axis_name(kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                x,
                y,
                "[editor] gizmo drag press"
            ),
        }
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

    /// Finish the drag a `gizmo press` started, like a mouse release.
    fn scripted_gizmo_release(&mut self) {
        if self.gizmo_drag.is_none() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo release needs a drag in flight"
            );
            return;
        }
        self.synthetic_gizmo_press = None;
        self.end_gizmo_drag();
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

    /// Log the active viewport, the interaction state, the hovered handle
    /// and every view's camera: the suite's window into navigation and the
    /// viewport routing, with no pixel clicks.
    fn dump_viewport(&mut self) {
        let hovered = self
            .hovered_handle
            .map(|handle| handle.to_string())
            .unwrap_or_else(|| "-".to_string());
        info!(
            target: telemetry_target::ENGINE,
            active = self.active_view,
            interaction = self.interaction_name(),
            hovered = hovered.as_str(),
            keymap = %self.keymap,
            "[test] viewport"
        );
        for view in 0..self.views.len() {
            let camera = self.cameras[view];
            let eye = camera.eye();
            let pivot = camera.pivot;
            info!(
                target: telemetry_target::ENGINE,
                view,
                eye = format!("{:.3},{:.3},{:.3}", eye[0], eye[1], eye[2]).as_str(),
                pivot = format!("{:.3},{:.3},{:.3}", pivot[0], pivot[1], pivot[2]).as_str(),
                distance = camera.distance,
                yaw = camera.yaw,
                pitch = camera.pitch,
                "[test] viewport camera"
            );
        }
    }

    /// The viewport interaction state's name: one owner at a time, so the
    /// dump and the logs say who has the pointer.
    fn interaction_name(&self) -> &'static str {
        if self.camera_drag.is_some() {
            return "navigating";
        }
        match self.gizmo_drag.as_ref().map(|drag| drag.kind) {
            Some(HandleKind::Plane { .. }) => "dragging-plane",
            Some(HandleKind::Axis { .. }) => "dragging-axis",
            None if self.hovered_handle.is_some() => "hovering",
            None => "idle",
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

    /// Log the Assets tab's open folder and the rows its grid lists there -
    /// the suite's window into the tree and grid navigation, which needs no
    /// pixel clicks.
    fn dump_browser(&mut self) {
        let folder = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource::<EguiShellState>() {
                Some(state) => state.browser.folder.clone(),
                None => String::new(),
            }
        };
        let entries = pill_editor_core_egui::browser::children(&self.asset_entries, &folder);
        info!(
            target: telemetry_target::ENGINE,
            folder = folder.as_str(),
            count = entries.len(),
            "[test] browser"
        );
        for row in entries {
            info!(
                target: telemetry_target::ENGINE,
                path = row.path.as_str(),
                kind = row.kind.as_str(),
                "[test] browser item"
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

    /// Open a folder in the Assets tab's browser (`root` opens the asset
    /// root), through the shell's session state - the scripted channel's
    /// stand-in for clicking the folder tree.
    fn scripted_browser(&mut self, folder: &str) {
        let folder = if folder == "root" { "" } else { folder };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] browser navigation needs the shell module's state"
            );
            return;
        };
        if !state.browser.open(folder) {
            info!(
                target: telemetry_target::ENGINE,
                folder,
                "[editor] browser folder unchanged"
            );
        }
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
    fn scripted_toolbar(&mut self, action: ToolbarAction) {
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
        let selection = EditorSelection {
            asset: selected,
            ..EditorSelection::default()
        };
        match action {
            ToolbarAction::Create => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                    assets::open_create(&mut state.asset_dialog, &selection, &rows, &types);
                }
            }
            ToolbarAction::Rename => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                    assets::open_rename(&mut state.asset_dialog, &selection, &rows);
                }
            }
            ToolbarAction::Delete => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    assets::delete_selection(pending, &selection, &rows);
                }
            }
            ToolbarAction::Meta => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    assets::write_meta_selection(pending, &selection, &rows);
                }
            }
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

    /// One `edit asset` command, queued as the recorded edit the Assets tab's
    /// actions also queue.
    fn scripted_asset_edit(&mut self, edit: AssetEdit) {
        let (command_value, label) = match edit {
            AssetEdit::Create {
                type_name,
                folder,
                name,
            } => (
                EditorCommand::CreateStandaloneAsset {
                    folder: if folder == "-" { String::new() } else { folder },
                    name: name.clone(),
                    type_name,
                },
                format!("create asset {name}"),
            ),
            AssetEdit::Delete { path } => (
                EditorCommand::DeleteAssetFile { path: path.clone() },
                format!("delete asset {path}"),
            ),
            AssetEdit::Move { from, to } => (
                EditorCommand::MoveAssetFile {
                    from: from.clone(),
                    to,
                },
                format!("move asset {from}"),
            ),
            AssetEdit::Meta { path } => (
                EditorCommand::WriteAssetMeta { path: path.clone() },
                format!("write meta {path}"),
            ),
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
    fn scripted_edit_set(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
        value_text: &str,
    ) {
        let Some((entity, component, current)) =
            self.resolve_edit_target(entity_text, component_text, field)
        else {
            return;
        };
        let value = match inspector::parse_field_value(value_text, &current) {
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
    fn scripted_edit_array(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
        index: usize,
        value_text: &str,
    ) {
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
        let value = match inspector::parse_field_value(value_text, &element) {
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
    fn scripted_asset_settings(&mut self, path: &str, key: &str, value_text: &str) {
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
        let value = match inspector::parse_settings_value(value_text, current) {
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
        if self.frame_index.is_multiple_of(210)
            || self
                .last_heartbeat
                .is_none_or(|at| now.duration_since(at).as_secs_f32() >= 2.0)
        {
            self.last_heartbeat = Some(now);
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
        // Step 2a: any camera a navigation moved is re-published, so the
        // renderer draws the frame with the view the user just steered.
        self.publish_cameras();
        // Step 2b: a scripted drag in flight takes its next stage - the
        // press, the move and the release, one per frame.
        self.advance_scripted_drag();
        // Step 2c: what the host did since the last frame - a reload, a patch,
        // a failure, a rollback - becomes the notices the shell draws toasts
        // and Problems rows from (W-0038 Stages 4 and 6).
        self.fold_host_notices();
        // Step 3: the shell boundary's input resource.
        if let Some(input) = self
            .host
            .host_mut()
            .engine_mut()
            .world_mut()
            .get_resource_mut::<EguiShellInput>()
        {
            // The modifier state opens the frame's stream, so the bridge
            // starts from what is held rather than from nothing.
            let mut events = Vec::with_capacity(self.shell_events.len() + 1);
            events.push(InputEvent::ModifiersChanged {
                modifiers: EngineModifiers {
                    shift: self.modifiers.shift,
                    control: self.modifiers.control,
                    alt: self.modifiers.alt,
                    super_key: false,
                },
            });
            events.append(&mut self.shell_events);
            *input = EguiShellInput {
                screen_size: pill_core::math::Vector2f::new(
                    self.size.0 as f32 / self.scale_factor,
                    self.size.1 as f32 / self.scale_factor,
                ),
                scale_factor: self.scale_factor,
                focused: self.focused,
                time: self.started.elapsed().as_secs_f64(),
                frames_per_second: self.fps,
                events,
                cursor: self.shell_cursor,
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
        // Step 7b: the menu bar's requests - undo/redo, a window switch and a
        // quit - taken through the same paths the scripted channel uses.
        self.drain_menu_requests();
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
        let (undo_depth, redo_depth) = engine
            .world()
            .get_resource::<CommandHistory>()
            .map(|history| (history.undo_depth(), history.redo_depth()))
            .unwrap_or((0, 0));
        snapshot.undo_depth = undo_depth;
        snapshot.redo_depth = redo_depth;
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

    /// Apply the batches the shell's assets actions and the scripted channel
    /// queued, then publish what the frame needs to follow them.
    ///
    /// The application itself is the shared core drain
    /// ([`pill_editor_core_egui::drain::apply_pending`]), so the browser
    /// editor runs exactly this contract; the only executable-owned half is
    /// the asset listing walk a file batch asks for.
    fn apply_pending_commands(&mut self) {
        let now_millis = self.started.elapsed().as_millis() as u64;
        let outcome = drain::apply_pending(self.host.host_mut().engine_mut(), now_millis);
        if outcome.files_changed {
            self.asset_listing_dirty = true;
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
        // The keymap preset the file names drives the viewport navigation
        // chords; Blender's table stands until the file says otherwise.
        let keymap = preferences::keymap_preset(&self.preferences_document);
        if keymap != self.keymap {
            self.keymap = keymap;
            info!(
                target: telemetry_target::ENGINE,
                preset = %keymap,
                "[editor] camera keymap"
            );
        }
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

    /// Run `editor.undo` from the scripted channel: the shared core walk pops
    /// the newest step and queues its inverse - applied, never recorded, the
    /// same walk the browser editor and the native editor take.
    fn scripted_undo(&mut self) {
        drain::step_history(self.host.host_mut().engine_mut(), HistoryDirection::Undo);
    }

    /// Run `editor.redo` from the scripted channel: the same walk, queueing
    /// the next step's forward batch.
    fn scripted_redo(&mut self) {
        drain::step_history(self.host.host_mut().engine_mut(), HistoryDirection::Redo);
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
    fn scripted_edit_system(&mut self, index: usize, enabled: bool) {
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
    fn scripted_edit_preference(&mut self, key: &str, value: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::SetPreferences {
                values: vec![(key.to_string(), value.to_string())],
            }]);
        }
        info!(
            target: telemetry_target::ENGINE,
            key,
            value,
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
    fn scripted_component_edit(&mut self, entity_text: &str, component_text: &str, add: bool) {
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
            // The menu bar's quit is drained at the frame boundary (Step 7b),
            // not while the frame is presented.
            quit_requested: _,
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
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] composition overlay still unavailable"
                );
            }
        }
    }

    /// Refuses one scripted line, naming it and why it cannot run.
    ///
    /// Every refusal - an unknown verb, an unknown target, a missing or
    /// malformed argument - prints this one token, so a suite can tell a
    /// rejected line from a slow answer without pinning twenty messages, and
    /// `EditorSession` raises on it the moment it lands instead of timing out.
    fn reject_scripted(&self, command: &str, reason: &str) {
        warn!(
            target: telemetry_target::ENGINE,
            command,
            reason,
            "[editor] scripted command rejected"
        );
    }

    /// Run the scripted commands appended since the last frame.
    ///
    /// The grammar lives in [`crate::scripted`]: this parses each line and
    /// dispatches it, and a line that cannot be parsed takes the one refusal
    /// path, so a suite sees it at once instead of waiting out a timeout.
    fn poll_test_channel(&mut self) {
        let Some(channel) = &mut self.test_channel else {
            return;
        };
        for line in channel.poll() {
            match ScriptedCommand::parse(&line) {
                Ok(command) => self.run_scripted(&line, command),
                Err(error) => self.reject_scripted(&line, &error.reason),
            }
        }
    }

    /// Run one parsed scripted command.
    ///
    /// No grammar is left here: an arm names its side effect, so the verbs,
    /// their arguments and their refusal sentences have exactly one home.
    fn run_scripted(&mut self, line: &str, command: ScriptedCommand) {
        match command {
            ScriptedCommand::Quit => {
                info!(target: telemetry_target::ENGINE, "[editor] quit requested");
                self.quit = true;
            }
            ScriptedCommand::Undock(tab) => self.undock_tab(tab),
            ScriptedCommand::ActivateTab(tab) => self.activate_tab(tab),
            ScriptedCommand::Click { x, y } => self.inject_click(x, y),
            ScriptedCommand::RightClick { x, y } => self.inject_rclick(x, y),
            ScriptedCommand::Drag { from, to } => self.start_scripted_drag(from, to),
            ScriptedCommand::GizmoDrag { target, delta } => {
                match self.scripted_gizmo_handle(&target) {
                    Some(handle) => self.start_synthetic_gizmo_drag(handle, Some(delta)),
                    None => self.reject_scripted(line, "gizmo drag needs a known axis or plane"),
                }
            }
            ScriptedCommand::GizmoPress { target } => match self.scripted_gizmo_handle(&target) {
                Some(handle) => self.start_synthetic_gizmo_drag(handle, None),
                None => self.reject_scripted(line, "gizmo press needs a known axis or plane"),
            },
            ScriptedCommand::GizmoRelease => self.scripted_gizmo_release(),
            ScriptedCommand::GizmoTool(mode) => self.set_gizmo_tool(mode),
            ScriptedCommand::Key(chord) => self.inject_key(&chord),
            ScriptedCommand::Dump(dump) => match dump {
                Dump::Tabs => self.dump_tabs(),
                Dump::Scenes => self.dump_scenes(),
                Dump::Viewport => self.dump_viewport(),
                Dump::Grid => self.dump_grid(),
                Dump::Assets => self.dump_assets(),
                Dump::Browser => self.dump_browser(),
                Dump::StandaloneTypes => self.dump_standalone_types(),
                Dump::Systems => self.dump_systems(),
                Dump::Preferences => self.dump_preferences(),
                Dump::Field {
                    entity,
                    component,
                    field,
                } => self.dump_field(&entity, &component, &field),
            },
            ScriptedCommand::Browser(folder) => self.scripted_browser(&folder),
            ScriptedCommand::Camera { navigation, delta } => {
                self.scripted_camera(navigation, delta.0, delta.1)
            }
            ScriptedCommand::CameraZoom(lines) => self.zoom_camera(self.active_view, lines),
            ScriptedCommand::Hover { view, x, y } => self.scripted_hover(view, x, y),
            ScriptedCommand::SelectAsset(path) => self.scripted_select_asset(&path),
            ScriptedCommand::SelectEntity(handle) => self.scripted_select_entity(&handle),
            ScriptedCommand::Toolbar(action) => self.scripted_toolbar(action),
            ScriptedCommand::DialogSetName(text) => self.scripted_dialog_set_name(&text),
            ScriptedCommand::DialogSubmit => self.scripted_dialog_submit(),
            ScriptedCommand::EditAsset(edit) => self.scripted_asset_edit(edit),
            ScriptedCommand::EditSet {
                entity,
                component,
                field,
                value,
            } => self.scripted_edit_set(&entity, &component, &field, &value),
            ScriptedCommand::EditArray {
                entity,
                component,
                field,
                index,
                value,
            } => self.scripted_edit_array(&entity, &component, &field, index, &value),
            ScriptedCommand::EditAssetSettings { path, key, value } => {
                self.scripted_asset_settings(&path, &key, &value)
            }
            ScriptedCommand::EditSystem { index, enabled } => {
                self.scripted_edit_system(index, enabled)
            }
            ScriptedCommand::EditPreference { key, value } => {
                self.scripted_edit_preference(&key, &value)
            }
            ScriptedCommand::EditComponent {
                entity,
                component,
                add,
            } => self.scripted_component_edit(&entity, &component, add),
            ScriptedCommand::Undo => self.scripted_undo(),
            ScriptedCommand::Redo => self.scripted_redo(),
            ScriptedCommand::PauseToggle => self.scripted_pause_toggle(),
            ScriptedCommand::Step => self.scripted_step(),
            ScriptedCommand::EntityCreate => self.scripted_entity_create(),
            ScriptedCommand::EntityDelete => self.scripted_entity_delete(),
            ScriptedCommand::SnapToggle => {
                let snap = self.gizmo_settings().snap;
                self.scripted_gizmo_tool(None, None, Some(!snap))
            }
            ScriptedCommand::SpaceToggle => {
                let space = self.gizmo_settings().space;
                let next = match space {
                    GizmoSpace::World => GizmoSpace::Local,
                    GizmoSpace::Local => GizmoSpace::World,
                };
                self.scripted_gizmo_tool(None, Some(next), None)
            }
        }
    }

    /// Inject a keyboard chord through the same engine input seam a real key
    /// enters: the modifiers, the press, the typed text and the release, in
    /// the order a window reports them.
    ///
    /// The editor's own tool shortcuts read winit events rather than the
    /// engine's input stream, so the chord also runs the handler a real press
    /// runs - without that, `key w` would reach the shell but never switch
    /// the tool.
    fn inject_key(&mut self, chord: &KeyChord) {
        let held = EngineModifiers {
            shift: chord.shift,
            control: chord.control,
            alt: chord.alt,
            super_key: chord.super_key,
        };
        self.queue_input(InputEvent::ModifiersChanged { modifiers: held });
        self.queue_input(InputEvent::Key {
            key: chord.key,
            state: ButtonState::Pressed,
        });
        if let Some(character) = chord.typed_character() {
            self.queue_input(InputEvent::TextInput {
                text: character.to_string(),
            });
        }
        self.queue_input(InputEvent::Key {
            key: chord.key,
            state: ButtonState::Released,
        });
        // The chord releases its modifiers as it ends: a later injection must
        // not inherit a held Control from this one.
        self.queue_input(InputEvent::ModifiersChanged {
            modifiers: EngineModifiers {
                shift: false,
                control: false,
                alt: false,
                super_key: false,
            },
        });
        info!(
            target: telemetry_target::ENGINE,
            chord = chord.label().as_str(),
            "[editor] key injected"
        );
        if let Some(mode) = gizmo_mode_for_key(chord.key) {
            self.set_gizmo_tool(mode);
        }
    }

    /// Queue one scripted event: to the engine, and to the shell's list too.
    ///
    /// A scripted click or key stands in for a user's, so the UI must see it
    /// the way a real event arrives - through the shell's own list - while
    /// the game still receives what a user's would.
    fn queue_input(&mut self, event: InputEvent) {
        if let InputEvent::CursorMoved { position } = event {
            // The scripted pointer is the shell's pointer too: the seed the
            // next frame carries has to match what the shell just saw.
            self.shell_cursor = Some(position);
        }
        self.host
            .host_mut()
            .engine_mut()
            .push_input_event(event.clone());
        self.shell_events.push(event);
    }

    /// Fold the host's queued notices into the editor's notice resource.
    ///
    /// Every notice becomes a toast line; a failure also becomes Problems rows
    /// for its module, and a clean reload clears the rows its module left
    /// behind. The resource is rewritten only when something arrived, so the
    /// shell's read stays cheap.
    fn fold_host_notices(&mut self) {
        let notices = take_notices();
        if notices.is_empty() {
            return;
        }
        for notice in &notices {
            let (level, text) = notice_toast(notice);
            let seq = self.notices.next_event_seq;
            self.notices.next_event_seq = seq + 1;
            self.notices.events.push(Notice { seq, level, text });
            match notice {
                HostNotice::ModuleFailed { module, output } => {
                    self.notices.problems.retain(|row| row.module != *module);
                    let rows = problems_from_build_output(module, output);
                    // One line per parsed problem (bounded), so the log says
                    // what the panel holds and where each row would jump to.
                    for row in rows.iter().take(10) {
                        info!(
                            target: telemetry_target::ENGINE,
                            module = row.module.as_str(),
                            file = row.problem.file.as_deref().unwrap_or(""),
                            line = row.problem.line.unwrap_or(0),
                            text = row.problem.message.as_str(),
                            "[editor] problem"
                        );
                    }
                    self.notices.problems.extend(rows);
                }
                HostNotice::Reloaded { module, .. } => {
                    self.notices.problems.retain(|row| row.module != *module);
                }
                HostNotice::PatchFailed { function, detail } => {
                    self.notices.problems.retain(|row| row.module != *function);
                    self.notices.problems.push(ProblemRow {
                        module: function.clone(),
                        problem: problem_parser::Problem {
                            severity: Severity::Error,
                            message: detail.clone(),
                            file: None,
                            line: None,
                            column: None,
                        },
                    });
                }
                HostNotice::RolledBack {
                    function,
                    to_generation,
                    automatic,
                } => {
                    // The running implementation no longer matches the sources
                    // until the next good edit, so the rollback is a warning.
                    self.notices.problems.retain(|row| row.module != *function);
                    let message = if *automatic {
                        format!("rolled back to generation {to_generation} after repeated panics")
                    } else {
                        format!("rolled back to generation {to_generation}")
                    };
                    self.notices.problems.push(ProblemRow {
                        module: function.clone(),
                        problem: problem_parser::Problem {
                            severity: Severity::Warning,
                            message,
                            file: None,
                            line: None,
                            column: None,
                        },
                    });
                }
                HostNotice::PatchApplied { .. } => {}
            }
        }
        self.notices.revision = self.notices.revision.wrapping_add(1);
        let world = self.host.host_mut().engine_mut().world_mut();
        match world.get_resource_mut::<EditorNotices>() {
            Some(resource) => *resource = self.notices.clone(),
            None => warn!(
                target: telemetry_target::ENGINE,
                "[editor] notices resource missing"
            ),
        }
    }

    /// Inject a full right click at a point of the window's client area,
    /// through the same engine input seam a real click enters: the shell's
    /// egui pass hit-tests and routes it, so a context menu opens over
    /// whatever is under the pointer. No scene pick: a right click is the
    /// UI's, never the scene's.
    fn inject_rclick(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        self.queue_input(InputEvent::CursorMoved { position });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ButtonState::Pressed,
        });
        self.queue_input(InputEvent::MouseButton {
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
        self.queue_input(InputEvent::CursorMoved { position });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Pressed,
        });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Released,
        });
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] click injected"
        );
        // A click inside a visible scene viewport activates it and asks what
        // is under it, through the same press the window events route: the
        // pick's answer moves the shared selection, and the outline follows.
        // A point over the overlay's chips is the UI's, not the scene's.
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
                // A scripted click is a press and a release in one command:
                // the pick answers later with the button already up, so a
                // handle under the point does not start a drag.
                self.activate_view(view);
                self.press_scene(view, x - origin_x, y - origin_y);
                self.release_scene(view);
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
                self.queue_input(InputEvent::CursorMoved {
                    position: pill_core::math::Vector2f::new(from.0, from.1),
                });
                self.queue_input(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Pressed,
                });
                if let Some(drag) = self.scripted_drag.as_mut() {
                    drag.stage = 1;
                }
            }
            1 => {
                self.queue_input(InputEvent::CursorMoved {
                    position: pill_core::math::Vector2f::new(to.0, to.1),
                });
                if let Some(drag) = self.scripted_drag.as_mut() {
                    drag.stage = 2;
                }
            }
            _ => {
                self.queue_input(InputEvent::MouseButton {
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
        if pill_editor_core_egui::layout::activate_tab(state.dock.state_mut(), tab) {
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name(),
                "[editor] tab activated"
            );
        } else {
            warn!(
                target: telemetry_target::ENGINE,
                tab = tab.name(),
                "[editor] tab activation needs the tab in the dock"
            );
        }
    }

    /// Drain what the menu bar asked for at the frame boundary: undo and redo
    /// through the scripted channel's own walk, the Window menu's tab through
    /// the shared activation, and a quit through the frame's flag.
    fn drain_menu_requests(&mut self) {
        let (requests, tab, quit) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let quit = world
                .get_resource::<EguiShellFrame>()
                .is_some_and(|frame| frame.quit_requested);
            match world.get_resource_mut::<EguiShellState>() {
                Some(state) => (
                    std::mem::take(&mut state.menubar.history_requests),
                    state.menubar.pending_tab.take(),
                    quit,
                ),
                None => (Vec::new(), None, quit),
            }
        };
        for request in requests {
            match request {
                HistoryRequest::Undo => self.scripted_undo(),
                HistoryRequest::Redo => self.scripted_redo(),
            }
        }
        if let Some(tab) = tab {
            self.activate_tab(tab);
        }
        if quit {
            info!(
                target: telemetry_target::ENGINE,
                "[editor] quit requested"
            );
            self.quit = true;
        }
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
// Host notices to editor notices (W-0038 Stages 4 and 6)
// =============================================================================

/// The toast a host notice becomes: its level and its one line.
fn notice_toast(notice: &HostNotice) -> (NoticeLevel, String) {
    match notice {
        HostNotice::ModuleFailed { module, .. } => (
            NoticeLevel::Error,
            format!("{module}: reload failed; the previous generation still runs"),
        ),
        HostNotice::Reloaded { module, build_ms } => {
            let text = if *build_ms == 0 {
                format!("{module} reloaded")
            } else {
                format!("{module} reloaded in {build_ms} ms")
            };
            (NoticeLevel::Note, text)
        }
        HostNotice::PatchApplied {
            function,
            milliseconds,
        } => (
            NoticeLevel::Note,
            format!("{function} patched live in {milliseconds:.0} ms"),
        ),
        HostNotice::PatchFailed { function, detail } => (
            NoticeLevel::Error,
            format!("{function} patch failed: {detail}"),
        ),
        HostNotice::RolledBack {
            function,
            to_generation,
            automatic,
        } => {
            let text = if *automatic {
                format!(
                    "{function} rolled back to generation {to_generation} after repeated panics"
                )
            } else {
                format!("{function} rolled back to generation {to_generation}")
            };
            (NoticeLevel::Warning, text)
        }
    }
}

/// The Problems rows a module's failed build output becomes.
///
/// The build text is parsed for compiler diagnostics; when none is
/// recognisable - a linker error, a foreign toolchain's wording - the first
/// non-empty line is kept as one row, so a failure never shows as nothing.
fn problems_from_build_output(module: &str, output: &str) -> Vec<ProblemRow> {
    let parsed = problem_parser::parse_build_output(output);
    if parsed.is_empty() {
        let message = output
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("build failed")
            .to_string();
        return vec![ProblemRow {
            module: module.to_string(),
            problem: problem_parser::Problem {
                severity: Severity::Error,
                message,
                file: None,
                line: None,
                column: None,
            },
        }];
    }
    parsed
        .into_iter()
        .map(|problem| ProblemRow {
            module: module.to_string(),
            problem,
        })
        .collect()
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
    fn attach(&mut self, host: &mut RenderingHost, camera: &OrbitCamera, size: (u32, u32)) {
        match host.attach_surface(
            self.surface_id,
            Arc::clone(&self.window.window),
            size.0,
            size.1,
            SurfaceOptions::default(),
        ) {
            Ok(()) => {
                host.set_surface_camera(self.surface_id, Some(camera_binding(camera)));
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

/// The tab's display name for log lines.
fn tab_name(tab: EditorTab) -> &'static str {
    tab.name()
}

// =============================================================================
// EditorCamera
// =============================================================================

/// The camera binding the scene surface draws through: the view's orbit
/// camera as a camera transform, never a world entity (D29).
fn camera_binding(camera: &OrbitCamera) -> CameraBinding {
    CameraBinding::View {
        camera: CameraComponent::default(),
        transform: TransformComponent {
            translation: camera.eye(),
            rotation: camera.rotation(),
            ..Default::default()
        },
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

/// The tool a bare W, E or R picks - the winit shortcut handler and a
/// scripted `key` chord read this one mapping, so the two cannot drift.
fn gizmo_mode_for_key(key: KeyCode) -> Option<GizmoMode> {
    match key {
        KeyCode::KeyW => Some(GizmoMode::Translate),
        KeyCode::KeyE => Some(GizmoMode::Rotate),
        KeyCode::KeyR => Some(GizmoMode::Scale),
        _ => None,
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
/// The press records where the drag started - the axis parameter, ring
/// angle or in-plane coordinates the cursor ray named, the transform it
/// started from and the basis its handles were built in - so every move
/// applies a delta against the press and nothing jumps on the first pixel.
/// The start pivot stays put for the displacement line while the current
/// one follows the object.
struct GizmoDrag {
    /// The scene view the drag belongs to: every move re-reads that view's
    /// ray.
    view: usize,
    /// The handle id the drag holds: its axis, or its plane's pair.
    handle: u32,
    /// What the handle addresses.
    kind: HandleKind,
    /// The entity being dragged.
    entity: Entity,
    /// The transform at the press; every move is computed from it, so a
    /// snapped drag cannot accumulate rounding.
    original: TransformComponent,
    /// The frame the handles were built in (the world axes, or the object's
    /// rotated ones): a local-space drag follows these for its whole life.
    basis: [[f32; 3]; 3],
    /// The axis parameter (translate, scale) or ring angle (rotate) at the
    /// press; unused by a plane drag.
    start_value: f32,
    /// The in-plane coordinates at the press; `None` for a single axis.
    start_plane: Option<(f32, f32)>,
    /// The gizmo's world pivot when the drag began: the displacement line's
    /// fixed end.
    start_pivot: [f32; 3],
    /// The pivot now, so the line's other end follows every move.
    current_pivot: [f32; 3],
    /// How long the handles were at the press, in world units: the scale
    /// drag's reference and the guides' dash length.
    reference: f32,
    /// The value written on the previous move, for the end log.
    last_value: Option<[f32; 4]>,
}

/// The pick a scene press asked: the view and press pixel it happened at,
/// and the request id its answer will carry.
struct PressPick {
    /// The view the press landed in.
    view: usize,
    /// The press pixel in the view's physical surface coordinates.
    x: f32,
    /// The press pixel in the view's physical surface coordinates.
    y: f32,
    /// The pick request the answer is matched by.
    id: u64,
}

/// A camera navigation drag in flight.
struct CameraDrag {
    /// The view whose camera the drag moves.
    view: usize,
    /// The gesture the press matched.
    navigation: CameraNavigation,
    /// The last pointer position, in the view's physical pixels: every move
    /// applies its delta since this one.
    last: (f32, f32),
}

/// The tool mode a handle's drag edits: a plane square translates.
fn gizmo_drag_mode(kind: HandleKind) -> GizmoMode {
    match kind {
        HandleKind::Axis { mode, .. } => mode,
        HandleKind::Plane { .. } => GizmoMode::Translate,
    }
}

/// The display name of a plane handle, as the axis pair it spans.
fn gizmo_plane_name(first: usize, second: usize) -> &'static str {
    match (first, second) {
        (0, 1) => "xy",
        (0, 2) => "xz",
        _ => "yz",
    }
}

/// Draw the drag guides: the axis constraint line through a single-axis
/// handle's axis, and - for a translation drag - the dotted line from the
/// press's pivot to the current one.
///
/// They are plain debug lines, not gizmo geometry: no handle id, so a pick
/// never names them and they cannot be grabbed. The axis guide uses the
/// handle's own colour; the displacement line is neutral so it never reads
/// as a fourth axis.
fn draw_gizmo_guides(
    debug: &mut DebugDraw,
    kind: HandleKind,
    basis: [[f32; 3]; 3],
    start_pivot: [f32; 3],
    current_pivot: [f32; 3],
    reference: f32,
) {
    let style = |color: [f32; 4]| DebugStyle {
        color,
        depth_test: false,
        ..DebugStyle::default()
    };
    if let Some((_, axis)) = kind.axis() {
        let span = reference * GIZMO_GUIDE_SPAN;
        let (from, to) = gizmo::axis_guide(current_pivot, basis[axis], span);
        let mut color = gizmo::AXIS_COLORS[axis];
        color[3] = 0.9;
        debug.line(from, to, style(color));
    }
    if gizmo_drag_mode(kind) == GizmoMode::Translate {
        let dash = reference * GIZMO_DASH;
        for (from, to) in gizmo::dotted_segments(start_pivot, current_pivot, dash) {
            debug.line(from, to, style([0.92, 0.92, 0.92, 0.85]));
        }
    }
}

/// The display name of a camera navigation gesture.
fn camera_navigation_name(navigation: CameraNavigation) -> &'static str {
    match navigation {
        CameraNavigation::Orbit => "orbit",
        CameraNavigation::Pan => "pan",
        CameraNavigation::Dolly => "dolly",
    }
}

/// A shell rectangle in logical points as the child window's own client
/// pixels: scaled, rounded, kept at least one pixel wide, and clamped inside
/// the window so a viewport that overhangs an edge never asks for pixels the
/// window does not have.
fn scaled_view_rect(
    rect: ShellRect,
    scale_factor: f32,
    window_size: (u32, u32),
) -> (i32, i32, u32, u32) {
    let scale = scale_factor as f64;
    let x = (rect.x as f64 * scale).round() as i32;
    let y = (rect.y as f64 * scale).round() as i32;
    let width = (rect.width as f64 * scale).round().max(1.0) as i32;
    let height = (rect.height as f64 * scale).round().max(1.0) as i32;
    let (client_width, client_height) = (window_size.0 as i32, window_size.1 as i32);
    let x = x.clamp(0, (client_width - 1).max(0));
    let y = y.clamp(0, (client_height - 1).max(0));
    let width = width.min((client_width - x).max(1)) as u32;
    let height = height.min((client_height - y).max(1)) as u32;
    (x, y, width, height)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle inside the window scales with the display and lands where
    /// the shell drew it.
    #[test]
    fn a_view_rect_scales_with_the_display() {
        let rect = ShellRect {
            x: 100.0,
            y: 50.0,
            width: 300.0,
            height: 200.0,
        };
        assert_eq!(
            scaled_view_rect(rect, 1.0, (1280, 800)),
            (100, 50, 300, 200)
        );
        assert_eq!(
            scaled_view_rect(rect, 2.0, (2560, 1600)),
            (200, 100, 600, 400)
        );
        // A fractional scale rounds each edge rather than truncating it.
        assert_eq!(
            scaled_view_rect(rect, 1.5, (1920, 1200)),
            (150, 75, 450, 300)
        );
    }

    /// A viewport that overhangs the window's edge is clipped to what the
    /// window has, and never collapses to zero.
    #[test]
    fn a_view_rect_is_clipped_to_the_window() {
        let overhanging = ShellRect {
            x: 1200.0,
            y: 780.0,
            width: 400.0,
            height: 300.0,
        };
        assert_eq!(
            scaled_view_rect(overhanging, 1.0, (1280, 800)),
            (1200, 780, 80, 20)
        );
        // A rectangle entirely past the edge keeps one pixel of each side
        // instead of asking for a zero-sized child window.
        let past = ShellRect {
            x: 5000.0,
            y: 5000.0,
            width: 10.0,
            height: 10.0,
        };
        assert_eq!(scaled_view_rect(past, 1.0, (1280, 800)), (1279, 799, 1, 1));
        // A sub-pixel rectangle still reserves one pixel.
        let tiny = ShellRect {
            x: 4.0,
            y: 4.0,
            width: 0.1,
            height: 0.1,
        };
        assert_eq!(scaled_view_rect(tiny, 1.0, (1280, 800)), (4, 4, 1, 1));
    }
}
